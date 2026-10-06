#![expect(clippy::arithmetic_side_effects, reason = "a test may panic")]

use std::future::poll_fn;
use std::num::{NonZeroU8, NonZeroU32};
use std::path::Path;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use super::framed;
use crate::Error;
use crate::device::Device;
use crate::pdu::{Exception, Reply, Request, Table};
use crate::rtu::{self, Client, Failure};
use env::clock::Clock;
use env::serial::{Config, Parity, Port, Settings, StopBits};
use env::thread::Handle;
use sim::{Sim, line, node};
use types::time::{Monotonic, Span};

const CLIENT: &str = "/dev/ttyUSB0";
const DEVICE: &str = "/dev/ttyS0";
const UNIT: NonZeroU8 = NonZeroU8::new(17).expect("not 0");
const TIMEOUT: Span = ms(300);

const fn ms(n: i64) -> Span {
    Span::from_nanos(n * 1_000_000)
}

fn settings(baud: u32, parity: Option<Parity>) -> Settings {
    Settings {
        baud: NonZeroU32::new(baud).expect("not 0"),
        parity,
        stop_bits: StopBits::One,
    }
}

fn config(path: &str, settings: Settings) -> Config {
    Config {
        path: path.into(),
        settings,
    }
}

fn shard(name: &str) -> env::shards::Config {
    env::shards::Config {
        name: name.into(),
        core: Some(0),
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

/// A run with a client node and a device node joined by `line`.
struct Bus {
    sim: Sim,
    client: node::Node,
    device: node::Node,
    settings: Settings,
    line: line::Config,
    handles: Vec<Handle>,
}

impl Bus {
    fn new(seed: u64, settings: Settings, line: line::Config) -> Self {
        let mut sim = Sim::new(sim::Config {
            seed,
            steps_max: 10_000_000,
            ..sim::Config::default()
        });
        let client = sim.node(node::Config::default());
        let device = sim.node(node::Config::default());
        let mut bus = Self {
            sim,
            client,
            device,
            settings,
            line,
            handles: Vec::new(),
        };
        bus.set(line);
        bus
    }

    /// Sets the faults of the line again.
    fn set(&mut self, line: line::Config) {
        self.line = line;
        let (client, device) = (&self.client, &self.device);
        self.sim
            .line(client, Path::new(CLIENT), device, Path::new(DEVICE), line);
    }

    /// Serves `device` as `unit` on the device node.
    fn serve(&mut self, unit: NonZeroU8, device: &Arc<Mutex<Device>>) {
        let (serial, clock) = (self.device.serial(), self.device.clock());
        let config = config(DEVICE, self.settings);
        let device = Arc::clone(device);
        let handle = self
            .device
            .shards()
            .start(shard("device"), move |_| async move {
                let error = rtu::serve(&serial, &config, clock, unit, &device).await;
                panic!("the device port failed: {error}");
            });
        self.handles.push(handle.expect("the shard starts"));
    }

    /// Runs `main` with a client on the client node, for at most 60 seconds, and
    /// gives its output.
    fn client<T: Send + 'static, F: Future<Output = T> + 'static>(
        &mut self,
        main: impl FnOnce(Client, Clock) -> F + Send + 'static,
    ) -> T {
        let (serial, clock) = (self.client.serial(), self.client.clock());
        let config = config(CLIENT, self.settings);
        self.on_client(move || async move {
            let client = Client::open(&serial, &config, clock.clone(), TIMEOUT)
                .await
                .expect("the port opens");
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

/// The start of an exchange, and what it gave.
type Logged = (Monotonic, Result<Said, Failure>);

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

#[test]
fn reads_and_writes_each_table() {
    for settings in [settings(9_600, Some(Parity::Even)), settings(115_200, None)] {
        let mut bus = Bus::new(1, settings, line::Config::default());
        let device = device();
        bus.serve(UNIT, &device);
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
        let replies = bus.client(move |mut client, _| async move {
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
            ],
            "{settings:?}"
        );
        let device = device.lock().expect("no panic under the lock");
        assert!(device.coils[1], "{settings:?}");
        assert!(device.coils[90..].iter().all(|&coil| coil), "{settings:?}");
        assert_eq!(device.holding_registers[..100], registers, "{settings:?}");
    }
}

#[test]
fn refuses_a_request_that_is_not_valid_and_sends_nothing() {
    let mut bus = Bus::new(2, settings(9_600, None), line::Config::default());
    let device = device();
    bus.serve(UNIT, &device);
    let (got, next) = bus.client(|mut client, _| async move {
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
fn times_out_when_no_device_answers() {
    let mut bus = Bus::new(3, settings(9_600, None), line::Config::default());
    let device = device();
    bus.serve(NonZeroU8::new(5).expect("not 0"), &device);
    let (got, spent) = bus.client(|mut client, clock| async move {
        let start = clock.now();
        let got = said(client.exchange(UNIT, &read(Table::Coils, 0, 1)).await);
        (got, clock.now() - start)
    });
    assert_eq!(got, Err(Failure::Timeout));
    assert_eq!(
        Failure::Timeout.to_string(),
        "no whole reply came before the timeout"
    );
    assert!(spent >= TIMEOUT, "{spent}");
    assert!(spent < ms(320), "{spent}");
}

/// Exchanges a read every 50 ms for 8 seconds while the line is noisy for the
/// first 4, and checks each result.
fn recovers_after(line: line::Config, parity: Option<Parity>, failures: &[&str]) {
    for seed in 0..4 {
        let mut bus = Bus::new(seed, settings(19_200, parity), line);
        let device = device();
        bus.serve(UNIT, &device);
        let results: Arc<Mutex<Vec<Logged>>> = Arc::default();
        let log = Arc::clone(&results);
        let (serial, clock) = (bus.client.serial(), bus.client.clock());
        let config = config(CLIENT, bus.settings);
        let handle = bus
            .client
            .shards()
            .start(shard("client"), move |_| async move {
                let mut client = Client::open(&serial, &config, clock.clone(), TIMEOUT)
                    .await
                    .expect("the port opens");
                for _ in 0..160 {
                    let start = clock.now();
                    let got = said(
                        client
                            .exchange(UNIT, &read(Table::HoldingRegisters, 0, 4))
                            .await,
                    );
                    log.lock().expect("no panic").push((start, got));
                    clock.sleep(ms(50)).await;
                }
            });
        bus.handles.push(handle.expect("the shard starts"));
        bus.sim.run_for(ms(4_000)).expect("the run goes on");
        bus.set(line::Config::default());
        bus.sim.run_for(Span::MINUTE).expect("the run ends");
        let clean = node::Config::default().monotonic + ms(4_300);
        let results = results.lock().expect("no panic");
        assert!(results.len() > 100, "seed {seed}: {}", results.len());
        let mut kinds = Vec::new();
        for (start, got) in results.iter() {
            match got {
                Ok(said) => {
                    assert_eq!(said, &Said::Registers(vec![0, 1, 2, 3]), "seed {seed}");
                }
                Err(failure) => {
                    assert!(*start < clean, "seed {seed}: {failure} at {start:?}");
                    let kind = match failure {
                        Failure::Timeout => "timeout",
                        Failure::Frame(Error::Crc { .. }) => "crc",
                        other => panic!("seed {seed}: {other:?}"),
                    };
                    kinds.push(kind);
                }
            }
        }
        for kind in failures {
            assert!(kinds.contains(kind), "seed {seed}: no {kind} in {kinds:?}");
        }
    }
}

#[test]
fn recovers_after_flipped_bits() {
    let flip = line::Config {
        flip: 0.01,
        ..line::Config::default()
    };
    recovers_after(flip, None, &["crc", "timeout"]);
}

#[test]
fn recovers_after_lost_bytes() {
    let loss = line::Config {
        loss: 0.01,
        ..line::Config::default()
    };
    recovers_after(loss, None, &["timeout"]);
    let flip = line::Config {
        flip: 0.01,
        ..line::Config::default()
    };
    recovers_after(flip, Some(Parity::Even), &["timeout"]);
}

/// Writes all of `bytes`.
async fn write(port: &mut Port, bytes: &[u8]) {
    let mut sent = 0;
    while sent < bytes.len() {
        sent += poll_fn(|cx| port.poll_write(cx, &bytes[sent..]))
            .await
            .expect("the write works");
    }
}

/// Reads exactly `n` bytes, and gives them with the arrival time of the first.
async fn take(port: &mut Port, clock: &Clock, n: usize) -> (Monotonic, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut first = None;
    while bytes.len() < n {
        let mut buffer = vec![0; n - bytes.len()];
        let got = poll_fn(|cx| port.poll_read(cx, &mut buffer))
            .await
            .expect("the read works");
        first.get_or_insert(clock.now());
        bytes.extend_from_slice(&buffer[..got]);
    }
    (first.expect("n is not 0"), bytes)
}

/// Runs `answer` on the device node with a raw port. `answer` gives the port back,
/// so that its bytes in flight still arrive.
fn raw<F: Future<Output = Port> + 'static>(
    bus: &mut Bus,
    answer: impl FnOnce(Port, Clock) -> F + Send + 'static,
) {
    let (serial, clock) = (bus.device.serial(), bus.device.clock());
    let config = config(DEVICE, bus.settings);
    let handle = bus
        .device
        .shards()
        .start(shard("raw"), move |_| async move {
            let port = serial.open(&config).await.expect("the port opens");
            let _port = answer(port, clock).await;
            std::future::pending::<()>().await;
        });
    bus.handles.push(handle.expect("the shard starts"));
}

#[test]
fn refuses_a_reply_from_another_unit() {
    let mut bus = Bus::new(4, settings(9_600, None), line::Config::default());
    raw(&mut bus, |mut port, clock| async move {
        take(&mut port, &clock, 8).await;
        write(&mut port, &framed(&[9, 0x03, 0x02, 0x00, 0x2A])).await;
        port
    });
    let got = bus.client(|mut client, _| async move {
        said(
            client
                .exchange(UNIT, &read(Table::HoldingRegisters, 0, 1))
                .await,
        )
    });
    let error = Failure::Unit { want: 17, got: 9 };
    assert_eq!(got, Err(error.clone()));
    assert_eq!(
        error.to_string(),
        "a reply from unit 9 to a request to unit 17"
    );
}

#[test]
fn gives_a_reply_that_does_not_match_the_request() {
    let mut bus = Bus::new(5, settings(9_600, None), line::Config::default());
    raw(&mut bus, |mut port, clock| async move {
        take(&mut port, &clock, 8).await;
        write(&mut port, &framed(&[17, 0x04, 0x02, 0x00, 0x2A])).await;
        port
    });
    let got = bus.client(|mut client, _| async move {
        said(
            client
                .exchange(UNIT, &read(Table::HoldingRegisters, 0, 1))
                .await,
        )
    });
    let error = Failure::Frame(Error::Answer { want: 3, got: 4 });
    assert_eq!(got, Err(error.clone()));
    assert_eq!(
        error.to_string(),
        "a reply of function 4 to a request of function 3"
    );
}

#[test]
fn leaves_the_line_quiet_between_frames() {
    // 11 bits a character: 3.5 characters, and a fixed 1.75 ms above 19,200 baud.
    for (baud, quiet) in [
        (9_600, 4_010_416),
        (19_200, 2_005_208),
        (115_200, 1_750_000),
    ] {
        leaves_the_line_quiet(
            settings(baud, Some(Parity::Even)),
            Span::from_nanos(quiet),
        );
    }
}

fn leaves_the_line_quiet(settings: Settings, quiet: Span) {
    let mut bus = Bus::new(6, settings, line::Config::default());
    let rate = bus.settings.rate();
    let gaps = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&gaps);
    raw(&mut bus, move |mut port, clock| async move {
        let reply = framed(&[17, 0x03, 0x02, 0x00, 0x2A]);
        let mut sent = None;
        for _ in 0..3 {
            let (first, _) = take(&mut port, &clock, 8).await;
            if let Some(end) = sent {
                // `first` is when the first byte has arrived whole.
                log.lock()
                    .expect("no panic")
                    .push(first - rate.span(1) - end);
            }
            let start = clock.now();
            write(&mut port, &reply).await;
            sent = Some(start + rate.span(u64::try_from(reply.len()).expect("fits")));
        }
        port
    });
    let got = bus.client(|mut client, _| async move {
        let mut got = Vec::new();
        for _ in 0..3 {
            got.push(said(
                client
                    .exchange(UNIT, &read(Table::HoldingRegisters, 0, 1))
                    .await,
            ));
        }
        got
    });
    assert!(got.iter().all(|got| got == &Ok(Said::Registers(vec![42]))));
    let gaps = gaps.lock().expect("no panic");
    assert_eq!(gaps.len(), 2);
    for &gap in gaps.iter() {
        assert!(gap >= quiet, "{gap}");
        assert!(gap.nanos() < quiet.nanos() + 1_000_000, "{gap}");
    }
}

#[test]
fn reads_its_own_reply_after_a_dropped_exchange() {
    let mut bus = Bus::new(7, settings(9_600, None), line::Config::default());
    let device = device();
    bus.serve(UNIT, &device);
    let (dropped, got) = bus.client(|mut client, clock| async move {
        let dropped = {
            let request = read(Table::HoldingRegisters, 0, 4);
            let mut exchange = pin!(client.exchange(UNIT, &request));
            let mut sleep = clock.sleep(ms(12));
            poll_fn(|cx| {
                if exchange.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(false);
                }
                std::pin::Pin::new(&mut sleep).poll(cx).map(|()| true)
            })
            .await
        };
        let got = said(
            client
                .exchange(UNIT, &read(Table::InputRegisters, 0, 2))
                .await,
        );
        (dropped, got)
    });
    assert!(dropped, "the first exchange was still waiting at 12 ms");
    assert_eq!(got, Ok(Said::Registers(vec![1000, 1001])));
}

#[test]
fn serves_only_its_unit_and_drops_a_bad_frame() {
    let mut bus = Bus::new(8, settings(9_600, None), line::Config::default());
    let device = device();
    bus.serve(UNIT, &device);
    let (serial, clock) = (bus.client.serial(), bus.client.clock());
    let config = config(CLIENT, bus.settings);
    let got = bus.on_client(move || async move {
        let mut port = serial.open(&config).await.expect("the port opens");
        let mut frame = Vec::new();
        let mut silent = Vec::new();
        let other = Request::WriteRegister {
            address: 3,
            value: 1,
        };
        rtu::encode(5, &other, &mut frame).expect("valid");
        let broadcast = Request::WriteRegister {
            address: 4,
            value: 0x1234,
        };
        rtu::encode(0, &broadcast, &mut frame).expect("valid");
        frame.extend_from_slice(&[17, 0x03, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00]);
        for part in [&frame[..8], &frame[8..16], &frame[16..]] {
            write(&mut port, part).await;
            clock.sleep(ms(15)).await;
        }
        let mut buffer = [0; 64];
        let mut sleep = clock.sleep(ms(100));
        let heard = poll_fn(|cx| match port.poll_read(cx, &mut buffer) {
            Poll::Ready(n) => Poll::Ready(n.expect("the read works")),
            Poll::Pending => std::pin::Pin::new(&mut sleep).poll(cx).map(|()| 0),
        })
        .await;
        silent.push(heard);
        let mut request = Vec::new();
        rtu::encode(17, &read(Table::HoldingRegisters, 3, 2), &mut request)
            .expect("valid");
        write(&mut port, &request).await;
        let (_, reply) = take(&mut port, &clock, 9).await;
        (silent, reply)
    });
    let (silent, reply) = got;
    assert_eq!(
        silent,
        [0],
        "no reply to another unit, a broadcast, or a bad frame"
    );
    assert_eq!(reply, framed(&[17, 0x03, 0x04, 0x00, 0x03, 0x12, 0x34]));
    let device = device.lock().expect("no panic");
    assert_eq!(device.holding_registers[3], 3, "another unit's write");
    assert_eq!(device.holding_registers[4], 0x1234, "the broadcast write");
}

#[test]
fn serves_only_after_the_line_is_quiet_again() {
    // 10 bits a character at 9,600 baud: 3.5 characters take 3.65 ms.
    let quiet = Span::from_nanos(3_645_833);
    let mut bus = Bus::new(9, settings(9_600, None), line::Config::default());
    let device = device();
    bus.serve(UNIT, &device);
    let (serial, clock) = (bus.client.serial(), bus.client.clock());
    let config = config(CLIENT, bus.settings);
    let rate = bus.settings.rate();
    let gap = bus.on_client(move || async move {
        let mut port = serial.open(&config).await.expect("the port opens");
        let (mut request, mut other) = (Vec::new(), Vec::new());
        let frame = read(Table::HoldingRegisters, 0, 1);
        rtu::encode(17, &frame, &mut request).expect("valid");
        rtu::encode(5, &frame, &mut other).expect("valid");
        clock.sleep(ms(10)).await;
        write(&mut port, &request).await;
        // 2 ms after the request ends: inside the device's quiet before its reply.
        clock
            .sleep(Span::from_nanos(rate.span(8).nanos() + 2_000_000))
            .await;
        let start = clock.now();
        write(&mut port, &other).await;
        let (first, _) = take(&mut port, &clock, 7).await;
        first - rate.span(1) - (start + rate.span(8))
    });
    assert!(gap >= quiet, "{gap}");
    assert!(gap.nanos() < quiet.nanos() + 1_000_000, "{gap}");
}

/// Polls `exchange` for at most 5 seconds, and gives what it gave and how long it
/// took.
async fn bounded(
    client: &mut Client,
    clock: &Clock,
    request: &Request,
) -> (Option<Result<Said, Failure>>, Span) {
    let start = clock.now();
    let mut exchange = pin!(client.exchange(UNIT, request));
    let mut sleep = clock.sleep(ms(5_000));
    let got = poll_fn(|cx| {
        if let Poll::Ready(reply) = exchange.as_mut().poll(cx) {
            return Poll::Ready(Some(said(reply)));
        }
        std::pin::Pin::new(&mut sleep).poll(cx).map(|()| None)
    })
    .await;
    (got, clock.now() - start)
}

#[test]
fn times_out_on_a_line_that_is_never_quiet() {
    let mut bus = Bus::new(10, settings(9_600, None), line::Config::default());
    raw(&mut bus, |mut port, clock| async move {
        loop {
            write(&mut port, &[0x55]).await;
            clock.sleep(ms(2)).await;
        }
    });
    let (got, spent) = bus.client(|mut client, clock| async move {
        let request = read(Table::HoldingRegisters, 0, 1);
        bounded(&mut client, &clock, &request).await
    });
    assert_eq!(got, Some(Err(Failure::Timeout)));
    assert!(spent <= TIMEOUT, "{spent}");
}

#[test]
fn does_not_wait_out_the_timeout_after_a_bad_reply() {
    let mut bus = Bus::new(11, settings(9_600, None), line::Config::default());
    raw(&mut bus, |mut port, clock| async move {
        let reply = framed(&[17, 0x03, 0x02, 0x00, 0x2A]);
        let mut bad = reply.clone();
        bad[6] ^= 0xFF;
        take(&mut port, &clock, 8).await;
        write(&mut port, &bad).await;
        take(&mut port, &clock, 8).await;
        write(&mut port, &reply).await;
        port
    });
    let got = bus.client(|mut client, clock| async move {
        let request = read(Table::HoldingRegisters, 0, 1);
        let first = said(client.exchange(UNIT, &request).await);
        let (second, spent) = bounded(&mut client, &clock, &request).await;
        (first, second, spent)
    });
    let (first, second, spent) = got;
    assert!(
        matches!(first, Err(Failure::Frame(Error::Crc { .. }))),
        "{first:?}"
    );
    assert_eq!(second, Some(Ok(Said::Registers(vec![42]))));
    assert!(spent < ms(50), "{spent}");
}

#[test]
fn times_out_at_once_with_a_timeout_below_zero() {
    let mut bus = Bus::new(12, settings(9_600, None), line::Config::default());
    let (serial, clock) = (bus.client.serial(), bus.client.clock());
    let config = config(CLIENT, bus.settings);
    let (got, spent) = bus.on_client(move || async move {
        let timeout = Span::from_nanos(i64::MIN);
        let mut client = Client::open(&serial, &config, clock.clone(), timeout)
            .await
            .expect("the port opens");
        let request = read(Table::HoldingRegisters, 0, 1);
        bounded(&mut client, &clock, &request).await
    });
    assert_eq!(got, Some(Err(Failure::Timeout)));
    assert_eq!(spent, Span::ZERO);
}
