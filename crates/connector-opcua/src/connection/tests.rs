use std::cell::{Cell, RefCell};
use std::convert::Infallible;
use std::ffi::{CStr, CString, c_char, c_void};
use std::future::poll_fn;
use std::io::IoSlice;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::panic::{self, AssertUnwindSafe};
use std::pin::{Pin, pin};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use env::clock::Clock;
use env::net::{self, Tcp, tcp};
use env::rng::Rng;
use sim::{Sim, node};
use types::time::{Monotonic, Span};

use super::{LINGER, Manager, OPTIONS, READ_BYTES, SENDS};
use crate::child;
use crate::event::Loop;
use crate::ffi::test::{
    Members, NUMERIC, NodeId, QualifiedName, UA_Client_disconnect,
    UA_KeyValueMap_clear, UA_KeyValueMap_getScalar, UA_KeyValueMap_setScalar,
    UA_findDataType, shim_map_set_strings,
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
#[derive(Clone, Copy)]
enum Value<'a> {
    Boolean(bool),
    UInt16(u16),
    String(&'a str),
    /// An array of strings.
    Strings(&'a [&'a CStr]),
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
            Value::Strings(v) => {
                let key = CString::new(*key).expect("no NUL");
                let strings: Vec<*const c_char> =
                    v.iter().map(|v| v.as_ptr()).collect();
                // SAFETY: the map copies the key and the strings.
                let status = Status(unsafe {
                    shim_map_set_strings(
                        &raw mut map,
                        key.as_ptr(),
                        strings.as_ptr(),
                        v.len(),
                    )
                });
                assert_eq!(status, Status::GOOD);
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
    // SAFETY: the map copies the key and the value, each of the type `kind`.
    let status = Status(unsafe {
        UA_KeyValueMap_setScalar(map, name(key), value, builtin(kind))
    });
    assert_eq!(status, Status::GOOD);
}

/// Gives the value of `key` in `map` when it has the type `kind`, or null.
fn get(map: *const KeyValueMap, key: &str, kind: u32) -> *const c_void {
    // SAFETY: the caller gives a map that C allocated.
    unsafe { UA_KeyValueMap_getScalar(map, name(key), builtin(kind)) }
}

/// Gives the builtin type of node `kind`.
fn builtin(kind: u32) -> *const c_void {
    let id = NodeId {
        namespace: 0,
        kind: NUMERIC,
        numeric: kind,
        rest: [0; 3],
    };
    // SAFETY: the id is a numeric node of namespace 0.
    let kind = unsafe { UA_findDataType(&raw const id) };
    assert!(!kind.is_null(), "a builtin type");
    kind
}

/// Gives the key `key` of namespace 0, which borrows `key`.
fn name(key: &str) -> QualifiedName {
    QualifiedName {
        namespace: 0,
        name: text(key),
    }
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
        Self::of(clock, manager)
    }

    /// A side whose manager accepts on a listener at `local`.
    fn listening(node: &node::Node, local: SocketAddr) -> Self {
        let clock = node.clock();
        let rng = &mut Rng::from_seed(0);
        let manager =
            Manager::listening(Clock::clone(&clock), node.net(), local, 4, rng)
                .expect("the port is free");
        Self::of(clock, manager)
    }

    fn of(clock: Clock, manager: Manager) -> Self {
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

    /// Opens a listen connection on `port` with the callback of the side.
    fn listen(&self, port: u16) -> Status {
        self.open(&[
            ("listen", Value::Boolean(true)),
            ("port", Value::UInt16(port)),
        ])
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

    /// The length of the read buffer of each connection in the table. No call of
    /// open62541 shows it, and `tests/memory.rs` cannot reach the private manager.
    fn buffers(&self) -> Vec<(usize, usize)> {
        let table = self.manager.state().table.borrow();
        table.iter().map(|(id, c)| (*id, c.buffer.len())).collect()
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

    /// Drives the manager and runs the loop until `span` passes, and gives the count
    /// of runs.
    async fn drive(&self, span: Span) -> usize {
        let runs = Cell::new(0);
        let mut end = self.clock.sleep(span);
        let mut drive = pin!(self.manager.drive(|_| {
            runs.set(runs.get() + 1);
            self.events().run();
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

    /// The address that a listening side binds.
    fn listening(&self) -> SocketAddr {
        SocketAddr::new(self.local.addresses()[0], PORT)
    }

    /// Connects from the peer to [`Self::listening`] after `delay`, writes `say`,
    /// records what it reads until the stream ends, and closes. A connect that fails
    /// records its error.
    fn dial(&self, delay: Span, say: &[u8]) -> Arc<Mutex<Reads>> {
        let say = say.to_vec();
        let reads = Arc::new(Mutex::new(Reads::default()));
        let slot = Arc::clone(&reads);
        let net = self.peer.net();
        let clock = self.peer.clock();
        let config = tcp::Config {
            remote: self.listening(),
            options: OPTIONS,
        };
        let shard = env::shards::Config {
            name: "dial".into(),
            core: None,
        };
        let handle = self.peer.shards().start(shard, move |_| async move {
            clock.sleep(delay).await;
            let mut stream = match net.connect(&config).await {
                Ok(stream) => stream,
                Err(e) => {
                    let mut reads = slot.lock().expect("no panic under the lock");
                    reads.ended = Some(clock.now());
                    reads.error = Some(e.to_string());
                    return;
                }
            };
            write(&mut stream, &say).await;
            read_all(&mut stream, &clock, &slot, Span::ZERO).await;
            drop(poll_fn(|cx| stream.poll_close(cx)).await);
        });
        drop(handle.expect("the shard starts"));
        reads
    }

    /// Connects from the peer to [`Self::listening`], and runs [`hold`] on the
    /// stream.
    fn dial_and_hold(&self, delay: Span, bytes: usize) -> Arc<AtomicUsize> {
        let taken = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&taken);
        let net = self.peer.net();
        let clock = self.peer.clock();
        let config = tcp::Config {
            remote: self.listening(),
            options: OPTIONS,
        };
        let shard = env::shards::Config {
            name: "hold".into(),
            core: None,
        };
        let handle = self.peer.shards().start(shard, move |_| async move {
            let stream = net.connect(&config).await.expect("the connect works");
            hold(stream, clock, delay, bytes, count).await;
        });
        drop(handle.expect("the shard starts"));
        taken
    }

    /// Accepts one stream on the peer, and runs [`hold`] on it.
    fn accept_and_hold(&self, delay: Span, bytes: usize) -> Arc<AtomicUsize> {
        let taken = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&taken);
        self.accept(move |stream, clock| hold(stream, clock, delay, bytes, count));
        taken
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

/// Holds `stream` with no read. After `delay`, it writes up to `bytes` for 1 s, and
/// puts in `taken` what the stream takes.
async fn hold(
    mut stream: Tcp,
    clock: Clock,
    delay: Span,
    bytes: usize,
    taken: Arc<AtomicUsize>,
) {
    clock.sleep(delay).await;
    let say = vec![7; bytes];
    let mut sent = 0;
    let mut end = pin!(clock.sleep(Span::SECOND));
    while sent < bytes {
        let parts = [IoSlice::new(&say[sent..])];
        let write = poll_fn(|cx| match end.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(None),
            Poll::Pending => stream.poll_write(cx, &parts).map(Some),
        });
        let Some(n) = write.await else { break };
        sent += n.expect("the write works");
        taken.store(sent, Ordering::Relaxed);
    }
    clock.sleep(Span::from_nanos(1_000_000_000_000)).await;
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
fn an_open_without_its_parameters_is_refused_with_no_call() {
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
                    ("port", Value::String("4840")),
                ]),
            ];
            assert_eq!(statuses, [Status::BAD_INVALID_ARGUMENT; 4]);
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
                    side.events().run();
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

/// Records the call, and on a read, closes the connection and runs the loop, as a
/// synchronous disconnect from a response callback does.
unsafe extern "C" fn close_and_run(
    cm: *mut ffi::ConnectionManager,
    id: usize,
    application: *mut c_void,
    context: *mut *mut c_void,
    state: ConnectionState,
    params: *const KeyValueMap,
    message: Bytes,
) {
    let read = state == ffi::ESTABLISHED && message.length > 0;
    // SAFETY: the manager gives the arguments that it gives `record`.
    unsafe { record(cm, id, application, context, state, params, message) };
    if read {
        // SAFETY: the manager lives through the test.
        let members = unsafe { &*cm.cast::<Members>() };
        // SAFETY: the member takes its own manager.
        assert_eq!(Status(unsafe { (members.close)(cm, id) }), Status::GOOD);
        let el = members.event_loop;
        // SAFETY: the loop of the manager lives through the test.
        let run = unsafe { (*el).run };
        // SAFETY: the loop runs on this thread, outside a run of its own.
        assert_eq!(Status(unsafe { run(el, 0) }), Status::GOOD);
    }
}

/// A run of the loop inside a read callback gives the `CLOSING` of a close from that
/// callback.
#[test]
fn a_run_of_the_loop_from_a_read_callback_gives_closing() {
    let mut network = Network::new();
    drop(network.serve(Some(b"ack")));
    let remote = network.remote();
    let calls = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side {
                callback: close_and_run,
                ..Side::new(&node)
            };
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::SECOND).await;
            side.states()
        })
        .expect("the run ends");
    let states = [
        ffi::OPENING,
        ffi::ESTABLISHED,
        ffi::ESTABLISHED,
        ffi::CLOSING,
    ];
    assert_eq!(calls, states);
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
                    side.events().run();
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

/// Sends from one `run` on one connection ask for one move of it. The test counts the
/// moves, since a second move of a waiting connection shows nothing to a caller or a
/// peer.
#[cfg(feature = "sim")]
#[test]
fn sends_from_one_run_on_one_connection_ask_for_one_move() {
    let mut network = Network::new();
    let reads = network.serve(None);
    let remote = network.remote();
    let (moves, sent) = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::from_nanos(100_000_000)).await;
            let state = side.manager.state();
            let before = state.moves.get();
            side.manager
                .drive(|_| {
                    for bytes in [&b"a"[..], b"b", b"c"] {
                        assert_eq!(side.send(1, bytes), Status::GOOD);
                    }
                    Poll::Ready(())
                })
                .await;
            let moves = state.moves.get() - before;
            let sent = side.clock.now();
            // A move of the wrong connection leaves the bytes to the next drive.
            side.clock.sleep(Span::from_nanos(1_000_000)).await;
            side.drive(Span::SECOND).await;
            (moves, sent)
        })
        .expect("the run ends");
    // One in the pass and one after `run`.
    assert_eq!(moves, 2);
    let reads = reads.lock().expect("no panic under the lock");
    let parts: Vec<_> = reads.parts.iter().map(|(at, b)| (*at, &b[..])).collect();
    assert_eq!(parts, [(sent + DELAY, &b"abc"[..])]);
}

/// A `run` that sends and gives no value makes the drive move on only that
/// connection before it calls `run` again, with no second pass. The test counts the
/// moves, since a step of a waiting connection shows nothing to a caller or a peer.
#[cfg(feature = "sim")]
#[test]
fn a_send_from_a_run_with_no_value_moves_on_only_its_connection() {
    let mut network = Network::new();
    let reads = network.serve(None);
    let remote = network.remote();
    let (moves, runs, sent) = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::from_nanos(100_000_000)).await;
            assert_eq!(side.connections(), 2);
            let state = side.manager.state();
            let before = state.moves.get();
            let mut runs = 0;
            side.manager
                .drive(|_| {
                    runs += 1;
                    if runs == 1 {
                        assert_eq!(side.send(1, b"a"), Status::GOOD);
                        Poll::Pending
                    } else {
                        Poll::Ready(())
                    }
                })
                .await;
            let moves = state.moves.get() - before;
            let sent = side.clock.now();
            // A move of the wrong connection leaves the bytes to the next drive.
            side.clock.sleep(Span::from_nanos(1_000_000)).await;
            side.drive(Span::SECOND).await;
            (moves, runs, sent)
        })
        .expect("the run ends");
    // Two in the pass and one after the first `run`.
    assert_eq!((moves, runs), (3, 2));
    let reads = reads.lock().expect("no panic under the lock");
    let parts: Vec<_> = reads.parts.iter().map(|(at, b)| (*at, &b[..])).collect();
    assert_eq!(parts, [(sent + DELAY, &b"a"[..])]);
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
                    side.events().run();
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
                side.events().run();
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
fn a_second_close_during_the_connect_gives_one_closing() {
    let mut network = Network::new();
    drop(network.serve(None));
    let remote = network.remote();
    let error = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            assert_eq!(side.close(1), Status::GOOD);
            assert_eq!(side.close(1), Status::GOOD);
            side.drive(Span::SECOND).await;
            assert_eq!(side.connections(), 0);
            assert_eq!(side.states(), [ffi::OPENING, ffi::CLOSING]);
        })
        .expect_err("the peer gets no stream");
    let threads = vec!["peer".to_owned()];
    assert_eq!(error, sim::Error::Stuck { threads, seed: 0 });
}

/// A close drops the connection once the peer closes its side, not at a later wake.
#[test]
fn a_close_drops_the_connection_when_the_peer_closes_its_side() {
    let mut network = Network::new();
    let reads = network.serve(None);
    let remote = network.remote();
    let (closed, dropped) = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::SECOND).await;
            let closed = side.clock.now();
            assert_eq!(side.close(1), Status::GOOD);
            let mut end = side.clock.sleep(Span::SECOND);
            let dropped = side
                .manager
                .drive(|cx| {
                    side.events().run();
                    if side.connections() == 0 {
                        Poll::Ready(Some(side.clock.now()))
                    } else {
                        Pin::new(&mut end).poll(cx).map(|()| None)
                    }
                })
                .await;
            (closed, dropped)
        })
        .expect("the run ends");
    assert_eq!(dropped, Some(closed + DELAY + DELAY));
    let reads = reads.lock().expect("no panic under the lock");
    assert_eq!(reads.ended, Some(closed + DELAY));
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

/// Counts the wakes of a task.
#[derive(Default)]
struct Wakes(AtomicUsize);

impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

/// A drive whose `run` panics ends as a dropped drive does, so a send after it wakes
/// the task of the drive.
#[test]
fn a_send_after_a_drive_that_panics_wakes_its_task() {
    let mut network = Network::new();
    network.serve(None);
    let remote = network.remote();
    let woken = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::new(&node);
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::SECOND).await;
            let count = Arc::new(Wakes::default());
            let waker = Waker::from(Arc::clone(&count));
            let mut drive = pin!(
                side.manager
                    .drive(|_| -> Poll<()> { panic!("the run panics") })
            );
            let poll = panic::catch_unwind(AssertUnwindSafe(|| {
                drive.as_mut().poll(&mut Context::from_waker(&waker))
            }));
            let panic = poll.expect_err("the drive panics");
            assert_eq!(panic.downcast_ref::<&str>(), Some(&"the run panics"));
            assert_eq!(side.send(1, b"one"), Status::GOOD);
            count.0.load(Ordering::Relaxed)
        })
        .expect("the run ends");
    assert_eq!(woken, 1);
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

/// The peer closes its side at 50 ms and drops the stream at 150 ms, and reads
/// nothing, while the stream closes with sends that wait, when `child::running()`.
#[test]
fn reset_as_writing() {
    if !child::running() {
        return;
    }
    let mut network = Network::new();
    network.accept(|mut stream, clock| async move {
        clock.sleep(Span::from_nanos(50_000_000)).await;
        poll_fn(|cx| stream.poll_close(cx))
            .await
            .expect("the close works");
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
fn a_write_to_a_reset_stream_gives_closing_and_a_warning() {
    assert_eq!(
        stderr("reset_as_writing"),
        "connector-opcua: open62541 warning: connection 1: the write failed: \
         10.0.0.2:4840 reset the stream\n"
    );
}

/// Records the call, with a `CLOSING` as one byte: 1 when the context is the
/// application. On a read, sets the context to the application and closes. Gives
/// whether it closed.
///
/// # Safety
///
/// The arguments are those that the manager of a live side gives a callback, on the
/// thread of its loop.
unsafe fn mark(
    cm: *mut ffi::ConnectionManager,
    id: usize,
    application: *mut c_void,
    context: *mut *mut c_void,
    state: ConnectionState,
    params: *const KeyValueMap,
    message: Bytes,
) -> bool {
    if state == ffi::CLOSING {
        // SAFETY: `open` passes the calls of a live side.
        let calls = unsafe { &*application.cast::<RefCell<Vec<Call>>>() };
        // SAFETY: the manager gives a live context slot.
        let marked = unsafe { *context } == application;
        calls.borrow_mut().push((id, state, vec![u8::from(marked)]));
        return false;
    }
    let read = state == ffi::ESTABLISHED && message.length > 0;
    // SAFETY: the manager gives the arguments that it gives `record`.
    unsafe { record(cm, id, application, context, state, params, message) };
    if read {
        // SAFETY: the manager gives a live context slot.
        unsafe { *context = application };
        // SAFETY: the manager lives through the test.
        let members = unsafe { &*cm.cast::<Members>() };
        // SAFETY: the member takes its own manager.
        assert_eq!(Status(unsafe { (members.close)(cm, id) }), Status::GOOD);
    }
    read
}

unsafe extern "C" fn mark_close(
    cm: *mut ffi::ConnectionManager,
    id: usize,
    application: *mut c_void,
    context: *mut *mut c_void,
    state: ConnectionState,
    params: *const KeyValueMap,
    message: Bytes,
) {
    // SAFETY: the manager gives the arguments.
    unsafe { mark(cm, id, application, context, state, params, message) };
}

unsafe extern "C" fn mark_close_and_run(
    cm: *mut ffi::ConnectionManager,
    id: usize,
    application: *mut c_void,
    context: *mut *mut c_void,
    state: ConnectionState,
    params: *const KeyValueMap,
    message: Bytes,
) {
    // SAFETY: the manager gives the arguments.
    if unsafe { mark(cm, id, application, context, state, params, message) } {
        // SAFETY: the manager lives through the test.
        let el = unsafe { &*cm.cast::<Members>() }.event_loop;
        // SAFETY: the loop of the manager lives through the test.
        let run = unsafe { (*el).run };
        // SAFETY: the loop runs on this thread, outside a run of its own.
        assert_eq!(Status(unsafe { run(el, 0) }), Status::GOOD);
    }
}

fn closing_context(callback: ffi::ConnectionCallback) -> Vec<Call> {
    let mut network = Network::new();
    drop(network.serve(Some(b"ack")));
    let remote = network.remote();
    network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side {
                callback,
                ..Side::new(&node)
            };
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::SECOND).await;
            side.calls()
        })
        .expect("the run ends")
}

/// The `CLOSING` gets the context that the read callback wrote, also when that
/// callback runs the loop that gives the `CLOSING`.
#[test]
fn a_closing_from_a_run_in_a_callback_gets_the_context_it_wrote() {
    let closing = (1, ffi::CLOSING, vec![1]);
    let late = closing_context(mark_close);
    assert_eq!(late.last(), Some(&closing), "a close with no run");
    let nested = closing_context(mark_close_and_run);
    assert_eq!(nested.last(), Some(&closing), "a close and a run");
}

/// Gives the address of a listener of `node` at [`Network::listening`].
fn local(node: &node::Node) -> SocketAddr {
    SocketAddr::new(node.addresses()[0], PORT)
}

/// Gives a side on `node` whose manager accepts at [`local`], with the callback
/// [`adopt`].
fn listening(node: &node::Node) -> Side {
    let mut side = Side::listening(node, local(node));
    side.callback = adopt;
    side
}

/// Gives the bytes of `string`, a `UA_String` that C holds.
fn string(string: *const Bytes) -> Vec<u8> {
    // SAFETY: the caller gives a live string.
    let string = unsafe { &*string };
    if string.length == 0 {
        return Vec::new();
    }
    // SAFETY: a string holds `length` bytes at `data`.
    unsafe { std::slice::from_raw_parts(string.data, string.length) }.to_vec()
}

/// Records a call as [`record`] does, with the parameters of the call as the
/// message: `key=value` for each of `listen-address`, `listen-port`, and
/// `remote-address` that the call has, joined with a space.
unsafe extern "C" fn note(
    _: *mut ffi::ConnectionManager,
    id: usize,
    application: *mut c_void,
    _: *mut *mut c_void,
    state: ConnectionState,
    params: *const KeyValueMap,
    _: Bytes,
) {
    // SAFETY: `open` passes the calls of a live side.
    let calls = unsafe { &*application.cast::<RefCell<Vec<Call>>>() };
    let mut notes = Vec::new();
    for key in ["listen-address", "remote-address"] {
        let value = get(params, key, STRING);
        if !value.is_null() {
            notes.push([key.as_bytes(), b"=", &string(value.cast())].concat());
        }
    }
    let port = get(params, "listen-port", UINT16);
    if !port.is_null() {
        // SAFETY: the value has the type `UInt16`.
        let port = unsafe { *port.cast::<u16>() };
        notes.push(format!("listen-port={port}").into_bytes());
    }
    calls.borrow_mut().push((id, state, notes.join(&b' ')));
}

/// Gives the calls of [`note`] on a side that listens at `local` with the extra
/// params `params` and accepts one stream from the peer.
fn notes(
    local: IpAddr,
    params: &'static [(&'static str, Value<'static>)],
) -> Vec<(usize, ConnectionState, String)> {
    let mut network = Network::new();
    network.dial(Span::MILLISECOND, b"");
    network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let mut side = Side::listening(&node, SocketAddr::new(local, PORT));
            side.callback = note;
            let mut all = vec![
                ("listen", Value::Boolean(true)),
                ("port", Value::UInt16(PORT)),
            ];
            all.extend_from_slice(params);
            assert_eq!(side.open(&all), Status::GOOD);
            side.drive(Span::from_nanos(100_000_000)).await;
            side.calls()
        })
        .expect("the run ends")
        .into_iter()
        .map(|(id, state, bytes)| (id, state, String::from_utf8(bytes).expect("UTF-8")))
        .collect()
}

/// A read buffer for each listen and connecting connection costs 64 KiB each, which no
/// call of open62541 shows, so the test reads the table.
#[test]
fn only_a_stream_that_reads_holds_a_read_buffer() {
    let mut network = Network::new();
    network.dial(Span::MILLISECOND, b"");
    let peer = SocketAddr::new(network.peer.addresses()[0], PORT);
    let buffers = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = Side::listening(&node, local(&node));
            assert_eq!(side.listen(PORT), Status::GOOD);
            assert_eq!(side.connect(peer), Status::GOOD);
            let opened = side.buffers();
            side.drive(Span::from_nanos(100_000_000)).await;
            (opened, side.buffers())
        })
        .expect("the run ends");
    assert_eq!(buffers, (vec![(1, 0), (2, 0)], vec![(1, 0), (3, 64 << 10)]));
}

#[test]
fn a_listen_gives_its_address_and_port_and_an_accept_the_address_of_the_peer() {
    let network = Network::new();
    let local = network.local.addresses()[0];
    let peer = network.peer.addresses()[0];
    drop(network);
    let calls = notes(local, &[]);
    assert_eq!(
        calls[..2],
        [
            (
                1,
                ffi::ESTABLISHED,
                format!("listen-address={local} listen-port={PORT}")
            ),
            (2, ffi::ESTABLISHED, format!("remote-address={peer}")),
        ]
    );
    assert!(calls[2..].iter().all(|(_, _, notes)| notes.is_empty()));
}

#[test]
fn a_listen_on_each_address_gives_no_address_or_port() {
    let network = Network::new();
    let peer = network.peer.addresses()[0];
    drop(network);
    let calls = notes(IpAddr::V4(Ipv4Addr::UNSPECIFIED), &[]);
    assert_eq!(
        calls[..2],
        [
            (1, ffi::ESTABLISHED, String::new()),
            (2, ffi::ESTABLISHED, format!("remote-address={peer}")),
        ]
    );
}

#[test]
fn a_listen_gives_the_host_of_its_address_as_its_listen_address() {
    let network = Network::new();
    let local = network.local.addresses()[0];
    drop(network);
    let listen = format!("listen-address=plc.example listen-port={PORT}");
    let scalar = notes(local, &[("address", Value::String("plc.example"))]);
    assert_eq!(scalar[0], (1, ffi::ESTABLISHED, listen.clone()));
    let array = notes(
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        &[("address", Value::Strings(&[c"plc.example"]))],
    );
    assert_eq!(array[0], (1, ffi::ESTABLISHED, listen));
    let empty = notes(local, &[("address", Value::Strings(&[]))]);
    assert_eq!(
        empty[0],
        (
            1,
            ffi::ESTABLISHED,
            format!("listen-address={local} listen-port={PORT}")
        )
    );
}

#[test]
fn a_listen_with_two_addresses_or_one_that_is_not_a_string_is_refused() {
    let mut network = Network::new();
    let calls = network
        .sim
        .run_on(&network.local.clone(), |node, _| async move {
            let side = Side::listening(&node, local(&node));
            let listen = |address| {
                side.open(&[
                    ("listen", Value::Boolean(true)),
                    ("port", Value::UInt16(PORT)),
                    ("address", address),
                ])
            };
            let statuses = [
                listen(Value::Strings(&[c"10.0.0.1", c"plc.example"])),
                listen(Value::UInt16(1)),
            ];
            assert_eq!(statuses, [Status::BAD_INVALID_ARGUMENT; 2]);
            assert_eq!(side.listen(PORT), Status::GOOD);
            side.calls()
        })
        .expect("the run ends");
    assert_eq!(calls, [(1, ffi::ESTABLISHED, Vec::new())]);
}

#[test]
fn a_server_with_a_host_in_its_url_has_that_url_alone_as_its_discovery_url() {
    let mut network = Network::new();
    network
        .sim
        .run_on(&network.local.clone(), |node, _| async move {
            let side = Side::listening(&node, local(&node));
            let url = c"opc.tcp://plc.example:4840";
            // SAFETY: the loop outlives the server, which the test deletes.
            let server = unsafe {
                ffi::test::shim_server_new(side.events().raw(), PORT, url.as_ptr(), 0)
            };
            assert!(!server.is_null());
            // SAFETY: the server lives.
            let status = Status(unsafe { ffi::test::UA_Server_run_startup(server) });
            assert_eq!(status, Status::GOOD);
            // SAFETY: the server lives.
            let first = unsafe { ffi::test::shim_server_discovery_url(server, 0) };
            assert_eq!(string(first), url.to_bytes());
            // SAFETY: the server lives.
            let second = unsafe { ffi::test::shim_server_discovery_url(server, 1) };
            assert!(second.is_null());
            // SAFETY: the server lives on the loop, and nothing uses it after.
            unsafe { side.manager.close_server(server) }.await;
        })
        .expect("the run ends");
}

/// Records a call as [`record`] does, with the context that the call saw as the
/// first byte of the message. A call with no context writes the id as the context. A
/// first `ESTABLISHED` with a context, of an accepted connection, writes 10 times the
/// id as its context and sends `hi`.
unsafe extern "C" fn adopt(
    cm: *mut ffi::ConnectionManager,
    id: usize,
    application: *mut c_void,
    context: *mut *mut c_void,
    state: ConnectionState,
    _: *const KeyValueMap,
    message: Bytes,
) {
    // SAFETY: `open` passes the calls of a live side.
    let calls = unsafe { &*application.cast::<RefCell<Vec<Call>>>() };
    // SAFETY: the manager gives the slot of the connection for the call.
    let seen = unsafe { *context }.addr();
    let mut bytes = vec![u8::try_from(seen).expect("a small context")];
    if message.length > 0 {
        // SAFETY: the manager gives `length` bytes at `data` for the call.
        bytes.extend(unsafe {
            std::slice::from_raw_parts(message.data, message.length)
        });
    }
    calls.borrow_mut().push((id, state, bytes));
    let accepted = state == ffi::ESTABLISHED && message.length == 0 && seen != 0;
    if seen == 0 {
        // SAFETY: as above.
        unsafe { *context = ptr::without_provenance_mut(id) };
    } else if accepted && seen < 10 {
        // SAFETY: as above.
        unsafe { *context = ptr::without_provenance_mut(10 * id) };
        assert_eq!(send_on(cm, id, b"hi"), Status::GOOD);
    }
}

#[test]
fn each_accepted_stream_is_a_connection_with_the_context_of_the_listen() {
    let mut network = Network::new();
    let first = network.dial(Span::from_nanos(1_000_000), b"a");
    let second = network.dial(Span::from_nanos(2_000_000), b"b");
    let late = network.dial(Span::from_nanos(500_000_000), b"c");
    let calls = network
        .sim
        .run_on(&network.local.clone(), |node, _| async move {
            let side = listening(&node);
            assert_eq!(side.listen(PORT), Status::GOOD);
            assert_eq!(side.calls(), [(1, ffi::ESTABLISHED, vec![0])]);
            side.drive(Span::from_nanos(100_000_000)).await;
            assert_eq!(side.close(1), Status::GOOD);
            side.drive(Span::from_nanos(100_000_000)).await;
            assert_eq!(side.send(2, b"late"), Status::GOOD);
            side.drive(Span::SECOND).await;
            side.calls()
        })
        .expect("the run ends");
    assert_eq!(
        calls,
        [
            (1, ffi::ESTABLISHED, vec![0]),
            (2, ffi::ESTABLISHED, vec![1]),
            (2, ffi::ESTABLISHED, vec![20, b'a']),
            (3, ffi::ESTABLISHED, vec![1]),
            (3, ffi::ESTABLISHED, vec![30, b'b']),
            (1, ffi::CLOSING, vec![1]),
        ]
    );
    let bytes = |reads: &Mutex<Reads>| reads.lock().expect("no panic").bytes();
    assert_eq!(bytes(&first), b"hilate");
    assert_eq!(bytes(&second), b"hi");
    let late = late.lock().expect("no panic under the lock");
    assert_eq!(
        late.error.as_deref(),
        Some("10.0.0.1:4840 refused the connection")
    );
}

#[test]
fn a_listen_from_a_run_accepts_and_reads_in_that_drive() {
    let mut network = Network::new();
    network.dial(Span::ZERO, b"a");
    let calls = network
        .sim
        .run_on(&network.local.clone(), |node, _| async move {
            let side = Side::listening(&node, local(&node));
            // The stream and its byte wait before the listen.
            side.clock.sleep(Span::MILLISECOND).await;
            let listened = Cell::new(false);
            let mut drive = pin!(side.manager.drive(|_| {
                if !listened.replace(true) {
                    assert_eq!(side.listen(PORT), Status::GOOD);
                }
                Poll::<Infallible>::Pending
            }));
            let mut end = side.clock.sleep(Span::from_nanos(10_000_000));
            // The end comes first, so the drive gets no pass at the end.
            poll_fn(|cx| {
                if Pin::new(&mut end).poll(cx).is_ready() {
                    return Poll::Ready(());
                }
                if let Poll::Ready(never) = drive.as_mut().poll(cx) {
                    match never {}
                }
                Poll::Pending
            })
            .await;
            side.calls()
        })
        .expect("the run ends");
    assert_eq!(
        calls,
        [
            (1, ffi::ESTABLISHED, vec![]),
            (2, ffi::ESTABLISHED, vec![]),
            (2, ffi::ESTABLISHED, b"a".to_vec()),
        ]
    );
}

#[test]
fn a_pass_accepts_each_stream_that_waits() {
    let mut network = Network::new();
    for say in [b"a", b"b", b"c", b"d"] {
        network.dial(Span::ZERO, say);
    }
    let calls = network
        .sim
        .run_on(&network.local.clone(), |node, _| async move {
            let side = Side::listening(&node, local(&node));
            side.drive(Span::from_nanos(10_000_000)).await;
            assert_eq!(side.listen(PORT), Status::GOOD);
            side.drive(Span::from_nanos(10_000_000)).await;
            side.calls()
        })
        .expect("the run ends");
    let opened: Vec<usize> = calls
        .iter()
        .filter(|(_, state, bytes)| *state == ffi::ESTABLISHED && bytes.is_empty())
        .map(|(id, _, _)| *id)
        .collect();
    assert_eq!(opened, [1, 2, 3, 4, 5]);
    assert_eq!(calls.len(), 9);
}

#[test]
fn a_send_on_the_listen_connection_is_refused() {
    let mut network = Network::new();
    let status = network
        .sim
        .run_on(&network.local.clone(), |node, _| async move {
            let side = Side::listening(&node, local(&node));
            assert_eq!(side.listen(PORT), Status::GOOD);
            side.send(1, b"no")
        })
        .expect("the run ends");
    assert_eq!(status, Status::BAD_CONNECTION_CLOSED);
}

/// Fails the listener once it accepted one stream, when `child::running()`.
#[test]
fn failed_listener() {
    if !child::running() {
        return;
    }
    let mut network = Network::new();
    let first = network.dial(Span::ZERO, b"a");
    let at = network.listening();
    let calls = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = listening(&node);
            assert_eq!(side.listen(PORT), Status::GOOD);
            side.drive(Span::from_nanos(10_000_000)).await;
            node.fail_listener(at);
            side.drive(Span::from_nanos(10_000_000)).await;
            assert_eq!(side.close(1), Status::BAD_NOT_FOUND);
            assert_eq!(side.send(2, b"on"), Status::GOOD);
            side.drive(Span::from_nanos(10_000_000)).await;
            side.calls()
        })
        .expect("the run ends");
    assert_eq!(
        calls,
        [
            (1, ffi::ESTABLISHED, vec![0]),
            (2, ffi::ESTABLISHED, vec![1]),
            (2, ffi::ESTABLISHED, vec![20, b'a']),
            (1, ffi::CLOSING, vec![1]),
        ]
    );
    assert_eq!(first.lock().expect("no panic").bytes(), b"hion");
}

#[test]
fn an_accept_error_gives_closing_of_the_listen_and_a_warning() {
    assert_eq!(
        stderr("failed_listener"),
        "connector-opcua: open62541 warning: connection 1: the accept failed: \
         network call failed with OS error 5\n"
    );
}

/// Opens a listen on `port`, with a listener at [`PORT`] unless `none`, and a second
/// listen when `twice`, when `child::running()`.
fn listen_and_abort(port: u16, none: bool, twice: bool) {
    if !child::running() {
        return;
    }
    let mut network = Network::new();
    network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let side = if none {
                Side::new(&node)
            } else {
                listening(&node)
            };
            side.listen(port);
            if twice {
                side.listen(port);
            }
        })
        .expect("the run ends");
}

#[test]
fn listen_with_no_listener() {
    listen_and_abort(PORT, true, false);
}

#[test]
fn listen_twice() {
    listen_and_abort(PORT, false, true);
}

#[test]
fn listen_on_another_port() {
    listen_and_abort(PORT + 1, false, false);
}

/// Runs the test `name` of this module in a child process, asserts that it aborts,
/// and gives its panic message. The test harness writes the message to stdout.
fn abort(name: &str) -> String {
    let output = child::output(&format!("connection::tests::{name}"), &[]);
    assert!(!output.status.success(), "{name} ends");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let message = stdout
        .lines()
        .skip_while(|line| {
            !line.contains("panicked at crates/connector-opcua/src/connection.rs")
        })
        .nth(1);
    message.expect(&stdout).to_owned()
}

#[test]
fn a_listen_with_no_listener_or_after_a_listen_aborts() {
    let message = "a listen open takes the listener that `Manager::listening` got";
    assert_eq!(abort("listen_with_no_listener"), message);
    assert_eq!(abort("listen_twice"), message);
}

#[test]
fn a_listen_on_a_port_other_than_the_listener_aborts() {
    assert_eq!(
        abort("listen_on_another_port"),
        "a listen open on port 4841, but the listener is at 10.0.0.1:4840"
    );
}

/// Gives the `HEL` message of a client that asks for `url`, with buffers of 64 KiB
/// and no limit on a message or its chunks.
fn hel(url: &str) -> Vec<u8> {
    let length = u32::try_from(32 + url.len()).expect("a short URL");
    let url_length = u32::try_from(url.len()).expect("a short URL");
    let mut bytes = b"HELF".to_vec();
    for field in [length, 0, 1 << 16, 1 << 16, 0, 0, url_length] {
        bytes.extend(field.to_le_bytes());
    }
    bytes.extend(url.as_bytes());
    bytes
}

#[test]
fn a_server_answers_hel_with_ack_and_its_shutdown_closes_each_connection() {
    let mut network = Network::new();
    let reads = network.dial(Span::MILLISECOND, &hel("opc.tcp://10.0.0.1:4840"));
    let calls = network
        .sim
        .run_on(&network.local.clone(), |node, _| async move {
            let side = Side::listening(&node, local(&node));
            // SAFETY: the loop outlives the server, which the test deletes.
            let server = unsafe {
                ffi::test::shim_server_new(
                    side.events().raw(),
                    PORT,
                    c"opc.tcp://:4840".as_ptr(),
                    1,
                )
            };
            assert!(!server.is_null());
            // SAFETY: the server lives.
            let status = Status(unsafe { ffi::test::UA_Server_run_startup(server) });
            assert_eq!(status, Status::GOOD);
            // SAFETY: the server lives.
            let url = unsafe { ffi::test::shim_server_discovery_url(server, 0) };
            assert_eq!(string(url), b"opc.tcp://10.0.0.1:4840");
            // SAFETY: the server lives.
            let url = unsafe { ffi::test::shim_server_discovery_url(server, 1) };
            assert!(url.is_null());
            side.drive(Span::SECOND).await;
            // SAFETY: the server lives.
            let status = Status(unsafe { ffi::test::UA_Server_run_shutdown(server) });
            assert_eq!(status, Status::GOOD);
            // SAFETY: the server lives.
            let state = unsafe { ffi::test::UA_Server_getLifecycleState(server) };
            assert_eq!(state, ffi::test::Lifecycle::STOPPING);
            side.drive(Span::SECOND).await;
            assert_eq!(side.connections(), 0);
            // SAFETY: the server lives.
            let state = unsafe { ffi::test::UA_Server_getLifecycleState(server) };
            assert_eq!(state, ffi::test::Lifecycle::STOPPED);
            // SAFETY: the server is stopped, and nothing holds it.
            let status = Status(unsafe { ffi::test::UA_Server_delete(server) });
            assert_eq!(status, Status::GOOD);
            side.calls()
        })
        .expect("the run ends");
    assert_eq!(calls, []);
    let reads = reads.lock().expect("no panic under the lock");
    assert_eq!(reads.error, None);
    assert!(reads.ended.is_some());
    let mut ack = b"ACKF".to_vec();
    for field in [28_u32, 0, 1 << 16, 1 << 16, 1 << 29, 1 << 14] {
        ack.extend(field.to_le_bytes());
    }
    assert_eq!(reads.bytes(), ack);
}

#[test]
fn a_stopped_server_with_a_session_is_deleted_with_its_session() {
    let mut network = Network::new();
    network
        .sim
        .run_on(&network.local.clone(), |node, _| async move {
            let side = Side::listening(&node, local(&node));
            // SAFETY: the loop outlives the server, which the test deletes.
            let server = unsafe {
                ffi::test::shim_server_new(
                    side.events().raw(),
                    PORT,
                    c"opc.tcp://:4840".as_ptr(),
                    1,
                )
            };
            assert!(!server.is_null());
            // SAFETY: the server lives.
            let status = Status(unsafe { ffi::test::UA_Server_run_startup(server) });
            assert_eq!(status, Status::GOOD);
            // SAFETY: the loop outlives the client, which the test deletes.
            let client = unsafe { ffi::shim_client_new(side.events().raw()) };
            assert!(!client.is_null());
            let url = c"opc.tcp://10.0.0.1:4840";
            // SAFETY: the client lives, and copies the URL.
            let status = Status(unsafe {
                ffi::test::UA_Client_connectAsync(client, url.as_ptr())
            });
            assert_eq!(status, Status::GOOD);
            side.drive(Span::SECOND).await;
            // SAFETY: the server lives on the loop, and nothing uses it after.
            unsafe { side.manager.close_server(server) }.await;
            // A client that has not seen the close waits in its delete for the answer
            // of `CloseSession`, while the sim clock stands still.
            side.drive(Span::SECOND).await;
            // SAFETY: nothing uses the client after it.
            unsafe { ffi::UA_Client_delete(client) };
            side.drive(Span::SECOND).await;
        })
        .expect("the run ends");
}

/// The `CloseSession` service of a running server removes the session after the
/// service ends, which still writes to it.
#[test]
fn a_session_that_its_client_closes_is_removed_after_the_service() {
    let mut network = Network::new();
    network
        .sim
        .run_on(&network.local.clone(), |node, _| async move {
            let side = Side::listening(&node, local(&node));
            // SAFETY: the loop outlives the server, which the test deletes.
            let server = unsafe {
                ffi::test::shim_server_new(
                    side.events().raw(),
                    PORT,
                    c"opc.tcp://:4840".as_ptr(),
                    1,
                )
            };
            assert!(!server.is_null());
            // SAFETY: the server lives.
            let status = Status(unsafe { ffi::test::UA_Server_run_startup(server) });
            assert_eq!(status, Status::GOOD);
            // SAFETY: the loop outlives the client, which the test deletes.
            let client = unsafe { ffi::shim_client_new(side.events().raw()) };
            assert!(!client.is_null());
            let url = c"opc.tcp://10.0.0.1:4840";
            // SAFETY: the client lives, and copies the URL.
            let status = Status(unsafe {
                ffi::test::UA_Client_connectAsync(client, url.as_ptr())
            });
            assert_eq!(status, Status::GOOD);
            side.drive(Span::SECOND).await;
            // SAFETY: the client lives.
            let status =
                Status(unsafe { ffi::test::UA_Client_disconnectAsync(client) });
            assert_eq!(status, Status::GOOD);
            side.drive(Span::SECOND).await;
            // SAFETY: the server lives on the loop, and nothing uses it after.
            unsafe { side.manager.close_server(server) }.await;
            // SAFETY: nothing uses the client after it.
            unsafe { ffi::UA_Client_delete(client) };
            side.drive(Span::SECOND).await;
        })
        .expect("the run ends");
}

/// Records the service result of the response at `response` in the `Cell<u32>` at
/// `data`.
///
/// # Safety
///
/// `data` points at a live `Cell<u32>`, and `response` at a response of a service.
unsafe extern "C" fn created(
    _: *mut ffi::Client,
    data: *mut c_void,
    _: u32,
    response: *mut c_void,
) {
    // SAFETY: open62541 gives a response.
    let result = unsafe { ffi::test::shim_response_result(response) };
    // SAFETY: the test gives a live cell.
    unsafe { (*data.cast::<Cell<u32>>()).set(result) };
}

/// The close of a channel purges its session that is not activated, and the removal
/// waits in the loop when the server becomes `STOPPED`.
#[test]
fn a_stopped_server_is_deleted_when_its_loop_has_nothing_due() {
    // On and off the 100 ns grid of the loop.
    for offset in [0, 50] {
        let mut network = Network::new();
        network
            .sim
            .run_on(&network.local.clone(), move |node, _| async move {
                let side = Side::listening(&node, local(&node));
                // SAFETY: the loop outlives the server, which the test deletes.
                let server = unsafe {
                    ffi::test::shim_server_new(
                        side.events().raw(),
                        PORT,
                        c"opc.tcp://:4840".as_ptr(),
                        1,
                    )
                };
                assert!(!server.is_null());
                // SAFETY: the server lives.
                let status =
                    Status(unsafe { ffi::test::UA_Server_run_startup(server) });
                assert_eq!(status, Status::GOOD);
                // SAFETY: the loop outlives the client, which the test deletes.
                let client = unsafe { ffi::shim_client_new(side.events().raw()) };
                assert!(!client.is_null());
                let url = c"opc.tcp://10.0.0.1:4840";
                // SAFETY: the client lives, and copies the URL.
                let status = Status(unsafe {
                    ffi::test::UA_Client_connectSecureChannelAsync(client, url.as_ptr())
                });
                assert_eq!(status, Status::GOOD);
                side.drive(Span::SECOND).await;
                let request_type = builtin(459); // CreateSessionRequest
                let response_type = builtin(462); // CreateSessionResponse
                // SAFETY: a valid type.
                let request = unsafe { ffi::test::UA_new(request_type) };
                let result = Box::new(Cell::new(u32::MAX));
                // SAFETY: the client lives; the request is encoded at once.
                let status = Status(unsafe {
                    ffi::test::__UA_Client_AsyncService(
                        client,
                        request,
                        request_type,
                        created as *const c_void,
                        response_type,
                        ptr::from_ref(&*result).cast_mut().cast(),
                        ptr::null_mut(),
                    )
                });
                // SAFETY: made by `UA_new`.
                unsafe { ffi::test::UA_delete(request, request_type) };
                assert_eq!(status, Status::GOOD);
                side.drive(Span::SECOND).await;
                assert_eq!(Status(result.get()), Status::GOOD, "the session is made");
                side.clock.sleep(Span::from_nanos(offset)).await;
                // SAFETY: the server lives on the loop, and nothing uses it after.
                unsafe { side.manager.close_server(server) }.await;
                // SAFETY: nothing uses the client after it.
                unsafe { ffi::UA_Client_delete(client) };
                side.drive(Span::SECOND).await;
            })
            .expect("the run ends");
    }
}

#[test]
fn a_manager_that_listens_at_a_taken_address_fails() {
    let mut network = Network::new();
    let failed = network
        .sim
        .run_on(&network.local.clone(), |node, _| async move {
            let local = local(&node);
            let _side = Side::listening(&node, local);
            let rng = &mut Rng::from_seed(0);
            let second = Manager::listening(node.clock(), node.net(), local, 4, rng);
            (second.err(), local)
        })
        .expect("the run ends");
    let (error, local) = failed;
    assert_eq!(error, Some(net::Error::AddressInUse { local }));
}

/// How the manager opens the one stream of a test.
#[derive(Clone, Copy)]
enum Open {
    Accepted,
    Dialed,
}

impl Open {
    /// Opens the stream with the peer in [`hold`] with `delay` and `bytes`, drives
    /// the manager for 3.5 s, and gives `body` the side, the key of the stream, and
    /// the count of [`hold`].
    fn run<T, F>(
        self,
        network: &mut Network,
        delay: Span,
        bytes: usize,
        body: impl FnOnce(Side, usize, Arc<AtomicUsize>) -> F + Send + 'static,
    ) -> T
    where
        T: Send + 'static,
        F: Future<Output = T> + 'static,
    {
        let taken = match self {
            Self::Accepted => network.dial_and_hold(delay, bytes),
            Self::Dialed => network.accept_and_hold(delay, bytes),
        };
        let remote = network.remote();
        network
            .sim
            .run_on(&network.local.clone(), move |node, _| async move {
                let side = match self {
                    Self::Accepted => {
                        let side = Side::listening(&node, local(&node));
                        assert_eq!(side.listen(PORT), Status::GOOD);
                        side
                    }
                    Self::Dialed => {
                        let side = Side::new(&node);
                        assert_eq!(side.connect(remote), Status::GOOD);
                        side
                    }
                };
                side.drive(Span::from_nanos(3_500_000_000)).await;
                let id = match self {
                    Self::Accepted => side.calls()[1].0,
                    Self::Dialed => 1,
                };
                body(side, id, taken).await
            })
            .expect("the run ends")
    }
}

/// Sends 1 KiB at a time on the stream that `open` opens, whose peer reads nothing,
/// until the manager refuses one, and gives the count of sends that it took.
fn sent(network: &mut Network, open: Open) -> usize {
    open.run(network, Span::ZERO, 0, |side, id, _| async move {
        let mut sent = 0;
        while side.send(id, &[7; 1 << 10]) == Status::GOOD {
            sent += 1;
            side.drive(Span::from_nanos(1_000_000)).await;
        }
        sent
    })
}

/// Gives the bytes that the peer writes in 1 s on the stream that `open` opens,
/// while the manager reads nothing.
fn received(network: &mut Network, open: Open) -> usize {
    let delay = Span::from_nanos(3_600_000_000);
    open.run(network, delay, 1 << 20, |side, _, taken| async move {
        side.clock.sleep(Span::from_nanos(2_000_000_000)).await;
        let taken = taken.load(Ordering::Relaxed);
        // The block holds all of `side`, not only its clock, so its streams live.
        drop(side);
        taken
    })
}

/// A link of 1 s, so that no ack comes back while [`sent`] sends.
fn slow(network: &mut Network) {
    let link = sim::link::Config {
        delay: Span::SECOND,
        ..sim::link::Config::default()
    };
    network.sim.link(&network.local, &network.peer, link);
    network.sim.link(&network.peer, &network.local, link);
}

/// After acks, an accepted stream holds the 16 KiB of `unsent_bytes_max` of
/// [`OPTIONS`] past the 64 KiB window of the peer, and [`SENDS`] wait.
#[test]
fn an_accepted_stream_holds_the_unsent_bytes_of_the_manager() {
    let mut network = Network::new();
    assert_eq!(sent(&mut network, Open::Accepted), SENDS + 64 + 16);
}

/// With no ack, an accepted stream takes the 64 KiB of `send_buffer_bytes` of
/// [`OPTIONS`], and [`SENDS`] wait.
#[test]
fn an_accepted_stream_takes_the_send_buffer_of_the_manager() {
    let mut network = Network::new();
    slow(&mut network);
    assert_eq!(sent(&mut network, Open::Accepted), SENDS + 64);
}

/// The peer writes the 64 KiB of `recv_buffer_bytes` of [`OPTIONS`], and the 64 KiB
/// of its own send buffer.
#[test]
fn an_accepted_stream_takes_the_receive_buffer_of_the_manager() {
    let mut network = Network::new();
    assert_eq!(received(&mut network, Open::Accepted), (64 + 64) << 10);
}

/// As [`an_accepted_stream_holds_the_unsent_bytes_of_the_manager`], for a dialed
/// stream.
#[test]
fn a_dialed_stream_holds_the_unsent_bytes_of_the_manager() {
    let mut network = Network::new();
    assert_eq!(sent(&mut network, Open::Dialed), SENDS + 64 + 16);
}

/// As [`an_accepted_stream_takes_the_send_buffer_of_the_manager`], for a dialed
/// stream.
#[test]
fn a_dialed_stream_takes_the_send_buffer_of_the_manager() {
    let mut network = Network::new();
    slow(&mut network);
    assert_eq!(sent(&mut network, Open::Dialed), SENDS + 64);
}

/// As [`an_accepted_stream_takes_the_receive_buffer_of_the_manager`], for a dialed
/// stream.
#[test]
fn a_dialed_stream_takes_the_receive_buffer_of_the_manager() {
    let mut network = Network::new();
    assert_eq!(received(&mut network, Open::Dialed), (64 + 64) << 10);
}

/// The peer sends 64 KiB, the receive buffer of [`OPTIONS`], before the stream
/// reads, so the first read takes it all.
#[test]
fn a_read_takes_the_receive_buffer_at_once() {
    let mut network = Network::new();
    network.dial(Span::ZERO, &vec![7; OPTIONS.recv_buffer_bytes]);
    let calls = network
        .sim
        .run_on(&network.local.clone(), |node, _| async move {
            let side = Side::listening(&node, local(&node));
            assert_eq!(side.listen(PORT), Status::GOOD);
            side.drive(Span::from_nanos(100_000_000)).await;
            side.calls()
        })
        .expect("the run ends");
    let lengths: Vec<usize> = calls.iter().map(|(_, _, bytes)| bytes.len()).collect();
    assert_eq!(lengths, [0, 0, OPTIONS.recv_buffer_bytes]);
}
