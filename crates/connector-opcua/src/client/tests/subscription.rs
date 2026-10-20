//! The subscription of a client: its items, its changes, and its end.

use std::cell::Cell;
use std::future;
use std::pin::pin;
use std::rc::Rc;

use types::quality::Quality;
use types::time::Span;

use super::common::{is, set_result};
use super::{
    Peer, READS, RELAY, Relay, drop_reads, numbered, recount_of, relayed, run, serve,
};
use crate::client::service::Request;
use crate::client::subscription::{Change, Revised, Sampled};
use crate::client::{Client, PUBLISHES};
use crate::ffi::{self, Status};
use crate::node::Node;
use crate::unwind::caught;
use crate::value::{Fault, Value};

/// A sampling interval under the shortest publishing interval of a client.
const SAMPLING: Span = Span::from_nanos(50_000_000);
/// The sampling interval of the item of [`on_level`].
const LEVEL: Span = Span::from_nanos(250_000_000);

/// The binary encoding of a `CreateSubscription` response, a `NodeId` of 4 bytes.
const SUBSCRIBE_RESPONSE: [u8; 4] = [0x01, 0x00, 0x16, 0x03];
/// The binary encoding of a `CreateMonitoredItems` response, a `NodeId` of 4 bytes.
const MONITOR_RESPONSE: [u8; 4] = [0x01, 0x00, 0xF2, 0x02];
/// The binary encoding of a Publish response, a `NodeId` of 4 bytes.
const PUBLISH_RESPONSE: [u8; 4] = [0x01, 0x00, 0x3D, 0x03];
/// `GoodCallAgain`, a service result that is not Good and not Bad.
const GOOD_CALL_AGAIN: Status = Status(0x00A9_0000);
/// `BadTooManyPublishRequests`.
const TOO_MANY: Status = Status(0x8078_0000);

/// Gives the span of `ms` ms.
const fn ms(ms: i64) -> Span {
    Span::from_nanos(ms * 1_000_000)
}

/// Adds the Double [`numbered`] 0, whose value the returned cell sets.
fn add_level(peer: &Peer<'_>) -> Rc<Cell<f64>> {
    let level = Rc::new(Cell::new(0.0));
    let answer = Rc::clone(&level);
    peer.server.add_source(&numbered(0), move || {
        (Value::F64(answer.get()), Quality::GOOD, None)
    });
    level
}

/// The change of the Double of [`add_level`] to `value`, with no timestamps.
fn changed_to(value: f64, overflow: bool) -> Change {
    Change {
        index: 0,
        value: Ok(Value::F64(value)),
        source: None,
        server: None,
        overflow,
    }
}

/// `changes` with no timestamps, which the tests do not pin.
fn untimed(changes: &[Change]) -> Vec<Change> {
    (changes.iter())
        .map(|change| Change {
            source: None,
            server: None,
            ..change.clone()
        })
        .collect()
}

/// What [`monitored`] gives: what the server gave, the `CreateMonitoredItems` calls,
/// and the index and value of each change of the first 3 s.
type Subscribed = (
    Result<Revised, Status>,
    usize,
    Vec<(usize, Result<Value, Fault>)>,
);

/// Subscribes a client to `count` nodes of [`numbered`], each sampled at `sampling`,
/// on a server that grants a publishing interval of 10 ms or more and that `limit`
/// sets.
fn monitored(
    count: u32,
    sampling: Span,
    limit: impl FnOnce(&Peer<'_>) + Send + 'static,
) -> Subscribed {
    limited(count, sampling, None, limit)
}

/// [`monitored`], with a client whose receive buffer is `receive` bytes, or the
/// default of open62541 for `None`.
fn limited(
    count: u32,
    sampling: Span,
    receive: Option<u32>,
    limit: impl FnOnce(&Peer<'_>) + Send + 'static,
) -> Subscribed {
    run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            peer.server.set_publishing(10.0);
            limit(&peer);
            for index in 0..count {
                peer.server
                    .add_scalar(&numbered(index), Value::F64(f64::from(index)));
            }
            let nodes: Vec<Node> = (0..count).map(numbered).collect();
            let counts = peer.server.count();
            let url = peer.url.clone();
            let mut client = peer.limited(&url, receive, None).await;
            let revised = client.subscribe(&nodes, sampling).await;
            let calls = counts.monitors();
            let mut changes = Vec::new();
            let mut wait = pin!(peer.clock.sleep(ms(3000)));
            loop {
                let got = client.changes(wait.as_mut()).await.expect("no end");
                if got.is_empty() {
                    break;
                }
                changes.extend(got.iter().map(|change| (change.index, change.value)));
            }
            client.close().await;
            (revised, calls, changes)
        })
        .await
    })
    .0
}

/// The value of each of `count` nodes of [`numbered`], by index.
fn numbers(count: u32) -> Vec<(usize, Result<Value, Fault>)> {
    (0..count)
        .map(|index| {
            (
                usize::try_from(index).expect("fits"),
                Ok(Value::F64(f64::from(index))),
            )
        })
        .collect()
}

/// `count` items sampled at [`SAMPLING`], at the interval of 100 ms, each with a
/// queue of the 4 values of two intervals.
fn made(count: usize) -> Revised {
    let item = Sampled {
        sampling: SAMPLING,
        queue: 4,
    };
    Revised {
        interval: ms(100),
        items: vec![Ok(item); count],
    }
}

/// The items go in calls of at most the `MaxMonitoredItemsPerCall` of the server, and
/// a limit of 0 is no limit. The change of each item gives the index of its node.
#[test]
fn the_items_take_calls_of_at_most_the_max_monitored_items_per_call() {
    let tens = monitored(25, SAMPLING, |peer| peer.server.set_per_monitor(10));
    assert_eq!(tens, (Ok(made(25)), 3, numbers(25)));
    let none = monitored(25, SAMPLING, |peer| peer.server.set_per_monitor(0));
    assert_eq!(none, (Ok(made(25)), 1, numbers(25)));
}

/// A sampling interval past 100 ms is the publishing interval, and the queue holds
/// the 2 values of two intervals.
#[test]
fn a_sampling_past_100_ms_is_the_publishing_interval() {
    let (revised, ..) = monitored(1, LEVEL, |_| {});
    let item = Sampled {
        sampling: LEVEL,
        queue: 2,
    };
    let interval = LEVEL;
    assert_eq!(
        revised,
        Ok(Revised {
            interval,
            items: vec![Ok(item)]
        })
    );
}

/// Gives the subscribe of 2 nodes at [`SAMPLING`] through a relay that changes each
/// message from the server with `rewrite`.
fn rewritten(
    rewrite: impl Fn(&mut Vec<u8>) + Send + 'static,
) -> Result<Revised, Status> {
    run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            for index in 0..2 {
                peer.server.add_scalar(&numbered(index), Value::U32(index));
            }
            let url = format!("opc.tcp://{}:{RELAY}", node.addresses()[0]);
            relayed(&node, &Relay::default(), rewrite, async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                let nodes = [numbered(0), numbered(1)];
                let revised = client.subscribe(&nodes, SAMPLING).await;
                client.close().await;
                revised
            })
            .await
        })
        .await
    })
    .0
}

/// Gives [`rewritten`] with the service result [`GOOD_CALL_AGAIN`] in each answer of
/// the type `kind`, and no results in a `CreateMonitoredItems` answer.
fn called_again(kind: [u8; 4]) -> Result<Revised, Status> {
    rewritten(move |message| {
        if is(message, kind) {
            set_result(message, GOOD_CALL_AGAIN);
            if kind == MONITOR_RESPONSE {
                recount_of(message, 0);
            }
        }
    })
}

/// A create answer whose service result is not Good fails, as open62541 then makes
/// no subscription or item: the subscribe at a `CreateSubscription`, and each item of
/// the call at a `CreateMonitoredItems`, also with too few results.
#[test]
fn a_create_answer_that_is_not_good_fails() {
    let subscribe = called_again(SUBSCRIBE_RESPONSE);
    let monitor = called_again(MONITOR_RESPONSE).map(|revised| revised.items);
    let items = vec![Err(GOOD_CALL_AGAIN); 2];
    assert_eq!((subscribe, monitor), (Err(GOOD_CALL_AGAIN), Ok(items)));
}

/// A Publish answer of `BadSessionClosed` makes open62541 delete the subscription
/// with no status change, so `changes` gives `BadNoSubscription`, also at a later
/// call.
#[test]
fn a_deleted_subscription_ends_with_bad_no_subscription() {
    let ends = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            add_level(&peer);
            let url = format!("opc.tcp://{}:{RELAY}", node.addresses()[0]);
            let armed = Cell::new(false);
            let rewrite = |message: &mut Vec<u8>| {
                if armed.get() && is(message, PUBLISH_RESPONSE) {
                    set_result(message, Status::BAD_SESSION_CLOSED);
                }
            };
            relayed(&node, &Relay::default(), rewrite, async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                let revised = client.subscribe(&[numbered(0)], LEVEL).await;
                assert!(revised.is_ok(), "{revised:?}");
                let first = client.changes(future::pending()).await.map(untimed);
                assert_eq!(first, Ok(vec![changed_to(0.0, false)]));
                armed.set(true);
                let mut ends = Vec::new();
                for _ in 0..2 {
                    ends.push(client.changes(future::pending()).await.map(untimed));
                }
                client.close().await;
                ends
            })
            .await
        })
        .await
    })
    .0;
    let end = Err(Status::BAD_NO_SUBSCRIPTION);
    assert_eq!(ends, [end.clone(), end]);
}

/// Gives what `run` gives, or panics when it takes more than 60 s of wall time, as a
/// client that spins in a poll never moves the clock of the sim.
#[expect(
    clippy::disallowed_methods,
    reason = "a test bounds its own run time, as the client thread can spin"
)]
fn bounded<T: Send + 'static>(run: impl FnOnce() -> T + Send + 'static) -> T {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || send.send(run()));
    let ran = receive.recv_timeout(std::time::Duration::from_secs(60));
    ran.expect("the run ends within 60 s of wall time")
}

/// The server refuses the Publish answers as `BadTooManyPublishRequests` from the
/// change of 2.0 on, `refusals` of them, while the change of 1.0 waits. Gives the
/// changes of 1.0, then those up to 5 s after changes to 3.0 and, 1 s later, 4.0.
fn refused(
    refusals: u32,
) -> (Result<Vec<Change>, Status>, Result<Vec<Change>, Status>) {
    run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            let level = add_level(&peer);
            let url = format!("opc.tcp://{}:{RELAY}", node.addresses()[0]);
            let left = Cell::new(0);
            let rewrite = |message: &mut Vec<u8>| {
                if left.get() > 0 && is(message, PUBLISH_RESPONSE) {
                    left.set(left.get() - 1);
                    set_result(message, TOO_MANY);
                }
            };
            relayed(&node, &Relay::default(), rewrite, async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                let revised = client.subscribe(&[numbered(0)], LEVEL).await;
                assert!(revised.is_ok(), "{revised:?}");
                let first = client.changes(future::pending()).await.map(untimed);
                assert_eq!(first, Ok(vec![changed_to(0.0, false)]));
                let idle = client.idle(peer.clock.sleep(ms(100))).await;
                assert_eq!(idle, Ok(()));
                level.set(1.0);
                let idle = client.idle(peer.clock.sleep(ms(1000))).await;
                assert_eq!(idle, Ok(()));
                left.set(refusals);
                level.set(2.0);
                let idle = client.idle(peer.clock.sleep(ms(1000))).await;
                assert_eq!(idle, Ok(()));
                let one = client.changes(future::ready(())).await.map(untimed);
                level.set(3.0);
                let idle = client.idle(peer.clock.sleep(ms(1000))).await;
                assert_eq!(idle, Ok(()));
                level.set(4.0);
                let next = client.changes(peer.clock.sleep(ms(5000))).await;
                let next = next.map(untimed);
                client.close().await;
                (one, next)
            })
            .await
        })
        .await
    })
    .0
}

/// open62541 lowers its count of Publish requests at each refusal, and ends the
/// subscription at a count of 1. A refusal of a request sent before the stop of a
/// waiting change lowers the count to 1 and keeps the subscription, so the next change
/// comes, then stops the Publish requests again.
#[test]
fn a_refusal_while_a_change_waits_keeps_the_subscription() {
    let (one, next) = bounded(|| refused(1));
    assert_eq!(one, Ok(vec![changed_to(1.0, false)]));
    assert_eq!(next, Ok(vec![changed_to(3.0, false)]));
}

/// The resume after a refusal while a change waits sends 1 Publish request, not
/// [`PUBLISHES`], so the next refusal comes at a count of 1 and ends the changes with
/// its status.
#[test]
fn a_resume_after_a_refusal_keeps_the_lowered_count() {
    let (one, next) = bounded(|| refused(2));
    assert_eq!(one, Ok(vec![changed_to(1.0, false)]));
    assert_eq!(next, Err(TOO_MANY));
}

/// A server that refuses each Publish request ends the changes with the status of the
/// refusal once the count is 1, with no service call from within the answer.
#[test]
fn a_refusal_at_one_publish_request_ends_the_changes() {
    let (one, next) = bounded(|| refused(u32::MAX));
    assert_eq!(one, Ok(vec![changed_to(1.0, false)]));
    assert_eq!(next, Err(TOO_MANY));
}

/// Gives the publishing interval of a subscribe at a sampling of 1 s on a server that
/// grants a keep-alive count of at least `least`, and the Publish requests that the
/// server takes from the subscribe to 5 s after it.
fn kept_alive(least: u32) -> (Result<Span, Status>, usize) {
    run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            add_level(&peer);
            peer.server.set_keep_alives(least);
            let mut client = peer.connected().await;
            let counts = peer.server.count();
            let revised = client.subscribe(&[numbered(0)], Span::SECOND).await;
            let idle = client.idle(peer.clock.sleep(ms(5000))).await;
            assert_eq!(idle, Ok(()));
            client.close().await;
            (revised.map(|revised| revised.interval), counts.publishes())
        })
        .await
    })
    .0
}

/// A subscribe fails with `BadOutOfRange`, and sends no Publish request, when the
/// server gives a publishing interval times a keep-alive count over 5 min, half of
/// the timeout of a Publish request in open62541. At 5 min, the server takes the
/// [`PUBLISHES`] requests, and none after the answer of the first change, which waits
/// for `changes`.
#[test]
fn a_keep_alive_past_5_min_fails_with_no_publish_request() {
    let (at, past) = (kept_alive(300), kept_alive(301));
    let publishes = usize::from(PUBLISHES);
    assert_eq!(
        (at, past),
        (
            (Ok(Span::SECOND), publishes),
            (Err(Status::BAD_OUT_OF_RANGE), 0)
        )
    );
}

/// Gives the publishing interval of [`rewritten`] with the revised publishing interval
/// `interval` ms and keep-alive count `keep_alive` in the `CreateSubscription` answer.
fn revised(interval: f64, keep_alive: u32) -> Result<Span, Status> {
    let revised = rewritten(move |message| {
        if is(message, SUBSCRIBE_RESPONSE) {
            message[56..64].copy_from_slice(&interval.to_le_bytes());
            message[68..72].copy_from_slice(&keep_alive.to_le_bytes());
        }
    });
    revised.map(|revised| revised.interval)
}

/// A revised publishing interval that is not positive fails as one past the bound. A
/// keep-alive count of 0 holds a Publish request for one interval, as one of 1 does.
#[test]
fn a_revised_interval_that_is_not_positive_fails() {
    let refused = Err(Status::BAD_OUT_OF_RANGE);
    let got = [
        revised(f64::NAN, 600),
        revised(-1000.0, 600),
        revised(0.0, 600),
        revised(600_000.0, 0),
        revised(100.0, 0),
    ];
    assert_eq!(got, [refused, refused, refused, refused, Ok(ms(100))]);
}

/// At a sampling of 2 min, the publishing interval is 1 min, so the server holds each
/// Publish request for at most 1 min, and never more than [`PUBLISHES`] of them, also
/// past the 10 min timeout of a Publish request in open62541.
#[test]
fn the_server_holds_at_most_publishes_at_a_long_sampling() {
    let (interval, held) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            add_level(&peer);
            let mut client = peer.connected().await;
            let sampling = Span::from_nanos(2 * Span::MINUTE.nanos());
            let revised = client.subscribe(&[numbered(0)], sampling).await;
            let first = client.changes(future::pending()).await.map(untimed);
            assert_eq!(first, Ok(vec![changed_to(0.0, false)]));
            let mut held = Vec::new();
            for _ in 0..(30 * 60) {
                let got = client.changes(peer.clock.sleep(Span::SECOND)).await;
                assert_eq!(got.map(<[_]>::len), Ok(0));
                held.push(peer.server.held());
            }
            client.close().await;
            (
                revised.map(|revised| revised.interval),
                held.into_iter().max(),
            )
        })
        .await
    })
    .0;
    let most = Some(u32::from(PUBLISHES));
    assert_eq!((interval, held), (Ok(Span::MINUTE), most));
}

/// The client sends its [`PUBLISHES`] Publish requests once the server makes the
/// subscription, before its items, and asks for a keep-alive count of the publishing
/// intervals in 1 min, so an idle subscription of an interval of 1 s gets one Publish
/// answer a minute.
#[test]
fn an_idle_subscription_gets_one_publish_answer_a_minute() {
    let (at_subscribe, idle) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            add_level(&peer);
            let mut client = peer.connected().await;
            let counts = peer.server.count();
            let revised = client.subscribe(&[numbered(0)], Span::SECOND).await;
            assert!(revised.is_ok(), "{revised:?}");
            let at_subscribe = counts.publishes();
            let first = client.changes(future::pending()).await.map(untimed);
            assert_eq!(first, Ok(vec![changed_to(0.0, false)]));
            let counts = peer.server.count();
            let got = client.changes(peer.clock.sleep(ms(10 * 60_000))).await;
            assert_eq!(got.map(<[_]>::len), Ok(0));
            let idle = counts.publishes();
            client.close().await;
            (at_subscribe, idle)
        })
        .await
    })
    .0;
    assert_eq!((at_subscribe, idle), (usize::from(PUBLISHES), 10));
}

/// Gives the changes and the Publish requests after their end when the server answers
/// both Publish requests while the client is not driven, then ends each subscription,
/// and deletes it with no Publish request queued, so with no status change. Before it,
/// the client makes `more` subscriptions with no items through the service path.
fn ended_with_no_publish_queued(
    more: usize,
) -> (Vec<Result<Vec<Change>, Status>>, usize) {
    on_level(4, async move |peer, client, level| {
        for _ in 0..more {
            let made = client.serve(&Request::subscribe(1000.0, 60, 100)).await;
            assert_eq!(made.map(|_| ()), Ok(()));
        }
        level.set(1.0);
        peer.clock.sleep(ms(1200)).await;
        level.set(2.0);
        peer.clock.sleep(ms(1200)).await;
        peer.server.end_subscriptions(Status::BAD_TIMEOUT);
        peer.clock.sleep(ms(1200)).await;
        let mut got = Vec::new();
        let mut wait = pin!(peer.clock.sleep(ms(5000)));
        loop {
            let changes = client.changes(wait.as_mut()).await.map(untimed);
            let done = !matches!(&changes, Ok(changes) if !changes.is_empty());
            got.push(changes);
            if done {
                break;
            }
        }
        let after = peer.server.count();
        let idle = client.idle(peer.clock.sleep(ms(10_000))).await;
        assert_eq!(idle, Ok(()));
        (got, after.publishes())
    })
}

/// When the server ends the subscription with no Publish request queued, the next
/// Publish answer of `BadNoSubscription` ends the changes, and the client sends no
/// Publish request after it, also when the client holds a second subscription. Its
/// first keep-alive takes a Publish request that the change of 2.0 then lacks.
#[test]
fn an_end_with_no_publish_request_queued_ends_the_changes() {
    let changed = vec![changed_to(1.0, false), changed_to(2.0, false)];
    let end = Err(Status::BAD_NO_SUBSCRIPTION);
    let (one, (two, publishes)) = (
        ended_with_no_publish_queued(0),
        ended_with_no_publish_queued(1),
    );
    assert_eq!(
        (one, two.last(), publishes),
        ((vec![Ok(changed), end.clone()], 0), Some(&end), 0)
    );
}

/// A call that the server refuses gives its status to each item of it.
#[test]
fn a_refused_call_gives_its_status_to_each_item() {
    let (revised, calls, changes) =
        monitored(3, SAMPLING, |peer| peer.server.refuse_monitors_past(2));
    let refused = Err(Status::BAD_TOO_MANY_OPERATIONS);
    let revised = revised.expect("the subscription");
    assert_eq!(
        (revised.items, calls, changes),
        (vec![refused; 3], 1, vec![])
    );
}

/// Runs `body` on a client subscribed to the Double of [`add_level`] at [`LEVEL`],
/// after its first change and once the server holds [`PUBLISHES`] Publish requests, and
/// gives its output. The server gives a publishing
/// interval of 1 s, so the item asks for a queue of 8, and grants queues of at most
/// `queues` and each count of notifications of a Publish answer that the client asks
/// for.
fn on_level<T: Send + 'static>(
    queues: u32,
    body: impl AsyncFnOnce(&mut Peer<'_>, &mut Client, &Cell<f64>) -> T + Send + 'static,
) -> T {
    run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            let level = add_level(&peer);
            peer.server.set_publishing(1000.0);
            peer.server.set_queues(queues);
            peer.server.set_per_publish(u32::MAX);
            let mut client = peer.connected().await;
            let nodes = [numbered(0)];
            let revised = client.subscribe(&nodes, LEVEL).await;
            let revised = revised.expect("the subscription");
            let item = Sampled {
                sampling: LEVEL,
                queue: queues.min(8),
            };
            assert_eq!(
                revised,
                Revised {
                    interval: Span::SECOND,
                    items: vec![Ok(item)]
                }
            );
            let first = client.changes(future::pending()).await.map(untimed);
            assert_eq!(first, Ok(vec![changed_to(0.0, false)]));
            let sent = client.idle(peer.clock.sleep(ms(100))).await;
            assert_eq!((sent, peer.server.held()), (Ok(()), u32::from(PUBLISHES)));
            let out = body(&mut peer, &mut client, &level).await;
            client.close().await;
            out
        })
        .await
    })
    .0
}

/// A Publish answer holds the most changes that fit the receive limit of the client:
/// 13 785 at the default of open62541.
#[test]
fn a_publish_answer_holds_the_most_changes_that_fit_the_client() {
    let most = on_level(100, async |peer, _, _| peer.server.per_publish());
    assert_eq!(most, 13_785);
}

/// A client with a receive buffer of 8 KiB gets the first value of each of 3000 items
/// with no error, so each Publish answer fits it.
#[test]
fn each_publish_answer_fits_a_small_client() {
    let (revised, _, changes) = limited(3000, SAMPLING, Some(8192), |peer| {
        peer.server.set_per_publish(u32::MAX);
    });
    assert_eq!((revised, changes), (Ok(made(3000)), numbers(3000)));
}

/// A queue of 2 with 3 changes in one publishing interval drops the oldest, and the
/// first change that comes has the overflow bit.
#[test]
fn a_full_queue_drops_the_oldest_change_and_marks_the_next() {
    let changes = on_level(2, async |peer, client, level| {
        for value in [1.0, 2.0, 3.0] {
            level.set(value);
            peer.clock.sleep(ms(300)).await;
        }
        client.changes(future::pending()).await.map(untimed)
    });
    assert_eq!(
        changes,
        Ok(vec![changed_to(2.0, true), changed_to(3.0, false)])
    );
}

/// The end of the subscription comes after the changes that came before it, also in
/// one drive, and again at each later call. The server takes only the one Publish
/// request that the client sent before the end, and one after it, whose answer of
/// `BadNoSubscription` removes the subscription in the client.
#[test]
fn the_end_comes_after_the_changes_and_stops_the_publish_requests() {
    let (ends, publishes) = on_level(100, async |peer, client, level| {
        level.set(1.0);
        peer.clock.sleep(ms(1200)).await;
        peer.server.end_subscriptions(Status::BAD_TIMEOUT);
        peer.clock.sleep(ms(2500)).await;
        let mut ends = vec![client.changes(future::pending()).await.map(untimed)];
        let after = peer.server.count();
        for _ in 0..2 {
            ends.push(client.changes(future::pending()).await.map(untimed));
        }
        let idle = client.idle(peer.clock.sleep(ms(10_000))).await;
        assert_eq!(idle, Ok(()));
        (ends, after.publishes())
    });
    let end = Err(Status::BAD_TIMEOUT);
    let changed = Ok(vec![changed_to(1.0, false)]);
    assert_eq!((ends, publishes), (vec![changed, end.clone(), end], 2));
}

/// A `changes` whose `until` ends first gives no changes, and a change that a read
/// takes in comes at the next `changes`.
#[test]
fn a_change_that_a_read_takes_in_comes_at_the_next_changes() {
    let changes = on_level(100, async |peer, client, level| {
        let none = client
            .changes(peer.clock.sleep(ms(2500)))
            .await
            .map(untimed);
        level.set(1.0);
        peer.clock.sleep(ms(1500)).await;
        let read = client.read_one(&numbered(0)).await;
        let next = client.changes(future::ready(())).await.map(untimed);
        (none, read, next)
    });
    let (none, read, next) = changes;
    assert_eq!(none, Ok(vec![]));
    assert_eq!(read, Ok(Value::F64(1.0)));
    assert_eq!(next, Ok(vec![changed_to(1.0, false)]));
}

/// While a change waits for `changes`, the client sends no Publish request, also when
/// it reads, so at most the answers of the requests in flight wait in it, and later
/// values wait in the queue of 2 of the server, the oldest dropped.
#[test]
fn a_waiting_change_stops_the_publish_requests() {
    let (publishes, first, next) = on_level(2, async |peer, client, level| {
        let counts = peer.server.count();
        for value in 1..=8 {
            level.set(f64::from(value));
            peer.clock.sleep(ms(1000)).await;
            client.read_one(&numbered(0)).await.expect("the level");
        }
        let publishes = counts.publishes();
        let first = client.changes(future::ready(())).await.map(untimed);
        let next = client.changes(peer.clock.sleep(ms(5000))).await;
        (publishes, first, next.map(untimed))
    });
    assert_eq!(publishes, 0);
    assert_eq!(
        first,
        Ok(vec![changed_to(1.0, false), changed_to(2.0, false)])
    );
    assert_eq!(
        next,
        Ok(vec![changed_to(7.0, true), changed_to(8.0, false)])
    );
}

/// A server that holds 1 Publish request refuses the second at the subscribe, and the
/// client then sends 1 Publish request for each `changes`, so the server refuses no
/// more.
#[test]
fn a_take_keeps_the_publish_requests_that_a_refusal_lowers() {
    let (refused, sent) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let level = add_level(&peer);
            peer.server.set_publishing(1000.0);
            peer.server.set_held(1);
            let counts = peer.server.count();
            let mut client = peer.connected().await;
            let revised = client.subscribe(&[numbered(0)], LEVEL).await;
            assert!(revised.is_ok(), "{revised:?}");
            let first = client.changes(future::pending()).await.map(untimed);
            assert_eq!(first, Ok(vec![changed_to(0.0, false)]));
            let mut sent = Vec::new();
            for value in 1..=5 {
                let taken = peer.server.count();
                level.set(f64::from(value));
                let got = client.changes(future::pending()).await.map(untimed);
                assert_eq!(got, Ok(vec![changed_to(f64::from(value), false)]));
                sent.push(taken.publishes());
            }
            client.close().await;
            (counts.refused(), sent)
        })
        .await
    })
    .0;
    assert_eq!((refused, sent), (1, vec![1; 5]));
}

/// The subscribe reserves one block for the decode of a Publish answer. An answer of
/// 4 KiB takes a second block, which the client frees after the answer.
#[test]
fn an_answer_past_the_reserve_keeps_only_the_reserved_block() {
    let (changes, blocks) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            peer.server.add_bytes(&numbered(0), 4096);
            let mut client = peer.connected().await;
            let revised = client.subscribe(&[numbered(0)], LEVEL).await;
            assert!(revised.is_ok(), "{revised:?}");
            // SAFETY: the client lives.
            let reserved =
                unsafe { ffi::test::shim_client_blocks(client.raw.as_ptr()) };
            let changes = client.changes(future::pending()).await.map(<[_]>::len);
            // SAFETY: the client lives.
            let after = unsafe { ffi::test::shim_client_blocks(client.raw.as_ptr()) };
            client.close().await;
            (changes, [reserved, after])
        })
        .await
    })
    .0;
    assert_eq!((changes, blocks), (Ok(1), [1, 1]));
}

/// A reserve past the memory of the process fails with `BadOutOfMemory` and keeps the
/// block of the subscribe: of more changes than a size holds, of more bytes than a
/// block with its header holds, and of more bytes than the allocator gives.
#[test]
fn a_reserve_past_the_memory_fails_and_keeps_the_block() {
    let (names, blocks) = on_level(100, async |_, client, _| {
        let raw = client.raw.as_ptr();
        let statuses = [
            // SAFETY: the client lives, and no answer decodes.
            unsafe { ffi::shim_client_reserve(raw, usize::MAX) },
            // SAFETY: as above.
            unsafe { ffi::test::__Client_PublishArena_reserve(raw, usize::MAX) },
            // SAFETY: as above.
            unsafe { ffi::test::__Client_PublishArena_reserve(raw, usize::MAX / 2) },
        ];
        // SAFETY: the client lives.
        let blocks = unsafe { ffi::test::shim_client_blocks(raw) };
        (statuses.map(|status| Status(status).name()), blocks)
    });
    assert_eq!((names, blocks), (["BadOutOfMemory"; 3], 1));
}

/// Once subscribed, a client keeps [`PUBLISHES`] of its [`READS`] for the Publish
/// requests, so a read fails at once at `READS - PUBLISHES` dropped reads, and below
/// it sends its request.
#[test]
fn a_subscribed_client_keeps_reads_for_the_publish_requests() {
    let held = |dropped: u16| {
        on_level(100, async move |peer, client, _| {
            peer.gate.close();
            drop_reads(client, dropped).await;
            let start = peer.clock.now();
            let read = client.read_one(&numbered(0)).await;
            let took = peer.clock.now() - start;
            peer.gate.open();
            (read, took)
        })
    };
    let bound = READS - PUBLISHES;
    let ((below, below_took), (at, at_took)) = (held(bound - 1), held(bound));
    assert_eq!(
        (below, at),
        (
            Err(Fault::status(Status::BAD_TIMEOUT)),
            Err(Fault::status(Status::BAD_TOO_MANY_OPERATIONS))
        )
    );
    let timeout = ms(5000)..=ms(6000);
    assert!(timeout.contains(&below_took), "{below_took:?}");
    assert!(timeout.contains(&at_took), "{at_took:?}");
}

/// A second subscribe panics, as a client holds one subscription.
#[test]
fn a_second_subscribe_panics() {
    let panic = on_level(100, async |_, client, _| {
        let nodes = [numbered(0)];
        let second = caught(client.subscribe(&nodes, SAMPLING)).await;
        let panic = second.expect_err("the subscribe panics");
        panic.downcast_ref::<String>().cloned()
    });
    let text = "assertion `left == right` failed: invariant: a client holds one \
                subscription\n  left: 7\n right: 9";
    assert_eq!(panic.as_deref(), Some(text));
}

/// A sampling interval that is not positive panics.
#[test]
fn a_sampling_of_zero_panics() {
    let panic = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            let nodes = [numbered(0)];
            let zero = caught(client.subscribe(&nodes, Span::ZERO)).await;
            client.close().await;
            let panic = zero.expect_err("the subscribe panics");
            panic.downcast_ref::<&str>().copied()
        })
        .await
    })
    .0;
    assert_eq!(panic, Some("invariant: a sampling interval is positive"));
}
