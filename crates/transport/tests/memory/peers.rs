//! Peers that each come with a new key, and whose sessions end, do not grow the heap of
//! the transport that accepts them.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use sim::Sim;
use sim::node::Node;
use transport::{Address, Code, Transport};
use types::ed25519::PrivateKey;
use types::time::Span;

use crate::ALLOCATOR;
use crate::common::{PORT, SERVER, config, part};

/// The peers before the first count of the heap.
const FIRST: usize = 50;
/// The peers before the last count of the heap.
const LAST: usize = 1050;
/// The bytes of a key. A transport that kept anything for each ended peer would hold
/// at least its key.
const KEY: usize = 32;
/// How long the server waits for the sessions to drain before a count.
const DRAIN: Span = Span::from_nanos(5_000_000_000);
/// How long the client waits at a count: past the drain.
const LIVE: Span = Span::from_nanos(10_000_000_000);

pub(crate) fn main() {
    let (first, last) = run();
    let grown = last.saturating_sub(first);
    panic!("PROBE END");
    assert!(
        grown < (LAST - FIRST) * KEY,
        "the run holds {first} heap bytes after {FIRST} peers, and {last} after \
         {LAST}"
    );
}

/// The heap bytes of the whole run after [`FIRST`] and after [`LAST`] peers.
fn run() -> (usize, usize) {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let at = [Address::Udp(SocketAddr::new(server.addresses()[0], PORT))];
    let out = Arc::new(Mutex::new((0, 0)));
    serve(&server, Arc::clone(&out));
    sim.run_on(&client, move |node, tasks| async move {
        for peer in 0..LAST {
            let mut key = [0xa5; 32];
            key[..8].copy_from_slice(&peer.to_le_bytes());
            let config = config(&node, tasks.clone(), PrivateKey(key));
            let transport =
                Transport::new(config, part(&node, 0)).expect("a transport");
            let dialed = transport.dial(SERVER.public(), &at).await;
            dialed.expect("a session").close(Code(1));
            drop(transport);
            if (peer + 1) % 50 == 0 {
                node.clock().sleep(LIVE).await;
            }
        }
    })
    .expect("the run ends");
    *out.lock().expect("not poisoned")
}

/// Starts the server on `node`. It accepts each session, and puts its counts of the
/// heap in `out`.
fn serve(node: &Node, out: Arc<Mutex<(usize, usize)>>) {
    let own = node.clone();
    let shard = env::shards::Config {
        name: "server".into(),
        core: None,
    };
    let started = node.shards().start(shard, move |tasks| async move {
        let config = config(&own, tasks, SERVER);
        let transport = Transport::new(config, part(&own, PORT)).expect("a transport");
        let mut counts = [0; 2];
        for peer in 1..=LAST {
            drop(transport.accept().await.expect("a session"));
            if peer % 50 == 0 {
                own.clock().sleep(DRAIN).await;
                eprintln!("PROBE {peer} {}", ALLOCATOR.held());
                counts[usize::from(peer == LAST)] = ALLOCATOR.held();
            }
        }
        *out.lock().expect("not poisoned") = (counts[0], counts[1]);
    });
    drop(started.expect("a shard"));
}
