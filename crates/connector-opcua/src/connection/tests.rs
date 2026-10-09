use std::cell::RefCell;
use std::ffi::{CString, c_char, c_int, c_void};
use std::future::poll_fn;
use std::net::SocketAddr;
use std::pin::Pin;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use env::clock::{Clock, Sleep};
use env::net::{Tcp, tcp};
use env::rng::Rng;
use sim::{Sim, node};
use types::time::{Monotonic, Span};

use super::{Manager, OPTIONS};
use crate::event::Loop;
use crate::ffi::{self, Bytes, ConnectionState, KeyValueMap, Status};

const PORT: u16 = 4840;

/// `UA_NodeId` with a numeric identifier.
#[repr(C)]
struct NodeId {
    namespace: u16,
    kind: c_int,
    numeric: u32,
    rest: [u32; 3],
}

/// `UA_QualifiedName`.
#[repr(C)]
struct QualifiedName {
    namespace: u16,
    name: Bytes,
}

unsafe extern "C" {
    fn UA_findDataType(id: *const NodeId) -> *const c_void;
    fn UA_KeyValueMap_setScalar(
        map: *mut KeyValueMap,
        key: QualifiedName,
        value: *const c_void,
        kind: *const c_void,
    ) -> u32;
    fn UA_KeyValueMap_clear(map: *mut KeyValueMap);
    fn UA_Client_disconnect(client: *mut ffi::Client) -> u32;
}

/// The numeric node of a builtin type.
const BOOLEAN: u32 = 1;
const UINT16: u32 = 5;
const STRING: u32 = 12;

/// A value of the parameters of `openConnection`.
enum Value<'a> {
    Boolean(bool),
    UInt16(u16),
    String(&'a str),
}

/// Gives a map of `values`. Free it with `UA_KeyValueMap_clear`.
fn map(values: &[(&str, Value<'_>)]) -> KeyValueMap {
    let mut map = KeyValueMap {
        size: 0,
        map: ptr::null_mut(),
    };
    for (key, value) in values {
        match value {
            Value::Boolean(v) => set(&mut map, key, ptr::from_ref(v).cast(), BOOLEAN),
            Value::UInt16(v) => set(&mut map, key, ptr::from_ref(v).cast(), UINT16),
            Value::String(v) => {
                let string = text(v);
                set(&mut map, key, ptr::from_ref(&string).cast(), STRING);
            }
        }
    }
    map
}

/// Gives a `UA_String` that borrows `text`.
fn text(text: &str) -> Bytes {
    Bytes {
        length: text.len(),
        data: text.as_ptr().cast_mut(),
    }
}

fn set(map: &mut KeyValueMap, key: &str, value: *const c_void, kind: u32) {
    let id = NodeId {
        namespace: 0,
        kind: 0,
        numeric: kind,
        rest: [0; 3],
    };
    // SAFETY: the id is a numeric node of namespace 0.
    let kind = unsafe { UA_findDataType(&raw const id) };
    assert!(!kind.is_null(), "a builtin type");
    let key = QualifiedName {
        namespace: 0,
        name: text(key),
    };
    // SAFETY: the map copies the key and the value, each of the type `kind`.
    let status = Status(unsafe { UA_KeyValueMap_setScalar(map, key, value, kind) });
    assert_eq!(status, Status::GOOD);
}

/// One call of the connection callback: the connection, the state, and the message.
type Call = (usize, ConnectionState, Vec<u8>);

/// A loop and its manager, and the calls of the connection callback.
struct Side {
    clock: Clock,
    events: Loop,
    manager: Manager,
    calls: Box<RefCell<Vec<Call>>>,
}

impl Side {
    fn new(node: &node::Node) -> Self {
        let clock = node.clock();
        let events = Loop::new(Clock::clone(&clock), &mut Rng::from_seed(0));
        let manager = Manager::new(&events, node.net());
        // SAFETY: the member takes its own loop.
        let status = Status(unsafe { (events.members().start)(events.raw()) });
        assert_eq!(status, Status::GOOD);
        Self {
            clock,
            events,
            manager,
            calls: Box::new(RefCell::new(Vec::new())),
        }
    }

    /// The manager, as the loop lists it.
    fn cm(&self) -> *mut ffi::ConnectionManager {
        self.events.members().sources.cast()
    }

    fn members(&self) -> &ffi::ConnectionManager {
        // SAFETY: the manager lives as long as `self`.
        unsafe { &*self.cm() }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }

    /// Opens a connection with `params`, with the callback that records each call.
    fn open(&self, params: &[(&str, Value<'_>)]) -> Status {
        let mut params = map(params);
        let application = ptr::from_ref(&*self.calls).cast_mut().cast();
        // SAFETY: the callback reads the calls, which live as long as the manager.
        let status = Status(unsafe {
            (self.members().open)(
                self.cm(),
                &raw const params,
                application,
                ptr::null_mut(),
                record,
            )
        });
        // SAFETY: `map` made it.
        unsafe { UA_KeyValueMap_clear(&raw mut params) };
        status
    }

    /// Opens a connection to `remote`.
    fn connect(&self, remote: SocketAddr) -> Status {
        let host = remote.ip().to_string();
        self.open(&[
            ("address", Value::String(&host)),
            ("port", Value::UInt16(remote.port())),
        ])
    }

    /// Sends `bytes` on connection `id` in a buffer of the manager.
    fn send(&self, id: usize, bytes: &[u8]) -> Status {
        send_on(self.cm(), id, bytes)
    }

    fn close(&self, id: usize) -> Status {
        // SAFETY: the member takes its own manager.
        Status(unsafe { (self.members().close)(self.cm(), id) })
    }

    fn run(&self) {
        // SAFETY: the member takes its own loop.
        let status =
            Status(unsafe { (self.events.members().run)(self.events.raw(), 0) });
        assert_eq!(status, Status::GOOD);
    }

    /// Polls the manager and runs the loop until `span` passes, as the owner of a
    /// client does: it sleeps until the next timer, and polls again only on that
    /// timer or on a wake.
    async fn drive(&self, span: Span) {
        let end = self.clock.now() + span;
        let mut sleep: Option<(Monotonic, Sleep)> = None;
        poll_fn(|cx| {
            loop {
                self.manager.poll(cx);
                self.run();
                if self.clock.now() >= end {
                    return Poll::Ready(());
                }
                let next = self.events.next().map_or(end, |next| next.min(end));
                if sleep.as_ref().is_none_or(|(at, _)| *at != next) {
                    sleep = Some((next, self.clock.sleep_until(next)));
                }
                let (_, timer) = sleep.as_mut().expect("invariant: set above");
                match Pin::new(timer).poll(cx) {
                    Poll::Ready(()) => sleep = None,
                    Poll::Pending => return Poll::Pending,
                }
            }
        })
        .await;
    }
}

/// Sends `bytes` on connection `id` of `cm` in a buffer of the manager.
fn send_on(cm: *mut ffi::ConnectionManager, id: usize, bytes: &[u8]) -> Status {
    let mut buffer = Bytes {
        length: 0,
        data: ptr::null_mut(),
    };
    // SAFETY: the manager lives through the test.
    let members = unsafe { &*cm };
    // SAFETY: the member takes its own manager.
    let status =
        Status(unsafe { (members.alloc)(cm, id, &raw mut buffer, bytes.len()) });
    assert_eq!(status, Status::GOOD);
    // SAFETY: the buffer holds `bytes.len()` bytes.
    unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), buffer.data, bytes.len()) };
    let params = KeyValueMap {
        size: 0,
        map: ptr::null_mut(),
    };
    // SAFETY: the member takes its own manager, and the send takes the buffer.
    let status =
        Status(unsafe { (members.send)(cm, id, &raw const params, &raw mut buffer) });
    assert!(buffer.data.is_null(), "the send takes the buffer");
    status
}

/// Records a call in the `RefCell<Vec<Call>>` at `application`.
unsafe extern "C" fn record(
    _: *mut ffi::ConnectionManager,
    id: usize,
    application: *mut c_void,
    _: *mut *mut c_void,
    state: ConnectionState,
    _: *const KeyValueMap,
    message: Bytes,
) {
    // SAFETY: `open` passes the calls of a live side.
    let calls = unsafe { &*application.cast::<RefCell<Vec<Call>>>() };
    let bytes = if message.length == 0 {
        Vec::new()
    } else {
        // SAFETY: the manager gives `length` bytes at `data` for the call.
        unsafe { std::slice::from_raw_parts(message.data, message.length) }.to_vec()
    };
    calls.borrow_mut().push((id, state, bytes));
}

/// What the peer read: each read with its time, and when the stream ended.
#[derive(Default)]
struct Reads {
    reads: Vec<(Monotonic, Vec<u8>)>,
    ended: Option<Monotonic>,
}

impl Reads {
    fn bytes(&self) -> Vec<u8> {
        self.reads
            .iter()
            .flat_map(|(_, bytes)| bytes.clone())
            .collect()
    }
}

/// A run with the manager on one node and a TCP peer on another.
struct Network {
    sim: Sim,
    local: node::Node,
    peer: node::Node,
}

impl Network {
    fn new() -> Self {
        let mut sim = Sim::new(sim::Config::default());
        let local = sim.node(node::Config::default());
        let peer = sim.node(node::Config::default());
        Self { sim, local, peer }
    }

    fn remote(&self) -> SocketAddr {
        SocketAddr::new(self.peer.addresses()[0], PORT)
    }

    /// Accepts one stream on the peer, writes `answer` and closes when it is given,
    /// and records what it reads until the stream ends.
    fn serve(&self, answer: Option<&'static [u8]>) -> Arc<Mutex<Reads>> {
        let listen = tcp::Listen {
            local: self.remote(),
            backlog: 4,
            options: OPTIONS,
        };
        let mut listener = self.peer.net().listen(&listen).expect("the port is free");
        let clock = self.peer.clock();
        let reads = Arc::new(Mutex::new(Reads::default()));
        let slot = Arc::clone(&reads);
        let config = env::shards::Config {
            name: "peer".into(),
            core: None,
        };
        let handle = self.peer.shards().start(config, move |_| async move {
            let mut stream = poll_fn(|cx| listener.poll_accept(cx))
                .await
                .expect("a stream comes");
            if let Some(answer) = answer {
                write(&mut stream, answer).await;
                poll_fn(|cx| stream.poll_close(cx))
                    .await
                    .expect("the close works");
            }
            loop {
                let mut buffer = [0; 512];
                let read = poll_fn(|cx| stream.poll_read(cx, &mut buffer)).await;
                let mut reads = slot.lock().expect("no panic under the lock");
                match read {
                    Ok(0) | Err(_) => {
                        reads.ended = Some(clock.now());
                        break;
                    }
                    Ok(n) => reads.reads.push((clock.now(), buffer[..n].to_vec())),
                }
            }
        });
        drop(handle.expect("the shard starts"));
        reads
    }
}

async fn write(stream: &mut Tcp, bytes: &[u8]) {
    let mut sent = 0;
    while sent < bytes.len() {
        let parts = [std::io::IoSlice::new(&bytes[sent..])];
        sent += poll_fn(|cx| stream.poll_write(cx, &parts))
            .await
            .expect("the write works");
    }
}

#[test]
fn a_connect_gives_opening_then_established_and_the_peer_reads_each_send() {
    let mut network = Network::new();
    let reads = network.serve(None);
    let remote = network.remote();
    let calls = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            assert_eq!(side.calls(), [(1, ffi::OPENING, vec![])]);
            side.drive(Span::SECOND).await;
            assert_eq!(side.send(1, b"one"), Status::GOOD);
            assert_eq!(side.send(1, b"two"), Status::GOOD);
            assert_eq!(side.close(1), Status::GOOD);
            assert_eq!(side.close(1), Status::GOOD);
            assert_eq!(side.send(1, b"three"), Status::BAD_CONNECTION_CLOSED);
            side.drive(Span::SECOND).await;
            assert_eq!(side.close(1), Status::BAD_NOT_FOUND);
            assert_eq!(side.send(1, b"four"), Status::BAD_CONNECTION_CLOSED);
            side.calls()
        })
        .expect("the run ends");
    assert_eq!(
        calls,
        [
            (1, ffi::OPENING, vec![]),
            (1, ffi::ESTABLISHED, vec![]),
            (1, ffi::CLOSING, vec![]),
        ]
    );
    let reads = reads.lock().expect("no panic under the lock");
    assert_eq!(reads.bytes(), b"onetwo");
    assert!(reads.ended.is_some());
}

/// Three sends of 30000 bytes pass the 64 KiB send buffer, so the stream takes part
/// of a buffer in one write and the rest in a later one.
#[test]
fn the_peer_reads_each_byte_of_sends_that_the_stream_takes_in_parts() {
    let mut network = Network::new();
    let reads = network.serve(None);
    let remote = network.remote();
    let sends: Vec<Vec<u8>> = (0..3u8)
        .map(|k| (0..=u8::MAX).cycle().take(30_000).map(|b| b ^ k).collect())
        .collect();
    let written = sends.clone();
    network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::SECOND).await;
            for send in &written {
                assert_eq!(side.send(1, send), Status::GOOD);
            }
            side.drive(Span::SECOND).await;
            assert_eq!(side.close(1), Status::GOOD);
            side.drive(Span::SECOND).await;
        })
        .expect("the run ends");
    let reads = reads.lock().expect("no panic under the lock");
    assert_eq!(reads.bytes(), sends.concat());
    assert!(reads.ended.is_some());
}

#[test]
fn a_connect_with_no_listener_gives_closing_and_no_established() {
    let mut network = Network::new();
    let remote = network.remote();
    let calls = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::SECOND).await;
            assert_eq!(side.close(1), Status::BAD_NOT_FOUND);
            side.calls()
        })
        .expect("the run ends");
    assert_eq!(
        calls,
        [(1, ffi::OPENING, vec![]), (1, ffi::CLOSING, vec![])]
    );
}

#[test]
fn a_close_of_the_peer_gives_its_bytes_then_closing_once() {
    let mut network = Network::new();
    let reads = network.serve(Some(b"ack"));
    let remote = network.remote();
    let calls = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::SECOND).await;
            assert_eq!(side.close(1), Status::BAD_NOT_FOUND);
            assert_eq!(side.send(1, b"late"), Status::BAD_CONNECTION_CLOSED);
            side.calls()
        })
        .expect("the run ends");
    assert_eq!(
        calls,
        [
            (1, ffi::OPENING, vec![]),
            (1, ffi::ESTABLISHED, vec![]),
            (1, ffi::ESTABLISHED, b"ack".to_vec()),
            (1, ffi::CLOSING, vec![]),
        ]
    );
    let reads = reads.lock().expect("no panic under the lock");
    assert!(reads.reads.is_empty());
    assert!(reads.ended.is_some());
}

#[test]
fn a_drop_unlinks_the_manager_and_drops_a_closing_that_waits() {
    let mut network = Network::new();
    let reads = network.serve(None);
    let remote = network.remote();
    let calls = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::SECOND).await;
            assert_eq!(side.close(1), Status::GOOD);
            let Side {
                events,
                manager,
                calls,
                ..
            } = side;
            drop(manager);
            assert!(events.members().sources.is_null());
            // SAFETY: the member takes its own loop.
            let status = Status(unsafe { (events.members().run)(events.raw(), 0) });
            assert_eq!(status, Status::GOOD);
            calls.take()
        })
        .expect("the run ends");
    let states: Vec<_> = calls.iter().map(|(_, state, _)| *state).collect();
    assert_eq!(states, [ffi::OPENING, ffi::ESTABLISHED]);
    assert!(
        reads
            .lock()
            .expect("no panic under the lock")
            .ended
            .is_some()
    );
}

#[test]
fn an_open_without_its_parameters_or_to_listen_is_refused_with_no_call() {
    let mut network = Network::new();
    let calls = network
        .sim
        .run_on(&network.local.clone(), |node, _| async move {
            let side = Side::new(&node);
            let statuses = [
                side.open(&[("address", Value::String("10.0.0.2"))]),
                side.open(&[("port", Value::UInt16(PORT))]),
                side.open(&[
                    ("address", Value::String("10.0.0.2")),
                    ("port", Value::String("4840")),
                ]),
                side.open(&[
                    ("listen", Value::Boolean(true)),
                    ("port", Value::UInt16(PORT)),
                ]),
            ];
            assert_eq!(
                statuses,
                [
                    Status::BAD_INVALID_ARGUMENT,
                    Status::BAD_INVALID_ARGUMENT,
                    Status::BAD_INVALID_ARGUMENT,
                    Status::BAD_NOT_SUPPORTED,
                ]
            );
            side.calls()
        })
        .expect("the run ends");
    assert_eq!(calls, []);
}

#[test]
fn ids_count_from_one_and_a_listen_of_false_connects() {
    let mut network = Network::new();
    let first = network.serve(None);
    let remote = network.remote();
    let calls = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            let host = remote.ip().to_string();
            let status = side.open(&[
                ("listen", Value::Boolean(false)),
                ("address", Value::String(&host)),
                ("port", Value::UInt16(PORT)),
            ]);
            assert_eq!(status, Status::GOOD);
            side.drive(Span::SECOND).await;
            let closed = SocketAddr::new(remote.ip(), PORT + 1);
            assert_eq!(side.connect(closed), Status::GOOD);
            side.drive(Span::SECOND).await;
            assert_eq!(side.close(1), Status::GOOD);
            side.drive(Span::SECOND).await;
            side.calls()
        })
        .expect("the run ends");
    let states: Vec<_> = calls.iter().map(|(id, state, _)| (*id, *state)).collect();
    assert_eq!(
        states,
        [
            (1, ffi::OPENING),
            (1, ffi::ESTABLISHED),
            (2, ffi::OPENING),
            (2, ffi::CLOSING),
            (1, ffi::CLOSING),
        ]
    );
    assert!(
        first
            .lock()
            .expect("no panic under the lock")
            .ended
            .is_some()
    );
}

/// What a timer does in a run of the loop.
#[derive(Clone, Copy)]
enum Action {
    Connect,
    Send,
    Close,
}

/// What the timer of `after_a_timer` reads.
struct Later<'a> {
    side: &'a Side,
    action: Action,
    remote: SocketAddr,
}

unsafe extern "C" fn act(_: *mut c_void, data: *mut c_void) {
    // SAFETY: `after_a_timer` keeps it live through the run.
    let later = unsafe { &*data.cast::<Later<'_>>() };
    let status = match later.action {
        Action::Connect => later.side.connect(later.remote),
        Action::Send => later.side.send(1, b"late"),
        Action::Close => later.side.close(1),
    };
    assert_eq!(status, Status::GOOD);
}

unsafe extern "C" fn nothing(_: *mut c_void, _: *mut c_void) {}

/// The outcome of `after_a_timer`.
struct Outcome {
    /// The due time of the timer.
    due: Monotonic,
    /// The calls of the connection callback until 500 ms after the timer.
    calls: Vec<Call>,
    reads: Arc<Mutex<Reads>>,
}

/// Connects to a peer first unless `action` connects, then does `action` in a timer
/// due in 10 ms, with another timer due in 1 s. The owner polls the manager again
/// before 1 s only when the action wakes it.
fn after_a_timer(action: Action) -> Outcome {
    let mut network = Network::new();
    let reads = network.serve(None);
    let remote = network.remote();
    let (due, calls) = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            if !matches!(action, Action::Connect) {
                assert_eq!(side.connect(remote), Status::GOOD);
            }
            side.drive(Span::from_nanos(100_000_000)).await;
            let due = side.clock.now() + Span::from_nanos(10_000_000);
            let later = Later {
                side: &side,
                action,
                remote,
            };
            let timers: [(ffi::Callback, f64, *mut c_void); 2] = [
                (act, 10.0, ptr::from_ref(&later).cast_mut().cast()),
                (nothing, 1000.0, ptr::null_mut()),
            ];
            for (callback, interval_ms, data) in timers {
                let mut key = 0;
                // SAFETY: the member takes its own loop, and `later` outlives the
                // run.
                let status = Status(unsafe {
                    (side.events.members().add_timer)(
                        side.events.raw(),
                        callback,
                        ptr::null_mut(),
                        data,
                        interval_ms,
                        ptr::null_mut(),
                        ffi::ONCE,
                        &raw mut key,
                    )
                });
                assert_eq!(status, Status::GOOD);
            }
            side.drive(Span::from_nanos(500_000_000)).await;
            let calls = side.calls();
            if !matches!(action, Action::Close) {
                assert_eq!(side.close(1), Status::GOOD);
            }
            side.drive(Span::SECOND).await;
            (due, calls)
        })
        .expect("the run ends");
    Outcome { due, calls, reads }
}

/// The latest time that a step the timer starts may end, with no wait for the 1 s
/// timer.
fn soon(due: Monotonic) -> Monotonic {
    due + Span::from_nanos(50_000_000)
}

#[test]
fn a_connect_from_a_run_of_the_loop_is_established_with_no_other_event() {
    let outcome = after_a_timer(Action::Connect);
    let states: Vec<_> = outcome.calls.iter().map(|(_, state, _)| *state).collect();
    assert_eq!(states, [ffi::OPENING, ffi::ESTABLISHED]);
}

#[test]
fn a_send_from_a_run_of_the_loop_goes_out_with_no_other_event() {
    let outcome = after_a_timer(Action::Send);
    let reads = outcome.reads.lock().expect("no panic under the lock");
    let [(at, bytes)] = &reads.reads[..] else {
        panic!("one read: {:?}", reads.reads.len());
    };
    assert_eq!(bytes, b"late");
    assert!(
        *at <= soon(outcome.due),
        "read at {at:?}, due at {:?}",
        outcome.due
    );
}

#[test]
fn a_close_from_a_run_of_the_loop_ends_the_stream_with_no_other_event() {
    let outcome = after_a_timer(Action::Close);
    let states: Vec<_> = outcome.calls.iter().map(|(_, state, _)| *state).collect();
    assert_eq!(states, [ffi::OPENING, ffi::ESTABLISHED, ffi::CLOSING]);
    let reads = outcome.reads.lock().expect("no panic under the lock");
    let ended = reads.ended.expect("the stream ends");
    assert!(
        ended <= soon(outcome.due),
        "ended at {ended:?}, due at {:?}",
        outcome.due
    );
}

#[test]
fn a_client_sends_hel_and_its_disconnect_ends_the_stream() {
    let mut network = Network::new();
    let reads = network.serve(None);
    let remote = network.remote();
    let calls = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            // SAFETY: the loop outlives the client, which the test deletes.
            let client = unsafe { ffi::shim_client_new(side.events.raw()) };
            assert!(!client.is_null());
            let url = CString::new(format!("opc.tcp://{remote}")).expect("no NUL");
            let url: *const c_char = url.as_ptr();
            // SAFETY: the client lives, and copies the URL.
            let status =
                Status(unsafe { ffi::test::UA_Client_connectAsync(client, url) });
            assert_eq!(status, Status::GOOD);
            side.drive(Span::SECOND).await;
            // SAFETY: the client lives.
            let status = Status(unsafe { UA_Client_disconnect(client) });
            assert_eq!(status, Status::GOOD);
            side.drive(Span::SECOND).await;
            // SAFETY: nothing uses the client after it.
            unsafe { ffi::UA_Client_delete(client) };
            side.calls()
        })
        .expect("the run ends");
    assert_eq!(calls, []);
    let reads = reads.lock().expect("no panic under the lock");
    let bytes = reads.bytes();
    assert_eq!(&bytes[..4], b"HELF", "{bytes:?}");
    let length = u32::from_le_bytes(bytes[4..8].try_into().expect("4 bytes"));
    assert_eq!(usize::try_from(length).expect("u32 fits"), bytes.len());
    assert!(reads.ended.is_some());
}
