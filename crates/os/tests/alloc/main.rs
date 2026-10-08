//! A write and a read of one message over the loopback make no heap allocation after
//! the first poll of each end, which registers the socket. Nor does a write past the
//! unsent bound, or one that waits for the bound. This binary has no test
//! harness: the count covers each thread, and a harness allocates on its own thread
//! at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::future::poll_fn;
use std::io::IoSlice;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::sync::mpsc;
use std::task::{Context, Poll};
use std::time::Duration;

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
        unsent_bytes_max: NonZeroUsize::new(1 << 14).expect("invariant: 2^14 is not 0"),
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

/// Writes a block larger than the unsent bound until a write waits, then one write
/// more, which the peer's reads let through, with the allocations the polls made.
async fn write_past_the_bound(tcp: &mut Tcp, peer: &mut Tcp) -> u64 {
    let block = vec![7; options().unsent_bytes_max.get() * 2];
    let parts = [IoSlice::new(&block)];
    let mut buffer = vec![0; 1 << 20];
    let mut cx = Context::from_waker(std::task::Waker::noop());
    let (mut written, mut allocations) = (0, 0);
    loop {
        let (polled, made) = ALLOCATOR.count(|| tcp.poll_write(&mut cx, &parts));
        allocations += made;
        match polled {
            Poll::Ready(sent) => written += sent.expect("the stream takes bytes"),
            Poll::Pending => break,
        }
    }
    assert!(written > 0, "a write takes bytes before it waits");
    let polls = ready(|cx| {
        let polled = tcp.poll_write(cx, &parts);
        if polled.is_pending() {
            while let Poll::Ready(Ok(1..)) = peer.poll_read(cx, &mut buffer) {}
        }
        polled
    });
    let (sent, made) = timeout(BOUND, polls)
        .await
        .expect("the peer's reads free the bound");
    assert!(matches!(sent, Ok(1..)), "{sent:?}");
    allocations + made
}

/// The allocations of a write and a read on each end, after the first poll of each.
async fn count() -> [u64; 5] {
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
        write_past_the_bound(&mut client, &mut server).await,
    ]
}

fn main() {
    let (sent, received) = mpsc::channel();
    let handle = os::threads()
        .expect("the OS gives the cores of this process")
        .start("net-alloc", move || async move {
            sent.send(count().await).expect("main waits for the counts");
        })
        .expect("the thread starts");
    assert_eq!(handle.join(), Ok(()), "the thread ends with no panic");
    let counts = received.try_recv().expect("the thread sent its counts");
    assert_eq!(counts, [0; 5], "a poll after the first allocates nothing");
}
