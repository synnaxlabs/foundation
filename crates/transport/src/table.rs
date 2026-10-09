//! The table of a [`Transport`](crate::Transport): the open session to each node, the
//! dial that runs for it, and the sessions that wait for `accept`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::rc::{self, Rc};
use std::task::{Context, Poll, Waker, ready};

use env::tasks::Tasks;
use types::ed25519::PublicKey;
use types::hash::Map;

use crate::session::{self, Session};
use crate::{Address, Code, Error, Peer, dial, quic, wake};

/// The fewest entries at which a prune runs.
const FLOOR: usize = 16;

pub(crate) struct Table {
    /// This node's key, which decides which of two sessions to one node wins.
    key: PublicKey,
    /// Runs the dials and the pings.
    tasks: Tasks,
    /// This table, for its tasks. A strong handle would keep the sessions for
    /// `accept` open after the transport drops.
    this: rc::Weak<RefCell<Table>>,
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
    /// The session of a node with a higher key, which this node holds while its own
    /// dial runs or until its own session answers a ping. No caller gets it.
    held: Option<Session>,
    /// A task of [`Table::prove`] runs for the entry.
    proving: bool,
}

/// What [`Table::find`] gives.
enum Found {
    Open(Session),
    Dialing(Rc<Attempt>),
    /// No session and no dial: the caller starts this attempt.
    Start(Rc<Attempt>),
}

/// Gives the open session to `node`, else waits for the dial that runs for it, else
/// starts a dial at `addresses` as a task and waits for it.
///
/// # Errors
///
/// As [`Transport::dial`](crate::Transport::dial).
pub(crate) async fn dial(
    table: &RefCell<Table>,
    carrier: &quic::Carrier,
    node: PublicKey,
    addresses: &[Address],
) -> Result<Session, Error> {
    let found = {
        let mut table = table.borrow_mut();
        table.take(&carrier.dialer());
        table.find(node)
    };
    let attempt = match found {
        Found::Open(session) => return Ok(session),
        Found::Dialing(attempt) => attempt,
        Found::Start(attempt) => {
            let dialer = carrier.dialer();
            let table = table.borrow();
            let this = rc::Weak::clone(&table.this);
            let addresses = addresses.to_vec();
            let started = Rc::clone(&attempt);
            table.tasks.spawn(async move {
                let dial = dial::dial(&dialer, node, &addresses).await;
                let Some(table) = this.upgrade() else {
                    return;
                };
                let mut table = table.borrow_mut();
                if dial.is_err() {
                    table.take(&dialer);
                }
                table.dialed(node, &started, dial.map(Session::new));
            });
            attempt
        }
    };
    poll_fn(|cx| attempt.poll(cx)).await
}

/// Waits for the next session for `accept`: one that a dial made, else one that a
/// peer opened on `carrier` and that the table does not hold.
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
        loop {
            if let Some(session) = table.poll_ready(cx) {
                return Poll::Ready(Ok(session));
            }
            let session = ready!(carrier.poll_accept(cx)).map(Session::new)?;
            if let Some(session) = table.arrive(session) {
                return Poll::Ready(Ok(session));
            }
        }
    })
    .await
}

impl Table {
    /// An empty table for the node `key`, whose dials and pings run on `tasks`.
    pub(crate) fn new(key: PublicKey, tasks: Tasks) -> Rc<RefCell<Self>> {
        Rc::new_cyclic(|this| {
            RefCell::new(Self {
                key,
                tasks,
                this: rc::Weak::clone(this),
                nodes: Map::default(),
                limit: 0,
                ready: VecDeque::new(),
                accepting: Vec::new(),
            })
        })
    }

    /// The open session to `node`, or else the dial that runs for it, or else a new
    /// attempt that the caller must start and end with [`Table::dialed`].
    fn find(&mut self, node: PublicKey) -> Found {
        if let Some(session) = self.settle(node) {
            return Found::Open(session);
        }
        let entry = self.entry(node);
        if let Some(attempt) = &entry.dial {
            return Found::Dialing(Rc::clone(attempt));
        }
        let attempt = Rc::new(Attempt::default());
        entry.dial = Some(Rc::clone(&attempt));
        Found::Start(attempt)
    }

    /// Ends `attempt` to `node` with what its dial gave, unless a session from the
    /// peer ended it first: then the dial's session drops. A session waits for
    /// `accept`, and closes the held one. After an error, the attempt gives the held
    /// session.
    fn dialed(
        &mut self,
        node: PublicKey,
        attempt: &Rc<Attempt>,
        dialed: Result<Session, Error>,
    ) {
        if attempt.ended() {
            return;
        }
        let entry = self.nodes.get_mut(&node);
        let entry = entry.expect("invariant: a dial keeps its entry");
        let dial = entry.dial.take().expect("invariant: one dial ends it");
        assert!(
            Rc::ptr_eq(&dial, attempt),
            "invariant: one dial runs per node"
        );
        let result = match dialed {
            Ok(session) => {
                if let Some(held) = entry.held.take() {
                    held.close(Code(0));
                }
                entry.session = session.downgrade();
                self.push(session.clone());
                Ok(session)
            }
            Err(error) => self.settle(node).ok_or(error),
        };
        attempt.end(result);
    }

    /// Takes each session that a peer opened and that no `accept` took from the
    /// carrier of `dialer`, so that a dial that failed finds it.
    fn take(&mut self, dialer: &quic::Dialer) {
        while let Some(session) = dialer.accepted() {
            if let Some(session) = self.arrive(Session::new(session)) {
                self.push(session);
            }
        }
    }

    /// Takes `session`, which a peer dialed, and gives it back when it goes to
    /// `accept`. The lower key's dial wins: the lower node holds a higher peer's
    /// session while its own dial runs, or while it pings its own open session. Else
    /// the newer session wins. Each loser closes with `Code(0)`. A client's session
    /// has no node, and one that ended changes nothing.
    fn arrive(&mut self, session: Session) -> Option<Session> {
        let Peer::Node(node) = session.peer() else {
            return Some(session);
        };
        if !session.live() {
            return Some(session);
        }
        let lower = self.key < node;
        let entry = self.entry(node);
        let open = entry.session.open();
        let own = open.as_ref().is_some_and(Session::dialed);
        if lower && (own || entry.dial.is_some()) {
            let replaced = entry.held.replace(session.clone());
            if let Some(open) = open.filter(|_| !entry.proving) {
                entry.proving = true;
                self.prove(node, &open, session.downgrade());
            }
            if let Some(replaced) = replaced {
                replaced.close(Code(0));
            }
            return None;
        }
        entry.session = session.downgrade();
        let held = entry.held.take();
        if let Some(attempt) = entry.dial.take() {
            attempt.end(Ok(session.clone()));
        }
        for loser in [open, held].into_iter().flatten() {
            loser.close(Code(0));
        }
        Some(session)
    }

    /// Pings `open`, the session that this node dialed to `node`, on the one task
    /// of the entry. When the peer answers a ping sent after the held session `held`
    /// arrived, the held session closes. When `open` ends or drops first, the held
    /// session wins.
    fn prove(&self, node: PublicKey, open: &Session, mut held: session::Weak) {
        let this = rc::Weak::clone(&self.this);
        let mut ping = open.ping();
        self.tasks.spawn(async move {
            loop {
                let answered = ping.await.is_ok();
                let Some(table) = this.upgrade() else {
                    return;
                };
                let mut table = table.borrow_mut();
                // A prune drops the entry only once each of its sessions ended.
                let Some(entry) = table.nodes.get_mut(&node) else {
                    return;
                };
                let newer = entry.held.as_ref().filter(|session| !held.is(session));
                let open = entry.session.open().filter(|_| answered);
                if let (Some(session), Some(open)) = (newer, &open) {
                    held = session.downgrade();
                    ping = open.ping();
                    continue;
                }
                entry.proving = false;
                if open.is_some() {
                    if let Some(held) = entry.held.take() {
                        held.close(Code(0));
                    }
                } else {
                    table.settle(node);
                }
                return;
            }
        });
    }

    /// The open session to `node`. When it has none and no dial runs, the held
    /// session wins first, if it is open, and goes to `accept`.
    fn settle(&mut self, node: PublicKey) -> Option<Session> {
        let entry = self.entry(node);
        if let Some(session) = entry.session.open() {
            return Some(session);
        }
        if entry.dial.is_some() {
            return None;
        }
        let held = entry.held.take().filter(Session::live)?;
        entry.session = held.downgrade();
        self.push(held.clone());
        Some(held)
    }

    /// Gives `session` to the next `accept`.
    fn push(&mut self, session: Session) {
        self.ready.push_back(session);
        self.accepting.drain(..).for_each(Waker::wake);
    }

    /// The entry of `node`, new when it has none. First it drops each entry with no
    /// open session, no dial, and no held session, when the table is at its limit.
    fn entry(&mut self, node: PublicKey) -> &mut Entry {
        if self.nodes.len() >= self.limit {
            let live = |_: &PublicKey, entry: &mut Entry| {
                entry.dial.is_some()
                    || entry.held.is_some()
                    || entry.session.open().is_some()
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

    /// Whether the attempt has its result.
    fn ended(&self) -> bool {
        self.0.borrow().result.is_some()
    }

    fn end(&self, result: Result<Session, Error>) {
        let mut state = self.0.borrow_mut();
        state.result = Some(result);
        state.waiting.drain(..).for_each(Waker::wake);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::net::SocketAddr;
    use std::pin::pin;
    use std::rc::Rc;

    use sim::node::Node;
    use types::ed25519::{PrivateKey, PublicKey};
    use types::time::Span;

    use crate::testing::{self, CLIENT, SERVER};
    use crate::{Address, Client, Code, Config, Error, Peer, Session, Transport};

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

    // No `accept` runs on the server, so the client's session waits in its carrier
    // when the server dials. The server's dial gives it, and makes no second session.
    #[test]
    fn a_dial_of_the_lower_key_gives_the_peers_session_that_waits_for_accept() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let back = [Address::Udp(testing::address(&client))];
        testing::transport(&server, SERVER, move |transport, node| async move {
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 200))
                .await;
            let open = transport.dial(CLIENT.public(), &back).await;
            let open = open.expect("the client's session");
            assert!(!open.dialed());
            let accepted = transport.accept().await.expect("the client's session");
            assert!(accepted.downgrade().is(&open));
            assert!(testing::poll_once(pin!(transport.accept())).await.is_none());
            open.close(Code(2));
            linger(&node).await;
        });
        testing::transport(&client, CLIENT, move |transport, _| async move {
            let dialed = transport.dial(SERVER.public(), &at).await;
            let closed = Error::PeerClosed { code: Code(2) };
            assert_eq!(dialed.expect("a session").closed().await, closed);
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

    /// A transport for `config` at [`testing::address`] on `node`, and the sessions
    /// that a task on the shard accepts from it.
    fn accepting(config: Config, node: &Node) -> (Rc<Transport>, Sessions) {
        let tasks = config.tasks.clone();
        let part = testing::part(&node.net(), testing::address(node));
        let transport = Rc::new(Transport::new(config, part).expect("a transport"));
        let sessions = Sessions::default();
        let (accepting, accepted) = (Rc::clone(&transport), Rc::clone(&sessions));
        tasks.spawn(async move {
            while let Ok(session) = accepting.accept().await {
                accepted.borrow_mut().push(session);
            }
        });
        (transport, sessions)
    }

    type Sessions = Rc<RefCell<Vec<Session>>>;

    /// A node of [`two_nodes_that_dial_each_other_keep_the_session_of_the_lower_key`]
    /// with `key`. After a random delay under 700 us, it dials `peer` at `at`. A
    /// session reaches the peer three one-way delays of 250 us after its dial, so
    /// both dials start before either session arrives, and one may end first. It
    /// keeps each session that `dial` and `accept` give, and checks that each one
    /// but the open one ended with `Code(0)`.
    fn dial_at_once(node: &Node, key: PrivateKey, peer: PublicKey, at: [Address; 1]) {
        let lower = key.public() < peer;
        testing::shard(node, key, move |config, node| async move {
            let (transport, sessions) = accepting(config, &node);
            let offset = node.entropy().rng().below(700_000);
            let offset = Span::from_nanos(i64::try_from(offset).expect("small"));
            node.clock().sleep(offset).await;
            let dialed = transport.dial(peer, &at).await.expect("a session");
            sessions.borrow_mut().push(dialed);
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 100))
                .await;
            let open = transport.dial(peer, &[]).await.expect("the open session");
            assert_eq!(open.dialed(), lower);
            let held: Vec<_> = sessions.borrow_mut().drain(..).collect();
            for session in held.iter().filter(|s| !s.downgrade().is(&open)) {
                let error = session.closed().await;
                assert!(
                    matches!(
                        error,
                        Error::Closed { code: Code(0) }
                            | Error::PeerClosed { code: Code(0) }
                    ),
                    "{error:?}"
                );
            }
            // The higher node must find the open session first.
            if lower {
                node.clock()
                    .sleep(testing::spans(Span::MILLISECOND, 10))
                    .await;
                open.close(Code(9));
                linger(&node).await;
            } else {
                assert_eq!(open.closed().await, Error::PeerClosed { code: Code(9) });
            }
        });
    }

    // The lower node closes its open session with code 9, and the higher node's open
    // session gets it, so both keep the same one.
    #[test]
    fn two_nodes_that_dial_each_other_keep_the_session_of_the_lower_key() {
        for value in 0..32 {
            for (a, b) in [(CLIENT, SERVER), (SERVER, CLIENT)] {
                let (mut sim, one, two) = testing::nodes(value);
                let (a_key, b_key) = (a.public(), b.public());
                let at = |node| [Address::Udp(testing::address(node))];
                dial_at_once(&one, a, b_key, at(&two));
                dial_at_once(&two, b, a_key, at(&one));
                assert_eq!(sim.run(), Ok(()), "run {value}");
            }
        }
    }

    // The server has the lower key, so it holds the client's session while its own
    // dial runs, and gives it when that dial fails.
    #[test]
    fn a_failed_dial_of_the_lower_key_gives_the_session_that_it_held() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let dead = dead(&client);
        testing::transport(&server, SERVER, move |transport, node| async move {
            let (accepted, dialed) = testing::join(
                transport.accept(),
                transport.dial(CLIENT.public(), &dead),
            )
            .await;
            let dialed = dialed.expect("the client's session");
            assert!(!dialed.dialed());
            assert!(accepted.expect("a session").downgrade().is(&dialed));
            assert!(testing::poll_once(pin!(transport.accept())).await.is_none());
            dialed.close(Code(6));
            linger(&node).await;
        });
        testing::transport(&client, CLIENT, move |transport, _| async move {
            let dialed = transport.dial(SERVER.public(), &at).await;
            let closed = Error::PeerClosed { code: Code(6) };
            assert_eq!(dialed.expect("a session").closed().await, closed);
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The client closes its session after the server holds it, so the server's failed
    // dial has no session to give.
    #[test]
    fn a_failed_dial_of_the_lower_key_gives_its_error_when_the_held_session_ended() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let dead = dead(&client);
        let failed = unreachable(CLIENT.public(), &client);
        testing::shard(&server, SERVER, move |config, node| async move {
            let (transport, sessions) = accepting(config, &node);
            let dialed = transport.dial(CLIENT.public(), &dead).await;
            assert_eq!(dialed.err(), Some(failed));
            assert!(sessions.borrow().is_empty());
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            let dialed = transport.dial(SERVER.public(), &at).await;
            let dialed = dialed.expect("a session");
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 10))
                .await;
            dialed.close(Code(1));
            linger(&node).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    /// `N` transports for [`CLIENT`] on the client node, at [`testing::PORT`] and the
    /// ports after it, so that each makes its own session to one peer.
    fn copies<const N: usize>(shard: &testing::Shard, node: &Node) -> [Transport; N] {
        std::array::from_fn(|index| {
            let index = u16::try_from(index).expect("few copies");
            let at = SocketAddr::new(shard.ip(), testing::PORT + index);
            let part = testing::part(&node.net(), at);
            let config = shard.config(CLIENT, testing::IDLE);
            Transport::new(config, part).expect("a transport")
        })
    }

    // The server dials a dead address. The second session from the client replaces
    // the first one that it held, and its dial gives the second when it fails.
    #[test]
    fn a_newer_session_of_the_higher_key_replaces_the_held_one() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let dead = dead(&client);
        testing::transport(&server, SERVER, move |transport, node| async move {
            let dialed = transport.dial(CLIENT.public(), &dead).await;
            dialed.expect("the second session").close(Code(6));
            linger(&node).await;
        });
        testing::start(&client, move |shard, node| async move {
            let [first, second] = copies(&shard, &node);
            let first = first.dial(SERVER.public(), &at).await.expect("a session");
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 50))
                .await;
            let second = second.dial(SERVER.public(), &at).await.expect("a session");
            assert_eq!(first.closed().await, Error::PeerClosed { code: Code(0) });
            assert_eq!(second.closed().await, Error::PeerClosed { code: Code(6) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The server's session is open. A second session from the client arrives, and the
    // server closes it once the client acknowledges a ping on the open one.
    #[test]
    fn the_lower_key_closes_the_held_session_once_its_open_one_answers_a_ping() {
        let (mut sim, client, server) = testing::nodes(0);
        let back = [Address::Udp(testing::address(&client))];
        let at = [Address::Udp(testing::address(&server))];
        testing::shard(&server, SERVER, move |config, node| async move {
            let (transport, _) = accepting(config, &node);
            let open = transport.dial(CLIENT.public(), &back).await;
            let open = open.expect("a session");
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 100))
                .await;
            open.close(Code(5));
            linger(&node).await;
        });
        testing::start(&client, move |shard, node| async move {
            let [first, second] = copies(&shard, &node);
            let accepted = first.accept().await.expect("the server's session");
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 10))
                .await;
            let start = node.clock().now();
            let held = second.dial(SERVER.public(), &at).await.expect("a session");
            assert_eq!(held.closed().await, Error::PeerClosed { code: Code(0) });
            let closed = node.clock().now() - start;
            // The client acknowledges a lone ping after its ack delay of 25 ms.
            let bound = testing::spans(Span::MILLISECOND, 30);
            assert!(closed < bound, "closed after {closed:?}");
            assert_eq!(accepted.closed().await, Error::PeerClosed { code: Code(5) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // A third session from the client replaces the held one while the server pings
    // its open session. The client acknowledges that ping 6 ms after the third dial
    // starts. The server closes the third session only once the client acknowledges
    // a ping sent after it arrived, one round trip of 500 us later.
    #[test]
    fn a_newer_held_session_waits_for_a_ping_sent_after_it() {
        let (mut sim, client, server) = testing::nodes(0);
        let back = [Address::Udp(testing::address(&client))];
        let at = [Address::Udp(testing::address(&server))];
        testing::shard(&server, SERVER, move |config, node| async move {
            let (transport, _) = accepting(config, &node);
            let open = transport.dial(CLIENT.public(), &back).await;
            let open = open.expect("a session");
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 100))
                .await;
            open.close(Code(5));
            linger(&node).await;
        });
        testing::start(&client, move |shard, node| async move {
            let [first, second, third] = copies(&shard, &node);
            let accepted = first.accept().await.expect("the server's session");
            let held = second.dial(SERVER.public(), &at).await.expect("a session");
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 20))
                .await;
            let start = node.clock().now();
            let newer = third.dial(SERVER.public(), &at).await.expect("a session");
            assert_eq!(held.closed().await, Error::PeerClosed { code: Code(0) });
            assert_eq!(newer.closed().await, Error::PeerClosed { code: Code(0) });
            let closed = node.clock().now() - start;
            let bound = testing::spans(Span::MICROSECOND, 6_250);
            assert!(closed > bound, "closed after {closed:?}");
            assert_eq!(accepted.closed().await, Error::PeerClosed { code: Code(5) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The server drops its open session while it pings it, before the client can
    // acknowledge the ping. The ping does not keep the session open, so the session
    // that the server held wins.
    #[test]
    fn the_held_session_wins_when_the_open_one_drops_during_the_ping() {
        let (mut sim, client, server) = testing::nodes(0);
        let back = [Address::Udp(testing::address(&client))];
        let at = [Address::Udp(testing::address(&server))];
        testing::shard(&server, SERVER, move |config, node| async move {
            let (transport, sessions) = accepting(config, &node);
            let open = transport.dial(CLIENT.public(), &back).await;
            let open = open.expect("a session");
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 15))
                .await;
            sessions.borrow_mut().clear();
            drop(open);
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 5))
                .await;
            let won = sessions.borrow_mut().pop().expect("the held session");
            assert!(!won.dialed());
            won.close(Code(7));
            linger(&node).await;
        });
        testing::start(&client, move |shard, node| async move {
            let [first, second] = copies(&shard, &node);
            let accepted = first.accept().await.expect("the server's session");
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 10))
                .await;
            let held = second.dial(SERVER.public(), &at).await.expect("a session");
            assert_eq!(accepted.closed().await, Error::PeerClosed { code: Code(0) });
            assert_eq!(held.closed().await, Error::PeerClosed { code: Code(7) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    /// The `survivor` node dials the other, which then restarts with `restarted` at
    /// the same address and dials back. The new session stays open on both nodes,
    /// and the survivor's first session ends with `old`.
    fn restart(survivor: PrivateKey, restarted: PrivateKey, old: Error) {
        let (mut sim, one, two) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&two))];
        let back = [Address::Udp(testing::address(&one))];
        let (peer, key) = (restarted.public(), survivor.public());
        testing::transport(&one, survivor, move |transport, node| async move {
            let first = transport.dial(peer, &at).await.expect("a session");
            drop(transport.accept().await.expect("the dialed session"));
            let new = transport
                .accept()
                .await
                .expect("the restarted node's session");
            assert_eq!(first.closed().await, old);
            assert!(!first.downgrade().is(&new));
            let open = transport.dial(peer, &[]).await.expect("the new session");
            assert!(open.downgrade().is(&new));
            new.close(Code(3));
            linger(&node).await;
        });
        // The first shard ends with no linger, so no close goes out.
        testing::transport(
            &two,
            restarted.clone(),
            move |transport, node| async move {
                let first = transport.accept().await.expect("the survivor's session");
                testing::shard(&node, restarted, move |config, node| async move {
                    node.clock()
                        .sleep(testing::spans(Span::MILLISECOND, 10))
                        .await;
                    let part = testing::part(&node.net(), testing::address(&node));
                    let transport = Transport::new(config, part).expect("a transport");
                    let new = transport.dial(key, &back).await.expect("a session");
                    assert_eq!(new.closed().await, Error::PeerClosed { code: Code(3) });
                });
                drop(first);
            },
        );
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_restarted_peer_of_the_lower_key_replaces_the_session_of_the_higher() {
        restart(CLIENT, SERVER, Error::Closed { code: Code(0) });
    }

    // The survivor holds the new session and pings the first one, which the restarted
    // node resets.
    #[test]
    fn a_restarted_peer_of_the_higher_key_wins_once_the_first_session_resets() {
        let reason = "reset by peer".to_owned();
        restart(SERVER, CLIENT, Error::Broken { reason });
    }

    // The server dials the client and closes that session at once. The client then
    // dials the server while its accept takes the ended session from the carrier.
    #[test]
    fn an_ended_session_from_the_lower_key_does_not_end_a_dial() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let back = [Address::Udp(testing::address(&client))];
        testing::transport(&server, SERVER, move |transport, node| async move {
            let first = transport.dial(CLIENT.public(), &back).await;
            first.expect("a session").close(Code(7));
            drop(transport.accept().await.expect("the dialed session"));
            let theirs = transport.accept().await.expect("the client's session");
            theirs.close(Code(6));
            linger(&node).await;
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 10))
                .await;
            let (dialed, _accepted) =
                testing::join(transport.dial(SERVER.public(), &at), transport.accept())
                    .await;
            let dialed = dialed.expect("a session");
            assert_eq!(dialed.closed().await, Error::PeerClosed { code: Code(6) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // As above, but the client's dial completes first, and both nodes keep its
    // session open. Then the client's accept takes the server's ended session.
    #[test]
    fn an_ended_session_from_the_lower_key_does_not_close_the_open_one() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let back = [Address::Udp(testing::address(&client))];
        testing::transport(&server, SERVER, move |transport, node| async move {
            let first = transport.dial(CLIENT.public(), &back).await;
            first.expect("a session").close(Code(7));
            drop(transport.accept().await.expect("the dialed session"));
            let theirs = transport.accept().await.expect("the client's session");
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 50))
                .await;
            let open = transport.dial(CLIENT.public(), &[]).await;
            assert!(open.expect("the open session").downgrade().is(&theirs));
            theirs.close(Code(6));
            linger(&node).await;
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 10))
                .await;
            let dialed = transport.dial(SERVER.public(), &at).await;
            let dialed = dialed.expect("a session");
            drop(transport.accept().await.expect("the dialed session"));
            drop(
                transport
                    .accept()
                    .await
                    .expect("the server's ended session"),
            );
            assert_eq!(dialed.closed().await, Error::PeerClosed { code: Code(6) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The client dials both sessions, so the newer wins on the server, which closes
    // the first with `Code(0)`.
    #[test]
    fn a_newer_session_that_the_higher_key_dialed_replaces_the_open_one() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        testing::shard(&server, SERVER, move |config, node| async move {
            let (transport, sessions) = accepting(config, &node);
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 100))
                .await;
            let open = transport.dial(CLIENT.public(), &[]).await;
            let open = open.expect("the newer session");
            assert!(!open.dialed());
            let last = sessions.borrow().last().expect("a session").downgrade();
            assert!(last.is(&open));
            open.close(Code(6));
            linger(&node).await;
        });
        testing::start(&client, move |shard, node| async move {
            let [first, second] = copies(&shard, &node);
            let first = first.dial(SERVER.public(), &at).await.expect("a session");
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 10))
                .await;
            let second = second.dial(SERVER.public(), &at).await.expect("a session");
            assert_eq!(first.closed().await, Error::PeerClosed { code: Code(0) });
            assert_eq!(second.closed().await, Error::PeerClosed { code: Code(6) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The client dials a dead address first, so its first dial connects 250 ms
    // later. The server's session ends that attempt, and then closes. A second dial
    // starts, also slow, and the first dial's session comes while it runs: the
    // second dial must still end.
    #[test]
    fn a_late_session_of_an_ended_attempt_does_not_end_the_next_one() {
        let (mut sim, client, server) = testing::nodes(0);
        let slow = [dead(&server)[0], Address::Udp(testing::address(&server))];
        let back = [Address::Udp(testing::address(&client))];
        testing::shard(&server, SERVER, move |config, node| async move {
            let (transport, _sessions) = accepting(config, &node);
            let mine = transport.dial(CLIENT.public(), &back).await;
            let mine = mine.expect("a session");
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 50))
                .await;
            mine.close(Code(1));
            node.clock().sleep(Span::SECOND).await;
        });
        testing::shard(&client, CLIENT, move |config, node| async move {
            let (transport, _sessions) = accepting(config, &node);
            let first = transport.dial(SERVER.public(), &slow).await;
            let first = first.expect("the server's session");
            assert!(!first.dialed());
            assert_eq!(first.closed().await, Error::PeerClosed { code: Code(1) });
            let again = transport.dial(SERVER.public(), &slow).await;
            let again = again.expect("a new session");
            assert!(again.dialed());
            again.close(Code(2));
            linger(&node).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The server dials a dead address first, so its dial runs for 250 ms, and it
    // holds the client's session meanwhile. A second dial waits for its own dial, and
    // does not get the held session.
    #[test]
    fn a_dial_while_the_lower_key_dials_waits_for_its_own_dial() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let slow = [dead(&client)[0], Address::Udp(testing::address(&client))];
        testing::shard(&server, SERVER, move |config, node| async move {
            let (transport, _sessions) = accepting(config, &node);
            let first = pin!(transport.dial(CLIENT.public(), &slow));
            assert!(testing::poll_once(first).await.is_none());
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 50))
                .await;
            let second = transport.dial(CLIENT.public(), &[]).await;
            let second = second.expect("its own session");
            assert!(second.dialed());
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 400))
                .await;
            second.close(Code(4));
            linger(&node).await;
        });
        testing::shard(&client, CLIENT, move |config, node| async move {
            let (transport, _sessions) = accepting(config, &node);
            let first = transport.dial(SERVER.public(), &at).await;
            drop(first.expect("a session"));
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 400))
                .await;
            let open = transport.dial(SERVER.public(), &[]).await;
            let open = open.expect("the server's session");
            assert!(!open.dialed());
            assert_eq!(open.closed().await, Error::PeerClosed { code: Code(4) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The client runs no `accept`, so only the server can close the client's
    // session. The server holds it while its slow dial runs, and closes it with
    // `Code(0)` when that dial completes.
    #[test]
    fn the_lower_key_closes_the_held_session_when_its_dial_completes() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let slow = [dead(&client)[0], Address::Udp(testing::address(&client))];
        testing::shard(&server, SERVER, move |config, node| async move {
            let (transport, _sessions) = accepting(config, &node);
            let mine = transport.dial(CLIENT.public(), &slow).await;
            let mine = mine.expect("its own session");
            assert!(mine.dialed());
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 100))
                .await;
            mine.close(Code(5));
            linger(&node).await;
        });
        testing::transport(&client, CLIENT, move |transport, _| async move {
            let dialed = transport.dial(SERVER.public(), &at).await;
            let dialed = dialed.expect("a session");
            assert_eq!(dialed.closed().await, Error::PeerClosed { code: Code(0) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The server's session has been open for 1 s, and waits in the client's carrier
    // because no `accept` ran yet. The client's dial gives it, so it stays open.
    #[test]
    fn a_dial_of_the_higher_key_gives_a_session_that_stays_open() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let back = [Address::Udp(testing::address(&client))];
        testing::shard(&server, SERVER, move |config, node| async move {
            let (transport, _sessions) = accepting(config, &node);
            let dialed = transport.dial(CLIENT.public(), &back).await;
            let dialed = dialed.expect("a session");
            node.clock().sleep(testing::spans(Span::SECOND, 3)).await;
            drop(dialed);
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            node.clock().sleep(Span::SECOND).await;
            let dialed = transport.dial(SERVER.public(), &at).await;
            let dialed = dialed.expect("a session");
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 100))
                .await;
            assert!(dialed.live(), "{:?}", dialed.closed().await);
            linger(&node).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }
}
