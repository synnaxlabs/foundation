//! The connection keeps the buffer of the stretches that a `send_parts` copies, with
//! its capacity. That capacity stays under twice the peer's largest message, and grows
//! only with the longest walk: a stretch and at most 1452 bytes of the run after it.

use std::iter;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use sim::Sim;
use sim::node::Node;
use transport::stream::Part;
use transport::{Address, Class, Code, Error, Transport};
use types::time::Span;

use crate::ALLOCATOR;
use crate::common::{CLIENT, PORT, SERVER, config, filled, part};

/// The longest chunk that noq-proto copies into its own buffer.
const COPIED_MAX: usize = 1452;
/// Long enough for the peer to read and acknowledge a message.
const PAUSE: Span = Span::from_nanos(100_000_000);
const CLOSED: Error = Error::PeerClosed { code: Code(0) };

/// A case: the peer's largest message, the parts, and the bound of the heap that the
/// connection keeps after their `send_parts`, over a `send` of the same bytes.
struct Case {
    name: &'static str,
    largest: usize,
    parts: Vec<Part>,
    bound: usize,
}

pub(crate) fn main() {
    let cases = [
        Case {
            name: "200 stretches of 8 bytes, to a peer of at most 1600 bytes",
            largest: 1_600,
            parts: (0..200)
                .map(|at| Part {
                    range: at * 20..at * 20 + 8,
                    zeros: 0,
                })
                .collect(),
            bound: 2 * 1_600,
        },
        Case {
            name: "a stretch of 100 bytes, then a run of 4000 bytes in one part",
            largest: 1 << 18,
            parts: walk(iter::once(200..4_200)),
            bound: 2 * (100 + COPIED_MAX),
        },
        Case {
            name: "a stretch of 100 bytes, then a run of 4000 bytes in parts of 8",
            largest: 1 << 18,
            parts: walk((0..500).map(|at| 200 + at * 8..208 + at * 8)),
            bound: 2 * (100 + COPIED_MAX),
        },
    ];
    for case in cases {
        let kept = run(&case);
        assert!(
            kept < case.bound,
            "{}: the connection keeps {kept} bytes over a send",
            case.name
        );
    }
}

/// The parts of a stretch of 100 bytes, then the ranges of `run`.
fn walk(run: impl Iterator<Item = Range<usize>>) -> Vec<Part> {
    iter::once(0..100)
        .chain(run)
        .map(|range| Part { range, zeros: 0 })
        .collect()
}

/// The heap bytes that the client holds after the `send_parts` of `case` on a new
/// connection, over those after a `send` of the same bytes, each once the peer
/// acknowledges it.
fn run(case: &Case) -> usize {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let at = SocketAddr::new(server.addresses()[0], PORT);
    serve(&server, case.largest);
    let parts = case.parts.clone();
    let bytes = parts.iter().map(|part| part.range.len()).sum();
    let large = parts.iter().map(|part| part.range.end).max().unwrap_or(0);
    let out = Arc::new(Mutex::new(0));
    let kept = Arc::clone(&out);
    sim.run_on(&client, move |node, tasks| async move {
        let config = config(&node, tasks, CLIENT);
        let pool = Rc::clone(&config.pool);
        let transport = Transport::new(config, part(&node, 0)).expect("a transport");
        let session = transport
            .dial(SERVER.public(), &[Address::Udp(at)])
            .await
            .expect("a session");
        let mut sender = session
            .open_sender(Class::Complete)
            .await
            .expect("a stream");
        let clock = node.clock();
        sender.send(filled(&pool, 8)).await.expect("sent");
        clock.sleep(PAUSE).await;
        sender.send(filled(&pool, bytes)).await.expect("sent");
        clock.sleep(PAUSE).await;
        let sent = ALLOCATOR.held();
        let block = filled(&pool, large);
        sender.send_parts(block, &parts).await.expect("sent");
        clock.sleep(PAUSE).await;
        *kept.lock().expect("not poisoned") = ALLOCATOR.held().saturating_sub(sent);
        sender.finish().expect("finished");
        clock.sleep(PAUSE).await;
        session.close(Code(0));
        clock.sleep(Span::MILLISECOND).await;
    })
    .expect("the run ends");
    *out.lock().expect("not poisoned")
}

/// Starts the server on `node`, with messages of at most `largest` bytes. It reads
/// each message to the end of the stream.
fn serve(node: &Node, largest: usize) {
    let own = node.clone();
    let shard = env::shards::Config {
        name: "server".into(),
        core: None,
    };
    let started = node.shards().start(shard, move |tasks| async move {
        let config = transport::Config {
            message_bytes_max: NonZeroUsize::new(largest).expect("not zero"),
            ..config(&own, tasks, SERVER)
        };
        let transport = Transport::new(config, part(&own, PORT)).expect("a transport");
        let session = transport.accept().await.expect("a session");
        let mut receiver = session.accept().await.expect("a stream").receiver;
        while receiver.recv().await.expect("a message").is_some() {}
        assert_eq!(session.closed().await, CLOSED, "the client closes");
    });
    drop(started.expect("a shard"));
}
