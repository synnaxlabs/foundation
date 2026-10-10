//! Runs the event loop of open62541 alone, and a connection manager with a server and
//! clients on it, for benchmarks and allocation tests. Not part of the contract of the
//! crate.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::cell::Cell;
use std::ffi::{CString, c_void};
use std::future::{Future as _, poll_fn};
use std::mem::ManuallyDrop;
use std::net::{IpAddr, SocketAddr};
use std::pin::{Pin, pin};
use std::ptr::{self, NonNull};
use std::task::{Context, Poll, Waker};

use env::clock::Clock;
use env::net::Net;
use env::rng::Rng;
use types::time::{Monotonic, Span};

use crate::connection;
use crate::event::Loop;
use crate::ffi::{self, Status};

/// A client of open62541 on its own event loop, which also runs a count of repeated
/// timers that do nothing.
pub struct Client {
    raw: NonNull<ffi::Client>,
    events: Loop,
}

impl Client {
    /// Makes and starts a loop on `clock` with a client and `timers` repeated timers
    /// of 1 ms.
    ///
    /// # Panics
    ///
    /// Panics if open62541 refuses the client, or, with the status name, if it gives a
    /// status other than `Good` for the first run or a timer.
    #[must_use]
    pub fn new(clock: Clock, timers: usize) -> Self {
        let events = Loop::new(clock, &mut Rng::from_seed(0));
        // SAFETY: the loop outlives the client, which `drop` deletes first.
        let client = unsafe { ffi::shim_client_new(events.raw()) };
        let raw = NonNull::new(client).expect("open62541 refused the client");
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

/// A timer callback that does nothing.
unsafe extern "C" fn idle(_: *mut c_void, _: *mut c_void) {}

/// The port of the server of a [`Manager`].
const PORT: u16 = 4840;

/// The time that a [`Manager`] gives its clients to connect. A connect has a timeout
/// only while a request waits: before its first request, it can wait forever.
const CONNECT_TIMEOUT: Span = Span::from_nanos(60_000_000_000);

/// A connection manager over `env::net` with a test server of open62541 and `idle + 1`
/// clients, each with an activated session and the namespaces of the server, on its
/// loop. The first client reads; the others send nothing.
pub struct Manager {
    clients: Vec<NonNull<ffi::Client>>,
    server: NonNull<ffi::test::Server>,
    /// C holds its address while a read waits.
    answers: Box<Answers>,
    /// Only `close` drops it, after it deletes the server and the clients on its loop.
    connections: ManuallyDrop<connection::Manager>,
}

#[derive(Default)]
struct Answers {
    count: Cell<usize>,
    /// The first status other than `Good`.
    failed: Cell<Option<Status>>,
}

impl Manager {
    /// Makes the manager on `clock` and `net`, with its listener on `address` and port
    /// 4840, and drives it until each client is connected. Then it runs `body` on the
    /// manager, and closes each session and deletes the server and the clients. A
    /// panic in `body` leaks them.
    ///
    /// # Panics
    ///
    /// If `idle` is 65535 or more, as open62541 counts sessions in 16 bits, if a
    /// connect fails or does not end in 60 s of `clock`, if open62541 refuses the
    /// server, a client, or a step of the close, or if the port is taken.
    pub async fn scope<T>(
        clock: Clock,
        net: Net,
        address: IpAddr,
        idle: usize,
        body: impl AsyncFnOnce(&Self) -> T,
    ) -> T {
        let this = Self::new(clock, net, address, idle).await;
        let value = body(&this).await;
        this.close().await;
        value
    }

    async fn new(clock: Clock, net: Net, address: IpAddr, idle: usize) -> Self {
        let sessions = u16::try_from(idle.saturating_add(1))
            .expect("open62541 counts sessions in 16 bits");
        let local = SocketAddr::new(address, PORT);
        let rng = &mut Rng::from_seed(0);
        let manager = connection::Manager::listening(
            Clock::clone(&clock),
            net,
            local,
            u32::from(sessions),
            rng,
        )
        .expect("the port is free");
        let events = manager.events();
        // SAFETY: the member takes its own loop.
        let status = Status(unsafe { (events.members().start)(events.raw()) });
        assert_eq!(status, Status::GOOD, "open62541 refused the loop");
        // SAFETY: the loop outlives the server, which `close` deletes.
        let server = unsafe {
            ffi::test::shim_server_new(
                events.raw(),
                PORT,
                c"opc.tcp://:4840".as_ptr(),
                sessions,
            )
        };
        let server = NonNull::new(server).expect("open62541 refused the server");
        // SAFETY: the server lives.
        let status =
            Status(unsafe { ffi::test::UA_Server_run_startup(server.as_ptr()) });
        assert_eq!(status, Status::GOOD, "open62541 refused the server start");
        let url = CString::new(format!("opc.tcp://{local}")).expect("no NUL");
        let clients = (0..=idle)
            .map(|_| {
                // SAFETY: the loop outlives the client, which `close` deletes.
                let client = unsafe { ffi::shim_client_new(events.raw()) };
                let client = NonNull::new(client).expect("open62541 refused a client");
                // SAFETY: the client lives, and copies the URL.
                let status = Status(unsafe {
                    ffi::test::UA_Client_connectAsync(client.as_ptr(), url.as_ptr())
                });
                assert_eq!(status, Status::GOOD, "open62541 refused a connect");
                client
            })
            .collect();
        let this = Self {
            clients,
            server,
            answers: Box::default(),
            connections: ManuallyDrop::new(manager),
        };
        this.connect(&clock).await;
        this
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
            self.connections.events().run();
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
        self.drive_after(|| ());
    }

    /// Drives the manager once, as `drive` does, with a run that first asks the first
    /// client for a read of the current time of the server.
    ///
    /// # Panics
    ///
    /// If open62541 refuses the read.
    pub fn ask(&self) {
        self.drive_after(|| self.read(ffi::test::TIME));
    }

    /// Asks the first client for a read of the Value of `node` of namespace 0.
    fn read(&self, node: u32) {
        let data = ptr::from_ref::<Answers>(&self.answers).cast_mut().cast();
        // SAFETY: the client lives, and `answers` outlives it.
        let status = Status(unsafe {
            ffi::test::shim_client_read(self.clients[0].as_ptr(), node, answer, data)
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

    /// Asks each client to close its session, stops the server, deletes it with
    /// `delete_server`, then deletes the clients.
    async fn close(mut self) {
        for client in &self.clients {
            // SAFETY: the client lives.
            let status = Status(unsafe {
                ffi::test::UA_Client_disconnectAsync(client.as_ptr())
            });
            assert_eq!(status, Status::GOOD, "open62541 refused a disconnect");
        }
        let server = self.server.as_ptr();
        // SAFETY: the server lives.
        let status = Status(unsafe { ffi::test::UA_Server_run_shutdown(server) });
        assert_eq!(status, Status::GOOD, "open62541 refused the server stop");
        // SAFETY: the server lives on the loop, and nothing uses it after.
        unsafe { self.connections.delete_server(server) }.await;
        for client in self.clients.drain(..) {
            // SAFETY: nothing uses the client after it. It asked to disconnect, so the
            // delete waits for no answer.
            unsafe { ffi::UA_Client_delete(client.as_ptr()) };
        }
        // SAFETY: nothing is on its loop, and nothing uses it after it.
        unsafe { ManuallyDrop::drop(&mut self.connections) };
    }

    fn drive_after(&self, first: impl FnOnce()) {
        let mut first = Some(first);
        let drive = pin!(self.connections.drive(|_| {
            if let Some(first) = first.take() {
                first();
            }
            self.connections.events().run();
            Poll::Ready(())
        }));
        let ready = drive.poll(&mut Context::from_waker(Waker::noop()));
        assert!(ready.is_ready(), "a drive whose run is ready ends");
    }

    /// Gives the count of clients with the namespaces of the server, which a client
    /// reads after its session activates.
    ///
    /// # Panics
    ///
    /// If the connect of a client failed, as a client does not try it again.
    fn connected(&self) -> usize {
        let connected = |client: &&NonNull<ffi::Client>| {
            let mut status = Status::GOOD;
            // SAFETY: the client lives.
            unsafe {
                ffi::test::UA_Client_getState(
                    client.as_ptr(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &raw mut status.0,
                );
            }
            assert!(status == Status::GOOD, "a connect failed: {status:?}");
            // SAFETY: the client lives.
            unsafe { ffi::test::shim_client_namespaced(client.as_ptr()) }
        };
        self.clients.iter().filter(connected).count()
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
/// `data` points at live `Answers`, and `value` at the answer when `status` is `Good`.
unsafe extern "C" fn answer(
    _: *mut ffi::Client,
    data: *mut c_void,
    _: u32,
    status: u32,
    value: *mut c_void,
) {
    // SAFETY: `read` gives live answers.
    let answers = unsafe { &*data.cast::<Answers>() };
    answers.count.set(answers.count.get() + 1);
    let mut status = Status(status);
    if status == Status::GOOD {
        // SAFETY: open62541 gives the answer with `Good`.
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
    use sim::{Sim, node};
    use types::time::Span;

    use super::{Client, Manager};
    use crate::ffi;

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
            Manager::scope(node.clock(), node.net(), address, idle, body).await;
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
            for _ in 0..2 {
                let body = async |manager: &Manager| manager.answers();
                let answers =
                    Manager::scope(node.clock(), node.net(), address, 3, body).await;
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
            Manager::scope(node.clock(), node.net(), address, idle, body).await;
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

    /// A client reads the namespaces after its session activates, and a scope waits for
    /// them, so that its close cancels no read.
    #[test]
    fn a_scope_waits_for_the_namespaces_of_each_client() {
        check(3, async |manager, _| {
            for client in &manager.clients {
                // SAFETY: the client lives.
                let namespaced =
                    unsafe { ffi::test::shim_client_namespaced(client.as_ptr()) };
                assert!(namespaced, "{manager:?}");
            }
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
            manager.drive_after(|| manager.read(999_999));
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

    /// The connect takes 4 ms on the default link. The node pauses for 61 s at 3.9 ms,
    /// while the last answer is in flight, so the connect ends after 1m of `clock`.
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
                clock.sleep_until(start + Span::from_nanos(3_900_000)).await;
                pauser.pause(Span::from_nanos(61_000_000_000));
            });
            let body = async |_: &Manager| {};
            Manager::scope(node.clock(), node.net(), address, 0, body).await;
        })
        .expect("the run ends");
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

    #[test]
    fn a_run_runs_the_due_timers() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let clock = sim.node(sim::node::Config::default()).clock();
        let mut client = Client::new(clock, 1);
        let first = client.next().expect("a timer waits");
        sim.run_for(Span::MILLISECOND).expect("the run has no task");
        client.run();
        assert_eq!(client.next(), Some(first + Span::MILLISECOND), "{client:?}");
    }

    #[test]
    fn the_debug_gives_the_next_timer() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let clock = sim.node(sim::node::Config::default()).clock();
        let client = Client::new(clock, 0);
        assert_eq!(
            format!("{client:?}"),
            "Client { next: Some(Monotonic(3601000000000)), .. }"
        );
    }
}
