//! A client of an OPC UA server, on its own loop and connection manager.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::ffi::{CStr, CString, c_void};
use std::future;
use std::marker::PhantomData;
use std::mem;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::pin::pin;
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::task::Poll;

use env::clock::Clock;
use env::net::Net;
use env::rng::Rng;
use types::time::{Monotonic, Span, Stamp};

use crate::alloc;
use crate::connection::{CHUNKS, Manager, SENDS};
use crate::event::Time;
use crate::ffi::{self, Status};
use crate::node::{Key, Node};
use crate::value::{self, Fault, Value};

pub(crate) mod browse;
mod service;
#[cfg(test)]
pub(crate) mod tests;

use service::{Bytes, Item, Request, Sizes};

/// The most requests of a client in flight. A request past it waits until one ends.
const READS: u16 = 9;

/// The most intermediate chunks that a client queues across its answers: each of
/// [`READS`] answers waits for its final chunk with at most [`CHUNKS`] - 1 chunks. A
/// chunk past it closes the channel.
const QUEUED: u32 = READS as u32 * (CHUNKS - 1);

const _: () = assert!(
    QUEUED <= 63,
    "OPCUA READ bounds the queue of a channel at 63 chunks of 64 KiB, about 4 MiB"
);

/// The most time with no read that [`Client::idle`] waits before it checks the
/// connection.
pub(crate) const CHECK: Span = Span::from_nanos(10 * Span::SECOND.nanos());

/// The state of the server, which [`Client::idle`] reads as its check.
const STATE: Node = Node {
    namespace: 0,
    key: Key::Number(ffi::SERVER_STATE),
};

/// The most time that [`Client::close`] waits for the server.
pub(crate) const GRACE: Span = Span::SECOND;

/// The most bytes that open62541 leaves empty at the end of a chunk of a Read but the
/// last. It moves a value of a fixed size whole to the next chunk and splits a string,
/// so this is 1 less than the largest value of a fixed size in a Read today, a Double
/// or a 64-bit integer (8 bytes). A GUID key (19 bytes) would need 18.
const SPLIT: usize = 7;

/// The value of a slot that no read has written yet. No read gives it: a read ends only
/// when each of its requests has its answer, which writes each of its values.
const UNREAD: Result<Value, Fault> = Err(Fault::status(Status::BAD_UNEXPECTED_ERROR));

const _: () = assert!(
    READS as usize * CHUNKS as usize <= SENDS / 2,
    "the other half of the sends of a connection is for the requests of open62541"
);

/// Gives a client of open62541 on the loop `el` with no connection. It takes at most
/// [`CHUNKS`] chunks in a message and queues at most [`QUEUED`] intermediate chunks.
///
/// # Safety
///
/// `el` is a live loop that outlives the client.
///
/// # Aborts
///
/// When open62541 cannot allocate the client: the process stops as the Rust allocator
/// does when it has no memory.
pub(crate) unsafe fn unconnected(el: *mut ffi::EventLoop) -> NonNull<ffi::Client> {
    // SAFETY: as the caller ensures.
    let client = unsafe { ffi::shim_client_new(el, CHUNKS, QUEUED) };
    NonNull::new(client).unwrap_or_else(|| alloc::exhausted())
}

/// The certificate and private key of a peer, each DER or PEM, and its `trust`: DER
/// certificates back to back, or PEM certificates with any text around them. In PEM,
/// all but a block that aws-lc reads as a certificate is text, also DER bytes and a
/// block with a damaged `BEGIN` line. A client offers each of the three policies of
/// the copy on them, with the URI of `certificate` as its application URI.
///
/// # Errors
///
/// [`Client::connect`] gives `BadCertificateInvalid` when `certificate` does not parse
/// or holds no RSA key, `BadInvalidArgument` when `key` does not parse,
/// `BadSecurityChecksFailed` when they do not match, `BadCertificateUriInvalid` when
/// `certificate` holds no URI, and `BadInternalError` when `trust` holds no
/// certificate, a certificate of it does not parse, a byte follows the certificate in
/// a PEM block (also trust settings), or a byte follows its last DER certificate.
pub(crate) struct Credentials<'a> {
    pub(crate) certificate: &'a [u8],
    pub(crate) key: &'a [u8],
    pub(crate) trust: &'a [u8],
}

impl<'a> Credentials<'a> {
    /// Gives the `shim_security` of the credentials, which borrows their bytes.
    pub(crate) fn raw(&self) -> ffi::Security<'a> {
        ffi::Security {
            certificate: ffi::Bytes::of(self.certificate),
            key: ffi::Bytes::of(self.key),
            trust: ffi::Bytes::of(self.trust),
            borrowed: PhantomData,
        }
    }
}

/// The mode of a secure channel. open62541 takes the endpoint in the mode of the
/// highest security level that the server offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the config gives the mode in PR 2b of #2934")
)]
pub(crate) enum Mode {
    Sign,
    SignAndEncrypt,
}

impl From<Mode> for ffi::Mode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Sign => Self::SIGN,
            Mode::SignAndEncrypt => Self::SIGN_AND_ENCRYPT,
        }
    }
}

/// A client of open62541 with an active session, on its own loop and connection
/// manager, on the thread that made it. Its loop runs only in its calls, so a client
/// that waits outside [`Client::idle`] for longer than the channel lifetime (often 10
/// minutes) or the session timeout of its server loses its connection or its session,
/// and each later read fails. A drop closes its connection and waits for nothing, but
/// leaves its session on the server until the session times out; a [`Client::close`]
/// closes the session too.
pub(crate) struct Client {
    raw: NonNull<ffi::Client>,
    /// C holds a pointer to it until `raw` is deleted, which calls the callback of each
    /// request in flight.
    calls: Box<Calls>,
    /// The values of the last read, which [`Client::read`] gives. It swaps them with
    /// [`Reads::values`], so neither grows past the most nodes of a read.
    values: Vec<Result<Value, Fault>>,
    clock: Clock,
    /// The request timeout of the client.
    timeout: Span,
    /// When the last read of the client started.
    asked: Monotonic,
    /// The most bytes of the body of a message that the client sends.
    sends: usize,
    /// The most bytes of the body of a message that the client takes.
    takes: usize,
    /// The encoded bytes of the session token.
    token: usize,
    /// The `MaxNodesPerRead` of the server, the most nodes of one Read request.
    per_read: Option<NonZeroUsize>,
    /// Tells the [`Nodes`] that this client checked from those of another.
    key: Rc<()>,
    /// The supertype of each data type that the client browsed, or `None` when it has
    /// none.
    supertypes: BTreeMap<Node, Option<Node>>,
    /// The bounds of the server on a Browse and a Translate, once
    /// [`Client::describe`] reads them.
    bounds: Option<browse::Bounds>,
    /// The endpoint that [`Client::open`] connects to, which each failure names.
    url: CString,
    /// Drops after `raw` is deleted, as it holds the loop of the client.
    connections: Manager,
}

/// The requests of a client in flight, shared with the callbacks.
#[derive(Default)]
struct Calls {
    /// The requests that open62541 took and has not called back, of each read and
    /// service, at most [`READS`].
    pending: Cell<u16>,
    reads: Reads,
    service: service::Slot,
}

/// The reads of a client in flight.
#[derive(Default)]
struct Reads {
    /// The requests of the last read that open62541 took and has not answered. The
    /// callback drops the answer of another request, so a read clears them when it
    /// starts, and the first failed answer clears them to drop each later answer.
    requests: RefCell<Vec<Flight>>,
    /// The status of the first failed answer of the last read.
    failed: Cell<Option<Status>>,
    /// The values of the last read, in node order.
    values: Cell<Vec<Result<Value, Fault>>>,
}

/// A request of a read in flight.
#[derive(Clone, Copy)]
struct Flight {
    /// The number that open62541 gave it.
    key: u32,
    slots: Slots,
}

/// The nodes of one request of a read: the index of the first in the list, and their
/// count.
#[derive(Clone, Copy)]
struct Slots {
    first: usize,
    count: usize,
}

impl Client {
    /// Makes a client on its own manager over `time`, `span`, `net`, and `rng`, as
    /// [`Manager::new`] does, and connects it to the server at the endpoint `url` with
    /// the credentials and mode of `security`, or the security mode `None` when it is
    /// `None`, and an anonymous
    /// session. It gives the client once the session is active and it has read the
    /// `MaxNodesPerRead` of the server, or `None` when `until` ends first.
    /// After an error, or when `until` ends first, it closes the session that the
    /// connect made, as [`Client::close`] does, so it ends within [`GRACE`] of either.
    /// A drop of the future drops the client.
    ///
    /// # Errors
    ///
    /// Before it dials, the error of [`Credentials`] when open62541 refuses them.
    /// `BadTimeout` when the session is not active after the request timeout of the
    /// client (5 s), `BadConnectionClosed` when the server closes or resets an open
    /// stream before the session is active, with no new dial after that close, the
    /// status of the error when the client cannot process a message of the server,
    /// `BadDecodingError` for an `ERR` whose code is not Bad or another status of the
    /// connect that is neither Bad nor `Good` (the code 0), such as a session answer of
    /// `GoodCallAgain`, else the status of open62541 when it refuses `url` or the
    /// connect fails, else the error of [`Client::read`] when the read of
    /// `MaxNodesPerRead` fails.
    ///
    /// # Aborts
    ///
    /// When open62541 cannot allocate the loop, the connection manager, or the
    /// client, as [`alloc::exhausted`] says.
    pub(crate) async fn connect(
        time: Time,
        span: tracing::Span,
        net: Net,
        rng: &mut Rng,
        url: &CStr,
        security: Option<(&Credentials<'_>, Mode)>,
        until: impl Future<Output = ()>,
    ) -> Result<Option<Self>, Status> {
        let mut client = Self::new(time, span, net, rng, url.to_owned());
        if let Some((credentials, mode)) = security {
            let raw = credentials.raw();
            // SAFETY: the client lives and has not connected, and it copies `raw`.
            let status = Status(unsafe {
                ffi::shim_client_secure(
                    client.raw.as_ptr(),
                    mode.into(),
                    &raw const raw,
                )
            });
            if status != Status::GOOD {
                return Err(status);
            }
        }
        let opened = {
            let (mut until, mut open) = (pin!(until), pin!(client.open()));
            // The first poll of `open` starts the loop, which the close drives.
            future::poll_fn(|cx| {
                if let Poll::Ready(opened) = open.as_mut().poll(cx) {
                    return Poll::Ready(Some(opened));
                }
                until.as_mut().poll(cx).map(|()| None)
            })
            .await
        };
        match opened {
            Some(Ok(())) => Ok(Some(client)),
            Some(Err(status)) => {
                client.close().await;
                Err(status)
            }
            None => {
                client.close().await;
                Ok(None)
            }
        }
    }

    /// Makes a client of `url` on its own manager over `time`, `span`, `net`, and
    /// `rng`, with no connection.
    ///
    /// # Aborts
    ///
    /// As [`Client::connect`].
    fn new(
        time: Time,
        span: tracing::Span,
        net: Net,
        rng: &mut Rng,
        url: CString,
    ) -> Self {
        let clock = Clock::clone(&time.clock);
        let connections = Manager::new(time, span, net, rng);
        // SAFETY: the loop outlives the client, which `drop` deletes first.
        let raw = unsafe { unconnected(connections.events().raw()) };
        // SAFETY: `raw` is a new client of the loop of `connections`.
        unsafe { Self::of(clock, connections, raw, url) }
    }

    /// The client of `raw`, as [`Client::new`] gives it.
    ///
    /// # Safety
    ///
    /// `raw` comes from [`unconnected`] on the loop of `connections`, has not
    /// connected, and nothing else owns it: the client frees it.
    unsafe fn of(
        clock: Clock,
        connections: Manager,
        raw: NonNull<ffi::Client>,
        url: CString,
    ) -> Self {
        // SAFETY: the client lives.
        let timeout_ms = unsafe { ffi::shim_client_timeout(raw.as_ptr()) };
        Self {
            raw,
            calls: Box::default(),
            values: Vec::new(),
            asked: clock.now(),
            clock,
            timeout: Span::from_nanos(i64::from(timeout_ms) * 1_000_000),
            sends: 0,
            takes: 0,
            token: 0,
            per_read: None,
            key: Rc::default(),
            supertypes: BTreeMap::new(),
            bounds: None,
            url,
            connections,
        }
    }

    /// The endpoint that the client connects to.
    pub(crate) fn url(&self) -> &CStr {
        &self.url
    }

    /// Connects the new client to its URL, as [`Client::connect`] does. The client
    /// holds the state of the connect, so the future can drop at any point, and
    /// [`Client::close`] then closes the session that it made.
    async fn open(&mut self) -> Result<(), Status> {
        let deadline = self.clock.now() + self.timeout;
        // SAFETY: the client lives, and copies the URL. The drive reads the status that
        // it gives from the state of the client.
        unsafe { ffi::UA_Client_connectAsync(self.raw.as_ptr(), self.url.as_ptr()) };
        let connected = self
            .connections
            .drive_until(Some(deadline), |_| {
                let state = self.state();
                if state.status != Status::GOOD {
                    Poll::Ready(Err(state.status))
                } else if state.session == ffi::Session::ACTIVATED {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            })
            .await
            .unwrap_or(Err(Status::BAD_TIMEOUT));
        let per_read = match connected {
            Ok(()) => {
                let mut limits = [0; 5];
                // SAFETY: the client lives.
                let status = Status(unsafe {
                    ffi::shim_client_limits(self.raw.as_ptr(), &raw mut limits)
                });
                assert_eq!(status, Status::GOOD, "open62541 gives the limits");
                let [send, receive, message, chunks, token] = limits;
                (self.sends, self.takes) =
                    (most(send, message, chunks), most(receive, message, chunks));
                self.token = token;
                self.per_read().await
            }
            Err(status) => Err(status),
        };
        self.per_read = per_read?;
        Ok(())
    }

    /// Reads the `MaxNodesPerRead` of the server, as [`limit`] gives it.
    async fn per_read(&mut self) -> Result<Option<NonZeroUsize>, Status> {
        let node = Node {
            namespace: 0,
            key: Key::Number(ffi::MAX_NODES_PER_READ),
        };
        let nodes = self.numbered(&[node]);
        Ok(limit(&self.read(&nodes).await?[0]))
    }

    /// Builds the Read requests of the Value of each node of `nodes`, for this client
    /// alone. Each request takes the next nodes in order while it has at most the
    /// `MaxNodesPerRead` of the server, and while it and a good answer with each field
    /// of a Double, the largest scalar, for each of its nodes fit one message of the
    /// session: the bytes of a chunk past its headers, signature, and padding times the
    /// smaller of [`CHUNKS`] and the `MaxChunkCount` of the server, less 7 bytes at the
    /// end of each chunk but the last, or the `MaxMessageSize` of the server when that
    /// is smaller.
    ///
    /// A node whose request alone, or its answer, does not fit one message is in no
    /// request, and is in [`Nodes::large`].
    ///
    /// # Aborts
    ///
    /// If open62541 cannot allocate a request, as [`alloc::exhausted`] says.
    pub(crate) fn nodes(&self, nodes: &[Node]) -> Nodes {
        let sizes = self.sizes(Request::values, Item::Value.bytes());
        let split = self.split(nodes.len(), self.per_read, &sizes, |index| {
            id_bytes(&nodes[index])
        });
        let fit: Vec<Node> = split
            .ranges
            .iter()
            .flat_map(|range| nodes[range.clone()].iter().cloned())
            .collect();
        let counts: Vec<usize> =
            split.ranges.iter().map(ExactSizeIterator::len).collect();
        let mut built = Nodes::new(&fit, &counts, Rc::clone(&self.key));
        built.large = split.large;
        built
    }

    /// [`Client::nodes`] of nodes by number, whose read fits one message.
    ///
    /// # Panics
    ///
    /// If a node does not fit, with its [`Large`].
    fn numbered(&self, nodes: &[Node]) -> Nodes {
        let nodes = self.nodes(nodes);
        assert!(
            nodes.large.is_empty(),
            "invariant: a read of nodes by number fits one message: {:?}",
            nodes.large
        );
        nodes
    }

    /// Splits the `count` items of a list into requests, in order. Each request takes
    /// the next items while it has at most `most` items, and while it and its answer
    /// each fit one message of the session, the request in [`Client::sends`] and the
    /// answer in [`Client::takes`]. `sizes` gives the bytes of each, and `variable` the
    /// bytes of the variable part of item `index`, such as its node id.
    fn split(
        &self,
        count: usize,
        most: Option<NonZeroUsize>,
        sizes: &Sizes,
        variable: impl Fn(usize) -> usize,
    ) -> Split {
        let past = |bytes: Bytes| {
            [(bytes.ask, self.sends), (bytes.answer, self.takes)]
                .into_iter()
                .find(|&(bytes, most)| bytes > most)
        };
        let mut split = Split {
            ranges: Vec::new(),
            large: Vec::new(),
        };
        let mut open: Option<(Range<usize>, Bytes)> = None;
        for index in 0..count {
            let item = sizes.item(variable(index));
            if let Some((range, bytes)) = &mut open {
                let full = most.is_some_and(|most| range.len() == most.get());
                if !full && past(*bytes + item).is_none() {
                    *bytes = *bytes + item;
                    range.end += 1;
                    continue;
                }
                split.ranges.push(range.clone());
            }
            let bytes = sizes.empty + item;
            open = match past(bytes) {
                Some((bytes, most)) => {
                    split.large.push(Large { index, bytes, most });
                    None
                }
                None => Some((index..index + 1, bytes)),
            };
        }
        split.ranges.extend(open.map(|(range, _)| range));
        split
    }

    /// Sets the start of a call, and drives the client until `step` is ready or the
    /// request timeout of the client (5 s) passes. It polls `step` once before the
    /// drive, so it sends its first requests before the drive moves the connections
    /// on, and a connection that closed since the last call fails them in their
    /// answers. `step` sets its flag once it sent a request.
    ///
    /// # Errors
    ///
    /// The error of `step`, else at the end of the timeout `BadTimeout` when `step`
    /// sent a request, and `BadTooManyOperations` when it did not.
    async fn call<T>(
        &mut self,
        mut step: impl FnMut(&Self, &mut bool) -> Poll<Result<T, Status>>,
    ) -> Result<T, Status> {
        self.asked = self.clock.now();
        let deadline = self.asked + self.timeout;
        let mut sent = false;
        if let Poll::Ready(done) = step(self, &mut sent) {
            return done;
        }
        let done = self
            .connections
            .drive_until(Some(deadline), |_| step(self, &mut sent))
            .await;
        match done {
            Some(done) => done,
            None if sent => Err(Status::BAD_TIMEOUT),
            None => Err(Status::BAD_TOO_MANY_OPERATIONS),
        }
    }

    /// Reads the Value of each node of `nodes`, and gives the value of each node in
    /// order: its scalar, the status of its value when that is not Good (a Good with a
    /// subcode is Good), or the type of an array or of a scalar that is not one of
    /// [`Value`]. It sends the requests of `nodes` at once, but while [`READS`] are in
    /// flight it sends the next only when one ends. A read of no nodes gives no values
    /// and sends nothing. A request of a read that fails, times out, or whose future
    /// dropped stays in flight until the server answers it or the connection closes.
    ///
    /// # Errors
    ///
    /// `BadTooManyOperations` when no request ends in the request timeout of the client
    /// (5 s) before the first request goes, as the server holds [`READS`] requests:
    /// the client sends no more until the server answers one.
    /// `BadTimeout` when a request has no answer after that timeout, the waits
    /// included. Else the first of: the status of open62541 when it refuses a request,
    /// the service result of an answer when it is Bad, or `BadUnexpectedError` when an
    /// answer has a value for more or fewer nodes than its request. A close of the
    /// channel while a request is in flight gives the status of the error that closed
    /// it, `BadDecodingError` when that error holds no Bad code, `BadConnectionClosed`
    /// after a close by the client, and `BadSecureChannelClosed` after a close or reset
    /// of the stream by the server.
    ///
    /// # Panics
    ///
    /// If another client built `nodes`.
    pub(crate) async fn read(
        &mut self,
        nodes: &Nodes,
    ) -> Result<&[Result<Value, Fault>], Status> {
        assert!(
            Rc::ptr_eq(&nodes.client, &self.key),
            "invariant: a client reads only the nodes that it checked"
        );
        if nodes.count == 0 {
            return Ok(&[]);
        }
        let reads = &self.calls.reads;
        // They can hold the requests and the values of a read that did not end.
        reads.requests.borrow_mut().clear();
        reads.failed.set(None);
        // Only a growth needs a fill: a good read writes each value.
        let mut values = reads.values.take();
        values.resize(nodes.count, UNREAD);
        reads.values.set(values);
        let mut next = 0;
        self.call(|client, sent| {
            let reads = &client.calls.reads;
            if let Some(status) = reads.failed.get() {
                return Poll::Ready(Err(status));
            }
            if let Err(status) = client.send(nodes, &mut next) {
                return Poll::Ready(Err(status));
            }
            *sent = next > 0;
            if next == nodes.parts.len() && reads.requests.borrow().is_empty() {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        })
        .await?;
        let values = self.calls.reads.values.take();
        (self.calls.reads.values).set(mem::replace(&mut self.values, values));
        Ok(&self.values)
    }

    /// Sends the requests of `nodes` from the index `sent` while fewer than [`READS`]
    /// are in flight and no answer of this read failed, and moves `sent` past each.
    fn send(&self, nodes: &Nodes, sent: &mut usize) -> Result<(), Status> {
        let calls = &self.calls;
        while let Some(&Part { raw, slots }) = nodes.parts.get(*sent)
            && calls.pending.get() < READS
            && calls.reads.failed.get().is_none()
        {
            let mut key = 0;
            // SAFETY: the client and `request` live, and `calls` outlives the client.
            let status = Status(unsafe {
                ffi::shim_client_read(
                    self.raw.as_ptr(),
                    raw.as_ptr(),
                    answer,
                    ptr::from_ref::<Calls>(calls).cast_mut().cast(),
                    &raw mut key,
                )
            });
            if status != Status::GOOD {
                return Err(status);
            }
            calls.pending.set(calls.pending.get() + 1);
            (calls.reads.requests.borrow_mut()).push(Flight { key, slots });
            *sent += 1;
        }
        Ok(())
    }

    /// Reads the current time of the server (`ns=0;i=2258`), in ns from the Unix
    /// epoch.
    ///
    /// # Errors
    ///
    /// The error of a service request, as [`Client::read`] gives it, or of a value
    /// that is not Good: `BadNoData` for none, `BadTypeMismatch` when it holds no
    /// scalar `DateTime`, and `BadOutOfRange` when a [`Stamp`] cannot hold it.
    #[cfg_attr(not(test), expect(dead_code, reason = "PR 5 of #2622 calls it"))]
    pub(crate) async fn time(&mut self) -> Result<Stamp, Status> {
        let node = Node::ns0(ffi::TIME);
        let answer = self.serve(&Request::values(&[(&node).into()])).await?;
        answer.has(1)?;
        // SAFETY: the value lives with `answer`.
        unsafe { value::time(answer.value(0)) }
    }

    /// Drives the client until `until` ends, and gives its value, so the client keeps
    /// its channel and its session while it waits. When [`CHECK`] passes with no read,
    /// it reads the state of the server (`ns=0;i=2259`) to check the connection, and
    /// the next check waits [`CHECK`] from the start of that read. A check that is in
    /// flight when `until` ends stays in flight, as [`Client::read`] says.
    ///
    /// # Errors
    ///
    /// The status of the first failed check, which is the error of [`Client::read`]:
    /// the status and the value of the answer are not checked. When the client has no
    /// open channel or no active session between checks, it gives the status of the
    /// connect if it is not Good, else `BadConnectionClosed` or `BadSessionClosed`, at
    /// once.
    pub(crate) async fn idle<T>(
        &mut self,
        until: impl Future<Output = T>,
    ) -> Result<T, Status> {
        let mut until = pin!(until);
        loop {
            let deadline = self.asked + CHECK;
            let waited = self
                .connections
                .drive_until(Some(deadline), |cx| match until.as_mut().poll(cx) {
                    Poll::Ready(value) => Poll::Ready(Ok(value)),
                    Poll::Pending => self.lost().map(Err),
                })
                .await;
            if let Some(waited) = waited {
                return waited;
            }
            let nodes = self.numbered(&[STATE]);
            let mut check = pin!(self.read(&nodes));
            let checked = future::poll_fn(|cx| {
                if let Poll::Ready(value) = until.as_mut().poll(cx) {
                    return Poll::Ready(Ok(Some(value)));
                }
                check.as_mut().poll(cx).map(|read| read.map(|_| None))
            })
            .await;
            if let Some(value) = checked? {
                return Ok(value);
            }
        }
    }

    /// Ready with the cause when the client has no open channel or no active session.
    fn lost(&self) -> Poll<Status> {
        let state = self.state();
        let channel = state.channel == ffi::Channel::OPEN;
        if channel && state.session == ffi::Session::ACTIVATED {
            Poll::Pending
        } else if state.status != Status::GOOD {
            Poll::Ready(state.status)
        } else if channel {
            Poll::Ready(Status::BAD_SESSION_CLOSED)
        } else {
            Poll::Ready(Status::BAD_CONNECTION_CLOSED)
        }
    }

    /// Closes the session and the connection of the client, and waits until the
    /// connection is closed, at most [`GRACE`] after the call plus the time to close
    /// the connection. It sends a `CloseSession` and waits for its answer, but first
    /// waits for the answer to a `CreateSession` in flight. When the server does not
    /// answer in time, or the client lost its connection, the session stays on the
    /// server until it times out.
    pub(crate) async fn close(self) {
        let deadline = self.clock.now() + GRACE;
        // The server refuses a CloseSession sent before the CreateSession answer, and
        // open62541 then activates the session, which the server keeps.
        let created = self
            .connections
            .drive_until(Some(deadline), |_| {
                if self.state().session == ffi::Session::CREATE_REQUESTED {
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
        if created.is_some() {
            // SAFETY: the client lives. On a failure to send, it closes the connection.
            unsafe { ffi::UA_Client_disconnectAsync(self.raw.as_ptr()) };
            self.connections
                .drive_until(Some(deadline), |_| self.closed())
                .await;
        }
        // SAFETY: the client lives. A closed channel stays closed.
        unsafe { ffi::UA_Client_disconnectSecureChannelAsync(self.raw.as_ptr()) };
        self.connections.drive(|_| self.closed()).await;
    }

    /// Ready when the channel of the client is closed.
    fn closed(&self) -> Poll<()> {
        if self.state().channel == ffi::Channel::CLOSED {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    /// Gives the state of the client.
    fn state(&self) -> State {
        let (mut channel, mut session, mut status) =
            (ffi::Channel(0), ffi::Session(0), 0);
        // SAFETY: the client lives.
        unsafe {
            ffi::UA_Client_getState(
                self.raw.as_ptr(),
                &raw mut channel,
                &raw mut session,
                &raw mut status,
            );
        }
        State {
            channel,
            session,
            status: Status(status),
        }
    }
}

/// The requests of [`Client::split`].
struct Split {
    /// The items of each request, in order.
    ranges: Vec<Range<usize>>,
    /// Each item that no request takes, in order.
    large: Vec<Large>,
}

/// An item of a list whose request alone, or its answer, is past one message of a
/// session, such as a node of a Read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Large {
    /// The index of the item in its list.
    pub(crate) index: usize,
    /// The bytes of the body of its message, the request when it is past, else the
    /// answer.
    pub(crate) bytes: usize,
    /// The most bytes of the body of one message.
    pub(crate) most: usize,
}

/// Gives the limit of a server that `value`, a value of an operation limit, gives: a
/// `UInt32` above 0, else none, also for a fault, as a server can lack the node.
fn limit(value: &Result<Value, Fault>) -> Option<NonZeroUsize> {
    match value {
        Ok(Value::U32(most)) => NonZeroUsize::new(
            usize::try_from(*most).expect("invariant: a usize holds a u32"),
        ),
        _ => None,
    }
}

/// Gives the most bytes of the body of one message of a Read in chunks that each hold
/// `payload` bytes of it, with the `MaxMessageSize` `message` and the `MaxChunkCount`
/// `chunks` of the server, each 0 for none.
fn most(payload: usize, message: usize, chunks: usize) -> usize {
    let local = usize::try_from(CHUNKS).expect("invariant: a usize holds CHUNKS");
    let chunks = if chunks == 0 {
        local
    } else {
        chunks.min(local)
    };
    let most = chunks * payload - (chunks - 1) * SPLIT;
    if message == 0 {
        most
    } else {
        most.min(message)
    }
}

/// The state of a client.
struct State {
    channel: ffi::Channel,
    session: ffi::Session,
    /// The connect status.
    status: Status,
}

impl Drop for Client {
    fn drop(&mut self) {
        // SAFETY: the client lives, and nothing else holds it.
        unsafe { ffi::shim_client_free(self.raw.as_ptr()) };
    }
}

/// Gives the bytes of the `NodeId` of `node` in the shortest binary form, the form that
/// open62541 encodes.
fn id_bytes(node: &Node) -> usize {
    match &node.key {
        Key::Number(key) if node.namespace == 0 && *key <= 0xFF => 2,
        Key::Number(key) if node.namespace <= 0xFF && *key <= 0xFFFF => 4,
        Key::Number(_) => 7,
        Key::Name(name) => 7 + name.as_str().len(),
    }
}

/// The Read requests of a list of nodes, built once. It holds at most one request per
/// node: the count of nodes, the `MaxNodesPerRead` of the server, and the message sizes
/// of the session set its length.
pub(crate) struct Nodes {
    /// Each request, in node order.
    parts: Vec<Part>,
    count: usize,
    /// Each node in no request, in order.
    large: Vec<Large>,
    /// The [`Client::key`] of the client that checked it.
    client: Rc<()>,
}

/// A Read request of a [`Nodes`].
#[derive(Clone, Copy)]
struct Part {
    raw: NonNull<ffi::ReadRequest>,
    slots: Slots,
}

impl Nodes {
    /// As [`Nodes::new`] with one request, for a bench: [`Client::read`] panics on it.
    #[cfg(feature = "sim")]
    pub(crate) fn unchecked(nodes: &[Node]) -> Self {
        Self::new(nodes, &[nodes.len()], Rc::default())
    }

    /// Builds the Read requests of the Value of each node of `nodes`, in order, for the
    /// client of `client`: one for each count of `counts`, which sum to the count of
    /// `nodes`. It checks no size: [`Client::nodes`] does.
    ///
    /// # Aborts
    ///
    /// If open62541 cannot allocate a request, as [`alloc::exhausted`] says.
    fn new(nodes: &[Node], counts: &[usize], client: Rc<()>) -> Self {
        let raw: Vec<ffi::Node<'_>> = nodes.iter().map(Into::into).collect();
        let mut first = 0;
        let parts = counts
            .iter()
            .map(|&count| {
                let part = &raw[first..first + count];
                // SAFETY: `part` holds `count` nodes, and C copies each name.
                let request = unsafe { ffi::shim_read_new(part.as_ptr(), count) };
                let Some(request) = NonNull::new(request) else {
                    alloc::exhausted()
                };
                let slots = Slots { first, count };
                first += count;
                Part {
                    raw: request,
                    slots,
                }
            })
            .collect();
        assert_eq!(first, nodes.len(), "invariant: the counts take each node");
        Self {
            parts,
            count: nodes.len(),
            large: Vec::new(),
            client,
        }
    }

    /// Gives each node of the list of [`Client::nodes`] that is in no request, as its
    /// read does not fit one message, in order.
    pub(crate) fn large(&self) -> &[Large] {
        &self.large
    }

    /// Gives the count of nodes of each request, in order.
    #[cfg(test)]
    fn counts(&self) -> Vec<usize> {
        self.parts.iter().map(|part| part.slots.count).collect()
    }

    /// Gives the one request of a [`Nodes::unchecked`], which lives while `self` does.
    ///
    /// # Panics
    ///
    /// Unless `self` has exactly one request.
    #[cfg(feature = "sim")]
    pub(crate) fn raw(&self) -> *mut ffi::ReadRequest {
        let [part] = self.parts[..] else {
            panic!(
                "invariant: a bench reads a list of one request, not {}",
                self.parts.len()
            );
        };
        part.raw.as_ptr()
    }
}

impl Drop for Nodes {
    fn drop(&mut self) {
        for part in &self.parts {
            // SAFETY: the request lives, and nothing else holds it.
            unsafe { ffi::shim_read_free(part.raw.as_ptr()) };
        }
    }
}

/// Ends one read request of the [`Calls`] at `data`. When it is a request of the last
/// read, puts the values of the answer in its [`Reads`], or ends that read with its
/// failed status.
///
/// # Safety
///
/// `data` points at live [`Calls`], and `response` at the answer.
unsafe extern "C" fn answer(
    _: *mut ffi::Client,
    data: *mut c_void,
    request: u32,
    response: *mut ffi::ReadResponse,
) {
    // SAFETY: the client that holds the calls lives.
    let calls = unsafe { &*data.cast_const().cast::<Calls>() };
    calls.pending.set(calls.pending.get() - 1);
    let reads = &calls.reads;
    let mut requests = reads.requests.borrow_mut();
    let Some(index) = requests.iter().position(|sent| sent.key == request) else {
        return;
    };
    let Slots { first, count } = requests.swap_remove(index).slots;
    // SAFETY: open62541 gives a live response.
    let mut status = Status(unsafe { ffi::shim_response_result(response.cast()) });
    // SAFETY: open62541 gives a live response.
    if status == Status::GOOD && unsafe { ffi::shim_read_count(response) } != count {
        status = Status::BAD_UNEXPECTED_ERROR;
    }
    if status != Status::GOOD {
        requests.clear();
        reads.failed.set(Some(status));
        return;
    }
    let mut values = reads.values.take();
    for (index, value) in values[first..first + count].iter_mut().enumerate() {
        // SAFETY: the response lives and has more results than `index`.
        let result = unsafe { ffi::shim_read_result(response, index) };
        // SAFETY: the value lives with the response.
        *value = unsafe { Value::of(result) };
    }
    reads.values.set(values);
}

/// Whether open62541 reads `url` as an endpoint URL with a host.
pub(crate) fn endpoint(url: &str) -> bool {
    let url = ffi::Bytes {
        length: url.len(),
        data: url.as_ptr().cast_mut(),
    };
    let empty = || ffi::Bytes {
        length: 0,
        data: ptr::null_mut(),
    };
    let (mut host, mut path, mut port) = (empty(), empty(), 0);
    // SAFETY: `url` points at `length` bytes that live through the call, which only
    // reads them. It sets `host` and `path` to point into them, and they are not
    // freed.
    let status = unsafe {
        ffi::UA_parseEndpointUrl(
            &raw const url,
            &raw mut host,
            &raw mut port,
            &raw mut path,
        )
    };
    Status(status) == Status::GOOD && host.length > 0
}
