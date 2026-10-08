//! The table of a [`Transport`](crate::Transport): the open session to each node, the
//! dial that runs for it, and the dialed sessions that wait for `accept`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll, Waker};

use types::ed25519::PublicKey;
use types::hash::Map;

use crate::{Error, Peer, Session, quic};

/// The fewest entries at which a prune runs.
const FLOOR: usize = 16;

#[derive(Default)]
pub(crate) struct Table {
    nodes: Map<PublicKey, Entry>,
    /// The entry count at which the next prune runs: twice the count after the last
    /// one, and at least [`FLOOR`]. So peers that come with new keys cannot grow the
    /// table past twice its live entries.
    limit: usize,
    /// The sessions that dials made, in the order they connected, for `accept`.
    dialed: VecDeque<Session>,
    /// The wakers of the `accept` calls that wait.
    accepting: Vec<Waker>,
}

/// What a [`Table`] knows of one node.
#[derive(Default)]
struct Entry {
    open: Weak<quic::Session>,
    dial: Option<Rc<Attempt>>,
}

/// What [`Table::find`] gives.
pub(crate) enum Found {
    Open(Session),
    Dialing(Rc<Attempt>),
    /// No session and no dial: the caller starts this attempt.
    Start(Rc<Attempt>),
}

impl Table {
    /// The open session to `node`, or else the dial that runs for it, or else a new
    /// attempt that the caller must start and end with [`Table::dialed`].
    pub(crate) fn find(&mut self, node: PublicKey) -> Found {
        let entry = self.entry(node);
        if let Some(session) = Session::upgrade(&entry.open) {
            return Found::Open(session);
        }
        if let Some(attempt) = &entry.dial {
            return Found::Dialing(Rc::clone(attempt));
        }
        let attempt = Rc::new(Attempt::default());
        entry.dial = Some(Rc::clone(&attempt));
        Found::Start(attempt)
    }

    /// Ends the attempt to `node` with what its dial gave. A session becomes the open
    /// one and waits for `accept`. After an error, the attempt gives the open session
    /// that the peer opened meanwhile, if one is open.
    pub(crate) fn dialed(&mut self, node: PublicKey, dialed: Result<Session, Error>) {
        let entry = self.nodes.get_mut(&node);
        let entry = entry.expect("invariant: a dial keeps its entry");
        let attempt = entry.dial.take().expect("invariant: one dial ends it");
        let result = match dialed {
            Ok(session) => {
                entry.open = session.downgrade();
                self.dialed.push_back(session.clone());
                self.accepting.drain(..).for_each(Waker::wake);
                Ok(session)
            }
            Err(error) => Session::upgrade(&entry.open).ok_or(error),
        };
        if result.is_err() {
            self.nodes.remove(&node);
        }
        attempt.end(result);
    }

    /// Makes `session`, which a peer opened, the open one to its node. A client's
    /// session has no node.
    pub(crate) fn accepted(&mut self, session: &Session) {
        if let Peer::Node(node) = session.peer() {
            self.entry(node).open = session.downgrade();
        }
    }

    /// The entry of `node`, new when it has none. First it drops each entry with no
    /// session and no dial, when the table is at its limit.
    fn entry(&mut self, node: PublicKey) -> &mut Entry {
        if self.nodes.len() >= self.limit {
            let live = |_: &PublicKey, entry: &mut Entry| {
                entry.dial.is_some() || entry.open.strong_count() > 0
            };
            self.nodes.retain(live);
            self.limit = self.nodes.len().saturating_mul(2).max(FLOOR);
        }
        self.nodes.entry(node).or_default()
    }

    /// The next session that a dial made, or `None`, and then `cx` wakes when one
    /// comes.
    pub(crate) fn poll_dialed(&mut self, cx: &Context<'_>) -> Option<Session> {
        let session = self.dialed.pop_front();
        if session.is_none() {
            quic::register(&mut self.accepting, cx.waker());
        }
        session
    }
}

/// One dial to a node, which each caller that dials the node meanwhile waits on.
#[derive(Default)]
pub(crate) struct Attempt(RefCell<State>);

#[derive(Default)]
struct State {
    result: Option<Result<Session, Error>>,
    waiting: Vec<Waker>,
}

impl Attempt {
    /// Ready with what the dial gave.
    pub(crate) fn poll(&self, cx: &Context<'_>) -> Poll<Result<Session, Error>> {
        let mut state = self.0.borrow_mut();
        if let Some(result) = &state.result {
            return Poll::Ready(result.clone());
        }
        quic::register(&mut state.waiting, cx.waker());
        Poll::Pending
    }

    fn end(&self, result: Result<Session, Error>) {
        let mut state = self.0.borrow_mut();
        state.result = Some(result);
        state.waiting.drain(..).for_each(Waker::wake);
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::pin::pin;
    use std::rc::Rc;

    use sim::node::Node;
    use types::ed25519::{PrivateKey, PublicKey};
    use types::time::Span;

    use crate::testing::{self, CLIENT, SERVER};
    use crate::{Address, Client, Code, Error, Peer, Transport};

    /// An address on `node` where nothing answers.
    fn dead(node: &Node) -> [Address; 1] {
        [Address::Udp(SocketAddr::new(node.addresses()[0], 1))]
    }

    /// What a dial to `peer` at [`dead`] on `node` gives.
    fn unreachable(peer: PublicKey, node: &Node) -> Error {
        let [address] = dead(node);
        let attempts = vec![(address, Error::TimedOut)];
        Error::Unreachable { peer, attempts }
    }

    /// Gives the close of a shard that ends now time to go out: a shard that ends
    /// drops its tasks.
    async fn linger(node: &Node) {
        node.clock().sleep(Span::MILLISECOND).await;
    }

    #[test]
    fn two_dials_at_once_make_one_session_that_accept_gives_once() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        testing::transport(&server, SERVER, |transport, _| async move {
            let session = transport.accept().await.expect("a session");
            assert_eq!(session.peer(), Peer::Node(CLIENT.public()));
            let closed = Error::PeerClosed { code: Code(3) };
            assert_eq!(session.closed().await, closed);
            assert!(testing::poll_once(pin!(transport.accept())).await.is_none());
        });
        testing::transport(&client, CLIENT, move |transport, _| async move {
            let (first, second) = testing::join(
                transport.dial(SERVER.public(), &at),
                transport.dial(SERVER.public(), &at),
            )
            .await;
            let first = first.expect("a session");
            let accepted = transport.accept().await.expect("the dialed session");
            assert_eq!(accepted.peer(), Peer::Node(SERVER.public()));
            assert!(testing::poll_once(pin!(transport.accept())).await.is_none());
            first.close(Code(3));
            for session in [second.expect("a session"), accepted] {
                assert_eq!(session.closed().await, Error::Closed { code: Code(3) });
            }
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn an_accept_that_waits_gets_a_session_that_a_dial_makes_later() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        testing::transport(&server, SERVER, |transport, _| async move {
            let session = transport.accept().await.expect("a session");
            let closed = Error::PeerClosed { code: Code(7) };
            assert_eq!(session.closed().await, closed);
        });
        testing::shard(&client, CLIENT, move |config, node| async move {
            let tasks = config.tasks.clone();
            let part = testing::part(&node.net(), testing::address(&node));
            let transport = Rc::new(Transport::new(config, part).expect("a transport"));
            let accepting = Rc::clone(&transport);
            tasks.spawn(async move {
                let accepted = accepting.accept().await.expect("the dialed session");
                accepted.close(Code(7));
            });
            node.clock().sleep(Span::MILLISECOND).await;
            let dialed = transport.dial(SERVER.public(), &at).await;
            let closed = Error::Closed { code: Code(7) };
            assert_eq!(dialed.expect("a session").closed().await, closed);
            linger(&node).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // No address is given, so only the open session can answer.
    #[test]
    fn a_dial_after_accept_gave_a_session_from_the_peer_gives_that_session() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        testing::transport(&server, SERVER, |transport, _| async move {
            let accepted = transport.accept().await.expect("a session");
            let dialed = transport.dial(CLIENT.public(), &[]).await;
            dialed.expect("the accepted session").close(Code(4));
            assert_eq!(accepted.closed().await, Error::Closed { code: Code(4) });
        });
        testing::transport(&client, CLIENT, move |transport, _| async move {
            let dialed = transport.dial(SERVER.public(), &at).await;
            let closed = Error::PeerClosed { code: Code(4) };
            assert_eq!(dialed.expect("a session").closed().await, closed);
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_after_its_session_closed_dials_again() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        testing::transport(&server, SERVER, |transport, node| async move {
            let first = transport.accept().await.expect("a session");
            let closed = Error::PeerClosed { code: Code(1) };
            assert_eq!(first.closed().await, closed);
            let second = transport.accept().await.expect("a second session");
            second.close(Code(2));
            linger(&node).await;
        });
        testing::transport(&client, CLIENT, move |transport, _| async move {
            let first = transport.dial(SERVER.public(), &at).await;
            first.expect("a session").close(Code(1));
            let second = transport.dial(SERVER.public(), &at).await;
            let closed = Error::PeerClosed { code: Code(2) };
            assert_eq!(second.expect("a session").closed().await, closed);
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_after_the_peer_closed_the_session_dials_again() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        testing::transport(&server, SERVER, |transport, node| async move {
            transport.accept().await.expect("a session").close(Code(1));
            let second = transport.accept().await.expect("a second session");
            second.close(Code(2));
            linger(&node).await;
        });
        testing::transport(&client, CLIENT, move |transport, _| async move {
            let first = transport.dial(SERVER.public(), &at).await;
            let closed = Error::PeerClosed { code: Code(1) };
            assert_eq!(first.expect("a session").closed().await, closed);
            let second = transport.dial(SERVER.public(), &at).await;
            let closed = Error::PeerClosed { code: Code(2) };
            assert_eq!(second.expect("a session").closed().await, closed);
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_after_this_node_refused_the_peer_dials_again() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let back = [Address::Udp(testing::address(&client))];
        testing::transport(&server, SERVER, move |transport, _| async move {
            transport.accept().await.expect("a session").close(Code(1));
            let dialed = transport.dial(CLIENT.public(), &back).await;
            let closed = Error::PeerClosed { code: Code(2) };
            assert_eq!(dialed.expect("a new session").closed().await, closed);
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            let first = transport.dial(SERVER.public(), &at).await;
            let closed = Error::PeerClosed { code: Code(1) };
            assert_eq!(first.expect("a session").closed().await, closed);
            drop(transport.accept().await.expect("the dialed session"));
            let second = transport.accept().await.expect("the peer's session");
            second.close(Code(2));
            linger(&node).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // A client has no node key, so no dial can name its sessions.
    #[test]
    fn two_sessions_from_one_client_both_stay_open() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        testing::transport(&server, SERVER, |transport, node| async move {
            let first = transport.accept().await.expect("a session");
            let second = transport.accept().await.expect("a second session");
            assert_eq!([first.peer(), second.peer()], [Peer::Client; 2]);
            first.close(Code(1));
            second.close(Code(2));
            linger(&node).await;
        });
        testing::start(&client, move |shard, _| async move {
            let client = Client::new(shard.client(), shard.part()).expect("a client");
            let first = client.dial(SERVER.public(), &at).await.expect("a session");
            let second = client.dial(SERVER.public(), &at).await.expect("a session");
            let closed = |code| Error::PeerClosed { code: Code(code) };
            assert_eq!(first.closed().await, closed(1));
            assert_eq!(second.closed().await, closed(2));
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The second caller gives no address, so only the first caller's dial can answer.
    #[test]
    fn a_dial_whose_first_caller_dropped_goes_on_for_the_second() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        testing::transport(&server, SERVER, |transport, _| async move {
            let session = transport.accept().await.expect("a session");
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            let first = pin!(transport.dial(SERVER.public(), &at));
            assert!(testing::poll_once(first).await.is_none());
            let second = transport.dial(SERVER.public(), &[]).await;
            second.expect("the first caller's session").close(Code(5));
            linger(&node).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_that_fails_gives_the_session_that_the_peer_opened_meanwhile() {
        let (mut sim, client, server) = testing::nodes(0);
        let back = [Address::Udp(testing::address(&client))];
        let dead = dead(&server);
        testing::transport(&server, SERVER, move |transport, _| async move {
            let dialed = transport.dial(CLIENT.public(), &back).await;
            let closed = Error::PeerClosed { code: Code(6) };
            assert_eq!(dialed.expect("a session").closed().await, closed);
        });
        testing::transport(&client, CLIENT, move |transport, _| async move {
            let (accepted, dialed) = testing::join(
                transport.accept(),
                transport.dial(SERVER.public(), &dead),
            )
            .await;
            dialed.expect("the peer's session").close(Code(6));
            let accepted = accepted.expect("the peer's session");
            assert_eq!(accepted.closed().await, Error::Closed { code: Code(6) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_that_fails_with_no_session_gives_each_caller_its_error() {
        let (mut sim, client, server) = testing::nodes(0);
        let dead = dead(&server);
        let failed = unreachable(SERVER.public(), &server);
        testing::transport(&client, CLIENT, move |transport, _| async move {
            let (first, second) = testing::join(
                transport.dial(SERVER.public(), &dead),
                transport.dial(SERVER.public(), &[]),
            )
            .await;
            let failed = Some(failed);
            assert_eq!([first.err(), second.err()], [failed.clone(), failed]);
            let third = transport.dial(SERVER.public(), &[]).await;
            let none = Error::Unreachable {
                peer: SERVER.public(),
                attempts: Vec::new(),
            };
            assert_eq!(third.err(), Some(none));
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_that_connects_after_its_transport_dropped_closes_its_session() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        testing::transport(&server, SERVER, |transport, _| async move {
            let session = transport.accept().await.expect("a session");
            let closed = Error::PeerClosed { code: Code(0) };
            assert_eq!(session.closed().await, closed);
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            {
                let dial = pin!(transport.dial(SERVER.public(), &at));
                assert!(testing::poll_once(dial).await.is_none());
            }
            drop(transport);
            node.clock().sleep(testing::IDLE).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The table holds a dial, one held session, and 19 ended ones. The 16th accept
    // finds it at its limit, and keeps only the dial and the held session.
    #[test]
    fn the_table_drops_the_entry_of_each_ended_session_at_its_limit() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let other = PrivateKey([9; 32]).public();
        let dead = dead(&client);
        let failed = unreachable(other, &client);
        testing::transport(&server, SERVER, move |transport, node| async move {
            let dial = pin!(transport.dial(other, &dead));
            assert!(testing::poll_once(dial).await.is_none());
            let held = transport.accept().await.expect("a session");
            for _ in 1..20 {
                drop(transport.accept().await.expect("a session"));
            }
            assert_eq!(transport.table.borrow().nodes.len(), 7);
            let dialed = transport.dial(other, &[]).await;
            assert_eq!(dialed.err(), Some(failed));
            held.close(Code(1));
            linger(&node).await;
        });
        testing::shard(&client, CLIENT, move |config, node| async move {
            let transports = testing::transports(&config, &node, 20);
            let mut sessions = Vec::new();
            for transport in &transports {
                let dialed = transport.dial(SERVER.public(), &at).await;
                sessions.push(dialed.expect("a session"));
            }
            let closed = Error::PeerClosed { code: Code(1) };
            assert_eq!(sessions[0].closed().await, closed);
        });
        assert_eq!(sim.run(), Ok(()));
    }
}
