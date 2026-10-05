//! Measures the cross-shard handoff: producer threads push frames through `ring`
//! to shard threads that each run one loop on a Tokio `LocalRuntime`, against the
//! same work done inline on the shard. Prints one Markdown table line per run.
//! `run.sh` drives the matrix.

#![expect(
    clippy::disallowed_methods,
    reason = "a benchmark reads the real clock, its arguments, and spawns threads"
)]
#![expect(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "the run prints its table line or its error"
)]

use std::future::poll_fn;
use std::hint::{black_box, spin_loop};
use std::num::{NonZeroU32, NonZeroUsize};
use std::panic::resume_unwind;
use std::process::ExitCode;
use std::sync::{Arc, Barrier};
use std::task::Poll;
use std::thread::{self, ScopedJoinHandle};
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use ring::{Config, Consumer, Full, Producer};
use tokio::runtime::{Builder, LocalOptions, LocalRuntime};

type Error = Box<dyn std::error::Error + Send + Sync>;

const USAGE: &str = "\
usage:
  handoff columns                   the header of the table lines
  handoff ring [options]            producers push to shard loops through `ring`
  handoff inline [options]          each shard makes and processes its own frames
options:
  producers=<n>   producer threads, a divisor of 64 (default 8); `ring` only
  shards=<n>      shard threads, a divisor of 64 (default 8); with `ring`,
                  producers × shards is at most 64
  work=<n>        passes over the 1,000 f64 payload per frame (default 1)
  secs=<n>        seconds to run (default 10)
  pace=<n>        frames per second per producing thread, at most 10^9; 0 is
                  unthrottled (default)
  spin=<n>        nanoseconds a shard spins on empty rings before it parks
                  (default 0); `ring` only
  capacity=<n>    slots per ring (default 1024); `ring` only
  cpu=<n>         pin shards from this core and producers after them; Linux only
                  (default off)
";

/// Samples per frame.
const PAYLOAD: usize = 1_000;

/// Indexes the producers share out. Each index has one sequence counter on the
/// shard that owns it.
const INDEXES: u32 = 64;

/// The highest pace: one frame per nanosecond.
const FASTEST: u32 = 1_000_000_000;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match run(&args) {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the command in `args` and returns the line to print.
fn run(args: &[&str]) -> Result<String, Error> {
    let (design, rest) = match args.split_first() {
        Some((&"columns", [])) => return Ok(Report::COLUMNS.into()),
        Some((&"ring", rest)) => (Design::Ring, rest),
        Some((&"inline", rest)) => (Design::Inline, rest),
        _ => return Err(USAGE.into()),
    };
    let options = Options::parse(design, rest)?;
    let report = match design {
        Design::Ring => run_ring(&options),
        Design::Inline => run_inline(&options),
    };
    Ok(report.line(design, &options))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Design {
    /// Design A: frames cross from producer threads to shards through rings.
    Ring,
    /// Design C: each shard makes its own frames.
    Inline,
}

impl Design {
    /// The subcommand, also the first column of the table.
    fn name(self) -> &'static str {
        match self {
            Self::Ring => "ring",
            Self::Inline => "inline",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Options {
    producers: u32,
    shards: u32,
    work: u32,
    secs: Duration,
    /// Frames per second per producing thread. `None` is unthrottled.
    pace: Option<NonZeroU32>,
    spin: Duration,
    capacity: NonZeroUsize,
    /// The first core to pin to. `None` leaves the threads unpinned.
    cpu: Option<usize>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            producers: 8,
            shards: 8,
            work: 1,
            secs: Duration::from_secs(10),
            pace: None,
            spin: Duration::ZERO,
            capacity: NonZeroUsize::new(1024).expect("invariant: 1024 is not zero"),
            cpu: None,
        }
    }
}

impl Options {
    fn parse(design: Design, args: &[&str]) -> Result<Self, Error> {
        let mut options = Self::default();
        let ring = design == Design::Ring;
        for arg in args {
            match arg.split_once('=') {
                Some(("producers", n)) if ring => options.producers = n.parse()?,
                Some(("shards", n)) => options.shards = n.parse()?,
                Some(("work", n)) => options.work = n.parse()?,
                Some(("secs", n)) => {
                    options.secs =
                        Duration::try_from_secs_f64(n.parse()?).map_err(|error| {
                            format!("secs={n} is not a duration: {error}")
                        })?;
                }
                Some(("pace", n)) => options.pace = NonZeroU32::new(n.parse()?),
                Some(("spin", n)) if ring => {
                    options.spin = Duration::from_nanos(n.parse()?);
                }
                Some(("capacity", n)) if ring => options.capacity = n.parse()?,
                Some(("cpu", n)) => options.cpu = Some(n.parse()?),
                _ => {
                    return Err(format!(
                        "unknown option {arg:?} for {}\n{USAGE}",
                        design.name()
                    )
                    .into());
                }
            }
        }
        if let Some(pace) = options.pace.filter(|pace| pace.get() > FASTEST) {
            return Err(
                format!("pace={pace} is above {FASTEST} frames per second").into()
            );
        }
        for (name, count) in
            [("producers", options.producers), ("shards", options.shards)]
        {
            if count == 0 || !INDEXES.is_multiple_of(count) {
                return Err(format!("{name}={count} does not divide {INDEXES}").into());
            }
        }
        let rings = options.producers * options.shards;
        if ring && rings > INDEXES {
            return Err(format!(
                "producers={} × shards={} is above {INDEXES}, so a producer misses \
                shards",
                options.producers, options.shards
            )
            .into());
        }
        if let Some(cpu) = options.cpu.filter(|_| !cfg!(target_os = "linux")) {
            return Err(format!("cpu={cpu} pins threads, and only Linux can").into());
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

/// What shards counted.
struct Tally {
    latency: Histogram<u64>,
    /// Frames that came after the shard slept.
    parks: u64,
}

impl Default for Tally {
    fn default() -> Self {
        Self {
            latency: Histogram::new_with_bounds(1, 10_000_000_000, 3)
                .expect("invariant: the bounds are valid"),
            parks: 0,
        }
    }
}

impl Tally {
    fn record(&mut self, sent: Instant) {
        let nanos = u64::try_from(sent.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.latency.saturating_record(nanos);
    }

    fn add(&mut self, other: &Self) {
        self.latency
            .add(&other.latency)
            .expect("invariant: tallies have the same bounds");
        self.parks += other.parks;
    }
}

/// What producing threads counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Sent {
    /// Frames pushed or dropped. A frame still waiting for room at the end is not
    /// offered.
    offered: u64,
    /// Frames a full ring refused under pacing.
    drops: u64,
}

impl Sent {
    fn add(&mut self, other: Self) {
        self.offered += other.offered;
        self.drops += other.drops;
    }
}

/// The outcome of one run.
#[derive(Default)]
struct Report {
    tally: Tally,
    sent: Sent,
    elapsed: Duration,
}

impl Report {
    const COLUMNS: &str = "| design | P | S | work | spin | pace | M samples/s | \
        p50 µs | p99 µs | p99.9 µs | max µs | parked | drops |\n\
        | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- \
        | --- |";

    fn delivered(&self) -> u64 {
        self.tally.latency.len()
    }

    #[expect(clippy::cast_precision_loss, reason = "counts of frames, under 2^53")]
    fn line(&self, design: Design, options: &Options) -> String {
        let latency = &self.tally.latency;
        let micros = |nanos: u64| nanos as f64 / 1_000.0;
        let quantile = |q: f64| micros(latency.value_at_quantile(q));
        let per_second = self.delivered() as f64 / self.elapsed.as_secs_f64();
        let parked = self.tally.parks as f64 / self.delivered().max(1) as f64;
        let producers = match design {
            Design::Ring => options.producers.to_string(),
            Design::Inline => "-".into(),
        };
        format!(
            "| {} | {producers} | {} | {} | {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | \
            {:.0} | {:.1}% | {} |",
            design.name(),
            options.shards,
            options.work,
            options.spin.as_nanos(),
            options.pace.map_or(0, NonZeroU32::get),
            per_second * PAYLOAD as f64 / 1e6,
            quantile(0.5),
            quantile(0.99),
            quantile(0.999),
            micros(latency.max()),
            parked * 100.0,
            self.sent.drops,
        )
    }
}

/// The core of each thread: shards take the cores from `cpu` on, producers the
/// cores after them.
#[derive(Clone, Copy)]
struct Cores {
    first: Option<usize>,
    shards: usize,
}

impl Cores {
    fn new(options: &Options) -> Self {
        Self {
            first: options.cpu,
            shards: options.shards as usize,
        }
    }

    fn shard(self, shard: usize) -> Option<usize> {
        self.first.map(|cpu| cpu + shard)
    }

    fn producer(self, producer: usize) -> Option<usize> {
        self.first.map(|cpu| cpu + self.shards + producer)
    }
}

/// Pins the calling thread to `core`, if any, then waits at `start`. A refused pin
/// panics after the wait, so no other thread waits for this one forever.
fn pin_and_wait(core: Option<usize>, start: &Barrier) {
    let refused = core
        .filter(|&id| !core_affinity::set_for_current(core_affinity::CoreId { id }));
    start.wait();
    if let Some(id) = refused {
        panic!("core {id} refused to pin a thread");
    }
}

/// Joins `thread`, and panics with its panic.
fn join<T>(thread: ScopedJoinHandle<'_, T>) -> T {
    thread.join().unwrap_or_else(|panic| resume_unwind(panic))
}

/// Design A: `producers` threads push frames through one ring per (producer, shard)
/// to `shards` threads, each running one loop on a `LocalRuntime`.
fn run_ring(options: &Options) -> Report {
    let shards = options.shards as usize;
    let mut producers: Vec<Vec<Producer<Frame>>> = Vec::new();
    let mut consumers: Vec<Vec<Consumer<Frame>>> =
        (0..shards).map(|_| Vec::new()).collect();
    for _ in 0..options.producers {
        let mut ends = Vec::new();
        for shard in &mut consumers {
            let (producer, consumer) = ring::new(Config {
                capacity: options.capacity,
            });
            ends.push(producer);
            shard.push(consumer);
        }
        producers.push(ends);
    }
    let cores = Cores::new(options);
    let start = &Barrier::new(shards + producers.len() + 1);
    thread::scope(|scope| {
        let shard_threads: Vec<_> = consumers
            .into_iter()
            .enumerate()
            .map(|(shard, consumers)| {
                scope.spawn(move || {
                    let runtime = local_runtime();
                    pin_and_wait(cores.shard(shard), start);
                    runtime.block_on(consume(consumers, options))
                })
            })
            .collect();
        let producer_threads: Vec<_> = producers
            .into_iter()
            .zip(0..)
            .map(|(rings, producer)| {
                scope.spawn(move || {
                    pin_and_wait(cores.producer(producer as usize), start);
                    produce(producer, rings, options)
                })
            })
            .collect();
        start.wait();
        let started = Instant::now();
        let mut report = Report::default();
        for thread in producer_threads {
            report.sent.add(join(thread));
        }
        for thread in shard_threads {
            report.tally.add(&join(thread));
        }
        report.elapsed = started.elapsed();
        report
    })
}

fn local_runtime() -> LocalRuntime {
    Builder::new_current_thread()
        .enable_all()
        .build_local(LocalOptions::default())
        .unwrap_or_else(|error| {
            panic!("the shard's Tokio runtime did not build: {error}")
        })
}

/// One shard's state: the next sequence number of each index and what it counted.
struct Shard {
    work: u32,
    seqs: [u64; INDEXES as usize],
    tally: Tally,
}

impl Shard {
    fn new(work: u32) -> Self {
        Self {
            work,
            seqs: [0; INDEXES as usize],
            tally: Tally::default(),
        }
    }

    /// Does the per-frame work: `work` passes over the payload and the index's
    /// sequence check. Then records the frame's latency. Panics when a frame arrives
    /// out of order.
    fn take(&mut self, frame: &Frame) {
        let mut sum = 0.0;
        for _ in 0..self.work {
            for sample in frame.payload.iter() {
                sum += sample * 1.000_001;
            }
        }
        black_box(sum);
        let seq = &mut self.seqs[frame.index as usize];
        assert_eq!(
            *seq, frame.seq,
            "index {} got a frame out of order",
            frame.index
        );
        *seq += 1;
        self.tally.record(frame.sent);
    }
}

/// Runs one shard until every producer is gone. It takes one frame from each ring in
/// turn. When every ring is empty, it spins over them for `spin`, then parks on all
/// of them at once. A ring whose producer is gone looks empty to `try_pop`, so each
/// costs one spin at the end of the run.
async fn consume(mut consumers: Vec<Consumer<Frame>>, options: &Options) -> Tally {
    let mut shard = Shard::new(options.work);
    let mut next = 0;
    while !consumers.is_empty() {
        let popped = sweep(&mut consumers, &mut next)
            .or_else(|| spin(&mut consumers, &mut next, options.spin));
        if let Some(frame) = popped {
            shard.take(&frame);
            continue;
        }
        let mut pops: Vec<_> = consumers
            .iter_mut()
            .map(|consumer| Box::pin(consumer.pop()))
            .collect();
        let mut slept = false;
        let (ring, popped) = poll_fn(|cx| {
            for (ring, pop) in pops.iter_mut().enumerate() {
                if let Poll::Ready(popped) = pop.as_mut().poll(cx) {
                    return Poll::Ready((ring, popped));
                }
            }
            slept = true;
            Poll::Pending
        })
        .await;
        let closed = match popped {
            Some(frame) => {
                shard.tally.parks += u64::from(slept);
                shard.take(&frame);
                None
            }
            None => Some(ring),
        };
        // The pops drop after the frame is taken, so their boxes stay off its
        // latency.
        drop(pops);
        if let Some(ring) = closed {
            consumers.swap_remove(ring);
        }
    }
    shard.tally
}

/// Takes the first frame it finds, looking at each ring once from `next` on.
fn sweep(consumers: &mut [Consumer<Frame>], next: &mut usize) -> Option<Frame> {
    let count = consumers.len();
    for _ in 0..count {
        let ring = *next % count;
        *next = ring + 1;
        if let Some(frame) = consumers[ring].try_pop() {
            return Some(frame);
        }
    }
    None
}

/// Sweeps the rings until a frame comes or `spin` passes.
fn spin(
    consumers: &mut [Consumer<Frame>],
    next: &mut usize,
    spin: Duration,
) -> Option<Frame> {
    if spin.is_zero() {
        return None;
    }
    let until = Instant::now() + spin;
    loop {
        if let Some(frame) = sweep(consumers, next) {
            return Some(frame);
        }
        if Instant::now() >= until {
            return None;
        }
        spin_loop();
    }
}

/// The indexes of one thread, in the order it sends to them, without end.
struct Indexes {
    first: u32,
    count: u32,
    next: u32,
}

impl Indexes {
    /// Producer `producer` starts on shard `producer % shards`, so producers send to
    /// different shards on the same tick.
    fn of_producer(producer: u32, options: &Options) -> Self {
        let count = INDEXES / options.producers;
        Self {
            first: producer * count,
            count,
            next: producer % options.shards,
        }
    }

    /// The indexes shard `shard` owns in the inline design.
    fn of_shard(shard: u32, options: &Options) -> Self {
        let count = INDEXES / options.shards;
        Self {
            first: shard * count,
            count,
            next: 0,
        }
    }
}

impl Iterator for Indexes {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        let index = self.first + self.next;
        self.next = (self.next + 1) % self.count;
        Some(index)
    }
}

/// Pushes frames until `secs` pass, each to the shard `index % shards`. Paced, a full
/// ring drops the frame. Unthrottled, the thread spins until the frame fits or the
/// time is up.
fn produce(producer: u32, mut rings: Vec<Producer<Frame>>, options: &Options) -> Sent {
    let payload = Arc::new([1.5_f64; PAYLOAD]);
    let mut seqs = [0_u64; INDEXES as usize];
    let mut sent = Sent::default();
    let mut pace = Pace::new(options.pace);
    let deadline = Instant::now() + options.secs;
    'frames: for index in Indexes::of_producer(producer, options) {
        if pace.wait() >= deadline {
            break;
        }
        let seq = &mut seqs[index as usize];
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
                    break;
                }
                Err(Full(_)) if pace.paced() => {
                    sent.drops += 1;
                    break;
                }
                Err(Full(returned)) if Instant::now() < deadline => {
                    frame = returned;
                    spin_loop();
                }
                Err(Full(_)) => break 'frames,
            }
        }
        sent.offered += 1;
    }
    sent
}

/// A fixed schedule of frames. Unpaced, every frame is due at once.
struct Pace {
    interval: Option<Duration>,
    due: Instant,
}

impl Pace {
    fn new(per_second: Option<NonZeroU32>) -> Self {
        Self {
            interval: per_second.map(|n| Duration::from_secs(1) / n.get()),
            due: Instant::now(),
        }
    }

    fn paced(&self) -> bool {
        self.interval.is_some()
    }

    /// Spins until the next frame is due and returns the time it is sent.
    fn wait(&mut self) -> Instant {
        let Some(interval) = self.interval else {
            return Instant::now();
        };
        let due = self.due;
        while Instant::now() < due {
            spin_loop();
        }
        self.due += interval;
        due
    }
}

/// Design C: `shards` threads each make their own frames and process them in place.
/// Nothing crosses a thread, so no runtime is needed.
fn run_inline(options: &Options) -> Report {
    let cores = Cores::new(options);
    let start = &Barrier::new(options.shards as usize + 1);
    thread::scope(|scope| {
        let threads: Vec<_> = (0..options.shards)
            .map(|shard| {
                scope.spawn(move || {
                    pin_and_wait(cores.shard(shard as usize), start);
                    produce_inline(shard, options)
                })
            })
            .collect();
        start.wait();
        let started = Instant::now();
        let mut report = Report::default();
        for thread in threads {
            let (tally, sent) = join(thread);
            report.tally.add(&tally);
            report.sent.add(sent);
        }
        report.elapsed = started.elapsed();
        report
    })
}

/// Makes the same frames as `produce` until `secs` pass, and takes each in place.
fn produce_inline(shard: u32, options: &Options) -> (Tally, Sent) {
    let payload = Arc::new([1.5_f64; PAYLOAD]);
    let mut state = Shard::new(options.work);
    let mut sent = Sent::default();
    let mut pace = Pace::new(options.pace);
    let deadline = Instant::now() + options.secs;
    for index in Indexes::of_shard(shard, options) {
        if pace.wait() >= deadline {
            break;
        }
        let frame = Frame {
            sent: Instant::now(),
            index,
            seq: state.seqs[index as usize],
            payload: Arc::clone(&payload),
        };
        sent.offered += 1;
        state.take(&frame);
    }
    (state.tally, sent)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::num::{NonZeroU32, NonZeroUsize};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use ring::Config;

    use super::{
        Cores, Design, FASTEST, Frame, INDEXES, Indexes, Options, PAYLOAD, Report,
        Sent, Shard, USAGE, consume, local_runtime, produce, run, run_inline, run_ring,
    };

    /// Two producers and two shards for 200 ms.
    fn options(pace: u32) -> Options {
        Options {
            producers: 2,
            shards: 2,
            secs: Duration::from_millis(200),
            pace: NonZeroU32::new(pace),
            ..Options::default()
        }
    }

    fn capacity(slots: usize) -> NonZeroUsize {
        NonZeroUsize::new(slots).expect("invariant: the test asks for slots")
    }

    fn frame(index: u32, seq: u64) -> Frame {
        Frame {
            sent: Instant::now(),
            index,
            seq,
            payload: Arc::new([0.0; PAYLOAD]),
        }
    }

    /// The divisors of 64.
    fn counts() -> impl Iterator<Item = u32> + Clone {
        (0..7).map(|power| 1 << power)
    }

    #[test]
    fn ring_offers_each_paced_frame_and_delivers_or_drops_it() {
        let options = options(2_000);
        let report = run_ring(&options);
        assert!(
            (800..=802).contains(&report.sent.offered),
            "two producers at 2,000/s for 200 ms offered {}",
            report.sent.offered
        );
        assert_eq!(report.delivered() + report.sent.drops, report.sent.offered);
        assert!(report.tally.parks > 0, "a paced shard with no spin parks");
        let line = report.line(Design::Ring, &options);
        assert!(
            line.starts_with("| ring | 2 | 2 | 1 | 0 | 2000 | "),
            "{line}"
        );
    }

    #[test]
    fn inline_takes_each_frame_it_offers() {
        let report = run_inline(&options(2_000));
        assert!(
            (800..=802).contains(&report.sent.offered),
            "two shards at 2,000/s for 200 ms offered {}",
            report.sent.offered
        );
        assert_eq!(report.delivered(), report.sent.offered);
        assert_eq!((report.sent.drops, report.tally.parks), (0, 0));
    }

    #[test]
    fn unthrottled_ring_delivers_each_frame_in_order() {
        let report = run_ring(&options(0));
        assert!(
            report.sent.offered > 1_000,
            "too few frames: {}",
            report.sent.offered
        );
        assert_eq!(report.delivered(), report.sent.offered);
        assert_eq!(report.sent.drops, 0, "unthrottled never drops");
    }

    #[test]
    fn ring_counts_each_dropped_frame_as_offered() {
        let options = Options {
            capacity: capacity(1),
            ..options(1_000_000)
        };
        let report = run_ring(&options);
        assert!(
            report.sent.drops > 0,
            "a ring of one slot drops at 1,000,000/s"
        );
        assert_eq!(report.delivered() + report.sent.drops, report.sent.offered);
    }

    #[test]
    fn a_shard_that_spins_longer_than_the_gaps_never_parks() {
        let options = Options {
            spin: Duration::from_secs(1),
            ..options(2_000)
        };
        let report = run_ring(&options);
        assert!(report.delivered() > 0, "nothing delivered");
        assert_eq!(report.tally.parks, 0);
    }

    #[test]
    fn a_producer_stops_at_the_deadline_when_its_shard_is_gone() {
        let options = Options {
            producers: 1,
            shards: 1,
            secs: Duration::from_millis(50),
            capacity: capacity(1),
            ..Options::default()
        };
        let (producer, consumer) = ring::new(Config {
            capacity: options.capacity,
        });
        drop(consumer);
        assert_eq!(
            produce(0, vec![producer], &options),
            Sent {
                offered: 1,
                drops: 0
            },
            "the first frame fills the ring, and the second waits until the end"
        );
    }

    #[test]
    fn a_shard_drains_every_ring_and_ends_when_the_producers_are_gone() {
        let options = options(0);
        let mut consumers = Vec::new();
        for index in 0..3 {
            let (mut producer, consumer) = ring::new(Config {
                capacity: capacity(8),
            });
            for seq in 0..5 {
                assert!(
                    producer.push(frame(index, seq)).is_ok(),
                    "the ring has room"
                );
            }
            consumers.push(consumer);
        }
        let tally = local_runtime().block_on(consume(consumers, &options));
        assert_eq!((tally.latency.len(), tally.parks), (15, 0));
    }

    #[test]
    #[should_panic(expected = "index 3 got a frame out of order")]
    fn a_shard_panics_on_a_frame_out_of_order() {
        let mut shard = Shard::new(1);
        shard.take(&frame(3, 0));
        shard.take(&frame(3, 2));
    }

    #[test]
    fn the_line_gives_the_rate_the_quantiles_and_the_counts() {
        let mut report = Report {
            elapsed: Duration::from_millis(4),
            sent: Sent {
                offered: 7,
                drops: 3,
            },
            ..Report::default()
        };
        for nanos in [500, 1_000, 1_500, 2_000] {
            report
                .tally
                .latency
                .record(nanos)
                .expect("inside the bounds");
        }
        report.tally.parks = 2;
        let options = Options::default();
        assert_eq!(
            report.line(Design::Ring, &options),
            "| ring | 8 | 8 | 1 | 0 | 0 | 1.00 | 1.00 | 2.00 | 2.00 | 2 | 50.0% | 3 |"
        );
        assert!(
            report
                .line(Design::Inline, &options)
                .starts_with("| inline | - | 8 | "),
            "inline has no producers"
        );
        assert_eq!(
            Report::COLUMNS
                .lines()
                .map(|line| line.matches('|').count())
                .collect::<Vec<_>>(),
            [14, 14],
            "the header has one column per value"
        );
    }

    #[test]
    fn producers_start_on_different_shards() {
        for producers in counts() {
            for shards in counts()
                .filter(|&shards| producers <= shards && producers * shards <= INDEXES)
            {
                let options = Options {
                    producers,
                    shards,
                    ..Options::default()
                };
                let firsts: BTreeSet<u32> = (0..producers)
                    .map(|producer| {
                        let index = Indexes::of_producer(producer, &options)
                            .next()
                            .expect("indexes never end");
                        index % shards
                    })
                    .collect();
                assert_eq!(
                    firsts.len(),
                    producers as usize,
                    "P={producers} S={shards}"
                );
            }
        }
    }

    #[test]
    fn each_producer_cycles_its_own_indexes_and_reaches_every_shard() {
        for producers in counts() {
            for shards in counts().filter(|shards| producers * shards <= INDEXES) {
                let options = Options {
                    producers,
                    shards,
                    ..Options::default()
                };
                let mut all = BTreeSet::new();
                for producer in 0..producers {
                    let count = (INDEXES / producers) as usize;
                    let cycle: Vec<u32> = Indexes::of_producer(producer, &options)
                        .take(2 * count)
                        .collect();
                    assert_eq!(
                        cycle[..count],
                        cycle[count..],
                        "P={producers} p={producer}"
                    );
                    let own: BTreeSet<u32> = cycle.iter().copied().collect();
                    let reached: BTreeSet<u32> =
                        own.iter().map(|i| i % shards).collect();
                    assert_eq!(
                        own.len(),
                        count,
                        "P={producers} S={shards} p={producer}"
                    );
                    assert_eq!(
                        reached,
                        (0..shards).collect(),
                        "P={producers} S={shards}"
                    );
                    all.extend(own);
                }
                assert_eq!(all, (0..INDEXES).collect(), "P={producers} S={shards}");
            }
        }
    }

    #[test]
    fn shards_own_disjoint_indexes_inline() {
        for shards in counts() {
            let options = Options {
                shards,
                ..Options::default()
            };
            let count = (INDEXES / shards) as usize;
            let all: BTreeSet<u32> = (0..shards)
                .flat_map(|shard| Indexes::of_shard(shard, &options).take(count))
                .collect();
            assert_eq!(all, (0..INDEXES).collect(), "S={shards}");
        }
    }

    #[test]
    fn every_thread_gets_its_own_core() {
        let options = Options {
            producers: 8,
            shards: 4,
            cpu: Some(2),
            ..Options::default()
        };
        let cores = Cores::new(&options);
        let mut all: Vec<Option<usize>> =
            (0..4).map(|shard| cores.shard(shard)).collect();
        all.extend((0..8).map(|producer| cores.producer(producer)));
        assert_eq!(all, (2..14).map(Some).collect::<Vec<_>>());
        let unpinned = Cores::new(&Options::default());
        assert_eq!((unpinned.shard(0), unpinned.producer(0)), (None, None));
    }

    fn parse_error(design: Design, args: &[&str]) -> String {
        Options::parse(design, args)
            .expect_err("the arguments are wrong")
            .to_string()
    }

    #[test]
    fn args_parse_each_option() {
        let parsed = Options::parse(
            Design::Ring,
            &[
                "producers=4",
                "shards=2",
                "work=8",
                "secs=0.5",
                "pace=10000",
                "spin=20000",
                "capacity=16",
            ],
        )
        .expect("every option is known");
        assert_eq!(
            parsed,
            Options {
                producers: 4,
                shards: 2,
                work: 8,
                secs: Duration::from_millis(500),
                pace: NonZeroU32::new(10_000),
                spin: Duration::from_micros(20),
                capacity: capacity(16),
                cpu: None,
            }
        );
        assert_eq!(
            Options::parse(Design::Ring, &[]).ok(),
            Some(Options::default())
        );
        let unthrottled = Options::parse(Design::Inline, &["pace=0", "shards=64"]);
        assert_eq!(unthrottled.map(|options| options.pace).ok(), Some(None));
    }

    #[test]
    fn args_take_a_core_only_on_linux() {
        let parsed = Options::parse(Design::Inline, &["cpu=3"]);
        if cfg!(target_os = "linux") {
            assert_eq!(parsed.ok().and_then(|options| options.cpu), Some(3));
        } else {
            assert_eq!(
                parsed.expect_err("only Linux pins").to_string(),
                "cpu=3 pins threads, and only Linux can"
            );
        }
    }

    #[test]
    fn args_reject_an_unknown_option() {
        assert_eq!(
            parse_error(Design::Ring, &["work=2", "rate=5"]),
            format!("unknown option \"rate=5\" for ring\n{USAGE}")
        );
        for option in ["producers=2", "spin=10", "capacity=16"] {
            assert_eq!(
                parse_error(Design::Inline, &[option]),
                format!("unknown option {option:?} for inline\n{USAGE}")
            );
        }
    }

    #[test]
    fn args_reject_seconds_that_are_not_a_duration() {
        for (secs, reason) in [
            ("-1", "negative"),
            ("nan", "either too big or NaN"),
            ("inf", "either too big or NaN"),
        ] {
            assert_eq!(
                parse_error(Design::Ring, &[&format!("secs={secs}")]),
                format!(
                    "secs={secs} is not a duration: cannot convert float seconds to \
                    Duration: value is {reason}"
                )
            );
        }
    }

    #[test]
    fn args_reject_a_pace_above_one_frame_per_nanosecond() {
        assert_eq!(
            parse_error(Design::Ring, &["pace=4294967296"]),
            "number too large to fit in target type"
        );
        assert_eq!(
            parse_error(Design::Ring, &[&format!("pace={}", FASTEST + 1)]),
            "pace=1000000001 is above 1000000000 frames per second"
        );
        let fastest = Options::parse(Design::Ring, &[&format!("pace={FASTEST}")]);
        assert_eq!(
            fastest.map(|options| options.pace).ok(),
            Some(NonZeroU32::new(FASTEST))
        );
    }

    #[test]
    fn args_reject_a_count_that_does_not_divide_the_indexes() {
        assert_eq!(
            parse_error(Design::Ring, &["producers=3"]),
            "producers=3 does not divide 64"
        );
        assert_eq!(
            parse_error(Design::Inline, &["shards=0"]),
            "shards=0 does not divide 64"
        );
    }

    #[test]
    fn args_reject_more_rings_than_indexes() {
        assert_eq!(
            parse_error(Design::Ring, &["producers=16", "shards=8"]),
            "producers=16 × shards=8 is above 64, so a producer misses shards"
        );
        let full = Options::parse(Design::Ring, &["producers=8", "shards=8"]);
        assert_eq!(
            full.map(|options| options.producers * options.shards).ok(),
            Some(64)
        );
    }

    #[test]
    fn run_prints_the_columns_or_the_usage() {
        assert_eq!(run(&["columns"]).ok(), Some(Report::COLUMNS.to_owned()));
        for args in [&[][..], &["columns", "shards=2"], &["handoff"]] {
            assert_eq!(
                run(args).expect_err("not a command").to_string(),
                USAGE,
                "{args:?}"
            );
        }
        assert_eq!(
            run(&["inline", "spin=1"])
                .expect_err("inline does not spin")
                .to_string(),
            format!("unknown option \"spin=1\" for inline\n{USAGE}")
        );
    }
}
