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

/// The changes of a subscription that the callbacks gave, which [`Client::changes`]
/// takes.
#[derive(Default)]
pub(super) struct Changes {
    buffer: RefCell<Vec<Change>>,
    /// The status of the first status change, or `BadNoSubscription` when open62541
    /// deleted the subscription with none.
    ended: Cell<Option<Status>>,
}

/// The callbacks of the subscription of a client, which each get its [`Calls`].
pub(super) static WATCH: ffi::Watch = ffi::Watch {
    changed,
    ended,
    deleted,
};

impl Client {
    /// Creates a subscription at the publishing interval `interval`, and a monitored
    /// item for each node of `nodes` that samples at `sampling` and queues `queue`
    /// values, the oldest dropped first. Each item asks for both timestamps and a
    /// change of the status or the value. It reads the `MaxMonitoredItemsPerCall` of the
    /// server, and sends the items in `CreateMonitoredItems` calls of at most that
    /// many. Each answer of
    /// a Publish fits the receive limit of the client, for items of a scalar of at
    /// most 8 bytes. From the call, the client keeps [`PUBLISHES`] Publish requests in
    /// flight, and [`Client::read`] and the services send fewer to keep the sum.
    ///
    /// # Errors
    ///
    /// The error of the read of `MaxMonitoredItemsPerCall`, or of a service request, as
    /// [`Client::serve`] gives it, of the `CreateSubscription` or of a
    /// `CreateMonitoredItems` call. A Bad service result of a
    /// `CreateMonitoredItems` call goes to each item of that call, also the
    /// `BadInternalError` that open62541 gives when the answer has more or fewer
    /// results than the call. `BadRequestTooLarge` goes to a node that does not fit
    /// one call.
    ///
    /// # Panics
    ///
    /// On a second call, also after an error, as a `CreateSubscription` that failed in
    /// the client can still make a subscription.
    #[cfg_attr(not(test), expect(dead_code, reason = "PR 5 of #2622 calls it"))]
    pub(crate) async fn subscribe(
        &mut self,
        interval: Span,
        nodes: &[Node],
        sampling: Span,
        queue: u32,
    ) -> Result<Revised, Status> {
        assert_eq!(
            self.budget, READS,
            "invariant: a client holds one subscription"
        );
        self.budget = READS - PUBLISHES;
        let per_call = self.read_limit(ffi::MAX_MONITORED_ITEMS_PER_CALL).await?;
        let room = self.takes.saturating_sub(ffi::shim_publish_bytes());
        let most = (room / Item::Change.bytes()).max(1);
        let request = Request::subscribe(ms(interval), saturate(most));
        let (id, interval) = self.serve(&request).await?.subscription();
        let sampling = ms(sampling);
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
    /// `GoodSubscriptionTransferred`, or `BadNoSubscription` when open62541 deleted it
    /// with no status change.
    #[cfg_attr(not(test), expect(dead_code, reason = "PR 5 of #2622 calls it"))]
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
        self.changes.clear();
        let changes = &self.calls.changes;
        mem::swap(&mut self.changes, &mut changes.buffer.borrow_mut());
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

/// Gives `count`, or `u32::MAX` when it is past.
fn saturate(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// Adds a change of the item with the client handle `item` to the [`Changes`] of the
/// [`Calls`] at `context`.
///
/// # Safety
///
/// `context` points at live [`Calls`], and `value` at the value of the change.
unsafe extern "C" fn changed(
    _: *mut ffi::Client,
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
    calls.changes.buffer.borrow_mut().push(Change {
        index: item.addr(),
        value,
        source: value::stamp(source),
        server: value::stamp(server),
        overflow,
    });
}

/// Keeps the status of `change` in the [`Changes`] of the [`Calls`] at `context`,
/// unless they hold one, and stops the Publish requests of `client`, as the server
/// deletes the subscription.
///
/// # Safety
///
/// `client` is live, `context` points at its [`Calls`], and `change` at the change.
unsafe extern "C" fn ended(
    client: *mut ffi::Client,
    _: u32,
    context: *mut c_void,
    change: *mut ffi::StatusChange,
) {
    // SAFETY: the client that holds the calls lives.
    let calls = unsafe { &*context.cast_const().cast::<Calls>() };
    // SAFETY: the change lives.
    let status = Status(unsafe { ffi::shim_status_change(change) });
    // open62541 keeps its subscription, and sends a Publish again at each answer of
    // `BadNoSubscription`.
    // SAFETY: the client lives.
    unsafe { ffi::shim_client_publishes(client, 0) };
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
