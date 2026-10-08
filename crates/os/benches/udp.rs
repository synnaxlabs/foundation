//! The cost of UDP over the loopback through `os::net`: one datagram against a plain
//! Tokio socket, and a batch sent in one call against the same datagrams sent one by
//! one. Each sample sends and then receives all the bytes.

use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;

use divan::Bencher;
use env::net::udp::{self, Meta, Receiver, Sender, Transmit};
use tokio::net::UdpSocket;
use tokio::runtime::{Builder, Runtime};

/// The size of each datagram of a batch: a QUIC packet on a 1,280-byte path.
const SEGMENT: usize = 1_200;
/// The datagrams of a batch: the most of `SEGMENT` bytes that one transmit takes.
const DATAGRAMS: usize = 54;
const SAMPLES: u32 = 2_000;

fn main() {
    divan::main();
}

fn runtime() -> Runtime {
    Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("the runtime builds")
}

/// A sender and the receiver of another socket, both on the IPv4 loopback.
fn pair() -> (Sender, Receiver) {
    let net = os::net();
    let config = udp::Config {
        local: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
        send_buffer_bytes: 1 << 20,
        recv_buffer_bytes: 1 << 22,
    };
    let (sender, _) = net.udp(&config).expect("the loopback has a free port");
    let (_, receiver) = net.udp(&config).expect("the loopback has a free port");
    (sender, receiver)
}

fn transmit(destination: SocketAddr, contents: &[u8], segment: usize) -> Transmit<'_> {
    Transmit {
        destination,
        source: None,
        ecn: None,
        contents,
        segment: NonZeroUsize::new(segment),
    }
}

async fn send(sender: &mut Sender, transmit: &Transmit<'_>) {
    let sent = poll_fn(|cx| sender.poll_send(cx, transmit)).await;
    sent.expect("the loopback takes the datagrams");
}

/// Receives until `bytes` bytes arrive.
async fn receive(
    receiver: &mut Receiver,
    buffers: &mut [IoSliceMut<'_>],
    meta: &mut [Meta],
    bytes: usize,
) {
    let mut arrived = 0;
    while arrived < bytes {
        let batches = poll_fn(|cx| receiver.poll_recv(cx, buffers, meta)).await;
        let batches = batches.expect("the receive succeeds");
        arrived += meta[..batches].iter().map(|m| m.len).sum::<usize>();
    }
}

/// Sends `contents` `calls` times, and receives all the bytes, once per sample.
fn bench_os(bencher: Bencher<'_, '_>, contents: &[u8], segment: usize, calls: usize) {
    let runtime = runtime();
    let (mut sender, mut receiver) = pair();
    let mut storage = vec![vec![0; SEGMENT * receiver.batch_max().get()]; 4];
    let mut buffers: Vec<_> = storage.iter_mut().map(|b| IoSliceMut::new(b)).collect();
    let mut meta = [Meta::default(); 4];
    let bytes = contents.len() * calls;
    bencher.bench_local(|| {
        runtime.block_on(async {
            let transmit = transmit(receiver.local(), contents, segment);
            for _ in 0..calls {
                send(&mut sender, &transmit).await;
            }
            receive(&mut receiver, &mut buffers, &mut meta, bytes).await;
        });
    });
}

/// One 64-byte datagram through `os::net`.
#[divan::bench(sample_count = SAMPLES)]
fn os_datagram(bencher: Bencher<'_, '_>) {
    bench_os(bencher, &[7; 64], 0, 1);
}

/// The same datagram through a plain Tokio socket, for comparison.
#[divan::bench(sample_count = SAMPLES)]
fn tokio_datagram(bencher: Bencher<'_, '_>) {
    let runtime = runtime();
    let bind = || UdpSocket::bind((Ipv4Addr::LOCALHOST, 0));
    let (sender, receiver) = runtime.block_on(async { (bind().await, bind().await) });
    let sender = sender.expect("the loopback has a free port");
    let receiver = receiver.expect("the loopback has a free port");
    let remote = receiver
        .local_addr()
        .expect("a bound socket has an address");
    let mut buffer = [0; 2_048];
    bencher.bench_local(|| {
        runtime.block_on(async {
            let sent = sender.send_to(&[7; 64], remote).await;
            sent.expect("the loopback takes the datagram");
            let arrived = receiver.recv_from(&mut buffer).await;
            arrived.expect("the receive succeeds");
        });
    });
}

/// A batch of `DATAGRAMS` datagrams of `SEGMENT` bytes, sent in one call.
#[divan::bench(sample_count = SAMPLES)]
fn os_batch(bencher: Bencher<'_, '_>) {
    bench_os(bencher, &vec![7; SEGMENT * DATAGRAMS], SEGMENT, 1);
}

/// The datagrams of [`os_batch`], sent one per call.
#[divan::bench(sample_count = SAMPLES)]
fn os_one_by_one(bencher: Bencher<'_, '_>) {
    bench_os(bencher, &[7; SEGMENT], 0, DATAGRAMS);
}
