//! The table that a `hub` bench prints: per line, the ns per call of each timed round
//! and the allocations per call.

use std::time::{Duration, Instant};

/// One line of the table.
pub(crate) struct Line {
    name: &'static str,
    /// The timed calls of each round.
    calls: u64,
    /// The ns and allocations of the round so far.
    round: (u64, u64),
    /// The ns per call of each timed round.
    nanos: Vec<u64>,
    /// The allocations of all timed rounds.
    allocations: u64,
}

impl Line {
    /// A line of `calls` timed calls per round.
    pub(crate) fn new(name: &'static str, calls: usize) -> Self {
        Self {
            name,
            calls: u64::try_from(calls).expect("few"),
            round: (0, 0),
            nanos: Vec::new(),
            allocations: 0,
        }
    }

    /// Adds the ns and allocations of one call to the round.
    pub(crate) fn add(&mut self, (nanos, allocations): (u64, u64)) {
        self.round.0 += nanos;
        self.round.1 += allocations;
    }

    /// Ends a round, and keeps its figures when it is `timed`.
    pub(crate) fn close(&mut self, timed: bool) {
        let (nanos, allocations) = std::mem::take(&mut self.round);
        if timed {
            self.nanos.push(nanos / self.calls);
            self.allocations += allocations;
        }
    }

    /// The ns per call of the round at `percent`.
    fn at(&self, percent: usize) -> u64 {
        let mut nanos = self.nanos.clone();
        nanos.sort_unstable();
        nanos[nanos.len() * percent / 100]
    }
}

/// The result of `f`, and the ns it takes and the allocations that `allocator`
/// counts in it, as [`Line::add`] takes them.
#[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
pub(crate) fn timed<T>(
    allocator: &counting::Allocator,
    f: impl FnOnce() -> T,
) -> (T, (u64, u64)) {
    let ((value, span), allocations) = allocator.count(|| {
        let start = Instant::now();
        let value = f();
        (value, Instant::now().duration_since(start))
    });
    (value, (nanos(span), allocations))
}

fn nanos(span: Duration) -> u64 {
    u64::try_from(span.as_nanos()).expect("a call takes under 2^64 ns")
}

/// Prints `timer` and then `lines` under `title`. `net` is a line's p50 less the p50
/// of `timer`, the floor of each figure.
#[expect(clippy::print_stdout, reason = "a benchmark prints its results")]
pub(crate) fn print(title: &str, timer: &Line, lines: &[Line]) {
    println!("{title}");
    println!("pN: the round at percentile N");
    println!(
        "{:<14} {:>9} {:>9} {:>9} {:>9} {:>13}",
        "line", "p10", "p50", "p90", "net", "allocs/call"
    );
    let floor = timer.at(50);
    for line in std::iter::once(timer).chain(lines) {
        let calls = per(line.nanos.len()) * per(line.calls);
        let allocations = per(line.allocations) / calls;
        println!(
            "{:<14} {:>9} {:>9} {:>9} {:>9} {allocations:>13.2}",
            line.name,
            line.at(10),
            line.at(50),
            line.at(90),
            line.at(50).saturating_sub(floor)
        );
    }
}

/// `value` as a float, exact below 2^53.
#[expect(
    clippy::cast_precision_loss,
    clippy::as_conversions,
    reason = "a printed figure loses no digit it shows"
)]
fn per(value: impl TryInto<u64, Error: std::fmt::Debug>) -> f64 {
    value.try_into().expect("fits 64 bits") as f64
}
