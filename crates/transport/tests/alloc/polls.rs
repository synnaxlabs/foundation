//! A read keeps its list of chunks and the buffer of its message across polls, so no
//! allocation repeats on each poll. A message that comes over at least 64 polls makes
//! at most twice the allocations of a read of the whole message in one poll. A whole
//! message that waits for a block makes no allocation after its first poll, and the
//! read of a short message after it makes none: the reader keeps its list.

use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Waker};

use sim::Sim;
use sim::node::Node;
use transport::{Address, Class, Transport};
use types::time::Span;

use crate::ALLOCATOR;
use crate::common::{CLIENT, PORT, SERVER, config, fill, filled, part};

/// The bytes of the message after the long one on the stream.
const SHORT: usize = 1000;
/// The time between two polls of a read while its message comes, shorter than the
/// time between two packets.
const EVERY: Span = Span::from_nanos(20_000);
/// When the server reads after it accepts the stream, once the messages are in.
const READ: Span = Span::from_nanos(1_000_000_000);
/// How long the client lives after its sends: past the server's reads.
const LIVE: Span = Span::from_nanos(2_000_000_000);
/// The polls of a whole message that waits for a block.
const WAITS: usize = 8;

/// Whether each poll of the server's reads was ready, and the allocations it made.
type Out = Vec<(bool, u64)>;

/// How the server reads the long message.
#[derive(Clone, Copy, Debug)]
enum Mode {
    /// Once it is in, in one poll.
    Whole,
    /// Each [`EVERY`] from when the stream comes.
    Coming,
    /// Once it is in, [`WAITS`] polls with no block in the pool, then one with blocks.
    Waiting,
}

pub(crate) fn main() {
    for len in [100_000, 240_000] {
        let whole = run(len, Mode::Whole);
        let [(true, whole)] = whole[..] else {
            panic!("{len} bytes: the whole message in one poll gave {whole:?}");
        };
        let coming = run(len, Mode::Coming);
        let polls = coming.len();
        let allocated: u64 = coming.iter().map(|&(_, allocated)| allocated).sum();
        assert!(polls >= 64, "{len} bytes: {polls} polls");
        assert!(
            allocated <= 2 * whole,
            "{len} bytes: {allocated} allocations over {polls} polls, against {whole} \
             in one poll"
        );
        let waiting = run(len, Mode::Waiting);
        let [(false, _), ref waits @ .., (true, block), (true, short)] = waiting[..]
        else {
            panic!("{len} bytes: the waiting reads gave {waiting:?}");
        };
        assert_eq!(
            waits,
            [(false, 0); WAITS - 1],
            "{len} bytes: the polls after the first of a message that waits for a block"
        );
        assert_eq!(
            (block, short),
            (0, 0),
            "{len} bytes: the read with a block, and of the short message after it"
        );
    }
}

/// The [`Out`] of the server's reads in `mode` of a message of `len` bytes, and in
/// [`Mode::Waiting`] of the short message after it, on one stream.
fn run(len: usize, mode: Mode) -> Out {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let address = SocketAddr::new(server.addresses()[0], PORT);
    let out = Arc::new(Mutex::new(Vec::new()));
    serve(&server, len, mode, Arc::clone(&out));
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
        sender.send(filled(&pool, len)).await.expect("sent");
        sender.send(filled(&pool, SHORT)).await.expect("sent");
        node.clock().sleep(LIVE).await;
    })
    .expect("the run ends");
    out.lock().expect("not poisoned").clone()
}

/// Starts the server on `node`. It reads a message of `len` bytes in `mode`, and puts
/// the [`Out`] of its polls in `out`.
fn serve(node: &Node, len: usize, mode: Mode, out: Arc<Mutex<Out>>) {
    let own = node.clone();
    let shard = env::shards::Config {
        name: "server".into(),
        core: None,
    };
    let started = node.shards().start(shard, move |tasks| async move {
        let config = config(&own, tasks, SERVER);
        let full = matches!(mode, Mode::Waiting).then(|| fill(&config.pool, len));
        let transport = Transport::new(config, part(&own, PORT)).expect("a transport");
        let session = transport.accept().await.expect("a session");
        let mut receiver = session.accept().await.expect("a stream").receiver;
        let clock = own.clock();
        let mut polls = Vec::new();
        {
            let mut recv = pin!(receiver.recv());
            match mode {
                Mode::Whole => {
                    clock.sleep(READ).await;
                    polls.push(once(recv.as_mut()));
                }
                Mode::Coming => loop {
                    let poll = once(recv.as_mut());
                    polls.push(poll);
                    if poll.0 {
                        break;
                    }
                    clock.sleep(EVERY).await;
                },
                Mode::Waiting => {
                    clock.sleep(READ).await;
                    for _ in 0..WAITS {
                        polls.push(once(recv.as_mut()));
                    }
                    drop(full);
                    polls.push(once(recv.as_mut()));
                }
            }
        }
        if matches!(mode, Mode::Waiting) {
            polls.push(once(pin!(receiver.recv())));
        }
        *out.lock().expect("not poisoned") = polls;
    });
    drop(started.expect("a shard"));
}

/// Whether one poll of `read` is ready, and the allocations it makes.
fn once<T>(read: Pin<&mut impl Future<Output = T>>) -> (bool, u64) {
    let mut cx = Context::from_waker(Waker::noop());
    ALLOCATOR.count(|| read.poll(&mut cx).is_ready())
}
