//! The table of a [`Transport`](crate::Transport): the open session to each node, the
//! dial that runs for it, and the sessions that wait for `accept`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::rc::Rc;
use std::task::{Context, Poll, Waker, ready};

use env::tasks::Tasks;
use types::ed25519::PublicKey;
use types::hash::Map;

use crate::session::{self, Session};
use crate::{Address, Error, Peer, dial, quic, wake};

/// The fewest entries at which a prune runs.
const FLOOR: usize = 16;

#[derive(Default)]
pub(crate) struct Table {
    nodes: Map<PublicKey, Entry>,
    /// The entry count at which the next prune runs: twice the count after the last
    /// one, and at least [`FLOOR`]. So peers that come with new keys cannot grow the
    /// table past twice its live entries.
    limit: usize,
    /// The sessions for `accept`, in the order the table got them: each that a dial
    /// made, and each that a peer opened and the table took from the carrier.
    ready: VecDeque<Session>,
    /// The wakers of the `accept` calls that wait.
    accepting: Vec<Waker>,
}

/// What a [`Table`] knows of one node.
#[derive(Default)]
struct Entry {
    session: session::Weak,
    dial: Option<Rc<Attempt>>,
}

/// What [`Table::find`] gives.
enum Found {
    Open(Session),
    Dialing(Rc<Attempt>),
    /// No session and no dial: the caller starts this attempt.
    Start(Rc<Attempt>),
}

/// Gives the open session to `node`, else waits for the dial that runs for it, else
/// starts a dial at `addresses` as a task on `tasks` and waits for it.
///
/// # Errors
///
/// As [`Transport::dial`](crate::Transport::dial).
pub(crate) async fn dial(
    table: &Rc<RefCell<Table>>,
    carrier: &quic::Carrier,
    tasks: &Tasks,
    node: PublicKey,
    addresses: &[Address],
) -> Result<Session, Error> {
    let found = table.borrow_mut().find(node);
    let attempt = match found {
        Found::Open(session) => return Ok(session),
        Found::Dialing(attempt) => attempt,
        Found::Start(attempt) => {
            let dialer = carrier.dialer();
            // A strong handle would keep the sessions for `accept` open after the
            // transport drops.
            let table = Rc::downgrade(table);
            let addresses = addresses.to_vec();
            tasks.spawn(async move {
                let dial = dial::dial(&dialer, node, &addresses).await;
                let Some(table) = table.upgrade() else {
                    return;
                };
                let mut table = table.borrow_mut();
                if dial.is_err() {
                    table.take(&dialer);
                }
                table.dialed(node, dial.map(Session::new));
            });
            attempt
        }
    };
    poll_fn(|cx| attempt.poll(cx)).await
}

/// Waits for the next session for `accept`: one that a dial made, else one that a
/// peer opened on `carrier`.
///
/// # Errors
///
/// As [`Transport::accept`](crate::Transport::accept).
pub(crate) async fn accept(
    table: &RefCell<Table>,
    carrier: &quic::Carrier,
) -> Result<Session, Error> {
    poll_fn(|cx| {
        let mut table = table.borrow_mut();
        if let Some(session) = table.poll_ready(cx) {
            return Poll::Ready(Ok(session));
        }
        let session = ready!(carrier.poll_accept(cx)).map(Session::new)?;
        table.hold(&session);
        Poll::Ready(Ok(session))
    })
    .await
}

impl Table {
    /// The open session to `node`, or else the dial that runs for it, or else a new
    /// attempt that the caller must start and end with [`Table::dialed`].
    fn find(&mut self, node: PublicKey) -> Found {
        let entry = self.entry(node);
        if let Some(session) = entry.session.open() {
            return Found::Open(session);
        }
        if let Some(attempt) = &entry.dial {
            return Found::Dialing(Rc::clone(attempt));
        }
        let attempt = Rc::new(Attempt::default());
        entry.dial = Some(Rc::clone(&attempt));
        Found::Start(attempt)
    }

    /// Ends the attempt to `node` with what its dial gave. A session waits for
    /// `accept`. After an error, the attempt gives the open session that the peer
    /// opened meanwhile, if one is open.
    fn dialed(&mut self, node: PublicKey, dialed: Result<Session, Error>) {
        let entry = self.nodes.get_mut(&node);
        let entry = entry.expect("invariant: a dial keeps its entry");
        let attempt = entry.dial.take().expect("invariant: one dial ends it");
        let result = match dialed {
            Ok(session) => {
                self.hold(&session);
                self.push(session.clone());
                Ok(session)
            }
            Err(error) => entry.session.open().ok_or(error),
        };
        attempt.end(result);
    }

    /// Takes each session that a peer opened and that no `accept` took from the
    /// carrier of `dialer`, so that a dial that failed finds it.
    fn take(&mut self, dialer: &quic::Dialer) {
        while let Some(session) = dialer.accepted() {
            let session = Session::new(session);
            self.hold(&session);
            self.push(session);
        }
    }

    /// Makes `session` the open one to its node, unless the node has an open one. A
    /// client's session has no node.
    fn hold(&mut self, session: &Session) {
        if let Peer::Node(node) = session.peer() {
            let entry = self.entry(node);
            if entry.session.open().is_none() {
                entry.session = session.downgrade();
            }
        }
    }

    /// Gives `session` to the next `accept`.
    fn push(&mut self, session: Session) {
        self.ready.push_back(session);
        self.accepting.drain(..).for_each(Waker::wake);
    }

    /// The entry of `node`, new when it has none. First it drops each entry with no
    /// open session and no dial, when the table is at its limit.
    fn entry(&mut self, node: PublicKey) -> &mut Entry {
        if self.nodes.len() >= self.limit {
            let live = |_: &PublicKey, entry: &mut Entry| {
                entry.dial.is_some() || entry.session.open().is_some()
            };
            self.nodes.retain(live);
            self.limit = self.nodes.len().saturating_mul(2).max(FLOOR);
        }
        self.nodes.entry(node).or_default()
    }

    /// The next session for `accept`, or `None`, and then `cx` wakes when one comes.
    fn poll_ready(&mut self, cx: &Context<'_>) -> Option<Session> {
        let session = self.ready.pop_front();
        if session.is_none() {
            wake::register(&mut self.accepting, cx.waker());
        }
        session
    }
}

/// One dial to a node, which each caller that dials the node meanwhile waits on.
#[derive(Default)]
struct Attempt(RefCell<State>);

#[derive(Default)]
struct State {
    result: Option<Result<Session, Error>>,
    waiting: Vec<Waker>,
}

impl Attempt {
    /// Ready with what the dial gave.
    fn poll(&self, cx: &Context<'_>) -> Poll<Result<Session, Error>> {
        let mut state = self.0.borrow_mut();
        if let Some(result) = &state.result {
            return Poll::Ready(result.clone());
        }
        wake::register(&mut state.waiting, cx.waker());
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
    fn a_dial_in_flight_when_its_transport_drops_never_connects() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        testing::transport(&server, SERVER, |transport, node| async move {
            let mut accept = pin!(transport.accept());
            node.clock().sleep(testing::IDLE).await;
            assert!(testing::poll_once(accept.as_mut()).await.is_none());
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            {
                let dial = pin!(transport.dial(SERVER.public(), &at));
                assert!(testing::poll_once(dial).await.is_none());
            }
            // The dial task starts its attempt. Its first packet is still on the link.
            node.clock()
                .sleep(testing::spans(Span::MICROSECOND, 100))
                .await;
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
            // Only the count shows a prune that keeps the ended entries here. The
            // memory test bounds the heap.
            assert_eq!(transport.table.borrow().nodes.len(), 7);
            let Peer::Node(peer) = held.peer() else {
                panic!("a node dialed the held session");
            };
            let again = transport.dial(peer, &[]).await;
            again.expect("the held session").close(Code(1));
            let dialed = transport.dial(other, &[]).await;
            assert_eq!(dialed.err(), Some(failed));
            assert_eq!(held.closed().await, Error::Closed { code: Code(1) });
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

    // Each node dials the other. The session from the server ends first, and the
    // client's dialed session stays the open one.
    #[test]
    fn a_dial_gives_the_open_session_after_a_newer_session_from_the_peer_ended() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let back = [Address::Udp(testing::address(&client))];
        testing::transport(&server, SERVER, move |transport, node| async move {
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 200))
                .await;
            let mine = transport.dial(CLIENT.public(), &back).await;
            mine.expect("a session").close(Code(1));
            drop(transport.accept().await.expect("the dialed session"));
            let theirs = transport.accept().await.expect("the client's session");
            let closed = Error::PeerClosed { code: Code(2) };
            assert_eq!(theirs.closed().await, closed);
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            let dialed = transport.dial(SERVER.public(), &at).await;
            let dialed = dialed.expect("a session");
            drop(transport.accept().await.expect("the dialed session"));
            let theirs = transport.accept().await.expect("the server's session");
            let closed = Error::PeerClosed { code: Code(1) };
            assert_eq!(theirs.closed().await, closed);
            let again = transport.dial(SERVER.public(), &[]).await;
            again.expect("the open dialed session").close(Code(2));
            assert_eq!(dialed.closed().await, Error::Closed { code: Code(2) });
            linger(&node).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // No `accept` runs on the client, so the server's session waits in the carrier.
    #[test]
    fn a_dial_that_fails_gives_the_peers_session_that_no_accept_took() {
        let (mut sim, client, server) = testing::nodes(0);
        let back = [Address::Udp(testing::address(&client))];
        let dead = dead(&server);
        testing::transport(&server, SERVER, move |transport, _| async move {
            let dialed = transport.dial(CLIENT.public(), &back).await;
            let closed = Error::PeerClosed { code: Code(6) };
            assert_eq!(dialed.expect("a session").closed().await, closed);
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            let dialed = transport.dial(SERVER.public(), &dead).await;
            let dialed = dialed.expect("the peer's session");
            let accepted = transport.accept().await.expect("the peer's session");
            assert_eq!(accepted.peer(), Peer::Node(SERVER.public()));
            dialed.close(Code(6));
            assert_eq!(accepted.closed().await, Error::Closed { code: Code(6) });
            linger(&node).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // Two server transports dial the client in turn. The client's dial to the first
    // fails, and `accept` then gives both sessions in the order they came.
    #[test]
    fn accept_gives_the_sessions_that_a_failed_dial_took_in_order() {
        let (mut sim, client, server) = testing::nodes(0);
        let back = [Address::Udp(testing::address(&client))];
        let dead = dead(&server);
        testing::shard(&server, SERVER, move |config, node| async move {
            let transports = testing::transports(&config, &node, 2);
            let mut sessions = Vec::new();
            for transport in &transports {
                let dialed = transport.dial(CLIENT.public(), &back).await;
                sessions.push(dialed.expect("a session"));
            }
            for session in sessions {
                let closed = Error::PeerClosed { code: Code(6) };
                assert_eq!(session.closed().await, closed);
            }
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            let first = PrivateKey([10; 32]).public();
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 200))
                .await;
            let dialed = transport.dial(first, &dead).await;
            drop(dialed.expect("the first session"));
            for key in [10, 11] {
                let accepted = transport.accept().await.expect("a session");
                assert_eq!(accepted.peer(), Peer::Node(PrivateKey([key; 32]).public()));
                accepted.close(Code(6));
            }
            linger(&node).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The server closes the client's session, and once it drained, dials the client.
    // The new connection takes the handle of the old one, and a dial gives the new
    // session.
    #[test]
    fn a_dial_gives_the_new_session_on_the_handle_of_an_ended_one() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let back = [Address::Udp(testing::address(&client))];
        testing::transport(&server, SERVER, move |transport, node| async move {
            transport.accept().await.expect("a session").close(Code(1));
            node.clock().sleep(testing::spans(testing::IDLE, 5)).await;
            let dialed = transport.dial(CLIENT.public(), &back).await;
            let closed = Error::PeerClosed { code: Code(2) };
            assert_eq!(dialed.expect("a session").closed().await, closed);
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            let old = transport
                .dial(SERVER.public(), &at)
                .await
                .expect("a session");
            assert_eq!(old.closed().await, Error::PeerClosed { code: Code(1) });
            let own = transport.accept().await.expect("the dialed session");
            node.clock().sleep(testing::spans(testing::IDLE, 10)).await;
            let theirs = transport.accept().await.expect("the server's session");
            let dialed = transport.dial(SERVER.public(), &[]).await;
            dialed.expect("the server's session").close(Code(2));
            assert_eq!(theirs.closed().await, Error::Closed { code: Code(2) });
            drop(own);
            linger(&node).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The client drops its transport while its dial waits on a silent address. The
    // dial starts no attempt at the next address, so the server accepts nothing.
    #[test]
    fn a_dial_starts_no_attempt_after_its_transport_dropped() {
        let (mut sim, client, server) = testing::nodes(0);
        let [silent] = dead(&server);
        let addresses = [silent, Address::Udp(testing::address(&server))];
        testing::transport(&server, SERVER, |transport, node| async move {
            node.clock().sleep(testing::spans(testing::IDLE, 3)).await;
            let accept = pin!(transport.accept());
            assert!(testing::poll_once(accept).await.is_none());
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            {
                let dial = pin!(transport.dial(SERVER.public(), &addresses));
                assert!(testing::poll_once(dial).await.is_none());
            }
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 10))
                .await;
            drop(transport);
            node.clock().sleep(testing::spans(testing::IDLE, 4)).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    /// The client drops its transport at 200 ms while a dial to a dead address runs.
    /// When `taken`, a failed dial first takes the server's session from the carrier.
    fn drop_while_a_dial_runs(taken: bool) {
        let (mut sim, client, server) = testing::nodes(0);
        let back = [Address::Udp(testing::address(&client))];
        let other = PrivateKey([9; 32]).public();
        let dead = dead(&server);
        testing::transport(&server, SERVER, move |transport, node| async move {
            let start = node.clock().now();
            let dialed = transport.dial(CLIENT.public(), &back).await;
            let closed = Error::PeerClosed { code: Code(0) };
            assert_eq!(dialed.expect("a session").closed().await, closed);
            let open = node.clock().now() - start;
            assert!(
                open < testing::spans(Span::MILLISECOND, 400),
                "open {open:?}"
            );
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            {
                let long = pin!(transport.dial(other, &dead));
                assert!(testing::poll_once(long).await.is_none());
            }
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 200))
                .await;
            if taken {
                let dialed = transport.dial(SERVER.public(), &[]).await;
                drop(dialed.expect("the peer's session"));
            }
            drop(transport);
            node.clock().sleep(testing::spans(testing::IDLE, 3)).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn dropping_the_transport_closes_the_peers_session_in_the_carrier() {
        drop_while_a_dial_runs(false);
    }

    #[test]
    fn dropping_the_transport_closes_the_peers_session_that_a_failed_dial_took() {
        drop_while_a_dial_runs(true);
    }
}
