//! The requests and answers of each service, and the sizes of their messages, for
//! [`Client::read`], the browse, and the subscription.

use std::cell::Cell;
use std::ffi::c_void;
use std::num::NonZeroU32;
use std::ptr::{self, NonNull};
use std::task::Poll;

use super::browse::{Cause, Foreign};
use super::subscription;
use super::{Calls, Client, id_bytes};
use crate::alloc;
use crate::ffi::{self, Service, Status};
use crate::node::{Key, Node};

/// A request of a service, which C owns until the drop.
pub(super) struct Request {
    raw: NonNull<c_void>,
    service: Service,
}

impl Request {
    /// A Browse request of the references of each of `nodes`: the inverse references
    /// when `inverse`, else the forward, of the type `reference` in namespace 0 and its
    /// subtypes, to the node classes of the mask `classes`, at most `most` per node.
    ///
    /// # Aborts
    ///
    /// If open62541 cannot allocate it, as [`alloc::exhausted`] says.
    pub(super) fn browse(
        nodes: &[ffi::Node<'_>],
        reference: u32,
        inverse: bool,
        classes: u32,
        most: NonZeroU32,
    ) -> Self {
        // SAFETY: `nodes` holds its count, and C copies each name.
        let raw = unsafe {
            ffi::shim_browse_new(
                nodes.as_ptr(),
                nodes.len(),
                reference,
                inverse,
                classes,
                most.get(),
            )
        };
        Self::new(raw, Service::BROWSE)
    }

    /// A `BrowseNext` request of each continuation point of `points`, which releases
    /// them when `release`.
    ///
    /// # Aborts
    ///
    /// As [`Request::browse`].
    pub(super) fn browse_next(points: &[&[u8]], release: bool) -> Self {
        let raw: Vec<ffi::Bytes> = points
            .iter()
            .map(|point| ffi::Bytes {
                length: point.len(),
                data: point.as_ptr().cast_mut(),
            })
            .collect();
        // SAFETY: `raw` holds its count of points that live through the call, and C
        // copies them and writes none.
        let raw =
            unsafe { ffi::shim_browse_next_new(raw.as_ptr(), raw.len(), release) };
        Self::new(raw, Service::BROWSE_NEXT)
    }

    /// A `TranslateBrowsePathsToNodeIds` request of the path `HasProperty` to
    /// `0:EngineeringUnits` from each of `nodes`.
    ///
    /// # Aborts
    ///
    /// As [`Request::browse`].
    pub(super) fn units(nodes: &[ffi::Node<'_>]) -> Self {
        // SAFETY: `nodes` holds its count, and C copies each name.
        let raw = unsafe { ffi::shim_units_new(nodes.as_ptr(), nodes.len()) };
        Self::new(raw, Service::TRANSLATE)
    }

    /// A Read request of the attribute `attributes[i]` of `nodes[i]` for each `i`, with
    /// no timestamps.
    ///
    /// # Aborts
    ///
    /// As [`Request::browse`].
    ///
    /// # Panics
    ///
    /// If `nodes` and `attributes` have different counts.
    pub(super) fn attributes(nodes: &[ffi::Node<'_>], attributes: &[u32]) -> Self {
        assert_eq!(
            nodes.len(),
            attributes.len(),
            "invariant: a read has one attribute for each node"
        );
        // SAFETY: `nodes` and `attributes` hold their count, and C copies each name.
        let raw = unsafe {
            ffi::shim_read_attributes_new(
                nodes.as_ptr(),
                attributes.as_ptr(),
                nodes.len(),
            )
        };
        Self::new(raw, Service::READ)
    }

    /// A Read request of the Value of each of `nodes`, with no timestamps.
    ///
    /// # Aborts
    ///
    /// As [`Request::browse`].
    pub(super) fn values(nodes: &[ffi::Node<'_>]) -> Self {
        // SAFETY: `nodes` holds its count, and C copies each name.
        let raw = unsafe { ffi::shim_read_new(nodes.as_ptr(), nodes.len()) };
        Self::new(raw.cast(), Service::READ)
    }

    /// A `CreateSubscription` request at the publishing interval `interval` ms, with at
    /// most `most` notifications in a Publish answer.
    ///
    /// # Aborts
    ///
    /// As [`Request::browse`].
    pub(super) fn subscribe(interval: f64, most: u32) -> Self {
        // SAFETY: it reads nothing.
        let raw = unsafe { ffi::shim_subscribe_new(interval, most) };
        Self::new(raw, Service::SUBSCRIBE)
    }

    /// A `CreateMonitoredItems` request on the subscription `subscription` of an item
    /// for the Value of each of `nodes`, with the client handle `first + i` for
    /// `nodes[i]`, that reports each change of the status or the value with both
    /// timestamps, samples at `sampling` ms, and queues `queue` values, the oldest
    /// dropped first.
    ///
    /// # Aborts
    ///
    /// As [`Request::browse`].
    pub(super) fn monitor(
        subscription: u32,
        nodes: &[ffi::Node<'_>],
        first: u32,
        sampling: f64,
        queue: u32,
    ) -> Self {
        // SAFETY: `nodes` holds its count, and C copies each name.
        let raw = unsafe {
            ffi::shim_monitor_new(
                subscription,
                nodes.as_ptr(),
                nodes.len(),
                first,
                sampling,
                queue,
            )
        };
        Self::new(raw, Service::MONITOR)
    }

    /// Takes `raw`, a request of `service` from C, or NULL when C could not allocate
    /// it.
    fn new(raw: *mut c_void, service: Service) -> Self {
        let Some(raw) = NonNull::new(raw) else {
            alloc::exhausted()
        };
        Self { raw, service }
    }

    /// Gives the bytes of the body of its message with a session token of `token`
    /// encoded bytes.
    pub(super) fn bytes(&self, token: usize) -> usize {
        // SAFETY: the request lives.
        unsafe { ffi::shim_request_bytes(self.raw.as_ptr(), self.service, token) }
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        // SAFETY: the request lives, and nothing else holds it.
        unsafe { ffi::shim_request_free(self.raw.as_ptr(), self.service) };
    }
}

/// The bytes of the body of a request message and of its answer.
#[derive(Clone, Copy, Debug)]
pub(super) struct Bytes {
    pub(super) ask: usize,
    pub(super) answer: usize,
}

impl std::ops::Add for Bytes {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            ask: self.ask + other.ask,
            answer: self.answer + other.answer,
        }
    }
}

/// The bytes of the messages of the requests of one service and of their good answers,
/// for [`Client::split`].
#[derive(Clone, Copy, Debug)]
pub(super) struct Sizes {
    /// A request of no items, with the session token, and its answer.
    pub(super) empty: Bytes,
    /// An item of a request, less its node or point, and its result.
    item: Bytes,
}

impl Sizes {
    /// The sizes of the `BrowseNext` requests that release their points, with a session
    /// token of `token` encoded bytes, and of their answers.
    pub(super) fn points(token: usize) -> Self {
        let one = Request::browse_next(&[&[]], true);
        let empty = Request::browse_next(&[], true);
        Self::of(&empty, &one, 0, token, Item::Result.bytes())
    }

    /// The sizes of the requests of `empty`, with no items, and `one`, with one item of
    /// `variable` bytes of its node or point, with a session token of `token` encoded
    /// bytes, and of their answers, with `result` bytes for each item.
    fn of(
        empty: &Request,
        one: &Request,
        variable: usize,
        token: usize,
        result: usize,
    ) -> Self {
        let ask = empty.bytes(token);
        Self {
            empty: Bytes {
                ask,
                answer: answer_bytes(empty.service),
            },
            item: Bytes {
                ask: one.bytes(token) - ask - variable,
                answer: result,
            },
        }
    }

    /// The bytes that an item adds to a request, with `variable` bytes of its node or
    /// point, and to its answer.
    pub(super) fn item(&self, variable: usize) -> Bytes {
        Bytes {
            ask: self.item.ask + variable,
            answer: self.item.answer,
        }
    }
}

impl Client {
    /// The sizes of the requests that `build` gives for a list of nodes, on this
    /// client's session, and of their answers, with `result` bytes for each node.
    pub(super) fn sizes(
        &self,
        build: impl Fn(&[ffi::Node<'_>]) -> Request,
        result: usize,
    ) -> Sizes {
        let zero = Node {
            namespace: 0,
            key: Key::Number(0),
        };
        let one = build(&[(&zero).into()]);
        Sizes::of(&build(&[]), &one, id_bytes(&zero), self.token, result)
    }
}

/// Gives the bytes of the body of the message of a good answer of `service` with no
/// results.
pub(super) fn answer_bytes(service: Service) -> usize {
    // SAFETY: it reads nothing.
    unsafe { ffi::shim_answer_bytes(service) }
}

/// A kind of result of a good answer.
#[derive(Clone, Copy, Debug)]
pub(super) enum Item {
    /// A Read result of a Double, the largest scalar of a value.
    Value,
    /// A Read result of a `NodeId` by a name.
    Fact,
    /// A Read result of an `EUInformation`.
    Unit,
    /// A Translate result of one target.
    Path,
    /// A Browse or `BrowseNext` result with a continuation point and no references.
    Result,
    /// One reference of a Browse result.
    Reference,
    /// A `CreateMonitoredItems` result with no filter result.
    Created,
    /// A data change of a Double in a Publish answer.
    Change,
}

impl Item {
    /// Gives the most bytes of one result of the kind, with each text and byte string
    /// at 256 bytes. A larger text can make an answer past one message, and then the
    /// server closes the channel.
    pub(super) fn bytes(self) -> usize {
        match self {
            Self::Value => ffi::shim_item_value(),
            Self::Fact => ffi::shim_item_fact(),
            Self::Unit => ffi::shim_item_unit(),
            Self::Path => ffi::shim_item_path(),
            Self::Result => ffi::shim_item_result(),
            Self::Reference => ffi::shim_item_reference(),
            Self::Created => ffi::shim_item_created(),
            Self::Change => ffi::shim_item_change(),
        }
    }
}

/// The answer of a service, moved out of open62541.
pub(super) struct Answer {
    raw: NonNull<c_void>,
    service: Service,
}

/// A reference of a browse, copied out of its answer.
pub(super) struct Found {
    /// The node, or its text when [`Node`] cannot hold it.
    pub(super) node: Result<Node, Foreign>,
    /// The reference type, or `None` when its id is not a number or a name.
    pub(super) via: Option<Node>,
    pub(super) name: String,
    pub(super) display: String,
    /// The `UA_NodeClass`.
    pub(super) class: u32,
}

impl Answer {
    /// Gives the count of its results.
    fn count(&self) -> usize {
        // SAFETY: the answer lives.
        unsafe { ffi::shim_answer_count(self.raw.as_ptr()) }
    }

    /// Gives its service result.
    ///
    /// # Errors
    ///
    /// The service result when it is Bad.
    pub(super) fn result(&self) -> Result<(), Status> {
        // SAFETY: the answer lives.
        let status = Status(unsafe { ffi::shim_response_result(self.raw.as_ptr()) });
        if status == Status::GOOD {
            Ok(())
        } else {
            Err(status)
        }
    }

    /// Checks that the answer has `count` results.
    ///
    /// # Errors
    ///
    /// `BadUnexpectedError` when it has more or fewer.
    pub(super) fn has(&self, count: usize) -> Result<(), Status> {
        if self.count() == count {
            Ok(())
        } else {
            Err(Status::BAD_UNEXPECTED_ERROR)
        }
    }

    /// Asserts that the answer is of one of `services` and has more results than
    /// `index`.
    fn check(&self, services: &[Service], index: usize) {
        assert!(
            services.contains(&self.service),
            "invariant: a reader of {services:?} reads no answer of {:?}",
            self.service
        );
        assert!(
            index < self.count(),
            "invariant: a reader reads result {index} of {} results",
            self.count()
        );
    }

    /// Gives the node of the first target of result `index` of a Translate answer, or
    /// `None` when the result has none in this server by a number or a name.
    ///
    /// # Panics
    ///
    /// If the answer is not of a Translate, or has no result `index`.
    pub(super) fn target(&self, index: usize) -> Option<Node> {
        self.check(&[Service::TRANSLATE], index);
        let mut node = ffi::Node::default();
        // SAFETY: the answer lives and has more results than `index`, and `node` is a
        // place to write.
        let found =
            unsafe { ffi::shim_units_target(self.raw.as_ptr(), index, &raw mut node) };
        // SAFETY: when found, the name of `node` borrows from the answer.
        found.then(|| unsafe { node.owned() })?
    }

    /// Gives the value of result `index` of a Read answer, which lives with the answer.
    ///
    /// # Panics
    ///
    /// If the answer is not of a Read, or has no result `index`.
    pub(super) fn value(&self, index: usize) -> *const ffi::DataValue {
        self.check(&[Service::READ], index);
        // SAFETY: the answer lives, and has more results than `index`.
        unsafe { ffi::shim_read_result(self.raw.as_ptr().cast(), index) }
    }

    /// Gives the id of the subscription of a Good `CreateSubscription` answer, and its
    /// publishing interval in ms.
    ///
    /// # Panics
    ///
    /// If the answer is not of a `CreateSubscription`.
    pub(super) fn subscription(&self) -> (u32, f64) {
        assert_eq!(
            self.service,
            Service::SUBSCRIBE,
            "invariant: a reader of a subscription reads no other answer"
        );
        let mut interval = 0.0;
        // SAFETY: the answer lives, and `interval` is a place to write.
        let id =
            unsafe { ffi::shim_subscription(self.raw.as_ptr(), &raw mut interval) };
        (id, interval)
    }

    /// Gives the sampling interval in ms and the queue size of result `index` of a
    /// `CreateMonitoredItems` answer.
    ///
    /// # Errors
    ///
    /// The status of the result when it is not Good.
    ///
    /// # Panics
    ///
    /// If the answer is not of a `CreateMonitoredItems`, or has no result `index`.
    pub(super) fn created(&self, index: usize) -> Result<(f64, u32), Status> {
        self.check(&[Service::MONITOR], index);
        let (mut sampling, mut queue) = (0.0, 0);
        // SAFETY: the answer lives and has more results than `index`, and `sampling`
        // and `queue` are places to write.
        let status = Status(unsafe {
            ffi::shim_monitor_result(
                self.raw.as_ptr(),
                index,
                &raw mut sampling,
                &raw mut queue,
            )
        });
        if status == Status::GOOD {
            Ok((sampling, queue))
        } else {
            Err(status)
        }
    }

    /// Gives each reference of result `index` of a Browse or `BrowseNext` answer to
    /// `each`, in order, and gives its continuation point, or `None` when it has none.
    ///
    /// # Errors
    ///
    /// The status of the result when it is not Good.
    ///
    /// # Panics
    ///
    /// If the answer is not of a Browse or a `BrowseNext`, or has no result `index`.
    pub(super) fn browsed(
        &self,
        index: usize,
        mut each: impl FnMut(Found),
    ) -> Result<Option<Vec<u8>>, Status> {
        self.check(&[Service::BROWSE, Service::BROWSE_NEXT], index);
        let raw = self.raw.as_ptr();
        let (mut count, mut point) = (0, ffi::Bytes::default());
        // SAFETY: the answer lives and has more results than `index`.
        let status = Status(unsafe {
            ffi::shim_browse_result(raw, index, &raw mut count, &raw mut point)
        });
        if status != Status::GOOD {
            return Err(status);
        }
        for reference in 0..count {
            let mut out = ffi::Reference::default();
            // SAFETY: the result has `count` references, and `out` is a place to
            // write.
            unsafe { ffi::shim_browse_reference(raw, index, reference, &raw mut out) };
            let node = match out.held {
                // SAFETY: each text and the name of the node borrow from the answer,
                // which lives through the copies.
                ffi::held::NODE => unsafe { out.node.owned() }.ok_or(Cause::Name),
                ffi::held::KEY => Err(Cause::Key),
                ffi::held::REMOTE => Err(Cause::Remote),
                held => unreachable!("invariant: the shim gives no `held` of {held}"),
            }
            .map_err(|cause| Foreign {
                text: text(raw, index, reference),
                cause,
            });
            // SAFETY: as above.
            let via = unsafe { out.via.owned() }.filter(|_| out.typed);
            // SAFETY: as above.
            let name = unsafe { out.name.lossy() };
            // SAFETY: as above.
            let display = unsafe { out.display.lossy() };
            each(Found {
                node,
                via,
                name,
                display,
                class: out.class,
            });
        }
        // SAFETY: the point borrows from the answer.
        let point = unsafe { point.to_vec() };
        Ok((!point.is_empty()).then_some(point))
    }
}

/// Gives the bytes of the text of the node of reference `reference` of result `index`
/// of `raw`, a Browse or `BrowseNext` answer, such as `ns=1;g=<guid>`, with the bytes
/// of a name as the server gave them.
///
/// # Aborts
///
/// If open62541 cannot allocate it, as [`alloc::exhausted`] says.
fn text(raw: *const c_void, index: usize, reference: usize) -> Vec<u8> {
    let mut out = ffi::Bytes::default();
    // SAFETY: the caller gives a live answer with the reference, and `out` is a place
    // to write.
    let status =
        Status(unsafe { ffi::shim_browse_text(raw, index, reference, &raw mut out) });
    if status != Status::GOOD {
        alloc::exhausted()
    }
    // SAFETY: open62541 wrote `out`, which holds `length` bytes.
    let text = unsafe { out.to_vec() };
    // SAFETY: open62541 allocated `out`, and nothing else holds it.
    unsafe { ffi::shim_buffer_free(&raw mut out) };
    text
}

impl Drop for Answer {
    fn drop(&mut self) {
        // SAFETY: the answer lives, and nothing else holds it.
        unsafe { ffi::shim_answer_free(self.raw.as_ptr(), self.service) };
    }
}

/// The service request that a client waits for, shared with [`served`].
#[derive(Default)]
pub(super) struct Slot {
    /// Its number and service.
    awaited: Cell<Option<(u32, Service)>>,
    /// Its answer, once it comes.
    answer: Cell<Option<Answer>>,
}

impl Client {
    /// Sends `request` and drives the client until its answer, as [`Client::read`]
    /// does with one request.
    ///
    /// # Errors
    ///
    /// `BadRequestTooLarge`, with nothing sent, when `request` does not fit one message
    /// that the client sends, `BadTimeout` and `BadTooManyOperations` as
    /// [`Client::read`] gives them, else the status of open62541 when it refuses the
    /// request, or the service result of an answer when it is Bad.
    pub(super) async fn serve(&mut self, request: &Request) -> Result<Answer, Status> {
        let answer = self.exchange(request).await?;
        answer.result()?;
        Ok(answer)
    }

    /// As [`Client::serve`], but gives an answer whose service result is Bad.
    ///
    /// # Errors
    ///
    /// As [`Client::serve`], other than the service result.
    pub(super) async fn exchange(
        &mut self,
        request: &Request,
    ) -> Result<Answer, Status> {
        if request.bytes(self.token) > self.sends {
            return Err(Status::BAD_REQUEST_TOO_LARGE);
        }
        let slot = &self.calls.service;
        slot.awaited.set(None);
        drop(slot.answer.take());
        self.call(|client, sent| {
            if let Err(status) = client.ask(request, sent) {
                return Poll::Ready(Err(status));
            }
            let answer = client.calls.service.answer.take();
            answer.map_or(Poll::Pending, |answer| Poll::Ready(Ok(answer)))
        })
        .await
    }

    /// Sends `request` unless `sent`, or [`Client::budget`] requests are in flight,
    /// and then sets `sent`.
    fn ask(&self, request: &Request, sent: &mut bool) -> Result<(), Status> {
        let calls = &*self.calls;
        if *sent || calls.pending.get() >= self.budget {
            return Ok(());
        }
        let mut key = 0;
        // SAFETY: the client and the request live, and `calls` outlives the client.
        let status = Status(unsafe {
            ffi::shim_client_service(
                self.raw.as_ptr(),
                request.raw.as_ptr(),
                request.service,
                served,
                ptr::from_ref::<Calls>(calls).cast_mut().cast(),
                &raw const subscription::WATCH,
                &raw mut key,
            )
        });
        if status != Status::GOOD {
            return Err(status);
        }
        calls.pending.set(calls.pending.get() + 1);
        calls.service.awaited.set(Some((key, request.service)));
        *sent = true;
        Ok(())
    }
}

/// Ends one service request of the [`Calls`] at `data`. When it is the request that the
/// client waits for, moves its answer to its [`Slot`].
///
/// # Safety
///
/// `data` points at live [`Calls`], and `response` at the answer.
unsafe extern "C" fn served(
    _: *mut ffi::Client,
    data: *mut c_void,
    request: u32,
    response: *mut c_void,
) {
    // SAFETY: the client that holds the calls lives.
    let calls = unsafe { &*data.cast_const().cast::<Calls>() };
    calls.pending.set(calls.pending.get() - 1);
    let slot = &calls.service;
    let Some((key, service)) = slot.awaited.get() else {
        return;
    };
    if key != request {
        return;
    }
    slot.awaited.set(None);
    // SAFETY: open62541 gives a live answer of the service of the request.
    let raw = unsafe { ffi::shim_answer_move(response, service) };
    let Some(raw) = NonNull::new(raw) else {
        alloc::exhausted()
    };
    slot.answer.set(Some(Answer { raw, service }));
}
