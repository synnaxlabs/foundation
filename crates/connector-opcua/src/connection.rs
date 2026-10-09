//! The TCP connection manager of a loop. open62541 opens, writes, and closes its
//! connections through it, and each connection is an `env::net` stream that
//! [`Manager::drive`] drives.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::cell::{Cell, RefCell, UnsafeCell};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::fmt;
use std::future::poll_fn;
use std::io::IoSlice;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::task::{Context, Poll, Waker};

use env::clock::{Clock, Sleep};
use env::net::{self, Net, Tcp, tcp};
use env::rng::Rng;
use types::time::{Monotonic, Span};

use crate::event::Loop;
use crate::ffi::{self, Bytes, Status};

const OPTIONS: tcp::Options = tcp::Options {
    send_buffer_bytes: 1 << 16,
    recv_buffer_bytes: 1 << 16,
    unsent_bytes_max: NonZeroUsize::new(1 << 14).expect("invariant: 2^14 is not 0"),
    delayed: false,
};

/// The size of the read buffer of each connection.
const READ_BYTES: usize = 1 << 16;

/// The most sends that wait on one connection. One write takes them all. A send past
/// it closes the connection.
const SENDS: usize = 16;

/// How long a closed connection may write what waits before it drops its stream.
const LINGER: Span = Span::from_nanos(10_000_000_000);

static HOOKS: ffi::Hooks = ffi::Hooks { open, send, close };

/// A TCP connection manager and the loop it is linked into, on the thread that made
/// it. Delete each client and server on its loop before it drops.
pub(crate) struct Manager {
    state: NonNull<State>,
    events: Loop,
}

impl Manager {
    /// Makes a loop on `clock` and `rng`, as [`Loop::new`] does, and a manager that
    /// connects through `net`, linked first into the event sources of the loop, where
    /// a client finds it.
    ///
    /// # Panics
    ///
    /// When a C allocation fails.
    pub(crate) fn new(clock: Clock, net: Net, rng: &mut Rng) -> Self {
        let events = Loop::new(Clock::clone(&clock), rng);
        let state = NonNull::from(Box::leak(Box::new(State {
            net,
            clock,
            raw: Cell::new(ptr::null_mut()),
            events: events.raw(),
            table: RefCell::new(BTreeMap::new()),
            next: Cell::new(1),
            waker: RefCell::new(None),
            driving: Cell::new(false),
            ahead: Cell::new(usize::MAX),
            again: Cell::new(false),
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
    /// server. Between calls, the drive sleeps until the next timer of the loop or a
    /// wake, also from a send or a close that `run` or another task asks for.
    pub(crate) async fn drive<T>(&self, mut run: impl FnMut() -> Option<T>) -> T {
        let state = self.state();
        let mut sleep: Option<(Monotonic, Sleep)> = None;
        poll_fn(|cx| {
            state.driving.set(true);
            let poll = loop {
                state.again.set(false);
                self.pass(cx);
                if let Some(value) = run() {
                    break Poll::Ready(value);
                }
                if state.again.get() {
                    continue;
                }
                let Some(next) = self.events.next() else {
                    break Poll::Pending;
                };
                if sleep.as_ref().is_none_or(|(at, _)| *at != next) {
                    sleep = Some((next, state.clock.sleep_until(next)));
                }
                let (_, timer) = sleep.as_mut().expect("invariant: set above");
                match Pin::new(timer).poll(cx) {
                    Poll::Ready(()) => sleep = None,
                    Poll::Pending => break Poll::Pending,
                }
            };
            state.driving.set(false);
            poll
        })
        .await
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
            state.drive(id, cx);
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
    /// The first key that the running pass has not reached, or `usize::MAX` outside a
    /// pass.
    ahead: Cell<usize>,
    /// Whether the drive passes again before it sleeps.
    again: Cell<bool>,
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
            if id < self.ahead.get() {
                self.again.set(true);
            }
        } else if let Some(waker) = self.waker.borrow().as_ref() {
            waker.wake_by_ref();
        }
    }

    /// Logs `what` of connection `id` as a warning through the logger of the loop.
    fn warn(&self, id: usize, what: fmt::Arguments<'_>) {
        let message = format!("connection {id}: {what}");
        // SAFETY: the loop lives, and the call reads the bytes only during it.
        unsafe { ffi::shim_log_warning(self.events, message.as_ptr(), message.len()) };
    }

    /// Queues the `CLOSING` of `id` for the next run of the loop, once, and starts
    /// its linger.
    fn end(&self, id: usize) {
        {
            let mut table = self.table.borrow_mut();
            let Some(connection) = table.get_mut(&id) else {
                return;
            };
            if connection.closing {
                return;
            }
            connection.closing = true;
            match connection.stream {
                Stream::Connecting(_) => connection.stream = Stream::Closed,
                Stream::Open(_) => {
                    let until = self.clock.now() + LINGER;
                    connection.linger = Some(self.clock.sleep_until(until));
                }
                Stream::Closed => {}
            }
        }
        self.ends.borrow_mut().push_back(id);
        if !self.queued.replace(true) {
            // SAFETY: the loop lives, and the callback is not in its queue.
            unsafe { (self.members().add_delayed)(self.events, self.closed.get()) };
        }
        self.wake(id);
    }

    /// Moves connection `id` on until it waits, and calls C with no borrow held.
    fn drive(&self, id: usize, cx: &mut Context<'_>) {
        loop {
            let mut failure = None;
            let step = match self.table.borrow_mut().get_mut(&id) {
                Some(connection) => connection.step(cx).unwrap_or_else(|e| {
                    failure = Some(e);
                    connection.drop_stream();
                    Step::Ended
                }),
                None => return,
            };
            if let Some((during, e)) = failure {
                self.warn(id, format_args!("the {during} failed: {e}"));
            }
            match step {
                Step::Waiting => return,
                Step::Ended => self.end(id),
                Step::Gone => {
                    self.table.borrow_mut().remove(&id);
                    return;
                }
                Step::Established => self.call(id, ffi::ESTABLISHED, &mut []),
                Step::Read(mut buffer, n) => {
                    self.call(id, ffi::ESTABLISHED, &mut buffer[..n]);
                    if let Some(connection) = self.table.borrow_mut().get_mut(&id) {
                        connection.buffer = buffer;
                    }
                }
            }
        }
    }

    /// Calls the connection callback of `id` with `state` and `message`.
    fn call(&self, id: usize, state: ffi::ConnectionState, message: &mut [u8]) {
        let callback = self.table.borrow().get(&id).and_then(|c| c.callback);
        let Some(mut callback) = callback else { return };
        callback.call(self.raw.get(), id, state, message);
        if let Some(connection) = self.table.borrow_mut().get_mut(&id)
            && let Some(slot) = connection.callback.as_mut()
        {
            slot.context = callback.context;
        }
    }
}

/// The connection callback of open62541 for one connection, with its arguments.
#[derive(Clone, Copy)]
struct Callback {
    application: *mut c_void,
    context: *mut c_void,
    function: ffi::ConnectionCallback,
}

impl Callback {
    /// Calls the callback, which may write `context`.
    fn call(
        &mut self,
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
                &raw mut self.context,
                state,
                &raw const params,
                message,
            );
        }
    }
}

enum Stream {
    Connecting(Pin<Box<dyn Future<Output = Result<Tcp, net::Error>>>>),
    Open(Tcp),
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
    /// It gives no more reads, and closes once its sends are written.
    closing: bool,
    /// When a closing connection drops its stream with sends that wait.
    linger: Option<Sleep>,
    /// Empty until the connect ends, and while a read callback holds it.
    buffer: Box<[u8]>,
}

/// What the poll of one connection asks the manager to do.
enum Step {
    Waiting,
    Established,
    Read(Box<[u8]>, usize),
    Ended,
    Gone,
}

impl Connection {
    /// Moves the connection on, or gives the step that failed and its error.
    fn step(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Result<Step, (&'static str, net::Error)> {
        if let Stream::Connecting(connect) = &mut self.stream {
            return match connect.as_mut().poll(cx) {
                Poll::Pending => Ok(Step::Waiting),
                Poll::Ready(Ok(tcp)) => {
                    self.stream = Stream::Open(tcp);
                    self.buffer = vec![0; READ_BYTES].into_boxed_slice();
                    Ok(Step::Established)
                }
                Poll::Ready(Err(e)) => Err(("connect", e)),
            };
        }
        let Stream::Open(tcp) = &mut self.stream else {
            return Ok(if self.callback.is_none() {
                Step::Gone
            } else {
                Step::Waiting
            });
        };
        if self.closing {
            if let Some(linger) = &mut self.linger
                && Pin::new(linger).poll(cx).is_ready()
            {
                self.drop_stream();
                return Ok(Step::Ended);
            }
            // A stream that drops with bytes it has not read resets, and the peer
            // loses what it has not read yet.
            loop {
                match tcp.poll_read(cx, &mut self.buffer) {
                    Poll::Ready(Ok(0)) | Poll::Pending => break,
                    Poll::Ready(Ok(_)) => {}
                    Poll::Ready(Err(e)) => return Err(("read", e)),
                }
            }
        } else if !self.buffer.is_empty() {
            match tcp.poll_read(cx, &mut self.buffer) {
                Poll::Ready(Ok(0)) => return Ok(Step::Ended),
                Poll::Ready(Ok(n)) => {
                    return Ok(Step::Read(std::mem::take(&mut self.buffer), n));
                }
                Poll::Ready(Err(e)) => return Err(("read", e)),
                Poll::Pending => {}
            }
        }
        while !self.sends.is_empty() {
            let mut parts = [IoSlice::new(&[]); SENDS];
            let mut count = 0;
            for (part, buffer) in parts.iter_mut().zip(&self.sends) {
                *part = IoSlice::new(buffer.bytes());
                count += 1;
            }
            parts[0] = IoSlice::new(&self.sends[0].bytes()[self.sent..]);
            match tcp.poll_write(cx, &parts[..count]) {
                Poll::Ready(Ok(n)) => advance(&mut self.sends, &mut self.sent, n),
                Poll::Ready(Err(e)) => return Err(("write", e)),
                Poll::Pending => return Ok(Step::Waiting),
            }
        }
        if self.closing {
            match tcp.poll_close(cx) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => return Err(("close", e)),
                Poll::Pending => return Ok(Step::Waiting),
            }
            self.drop_stream();
            return Ok(Step::Ended);
        }
        Ok(Step::Waiting)
    }

    /// Drops the stream and what waits to be written.
    fn drop_stream(&mut self) {
        self.stream = Stream::Closed;
        self.sends.clear();
        self.sent = 0;
        self.linger = None;
    }
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
    let id = state.next.get();
    state.next.set(id + 1);
    let callback = Callback {
        application,
        context,
        function: callback,
    };
    state.table.borrow_mut().insert(
        id,
        Connection {
            callback: Some(callback),
            stream: Stream::Connecting(connect),
            sends: VecDeque::new(),
            sent: 0,
            closing: false,
            linger: None,
            buffer: Box::default(),
        },
    );
    state.call(id, ffi::OPENING, &mut []);
    state.wake(id);
    Status::GOOD.0
}

/// The hook of `sendWithConnection`: takes `buffer`, and queues it while the
/// connection is open. A send past [`SENDS`] closes the connection.
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
    if connection.closing || !matches!(connection.stream, Stream::Open(_)) {
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
            .and_then(|c| c.callback.take());
        if let Some(mut callback) = callback {
            callback.call(state.raw.get(), id, ffi::CLOSING, &mut []);
        }
        state.wake(id);
    }
}

#[cfg(test)]
mod tests;
