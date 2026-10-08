//! A write and a read of one message over the loopback, and a send and a receive of
//! a UDP batch, make no heap allocation after the first poll of each end, which
//! registers the socket. This binary has no test harness: the count covers each
//! thread, and a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::future::poll_fn;
use std::io::{IoSlice, IoSliceMut};
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::sync::mpsc;
use std::task::{Context, Poll};
use std::time::Duration;

use env::net::udp::{self, Meta, Receiver, Sender, Transmit};
use env::net::{Error, Tcp, tcp};
use tokio::time::timeout;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const MESSAGE: &[u8] = b"a message of a few bytes";
/// The bound of each wait.
const BOUND: Duration = Duration::from_secs(10);

fn options() -> tcp::Options {
    tcp::Options {
        send_buffer_bytes: 1 << 16,
        recv_buffer_bytes: 1 << 16,
        unsent_bytes_max: 1 << 14,
        delayed: false,
    }
}

/// Polls `poll` until it is ready, with the allocations the polls made. The runtime
/// turns the I/O driver between polls, and that is not counted.
async fn ready<T>(mut poll: impl FnMut(&mut Context<'_>) -> Poll<T>) -> (T, u64) {
    let mut allocations = 0;
    let value = poll_fn(|cx| {
        let (polled, made) = ALLOCATOR.count(|| poll(cx));
        allocations += made;
        polled
    })
    .await;
    (value, allocations)
}

/// Writes `MESSAGE` to `tcp` in two parts, with the allocations the polls made.
async fn write(tcp: &mut Tcp) -> u64 {
    let parts = [IoSlice::new(&MESSAGE[..4]), IoSlice::new(&MESSAGE[4..])];
    let (written, allocations) = timeout(BOUND, ready(|cx| tcp.poll_write(cx, &parts)))
        .await
        .expect("the send buffer takes one message in the bound");
    assert_eq!(written, Ok(MESSAGE.len()), "one write takes the message");
    allocations
}

/// Reads `MESSAGE` from `tcp`, with the allocations the polls made.
async fn read(tcp: &mut Tcp) -> u64 {
    let mut buffer = [0; 64];
    let polls = ready(|cx| tcp.poll_read(cx, &mut buffer));
    let (received, allocations): (Result<usize, Error>, u64) = timeout(BOUND, polls)
        .await
        .expect("the loopback delivers the message in the bound");
    assert_eq!(received, Ok(MESSAGE.len()), "one read gives the message");
    assert_eq!(
        &buffer[..MESSAGE.len()],
        MESSAGE,
        "the bytes are those sent"
    );
    allocations
}

/// The allocations of a write and a read on each end, after the first poll of each.
async fn count() -> [u64; 4] {
    let net = os::net();
    let local = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0);
    let listen = tcp::Listen {
        local,
        backlog: 1,
        options: options(),
    };
    let mut listener = net.listen(&listen).expect("the loopback has a free port");
    let connect = tcp::Config {
        remote: listener.local(),
        options: options(),
    };
    let mut client = (net.connect(&connect).await).expect("the listener accepts");
    let mut server = poll_fn(|cx| listener.poll_accept(cx))
        .await
        .expect("a stream waits in the backlog");
    write(&mut client).await;
    read(&mut server).await;
    [
        write(&mut client).await,
        read(&mut server).await,
        write(&mut server).await,
        read(&mut client).await,
    ]
}

/// Sends `MESSAGE` to `receiver` as a batch of two datagrams, with the allocations
/// the polls made.
async fn send(sender: &mut Sender, receiver: &Receiver) -> u64 {
    let transmit = Transmit {
        destination: receiver.local(),
        source: None,
        ecn: None,
        contents: MESSAGE,
        segment: NonZeroUsize::new(MESSAGE.len() / 2 + 1),
    };
    let (sent, allocations) =
        timeout(BOUND, ready(|cx| sender.poll_send(cx, &transmit)))
            .await
            .expect("the send buffer takes one batch in the bound");
    assert_eq!(sent, Ok(()), "the batch goes out");
    allocations
}

/// Receives the batch of `send`, with the allocations the polls made.
async fn receive(receiver: &mut Receiver) -> u64 {
    let mut buffers = [[0; 64]; 2];
    let [first, second] = &mut buffers;
    let mut slices = [IoSliceMut::new(first), IoSliceMut::new(second)];
    let mut meta = [Meta::default(); 2];
    let mut bytes = 0;
    let mut allocations = 0;
    while bytes < MESSAGE.len() {
        let polls = ready(|cx| receiver.poll_recv(cx, &mut slices, &mut meta));
        let (batches, made) = timeout(BOUND, polls)
            .await
            .expect("the loopback delivers the batch in the bound");
        let batches = batches.expect("the receive succeeds");
        bytes += meta[..batches].iter().map(|m| m.len).sum::<usize>();
        allocations += made;
    }
    assert_eq!(bytes, MESSAGE.len(), "the batch arrives whole");
    allocations
}

/// The allocations of a UDP send and receive, after the first poll of each half.
async fn count_udp() -> [u64; 2] {
    let config = udp::Config {
        local: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
        send_buffer_bytes: 1 << 16,
        recv_buffer_bytes: 1 << 16,
    };
    let net = os::net();
    let (mut sender, mut receiver) =
        net.udp(&config).expect("the loopback has a free port");
    send(&mut sender, &receiver).await;
    receive(&mut receiver).await;
    [
        send(&mut sender, &receiver).await,
        receive(&mut receiver).await,
    ]
}

fn main() {
    let (sent, received) = mpsc::channel();
    let handle = os::threads()
        .expect("the OS gives the cores of this process")
        .start("net-alloc", move || async move {
            let counts = (count().await, count_udp().await);
            sent.send(counts).expect("main waits for the counts");
        })
        .expect("the thread starts");
    assert_eq!(handle.join(), Ok(()), "the thread ends with no panic");
    let counts = received.try_recv().expect("the thread sent its counts");
    let (tcp, udp) = counts;
    assert_eq!(tcp, [0; 4], "a TCP poll after the first allocates nothing");
    assert_eq!(udp, [0; 2], "a UDP poll after the first allocates nothing");
}
