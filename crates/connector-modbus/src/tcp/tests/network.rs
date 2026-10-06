#![expect(clippy::arithmetic_side_effects, reason = "a test may panic")]

use std::future::poll_fn;
use std::io::IoSlice;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use env::clock::Clock;
use env::net::{self, Listener, Tcp, tcp};
use env::thread::Handle;
use sim::{Sim, node};
use types::time::Span;

use crate::Error;
use crate::device::Device;
use crate::pdu::{Exception, Reply, Request, Table};
use crate::tcp::{self as modbus, Client, Failure, Header};

const UNIT: u8 = 17;
const PORT: u16 = 502;
const TIMEOUT: Span = ms(300);

const fn ms(n: i64) -> Span {
    Span::from_nanos(n * 1_000_000)
}

fn shard(name: &str) -> env::shards::Config {
    env::shards::Config {
        name: name.into(),
        core: Some(0),
    }
}

fn options(buffer: usize) -> tcp::Options {
    tcp::Options {
        send_buffer_bytes: buffer,
        recv_buffer_bytes: buffer,
        unsent_bytes_max: buffer,
        delayed: false,
    }
}

/// A device whose items each hold their own address.
fn device() -> Arc<Mutex<Device>> {
    Arc::new(Mutex::new(Device {
        coils: (0..100).map(|a| a % 3 == 0).collect(),
        discrete_inputs: (0..100).map(|a| a % 2 == 0).collect(),
        holding_registers: (0..100).collect(),
        input_registers: (1000..1100).collect(),
    }))
}

/// A run with a client node and a device node.
struct Network {
    sim: Sim,
    client: node::Node,
    device: node::Node,
    handles: Vec<Handle>,
}

impl Network {
    fn new(seed: u64) -> Self {
        let mut sim = Sim::new(sim::Config {
            seed,
            steps_max: 10_000_000,
            ..sim::Config::default()
        });
        let client = sim.node(node::Config::default());
        let device = sim.node(node::Config::default());
        Self {
            sim,
            client,
            device,
            handles: Vec::new(),
        }
    }

    /// The device's address on `PORT`.
    fn remote(&self) -> SocketAddr {
        SocketAddr::new(self.device.addresses()[0], PORT)
    }

    /// Listens on the device node, with buffers of `buffer` bytes for each stream.
    fn listen(&self, buffer: usize) -> Listener {
        let listen = tcp::Listen {
            local: self.remote(),
            backlog: 4,
            options: options(buffer),
        };
        self.device.net().listen(&listen).expect("the port is free")
    }

    /// Runs `main` on the device node with a listener on `PORT`.
    fn on_device<F: Future<Output = ()> + 'static>(
        &mut self,
        buffer: usize,
        main: impl FnOnce(Listener, Clock) -> F + Send + 'static,
    ) {
        let (listener, clock) = (self.listen(buffer), self.device.clock());
        let handle = self
            .device
            .shards()
            .start(shard("device"), move |_| main(listener, clock));
        self.handles.push(handle.expect("the shard starts"));
    }

    /// Serves `device` as `UNIT` on the device node.
    fn serve(&mut self, device: &Arc<Mutex<Device>>) {
        let device = Arc::clone(device);
        self.on_device(1 << 16, move |listener, _| async move {
            let error = modbus::serve(listener, UNIT, &device).await;
            panic!("the listener failed: {error}");
        });
    }

    /// Answers the first stream on the device node with `answer`. `answer` gives the
    /// stream back, so that its bytes in flight still arrive.
    fn raw<F: Future<Output = Tcp> + 'static>(
        &mut self,
        answer: impl FnOnce(Tcp, Clock) -> F + Send + 'static,
    ) {
        self.on_device(1 << 16, move |mut listener, clock| async move {
            let stream = poll_fn(|cx| listener.poll_accept(cx))
                .await
                .expect("a stream comes");
            let stream = answer(stream, clock.clone()).await;
            clock.sleep(Span::MINUTE).await;
            drop((stream, listener));
        });
    }

    /// Runs `main` with a client of `buffer` bytes on the client node, for at most
    /// 60 seconds, and gives its output.
    fn client<T: Send + 'static, F: Future<Output = T> + 'static>(
        &mut self,
        buffer: usize,
        main: impl FnOnce(Client, Clock) -> F + Send + 'static,
    ) -> T {
        let (net, clock) = (self.client.net(), self.client.clock());
        let config = tcp::Config {
            remote: self.remote(),
            options: options(buffer),
        };
        self.on_client(move || async move {
            let client = Client::connect(&net, &config, clock.clone(), TIMEOUT)
                .await
                .expect("the device listens");
            main(client, clock).await
        })
    }

    /// Runs `main` on the client node, for at most 60 seconds, and gives its
    /// output.
    fn on_client<T: Send + 'static, F: Future<Output = T> + 'static>(
        &mut self,
        main: impl FnOnce() -> F + Send + 'static,
    ) -> T {
        let out = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&out);
        let handle = self
            .client
            .shards()
            .start(shard("client"), move |_| async move {
                let value = main().await;
                *slot.lock().expect("no panic under the lock") = Some(value);
            });
        self.handles.push(handle.expect("the shard starts"));
        self.sim.run_for(Span::MINUTE).expect("the run ends");
        let value = out.lock().expect("no panic under the lock").take();
        value.expect("main returned within a minute")
    }
}

/// What a reply says, owned.
#[derive(Debug, PartialEq, Eq)]
enum Said {
    Bits(Vec<bool>),
    Registers(Vec<u16>),
    Written,
    Exception(Exception),
}

fn said(reply: Result<Reply<'_>, Failure>) -> Result<Said, Failure> {
    Ok(match reply? {
        Reply::Bits(bits) => Said::Bits(bits.iter().collect()),
        Reply::Registers(registers) => Said::Registers(registers.iter().collect()),
        Reply::Written => Said::Written,
        Reply::Exception(exception) => Said::Exception(exception),
    })
}

fn read(table: Table, start: u16, count: u16) -> Request {
    Request::Read {
        table,
        start,
        count,
    }
}

/// Sends all of `bytes`.
async fn write(stream: &mut Tcp, bytes: &[u8]) {
    let mut sent = 0;
    while sent < bytes.len() {
        let parts = [IoSlice::new(&bytes[sent..])];
        sent += poll_fn(|cx| stream.poll_write(cx, &parts))
            .await
            .expect("the write works");
    }
}

/// Reads one whole request frame, and gives its header.
async fn request(stream: &mut Tcp) -> Header {
    let mut bytes = Vec::new();
    loop {
        if let Some(frame) = modbus::decode(&bytes).expect("a valid frame") {
            assert_eq!(frame.len, bytes.len(), "one frame at a time");
            return frame.header;
        }
        let mut buffer = [0; 260];
        let n = poll_fn(|cx| stream.poll_read(cx, &mut buffer))
            .await
            .expect("the read works");
        assert_ne!(n, 0, "the client closed");
        bytes.extend_from_slice(&buffer[..n]);
    }
}

/// A frame of a reply with `registers` to a read of holding registers.
fn registers(header: Header, registers: &[u16]) -> Vec<u8> {
    let count = u8::try_from(registers.len() * 2).expect("fits");
    let length = u16::from(count) + 3;
    let mut out = header.transaction.to_be_bytes().to_vec();
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(&[header.unit, 0x03, count]);
    for register in registers {
        out.extend_from_slice(&register.to_be_bytes());
    }
    out
}

#[test]
fn reads_and_writes_each_table() {
    let mut network = Network::new(1);
    let device = device();
    network.serve(&device);
    let requests = vec![
        read(Table::Coils, 3, 4),
        read(Table::DiscreteInputs, 0, 3),
        read(Table::HoldingRegisters, 10, 2),
        read(Table::InputRegisters, 98, 2),
        Request::WriteCoil {
            address: 1,
            value: true,
        },
        Request::WriteRegister {
            address: 2,
            value: 0xBEEF,
        },
        Request::WriteCoils {
            start: 90,
            values: vec![true; 10],
        },
        Request::WriteRegisters {
            start: 50,
            values: vec![7, 8, 9],
        },
        read(Table::HoldingRegisters, 0, 60),
        read(Table::HoldingRegisters, 99, 2),
    ];
    let replies = network.client(1 << 16, move |mut client, _| async move {
        let mut replies = Vec::new();
        for request in &requests {
            replies.push(said(client.exchange(UNIT, request).await));
        }
        replies
    });
    let mut registers: Vec<u16> = (0..100).collect();
    registers[2] = 0xBEEF;
    registers[50..53].copy_from_slice(&[7, 8, 9]);
    assert_eq!(
        replies,
        [
            Ok(Said::Bits(vec![true, false, false, true])),
            Ok(Said::Bits(vec![true, false, true])),
            Ok(Said::Registers(vec![10, 11])),
            Ok(Said::Registers(vec![1098, 1099])),
            Ok(Said::Written),
            Ok(Said::Written),
            Ok(Said::Written),
            Ok(Said::Written),
            Ok(Said::Registers(registers[..60].to_vec())),
            Ok(Said::Exception(Exception::IllegalAddress)),
        ]
    );
    let device = device.lock().expect("no panic under the lock");
    assert!(device.coils[1]);
    assert!(device.coils[90..].iter().all(|&coil| coil));
    assert_eq!(device.holding_registers[..100], registers);
}

#[test]
fn answers_another_unit_as_a_gateway_with_no_target() {
    let mut network = Network::new(2);
    network.serve(&device());
    let got = network.client(1 << 16, |mut client, _| async move {
        said(client.exchange(UNIT + 1, &read(Table::Coils, 0, 1)).await)
    });
    assert_eq!(got, Ok(Said::Exception(Exception::GatewayTarget)));
}

#[test]
fn refuses_a_request_that_is_not_valid_and_sends_nothing() {
    let mut network = Network::new(3);
    network.serve(&device());
    let (got, next) = network.client(1 << 16, |mut client, _| async move {
        let got = said(client.exchange(UNIT, &read(Table::Coils, 0, 0)).await);
        let next = said(client.exchange(UNIT, &read(Table::Coils, 0, 1)).await);
        (got, next)
    });
    let error = Failure::Request(Error::Count {
        count: 0,
        max: 2000,
    });
    assert_eq!(got, Err(error.clone()));
    assert_eq!(error.to_string(), "a count of 0, outside 1 to 2000");
    assert_eq!(next, Ok(Said::Bits(vec![true])));
}

#[test]
fn drops_the_late_reply_to_a_request_that_timed_out() {
    let mut network = Network::new(4);
    network.raw(|mut stream, clock| async move {
        let first = request(&mut stream).await;
        clock.sleep(ms(400)).await;
        let second = request(&mut stream).await;
        write(&mut stream, &registers(first, &[1])).await;
        write(&mut stream, &registers(second, &[2])).await;
        stream
    });
    let (first, second, spent) =
        network.client(1 << 16, |mut client, clock| async move {
            let request = read(Table::HoldingRegisters, 0, 1);
            let start = clock.now();
            let first = said(client.exchange(UNIT, &request).await);
            let spent = clock.now() - start;
            let second = said(client.exchange(UNIT, &request).await);
            (first, second, spent)
        });
    assert_eq!(first, Err(Failure::Timeout));
    assert_eq!(
        Failure::Timeout.to_string(),
        "no whole reply came before the timeout"
    );
    assert!(spent >= TIMEOUT, "{spent}");
    assert!(spent < ms(320), "{spent}");
    assert_eq!(second, Ok(Said::Registers(vec![2])));
}

#[test]
fn sends_the_rest_of_a_request_that_timed_out_first() {
    let mut network = Network::new(5);
    let device = device();
    let served = Arc::clone(&device);
    network.on_device(64, move |listener, clock| async move {
        clock.sleep(ms(400)).await;
        let error = modbus::serve(listener, UNIT, &served).await;
        panic!("the listener failed: {error}");
    });
    let (first, second) = network.client(64, |mut client, _| async move {
        let write = Request::WriteRegisters {
            start: 0,
            values: vec![7; 100],
        };
        let first = said(client.exchange(UNIT, &write).await);
        let read = read(Table::HoldingRegisters, 98, 2);
        let second = said(client.exchange(UNIT, &read).await);
        (first, second)
    });
    assert_eq!(first, Err(Failure::Timeout));
    assert_eq!(second, Ok(Said::Registers(vec![7, 7])));
    let device = device.lock().expect("no panic under the lock");
    assert_eq!(device.holding_registers[..100], [7; 100]);
}

#[test]
fn fails_on_a_stream_that_ends_or_is_out_of_step() {
    let cases: [(&[u8], Failure, &str); 3] = [
        (&[], Failure::Closed, "the device closed the stream"),
        (
            &[0, 1, 0, 1, 0, 3, UNIT, 3, 0],
            Failure::Stream(Error::Protocol(1)),
            "an MBAP header with protocol 1, not 0",
        ),
        (
            &[0, 1, 0, 0, 0, 3, UNIT + 1, 3, 0],
            Failure::Unit {
                want: UNIT,
                got: UNIT + 1,
            },
            "a reply from unit 18 to a request to unit 17",
        ),
    ];
    for (seed, (reply, failure, message)) in (6..).zip(cases) {
        let mut network = Network::new(seed);
        network.raw(move |mut stream, _| async move {
            request(&mut stream).await;
            if reply.is_empty() {
                poll_fn(|cx| stream.poll_close(cx))
                    .await
                    .expect("the close works");
            } else {
                write(&mut stream, reply).await;
            }
            stream
        });
        let got = network.client(1 << 16, |mut client, _| async move {
            said(client.exchange(UNIT, &read(Table::Coils, 0, 1)).await)
        });
        assert_eq!(got, Err(failure.clone()));
        assert_eq!(failure.to_string(), message);
    }
}

#[test]
fn gives_the_reply_that_does_not_match_the_request() {
    let mut network = Network::new(9);
    network.raw(|mut stream, _| async move {
        let header = request(&mut stream).await;
        write(&mut stream, &registers(header, &[1])).await;
        stream
    });
    let got = network.client(1 << 16, |mut client, _| async move {
        said(client.exchange(UNIT, &read(Table::Coils, 0, 1)).await)
    });
    assert!(matches!(got, Err(Failure::Frame(_))), "{got:?}");
}

#[test]
fn refuses_a_connect_with_no_listener() {
    let mut network = Network::new(10);
    let (net, clock) = (network.client.net(), network.client.clock());
    let remote = network.remote();
    let got = network.on_client(move || async move {
        let config = tcp::Config {
            remote,
            options: options(1 << 16),
        };
        Client::connect(&net, &config, clock, TIMEOUT).await.err()
    });
    assert_eq!(got, Some(net::Error::Refused { remote }));
}

#[test]
fn serves_the_next_stream_after_one_ends_or_is_out_of_step() {
    let mut network = Network::new(11);
    network.serve(&device());
    let (net, clock) = (network.client.net(), network.client.clock());
    let remote = network.remote();
    let got = network.on_client(move || async move {
        let config = tcp::Config {
            remote,
            options: options(1 << 16),
        };
        let request = read(Table::HoldingRegisters, 4, 1);
        let mut client = Client::connect(&net, &config, clock.clone(), TIMEOUT)
            .await
            .expect("the device listens");
        let first = said(client.exchange(UNIT, &request).await);
        drop(client);
        let mut bad = net.connect(&config).await.expect("the device listens");
        write(&mut bad, &[0, 1, 0, 1, 0, 3, UNIT, 3, 0]).await;
        let mut buffer = [0; 1];
        let end = poll_fn(|cx| bad.poll_read(cx, &mut buffer)).await;
        let mut again = Client::connect(&net, &config, clock, TIMEOUT)
            .await
            .expect("the device listens");
        let last = said(again.exchange(UNIT, &request).await);
        (first, end, last)
    });
    let (first, end, last) = got;
    assert_eq!(first, Ok(Said::Registers(vec![4])));
    assert!(end.is_err(), "the device reset the bad stream: {end:?}");
    assert_eq!(last, Ok(Said::Registers(vec![4])));
}
