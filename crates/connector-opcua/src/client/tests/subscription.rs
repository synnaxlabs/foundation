//! The subscription of a client: its items, its changes, and its end.

use std::cell::Cell;
use std::future;
use std::pin::pin;
use std::rc::Rc;

use types::quality::Quality;
use types::time::Span;

use super::common::is;
use super::{
    Peer, READS, RELAY, Relay, drop_reads, numbered, recount_of, relayed, run, serve,
};
use crate::client::service::Item;
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

/// Gives the subscribe of 2 nodes through a relay that gives each answer of the type
/// `kind` the service result [`GOOD_CALL_AGAIN`], and a `CreateMonitoredItems` answer
/// no results.
fn called_again(kind: [u8; 4]) -> Result<Revised, Status> {
    run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            for index in 0..2 {
                peer.server.add_scalar(&numbered(index), Value::U32(index));
            }
            let url = format!("opc.tcp://{}:{RELAY}", node.addresses()[0]);
            let rewrite = |message: &mut Vec<u8>| {
                if is(message, kind) {
                    message[40..44].copy_from_slice(&GOOD_CALL_AGAIN.0.to_le_bytes());
                    if kind == MONITOR_RESPONSE {
                        recount_of(message, 0);
                    }
                }
            };
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
                    let status = Status::BAD_SESSION_CLOSED.0.to_le_bytes();
                    message[40..44].copy_from_slice(&status);
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
/// after its first change, and gives its output. The server gives a publishing
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
            let out = body(&mut peer, &mut client, &level).await;
            client.close().await;
            out
        })
        .await
    })
    .0
}

/// A Publish answer holds the most changes that fit the receive limit of the client.
#[test]
fn a_publish_answer_holds_the_most_changes_that_fit_the_client() {
    let (most, fits) = on_level(100, async |peer, client, _| {
        let most = usize::try_from(peer.server.per_publish()).expect("fits");
        let bytes =
            |count: usize| ffi::shim_publish_bytes() + count * Item::Change.bytes();
        let fits = (bytes(most) <= client.takes, bytes(most + 1) <= client.takes);
        (most, fits)
    });
    assert_eq!(fits, (true, false), "{most}");
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
/// one drive, and again at each later call. The client then sends no Publish
/// request, and the server takes only the one that the client sent before the end.
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
    assert_eq!((ends, publishes), (vec![changed, end.clone(), end], 1));
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
