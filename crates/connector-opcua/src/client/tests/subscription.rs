//! The subscription of a client: its items, its changes, and its end.

use std::cell::Cell;
use std::future;
use std::pin::pin;
use std::rc::Rc;

use types::quality::Quality;
use types::time::Span;

use super::{Peer, READS, drop_reads, numbered, run, serve};
use crate::client::subscription::{Change, Revised, Sampled};
use crate::client::{Client, PUBLISHES};
use crate::ffi::Status;
use crate::node::Node;
use crate::unwind::caught;
use crate::value::{Fault, Value};

/// The publishing interval of each subscription.
const INTERVAL: Span = Span::SECOND;
/// The sampling interval of each item.
const SAMPLING: Span = Span::from_nanos(100_000_000);

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

/// Subscribes a client to `count` nodes of [`numbered`], each with a queue of 4, on a
/// server that `limit` sets.
fn monitored(count: u32, limit: impl FnOnce(&Peer<'_>) + Send + 'static) -> Subscribed {
    run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            limit(&peer);
            for index in 0..count {
                peer.server.add_scalar(&numbered(index), Value::U32(index));
            }
            let nodes: Vec<Node> = (0..count).map(numbered).collect();
            let counts = peer.server.count();
            let mut client = peer.connected().await;
            let revised = client.subscribe(INTERVAL, &nodes, SAMPLING, 4).await;
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
        .map(|index| (usize::try_from(index).expect("fits"), Ok(Value::U32(index))))
        .collect()
}

/// Each item that the server made, at the interval of [`INTERVAL`].
fn made(count: usize) -> Revised {
    let item = Sampled {
        sampling: SAMPLING,
        queue: 4,
    };
    Revised {
        interval: INTERVAL,
        items: vec![Ok(item); count],
    }
}

/// The items go in calls of at most the `MaxMonitoredItemsPerCall` of the server, and
/// a limit of 0 is no limit. The change of each item gives the index of its node.
#[test]
fn the_items_take_calls_of_at_most_the_max_monitored_items_per_call() {
    let tens = monitored(25, |peer| peer.server.set_per_monitor(10));
    assert_eq!(tens, (Ok(made(25)), 3, numbers(25)));
    let none = monitored(25, |peer| peer.server.set_per_monitor(0));
    assert_eq!(none, (Ok(made(25)), 1, numbers(25)));
}

/// A call that the server refuses gives its status to each item of it.
#[test]
fn a_refused_call_gives_its_status_to_each_item() {
    let (revised, calls, changes) =
        monitored(3, |peer| peer.server.refuse_monitors_past(2));
    let refused = Err(Status::BAD_TOO_MANY_OPERATIONS);
    let revised = revised.expect("the subscription");
    assert_eq!(
        (revised.items, calls, changes),
        (vec![refused; 3], 1, vec![])
    );
}

/// Runs `body` on a client subscribed to the Double of [`add_level`] with a queue of
/// `queue`, after its first change, and gives its output.
fn on_level<T: Send + 'static>(
    queue: u32,
    body: impl AsyncFnOnce(&mut Peer<'_>, &mut Client, &Cell<f64>) -> T + Send + 'static,
) -> T {
    run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            let level = add_level(&peer);
            let mut client = peer.connected().await;
            let nodes = [numbered(0)];
            let revised = client.subscribe(INTERVAL, &nodes, SAMPLING, queue).await;
            let revised = revised.expect("the subscription");
            assert_eq!(
                revised.items,
                [Ok(Sampled {
                    sampling: SAMPLING,
                    queue
                })]
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

/// A queue of 2 with 3 changes in one publishing interval drops the oldest, and the
/// first change that comes has the overflow bit.
#[test]
fn a_full_queue_drops_the_oldest_change_and_marks_the_next() {
    let changes = on_level(2, async |peer, client, level| {
        for value in [1.0, 2.0, 3.0] {
            level.set(value);
            peer.clock.sleep(ms(200)).await;
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
    let (ends, publishes) = on_level(4, async |peer, client, level| {
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
    let changes = on_level(4, async |peer, client, level| {
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
        on_level(4, async move |peer, client, _| {
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
    let panic = on_level(4, async |_, client, _| {
        let nodes = [numbered(0)];
        let second = caught(client.subscribe(INTERVAL, &nodes, SAMPLING, 4)).await;
        let panic = second.expect_err("the subscribe panics");
        panic.downcast_ref::<String>().cloned()
    });
    let text = "assertion `left == right` failed: invariant: a client holds one \
                subscription\n  left: 7\n right: 9";
    assert_eq!(panic.as_deref(), Some(text));
}
