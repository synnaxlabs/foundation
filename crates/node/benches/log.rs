//! The cost of one event through the log of a node, before its write. Run with
//! `cargo bench -p node --bench log`.
//!
//! - `started`: the event `started`, with three fields, as text and as JSON.
//! - `stopped`: the event `stopped`, with no field.
//! - `parked`: the event `started` as text, 1 ms after the line before, so it also
//!   wakes the thread `log`.
//! - `filtered`: a `debug` event under the level `info`, as noq-proto writes for each
//!   packet.
//! - `undispatched`: the control, the same event with no dispatch, as before a node
//!   had a log.
//! - `span`: a `trace` span entered and dropped under the level `trace`.
//! - `status`: the status line `error` of a connector, with the span of the
//!   connector as its parent, under the level `info`.
//! - `recorded`: a record of a field of a `trace` span under the level `trace`, as
//!   noq-proto makes for a frame that names a path.
//! - `build`: the build of the dispatch, once for each node.
//!
//! The writer is `io::sink`, so no line counts its write to stderr.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::io;
use std::net::SocketAddr;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use divan::Bencher;
use tracing::Level;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

fn main() {
    check();
    divan::main();
}

fn dispatch(json: bool, level: Level) -> node::bench::Log {
    open(log(json, level))
}

fn open(log: node::Log) -> node::bench::Log {
    let threads = os::threads().expect("threads");
    let wall = os::wall().expect("a wall clock");
    let (_mesh, reader) = clock::Clock::new(os::clock());
    node::bench::log(log, &threads, wall, reader)
}

fn log(json: bool, level: Level) -> node::Log {
    node::Log {
        writer: Box::new(io::sink()),
        format: ops::Log { json, level },
    }
}

/// A sink that sends once, at its first write.
struct First(Option<mpsc::SyncSender<()>>);

impl io::Write for First {
    fn write(&mut self, line: &[u8]) -> io::Result<usize> {
        if let Some(written) = self.0.take() {
            written.send(()).unwrap_or_default();
        }
        Ok(line.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn started() {
    let address: SocketAddr = ([127, 0, 0, 1], 9090).into();
    tracing::info!(
        name = %divan::black_box("edge"),
        address = %divan::black_box(address),
        shards = divan::black_box(16_usize),
        "started"
    );
}

fn filtered() {
    tracing::debug!(pn = divan::black_box(7_u64), "packet");
}

/// An event under the level allocates nothing, and one at the level allocates its line.
/// The counts start once the thread `log` has written a line, then 10 ms pass with no
/// allocation: the allocator counts each thread, and that thread allocates as it starts
/// and parks.
#[expect(
    clippy::disallowed_methods,
    reason = "the check waits for the thread log"
)]
fn check() {
    let (written, wrote) = mpsc::sync_channel(1);
    let d = open(node::Log {
        writer: Box::new(First(Some(written))),
        ..log(false, Level::INFO)
    });
    tracing::dispatcher::with_default(d.dispatch(), started);
    wrote.recv().expect("the thread log writes");
    let idle = (0..100).any(|_| {
        ALLOCATOR
            .count(|| thread::sleep(Duration::from_millis(10)))
            .1
            == 0
    });
    assert!(idle, "allocations while the thread log waits");
    let ((), allocations) =
        tracing::dispatcher::with_default(d.dispatch(), || ALLOCATOR.count(filtered));
    assert_eq!(allocations, 0, "allocations of a filtered event");
    let ((), allocations) =
        tracing::dispatcher::with_default(d.dispatch(), || ALLOCATOR.count(started));
    assert_ne!(allocations, 0, "allocations of an event at the level");
}

#[divan::bench(args = [false, true])]
fn started_line(bencher: Bencher<'_, '_>, json: bool) {
    let d = dispatch(json, Level::INFO);
    tracing::dispatcher::with_default(d.dispatch(), || bencher.bench_local(started));
}

#[divan::bench]
fn stopped(bencher: Bencher<'_, '_>) {
    let d = dispatch(false, Level::INFO);
    tracing::dispatcher::with_default(d.dispatch(), || {
        bencher.bench_local(|| tracing::info!("stopped"));
    });
}

#[divan::bench(sample_count = 200, sample_size = 1)]
fn parked(bencher: Bencher<'_, '_>) {
    let d = dispatch(false, Level::INFO);
    #[expect(clippy::disallowed_methods, reason = "the thread log parks")]
    let idle = || std::thread::sleep(Duration::from_millis(1));
    tracing::dispatcher::with_default(d.dispatch(), || {
        bencher.with_inputs(idle).bench_local_values(|()| started());
    });
}

#[divan::bench]
fn filtered_event(bencher: Bencher<'_, '_>) {
    let d = dispatch(false, Level::INFO);
    tracing::dispatcher::with_default(d.dispatch(), || bencher.bench_local(filtered));
}

#[divan::bench]
fn span(bencher: Bencher<'_, '_>) {
    let d = dispatch(false, Level::TRACE);
    tracing::dispatcher::with_default(d.dispatch(), || {
        bencher.bench_local(|| {
            let span = tracing::trace_span!("frame", ty = %divan::black_box("STREAM"));
            drop(span.entered());
        });
    });
}

#[divan::bench]
fn status(bencher: Bencher<'_, '_>) {
    let d = dispatch(false, Level::INFO);
    tracing::dispatcher::with_default(d.dispatch(), || {
        let span =
            tracing::error_span!("connector", connector = "plant.a", kind = "opcua");
        bencher.bench_local(|| {
            let error = divan::black_box("no route to host");
            tracing::warn!(parent: &span, error, "error");
        });
    });
}

#[divan::bench]
fn recorded(bencher: Bencher<'_, '_>) {
    let d = dispatch(false, Level::TRACE);
    tracing::dispatcher::with_default(d.dispatch(), || {
        let span = tracing::trace_span!("frame", path = tracing::field::Empty);
        bencher.bench_local(|| {
            span.record("path", tracing::field::display(divan::black_box(&1_u64)))
        });
    });
}

#[divan::bench]
fn build(bencher: Bencher<'_, '_>) {
    let threads = os::threads().expect("threads");
    let wall = os::wall().expect("a wall clock");
    let (_mesh, reader) = clock::Clock::new(os::clock());
    bencher
        .with_inputs(|| (wall.clone(), reader.clone()))
        .bench_local_values(|(wall, reader)| {
            node::bench::log(log(false, Level::INFO), &threads, wall, reader)
        });
}

#[divan::bench]
fn undispatched(bencher: Bencher<'_, '_>) {
    bencher.bench_local(filtered);
}
