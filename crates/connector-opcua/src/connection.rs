//! The TCP connection manager of a loop. open62541 opens, writes, and closes its
//! connections through it, and each connection is an `env::net` stream that
//! [`Manager::drive`] drives.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::fmt;
use std::future::poll_fn;
use std::io::{IoSlice, Write as _};
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use env::clock::{Clock, Sleep};
use env::net::{self, Listener, Net, Tcp, tcp};
use env::rng::Rng;
use types::time::Span;

use crate::event::Loop;
use crate::ffi::{self, Bytes, Status};

/// The options of each stream.
const OPTIONS: tcp::Options = tcp::Options {
    send_buffer_bytes: 1 << 16,
    recv_buffer_bytes: 1 << 16,
    unsent_bytes_max: NonZeroUsize::new(1 << 14).expect("invariant: 2^14 is not 0"),
    delayed: false,
};

/// The size of the read buffer of each connection.
const READ_BYTES: usize = 1 << 16;

/// Gives a buffer for one read.
fn read_buffer() -> Box<[u8]> {
    vec![0; READ_BYTES].into_boxed_slice()
}

/// The most sends that wait on one connection. A send past it closes the connection.
/// open62541 allocates each at most at the send buffer size of its channel. Sends wait
/// from one pass to the next, and longer while a stream is full, so an owner keeps the
/// chunks of its messages in flight at most this.
const SENDS: usize = 256;

/// The most sends that one write takes.
const PARTS: usize = 16;

/// How long a closed connection may write what waits before it drops its stream.
const LINGER: Span = Span::from_nanos(10_000_000_000);

static HOOKS: ffi::Hooks = ffi::Hooks {
    open,
    listen,
    send,
    close,
};

/// A TCP connection manager and the loop it is linked into, on the thread that made
/// it. Delete each client and server on its loop before it drops.
pub(crate) struct Manager {
    state: NonNull<State>,
    events: Loop,
}

impl Manager {
    /// Makes a loop on `clock` and `rng`, as [`Loop::new`] does, and a manager that
    /// connects through `net`, linked first into the event sources of the loop, where a
    /// client finds it.
    ///
    /// # Panics
    ///
    /// When a C allocation fails.
    pub(crate) fn new(clock: Clock, net: Net, rng: &mut Rng) -> Self {
        Self::make(clock, net, None, rng)
    }

    /// As [`Manager::new`], and the manager also accepts on a listener that it binds on
    /// `local`, with a queue of `backlog` streams, where a server finds it.
    ///
    /// # Errors
    ///
    /// As [`Net::listen`].
    ///
    /// # Panics
    ///
    /// When a C allocation fails.
    pub(crate) fn listening(
        clock: Clock,
        net: Net,
        local: SocketAddr,
        backlog: u32,
        rng: &mut Rng,
    ) -> Result<Self, net::Error> {
        let listener = net.listen(&tcp::Listen {
            local,
            backlog,
            options: OPTIONS,
        })?;
        Ok(Self::make(clock, net, Some(listener), rng))
    }

    fn make(clock: Clock, net: Net, listener: Option<Listener>, rng: &mut Rng) -> Self {
        let events = Loop::new(Clock::clone(&clock), rng);
        let state = NonNull::from(Box::leak(Box::new(State {
            net,
            listener: Cell::new(listener),
            clock,
            raw: Cell::new(ptr::null_mut()),
            events: events.raw(),
            table: RefCell::new(BTreeMap::new()),
            next: Cell::new(1),
            waker: RefCell::new(None),
            driving: Cell::new(false),
            held: Cell::new(false),
            ahead: Cell::new(usize::MAX),
            again: RefCell::new(Vec::new()),
            #[cfg(feature = "sim")]
            moves: Cell::new(0),
            ends: RefCell::new(VecDeque::new()),
            closed: UnsafeCell::new(ffi::DelayedCallback {
                next: ptr::null_mut(),
                callback: closed,
                application: ptr::null_mut(),
                context: ptr::null_mut(),
            }),
            queued: Cell::new(false),
        })));
        // SAFETY: the box lives until `drop`.
        let this = unsafe { state.as_ref() };
        // SAFETY: nothing else points at the callback yet.
        unsafe { (*this.closed.get()).application = state.as_ptr().cast() };
        // SAFETY: the hooks take the state, which lives until `drop` frees the manager.
        let raw = unsafe {
            ffi::shim_cm_new(events.raw(), &raw const HOOKS, state.as_ptr().cast())
        };
        assert!(
            !raw.is_null(),
            "open62541: out of memory for a connection manager"
        );
        this.raw.set(raw);
        Self { state, events }
    }

    /// Gives the loop, for a client or server config with `externalEventLoop`.
    pub(crate) fn events(&self) -> &Loop {
        &self.events
    }

    /// Moves the connections on and calls `run` until `run` gives a value, and gives
    /// it. `run` runs the loop with a timeout of 0, alone or through its client or
    /// server, and may poll its own sources with the context it gets. Between calls,
    /// the drive sleeps until the next timer of the loop or a wake, also from a send or
    /// a close that `run` or another task asks for. After each call, also the one that
    /// gives the value, it moves on each connection again after each connect, send, or
    /// close on it that the call or such a step asks for, so it can also read and call
    /// open62541 back after that call.
    ///
    /// # Panics
    ///
    /// When another drive of the manager runs, since one would take the wakes of the
    /// other.
    pub(crate) async fn drive<T>(
        &self,
        mut run: impl FnMut(&mut Context<'_>) -> Poll<T>,
    ) -> T {
        let state = self.state();
        assert!(
            !state.held.replace(true),
            "one drive of a manager at a time"
        );
        let _held = Held(&state.held);
        let mut sleep: Option<Sleep> = None;
        poll_fn(|cx| {
            state.driving.set(true);
            let _driving = Held(&state.driving);
            self.pass(cx);
            loop {
                let poll = run(cx);
                let moved = state.move_on_again(cx);
                if poll.is_ready() {
                    break poll;
                }
                if moved {
                    continue;
                }
                let Some(next) = self.events.next() else {
                    break Poll::Pending;
                };
                let timer = sleep.get_or_insert_with(|| state.clock.sleep_until(next));
                timer.reset(next);
                if Pin::new(timer).poll(cx).is_pending() {
                    break Poll::Pending;
                }
            }
        })
        .await
    }

    /// Drives the manager until nothing is due on the loop, then deletes `server`. Call
    /// it after `UA_Server_run_shutdown`. It checks after each run of the loop, so a
    /// busy loop only makes the delete later.
    ///
    /// # Safety
    ///
    /// `server` lives on the loop of the manager, and nothing uses it after the call
    /// returns.
    ///
    /// # Panics
    ///
    /// If open62541 fails a run of the loop or refuses the delete, or if the server is
    /// not stopped when nothing is due: with no `UA_Server_run_shutdown` before the
    /// call, or after a close that does not queue its `CLOSING`. Each panic but the
    /// refused delete comes before the delete, so `server` still lives after it.
    #[cfg(any(test, feature = "sim"))]
    pub(crate) async unsafe fn delete_server(&self, server: *mut ffi::test::Server) {
        let events = &self.events;
        self.drive(|_| {
            events.run();
            if events.due() {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
        // SAFETY: the server lives.
        let state = unsafe { ffi::test::UA_Server_getLifecycleState(server) };
        assert_eq!(
            state,
            ffi::test::Lifecycle::STOPPED,
            "invariant: a close queues its `CLOSING` at once"
        );
        // SAFETY: the server is stopped, and nothing holds it.
        let status = Status(unsafe { ffi::test::UA_Server_delete(server) });
        assert_eq!(status, Status::GOOD, "open62541 refused the server delete");
    }

    /// Connects, reads, and writes each connection until each waits, and has the task
    /// of `cx` woken when one can go on.
    fn pass(&self, cx: &mut Context<'_>) {
        let state = self.state();
        state.park(cx.waker());
        let mut after = 0;
        loop {
            state.ahead.set(after);
            let next = state
                .table
                .borrow()
                .range(after..)
                .next()
                .map(|(id, _)| *id);
            let Some(id) = next else { break };
            after = id + 1;
            state.move_on(id, cx);
        }
        state.ahead.set(usize::MAX);
    }

    fn state(&self) -> &State {
        // SAFETY: the box lives until `drop`, and C reads it only in a call on this
        // thread.
        unsafe { self.state.as_ref() }
    }
}

impl Drop for Manager {
    /// Drops each connection with no callback, unlinks and frees the manager, then
    /// drops the loop.
    fn drop(&mut self) {
        let state = self.state();
        if state.queued.get() {
            // SAFETY: the loop lives, and holds the callback in its queue.
            unsafe {
                (state.members().remove_delayed)(state.events, state.closed.get());
            };
        }
        // SAFETY: `new` made it, and the loop no longer calls its hooks.
        unsafe { ffi::shim_cm_free(state.raw.get()) };
        // SAFETY: `new` leaked the box, and nothing reads it now.
        drop(unsafe { Box::from_raw(self.state.as_ptr()) });
    }
}

/// What the manager and its hooks share. No borrow of a `RefCell` spans a call into C,
/// since C may call a hook again.
struct State {
    net: Net,
    /// The listener until a listen open takes it.
    listener: Cell<Option<Listener>>,
    clock: Clock,
    raw: Cell<*mut ffi::ConnectionManager>,
    events: *mut ffi::EventLoop,
    table: RefCell<BTreeMap<usize, Connection>>,
    /// The key of the next connection. open62541 reads 0 as no connection.
    next: Cell<usize>,
    /// The waker of the last pass.
    waker: RefCell<Option<Waker>>,
    /// Whether a drive runs, so a wake needs no waker.
    driving: Cell<bool>,
    /// Whether a drive exists, from its first poll until its drop.
    held: Cell<bool>,
    /// The first key that the running pass has not reached, or `usize::MAX` outside a
    /// pass.
    ahead: Cell<usize>,
    /// The connections that a hook asks to move on again during a drive, once the
    /// running pass has gone past them.
    again: RefCell<Vec<usize>>,
    /// The calls of `move_on`, for tests of the work of a drive.
    #[cfg(feature = "sim")]
    moves: Cell<usize>,
    /// The connections whose `CLOSING` the next run of the loop gives.
    ends: RefCell<VecDeque<usize>>,
    /// The delayed callback that gives each `CLOSING`. C writes its `next`.
    closed: UnsafeCell<ffi::DelayedCallback>,
    queued: Cell<bool>,
}

impl State {
    fn members(&self) -> &ffi::EventLoop {
        // SAFETY: the loop outlives the manager, and C writes it only in a call that
        // no shared borrow spans.
        unsafe { &*self.events }
    }

    fn park(&self, waker: &Waker) {
        let mut slot = self.waker.borrow_mut();
        if !slot.as_ref().is_some_and(|w| w.will_wake(waker)) {
            *slot = Some(waker.clone());
        }
    }

    /// Has connection `id` move on: in the pass that runs, in another pass of the
    /// drive, or in a drive that its waker starts.
    fn wake(&self, id: usize) {
        if self.driving.get() {
            let mut again = self.again.borrow_mut();
            if id < self.ahead.get() && !again.contains(&id) {
                again.push(id);
            }
        } else if let Some(waker) = self.waker.borrow().as_ref() {
            waker.wake_by_ref();
        }
    }

    /// Moves on each connection in `again`, and each that those steps add, until
    /// none is left, and gives whether it moved one.
    fn move_on_again(&self, cx: &mut Context<'_>) -> bool {
        let mut moved = false;
        loop {
            let Some(id) = self.again.borrow_mut().pop() else {
                return moved;
            };
            self.move_on(id, cx);
            moved = true;
        }
    }

    /// Logs `what` of connection `id` as a warning through the logger of the loop.
    fn warn(&self, id: usize, what: fmt::Arguments<'_>) {
        let message = format!("connection {id}: {what}");
        // SAFETY: the loop lives, and the call reads the bytes only during it.
        unsafe { ffi::shim_log_warning(self.events, message.as_ptr(), message.len()) };
    }

    /// Starts the close of the stream of `id`, and queues its `CLOSING` once.
    fn end(&self, id: usize) {
        {
            let mut table = self.table.borrow_mut();
            let Some(connection) = table.get_mut(&id) else {
                return;
            };
            connection.stream =
                match std::mem::replace(&mut connection.stream, Stream::Closed) {
                    Stream::Connecting(_) | Stream::Listening(_) => Stream::Closed,
                    Stream::Open(tcp) => Stream::Closing {
                        tcp,
                        linger: self.clock.sleep_until(self.clock.now() + LINGER),
                        shut: false,
                        drained: false,
                    },
                    stream @ (Stream::Closing { .. } | Stream::Closed) => {
                        connection.stream = stream;
                        return;
                    }
                };
        }
        self.queue_closing(id);
    }

    /// Warns of `failure` on `id`, drops its stream and sends, and queues its
    /// `CLOSING` unless a close queued it.
    fn fail(&self, id: usize, failure: &Failure) {
        self.warn(id, format_args!("{failure}"));
        let stream = self
            .table
            .borrow_mut()
            .get_mut(&id)
            .expect("invariant: only `Step::Gone` of this id removes it")
            .take_stream();
        match stream {
            Stream::Connecting(_) | Stream::Listening(_) | Stream::Open(_) => {
                self.queue_closing(id);
            }
            Stream::Closing { .. } | Stream::Closed => {}
        }
    }

    /// Queues the `CLOSING` of `id` for the next run of the loop.
    fn queue_closing(&self, id: usize) {
        self.ends.borrow_mut().push_back(id);
        if !self.queued.replace(true) {
            // SAFETY: the loop lives, and the callback is not in its queue.
            unsafe { (self.members().add_delayed)(self.events, self.closed.get()) };
        }
        self.wake(id);
    }

    /// Moves connection `id` on until it waits, and calls C with no borrow held.
    fn move_on(&self, id: usize, cx: &mut Context<'_>) {
        #[cfg(feature = "sim")]
        self.moves.set(self.moves.get() + 1);
        loop {
            let step = match self.table.borrow_mut().get_mut(&id) {
                Some(connection) => connection.step(cx),
                None => return,
            };
            let step = match step {
                Ok(step) => step,
                Err(failure) => {
                    self.fail(id, &failure);
                    continue;
                }
            };
            match step {
                Step::Waiting => return,
                Step::Ended => self.end(id),
                Step::Gone => {
                    self.table.borrow_mut().remove(&id);
                    return;
                }
                Step::Established => self.call(id, ffi::ESTABLISHED, &mut []),
                Step::Accepted(tcp) => self.accept(id, tcp),
                Step::Read(mut buffer, n) => {
                    self.call(id, ffi::ESTABLISHED, &mut buffer[..n]);
                    self.table
                        .borrow_mut()
                        .get_mut(&id)
                        .expect("invariant: only `Step::Gone` of this id removes it")
                        .buffer = buffer;
                }
            }
        }
    }

    /// Adds a connection with `callback` and `stream`, and gives its key.
    fn insert(&self, callback: Callback, stream: Stream) -> usize {
        let id = self.next.get();
        self.next.set(id + 1);
        let buffer = match stream {
            Stream::Open(_) => read_buffer(),
            _ => Box::default(),
        };
        let connection = Connection {
            callback: Some(callback),
            stream,
            sends: VecDeque::new(),
            sent: 0,
            buffer,
        };
        self.table.borrow_mut().insert(id, connection);
        id
    }

    /// Adds `tcp`, which listen connection `listen` accepted, as a new connection
    /// with the context that `listen` has now, and gives it `ESTABLISHED`.
    fn accept(&self, listen: usize, tcp: Tcp) {
        let callback = self
            .table
            .borrow()
            .get(&listen)
            .and_then(|c| c.callback.as_ref())
            .map(|c| Callback {
                application: c.application,
                context: Rc::new(Cell::new(c.context.get())),
                function: c.function,
            })
            .expect(
                "invariant: a listen connection keeps its callback while it listens",
            );
        let peer = tcp.peer();
        let id = self.insert(callback, Stream::Open(tcp));
        let mut text = [0; ADDRESS_BYTES];
        self.establish(id, Params::Remote(ip_text(peer.ip(), &mut text)));
        self.wake(id);
    }

    /// Calls the connection callback of `id` with `state` and `message`.
    fn call(&self, id: usize, state: ffi::ConnectionState, message: &mut [u8]) {
        self.callback(id).call(self.raw.get(), id, state, message);
    }

    /// Calls the connection callback of `id` with `ESTABLISHED`, `params`, and no
    /// message, as the first callback of a connection that a listen made.
    fn establish(&self, id: usize, params: Params<'_>) {
        self.callback(id).establish(self.raw.get(), id, params);
    }

    /// Gives the callback of `id`.
    fn callback(&self, id: usize) -> Callback {
        self.table
            .borrow()
            .get(&id)
            .and_then(|c| c.callback.clone())
            .expect("invariant: a connection that is not closing has its callback")
    }
}

/// Clears a flag of a drive when it drops, also in a panic.
struct Held<'a>(&'a Cell<bool>);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// The connection callback of open62541 for one connection, with its arguments.
#[derive(Clone)]
struct Callback {
    application: *mut c_void,
    /// The one slot of the connection that each call gives C. It is an `Rc`, so that
    /// the slot lives on after a `CLOSING` takes the callback in a run of the loop
    /// that the call makes.
    context: Rc<Cell<*mut c_void>>,
    function: ffi::ConnectionCallback,
}

impl Callback {
    /// Calls the callback with `ESTABLISHED`, `params`, and no message. It
    /// allocates nothing.
    fn establish(
        &self,
        cm: *mut ffi::ConnectionManager,
        id: usize,
        params: Params<'_>,
    ) {
        let (address, port) = match params {
            Params::Listen { host, port } => (host, Some(port)),
            Params::Remote(address) => (address, None),
        };
        // SAFETY: open62541 gave the callback with its application, and C reads the
        // address only during the call.
        unsafe {
            ffi::shim_establish(
                cm,
                id,
                self.application,
                self.context.as_ptr(),
                self.function,
                address.as_ptr(),
                address.len(),
                port.as_ref().map_or(ptr::null(), ptr::from_ref),
            );
        }
    }

    /// Calls the callback, which may write `context`.
    fn call(
        &self,
        cm: *mut ffi::ConnectionManager,
        id: usize,
        state: ffi::ConnectionState,
        message: &mut [u8],
    ) {
        let params = ffi::KeyValueMap {
            size: 0,
            map: ptr::null_mut(),
        };
        let message = Bytes {
            length: message.len(),
            data: if message.is_empty() {
                ptr::null_mut()
            } else {
                message.as_mut_ptr()
            },
        };
        // SAFETY: open62541 gave the callback with its application, and reads the
        // message only during the call.
        unsafe {
            (self.function)(
                cm,
                id,
                self.application,
                self.context.as_ptr(),
                state,
                &raw const params,
                message,
            );
        }
    }
}

/// The parameters of the first callback of a connection that a listen made, as the
/// POSIX manager gives them. An empty address gives none.
#[derive(Clone, Copy)]
enum Params<'a> {
    /// `listen-address` and `listen-port` of a listen, from which a server makes its
    /// discovery URL.
    Listen { host: &'a [u8], port: u16 },
    /// `remote-address` of an accepted stream, which a server gives its channel.
    Remote(&'a [u8]),
}

/// The longest text of an IP address: an IPv6 address that holds an IPv4 address.
const ADDRESS_BYTES: usize = 45;

/// Writes `ip` into `text` and gives the bytes written, or none when `ip` is
/// unspecified.
fn ip_text(ip: IpAddr, text: &mut [u8; ADDRESS_BYTES]) -> &[u8] {
    if ip.is_unspecified() {
        return &[];
    }
    let mut cursor = &mut text[..];
    write!(cursor, "{ip}").expect("invariant: an IP address fits");
    let length = ADDRESS_BYTES - cursor.len();
    &text[..length]
}

enum Stream {
    Connecting(Pin<Box<dyn Future<Output = Result<Tcp, net::Error>>>>),
    Listening(Listener),
    Open(Tcp),
    /// It gives no more reads, drops what the peer sends while it writes what waits,
    /// closes its side, then reads until the peer closes its side, so the drop sends
    /// no reset.
    Closing {
        tcp: Tcp,
        /// Ends the close when it takes too long, with sends or a peer that has not
        /// closed.
        linger: Sleep,
        /// This side is closed for writing.
        shut: bool,
        /// The peer closed its side.
        drained: bool,
    },
    Closed,
}

/// One connection. It leaves the table once open62541 has its `CLOSING` and the
/// stream has closed.
struct Connection {
    /// `None` once open62541 has the `CLOSING`.
    callback: Option<Callback>,
    stream: Stream,
    sends: VecDeque<Buffer>,
    /// The bytes of the first send already written.
    sent: usize,
    /// Empty until the connect ends, and while a read callback holds it.
    buffer: Box<[u8]>,
}

/// What the poll of one connection asks the manager to do.
enum Step {
    Waiting,
    Established,
    Accepted(Tcp),
    Read(Box<[u8]>, usize),
    Ended,
    Gone,
}

/// Why a connection ends with a warning.
enum Failure {
    /// A call to the stream failed, during the named step.
    Net(&'static str, net::Error),
    /// The close took [`LINGER`].
    Lingered,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Net(during, e) => write!(f, "the {during} failed: {e}"),
            Self::Lingered => {
                write!(f, "the close took {LINGER}, so it drops the stream")
            }
        }
    }
}

impl Connection {
    /// Moves the connection on, or gives why it fails.
    fn step(&mut self, cx: &mut Context<'_>) -> Result<Step, Failure> {
        match &mut self.stream {
            Stream::Connecting(connect) => match connect.as_mut().poll(cx) {
                Poll::Pending => Ok(Step::Waiting),
                Poll::Ready(Ok(tcp)) => {
                    self.stream = Stream::Open(tcp);
                    self.buffer = read_buffer();
                    Ok(Step::Established)
                }
                Poll::Ready(Err(e)) => Err(Failure::Net("connect", e)),
            },
            Stream::Listening(listener) => match listener.poll_accept(cx) {
                Poll::Pending => Ok(Step::Waiting),
                Poll::Ready(Ok(tcp)) => Ok(Step::Accepted(tcp)),
                Poll::Ready(Err(e)) => Err(Failure::Net("accept", e)),
            },
            Stream::Open(tcp) => {
                if !self.buffer.is_empty() {
                    match tcp.poll_read(cx, &mut self.buffer) {
                        Poll::Ready(Ok(0)) => return Ok(Step::Ended),
                        Poll::Ready(Ok(n)) => {
                            return Ok(Step::Read(std::mem::take(&mut self.buffer), n));
                        }
                        Poll::Ready(Err(e)) => return Err(Failure::Net("read", e)),
                        Poll::Pending => {}
                    }
                }
                write(tcp, &mut self.sends, &mut self.sent, cx)?;
                Ok(Step::Waiting)
            }
            Stream::Closing {
                tcp,
                linger,
                shut,
                drained,
            } => {
                if Pin::new(linger).poll(cx).is_ready() {
                    return Err(Failure::Lingered);
                }
                while !*drained {
                    match tcp.poll_read(cx, &mut self.buffer) {
                        Poll::Ready(Ok(0)) => *drained = true,
                        Poll::Ready(Ok(_)) => {}
                        Poll::Ready(Err(e)) => return Err(Failure::Net("read", e)),
                        Poll::Pending => break,
                    }
                }
                if !write(tcp, &mut self.sends, &mut self.sent, cx)? {
                    return Ok(Step::Waiting);
                }
                if !*shut {
                    match tcp.poll_close(cx) {
                        Poll::Ready(Ok(())) => *shut = true,
                        Poll::Ready(Err(e)) => return Err(Failure::Net("close", e)),
                        Poll::Pending => return Ok(Step::Waiting),
                    }
                }
                if *drained {
                    self.take_stream();
                    return Ok(self.gone());
                }
                Ok(Step::Waiting)
            }
            Stream::Closed => Ok(self.gone()),
        }
    }

    /// Gives [`Step::Gone`] once open62541 has the `CLOSING` of a closed connection.
    fn gone(&self) -> Step {
        if self.callback.is_none() {
            Step::Gone
        } else {
            Step::Waiting
        }
    }

    /// Drops what waits to be written, and gives the stream it puts `Closed` in place
    /// of.
    fn take_stream(&mut self) -> Stream {
        self.sends.clear();
        self.sent = 0;
        std::mem::replace(&mut self.stream, Stream::Closed)
    }
}

/// Writes `sends` to `tcp` until none waits or the stream takes no more, and gives
/// whether none waits. `sent` is the count of bytes of the first that were written.
fn write(
    tcp: &mut Tcp,
    sends: &mut VecDeque<Buffer>,
    sent: &mut usize,
    cx: &mut Context<'_>,
) -> Result<bool, Failure> {
    while !sends.is_empty() {
        let mut parts = [IoSlice::new(&[]); PARTS];
        let mut count = 0;
        for (part, buffer) in parts.iter_mut().zip(&*sends) {
            *part = IoSlice::new(buffer.bytes());
            count += 1;
        }
        parts[0] = IoSlice::new(&sends[0].bytes()[*sent..]);
        let n = match tcp.poll_write(cx, &parts[..count]) {
            Poll::Ready(Ok(n)) => n,
            Poll::Ready(Err(e)) => return Err(Failure::Net("write", e)),
            Poll::Pending => return Ok(false),
        };
        advance(sends, sent, n);
    }
    Ok(true)
}

/// Frees each buffer of `sends` that `n` more written bytes finish. `sent` is the
/// count of bytes of the first that were written.
fn advance(sends: &mut VecDeque<Buffer>, sent: &mut usize, mut n: usize) {
    while let Some(first) = sends.front() {
        let left = first.bytes().len() - *sent;
        if n < left {
            *sent += n;
            return;
        }
        n -= left;
        *sent = 0;
        sends.pop_front();
    }
}

/// A buffer of `allocNetworkBuffer`, which it frees on drop.
struct Buffer(Bytes);

impl Buffer {
    fn bytes(&self) -> &[u8] {
        if self.0.length == 0 {
            return &[];
        }
        // SAFETY: open62541 allocated `length` bytes at `data`, and gave them up.
        unsafe { std::slice::from_raw_parts(self.0.data, self.0.length) }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: open62541 allocated it, and only this buffer holds it.
        unsafe { ffi::shim_buffer_free(&raw mut self.0) };
    }
}

/// Gives the `State` at `state`.
///
/// # Safety
///
/// `state` is the state of a live manager.
unsafe fn state<'a>(state: *mut c_void) -> &'a State {
    // SAFETY: the caller keeps the manager live.
    unsafe { &*state.cast::<State>() }
}

/// The hook of `openConnection` for a client: connects to `host` on `port`, and gives
/// `OPENING` before it returns.
///
/// # Safety
///
/// `state` is the state of `shim_cm_new`, and `host` points at `host.length` readable
/// bytes, or has length 0.
unsafe extern "C" fn open(
    state: *mut c_void,
    host: Bytes,
    port: u16,
    application: *mut c_void,
    context: *mut c_void,
    callback: ffi::ConnectionCallback,
) -> u32 {
    // SAFETY: C passes the state of `shim_cm_new`.
    let state = unsafe { self::state(state) };
    let host = if host.length == 0 {
        String::new()
    } else {
        // SAFETY: C gives a string of `length` bytes, which it holds during the call.
        let bytes = unsafe { std::slice::from_raw_parts(host.data, host.length) };
        String::from_utf8_lossy(bytes).into_owned()
    };
    let net = state.net.clone();
    let connect = Box::pin(async move {
        let addresses = net.resolve(&host, port).await?;
        let remote = addresses[0];
        net.connect(&tcp::Config {
            remote,
            options: OPTIONS,
        })
        .await
    });
    let callback = Callback {
        application,
        context: Rc::new(Cell::new(context)),
        function: callback,
    };
    let id = state.insert(callback, Stream::Connecting(connect));
    state.call(id, ffi::OPENING, &mut []);
    state.wake(id);
    Status::GOOD.0
}

/// The hook of `openConnection` for a server: takes the listener of the manager into
/// a new connection, and gives `ESTABLISHED` before it returns, with `host` as
/// `listen-address`, or the address of the listener when `host` is empty. Each stream
/// it accepts is a new connection that gets `ESTABLISHED`, with the context of the
/// listen connection at the accept.
///
/// # Panics
///
/// When the manager has no listener or gave it to an earlier listen open, or when the
/// listener is not on `port`. A panic in a hook aborts.
///
/// # Safety
///
/// `state` is the state of `shim_cm_new`, and `host` points at `host.length` readable
/// bytes, or has length 0.
unsafe extern "C" fn listen(
    state: *mut c_void,
    host: Bytes,
    port: u16,
    application: *mut c_void,
    context: *mut c_void,
    callback: ffi::ConnectionCallback,
) -> u32 {
    // SAFETY: C passes the state of `shim_cm_new`.
    let state = unsafe { self::state(state) };
    let listener = state
        .listener
        .take()
        .expect("a listen open takes the listener that `Manager::listening` got");
    let local = listener.local();
    assert!(
        local.port() == port,
        "a listen open on port {port}, but the listener is at {local}"
    );
    let callback = Callback {
        application,
        context: Rc::new(Cell::new(context)),
        function: callback,
    };
    let id = state.insert(callback, Stream::Listening(listener));
    let mut text = [0; ADDRESS_BYTES];
    let host = if host.length == 0 {
        ip_text(local.ip(), &mut text)
    } else {
        // SAFETY: C gives a string of `length` bytes, which it holds during the call.
        unsafe { std::slice::from_raw_parts(host.data, host.length) }
    };
    state.establish(id, Params::Listen { host, port });
    state.wake(id);
    Status::GOOD.0
}

/// The hook of `sendWithConnection`: takes `buffer`, and queues it while the
/// connection is open. A send past [`SENDS`] closes the connection.
///
/// # Safety
///
/// `state` is the state of `shim_cm_new`, and `buffer` points at a buffer of
/// `allocNetworkBuffer`, which the hook takes.
unsafe extern "C" fn send(state: *mut c_void, id: usize, buffer: *mut Bytes) -> u32 {
    // SAFETY: C passes the state of `shim_cm_new`.
    let state = unsafe { self::state(state) };
    let empty = Bytes {
        length: 0,
        data: ptr::null_mut(),
    };
    // SAFETY: C gives up the buffer, also on a failure, and reads the empty one back.
    let buffer = Buffer(unsafe { ptr::replace(buffer, empty) });
    let mut table = state.table.borrow_mut();
    let Some(connection) = table.get_mut(&id) else {
        return Status::BAD_CONNECTION_CLOSED.0;
    };
    if !matches!(connection.stream, Stream::Open(_)) {
        return Status::BAD_CONNECTION_CLOSED.0;
    }
    if connection.sends.len() == SENDS {
        drop(table);
        state.warn(id, format_args!("{SENDS} sends wait, so it closes"));
        state.end(id);
        return Status::BAD_CONNECTION_CLOSED.0;
    }
    connection.sends.push_back(buffer);
    drop(table);
    state.wake(id);
    Status::GOOD.0
}

/// The hook of `closeConnection`: stops reads, writes what waits, and gives `CLOSING`
/// at the next run of the loop.
///
/// # Safety
///
/// `state` is the state of `shim_cm_new`.
unsafe extern "C" fn close(state: *mut c_void, id: usize) -> u32 {
    // SAFETY: C passes the state of `shim_cm_new`.
    let state = unsafe { self::state(state) };
    let open = state
        .table
        .borrow()
        .get(&id)
        .is_some_and(|c| c.callback.is_some());
    if !open {
        return Status::BAD_NOT_FOUND.0;
    }
    state.end(id);
    Status::GOOD.0
}

/// The delayed callback that gives each queued `CLOSING`.
///
/// # Safety
///
/// `application` is the state of the manager.
unsafe extern "C" fn closed(application: *mut c_void, _: *mut c_void) {
    // SAFETY: `new` sets the application to the state.
    let state = unsafe { self::state(application) };
    state.queued.set(false);
    loop {
        let Some(id) = state.ends.borrow_mut().pop_front() else {
            break;
        };
        let callback = state
            .table
            .borrow_mut()
            .get_mut(&id)
            .and_then(|c| c.callback.take())
            .expect("invariant: a connection keeps its callback until its `CLOSING`");
        callback.call(state.raw.get(), id, ffi::CLOSING, &mut []);
        state.wake(id);
    }
}

#[cfg(test)]
mod tests;
