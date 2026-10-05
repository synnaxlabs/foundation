//! Measures the cross-shard handoff: producer threads push frames through `ring`
//! to shard threads that each run a Tokio `LocalRuntime`, against the same work
//! done inline on the shard. Prints one Markdown table line per run. `run.sh` drives
//! the matrix.

#![expect(
    clippy::disallowed_methods,
    reason = "a benchmark reads the real clock, its arguments, and spawns threads"
)]
#![expect(clippy::print_stdout, reason = "the run prints its table line")]

use std::hint::{black_box, spin_loop};
use std::num::NonZeroUsize;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use ring::{Config, Consumer, Full, Producer};
use tokio::runtime::{Builder, LocalOptions, LocalRuntime};
use tokio::task::coop::consume_budget;
use tokio::task::spawn_local;

type Error = Box<dyn std::error::Error + Send + Sync>;

const USAGE: &str = "\
usage:
  handoff columns                   the header of the table lines
  handoff handoff [options]         producers push to shard tasks through `ring`
  handoff inline [options]          each shard produces and processes in place
options:
  producers=<n>   producer threads, a divisor of 64 (default 8); `inline` has none
  shards=<n>      shard threads, a divisor of 64 (default 8)
  work=<n>        passes over the 1,000 f64 payload per frame (default 1)
  secs=<n>        seconds to run (default 10)
  pace=<n>        frames per second per producing thread; 0 is unthrottled (default)
  spin=<n>        nanoseconds a shard task spins on an empty ring before it parks
                  (default 0)
  capacity=<n>    slots per ring (default 1024)
  cpu=<n>         pin shards from this core and producers after them (default off)
";

/// Samples per frame.
const PAYLOAD: usize = 1_000;

/// Indexes the producers share out. Each index has one sequence counter on the
/// shard that owns it.
const INDEXES: u32 = 64;

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (design, options) = match args.split_first() {
        Some((&"columns", [])) => {
            println!("{}", Report::COLUMNS);
            return Ok(());
        }
        Some((&"handoff", rest)) => (Design::Handoff, Options::parse(rest)?),
        Some((&"inline", rest)) => (Design::Inline, Options::parse(rest)?),
        _ => return Err(USAGE.into()),
    };
    let report = match design {
        Design::Handoff => handoff(&options),
        Design::Inline => inline(&options),
    };
    println!("{}", report.line(design, &options));
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Design {
    Handoff,
    Inline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Options {
    producers: u32,
    shards: u32,
    work: u32,
    secs: Duration,
    /// Frames per second per producing thread. Zero is unthrottled.
    pace: u64,
    spin: Duration,
    capacity: NonZeroUsize,
    /// The first core to pin to. `None` leaves the threads unpinned.
    cpu: Option<usize>,
}

impl Options {
    fn parse(args: &[&str]) -> Result<Self, Error> {
        let mut options = Self {
            producers: 8,
            shards: 8,
            work: 1,
            secs: Duration::from_secs(10),
            pace: 0,
            spin: Duration::ZERO,
            capacity: NonZeroUsize::new(1024).expect("not zero"),
            cpu: None,
        };
        for arg in args {
            match arg.split_once('=') {
                Some(("producers", n)) => options.producers = n.parse()?,
                Some(("shards", n)) => options.shards = n.parse()?,
                Some(("work", n)) => options.work = n.parse()?,
                Some(("secs", n)) => options.secs = Duration::from_secs_f64(n.parse()?),
                Some(("pace", n)) => options.pace = n.parse()?,
                Some(("spin", n)) => options.spin = Duration::from_nanos(n.parse()?),
                Some(("capacity", n)) => options.capacity = n.parse()?,
                Some(("cpu", n)) => options.cpu = Some(n.parse()?),
                _ => return Err(format!("unknown option {arg:?}\n{USAGE}").into()),
            }
        }
        for (name, count) in
            [("producers", options.producers), ("shards", options.shards)]
        {
            if count == 0 || !INDEXES.is_multiple_of(count) {
                return Err(format!("{name}={count} does not divide {INDEXES}").into());
            }
        }
        Ok(options)
    }
}

/// One frame of the handoff. The payload is built once per producer and shared.
struct Frame {
    sent: Instant,
    index: u32,
    seq: u64,
    payload: Arc<[f64; PAYLOAD]>,
}

/// What one shard task counted.
struct Tally {
    latency: Histogram<u64>,
    /// Pops that found the ring empty and parked.
    parks: u64,
}

impl Tally {
    fn new() -> Self {
        Self {
            latency: Histogram::new_with_bounds(1, 10_000_000_000, 3)
                .expect("the bounds are valid"),
            parks: 0,
        }
    }

    fn record(&mut self, sent: Instant) {
        let nanos = u64::try_from(sent.elapsed().as_nanos()).expect("under 585 years");
        self.latency
            .record(nanos.max(1))
            .expect("under the top bound");
    }
}

/// What one producing thread counted.
struct Sent {
    pushed: u64,
    /// Frames a full ring refused under pacing.
    drops: u64,
}

/// The outcome of one run.
struct Report {
    latency: Histogram<u64>,
    parks: u64,
    pushed: u64,
    drops: u64,
    elapsed: Duration,
    pinned: bool,
}

impl Report {
    const COLUMNS: &str = "| design | P | S | work | spin | pace | M samples/s | \
        p50 µs | p99 µs | p99.9 µs | max µs | parked | drops | pinned |\n\
        | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | \
        --- | --- |";

    fn delivered(&self) -> u64 {
        self.latency.len()
    }

    #[expect(clippy::cast_precision_loss, reason = "counts of frames, under 2^53")]
    fn line(&self, design: Design, options: &Options) -> String {
        let micros = |nanos: u64| nanos as f64 / 1_000.0;
        let quantile = |q: f64| micros(self.latency.value_at_quantile(q));
        let per_second = self.delivered() as f64 / self.elapsed.as_secs_f64();
        let parked = self.parks as f64 / self.delivered().max(1) as f64;
        let producers = match design {
            Design::Handoff => options.producers.to_string(),
            Design::Inline => "-".into(),
        };
        format!(
            "| {design:?} | {producers} | {} | {} | {} | {} | {:.2} | {:.2} | {:.2} | \
            {:.2} | {:.0} | {:.1}% | {} | {} |",
            options.shards,
            options.work,
            options.spin.as_nanos(),
            options.pace,
            per_second * PAYLOAD as f64 / 1e6,
            quantile(0.5),
            quantile(0.99),
            quantile(0.999),
            micros(self.latency.max()),
            parked * 100.0,
            self.drops,
            self.pinned,
        )
    }
}

/// Design A: `producers` threads push frames through one ring per (producer, shard)
/// to `shards` threads that each await their rings on a `LocalRuntime`.
fn handoff(options: &Options) -> Report {
    let shards = options.shards as usize;
    let mut rings: Vec<Vec<Producer<Frame>>> = Vec::new();
    let mut consumers: Vec<Vec<Consumer<Frame>>> =
        (0..shards).map(|_| Vec::new()).collect();
    for _ in 0..options.producers {
        let mut producers = Vec::new();
        for shard in &mut consumers {
            let (producer, consumer) = ring::new(Config {
                capacity: options.capacity,
            });
            producers.push(producer);
            shard.push(consumer);
        }
        rings.push(producers);
    }
    let start = &Barrier::new(shards + rings.len() + 1);
    let mut pinned = true;
    thread::scope(|scope| {
        let shard_threads: Vec<_> = consumers
            .into_iter()
            .enumerate()
            .map(|(shard, consumers)| {
                let core = options.cpu.map(|cpu| cpu + shard);
                scope.spawn(move || shard_thread(core, consumers, start, options))
            })
            .collect();
        let producer_threads: Vec<_> = rings
            .into_iter()
            .enumerate()
            .map(|(producer, rings)| {
                let core = options.cpu.map(|cpu| cpu + shards + producer);
                let producer = u32::try_from(producer).expect("a small count");
                scope.spawn(move || produce(core, producer, rings, start, options))
            })
            .collect();
        start.wait();
        let started = Instant::now();
        let mut sent = Sent {
            pushed: 0,
            drops: 0,
        };
        for thread in producer_threads {
            let (accepted, counts) = thread.join().expect("the producer ran");
            pinned &= accepted;
            sent.pushed += counts.pushed;
            sent.drops += counts.drops;
        }
        let mut report = Report {
            latency: Histogram::new_with_bounds(1, 10_000_000_000, 3)
                .expect("the bounds are valid"),
            parks: 0,
            pushed: sent.pushed,
            drops: sent.drops,
            elapsed: Duration::ZERO,
            pinned,
        };
        for thread in shard_threads {
            let (accepted, tallies) = thread.join().expect("the shard ran");
            report.pinned &= accepted;
            for tally in tallies {
                report.latency.add(&tally.latency).expect("the same bounds");
                report.parks += tally.parks;
            }
        }
        report.elapsed = started.elapsed();
        report
    })
}

/// Pins the thread to `core`. Returns `false` with no core or a refused pin.
fn pin(core: Option<usize>) -> bool {
    core.is_some_and(|id| core_affinity::set_for_current(core_affinity::CoreId { id }))
}

fn local_runtime() -> LocalRuntime {
    Builder::new_current_thread()
        .enable_all()
        .build_local(LocalOptions::default())
        .expect("the runtime builds")
}

fn shard_thread(
    core: Option<usize>,
    consumers: Vec<Consumer<Frame>>,
    start: &Barrier,
    options: &Options,
) -> (bool, Vec<Tally>) {
    let pinned = pin(core);
    let runtime = local_runtime();
    start.wait();
    let tallies = runtime.block_on(async {
        let tasks: Vec<_> = consumers
            .into_iter()
            .map(|consumer| spawn_local(consume(consumer, options.work, options.spin)))
            .collect();
        let mut tallies = Vec::new();
        for task in tasks {
            tallies.push(task.await.expect("the task ran"));
        }
        tallies
    });
    (pinned, tallies)
}

/// Pops until the producer is gone. Spins for `spin` on an empty ring, then parks
/// in `pop`, which wakes the runtime from its driver. A ready `pop` never yields,
/// so each frame spends Tokio's budget: a busy task cannot starve the other rings.
async fn consume(mut consumer: Consumer<Frame>, work: u32, spin: Duration) -> Tally {
    let mut tally = Tally::new();
    let mut seqs = [0_u64; INDEXES as usize];
    loop {
        let popped = consumer.try_pop().or_else(|| spin_pop(&mut consumer, spin));
        let frame = if let Some(frame) = popped {
            frame
        } else {
            tally.parks += 1;
            match consumer.pop().await {
                Some(frame) => frame,
                None => return tally,
            }
        };
        process(
            &frame.payload,
            work,
            &mut seqs[frame.index as usize],
            frame.seq,
        );
        tally.record(frame.sent);
        consume_budget().await;
    }
}

fn spin_pop(consumer: &mut Consumer<Frame>, spin: Duration) -> Option<Frame> {
    if spin.is_zero() {
        return None;
    }
    let until = Instant::now() + spin;
    loop {
        if let Some(frame) = consumer.try_pop() {
            return Some(frame);
        }
        if Instant::now() >= until {
            return None;
        }
        spin_loop();
    }
}

/// The per-frame work: `passes` over the payload and the index's sequence update.
/// Panics when a frame arrives out of order.
fn process(payload: &[f64; PAYLOAD], passes: u32, seq: &mut u64, expected: u64) {
    let mut sum = 0.0;
    for _ in 0..passes {
        for sample in payload {
            sum += sample * 1.000_001;
        }
    }
    black_box(sum);
    assert_eq!(*seq, expected, "a frame arrived out of order");
    *seq += 1;
}

/// Pushes frames for `secs`, each index in turn to the shard `index % shards`.
/// Paced, a full ring drops the frame; unthrottled, the thread spins until it fits.
fn produce(
    core: Option<usize>,
    producer: u32,
    mut rings: Vec<Producer<Frame>>,
    start: &Barrier,
    options: &Options,
) -> (bool, Sent) {
    let pinned = pin(core);
    start.wait();
    let payload = Arc::new([1.5_f64; PAYLOAD]);
    let per_producer = INDEXES / options.producers;
    let first = producer * per_producer;
    let mut seqs = vec![0_u64; per_producer as usize];
    let mut sent = Sent {
        pushed: 0,
        drops: 0,
    };
    let mut pace = Pace::new(options.pace);
    let deadline = Instant::now() + options.secs;
    let mut next = 0;
    while pace.wait() < deadline {
        let index = first + next;
        let seq = &mut seqs[next as usize];
        next = (next + 1) % per_producer;
        let ring = &mut rings[(index % options.shards) as usize];
        let mut frame = Frame {
            sent: Instant::now(),
            index,
            seq: *seq,
            payload: Arc::clone(&payload),
        };
        loop {
            match ring.push(frame) {
                Ok(()) => {
                    *seq += 1;
                    sent.pushed += 1;
                    break;
                }
                Err(Full(_)) if options.pace > 0 => {
                    sent.drops += 1;
                    break;
                }
                Err(Full(returned)) => {
                    frame = returned;
                    spin_loop();
                }
            }
        }
    }
    (pinned, sent)
}

/// A fixed schedule of frames. Unpaced, every frame is due at once.
struct Pace {
    interval: Duration,
    due: Instant,
}

impl Pace {
    fn new(per_second: u64) -> Self {
        let interval = match per_second {
            0 => Duration::ZERO,
            n => Duration::from_secs(1) / u32::try_from(n).expect("a rate under 2^32"),
        };
        Self {
            interval,
            due: Instant::now(),
        }
    }

    /// Spins until the next frame is due and returns the time it is sent.
    fn wait(&mut self) -> Instant {
        if self.interval.is_zero() {
            return Instant::now();
        }
        let due = self.due;
        while Instant::now() < due {
            spin_loop();
        }
        self.due += self.interval;
        due
    }
}

/// Design C: `shards` threads each produce their own indexes and process them in
/// place. Nothing crosses a thread, so no runtime is needed.
fn inline(options: &Options) -> Report {
    let start = &Barrier::new(options.shards as usize + 1);
    thread::scope(|scope| {
        let threads: Vec<_> = (0..options.shards)
            .map(|shard| {
                let core = options.cpu.map(|cpu| cpu + shard as usize);
                scope.spawn(move || {
                    let pinned = pin(core);
                    start.wait();
                    (pinned, produce_inline(options))
                })
            })
            .collect();
        start.wait();
        let started = Instant::now();
        let mut report = Report {
            latency: Histogram::new_with_bounds(1, 10_000_000_000, 3)
                .expect("the bounds are valid"),
            parks: 0,
            pushed: 0,
            drops: 0,
            elapsed: Duration::ZERO,
            pinned: true,
        };
        for thread in threads {
            let (pinned, tally) = thread.join().expect("the shard ran");
            report.pinned &= pinned;
            report.latency.add(&tally.latency).expect("the same bounds");
        }
        report.elapsed = started.elapsed();
        report.pushed = report.latency.len();
        report
    })
}

fn produce_inline(options: &Options) -> Tally {
    let payload = [1.5_f64; PAYLOAD];
    let per_shard = (INDEXES / options.shards) as usize;
    let mut seqs = vec![0_u64; per_shard];
    let mut tally = Tally::new();
    let mut pace = Pace::new(options.pace);
    let deadline = Instant::now() + options.secs;
    let mut next = 0;
    while pace.wait() < deadline {
        let sent = Instant::now();
        let seq = &mut seqs[next];
        next = (next + 1) % per_shard;
        process(&payload, options.work, seq, *seq);
        tally.record(sent);
    }
    tally
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use super::{Design, Options, USAGE, handoff, inline};

    fn options(pace: u64) -> Options {
        Options {
            producers: 2,
            shards: 2,
            work: 1,
            secs: Duration::from_millis(200),
            pace,
            spin: Duration::ZERO,
            capacity: NonZeroUsize::new(1024).expect("not zero"),
            cpu: None,
        }
    }

    #[test]
    fn handoff_records_every_frame_it_delivered() {
        let report = handoff(&options(2_000));
        assert!(report.pushed > 0, "nothing pushed");
        assert_eq!(report.delivered() + report.drops, report.pushed);
        assert!(report.parks > 0, "a paced consumer parks");
        assert!(!report.pinned, "no core was asked for");
        let line = report.line(Design::Handoff, &options(2_000));
        assert!(
            line.starts_with("| Handoff | 2 | 2 | 1 | 0 | 2000 | "),
            "{line}"
        );
    }

    #[test]
    fn inline_records_every_frame() {
        let report = inline(&options(2_000));
        assert!(report.pushed > 0, "nothing produced");
        assert_eq!(report.delivered(), report.pushed);
        assert_eq!((report.drops, report.parks), (0, 0));
    }

    #[test]
    fn unthrottled_handoff_delivers_in_order() {
        let report = handoff(&options(0));
        assert!(report.pushed > 1_000, "too few frames: {}", report.pushed);
        assert_eq!(report.delivered(), report.pushed);
        assert_eq!(report.drops, 0, "unthrottled never drops");
    }

    #[test]
    fn args_reject_an_unknown_option() {
        let error = Options::parse(&["work=2", "rate=5"]).expect_err("unknown");
        assert_eq!(
            error.to_string(),
            format!("unknown option \"rate=5\"\n{USAGE}")
        );
    }

    #[test]
    fn args_parse_each_option() {
        let parsed = Options::parse(&[
            "producers=4",
            "shards=2",
            "work=8",
            "secs=0.5",
            "pace=10000",
            "spin=20000",
            "capacity=16",
            "cpu=3",
        ])
        .expect("every option is known");
        assert_eq!(
            parsed,
            Options {
                producers: 4,
                shards: 2,
                work: 8,
                secs: Duration::from_millis(500),
                pace: 10_000,
                spin: Duration::from_micros(20),
                capacity: NonZeroUsize::new(16).expect("not zero"),
                cpu: Some(3),
            }
        );
        assert_eq!(Options::parse(&[]).expect("defaults").producers, 8);
    }

    #[test]
    fn args_reject_a_count_that_does_not_divide_the_indexes() {
        let error = Options::parse(&["producers=3"]).expect_err("3 does not divide 64");
        assert_eq!(error.to_string(), "producers=3 does not divide 64");
        let error = Options::parse(&["shards=0"]).expect_err("zero shards");
        assert_eq!(error.to_string(), "shards=0 does not divide 64");
    }
}
