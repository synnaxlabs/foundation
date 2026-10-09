//! Dials that fail, with no session from the peer, do not grow the heap of the
//! transport that dials.

use std::net::SocketAddr;
use std::task::Waker;

use sim::Sim;
use transport::{Address, Error, Transport};

use crate::ALLOCATOR;
use crate::common::{CLIENT, SERVER, config, part};

/// The dials before the first count of the heap.
const FIRST: usize = 50;
/// The dials before the last count of the heap.
const LAST: usize = 1050;

pub(crate) fn main() {
    let (first, last) = run();
    let grown = last.saturating_sub(first);
    assert!(
        grown < (LAST - FIRST) * size_of::<Waker>(),
        "the run holds {first} heap bytes after {FIRST} dials, and {last} after {LAST}"
    );
}

/// The heap bytes of the whole run after [`FIRST`] and after [`LAST`] dials to an
/// address where no peer answers.
fn run() -> (usize, usize) {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let dead = SocketAddr::new(server.addresses()[0], 1);
    let counts = sim.run_on(&client, move |node, tasks| async move {
        let config = config(&node, tasks, CLIENT);
        let transport = Transport::new(config, part(&node, 0)).expect("a transport");
        let mut counts = [0; 2];
        for dial in 1..=LAST {
            let dialed = transport.dial(SERVER.public(), &[Address::Udp(dead)]).await;
            let attempts = vec![(Address::Udp(dead), Error::TimedOut)];
            let unreachable = Error::Unreachable {
                peer: SERVER.public(),
                attempts,
            };
            assert_eq!(dialed.err(), Some(unreachable), "dial {dial}");
            if dial == FIRST || dial == LAST {
                counts[usize::from(dial == LAST)] = ALLOCATOR.held();
            }
        }
        (counts[0], counts[1])
    });
    counts.expect("the run ends")
}
