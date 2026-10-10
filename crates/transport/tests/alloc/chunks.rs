//! A read holds at most 64 of the chunks that a message comes in. Each time the list
//! is full, the read copies it into one heap buffer, so a message in many tiny
//! frames cannot grow the list. A packet holds at most 1472 bytes, and noq-proto
//! keeps each packet's bytes in place until their spare bytes pass the larger of
//! 32 KiB and 1.5 times the bytes it holds, which these messages do not reach. So
//! the only heap block that holds all of a longer pattern is that buffer. A read of
//! 64 chunks makes no more allocations than a list that grows 1.5 times or more from
//! 1 slot to 64, and one of 65 chunks makes one more: the buffer, and no larger list.
//! A second copy makes none, and a read of a short message after it makes none: the
//! reader keeps its list. A `recv_into` makes the same allocations as a `recv`.

use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use block::{Block, Pool};
use sim::Sim;
use sim::node::Node;
use transport::{Address, Class, Error, Transport};
use types::time::Span;

use crate::ALLOCATOR;
use crate::common::{CLIENT, PORT, SERVER, config, filled, part};

/// The bytes of the pattern, longer than a packet.
const PATTERN: usize = 4096;
/// A message that comes in 64 chunks in this sim, a full list that the read never
/// copies.
const FULL: usize = 83_600;
/// A message that comes in 65 chunks in this sim, one past a full list.
const PAST: usize = 84_900;
/// The bytes of the message after the long one on the stream.
const SHORT: usize = 1000;
/// Where the pattern starts in a long message. Each packet but the last carries
/// between 1000 and 1472 bytes of a message, so the pattern lies past its first 64
/// packets and inside its first 128: only a second copy of a full list holds it.
const SECOND: usize = 110_000;
/// Where the pattern starts in a message of 220 000 bytes: past 128 packets of 1472
/// bytes, so no second copy holds it. A third copy needs 193 chunks, so packets of
/// fewer than 1140 bytes.
const LAST: usize = 200_000;
/// When the server reads after it accepts the stream, once both messages are in.
const READ: Span = Span::from_nanos(1_000_000_000);
/// How long the client lives after its sends: past the server's reads.
const LIVE: Span = Span::from_nanos(2_000_000_000);

/// The server's one poll of a read, the heap blocks that hold the pattern that the
/// poll frees, and the allocations the poll makes.
type Read = (Poll<Result<Option<Vec<u8>>, Error>>, u64, u64);

/// The [`Read`] of the long message and of the short one after it.
type Out = [Read; 2];

/// How the server reads.
#[derive(Clone, Copy, Debug)]
enum Mode {
    Recv,
    /// `recv_into` a buffer made before the count.
    Into,
}

pub(crate) fn main() {
    let cases = [
        (FULL, 0, 0),
        (PAST, 0, 1),
        (240_000, SECOND, 1),
        (220_000, LAST, 0),
    ];
    let recv = check(&cases, Mode::Recv, 0);
    let into = check(&cases, Mode::Into, cases.len());
    assert_eq!(into, recv, "a recv_into makes the allocations of a recv");
}

/// The pattern of run `n`. A later run can get the address of a block that an
/// earlier run freed, and the bytes it does not write keep that run's pattern, so
/// each run has its own. No block of an earlier run holds the pattern of a later one
/// while the runs are fewer than 12: the pattern of run 10, then the filler, holds
/// that of run 11.
fn pattern(n: usize) -> Vec<u8> {
    (0..=250).cycle().skip(n).take(PATTERN).collect()
}

/// Runs each case of a message of `len` bytes with the pattern at `at`, whose read
/// frees `copies` heap buffers, with reads in `mode`, as runs from `first`. Returns
/// the allocations of each case's long read.
fn check(cases: &[(usize, usize, u64)], mode: Mode, first: usize) -> Vec<u64> {
    let mut allocations = Vec::new();
    for (n, &(len, at, copies)) in (first..).zip(cases) {
        let pattern = &pattern(n);
        let [(read, freed, allocated), (next, _, next_allocated)] =
            run(pattern, len, at, mode);
        allocations.push(allocated);
        let Poll::Ready(Ok(Some(read))) = read else {
            panic!("{mode:?}, {len} bytes: the read gave {read:?}");
        };
        assert_eq!(
            read.len(),
            len,
            "{mode:?}, {len} bytes: the message's length"
        );
        let end = at.saturating_add(PATTERN);
        assert_eq!(
            read[at..end],
            *pattern,
            "{mode:?}, {len} bytes: the pattern"
        );
        assert_eq!(
            freed, copies,
            "{mode:?}, {len} bytes: the buffers with the pattern that the read frees"
        );
        let Poll::Ready(Ok(Some(next))) = next else {
            panic!(
                "{mode:?}, {len} bytes: the read of the short message gave {next:?}"
            );
        };
        assert_eq!(
            next, [0x5a; SHORT],
            "{mode:?}, {len} bytes: the short message"
        );
        assert_eq!(
            next_allocated, 0,
            "{mode:?}, {len} bytes: the reader keeps its list for the short message"
        );
    }
    let [full, past, second, _] = allocations[..] else {
        unreachable!("four cases")
    };
    assert!(
        full <= 12,
        "a list that grows 1.5 times or more reaches 64 slots in 12 allocations, \
         not {full}"
    );
    assert_eq!(
        past.checked_sub(full),
        Some(1),
        "the copy of a full list allocates only its buffer"
    );
    assert_eq!(second, past, "a second copy allocates nothing");
    allocations
}

/// The [`Out`] of the server's reads of a message of `len` bytes with `pattern` at
/// `at`, then of a short message, on one stream, with reads in `mode`.
fn run(pattern: &[u8], len: usize, at: usize, mode: Mode) -> Out {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let address = SocketAddr::new(server.addresses()[0], PORT);
    let out = Arc::new(Mutex::new([(Poll::Pending, 0, 0), (Poll::Pending, 0, 0)]));
    serve(&server, pattern.to_vec(), mode, Arc::clone(&out));
    let message = pattern.to_vec();
    sim.run_on(&client, move |node, tasks| async move {
        let config = config(&node, tasks, CLIENT);
        let pool = Rc::clone(&config.pool);
        let transport = Transport::new(config, part(&node, 0)).expect("a transport");
        let session = transport
            .dial(SERVER.public(), &[Address::Udp(address)])
            .await
            .expect("a session");
        let mut sender = session
            .open_sender(Class::Complete)
            .await
            .expect("a stream");
        let long = patterned(&pool, &message, len, at);
        sender.send(long).await.expect("sent");
        sender.send(filled(&pool, SHORT)).await.expect("sent");
        node.clock().sleep(LIVE).await;
    })
    .expect("the run ends");
    out.lock().expect("not poisoned").clone()
}

/// Starts the server on `node`. Once both messages are in, it reads each in one poll
/// in `mode` and puts the [`Out`] of those polls for `pattern` in `out`.
fn serve(node: &Node, pattern: Vec<u8>, mode: Mode, out: Arc<Mutex<Out>>) {
    let own = node.clone();
    let shard = env::shards::Config {
        name: "server".into(),
        core: None,
    };
    let started = node.shards().start(shard, move |tasks| async move {
        let config = config(&own, tasks, SERVER);
        let transport = Transport::new(config, part(&own, PORT)).expect("a transport");
        let session = transport.accept().await.expect("a session");
        let mut receiver = session.accept().await.expect("a stream").receiver;
        own.clock().sleep(READ).await;
        let mut buffer = vec![0; 240_000];
        for read in out.lock().expect("not poisoned").iter_mut() {
            *read = match mode {
                Mode::Recv => {
                    let (poll, freed, allocated) =
                        once(&pattern, pin!(receiver.recv()));
                    let message =
                        poll.map(|poll| poll.map(|block| block.map(|b| b.to_vec())));
                    (message, freed, allocated)
                }
                Mode::Into => {
                    let (poll, freed, allocated) =
                        once(&pattern, pin!(receiver.recv_into(&mut buffer)));
                    let message = poll.map(|poll| {
                        poll.map(|len| len.map(|len| buffer[..len].to_vec()))
                    });
                    (message, freed, allocated)
                }
            };
        }
        drop(receiver);
    });
    drop(started.expect("a shard"));
}

/// One poll of `read`, the heap blocks that hold `pattern` that it frees, and the
/// allocations it makes.
fn once<T>(
    pattern: &[u8],
    read: Pin<&mut impl Future<Output = T>>,
) -> (Poll<T>, u64, u64) {
    let mut cx = Context::from_waker(Waker::noop());
    let ((poll, freed), allocated) =
        ALLOCATOR.count(|| ALLOCATOR.freed_holding(pattern, || read.poll(&mut cx)));
    (poll, freed, allocated)
}

/// A block of `len` bytes from `pool` with `pattern` at `at`.
fn patterned(pool: &Pool, pattern: &[u8], len: usize, at: usize) -> Block {
    let mut block = pool.alloc(len).expect("the pool has room");
    block.fill(0x5a);
    block[at..at.saturating_add(pattern.len())].copy_from_slice(pattern);
    block.freeze()
}
