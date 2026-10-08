use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use clock::{Clock, Reader, Time};
use estimate::Measurement;
use sim::node::Node;
use types::time::{Span, Stamp};

use crate::common::{ms, node};

/// A wait of 15 minutes, the longest life of a hello.
const WAIT: Span = Span::from_nanos(15 * 60 * 1_000_000_000);

/// Pushes one measurement at the node's reading now, so that the clock serves it.
fn push(node: &Node, clock: &mut Clock, offset: Span) {
    let source = clock.add();
    let at = node.clock().now();
    let measurement = Measurement::new(at, offset, ms(10)).expect("valid");
    clock.push(source, measurement);
}

/// Waits on a shard of `node` until `reader` reaches `at`, and gives what it reads then.
fn wait(node: &Node, reader: &Reader, at: Stamp) -> Arc<Mutex<Option<Time>>> {
    let done = Arc::new(Mutex::new(None));
    let (kept, reader) = (Arc::clone(&done), reader.clone());
    let shard = env::shards::Config {
        name: "shard-0".into(),
        core: Some(0),
    };
    let handle = node.shards().start(shard, move |_tasks| async move {
        reader.reach(at).await;
        *kept.lock().expect("not poisoned") = Some(reader.now());
    });
    drop(handle.expect("the shard starts"));
    done
}

/// Under one slew, the wait ends at a reading whose edge has reached the stamp, and
/// within 1 ms of the first such reading.
#[test]
fn completes_within_a_millisecond_of_the_first_reading_that_reaches_the_stamp() {
    let (mut sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    push(&node, &mut clock, ms(5));
    let start = reader.now();
    let at = start.mesh.expect("mesh time").latest + WAIT;
    let done = wait(&node, &reader, at);
    sim.run().expect("the run ends");
    let Time { monotonic, mesh } = done.lock().expect("not poisoned").expect("done");
    assert!(
        mesh.expect("mesh time").latest >= at,
        "{mesh:?} before {at:?}"
    );
    let before = reader.first(monotonic - ms(1)).expect("mesh time");
    assert!(before.latest < at, "{before:?} reaches {at:?} 1 ms early");
    let waited = monotonic - start.monotonic;
    let (low, high) = (WAIT.nanos() - Span::SECOND.nanos(), WAIT.nanos());
    assert!(waited.nanos() > low && waited.nanos() < high, "{waited:?}");
}

#[test]
fn completes_on_the_first_poll_for_a_stamp_already_reached() {
    let (_sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    push(&node, &mut clock, ms(5));
    let latest = reader.now().mesh.expect("mesh time").latest;
    let mut context = Context::from_waker(Waker::noop());
    for at in [latest, latest - Span::HOUR] {
        let wait = pin!(reader.reach(at));
        assert_eq!(wait.poll(&mut context), Poll::Ready(()));
    }
}

/// Before the first mesh time, the wait reads again each second, and ends only
/// after the first push makes the edge reach the stamp.
#[test]
fn waits_for_the_first_mesh_time() {
    let (mut sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let (at, start) = (Stamp::from_nanos(1_000), node.clock().now());
    let done = wait(&node, &reader, at);
    sim.run_for(Span::from_nanos(500_000_000)).expect("runs");
    assert_eq!(*done.lock().expect("not poisoned"), None);
    push(&node, &mut clock, Span::ZERO);
    let pushed = node.clock().now();
    sim.run().expect("the run ends");
    let Time { monotonic, mesh } = done.lock().expect("not poisoned").expect("done");
    assert!(mesh.expect("mesh time").latest >= at);
    let read = start + Span::SECOND;
    assert_eq!(monotonic, read, "after a push at {pushed:?}");
}

/// A narrower estimate inside the wait moves the edge back, so the reading that the
/// first slew gave is too early, and the wait reads again.
#[test]
fn waits_on_when_a_new_estimate_moves_the_edge_back() {
    let (mut sim, node) = node();
    let (mut clock, reader) = Clock::new(node.clock());
    let source = clock.add();
    let wide = Measurement::new(node.clock().now(), Span::ZERO, Span::SECOND);
    clock.push(source, wide.expect("valid"));
    let start = reader.now().mesh.expect("mesh time").latest;
    let at = start + Span::from_nanos(10_000_000_000);
    let done = wait(&node, &reader, at);
    sim.run_for(Span::from_nanos(5_000_000_000)).expect("runs");
    let narrow = Measurement::new(node.clock().now(), Span::ZERO, ms(10));
    clock.push(source, narrow.expect("valid"));
    sim.run().expect("the run ends");
    let Time { mesh, .. } = done.lock().expect("not poisoned").expect("done");
    let latest = mesh.expect("mesh time").latest;
    assert!(latest >= at, "{latest:?} before {at:?}");
}
