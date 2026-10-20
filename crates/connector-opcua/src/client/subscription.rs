//! The subscription of a client: its monitored items and their value changes.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::future;
use std::mem;
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;

use types::time::{Span, Stamp};

use super::service::{Item, Request};
use super::{Calls, Client, PUBLISHES, READS, id_bytes};
use crate::ffi::{self, Status};
use crate::node::Node;
use crate::value::{self, Fault, Value};

/// A value change of a monitored item.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Change {
    /// The index of the node of the item in the list of [`Client::subscribe`].
    pub(crate) index: usize,
    /// The value, as [`Client::read`] gives it.
    pub(crate) value: Result<Value, Fault>,
    /// The source timestamp, or `None` when it is absent or a [`Stamp`] cannot hold it.
    pub(crate) source: Option<Stamp>,
    /// The server timestamp, as `source`.
    pub(crate) server: Option<Stamp>,
    /// The server dropped one or more values of the item before this one, as its
    /// queue was full.
    pub(crate) overflow: bool,
}

/// What the server gave for a subscription.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Revised {
    /// The publishing interval.
    pub(crate) interval: Span,
    /// For each node, in order, its item, or the status of its create result.
    pub(crate) items: Vec<Result<Sampled, Status>>,
}

/// A monitored item that the server made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sampled {
    /// The sampling interval.
    pub(crate) sampling: Span,
    /// The size of the queue.
    pub(crate) queue: u32,
}

/// The shortest publishing interval of a subscription.
const PUBLISHING: Span = Span::from_nanos(100_000_000);

/// The longest publishing interval of a subscription, and the time that the client asks
/// the server to hold a Publish request with no change at most.
const KEEP_ALIVE: Span = Span::MINUTE;

/// The longest time that the server may hold a Publish request with no change: half of
/// the 10 min timeout of a Publish request in open62541.
const SILENCE: Span = Span::from_nanos(5 * Span::MINUTE.nanos());

/// The changes of a subscription that the callbacks gave, which [`Client::changes`]
/// takes.
#[derive(Default)]
pub(super) struct Changes {
    buffer: RefCell<Vec<Change>>,
    /// The status of the first status change, or `BadNoSubscription` when open62541
    /// deleted the subscription with none.
    ended: Cell<Option<Status>>,
}

impl Changes {
    /// Makes room in the buffer and in `given` for the changes of [`PUBLISHES`]
    /// answers of `each` changes, and in `client` for the decode of one such answer.
    ///
    /// # Safety
    ///
    /// `client` lives, and decodes no Publish answer.
    unsafe fn reserve(
        &self,
        client: *mut ffi::Client,
        given: &mut Vec<Change>,
        each: usize,
    ) {
        let count = each.saturating_mul(usize::from(PUBLISHES));
        for buffer in [&mut *self.buffer.borrow_mut(), given] {
            buffer.reserve(count.saturating_sub(buffer.len()));
        }
        // SAFETY: the caller keeps the client alive and out of a decode.
        let status = Status(unsafe { ffi::shim_client_reserve(client, each) });
        assert_eq!(
            status,
            Status::GOOD,
            "the memory of the decode of {each} changes"
        );
    }

    /// Adds `change`. The first change into the empty buffer stops the Publish
    /// requests of `client`.
    ///
    /// # Safety
    ///
    /// `client` lives.
    unsafe fn push(&self, client: *mut ffi::Client, change: Change) {
        let mut buffer = self.buffer.borrow_mut();
        if buffer.is_empty() {
            // SAFETY: the caller keeps the client alive.
            unsafe { ffi::shim_client_publishes(client, true) };
        }
        buffer.push(change);
    }

    /// Swaps the changes into `given`, which it clears first. When it gives changes,
    /// it resumes the Publish requests of `client`.
    ///
    /// # Safety
    ///
    /// `client` lives.
    unsafe fn take(&self, client: *mut ffi::Client, given: &mut Vec<Change>) {
        given.clear();
        mem::swap(given, &mut self.buffer.borrow_mut());
        if !given.is_empty() {
            // SAFETY: the caller keeps the client alive.
            unsafe { ffi::shim_client_publishes(client, false) };
        }
    }
}

/// The callbacks of the subscription of a client, which each get its [`Calls`].
pub(super) static WATCH: ffi::Watch = ffi::Watch {
    changed,
    ended,
    deleted,
};

impl Client {
    /// Creates a subscription, and a monitored item for each node of `nodes` that
    /// samples at `sampling`. It asks for a publishing interval of `sampling`, at
    /// least 100 ms and at most 1 min, and a keep-alive count of the publishing
    /// intervals in 1 min. Each item queues the values that it samples in the larger
    /// of two publishing intervals that the server gives and two of `round_trip`, the
    /// oldest dropped first. Each item
    /// asks for both timestamps and a change of the status or the value. It reads the
    /// `MaxMonitoredItemsPerCall` of the server, and sends the items in
    /// `CreateMonitoredItems` calls of at most that many. Each answer of a Publish fits
    /// the receive limit of the client, for items of a scalar of at most 8 bytes. Once
    /// the server makes the subscription, the client keeps [`PUBLISHES`] Publish
    /// requests in flight, and [`Client::read`] and the services send fewer to keep the
    /// sum from the call. Each change that comes, also while the client reads or
    /// serves, stays in the client until [`Client::changes`] gives it, with no drop.
    /// While a change waits, the client sends no Publish request, so at most the
    /// answers of the [`PUBLISHES`] requests in flight wait, and later values wait in
    /// the queues of the server. It reserves the memory of the changes of
    /// [`PUBLISHES`] answers that each hold the changes that the queues of its items
    /// hold, at most the most changes of an answer that fits, so the first drives
    /// allocate no memory for them.
    ///
    /// # Errors
    ///
    /// The error of the read of `MaxMonitoredItemsPerCall`, or of a service request, as
    /// [`Client::serve`] gives it, of the `CreateSubscription` or of a
    /// `CreateMonitoredItems` call, also a service result that is not Good. Such a
    /// result of a `CreateMonitoredItems` call goes to each item of that call, also the
    /// `BadInternalError` that open62541 gives when the answer has more or fewer
    /// results than the call. `BadRequestTooLarge` goes to a node that does not fit
    /// one call. `BadOutOfRange` when the publishing interval that the server gives is
    /// not positive, or times the larger of 1 and the keep-alive count that the server
    /// gives is over 5 min, half of the timeout of a Publish request in open62541, with
    /// no Publish request sent.
    ///
    /// # Panics
    ///
    /// When `sampling` is not positive. On a second call, also after an error, as a
    /// `CreateSubscription` that failed in the client can still make a subscription.
    pub(crate) async fn subscribe(
        &mut self,
        nodes: &[Node],
        sampling: Span,
        round_trip: Span,
    ) -> Result<Revised, Status> {
        assert!(
            sampling > Span::ZERO,
            "invariant: a sampling interval is positive"
        );
        assert_eq!(
            self.budget, READS,
            "invariant: a client holds one subscription"
        );
        self.budget = READS - PUBLISHES;
        let per_call = self.read_limit(ffi::MAX_MONITORED_ITEMS_PER_CALL).await?;
        let room = self.takes.saturating_sub(ffi::shim_publish_bytes());
        let most = (room / Item::Change.bytes()).max(1);
        let interval = sampling.clamp(PUBLISHING, KEEP_ALIVE);
        let keep_alive = u32::try_from(KEEP_ALIVE.nanos() / interval.nanos())
            .expect("invariant: at most 600 intervals of 100 ms fill 1 min");
        let request = Request::subscribe(ms(interval), keep_alive, saturate(most));
        let (id, interval, keep_alive) = self.serve(&request).await?.subscription();
        let silence = interval * f64::from(keep_alive.max(1));
        let bounded = interval > 0.0 && silence <= ms(SILENCE);
        if !bounded {
            return Err(Status::BAD_OUT_OF_RANGE);
        }
        // SAFETY: the client lives.
        unsafe { ffi::shim_client_publish_count(self.raw.as_ptr(), PUBLISHES) };
        // SAFETY: the client lives.
        unsafe { ffi::shim_client_publishes(self.raw.as_ptr(), false) };
        let sampling = ms(sampling);
        let queue = queue(interval.max(ms(round_trip)), sampling);
        let build =
            |nodes: &[ffi::Node<'_>]| Request::monitor(id, nodes, 0, sampling, queue);
        let sizes = self.sizes(build, Item::Created.bytes());
        let split = self.split(nodes.len(), per_call, &sizes, |index| {
            id_bytes(&nodes[index])
        });
        let mut items = vec![Err(Status::BAD_REQUEST_TOO_LARGE); nodes.len()];
        for range in split.ranges {
            let raw: Vec<ffi::Node<'_>> =
                nodes[range.clone()].iter().map(Into::into).collect();
            let first = u32::try_from(range.start)
                .expect("invariant: a call holds fewer than 2^32 items");
            let answer = self
                .exchange(&Request::monitor(id, &raw, first, sampling, queue))
                .await?;
            let result = answer.result();
            for (offset, item) in items[range].iter_mut().enumerate() {
                *item = result.and_then(|()| answer.created(offset)).map(
                    |(sampling, queue)| Sampled {
                        sampling: span(sampling),
                        queue,
                    },
                );
            }
        }
        let queued = (items.iter().flatten())
            .map(|item| usize::try_from(item.queue.max(1)).unwrap_or(usize::MAX))
            .fold(0, usize::saturating_add);
        // SAFETY: the client lives, and decodes no answer between drives.
        unsafe {
            (self.calls.changes).reserve(
                self.raw.as_ptr(),
                &mut self.changes,
                most.min(queued),
            );
        }
        Ok(Revised {
            interval: span(interval),
            items,
        })
    }

    /// Drives the client as [`Client::idle`] does until a change or the end of the
    /// subscription comes, or `until` ends. It gives each change since the last call,
    /// in the order the server sent them, or none when `until` ended first.
    ///
    /// # Errors
    ///
    /// The errors of [`Client::idle`]. Once no change is left: the status of the
    /// first status change of the subscription, which can be Good, such as
    /// `GoodSubscriptionTransferred`, `BadTooManyPublishRequests` when the server
    /// refuses a Publish request while the client keeps 1 in flight, or
    /// `BadNoSubscription` when open62541 deleted it with no status change.
    pub(crate) async fn changes(
        &mut self,
        until: impl Future<Output = ()>,
    ) -> Result<&[Change], Status> {
        let calls = Rc::clone(&self.calls);
        let mut until = pin!(until);
        self.idle(future::poll_fn(|cx| {
            let changes = &calls.changes;
            if !changes.buffer.borrow().is_empty() || changes.ended.get().is_some() {
                Poll::Ready(())
            } else {
                until.as_mut().poll(cx)
            }
        }))
        .await?;
        let changes = &self.calls.changes;
        // SAFETY: the client lives.
        unsafe { changes.take(self.raw.as_ptr(), &mut self.changes) };
        match changes.ended.get() {
            Some(status) if self.changes.is_empty() => Err(status),
            _ => Ok(&self.changes),
        }
    }
}

/// Gives `span` in ms.
#[expect(
    clippy::cast_precision_loss,
    reason = "a span past 2^53 ns, 104 days, loses only some ns"
)]
fn ms(span: Span) -> f64 {
    span.nanos() as f64 / 1e6
}

/// Gives the span of `ms` ms: 0 for NaN, and the nearest end for one out of range.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the cast saturates, as the doc says"
)]
fn span(ms: f64) -> Span {
    Span::from_nanos((ms * 1e6) as i64)
}

/// Gives the size of a queue that holds the values sampled every `sampling` ms in
/// twice `span` ms: 0 for NaN, and `u32::MAX` when it is past.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the cast saturates, as the doc says"
)]
fn queue(span: f64, sampling: f64) -> u32 {
    (2.0 * (span / sampling).ceil()) as u32
}

/// Gives `count`, or `u32::MAX` when it is past.
fn saturate(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// Adds a change of the item with the client handle `item` to the [`Changes`] of the
/// [`Calls`] at `context`.
///
/// # Safety
///
/// `client` and the [`Calls`] at `context` live, and `value` points at the value of the
/// change.
unsafe extern "C" fn changed(
    client: *mut ffi::Client,
    _: u32,
    context: *mut c_void,
    _: u32,
    item: *mut c_void,
    value: *mut ffi::DataValue,
) {
    // SAFETY: the client that holds the calls lives.
    let calls = unsafe { &*context.cast_const().cast::<Calls>() };
    let (mut source, mut server) = (0, 0);
    // SAFETY: the value lives, and `source` and `server` are places to write.
    let overflow =
        unsafe { ffi::shim_value_times(value, &raw mut source, &raw mut server) };
    // SAFETY: the value lives.
    let value = unsafe { Value::of(value) };
    let change = Change {
        index: item.addr(),
        value,
        source: value::stamp(source),
        server: value::stamp(server),
        overflow,
    };
    // SAFETY: the client lives.
    unsafe { calls.changes.push(client, change) };
}

/// Keeps the status of `change` in the [`Changes`] of the [`Calls`] at `context`,
/// unless they hold one.
///
/// # Safety
///
/// `context` points at live [`Calls`], and `change` at the change.
unsafe extern "C" fn ended(
    _: *mut ffi::Client,
    _: u32,
    context: *mut c_void,
    change: *mut ffi::StatusChange,
) {
    // SAFETY: the client that holds the calls lives.
    let calls = unsafe { &*context.cast_const().cast::<Calls>() };
    // SAFETY: the change lives.
    let status = Status(unsafe { ffi::shim_status_change(change) });
    let ended = &calls.changes.ended;
    ended.set(Some(ended.get().unwrap_or(status)));
}

/// Sets `BadNoSubscription` in the [`Changes`] of the [`Calls`] at `context`, unless
/// they hold a status.
///
/// # Safety
///
/// `context` points at live [`Calls`].
unsafe extern "C" fn deleted(_: *mut ffi::Client, _: u32, context: *mut c_void) {
    // SAFETY: the client that holds the calls lives.
    let calls = unsafe { &*context.cast_const().cast::<Calls>() };
    let ended = &calls.changes.ended;
    ended.set(Some(ended.get().unwrap_or(Status::BAD_NO_SUBSCRIPTION)));
}
