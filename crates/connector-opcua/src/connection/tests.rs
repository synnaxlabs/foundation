use std::cell::{Cell, RefCell};
use std::convert::Infallible;
use std::ffi::{CString, c_char, c_void};
use std::future::poll_fn;
use std::io::IoSlice;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::ptr;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use env::clock::Clock;
use env::net::{Tcp, tcp};
use env::rng::Rng;
use sim::{Sim, node};
use types::time::{Monotonic, Span};

use super::{LINGER, Manager, OPTIONS, READ_BYTES, SENDS};
use crate::child;
use crate::event::Loop;
use crate::ffi::test::{
    Members, NodeId, QualifiedName, UA_Client_disconnect, UA_KeyValueMap_clear,
    UA_KeyValueMap_setScalar, UA_findDataType,
};
use crate::ffi::{self, Bytes, ConnectionState, KeyValueMap, Status};

const PORT: u16 = 4840;

/// The delay of a link of the sim.
const DELAY: Span = Span::from_nanos(250_000);

/// Gives `count` sends of `length` bytes, each of other bytes than the 255 before it.
fn sends(count: usize, length: usize) -> Vec<Vec<u8>> {
    (0..=u8::MAX)
        .cycle()
        .take(count)
        .map(|k| (0..=u8::MAX).cycle().take(length).map(|b| b ^ k).collect())
        .collect()
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

/// A manager and its loop, and the calls of the connection callback.
struct Side {
    clock: Clock,
    manager: Manager,
    calls: Box<RefCell<Vec<Call>>>,
    /// The connection callback of each open: [`record`] unless a test sets another.
    callback: ffi::ConnectionCallback,
}

impl Side {
    fn new(node: &node::Node) -> Self {
        let clock = node.clock();
        let manager =
            Manager::new(Clock::clone(&clock), node.net(), &mut Rng::from_seed(0));
        let events = manager.events();
        // SAFETY: the member takes its own loop.
        let status = Status(unsafe { (events.members().start)(events.raw()) });
        assert_eq!(status, Status::GOOD);
        Self {
            clock,
            manager,
            calls: Box::new(RefCell::new(Vec::new())),
            callback: record,
        }
    }

    fn events(&self) -> &Loop {
        self.manager.events()
    }

    /// The manager, as the loop lists it.
    fn cm(&self) -> *mut ffi::ConnectionManager {
        self.events().members().sources.cast()
    }

    fn members(&self) -> &Members {
        // SAFETY: the manager lives as long as `self`.
        unsafe { &*self.cm().cast::<Members>() }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }

    /// Opens a connection with `params` and the callback of the side.
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
                self.callback,
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

    /// The count of connections in the table. A connection that open62541 has closed
    /// and whose stream is gone shows only here.
    fn connections(&self) -> usize {
        self.manager.state().table.borrow().len()
    }

    fn states(&self) -> Vec<ConnectionState> {
        self.calls().iter().map(|(_, state, _)| *state).collect()
    }

    /// Adds a timer of the loop that calls `callback` with `data` once, in
    /// `interval_ms`.
    fn add_timer(&self, callback: ffi::Callback, interval_ms: f64, data: *mut c_void) {
        let events = self.events();
        let mut key = 0;
        // SAFETY: the member takes its own loop. The caller keeps `data` live until
        // the timer runs.
        let status = Status(unsafe {
            (events.members().add_timer)(
                events.raw(),
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

    fn run(&self) {
        let events = self.events();
        // SAFETY: the member takes its own loop.
        let status = Status(unsafe { (events.members().run)(events.raw(), 0) });
        assert_eq!(status, Status::GOOD);
    }

    /// Drives the manager and runs the loop until `span` passes, and gives the count
    /// of runs.
    async fn drive(&self, span: Span) -> usize {
        let runs = Cell::new(0);
        let mut end = self.clock.sleep(span);
        let mut drive = pin!(self.manager.drive(|_| {
            runs.set(runs.get() + 1);
            self.run();
            Poll::<Infallible>::Pending
        }));
        poll_fn(|cx| {
            if let Poll::Ready(never) = drive.as_mut().poll(cx) {
                match never {}
            }
            Pin::new(&mut end).poll(cx)
        })
        .await;
        runs.get()
    }
}

/// Sends `bytes` on connection `id` of `cm` in a buffer of the manager.
fn send_on(cm: *mut ffi::ConnectionManager, id: usize, bytes: &[u8]) -> Status {
    let mut buffer = Bytes {
        length: 0,
        data: ptr::null_mut(),
    };
    // SAFETY: the manager lives through the test.
    let members = unsafe { &*cm.cast::<Members>() };
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

/// Records a call as [`record`] does, and answers the `ESTABLISHED` that opens a
/// connection with a send on it.
unsafe extern "C" fn answer(
    cm: *mut ffi::ConnectionManager,
    id: usize,
    application: *mut c_void,
    context: *mut *mut c_void,
    state: ConnectionState,
    params: *const KeyValueMap,
    message: Bytes,
) {
    let opened = state == ffi::ESTABLISHED && message.length == 0;
    // SAFETY: the manager gives the arguments that it gives `record`.
    unsafe { record(cm, id, application, context, state, params, message) };
    if opened {
        assert_eq!(send_on(cm, id, b"hi"), Status::GOOD);
    }
}

/// What the peer read: each read with its time, and when and how the stream ended.
#[derive(Default)]
struct Reads {
    parts: Vec<(Monotonic, Vec<u8>)>,
    ended: Option<Monotonic>,
    /// The error that ended the stream, if one did.
    error: Option<String>,
}

impl Reads {
    fn bytes(&self) -> Vec<u8> {
        self.parts
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

    /// Accepts one stream on the peer, and runs `peer` on it with the clock of the
    /// peer.
    fn accept<P, F>(&self, peer: P)
    where
        P: FnOnce(Tcp, Clock) -> F + Send + 'static,
        F: Future<Output = ()> + 'static,
    {
        let listen = tcp::Listen {
            local: self.remote(),
            backlog: 4,
            options: OPTIONS,
        };
        let mut listener = self.peer.net().listen(&listen).expect("the port is free");
        let clock = self.peer.clock();
        let config = env::shards::Config {
            name: "peer".into(),
            core: None,
        };
        let handle = self.peer.shards().start(config, move |_| async move {
            let stream = poll_fn(|cx| listener.poll_accept(cx))
                .await
                .expect("a stream comes");
            peer(stream, clock).await;
        });
        drop(handle.expect("the shard starts"));
    }

    /// Accepts one stream on the peer, writes `answer` and closes when it is given,
    /// records what it reads until the stream ends, and closes.
    fn serve(&self, answer: Option<&'static [u8]>) -> Arc<Mutex<Reads>> {
        let reads = Arc::new(Mutex::new(Reads::default()));
        let slot = Arc::clone(&reads);
        self.accept(move |mut stream, clock| async move {
            if let Some(answer) = answer {
                write(&mut stream, answer).await;
                poll_fn(|cx| stream.poll_close(cx))
                    .await
                    .expect("the close works");
            }
            read_all(&mut stream, &clock, &slot, Span::ZERO).await;
            // A drop before the close resets the stream.
            drop(poll_fn(|cx| stream.poll_close(cx)).await);
        });
        reads
    }
}

/// Records what `stream` reads in `reads` until it ends, and waits `pause` after
/// each read.
async fn read_all(stream: &mut Tcp, clock: &Clock, reads: &Mutex<Reads>, pause: Span) {
    loop {
        let mut buffer = vec![0; 1 << 15];
        let read = poll_fn(|cx| stream.poll_read(cx, &mut buffer)).await;
        {
            let mut reads = reads.lock().expect("no panic under the lock");
            match read {
                Ok(0) => {
                    reads.ended = Some(clock.now());
                    return;
                }
                Err(e) => {
                    reads.ended = Some(clock.now());
                    reads.error = Some(e.to_string());
                    return;
                }
                Ok(n) => reads.parts.push((clock.now(), buffer[..n].to_vec())),
            }
        }
        if pause > Span::ZERO {
            clock.sleep(pause).await;
        }
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
            assert_eq!(side.connections(), 0);
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
    let sends = sends(3, 30_000);
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

/// Connects with no listener, when `child::running()`.
#[test]
fn refused() {
    if !child::running() {
        return;
    }
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
fn a_connect_with_no_listener_gives_closing_a_warning_and_no_established() {
    assert_eq!(
        stderr("refused"),
        "connector-opcua: open62541 warning: connection 1: the connect failed: \
         10.0.0.2:4840 refused the connection\n"
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
    assert!(reads.parts.is_empty());
    assert!(reads.ended.is_some());
}

#[test]
fn a_drop_drops_a_closing_that_waits() {
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
            let Side { manager, calls, .. } = side;
            drop(manager);
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
            side.add_timer(act, 10.0, ptr::from_ref(&later).cast_mut().cast());
            side.add_timer(nothing, 1000.0, ptr::null_mut());
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
    let [(at, bytes)] = &reads.parts[..] else {
        panic!("one read: {:?}", reads.parts.len());
    };
    assert_eq!(bytes, b"late");
    assert_eq!(*at, outcome.due + DELAY);
}

#[test]
fn a_close_from_a_run_of_the_loop_ends_the_stream_with_no_other_event() {
    let outcome = after_a_timer(Action::Close);
    let states: Vec<_> = outcome.calls.iter().map(|(_, state, _)| *state).collect();
    assert_eq!(states, [ffi::OPENING, ffi::ESTABLISHED, ffi::CLOSING]);
    let reads = outcome.reads.lock().expect("no panic under the lock");
    assert_eq!(reads.ended, Some(outcome.due + DELAY));
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
            let client = unsafe { ffi::shim_client_new(side.events().raw()) };
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

#[test]
fn a_drive_ends_with_the_first_value_of_run() {
    let mut network = Network::new();
    drop(network.serve(None));
    let remote = network.remote();
    network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            let start = side.clock.now();
            assert_eq!(side.connect(remote), Status::GOOD);
            let at = side
                .manager
                .drive(|_| {
                    side.run();
                    if side.calls().len() == 2 {
                        Poll::Ready(side.clock.now())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
            assert_eq!(at, start + DELAY + DELAY);
            assert_eq!(side.states(), [ffi::OPENING, ffi::ESTABLISHED]);
        })
        .expect("the run ends");
}

/// A send from the run that gives the value of a drive goes out before the drive
/// ends, with no drive after it for 1 s.
#[test]
fn a_send_from_the_run_that_ends_a_drive_goes_out() {
    let mut network = Network::new();
    let reads = network.serve(None);
    let remote = network.remote();
    let due = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::from_nanos(100_000_000)).await;
            let due = side.clock.now() + Span::from_nanos(10_000_000);
            let later = Later {
                side: &side,
                action: Action::Send,
                remote,
            };
            side.add_timer(act, 10.0, ptr::from_ref(&later).cast_mut().cast());
            side.manager
                .drive(|_| {
                    side.run();
                    if side.clock.now() >= due {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
            side.clock.sleep(Span::SECOND).await;
            assert_eq!(side.close(1), Status::GOOD);
            side.drive(Span::SECOND).await;
            due
        })
        .expect("the run ends");
    let reads = reads.lock().expect("no panic under the lock");
    let parts: Vec<_> = reads.parts.iter().map(|(at, b)| (*at, &b[..])).collect();
    assert_eq!(parts, [(due + DELAY, &b"late"[..])]);
}

/// `run` polls its own source with the context it gets, and the wake of that source
/// runs it again, with no timer of the loop.
#[test]
fn a_source_that_run_polls_wakes_the_drive() {
    let mut network = Network::new();
    network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            let start = side.clock.now();
            let mut source = side.clock.sleep(Span::SECOND);
            let at = side
                .manager
                .drive(|cx| {
                    side.run();
                    Pin::new(&mut source).poll(cx).map(|()| side.clock.now())
                })
                .await;
            assert_eq!(at, start + Span::SECOND);
        })
        .expect("the run ends");
}

/// A close from a task that does not drive the manager wakes the task that does.
#[test]
fn a_close_from_another_task_ends_the_stream_with_no_other_event() {
    let mut network = Network::new();
    let reads = network.serve(None);
    let remote = network.remote();
    let at = network
        .sim
        .run_on(&network.local.clone(), move |node, tasks| async move {
            let side = Rc::new(Side::new(&node));
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::from_nanos(100_000_000)).await;
            let at = side.clock.now() + Span::from_nanos(10_000_000);
            let other = Rc::clone(&side);
            tasks.spawn(async move {
                other.clock.sleep_until(at).await;
                assert_eq!(other.close(1), Status::GOOD);
            });
            side.drive(Span::SECOND).await;
            at
        })
        .expect("the run ends");
    let reads = reads.lock().expect("no panic under the lock");
    assert_eq!(reads.ended, Some(at + DELAY));
}

/// Opens two connections to the peer with `callback`, drives them for 1 s, and gives
/// the count of runs of the drive.
fn runs(callback: ffi::ConnectionCallback) -> usize {
    let mut network = Network::new();
    drop(network.serve(None));
    let remote = network.remote();
    network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side {
                callback,
                ..Side::new(&node)
            };
            assert_eq!(side.connect(remote), Status::GOOD);
            assert_eq!(side.connect(remote), Status::GOOD);
            let runs = side.drive(Span::SECOND).await;
            let states = [
                ffi::OPENING,
                ffi::OPENING,
                ffi::ESTABLISHED,
                ffi::ESTABLISHED,
            ];
            assert_eq!(side.states(), states);
            runs
        })
        .expect("the run ends")
}

/// A send from the callback of a connection goes out in the pass that calls it, so
/// it adds no pass. Connection 2 sends while the pass drives it with connection 1
/// behind it.
#[test]
fn a_send_from_the_callback_of_its_connection_adds_no_pass() {
    assert_eq!(runs(answer), runs(record));
}

/// Records a call as [`record`] does, and answers the `ESTABLISHED` that opens
/// connection 2 with a send on connection 1.
unsafe extern "C" fn answer_on_1(
    cm: *mut ffi::ConnectionManager,
    id: usize,
    application: *mut c_void,
    context: *mut *mut c_void,
    state: ConnectionState,
    params: *const KeyValueMap,
    message: Bytes,
) {
    let opened = id == 2 && state == ffi::ESTABLISHED && message.length == 0;
    // SAFETY: the manager gives the arguments that it gives `record`.
    unsafe { record(cm, id, application, context, state, params, message) };
    if opened {
        assert_eq!(send_on(cm, 1, b"hi"), Status::GOOD);
    }
}

/// A send from a callback on a connection that the pass has gone past goes out in
/// another pass, with no other event.
#[test]
fn a_send_from_a_callback_on_a_connection_behind_the_pass_goes_out() {
    let mut network = Network::new();
    let reads = network.serve(None);
    let remote = network.remote();
    let start = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side {
                callback: answer_on_1,
                ..Side::new(&node)
            };
            let start = side.clock.now();
            assert_eq!(side.connect(remote), Status::GOOD);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::SECOND).await;
            start
        })
        .expect("the run ends");
    let reads = reads.lock().expect("no panic under the lock");
    let parts: Vec<_> = reads.parts.iter().map(|(at, b)| (*at, &b[..])).collect();
    // The connect takes a round trip, and the send one more link.
    assert_eq!(parts, [(start + DELAY + DELAY + DELAY, &b"hi"[..])]);
}

/// A `CLOSING` of a connection with no stream wakes the drive, which drops the
/// connection with no other event.
#[test]
fn a_closing_with_no_stream_drops_the_connection_at_once() {
    let mut network = Network::new();
    drop(network.serve(None));
    let remote = network.remote();
    let error = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            let start = side.clock.now();
            assert_eq!(side.connect(remote), Status::GOOD);
            assert_eq!(side.close(1), Status::GOOD);
            let mut end = side.clock.sleep(Span::SECOND);
            let mut drive = pin!(side.manager.drive(|_| {
                side.run();
                if side.connections() == 0 {
                    Poll::Ready(side.clock.now())
                } else {
                    Poll::Pending
                }
            }));
            let at = poll_fn(|cx| {
                if Pin::new(&mut end).poll(cx).is_ready() {
                    return Poll::Ready(None);
                }
                drive.as_mut().poll(cx).map(Some)
            })
            .await;
            assert_eq!(at, Some(start));
            assert_eq!(side.states(), [ffi::OPENING, ffi::CLOSING]);
        })
        .expect_err("the peer gets no stream");
    let threads = vec!["peer".to_owned()];
    assert_eq!(error, sim::Error::Stuck { threads, seed: 0 });
}

unsafe extern "C" fn send_late(_: *mut c_void, data: *mut c_void) {
    // SAFETY: the test keeps the side live through the run.
    let side = unsafe { &*data.cast::<Side>() };
    assert_eq!(side.send(1, b"late"), Status::GOOD);
}

/// The drive sleeps on a timer due in 1 s when another task adds one due sooner and
/// wakes it with a send.
#[test]
fn a_timer_added_while_the_drive_sleeps_runs_when_it_is_due() {
    let mut network = Network::new();
    let reads = network.serve(None);
    let remote = network.remote();
    let (at, due) = network
        .sim
        .run_on(&network.local.clone(), move |node, tasks| async move {
            let side = Rc::new(Side::new(&node));
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::from_nanos(100_000_000)).await;
            side.add_timer(nothing, 1000.0, ptr::null_mut());
            let at = side.clock.now() + Span::from_nanos(50_000_000);
            let other = Rc::clone(&side);
            tasks.spawn(async move {
                other.clock.sleep_until(at).await;
                let data = Rc::as_ptr(&other).cast_mut().cast();
                other.add_timer(send_late, 10.0, data);
                assert_eq!(other.send(1, b"wake"), Status::GOOD);
            });
            side.drive(Span::SECOND).await;
            assert_eq!(side.close(1), Status::GOOD);
            side.drive(Span::SECOND).await;
            (at, at + Span::from_nanos(10_000_000))
        })
        .expect("the run ends");
    let reads = reads.lock().expect("no panic under the lock");
    let parts: Vec<_> = reads.parts.iter().map(|(at, b)| (*at, &b[..])).collect();
    assert_eq!(
        parts,
        [(at + DELAY, &b"wake"[..]), (due + DELAY, &b"late"[..])]
    );
}

#[test]
fn a_close_during_the_connect_gives_closing_and_no_stream() {
    let mut network = Network::new();
    drop(network.serve(None));
    let remote = network.remote();
    let error = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            assert_eq!(side.close(1), Status::GOOD);
            side.drive(Span::SECOND).await;
            assert_eq!(side.connections(), 0);
            assert_eq!(side.states(), [ffi::OPENING, ffi::CLOSING]);
        })
        .expect_err("the peer gets no stream");
    let threads = vec!["peer".to_owned()];
    assert_eq!(error, sim::Error::Stuck { threads, seed: 0 });
}

#[test]
fn a_send_during_the_connect_is_refused() {
    let mut network = Network::new();
    let reads = network.serve(None);
    let remote = network.remote();
    network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            assert_eq!(side.send(1, b"early"), Status::BAD_CONNECTION_CLOSED);
            side.drive(Span::SECOND).await;
            assert_eq!(side.close(1), Status::GOOD);
            side.drive(Span::SECOND).await;
        })
        .expect("the run ends");
    let reads = reads.lock().expect("no panic under the lock");
    assert_eq!(reads.bytes(), b"");
    assert!(reads.ended.is_some());
}

/// The stream takes one send of 200,000 bytes in at least 4 writes, as its send
/// buffer holds 64 KiB.
#[test]
fn the_peer_reads_each_byte_of_one_send_larger_than_the_send_buffer() {
    let mut network = Network::new();
    let reads = network.serve(None);
    let remote = network.remote();
    let send = sends(1, 200_000).remove(0);
    let written = send.clone();
    network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::SECOND).await;
            assert_eq!(side.send(1, &written), Status::GOOD);
            assert_eq!(side.close(1), Status::GOOD);
            side.drive(Span::SECOND).await;
        })
        .expect("the run ends");
    let reads = reads.lock().expect("no panic under the lock");
    assert_eq!(reads.bytes(), send);
    assert_eq!(reads.error, None);
}

/// The peer answers its first read with more bytes than one read takes, then waits
/// 100 ms after each read. The close reads and drops the answer, so the stream ends
/// with no reset after each send.
#[test]
fn a_close_writes_each_send_to_a_peer_that_wrote_bytes_it_did_not_read() {
    let mut network = Network::new();
    let reads = Arc::new(Mutex::new(Reads::default()));
    let slot = Arc::clone(&reads);
    network.accept(move |mut stream, clock| async move {
        let mut buffer = [0; 512];
        let n = poll_fn(|cx| stream.poll_read(cx, &mut buffer))
            .await
            .expect("the read works");
        let first = (clock.now(), buffer[..n].to_vec());
        slot.lock()
            .expect("no panic under the lock")
            .parts
            .push(first);
        write(&mut stream, &vec![7; 3 * READ_BYTES]).await;
        read_all(&mut stream, &clock, &slot, Span::from_nanos(100_000_000)).await;
    });
    let remote = network.remote();
    let sends = sends(5, 30_000);
    let written = sends.clone();
    let states = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::SECOND).await;
            for send in &written {
                assert_eq!(side.send(1, send), Status::GOOD);
            }
            assert_eq!(side.close(1), Status::GOOD);
            side.drive(LINGER).await;
            assert_eq!(side.connections(), 0);
            side.states()
        })
        .expect("the run ends");
    assert_eq!(states, [ffi::OPENING, ffi::ESTABLISHED, ffi::CLOSING]);
    let reads = reads.lock().expect("no panic under the lock");
    assert_eq!(reads.error, None);
    assert_eq!(reads.bytes(), sends.concat());
}

/// The peer writes 100 bytes and reads up to 2000 each 10 ms, and closes once it
/// reads the end. A close keeps the stream until then, so no byte of the peer meets a
/// dropped stream and resets it.
#[test]
fn a_close_writes_each_send_to_a_peer_that_writes_as_it_reads() {
    let mut network = Network::new();
    let reads = Arc::new(Mutex::new(Reads::default()));
    let slot = Arc::clone(&reads);
    network.accept(move |mut stream, clock| async move {
        let mut buffer = [0; 2000];
        let chunk = [7; 100];
        loop {
            clock.sleep(Span::from_nanos(10_000_000)).await;
            let parts = [IoSlice::new(&chunk)];
            drop(poll_fn(|cx| Poll::Ready(stream.poll_write(cx, &parts))).await);
            let read =
                poll_fn(|cx| Poll::Ready(stream.poll_read(cx, &mut buffer))).await;
            if let Poll::Ready(Ok(0)) = read {
                slot.lock().expect("no panic under the lock").ended = Some(clock.now());
                drop(poll_fn(|cx| stream.poll_close(cx)).await);
                return;
            }
            let mut reads = slot.lock().expect("no panic under the lock");
            match read {
                Poll::Pending | Poll::Ready(Ok(0)) => {}
                Poll::Ready(Ok(n)) => {
                    reads.parts.push((clock.now(), buffer[..n].to_vec()));
                }
                Poll::Ready(Err(e)) => {
                    reads.ended = Some(clock.now());
                    reads.error = Some(e.to_string());
                    return;
                }
            }
        }
    });
    let remote = network.remote();
    let sends = sends(5, 30_000);
    let written = sends.clone();
    network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::from_nanos(5_000_000)).await;
            for send in &written {
                assert_eq!(side.send(1, send), Status::GOOD);
            }
            assert_eq!(side.close(1), Status::GOOD);
            side.drive(LINGER).await;
            assert_eq!(side.connections(), 0);
        })
        .expect("the run ends");
    let reads = reads.lock().expect("no panic under the lock");
    assert_eq!(reads.error, None);
    assert_eq!(reads.bytes(), sends.concat());
}

/// The peer reads nothing until 12 s, and the stream closes at 1 s and 6 s, when
/// `child::running()`.
#[test]
fn lingers() {
    if !child::running() {
        return;
    }
    let mut network = Network::new();
    let reads = Arc::new(Mutex::new(Reads::default()));
    let slot = Arc::clone(&reads);
    network.accept(move |mut stream, clock| async move {
        clock.sleep(Span::from_nanos(12_000_000_000)).await;
        read_all(&mut stream, &clock, &slot, Span::ZERO).await;
    });
    let remote = network.remote();
    let states = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::SECOND).await;
            for send in sends(10, 30_000) {
                assert_eq!(side.send(1, &send), Status::GOOD);
            }
            assert_eq!(side.close(1), Status::GOOD);
            side.clock.sleep(Span::from_nanos(5_000_000_000)).await;
            assert_eq!(side.close(1), Status::GOOD);
            side.drive(Span::from_nanos(4_999_000_000)).await;
            assert_eq!(side.connections(), 1);
            side.drive(Span::from_nanos(1_000_000)).await;
            assert_eq!(side.connections(), 0);
            side.clock.sleep(Span::from_nanos(2_000_000_000)).await;
            side.states()
        })
        .expect("the run ends");
    assert_eq!(states, [ffi::OPENING, ffi::ESTABLISHED, ffi::CLOSING]);
    let reads = reads.lock().expect("no panic under the lock");
    let error = reads.error.as_deref();
    assert_eq!(
        (reads.bytes().len(), error),
        (
            OPTIONS.recv_buffer_bytes,
            Some("10.0.0.1:49152 reset the stream")
        )
    );
}

/// A close drops a stream whose peer reads nothing [`LINGER`] after the first close,
/// not a later one, with a warning.
#[test]
fn a_close_drops_a_stream_that_takes_no_bytes_after_it_lingers() {
    assert_eq!(
        stderr("lingers"),
        "connector-opcua: open62541 warning: connection 1: the close took 10s, so it \
         drops the stream\n"
    );
}

#[test]
#[should_panic(expected = "one drive of a manager at a time")]
fn a_second_drive_of_a_manager_panics() {
    let mut network = Network::new();
    network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            let mut first = pin!(side.manager.drive(|_| Poll::<Infallible>::Pending));
            let mut second = pin!(side.manager.drive(|_| Poll::<Infallible>::Pending));
            poll_fn(|cx| {
                assert!(first.as_mut().poll(cx).is_pending());
                assert!(second.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
        })
        .expect("the run ends");
}

/// Runs the test `name` of this module in a child process, asserts that it passes,
/// and gives what it wrote to stderr.
fn stderr(name: &str) -> String {
    let output = child::output(&format!("connection::tests::{name}"), &[]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed"),
        "{stdout}"
    );
    String::from_utf8(output.stderr).unwrap()
}

/// The peer resets the stream as it accepts it, when `child::running()`.
#[test]
fn reset_as_open() {
    if !child::running() {
        return;
    }
    let mut network = Network::new();
    network.accept(|stream, _| async move { drop(stream) });
    let remote = network.remote();
    let states = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::SECOND).await;
            assert_eq!(side.connections(), 0);
            assert_eq!(side.close(1), Status::BAD_NOT_FOUND);
            side.states()
        })
        .expect("the run ends");
    assert_eq!(states, [ffi::OPENING, ffi::ESTABLISHED, ffi::CLOSING]);
}

#[test]
fn a_reset_gives_closing_and_a_warning() {
    assert_eq!(
        stderr("reset_as_open"),
        "connector-opcua: open62541 warning: connection 1: the read failed: \
         10.0.0.2:4840 reset the stream\n"
    );
}

/// The peer resets the stream 100 ms after it accepts it, and reads nothing, when
/// `child::running()`.
#[test]
fn reset_as_closing() {
    if !child::running() {
        return;
    }
    let mut network = Network::new();
    network.accept(|stream, clock| async move {
        clock.sleep(Span::from_nanos(100_000_000)).await;
        drop(stream);
    });
    let remote = network.remote();
    let states = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::from_nanos(10_000_000)).await;
            for send in sends(10, 30_000) {
                assert_eq!(side.send(1, &send), Status::GOOD);
            }
            assert_eq!(side.close(1), Status::GOOD);
            side.drive(Span::SECOND).await;
            assert_eq!(side.connections(), 0);
            side.states()
        })
        .expect("the run ends");
    assert_eq!(states, [ffi::OPENING, ffi::ESTABLISHED, ffi::CLOSING]);
}

#[test]
fn a_reset_of_a_closed_connection_drops_it_before_it_lingers() {
    assert_eq!(
        stderr("reset_as_closing"),
        "connector-opcua: open62541 warning: connection 1: the read failed: \
         10.0.0.2:4840 reset the stream\n"
    );
}

/// Sends one more than [`SENDS`] with no poll between, when `child::running()`.
#[test]
fn send_past_the_bound() {
    if !child::running() {
        return;
    }
    let mut network = Network::new();
    let reads = network.serve(None);
    let remote = network.remote();
    let sends = sends(SENDS, 100);
    let written = sends.clone();
    let states = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::SECOND).await;
            for send in &written {
                assert_eq!(side.send(1, send), Status::GOOD);
            }
            assert_eq!(side.send(1, b"past"), Status::BAD_CONNECTION_CLOSED);
            side.drive(Span::SECOND).await;
            assert_eq!(side.connections(), 0);
            side.states()
        })
        .expect("the run ends");
    assert_eq!(states, [ffi::OPENING, ffi::ESTABLISHED, ffi::CLOSING]);
    let reads = reads.lock().expect("no panic under the lock");
    assert_eq!(reads.bytes(), sends.concat());
    assert_eq!(reads.error, None);
}

#[test]
fn a_send_past_the_bound_closes_the_connection_with_a_warning() {
    assert_eq!(
        stderr("send_past_the_bound"),
        "connector-opcua: open62541 warning: connection 1: 256 sends wait, so it \
         closes\n"
    );
}
