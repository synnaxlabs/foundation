//! The table of a [`Transport`](crate::Transport): the open session to each node, the
//! dial that runs for it, and the sessions that wait for `accept`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::pin::pin;
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
    /// Dials and accepts on the transport's carrier.
    carrier: quic::Handle,
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
    /// This node dialed `session`.
    dialed: bool,
    dial: Option<Rc<Attempt>>,
    /// The session of a node with a higher key, which this node holds while its own
    /// dial runs or until its own session answers a ping. No caller gets it.
    held: Option<Session>,
    /// A task of [`Table::prove`] runs for the entry.
    proving: bool,
}

impl Entry {
    /// Makes `session` the open session, which this node dialed when `dialed`.
    fn replace(&mut self, session: &Session, dialed: bool) {
        self.session = session.downgrade();
        self.dialed = dialed;
    }

    /// Closes the held session with `Code(0)`: it lost.
    fn close_held(&mut self) {
        if let Some(held) = self.held.take() {
            held.close(Code(0));
        }
    }
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
    node: PublicKey,
    addresses: &[Address],
) -> Result<Session, Error> {
    let found = table.borrow_mut().find(node);
    let attempt = match found {
        Found::Open(session) => return Ok(session),
        Found::Dialing(attempt) => attempt,
        Found::Start(attempt) => {
            let table = table.borrow();
            let carrier = table.carrier.clone();
            let this = rc::Weak::clone(&table.this);
            let addresses = addresses.to_vec();
            let started = Rc::clone(&attempt);
            table.tasks.spawn(async move {
                let mut dial = pin!(dial::dial(&carrier, node, &addresses));
                // A session from the peer that ends the attempt drops the dial. The
                // take task can run after this one in the instant that both connect.
                let dialed = poll_fn(|cx| {
                    if let Some(table) = this.upgrade() {
                        table.borrow_mut().take();
                    }
                    if started.poll(cx).is_ready() {
                        return Poll::Ready(None);
                    }
                    dial.as_mut().poll(cx).map(Some)
                });
                let (Some(dialed), Some(table)) = (dialed.await, this.upgrade()) else {
                    return;
                };
                table
                    .borrow_mut()
                    .dialed(node, &started, dialed.map(Session::new));
            });
            attempt
        }
    };
    poll_fn(|cx| attempt.poll(cx)).await
}

/// Waits for the next session for `accept`: one that a dial made, else one that a
/// peer opened and that the table does not hold.
///
/// # Errors
///
/// As [`Transport::accept`](crate::Transport::accept).
pub(crate) async fn accept(table: &RefCell<Table>) -> Result<Session, Error> {
    poll_fn(|cx| {
        let mut table = table.borrow_mut();
        loop {
            if let Some(session) = table.poll_ready(cx) {
                return Poll::Ready(Ok(session));
            }
            let session = ready!(table.carrier.poll_accept(cx)).map(Session::new)?;
            if let Some(session) = table.arrive(session) {
                return Poll::Ready(Ok(session));
            }
        }
    })
    .await
}

impl Table {
    /// An empty table for the node `key`, which dials and accepts on `carrier`, and
    /// runs its dials and pings on `tasks`. A task on `tasks` takes each session
    /// that a peer opens as it comes.
    pub(crate) fn new(
        key: PublicKey,
        tasks: Tasks,
        carrier: quic::Handle,
    ) -> Rc<RefCell<Self>> {
        let table = Rc::new_cyclic(|this| {
            RefCell::new(Self {
                key,
                tasks,
                carrier,
                this: rc::Weak::clone(this),
                nodes: Map::default(),
                limit: 0,
                ready: VecDeque::new(),
                accepting: Vec::new(),
            })
        });
        let this = Rc::downgrade(&table);
        table.borrow().tasks.spawn(async move {
            poll_fn(|cx| match this.upgrade() {
                Some(table) => table.borrow_mut().poll_take(cx),
                None => Poll::Ready(()),
            })
            .await;
        });
        table
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

    /// Ends `attempt` to `node` with what its dial gave. A session waits for
    /// `accept`, and closes the held one. After an error, the attempt gives the held
    /// session.
    fn dialed(
        &mut self,
        node: PublicKey,
        attempt: &Rc<Attempt>,
        dialed: Result<Session, Error>,
    ) {
        let entry = self.nodes.get_mut(&node);
        let entry = entry.expect("invariant: a dial keeps its entry");
        let dial = entry.dial.take().expect("invariant: one dial ends it");
        assert!(
            Rc::ptr_eq(&dial, attempt),
            "invariant: one dial runs per node"
        );
        let result = match dialed {
            Ok(session) => {
                entry.close_held();
                entry.replace(&session, true);
                self.push(session.clone());
                Ok(session)
            }
            Err(error) => self.settle(node).ok_or(error),
        };
        attempt.end(result);
    }

    /// Takes each session that a peer opened and that no `accept` took from the
    /// carrier.
    fn take(&mut self) {
        while let Some(session) = self.carrier.accepted() {
            if let Some(session) = self.arrive(Session::new(session)) {
                self.push(session);
            }
        }
    }

    /// As [`Table::take`], and wakes `cx` when the next session waits in the carrier.
    /// Ready when the socket broke, which `accept` gives from the carrier.
    fn poll_take(&mut self, cx: &Context<'_>) -> Poll<()> {
        while let Ok(session) = ready!(self.carrier.poll_accept(cx)) {
            if let Some(session) = self.arrive(Session::new(session)) {
                self.push(session);
            }
        }
        Poll::Ready(())
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
        if !session.quic().live() {
            return Some(session);
        }
        let lower = self.key < node;
        let entry = self.entry(node);
        let open = entry.session.open();
        let dialed = open.is_some() && entry.dialed;
        if lower && (dialed || entry.dial.is_some()) {
            entry.close_held();
            entry.held = Some(session.clone());
            if let Some(open) = open.filter(|_| !entry.proving) {
                entry.proving = true;
                self.prove(node, &open, session.downgrade());
            }
            return None;
        }
        entry.close_held();
        entry.replace(&session, false);
        if let Some(attempt) = entry.dial.take() {
            attempt.end(Ok(session.clone()));
        }
        if let Some(open) = open {
            open.close(Code(0));
        }
        Some(session)
    }

    /// Pings `open`, the session that this node dialed to `node`, on the one task
    /// of the entry. When the peer answers a ping sent after the held session `held`
    /// arrived, the held session closes. When `open` ends or drops first, the held
    /// session wins.
    fn prove(&self, node: PublicKey, open: &Session, mut held: session::Weak) {
        let this = rc::Weak::clone(&self.this);
        let mut ping = open.quic().ping();
        self.tasks.spawn(async move {
            loop {
                // An error needs no case: the pinged session is then not open.
                drop(ping.await);
                let Some(table) = this.upgrade() else {
                    return;
                };
                let mut table = table.borrow_mut();
                let entry = table.nodes.get_mut(&node);
                let entry = entry.expect("invariant: a prune keeps a proving entry");
                let newer = entry.held.as_ref().filter(|session| !held.is(session));
                let open = entry.session.open();
                if let (Some(session), Some(open)) = (newer, &open) {
                    held = session.downgrade();
                    ping = open.quic().ping();
                    continue;
                }
                entry.proving = false;
                if open.is_some() {
                    entry.close_held();
                } else {
                    table.settle(node);
                }
                return;
            }
        });
    }

    /// The open session to `node`, after the table takes the sessions that wait in
    /// the carrier. When it has none and no dial runs, the held session wins first,
    /// if it is open, and goes to `accept`.
    fn settle(&mut self, node: PublicKey) -> Option<Session> {
        self.take();
        let entry = self.entry(node);
        if let Some(session) = entry.session.open() {
            return Some(session);
        }
        if entry.dial.is_some() {
            return None;
        }
        let held = entry.held.take().filter(|held| held.quic().live())?;
        entry.replace(&held, false);
        self.push(held.clone());
        Some(held)
    }

    /// Gives `session` to the next `accept`.
    fn push(&mut self, session: Session) {
        self.ready.push_back(session);
        self.accepting.drain(..).for_each(Waker::wake);
    }

    /// The entry of `node`, new when it has none. First it drops each entry with no
    /// open session, no dial, no held session, and no ping task, when the table is at
    /// its limit.
    fn entry(&mut self, node: PublicKey) -> &mut Entry {
        if self.nodes.len() >= self.limit {
            let live = |_: &PublicKey, entry: &mut Entry| {
                entry.proving
                    || entry.dial.is_some()
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
            assert!(!own(&transport, &open));
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

    #[test]
    fn accept_gives_the_error_of_a_broken_socket() {
        let (mut sim, _, server) = testing::nodes(0);
        testing::transport(&server, SERVER, |transport, node| async move {
            let mut accepting = pin!(transport.accept());
            assert!(testing::poll_once(accepting.as_mut()).await.is_none());
            node.fail_udp(testing::address(&node));
            let network = Error::Network {
                error: env::net::Error::Io { code: 5 },
            };
            assert_eq!(accepting.await.err(), Some(network));
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

    /// Whether `transport` dialed `session`, its open session to the peer. Public
    /// calls show it only once a later session from the peer arrives.
    fn own(transport: &Transport, session: &Session) -> bool {
        let Peer::Node(node) = session.peer() else {
            panic!("a session to a node");
        };
        let table = transport.table.borrow();
        let entry = &table.nodes[&node];
        assert!(entry.session.is(session), "the open session");
        entry.dialed
    }

    /// A node of [`two_nodes_that_dial_each_other_keep_the_session_of_the_lower_key`]
    /// with `key`. After a random delay under 750 us, it dials `peer` at `at`. A
    /// session reaches the peer three one-way delays of 250 us after its dial, so
    /// both dials start before either session arrives, and one may end first. It
    /// keeps each session that `dial` and `accept` give, and checks that each one
    /// but the open one ended with `Code(0)`.
    fn dial_at_once(node: &Node, key: PrivateKey, peer: PublicKey, at: [Address; 1]) {
        let lower = key.public() < peer;
        testing::shard(node, key, move |config, node| async move {
            let (transport, sessions) = accepting(config, &node);
            let offset = node.entropy().rng().below(750_000);
            let offset = Span::from_nanos(i64::try_from(offset).expect("small"));
            node.clock().sleep(offset).await;
            let dialed = transport.dial(peer, &at).await.expect("a session");
            sessions.borrow_mut().push(dialed);
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 100))
                .await;
            let open = transport.dial(peer, &[]).await.expect("the open session");
            assert_eq!(own(&transport, &open), lower);
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
            assert!(!own(&transport, &dialed));
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
            assert!(!own(&transport, &won));
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

    // As the test before, then a third session from the client 20 ms after the held
    // session won. The won session is not one that the server dialed, so the third
    // replaces it.
    #[test]
    fn a_third_session_replaces_the_held_session_that_won() {
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
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 40))
                .await;
            assert!(!won.quic().live());
            let third = sessions.borrow_mut().pop().expect("the third session");
            third.close(Code(8));
            linger(&node).await;
        });
        testing::start(&client, move |shard, node| async move {
            let [first, second, third] = copies(&shard, &node);
            let accepted = first.accept().await.expect("the server's session");
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 10))
                .await;
            let held = second.dial(SERVER.public(), &at).await.expect("a session");
            assert_eq!(accepted.closed().await, Error::PeerClosed { code: Code(0) });
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 20))
                .await;
            let newer = third.dial(SERVER.public(), &at).await.expect("a session");
            assert_eq!(held.closed().await, Error::PeerClosed { code: Code(0) });
            assert_eq!(newer.closed().await, Error::PeerClosed { code: Code(8) });
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

    // The client has the higher key and dials a dead address. The server's session
    // ends that attempt, and the client drops each handle to it, so it closes at once,
    // not when the dead dial times out.
    #[test]
    fn an_attempt_that_a_session_from_the_peer_ended_does_not_keep_it_open() {
        let (mut sim, client, server) = testing::nodes(0);
        let back = [Address::Udp(testing::address(&client))];
        let dead = dead(&server);
        testing::transport(&server, SERVER, move |transport, node| async move {
            let dialed = transport.dial(CLIENT.public(), &back).await;
            let dialed = dialed.expect("a session");
            let start = node.clock().now();
            assert_eq!(dialed.closed().await, Error::PeerClosed { code: Code(0) });
            let open = node.clock().now() - start;
            assert!(
                open < testing::spans(Span::MILLISECOND, 100),
                "open {open:?}"
            );
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            {
                let dial = pin!(transport.dial(SERVER.public(), &dead));
                assert!(testing::poll_once(dial).await.is_none());
            }
            drop(transport.accept().await.expect("the server's session"));
            node.clock().sleep(testing::spans(testing::IDLE, 5)).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The client has the higher key and dials a dead address, then a second
    // transport of the server after the stagger. The server's session ends that
    // attempt first, so the client's dial stops and never reaches the second.
    #[test]
    fn a_session_from_the_peer_stops_the_dial_of_the_higher_key() {
        let (mut sim, client, server) = testing::nodes(0);
        let back = [Address::Udp(testing::address(&client))];
        let second = SocketAddr::new(testing::address(&server).ip(), testing::PORT + 1);
        let slow = [dead(&server)[0], Address::Udp(second)];
        testing::start(&server, move |shard, node| async move {
            let [transport, witness] = [testing::address(&node), second].map(|at| {
                let part = testing::part(&node.net(), at);
                let config = shard.config(SERVER, testing::IDLE);
                Transport::new(config, part).expect("a transport")
            });
            let dialed = transport.dial(CLIENT.public(), &back).await;
            let dialed = dialed.expect("a session");
            node.clock().sleep(Span::SECOND).await;
            assert!(testing::poll_once(pin!(witness.accept())).await.is_none());
            dialed.close(Code(1));
            linger(&node).await;
        });
        testing::shard(&client, CLIENT, move |config, node| async move {
            let (transport, _sessions) = accepting(config, &node);
            let dialed = transport.dial(SERVER.public(), &slow).await;
            let dialed = dialed.expect("the server's session");
            assert_eq!(dialed.closed().await, Error::PeerClosed { code: Code(1) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // Only a session from a peer empties the carrier's accept wakers, so a dial
    // task that waits there leaves its waker after it ends.
    #[test]
    fn dials_with_no_session_from_a_peer_keep_no_wakers_in_the_carrier() {
        let (mut sim, client, server) = testing::nodes(0);
        let dead = dead(&server);
        let failed = unreachable(SERVER.public(), &server);
        testing::transport(&client, CLIENT, move |transport, _| async move {
            for _ in 0..50 {
                let dialed = transport.dial(SERVER.public(), &dead).await;
                assert_eq!(dialed.err(), Some(failed.clone()));
            }
            let wakers = transport.table.borrow().carrier.accept_wakers();
            assert_eq!(wakers, 1, "only the task that takes sessions waits");
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The client has the higher key, runs no accept, and dials a dead address. The
    // server's session reaches the client's carrier 1 ms later. Rule 4: the higher
    // node takes it at once and stops its own dial.
    #[test]
    fn a_session_from_the_peer_stops_a_dial_with_no_accept() {
        let (mut sim, client, server) = testing::nodes(0);
        let back = [Address::Udp(testing::address(&client))];
        let dead = dead(&server);
        testing::transport(&server, SERVER, move |transport, node| async move {
            node.clock().sleep(Span::MILLISECOND).await;
            let dialed = transport.dial(CLIENT.public(), &back).await;
            let dialed = dialed.expect("a session");
            assert_eq!(dialed.closed().await, Error::PeerClosed { code: Code(4) });
        });
        testing::transport(&client, CLIENT, move |transport, node| async move {
            let start = node.clock().now();
            let dialed = transport.dial(SERVER.public(), &dead).await;
            let waited = node.clock().now() - start;
            let dialed = dialed.expect("the server's session");
            assert!(
                waited < testing::spans(Span::MILLISECOND, 100),
                "waited {waited:?}"
            );
            dialed.close(Code(4));
            linger(&node).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The client's dial waits on a silent address while the server's session comes
    // through accept and ends the attempt. A second caller at 100 ms and the first
    // share that session.
    #[test]
    fn callers_of_one_dial_share_the_session_the_table_holds() {
        let (mut sim, client, server) = testing::nodes(0);
        let [silent] = dead(&server);
        let at = [silent, Address::Udp(testing::address(&server))];
        let back = [Address::Udp(testing::address(&client))];
        testing::transport(&server, SERVER, move |transport, node| async move {
            let mine = transport.dial(CLIENT.public(), &back).await;
            let _mine = mine.expect("a session");
            node.clock().sleep(testing::spans(testing::IDLE, 2)).await;
        });
        testing::shard(&client, CLIENT, move |config, node| async move {
            let tasks = config.tasks.clone();
            let part = testing::part(&node.net(), testing::address(&node));
            let transport = Rc::new(Transport::new(config, part).expect("a transport"));
            let accepting = Rc::clone(&transport);
            let held = Rc::new(std::cell::RefCell::new(Vec::new()));
            let keep = Rc::clone(&held);
            tasks.spawn(async move {
                while let Ok(session) = accepting.accept().await {
                    keep.borrow_mut().push(session);
                }
            });
            let clock = node.clock();
            let (first, second) =
                testing::join(transport.dial(SERVER.public(), &at), async {
                    clock.sleep(testing::spans(Span::MILLISECOND, 100)).await;
                    transport.dial(SERVER.public(), &[]).await
                })
                .await;
            let (first, second) =
                (first.expect("a session"), second.expect("a session"));
            first.close(Code(3));
            assert_eq!(second.closed().await, Error::Closed { code: Code(3) });
            held.borrow_mut().clear();
            linger(&node).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The client has the higher key and runs no accept. It dials 1 ms after the
    // server, so the server's session waits in the client's carrier when the
    // client's own dial ends. The dial gives the server's session.
    #[test]
    fn a_dial_that_completes_while_the_peers_session_waits_gives_it() {
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
            node.clock().sleep(Span::MILLISECOND).await;
            let dialed = transport.dial(SERVER.public(), &at).await;
            let dialed = dialed.expect("a session");
            let now = node.clock().now();
            let again = transport.dial(SERVER.public(), &[]).await;
            let again = again.expect("a session");
            assert_eq!(node.clock().now(), now);
            assert!(again.downgrade().is(&dialed));
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 100))
                .await;
            assert!(dialed.quic().live(), "{:?}", dialed.closed().await);
            linger(&node).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // Both nodes accept, and the client dials 250 us after the server, so the
    // server's session and the client's own dial connect at one instant. The dial
    // task runs first, and gives the server's session.
    #[test]
    fn a_dial_that_completes_with_the_peers_session_gives_it() {
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
        testing::shard(&client, CLIENT, move |config, node| async move {
            let (transport, sessions) = accepting(config, &node);
            node.clock().sleep(Span::from_nanos(250_000)).await;
            let dialed = transport.dial(SERVER.public(), &at).await;
            let dialed = dialed.expect("a session");
            let again = transport.dial(SERVER.public(), &[]).await;
            let again = again.expect("a session");
            assert!(again.downgrade().is(&dialed));
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 100))
                .await;
            assert_eq!(sessions.borrow().len(), 1);
            assert!(dialed.quic().live(), "{:?}", dialed.closed().await);
            linger(&node).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The server holds the client's second session, closes it once its own answers a
    // ping (after the peer's ack delay), then holds the third, and pings again.
    #[test]
    fn a_held_session_after_one_that_lost_gets_its_own_ping() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let back = [Address::Udp(testing::address(&client))];
        testing::transport(&server, SERVER, move |transport, node| async move {
            let first = transport.dial(CLIENT.public(), &back).await;
            let first = first.expect("a session");
            drop(transport.accept().await.expect("the dialed session"));
            for _ in 0..2 {
                node.clock()
                    .sleep(testing::spans(Span::MILLISECOND, 40))
                    .await;
                let open = transport.dial(CLIENT.public(), &[]).await;
                assert!(open.expect("the open session").downgrade().is(&first));
            }
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 40))
                .await;
            first.close(Code(3));
            linger(&node).await;
        });
        testing::start(&client, move |shard, node| async move {
            let [first, second, third] = copies(&shard, &node);
            let first = first.accept().await.expect("the server's session");
            for transport in [second, third] {
                node.clock()
                    .sleep(testing::spans(Span::MILLISECOND, 10))
                    .await;
                let held = transport.dial(SERVER.public(), &at).await;
                let held = held.expect("a session");
                assert_eq!(held.closed().await, Error::PeerClosed { code: Code(0) });
            }
            assert_eq!(first.closed().await, Error::PeerClosed { code: Code(3) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The server holds the client's session and pings its own, then closes both, and
    // dials a 16th node at once. The prune that this dial runs comes before the
    // carrier ends the ping, and keeps the entry of its task.
    #[test]
    fn a_prune_keeps_the_entry_of_a_ping_that_waits() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let back = [Address::Udp(testing::address(&client))];
        testing::transport(&server, SERVER, move |transport, node| async move {
            let first = transport.dial(CLIENT.public(), &back).await;
            let first = first.expect("a session");
            drop(transport.accept().await.expect("the dialed session"));
            let others = (10..25).map(|key| PrivateKey([key; 32]).public());
            let others: Vec<_> = others.collect();
            for &peer in &others[1..] {
                let dialed = transport.dial(peer, &[]).await;
                let attempts = Vec::new();
                assert_eq!(dialed.err(), Some(Error::Unreachable { peer, attempts }));
            }
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 20))
                .await;
            let open = transport.dial(CLIENT.public(), &[]).await;
            assert!(open.expect("the open session").downgrade().is(&first));
            first.close(Code(5));
            let held = transport.dial(CLIENT.public(), &[]).await;
            held.expect("the held session").close(Code(6));
            let dial = pin!(transport.dial(others[0], &[]));
            assert!(testing::poll_once(dial).await.is_none());
            {
                let table = transport.table.borrow();
                assert_eq!(table.nodes.len(), 2);
                assert!(table.nodes[&CLIENT.public()].proving);
            }
            linger(&node).await;
        });
        testing::start(&client, move |shard, node| async move {
            let [first, second] = copies(&shard, &node);
            let first = first.accept().await.expect("the server's session");
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 10))
                .await;
            let held = second.dial(SERVER.public(), &at).await;
            let held = held.expect("a session");
            assert_eq!(first.closed().await, Error::PeerClosed { code: Code(5) });
            assert_eq!(held.closed().await, Error::PeerClosed { code: Code(6) });
        });
        assert_eq!(sim.run(), Ok(()));
    }

    // The server holds the client's first session and pings its own, which no shard
    // answers. The client's second session waits in the carrier when the own one
    // idles out, so it wins, and the held one never goes to accept.
    #[test]
    fn a_failed_ping_lets_the_session_that_waits_for_accept_win() {
        let (mut sim, client, server) = testing::nodes(0);
        let at = [Address::Udp(testing::address(&server))];
        let back = [Address::Udp(testing::address(&client))];
        testing::transport(&server, SERVER, move |transport, node| async move {
            let first = transport.dial(CLIENT.public(), &back).await;
            let first = first.expect("a session");
            drop(transport.accept().await.expect("the dialed session"));
            node.clock()
                .sleep(testing::spans(Span::MILLISECOND, 20))
                .await;
            let open = transport.dial(CLIENT.public(), &[]).await;
            assert!(open.expect("the open session").downgrade().is(&first));
            node.clock().sleep(testing::spans(Span::SECOND, 2)).await;
            assert_eq!(first.closed().await, Error::TimedOut);
            let newest = transport.accept().await.expect("the second session");
            let open = transport.dial(CLIENT.public(), &[]).await;
            assert!(open.expect("the open session").downgrade().is(&newest));
            newest.close(Code(3));
            linger(&node).await;
        });
        // The first shard ends with no linger, so no close goes out.
        testing::transport(&client, CLIENT, move |transport, node| async move {
            let first = transport.accept().await.expect("the server's session");
            testing::start(&node, move |shard, node| async move {
                let [held, newest] = [1, 2].map(|index| {
                    let part = SocketAddr::new(shard.ip(), testing::PORT + index);
                    let part = testing::part(&node.net(), part);
                    let config = shard.config(CLIENT, testing::IDLE);
                    Transport::new(config, part).expect("a transport")
                });
                node.clock()
                    .sleep(testing::spans(Span::MILLISECOND, 10))
                    .await;
                let held = held.dial(SERVER.public(), &at).await.expect("a session");
                node.clock()
                    .sleep(testing::spans(Span::MILLISECOND, 20))
                    .await;
                let newest = newest.dial(SERVER.public(), &at).await;
                let newest = newest.expect("a session");
                assert_eq!(held.closed().await, Error::PeerClosed { code: Code(0) });
                assert_eq!(newest.closed().await, Error::PeerClosed { code: Code(3) });
            });
            drop(first);
        });
        assert_eq!(sim.run(), Ok(()));
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
            assert!(!own(&transport, &open));
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

    // The client dials a dead address first, so its first dial would connect 250 ms
    // later. The server's session ends that attempt, which stops the dial, and then
    // closes. A second dial starts, also slow, and gives its own session.
    #[test]
    fn a_dial_after_an_attempt_that_the_peer_ended_gives_its_own_session() {
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
            assert!(!own(&transport, &first));
            assert_eq!(first.closed().await, Error::PeerClosed { code: Code(1) });
            let again = transport.dial(SERVER.public(), &slow).await;
            let again = again.expect("a new session");
            assert!(own(&transport, &again));
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
            assert!(own(&transport, &second));
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
            assert!(!own(&transport, &open));
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
            assert!(own(&transport, &mine));
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
            assert!(dialed.quic().live(), "{:?}", dialed.closed().await);
            linger(&node).await;
        });
        assert_eq!(sim.run(), Ok(()));
    }
}
