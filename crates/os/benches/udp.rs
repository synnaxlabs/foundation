//! The cost of UDP over the loopback through `os::net`: one datagram against a plain
//! Tokio socket, and a batch sent in one call against the same datagrams sent one by
//! one, also on a socket that cannot use GSO and with a segment over the path MTU.
//! Each sample sends and then receives all the bytes that arrive, except in `register`.
use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::os::fd::AsRawFd;

use divan::Bencher;
use env::net::udp::{self, Meta, Receiver, Sender, Transmit};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::net::UdpSocket;
use tokio::runtime::{Builder, Runtime};

#[cfg(target_os = "linux")]
#[path = "../tests/common/gso.rs"]
mod gso;

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

/// Sends `contents` `calls` times from `source`, and receives all the bytes, once per
/// sample.
fn bench_os(
    bencher: Bencher<'_, '_>,
    (mut sender, mut receiver): (Sender, Receiver),
    contents: &[u8],
    segment: usize,
    calls: usize,
    source: Option<IpAddr>,
) {
    let runtime = runtime();
    let mut storage = vec![vec![0; SEGMENT * receiver.batch_max().get()]; 4];
    let mut buffers: Vec<_> = storage.iter_mut().map(|b| IoSliceMut::new(b)).collect();
    let mut meta = [Meta::default(); 4];
    let bytes = contents.len() * calls;
    bencher.bench_local(|| {
        runtime.block_on(async {
            let transmit = Transmit {
                source,
                ..transmit(receiver.local(), contents, segment)
            };
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
    bench_os(bencher, pair(), &[7; 64], 0, 1, None);
}

/// The same datagram from a given source address.
#[divan::bench(sample_count = SAMPLES)]
fn os_datagram_source(bencher: Bencher<'_, '_>) {
    bench_os(
        bencher,
        pair(),
        &[7; 64],
        0,
        1,
        Some(Ipv4Addr::LOCALHOST.into()),
    );
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
    bench_os(
        bencher,
        pair(),
        &vec![7; SEGMENT * DATAGRAMS],
        SEGMENT,
        1,
        None,
    );
}

/// The batch of [`os_batch`] on a socket whose kernel refuses GSO, so that it goes
/// out a datagram at a time.
#[cfg(target_os = "linux")]
#[divan::bench(sample_count = SAMPLES)]
fn os_batch_without_gso(bencher: Bencher<'_, '_>) {
    let (sender, receiver) = pair();
    gso::refuse(sender.local());
    let contents = vec![7; SEGMENT * DATAGRAMS];
    bench_os(bencher, (sender, receiver), &contents, SEGMENT, 1, None);
}

/// A transmit of `TRANSMIT_BYTES_MAX` bytes on the IPv6 loopback, in segments of
/// 65,500 bytes. With its headers, the full segment is over the 65,536-byte MTU of the
/// Linux loopback, so it is lost, and only the 7-byte last datagram arrives.
#[cfg(target_os = "linux")]
#[divan::bench(sample_count = SAMPLES)]
fn os_batch_over_mtu(bencher: Bencher<'_, '_>) {
    let net = os::net();
    let config = udp::Config {
        local: SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 0),
        send_buffer_bytes: 1 << 20,
        recv_buffer_bytes: 1 << 22,
    };
    let (mut sender, _) = net.udp(&config).expect("the loopback has a free port");
    let (_, mut receiver) = net.udp(&config).expect("the loopback has a free port");
    let runtime = runtime();
    let contents = vec![7; udp::TRANSMIT_BYTES_MAX];
    let mut storage = [0; 64];
    let mut buffers = [IoSliceMut::new(&mut storage)];
    let mut meta = [Meta::default()];
    bencher.bench_local(|| {
        runtime.block_on(async {
            send(&mut sender, &transmit(receiver.local(), &contents, 65_500)).await;
            receive(&mut receiver, &mut buffers, &mut meta, 7).await;
        });
    });
}

/// The datagrams of [`os_batch`], sent one per call.
#[divan::bench(sample_count = SAMPLES)]
fn os_one_by_one(bencher: Bencher<'_, '_>) {
    bench_os(bencher, pair(), &[7; SEGMENT], 0, DATAGRAMS, None);
}

/// The registration that a sender makes at the first `EAGAIN` of a send: a write
/// interest on its raw descriptor, and the drop when the send ends.
#[divan::bench(sample_count = SAMPLES)]
#[expect(
    clippy::disallowed_methods,
    reason = "`os::net` gives no descriptor, and the bench needs one"
)]
fn register(bencher: Bencher<'_, '_>) {
    let runtime = runtime();
    let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0));
    let socket = socket.expect("the loopback has a free port");
    let fd = socket.as_raw_fd();
    let _guard = runtime.enter();
    bencher.bench_local(|| {
        let registered = AsyncFd::with_interest(fd, Interest::WRITABLE);
        drop(registered.expect("the I/O driver registers the socket"));
    });
}
