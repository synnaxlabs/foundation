//! Runs the event loop of open62541 alone, its logger, and a connection manager with a
//! server and clients on it, for benchmarks and allocation tests. [`client`] runs a
//! server with clients that each have their own manager. [`kind`] writes the frames of
//! the `opcua` kind with no server. [`value`] decodes the values of a read. Not part of
//! the contract of the crate.

#![expect(unsafe_code, reason = "open62541 is a C library")]

pub mod client;
pub mod kind;
pub mod value;

use std::cell::Cell;
use std::ffi::{CStr, CString, c_void};
use std::future::{Future as _, poll_fn};
use std::mem::{ManuallyDrop, MaybeUninit};
use std::net::{IpAddr, SocketAddr};
use std::panic;
use std::pin::{Pin, pin};
use std::ptr::{self, NonNull};
use std::task::{Context, Poll, Waker};

use env::clock::Clock;
use env::net::Net;
use env::rng::Rng;
use types::time::{Monotonic, Span};

use crate::client::Nodes;
use crate::connection;
use crate::event::{self, Loop};
use crate::ffi::{self, Status};
use crate::node::{Key, Node};
use crate::server::Server;
use crate::unwind::caught;

/// A client of open62541 on its own event loop, which also runs a count of repeated
/// timers that do nothing.
pub struct Client {
    raw: NonNull<ffi::Client>,
    events: Loop,
}

impl Client {
    /// Makes and starts a loop on `clock` and `rng`, with a client and `timers`
    /// repeated timers of 1 ms.
    ///
    /// # Panics
    ///
    /// With the status name, if open62541 gives a status other than `Good` for the
    /// first run or a timer.
    ///
    /// # Aborts
    ///
    /// When open62541 cannot allocate the loop or the client, as [`alloc::exhausted`]
    /// says.
    #[must_use]
    pub fn new(clock: Clock, rng: &mut Rng, timers: usize) -> Self {
        let events =
            Loop::new(event::Time::monotonic(clock), tracing::Span::none(), rng);
        // SAFETY: the loop outlives the client, which `drop` deletes first.
        let raw = unsafe { crate::client::unconnected(events.raw()) };
        let mut client = Self { raw, events };
        client.run();
        let events = &client.events;
        for _ in 0..timers {
            // SAFETY: the member takes its own loop, and `idle` reads nothing.
            let status = Status(unsafe {
                (events.members().add_timer)(
                    events.raw(),
                    idle,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    1.0,
                    ptr::null_mut(),
                    ffi::CURRENT_TIME,
                    ptr::null_mut(),
                )
            });
            assert_eq!(status, Status::GOOD, "open62541 refused a timer");
        }
        client
    }

    /// Gives the due time of the next timer, or `None` when no timer waits or the next
    /// timer is due after the clock ends.
    #[must_use]
    pub fn next(&self) -> Option<Monotonic> {
        self.events.next()
    }

    /// Runs the due timers and delayed callbacks once.
    ///
    /// # Panics
    ///
    /// If open62541 gives a status other than `Good`, with its name.
    pub fn run(&mut self) {
        // SAFETY: the client and its loop live.
        let status =
            Status(unsafe { ffi::UA_Client_run_iterate(self.raw.as_ptr(), 0) });
        assert_eq!(status, Status::GOOD, "open62541 failed a run");
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("next", &self.next())
            .finish_non_exhaustive()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // SAFETY: the client lives, and its loop outlives it.
        unsafe { ffi::UA_Client_delete(self.raw.as_ptr()) };
    }
}

/// An event loop of open62541 whose logger writes each line as an event with a
/// parent span, as the loop of a connection does.
pub struct Log {
    events: Loop,
}

impl Log {
    /// Makes a loop on `clock` and `rng` whose lines have the parent `span`.
    ///
    /// # Aborts
    ///
    /// When open62541 cannot allocate the loop, as [`alloc::exhausted`] says.
    #[must_use]
    pub fn new(clock: Clock, rng: &mut Rng, span: tracing::Span) -> Self {
        Self {
            events: Loop::new(event::Time::monotonic(clock), span, rng),
        }
    }

    /// Logs `text` as a warning of the category `client` through the logger of
    /// open62541. Past 100 warnings in a window of 1 s of the clock, it only counts
    /// it.
    pub fn warn(&self, text: &CStr) {
        // SAFETY: the loop lives, and `text` ends in a NUL.
        unsafe { ffi::test::shim_log_warning(self.events.raw(), text.as_ptr()) };
    }
}

impl std::fmt::Debug for Log {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Log").finish_non_exhaustive()
    }
}

/// A timer callback that does nothing.
unsafe extern "C" fn idle(_: *mut c_void, _: *mut c_void) {}

/// The port of each test server.
const PORT: u16 = 4840;

/// The time that a [`Manager`] gives its clients to connect. A connect has a timeout
/// only while a request waits: before its first request, it can wait forever.
const CONNECT_TIMEOUT: Span = Span::from_nanos(60_000_000_000);

/// A connection manager over `env::net` with a test server of open62541 and `idle + 1`
/// clients, each with an activated session, on its loop. The first client reads; the
/// others send nothing.
pub struct Manager {
    clients: Vec<NonNull<ffi::Client>>,
    server: Server,
    /// C holds its address while a read waits.
    answers: Box<Answers>,
    /// The read of the current time of the server.
    time: Nodes,
    /// Only `close` drops it, after it deletes the server and the clients on its loop.
    /// After a panic of the close, nothing runs its loop again.
    connections: ManuallyDrop<connection::Manager>,
}

#[derive(Default)]
struct Answers {
    count: Cell<usize>,
    /// The first status other than `Good`.
    failed: Cell<Option<Status>>,
}

impl Manager {
    /// Makes the manager on `clock`, `net`, and `rng`, with its listener on `address`
    /// and port 4840, and drives it until each client is connected. Then it runs `body`
    /// on the manager, and deletes the server and the clients, also after a panic in
    /// the connect or in `body`, which it then resumes.
    ///
    /// # Panics
    ///
    /// If `idle` is 65535 or more, as open62541 counts sessions in 16 bits, if a
    /// connect fails or does not end in 60 s of `clock`, if open62541 refuses the
    /// server or a step of the close, if a channel of a client is not closed after the
    /// close, or if the port is taken. A panic of the close leaks the loop and its
    /// connections, and each server and client that it has not deleted, and replaces a
    /// panic of the connect or `body`. A panic before the connect leaks what was made
    /// by then: the manager, the server, and each client.
    ///
    /// # Aborts
    ///
    /// When open62541 cannot allocate the loop, the connection manager, a client, or
    /// the Read request of the time of the server, as [`alloc::exhausted`] says.
    pub async fn scope<T>(
        clock: Clock,
        net: Net,
        address: IpAddr,
        idle: usize,
        rng: &mut Rng,
        body: impl AsyncFnOnce(&Self) -> T,
    ) -> T {
        let this = Self::new(Clock::clone(&clock), net, address, idle, rng);
        let value = caught(async {
            this.connect(&clock).await;
            body(&this).await
        })
        .await;
        this.close().await;
        value.unwrap_or_else(|panic| panic::resume_unwind(panic))
    }

    fn new(
        clock: Clock,
        net: Net,
        address: IpAddr,
        idle: usize,
        rng: &mut Rng,
    ) -> Self {
        let sessions = u16::try_from(idle.saturating_add(1))
            .expect("open62541 counts sessions in 16 bits");
        let local = SocketAddr::new(address, PORT);
        let manager = ManuallyDrop::new(
            connection::Manager::listening(
                event::Time::monotonic(clock),
                net,
                local,
                u32::from(sessions),
                rng,
            )
            .expect("the port is free"),
        );
        let events = manager.events();
        // SAFETY: the member takes its own loop.
        let status = Status(unsafe { (events.members().start)(events.raw()) });
        assert_eq!(status, Status::GOOD, "open62541 refused the loop");
        // SAFETY: only `close` drops the manager, after it closes the server. A panic
        // before then leaks it.
        let server = unsafe { Server::start(&manager, c"", sessions) };
        let url = CString::new(format!("opc.tcp://{local}")).expect("no NUL");
        let clients = (0..=idle)
            .map(|_| {
                // SAFETY: the loop outlives the client, which `close` deletes.
                let client = unsafe { crate::client::unconnected(events.raw()) };
                // SAFETY: the client lives, and copies the URL.
                let status = Status(unsafe {
                    ffi::UA_Client_connectAsync(client.as_ptr(), url.as_ptr())
                });
                assert_eq!(status, Status::GOOD, "open62541 refused a connect");
                client
            })
            .collect();
        Self {
            clients,
            server,
            answers: Box::default(),
            time: Nodes::unchecked(&[Node {
                namespace: 0,
                key: Key::Number(ffi::TIME),
            }]),
            connections: manager,
        }
    }

    /// Drives the manager until each client is connected.
    ///
    /// # Panics
    ///
    /// If a connect fails or does not end in [`CONNECT_TIMEOUT`] of `clock`.
    async fn connect(&self, clock: &Clock) {
        let clients = self.clients.len();
        let mut deadline = clock.sleep(CONNECT_TIMEOUT);
        let mut connect = pin!(self.connections.drive(|_| {
            if self.connected() == clients {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }));
        // The deadline goes first, so a connect that a jump of the clock ends late
        // still panics.
        poll_fn(|cx| {
            if Pin::new(&mut deadline).poll(cx).is_ready() {
                let connected = self.connected();
                panic!(
                    "{connected} of {clients} clients connected in {CONNECT_TIMEOUT}"
                );
            }
            connect.as_mut().poll(cx)
        })
        .await;
    }

    /// Drives the manager once: a pass, one run of the loop, and the moves after it.
    pub fn drive(&self) {
        self.drive_then(|| ());
    }

    /// Drives the manager once, as `drive` does, and asks the first client after the
    /// run for a read of the current time of the server.
    ///
    /// # Panics
    ///
    /// If open62541 refuses the read.
    pub fn ask(&self) {
        self.drive_then(|| self.read(&self.time));
    }

    /// Asks the first client for the read of `nodes`, which holds one node.
    fn read(&self, nodes: &Nodes) {
        let data = ptr::from_ref::<Answers>(&self.answers).cast_mut().cast();
        // SAFETY: the client and the request live, and `answers` outlives the client.
        let status = Status(unsafe {
            ffi::shim_client_read(
                self.clients[0].as_ptr(),
                nodes.raw(),
                answer,
                data,
                ptr::null_mut(),
            )
        });
        assert_eq!(status, Status::GOOD, "open62541 refused the read");
    }

    /// Gives the count of reads answered.
    ///
    /// # Panics
    ///
    /// If an answer has a status other than `Good`, with its name.
    #[must_use]
    pub fn answers(&self) -> usize {
        if let Some(status) = self.answers.failed.get() {
            panic!("a read failed: {status:?}");
        }
        self.answers.count.get()
    }

    /// Closes the channel of each client, closes the server, then deletes the clients.
    ///
    /// # Panics
    ///
    /// If a channel is not closed after the drive of [`Server::close`], as the delete
    /// of its client would then wait for a close that nothing drives.
    async fn close(mut self) {
        for client in &self.clients {
            // SAFETY: the client lives.
            let status = Status(unsafe {
                ffi::UA_Client_disconnectSecureChannelAsync(client.as_ptr())
            });
            assert_eq!(status, Status::GOOD, "open62541 refused a disconnect");
        }
        self.server.close(&self.connections).await;
        for client in self.clients.drain(..) {
            assert_eq!(
                // SAFETY: the client lives.
                unsafe { state(client) }.channel,
                ffi::Channel::CLOSED,
                "invariant: the drive of the server close closes each channel"
            );
            // SAFETY: nothing uses the client after it.
            unsafe { ffi::UA_Client_delete(client.as_ptr()) };
        }
        // SAFETY: nothing is on its loop, and nothing uses it after it.
        unsafe { ManuallyDrop::drop(&mut self.connections) };
    }

    /// Drives the manager once, and calls `then` after the run of the loop.
    fn drive_then(&self, then: impl FnOnce()) {
        let mut then = Some(then);
        let drive = pin!(self.connections.drive(|_| {
            if let Some(then) = then.take() {
                then();
            }
            Poll::Ready(())
        }));
        let ready = drive.poll(&mut Context::from_waker(Waker::noop()));
        assert!(ready.is_ready(), "a drive whose run is ready ends");
    }

    /// Gives the count of clients whose session is active.
    ///
    /// # Panics
    ///
    /// If the connect of a client failed, as a client does not try it again.
    fn connected(&self) -> usize {
        let connected = |client: &&NonNull<ffi::Client>| {
            // SAFETY: the client lives.
            let State {
                session, status, ..
            } = unsafe { state(**client) };
            assert!(status == Status::GOOD, "a connect failed: {status:?}");
            session == ffi::Session::ACTIVATED
        };
        self.clients.iter().filter(connected).count()
    }
}

/// The state of the channel and the connect of a client of a [`Manager`].
struct State {
    channel: ffi::Channel,
    session: ffi::Session,
    /// The status of the connect.
    status: Status,
}

/// Gives the state of `client`.
///
/// # Safety
///
/// `client` lives.
unsafe fn state(client: NonNull<ffi::Client>) -> State {
    let (mut channel, mut session) = (MaybeUninit::uninit(), MaybeUninit::uninit());
    let mut status = MaybeUninit::uninit();
    // SAFETY: the client lives.
    unsafe {
        ffi::UA_Client_getState(
            client.as_ptr(),
            channel.as_mut_ptr(),
            session.as_mut_ptr(),
            status.as_mut_ptr(),
        );
    }
    // SAFETY: the call writes each output.
    let channel = unsafe { channel.assume_init() };
    // SAFETY: as above.
    let session = unsafe { session.assume_init() };
    // SAFETY: as above.
    let status = Status(unsafe { status.assume_init() });
    State {
        channel,
        session,
        status,
    }
}

impl std::fmt::Debug for Manager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Manager")
            .field("clients", &self.clients.len())
            .field("answers", &self.answers.count.get())
            .finish_non_exhaustive()
    }
}

/// Counts an answer of a read in the [`Answers`] at `data`, and keeps the first
/// status other than `Good`, of the read or of its value.
///
/// # Safety
///
/// `data` points at live `Answers`, and `response` at the answer.
unsafe extern "C" fn answer(
    _: *mut ffi::Client,
    data: *mut c_void,
    _: u32,
    response: *mut ffi::ReadResponse,
) {
    // SAFETY: `read` gives live answers.
    let answers = unsafe { &*data.cast::<Answers>() };
    answers.count.set(answers.count.get() + 1);
    // SAFETY: open62541 gives a live response.
    let mut status = Status(unsafe { ffi::shim_response_result(response.cast()) });
    if status == Status::GOOD {
        // SAFETY: the response lives and has one result, as its request has one node.
        let value = unsafe { ffi::shim_read_result(response, 0) };
        // SAFETY: the value lives with the response.
        status = Status(unsafe { ffi::test::shim_value_status(value) });
    }
    if status != Status::GOOD && answers.failed.get().is_none() {
        answers.failed.set(Some(status));
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{self, AssertUnwindSafe};

    use env::clock::Clock;
    use env::rng::Rng;
    use sim::{Sim, node};
    use types::time::Span;

    use super::{Client, Log, Manager};
    use crate::client::Nodes;
    use crate::ffi;
    use crate::node::{Key, Node};
    use crate::unwind::caught;

    /// The delay of the default link.
    fn delay() -> Span {
        sim::link::Config::default().delay
    }

    /// Runs `body` in the scope of a manager with `idle` idle clients.
    fn check(idle: usize, body: impl AsyncFnOnce(&Manager, Clock) + Send + 'static) {
        let mut sim = Sim::new(sim::Config::default());
        let node = sim.node(node::Config::default());
        sim.run_on(&node, move |node, _| async move {
            let (clock, address) = (node.clock(), node.addresses()[0]);
            let body = async |manager: &Manager| body(manager, clock).await;
            let rng = &mut node.entropy().rng();
            Manager::scope(node.clock(), node.net(), address, idle, rng, body).await;
        })
        .expect("the run ends");
    }

    /// A scope starts its loop from the next value of `rng`, so `rng` moves on by one.
    #[test]
    fn a_scope_draws_one_value_of_its_rng() {
        let mut sim = Sim::new(sim::Config::default());
        let node = sim.node(node::Config::default());
        let next = sim
            .run_on(&node, move |node, _| async move {
                let (address, rng) = (node.addresses()[0], &mut Rng::from_seed(1));
                Manager::scope(node.clock(), node.net(), address, 0, rng, async |_| ())
                    .await;
                rng.next_u64()
            })
            .expect("the run ends");
        let fresh = &mut Rng::from_seed(1);
        fresh.next_u64();
        assert_eq!(next, fresh.next_u64(), "the scope drew no value");
    }

    /// The server of a scope has one channel for each client and closes the stream of a
    /// peer, and the count on the rng of the caller passes when a loop starts from a
    /// constant, so only the generator of open62541 on this thread shows that a scope
    /// takes the value it draws.
    #[test]
    fn each_scope_of_one_rng_draws_other_values() {
        let mut sim = Sim::new(sim::Config::default());
        let node = sim.node(node::Config::default());
        let drawn = sim
            .run_on(&node, move |node, _| async move {
                let (address, rng) = (node.addresses()[0], &mut Rng::from_seed(1));
                let mut drawn = Vec::new();
                for _ in 0..2 {
                    let body = async |_: &Manager| {
                        // SAFETY: it draws from the generator of this thread.
                        unsafe { ffi::test::UA_UInt32_random() }
                    };
                    drawn.push(
                        Manager::scope(node.clock(), node.net(), address, 0, rng, body)
                            .await,
                    );
                }
                drawn
            })
            .expect("the run ends");
        assert_ne!(drawn[0], drawn[1], "two scopes drew the same value");
    }

    /// A scope closes after a panic in its body, so the next scope takes its port.
    #[test]
    fn a_scope_whose_body_panics_frees_its_port() {
        let mut sim = Sim::new(sim::Config::default());
        let node = sim.node(node::Config::default());
        sim.run_on(&node, move |node, _| async move {
            let address = node.addresses()[0];
            let body = async |_: &Manager| panic!("the body panics");
            let rng = &mut node.entropy().rng();
            let scope = Manager::scope(node.clock(), node.net(), address, 0, rng, body);
            let panic = caught(scope).await.expect_err("the body panics");
            assert_eq!(panic.downcast_ref(), Some(&"the body panics"));
            let body = async |manager: &Manager| manager.answers();
            let scope = Manager::scope(node.clock(), node.net(), address, 0, rng, body);
            assert_eq!(scope.await, 0);
        })
        .expect("the run ends");
    }

    /// A panic of the close replaces the panic of the body. The close then leaks the
    /// loop, its connections, the server, and the clients, so the child runs with no
    /// leak check.
    #[test]
    fn a_panic_of_the_close_replaces_a_panic_of_the_body() {
        let name = "bench::tests::the_close_panics_after_the_body";
        crate::child::run(name, &[("ASAN_OPTIONS", "detect_leaks=0")]);
    }

    /// Stops the server in a body that panics, so the close panics, when
    /// `child::running()`.
    #[test]
    fn the_close_panics_after_the_body() {
        if !crate::child::running() {
            return;
        }
        let mut sim = Sim::new(sim::Config::default());
        let node = sim.node(node::Config::default());
        sim.run_on(&node, move |node, _| async move {
            let body = async |manager: &Manager| {
                // SAFETY: the server lives.
                let status =
                    unsafe { ffi::test::UA_Server_run_shutdown(manager.server.raw()) };
                assert_eq!(ffi::Status(status), ffi::Status::GOOD);
                panic!("the body panics");
            };
            let address = node.addresses()[0];
            let rng = &mut node.entropy().rng();
            let scope = Manager::scope(node.clock(), node.net(), address, 0, rng, body);
            let panic = caught(scope).await.expect_err("the close panics");
            let message = panic.downcast_ref::<String>().map(String::as_str);
            assert_eq!(
                message,
                Some(
                    "assertion `left == right` failed: open62541 refused the server \
                     stop\n  left: BadInternalError\n right: Good"
                )
            );
        })
        .expect("the run ends");
    }

    #[test]
    fn a_read_is_answered_in_the_third_drive_after_two_hops() {
        for idle in [0, 3] {
            check(idle, async move |manager, clock| {
                for read in 1..=3 {
                    manager.ask();
                    clock.sleep(delay()).await;
                    manager.drive();
                    assert_eq!(manager.answers(), read - 1, "the server answered");
                    clock.sleep(delay()).await;
                    manager.drive();
                    assert_eq!(manager.answers(), read, "{idle} idle, read {read}");
                }
            });
        }
    }

    #[test]
    fn a_drive_before_the_hop_gets_no_answer() {
        check(0, async |manager, clock| {
            manager.ask();
            manager.drive();
            clock.sleep(delay()).await;
            manager.drive();
            manager.drive();
            assert_eq!(manager.answers(), 0);
            clock.sleep(delay()).await;
            manager.drive();
            assert_eq!(manager.answers(), 1);
        });
    }

    /// The close of a scope frees the port and deletes each client before the next
    /// scope, also when the close of a stream takes a long time to reach its peer.
    #[test]
    fn a_scope_closes_before_the_next_on_a_slow_link() {
        let link = sim::link::Config {
            delay: Span::from_nanos(100_000_000),
            ..sim::link::Config::default()
        };
        let mut sim = Sim::new(sim::Config {
            link,
            ..sim::Config::default()
        });
        let node = sim.node(node::Config::default());
        sim.run_on(&node, move |node, _| async move {
            let address = node.addresses()[0];
            let rng = &mut node.entropy().rng();
            for _ in 0..2 {
                let body = async |manager: &Manager| manager.answers();
                let answers =
                    Manager::scope(node.clock(), node.net(), address, 3, rng, body)
                        .await;
                assert_eq!(answers, 0);
            }
        })
        .expect("the run ends");
    }

    #[test]
    #[should_panic(
        expected = "open62541 counts sessions in 16 bits: TryFromIntError(PosOverflow)"
    )]
    fn a_scope_with_more_sessions_than_16_bits_count_panics() {
        check(65_535, async |_, _| ());
    }

    #[test]
    #[should_panic(
        expected = "open62541 counts sessions in 16 bits: TryFromIntError(PosOverflow)"
    )]
    fn a_scope_with_the_most_idle_clients_panics_at_the_sessions() {
        check(usize::MAX, async |_, _| ());
    }

    /// A client tries a connect once, so its timeout must end the scope.
    #[test]
    #[should_panic(expected = "a connect failed: BadTimeout")]
    fn a_scope_whose_connect_times_out_panics_with_its_status() {
        let link = sim::link::Config {
            delay: Span::from_nanos(3_000_000_000),
            ..sim::link::Config::default()
        };
        check_on(link, 0, async |_| ());
    }

    /// Runs `body` in the scope of a manager with `idle` idle clients, on links of
    /// `link`.
    fn check_on(
        link: sim::link::Config,
        idle: usize,
        body: impl AsyncFnOnce(&Manager) + Send + 'static,
    ) {
        let mut sim = Sim::new(sim::Config {
            link,
            ..sim::Config::default()
        });
        let node = sim.node(node::Config::default());
        sim.run_on(&node, move |node, _| async move {
            let address = node.addresses()[0];
            let rng = &mut node.entropy().rng();
            Manager::scope(node.clock(), node.net(), address, idle, rng, body).await;
        })
        .expect("the run ends");
    }

    /// With jitter, the sessions of a scope activate in different drives. No public
    /// call gives the count of clients that a scope waits for, and a later drive still
    /// gets each answer, so the test reads `connected`.
    #[test]
    fn a_scope_on_a_link_with_jitter_connects_each_client() {
        let link = sim::link::Config {
            jitter: Span::from_nanos(1_000_000),
            ..sim::link::Config::default()
        };
        check_on(link, 15, async |manager| {
            assert_eq!(manager.connected(), 16);
        });
    }

    /// The stream of the connect opens after 200 s, and no request waits until then.
    #[test]
    #[should_panic(expected = "0 of 1 clients connected in 1m")]
    fn a_scope_whose_connect_does_not_end_panics_at_its_deadline() {
        let link = sim::link::Config {
            delay: Span::from_nanos(100_000_000_000),
            ..sim::link::Config::default()
        };
        check_on(link, 0, async |_| ());
    }

    /// The stream of the connect opens after 200 s, so the deadline panic comes at
    /// 60 s of `clock`.
    #[test]
    fn the_deadline_of_a_connect_is_one_minute() {
        let link = sim::link::Config {
            delay: Span::from_nanos(100_000_000_000),
            ..sim::link::Config::default()
        };
        let mut sim = Sim::new(sim::Config {
            link,
            ..sim::Config::default()
        });
        let node = sim.node(node::Config::default());
        sim.run_on(&node, move |node, _| async move {
            let (clock, address) = (node.clock(), node.addresses()[0]);
            let start = clock.now();
            let rng = &mut node.entropy().rng();
            let scope =
                Manager::scope(node.clock(), node.net(), address, 0, rng, async |_| ());
            let panic = caught(scope).await.expect_err("the connect does not end");
            assert_eq!(clock.now() - start, Span::from_nanos(60_000_000_000));
            let message = panic.downcast_ref::<String>().map(String::as_str);
            assert_eq!(message, Some("0 of 1 clients connected in 1m"));
        })
        .expect("the run ends");
    }

    /// The minimal config of a server takes 100 sessions.
    #[test]
    fn a_scope_takes_more_clients_than_the_sessions_of_a_minimal_server() {
        check(100, async |manager, _| assert_eq!(manager.connected(), 101));
    }

    /// A read callback of open62541 gets `Good` also when the value has a bad status.
    /// `answers` panics after a failed read, so the test reads the count of answers in
    /// the `Debug` of the manager.
    #[test]
    fn a_read_of_an_unknown_node_fails_with_its_status() {
        check(0, async |manager, clock| {
            let unknown = Nodes::unchecked(&[Node {
                namespace: 0,
                key: Key::Number(999_999),
            }]);
            manager.drive_then(|| manager.read(&unknown));
            clock.sleep(delay()).await;
            manager.drive();
            clock.sleep(delay()).await;
            manager.drive();
            let failed = panic::catch_unwind(AssertUnwindSafe(|| manager.answers()))
                .expect_err("the read failed");
            let message = failed.downcast_ref::<String>().expect("a formatted panic");
            assert_eq!(message, "a read failed: BadNodeIdUnknown");
            assert_eq!(
                format!("{manager:?}"),
                "Manager { clients: 1, answers: 1, .. }"
            );
        });
    }

    /// The connect takes 3.5 ms on the default link. The node pauses for 61 s at
    /// 3.4 ms, while the last answer is in flight, so the connect ends after 1m of
    /// `clock`.
    #[test]
    #[should_panic(expected = "clients connected in 1m")]
    fn a_scope_whose_connect_ends_after_its_deadline_in_a_pause_panics() {
        let mut sim = Sim::new(sim::Config::default());
        let node = sim.node(node::Config::default());
        sim.run_on(&node, move |node, tasks| async move {
            let (clock, address) = (node.clock(), node.addresses()[0]);
            let start = clock.now();
            let pauser = node.clone();
            tasks.spawn(async move {
                clock.sleep_until(start + Span::from_nanos(3_400_000)).await;
                pauser.pause(Span::from_nanos(61_000_000_000));
            });
            let body = async |_: &Manager| {};
            let rng = &mut node.entropy().rng();
            Manager::scope(node.clock(), node.net(), address, 0, rng, body).await;
        })
        .expect("the run ends");
    }

    /// Runs a scope with `idle` idle clients on links of `link`, with a pause of the
    /// node for 61 s at `at` of the connect, and gives the message of its panic.
    fn paused(link: sim::link::Config, idle: usize, at: Span) -> String {
        let mut sim = Sim::new(sim::Config {
            link,
            ..sim::Config::default()
        });
        let node = sim.node(node::Config::default());
        sim.run_on(&node, move |node, tasks| async move {
            let (clock, address) = (node.clock(), node.addresses()[0]);
            let start = clock.now();
            let pauser = node.clone();
            tasks.spawn(async move {
                clock.sleep_until(start + at).await;
                pauser.pause(Span::from_nanos(61_000_000_000));
            });
            let body = async |_: &Manager| {};
            let rng = &mut node.entropy().rng();
            let scope =
                Manager::scope(node.clock(), node.net(), address, idle, rng, body);
            let panic = caught(scope).await.expect_err("the connect panics");
            panic
                .downcast_ref::<String>()
                .expect("a formatted panic")
                .clone()
        })
        .expect("the run ends")
    }

    /// At 3.3 ms, the request to activate the session waits for its answer, so the
    /// close finds an open channel and a session that is not activated.
    #[test]
    fn a_scope_closes_after_a_deadline_in_the_handshake() {
        let link = sim::link::Config::default();
        let message = paused(link, 0, Span::from_nanos(3_300_000));
        assert_eq!(message, "0 of 1 clients connected in 1m");
    }

    /// With jitter, 8 of the 16 clients have an active session at 13 ms.
    #[test]
    fn the_deadline_panic_gives_the_count_of_connected_clients() {
        let link = sim::link::Config {
            jitter: Span::from_nanos(1_000_000),
            ..sim::link::Config::default()
        };
        let message = paused(link, 15, Span::from_nanos(13_000_000));
        assert_eq!(message, "8 of 16 clients connected in 1m");
    }

    #[test]
    fn the_debug_of_a_manager_gives_its_clients_and_answers() {
        check(2, async |manager, _| {
            assert_eq!(
                format!("{manager:?}"),
                "Manager { clients: 3, answers: 0, .. }"
            );
        });
    }

    /// Each client starts its loop from the next value of `rng`, so two clients of one
    /// `rng` take two values.
    #[test]
    fn each_client_draws_one_value_of_its_rng() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let rng = &mut Rng::from_seed(1);
        drop((
            Client::new(node.clock(), rng, 0),
            Client::new(node.clock(), rng, 0),
        ));
        let fresh = &mut Rng::from_seed(1);
        fresh.next_u64();
        fresh.next_u64();
        assert_eq!(rng.next_u64(), fresh.next_u64(), "a client drew no value");
    }

    /// A `Client` has no peer, and the count on the rng of the caller passes when a
    /// loop starts from a constant, so only the generator of open62541 on this thread
    /// shows that each loop takes the value it draws.
    #[test]
    fn each_client_of_one_rng_draws_other_values() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let mut rng = node.entropy().rng();
        let first = Client::new(node.clock(), &mut rng, 0);
        // SAFETY: it draws from the generator of this thread.
        let a = unsafe { ffi::test::UA_UInt32_random() };
        let second = Client::new(node.clock(), &mut rng, 0);
        // SAFETY: as above.
        let b = unsafe { ffi::test::UA_UInt32_random() };
        drop((first, second));
        assert_ne!(a, b, "two clients drew the same value");
    }

    #[test]
    fn a_run_runs_the_due_timers() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let mut client = Client::new(node.clock(), &mut node.entropy().rng(), 1);
        let first = client.next().expect("a timer waits");
        sim.run_for(Span::MILLISECOND).expect("the run has no task");
        client.run();
        assert_eq!(client.next(), Some(first + Span::MILLISECOND), "{client:?}");
    }

    #[test]
    fn the_debug_gives_the_next_timer() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let client = Client::new(node.clock(), &mut node.entropy().rng(), 0);
        assert_eq!(
            format!("{client:?}"),
            "Client { next: Some(Monotonic(3601000000000)), .. }"
        );
    }

    #[test]
    fn a_warning_goes_to_target_open62541_under_the_span_of_the_log() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let (capture, dispatch) = connector::testing::Capture::new("open62541");
        tracing::dispatcher::with_default(&dispatch, || {
            let span = tracing::error_span!("connector", connector = "press");
            let log = Log::new(node.clock(), &mut node.entropy().rng(), span);
            log.warn(c"hot");
        });
        let line = r#"WARN hot connector="press" category="client""#;
        assert_eq!(capture.lines(), [line]);
    }

    #[test]
    fn the_debug_of_a_log_gives_no_field() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let span = tracing::Span::none();
        let log = Log::new(node.clock(), &mut node.entropy().rng(), span);
        assert_eq!(format!("{log:?}"), "Log { .. }");
    }
}
