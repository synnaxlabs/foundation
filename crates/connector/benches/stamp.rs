//! The cost of a stamp, on a sim node. Run with `cargo bench -p connector --bench
//! stamp`.
//!
//! - `now`: one read of mesh time by `Writer::now`.
//! - `request`: `Stamps::request` of a request that is ready at once, so the line is
//!   its two reads of mesh time and the rule of the stamp.
//! - `before`: `Fit::before` of a fit that bounds the clock, one value later in device
//!   time each call.
//! - `mesh`: `Fit::mesh` of that fit.
//! - `sample`: `Stamps::sample` of an interval 1 us later each call.
//!
//! Each figure is ps per call, so a change under 1 ns shows, with the count of the
//! allocations of the calls.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::hint::black_box;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use connector::kind::Table;
use connector::stamp::{Fit, Fitted, Stamps};
use connector::testing::{self, Rig};
use hub::writer::Writer;
use types::channel;
use types::name::Name;
use types::time::{Interval, Monotonic, Span, Stamp};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const CALLS: u64 = 10_000;
const WARMUP: usize = 10;
const ROUNDS: usize = 100;

fn main() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let lines = sim.run_on(&node, |node, tasks| async move {
        let env = hub::testing::Env {
            files: node.files(),
            clock: node.clock(),
            wall: node.wall(),
            entropy: node.entropy(),
            tasks,
        };
        let rig = Rig::open(env, node.net(), Table::new()).await;
        let connector: Name = "plant.stamp".parse().expect("a valid name");
        let first = channel::Key::from_u128(100);
        rig.define(testing::create_status(&connector, &[], first));
        let state = "plant.stamp.status.state".parse().expect("a valid name");
        let writer = rig.writer(connector, vec![state]).await;
        [
            time("now", || {
                black_box(writer.now());
            }),
            request(&writer),
            before(),
            mesh(),
            sample(),
        ]
    });
    print(&lines.expect("the run ends"));
}

/// The line `request`.
fn request(writer: &Writer) -> Line {
    let mut stamps = Stamps::default();
    let mut cx = Context::from_waker(Waker::noop());
    time("request", || {
        let request = pin!(stamps.request(writer, async { black_box(7_u8) }));
        let Poll::Ready(stamped) = request.poll(&mut cx) else {
            panic!("the request is ready")
        };
        black_box(stamped);
    })
}

/// A fit of a clock 500 ns behind mesh time, from one read at device time 0.
fn fit() -> Fit {
    let mut fit = Fit::default();
    let fitted =
        fit.between(Monotonic(0), Stamp::from_nanos(400), Stamp::from_nanos(500));
    assert_eq!(fitted, Fitted::Kept, "one reading fits");
    fit
}

/// The line `before`.
fn before() -> Line {
    let mut fit = fit();
    let mut device = 0;
    time("before", || {
        device += 1_000;
        let arrived = Stamp::from_nanos(i64::try_from(device).expect("small") + 500);
        let fitted = black_box(fit.before(Monotonic(device), black_box(arrived)));
        assert_eq!(fitted, Fitted::Kept, "a true reading fits");
    })
}

/// The line `mesh`.
fn mesh() -> Line {
    let fit = fit();
    let mut device = 0;
    time("mesh", || {
        device += 1_000;
        black_box(fit.mesh(black_box(Monotonic(device))));
    })
}

/// The line `sample`.
fn sample() -> Line {
    let mut stamps = Stamps::default();
    let mut earliest = Stamp::from_nanos(0);
    time("sample", || {
        earliest = earliest + Span::MICROSECOND;
        let interval = Interval {
            earliest,
            latest: earliest + Span::from_nanos(200),
        };
        black_box(stamps.sample(black_box(interval), earliest));
    })
}

/// The ps per call of each timed round, and the allocations of all of them.
struct Line {
    name: &'static str,
    picos: Vec<u64>,
    allocations: u64,
}

/// Times `ROUNDS` rounds of `CALLS` calls of `call`, after `WARMUP` rounds.
#[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
fn time(name: &'static str, mut call: impl FnMut()) -> Line {
    let mut line = Line {
        name,
        picos: Vec::new(),
        allocations: 0,
    };
    for round in 0..WARMUP + ROUNDS {
        let start = Instant::now();
        let ((), allocations) = ALLOCATOR.count(|| {
            for _ in 0..CALLS {
                call();
            }
        });
        let nanos = Instant::now().duration_since(start).as_nanos();
        if round >= WARMUP {
            let picos =
                u64::try_from(nanos * 1000).expect("a round takes under 2^64 ps");
            line.picos.push(picos / CALLS);
            line.allocations += allocations;
        }
    }
    line
}

/// Prints p10, p50, and p90 of the ps per call of each line, and its allocations.
#[expect(clippy::print_stdout, reason = "a benchmark prints its results")]
fn print(lines: &[Line]) {
    println!("ps per call over {ROUNDS} rounds of {CALLS} calls");
    println!(
        "{:<10} {:>7} {:>7} {:>7} {:>7}",
        "line", "p10", "p50", "p90", "allocs"
    );
    for line in lines {
        let mut picos = line.picos.clone();
        picos.sort_unstable();
        let at = |percent: usize| picos[picos.len() * percent / 100];
        println!(
            "{:<10} {:>7} {:>7} {:>7} {:>7}",
            line.name,
            at(10),
            at(50),
            at(90),
            line.allocations
        );
    }
}
