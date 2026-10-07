//! Scratch A/B timing of `log::Logs` for the review of PR 1554: the `log.rs` of the
//! merge base (two copies), of the round 1 head, and of the head, in one process.
//! Each round takes one sample of each variant, one after the other, so each
//! variant sees the same load. Not for the repo.
#![allow(warnings, clippy::all, clippy::pedantic)]

#[path = "../../src/entry.rs"]
mod entry;
mod log_base;
mod log_base2;
mod log_head;
mod log_rnd1;

use std::hint::black_box;
use std::time::Instant;

use types::channel::{self, Slot};
use types::frame::Path;
use types::time::Stamp;

use crate::entry::Header;

trait Variant: Default + 'static {
    type Tail: Default + Clone + 'static;
    fn append(&mut self, slot: Slot, header: &Header);
    fn sync(&mut self, slot: Slot, header: &Header, offset: u64);
    fn lookup(&self, slot: Slot, path: Path, seq: u64) -> Option<u64>;
    fn hide(&mut self, tail: u64);
    fn advance(tail: &mut Self::Tail, header: &Header);
}

macro_rules! variant {
    ($m:ident, |$logs:ident, $slot:ident, $path:ident, $from:ident| $lookup:expr,
     |$hidden:ident, $tail:ident| $hide:expr) => {
        impl Variant for $m::Logs {
            type Tail = $m::Tail;
            #[inline(always)]
            fn append(&mut self, slot: Slot, header: &Header) {
                $m::Logs::append(self, slot, header).expect("appends");
            }
            #[inline(always)]
            fn sync(&mut self, slot: Slot, header: &Header, offset: u64) {
                $m::Logs::sync(self, slot, header, offset).expect("syncs");
            }
            #[inline(always)]
            fn lookup(&self, $slot: Slot, $path: Path, seq: u64) -> Option<u64> {
                let $logs = self;
                let $from = $m::Mark::at(seq);
                $lookup
            }
            #[inline(always)]
            fn hide(&mut self, $tail: u64) {
                let $hidden = self;
                $hide
            }
            #[inline(always)]
            fn advance(tail: &mut $m::Tail, header: &Header) {
                tail.advance(header).expect("advances");
            }
        }
    };
}

variant!(
    log_base,
    |logs, slot, path, from| logs.run(slot, path, from).map(|(_, run)| run.offset),
    |logs, tail| unreachable!("the base hides nothing")
);
variant!(
    log_base2,
    |logs, slot, path, from| logs.run(slot, path, from).map(|(_, run)| run.offset),
    |logs, tail| unreachable!("the base hides nothing")
);
variant!(
    log_rnd1,
    |logs, slot, path, from| match logs.find(slot, path, from) {
        log_rnd1::Found::Run(_, run) => Some(run.offset),
        log_rnd1::Found::Trimmed(end) => Some(end.seq),
        log_rnd1::Found::Nothing => None,
    },
    |logs, tail| logs.trim(tail)
);
variant!(
    log_head,
    |logs, slot, path, from| match logs.find(slot, path, from) {
        log_head::Found::Run(_, run) => Some(run.offset),
        log_head::Found::End(end) => Some(end.seq),
    },
    |logs, tail| logs.hide(tail)
);

/// One sample: its nanoseconds and how many operations it timed.
type Sample = Box<dyn FnMut() -> (u64, u64)>;

fn header(index: u32, first: u64, len: u32) -> Header {
    Header {
        index: channel::Key::from_u128(u128::from(index)),
        path: Path::Live,
        first,
        len,
        stored_at: Stamp::from_nanos(7),
        last: Some(Stamp::from_nanos(7)),
        tag: 0,
        bytes: 0,
    }
}

/// `paths` paths, each with one entry of one sample in each of `records` records.
/// Record `r` is at offset `4096 * (r + 1)`.
fn create<V: Variant>(paths: u32, records: u64) -> V {
    let mut logs = V::default();
    for record in 0..records {
        for index in 0..paths {
            let header = header(index, record, 1);
            logs.append(Slot::new(index), &header);
            logs.sync(Slot::new(index), &header, 4096 * (record + 1));
        }
    }
    logs
}

fn nanos(started: Instant) -> u64 {
    started.elapsed().as_nanos() as u64
}

/// Control: one more appended entry of each path, `passes` times.
fn append_batch<V: Variant>(paths: u32, records: u64, passes: u64) -> Sample {
    let mut logs: V = create(paths, records);
    let mut seq = records;
    Box::new(move || {
        let started = Instant::now();
        for _ in 0..passes {
            for index in 0..paths {
                let header = header(index, seq, 1);
                logs.append(Slot::new(index), black_box(&header));
            }
            seq += 1;
        }
        (nanos(started), passes * u64::from(paths))
    })
}

/// Control: `Tail::advance` alone, the same text in each variant.
fn tail_advance<V: Variant>(paths: u32, passes: u64) -> Sample {
    let mut tails = vec![V::Tail::default(); paths as usize];
    let mut seq = 0;
    Box::new(move || {
        let started = Instant::now();
        for _ in 0..passes {
            for (index, tail) in tails.iter_mut().enumerate() {
                let header = header(index as u32, seq, 1);
                V::advance(black_box(&mut *tail), black_box(&header));
            }
            seq += 1;
        }
        (nanos(started), passes * u64::from(paths))
    })
}

/// One more entry of each path in the newest record of the path: no new run.
fn sync_same_record<V: Variant>(paths: u32, records: u64, passes: u64) -> Sample {
    let mut logs: V = create(paths, records);
    let mut seq = records;
    Box::new(move || {
        let started = Instant::now();
        for _ in 0..passes {
            for index in 0..paths {
                let header = header(index, seq, 1);
                logs.sync(
                    Slot::new(index),
                    black_box(&header),
                    black_box(4096 * records),
                );
            }
            seq += 1;
        }
        (nanos(started), passes * u64::from(paths))
    })
}

/// One entry of each path in a new record: one new run for each path, with room in
/// its deque. Each sample builds `inputs` new logs before it starts its timer.
fn sync_new_record<V: Variant>(paths: u32, inputs: usize) -> Sample {
    Box::new(move || {
        let mut fresh: Vec<V> = (0..inputs).map(|_| create(paths, 12)).collect();
        let started = Instant::now();
        for logs in &mut fresh {
            for index in 0..paths {
                let header = header(index, 12, 1);
                logs.sync(Slot::new(index), black_box(&header), black_box(4096 * 13));
            }
        }
        let nanos = nanos(started);
        (nanos, inputs as u64 * u64::from(paths))
    })
}

/// From empty: 256 records, one entry of each path in each. The deques grow.
fn sync_fill<V: Variant>(paths: u32, repeats: usize) -> Sample {
    Box::new(move || {
        let mut built: Vec<V> = Vec::with_capacity(repeats);
        let started = Instant::now();
        for _ in 0..repeats {
            let mut logs = V::default();
            for record in 0..256_u64 {
                for index in 0..paths {
                    let header = header(index, record, 1);
                    logs.sync(Slot::new(index), black_box(&header), 4096 * (record + 1));
                }
            }
            built.push(logs);
        }
        let nanos = nanos(started);
        (nanos, repeats as u64 * 256 * u64::from(paths))
    })
}

/// Each commit hides one record and syncs one entry of each path in a new record:
/// each path drops one run and adds one. Twelve runs stay for each path.
fn hide_then_sync<V: Variant>(paths: u32, passes: u64) -> Sample {
    let mut logs: V = create(paths, 12);
    let mut record = 12_u64;
    Box::new(move || {
        let started = Instant::now();
        for _ in 0..passes {
            logs.hide(black_box(4096 * (record - 10)));
            for index in 0..paths {
                let header = header(index, record, 1);
                logs.sync(Slot::new(index), black_box(&header), 4096 * (record + 1));
            }
            record += 1;
        }
        (nanos(started), passes * u64::from(paths))
    })
}

#[derive(Clone, Copy)]
enum Marks {
    /// Each mark in turn over every run, with nothing hidden.
    InOrder,
    /// Marks that jump over every run, with nothing hidden.
    Scattered,
    /// The older half hidden; each mark in turn over the half left.
    HalfHiddenLeft,
    /// The older half hidden; each mark in turn over the hidden half.
    HalfHiddenFrom,
    /// Every run hidden; the mark 0.
    AllHidden,
}

/// Lookups of a path with `runs` runs among 64 paths, `per` for each sample.
fn lookup<V: Variant>(runs: u64, per: u64, marks: Marks) -> Sample {
    let mut logs: V = create(64, 1);
    for record in 1..runs {
        let header = header(0, record, 1);
        logs.sync(Slot::new(0), &header, 4096 * (record + 1));
    }
    match marks {
        Marks::InOrder | Marks::Scattered => {}
        Marks::HalfHiddenLeft | Marks::HalfHiddenFrom => logs.hide(4096 * (runs / 2 + 1)),
        Marks::AllHidden => logs.hide(4096 * (runs + 1)),
    }
    let mut seq = match marks {
        Marks::HalfHiddenLeft => runs / 2,
        _ => 0,
    };
    Box::new(move || {
        let started = Instant::now();
        for _ in 0..per {
            let found = logs.lookup(Slot::new(0), Path::Live, black_box(seq));
            black_box(found);
            seq = match marks {
                Marks::InOrder => {
                    if seq + 1 == runs {
                        0
                    } else {
                        seq + 1
                    }
                }
                Marks::Scattered => {
                    (seq.wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407)
                        >> 33)
                        % runs
                }
                Marks::HalfHiddenLeft => {
                    if seq + 1 == runs {
                        runs / 2
                    } else {
                        seq + 1
                    }
                }
                Marks::HalfHiddenFrom => {
                    if seq + 1 >= runs / 2 {
                        0
                    } else {
                        seq + 1
                    }
                }
                Marks::AllHidden => 0,
            };
        }
        (nanos(started), per)
    })
}

/// A lookup of a path that the logs do not hold.
fn lookup_unknown<V: Variant>(per: u64) -> Sample {
    let logs: V = create(64, 12);
    Box::new(move || {
        let started = Instant::now();
        for _ in 0..per {
            let found = logs.lookup(black_box(Slot::new(99)), Path::Live, black_box(0));
            black_box(found);
        }
        (nanos(started), per)
    })
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    let at = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[at]
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    quantile(values, 0.5)
}

/// Takes `rounds` rounds of one sample of each variant, in an order that turns by
/// one each round, and prints one line for each variant. The first is the base.
fn measure(name: &str, rounds: usize, mut variants: Vec<(&'static str, Sample)>) {
    let count = variants.len();
    for (_, sample) in &mut variants {
        for _ in 0..(rounds / 20).max(3) {
            sample();
        }
    }
    let mut samples = vec![Vec::with_capacity(rounds); count];
    for round in 0..rounds {
        for turn in 0..count {
            let at = (turn + round) % count;
            let (nanos, ops) = (variants[at].1)();
            samples[at].push(nanos as f64 / ops as f64);
        }
    }
    let base = samples[0].clone();
    let mut base_sorted = base.clone();
    base_sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    for (at, (label, _)) in variants.iter().enumerate() {
        let mut sorted = samples[at].clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        let mut ratios: Vec<f64> =
            samples[at].iter().zip(&base).map(|(v, b)| v / b).collect();
        let blocks: Vec<f64> = ratios
            .chunks((rounds / 10).max(1))
            .filter(|chunk| chunk.len() == (rounds / 10).max(1))
            .map(|chunk| median(&mut chunk.to_vec()))
            .collect();
        let low = blocks.iter().copied().fold(f64::INFINITY, f64::min);
        let high = blocks.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let pair = median(&mut ratios);
        println!(
            "{name}|{label}|{rounds}|{:.4}|{:.4}|{:.4}|{:.4}|{:.4}|{:.4}|{:.4}",
            sorted[0],
            quantile(&sorted, 0.1),
            quantile(&sorted, 0.5),
            quantile(&sorted, 0.9),
            pair,
            low,
            high,
        );
    }
}

macro_rules! each {
    ($f:ident($($arg:expr),*)) => {
        vec![
            ("base", $f::<log_base::Logs>($($arg),*)),
            ("base2", $f::<log_base2::Logs>($($arg),*)),
            ("head", $f::<log_head::Logs>($($arg),*)),
            ("rnd1", $f::<log_rnd1::Logs>($($arg),*)),
        ]
    };
}

macro_rules! hiding {
    ($f:ident($($arg:expr),*)) => {
        vec![
            ("head", $f::<log_head::Logs>($($arg),*)),
            ("rnd1", $f::<log_rnd1::Logs>($($arg),*)),
        ]
    };
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("--")).collect();
    let scale: f64 = args.first().map_or(1.0, |a| a.parse().expect("a scale"));
    let filter = args.get(1).cloned().unwrap_or_default();
    let rounds = |count: usize| ((count as f64 * scale) as usize).max(20);
    let wanted = |name: &str| name.contains(&filter);
    println!("name|variant|rounds|min|p10|median|p90|pair|block_low|block_high");
    for (paths, passes) in [(1_u32, 8192_u64), (64, 128), (1024, 8)] {
        let name = format!("control_tail_advance/{paths}");
        if wanted(&name) {
            measure(&name, rounds(20_000), each!(tail_advance(paths, passes)));
        }
        let name = format!("control_append_batch/{paths}");
        if wanted(&name) {
            measure(&name, rounds(20_000), each!(append_batch(paths, 12, passes)));
        }
        let name = format!("sync_same_record/{paths}");
        if wanted(&name) {
            measure(&name, rounds(20_000), each!(sync_same_record(paths, 12, passes)));
        }
    }
    for (paths, inputs) in [(1_u32, 1024_usize), (64, 32), (1024, 4)] {
        let name = format!("sync_new_record/{paths}");
        if wanted(&name) {
            measure(&name, rounds(5_000), each!(sync_new_record(paths, inputs)));
        }
    }
    for (paths, repeats, count) in [(1_u32, 16_usize, 10_000_usize), (64, 1, 5_000), (1024, 1, 800)] {
        let name = format!("sync_fill_256_records/{paths}");
        if wanted(&name) {
            measure(&name, rounds(count), each!(sync_fill(paths, repeats)));
        }
    }
    for runs in [16_u64, 4096, 262_144] {
        let name = format!("lookup_in_order/{runs}");
        if wanted(&name) {
            measure(&name, rounds(20_000), each!(lookup(runs, 2048, Marks::InOrder)));
        }
        let name = format!("lookup_scattered/{runs}");
        if wanted(&name) {
            measure(&name, rounds(20_000), each!(lookup(runs, 2048, Marks::Scattered)));
        }
    }
    if wanted("lookup_unknown_path") {
        measure("lookup_unknown_path", rounds(20_000), each!(lookup_unknown(8192)));
    }
    if wanted("control_append_batch_wide") {
        measure(
            "control_append_batch_wide",
            rounds(600),
            each!(append_batch(100_000, 64, 1)),
        );
    }
    if wanted("sync_new_record_wide") {
        measure(
            "sync_new_record_wide",
            rounds(300),
            each!(sync_new_record(100_000, 1)),
        );
    }
    if wanted("sync_same_record_wide") {
        measure(
            "sync_same_record_wide",
            rounds(600),
            each!(sync_same_record(100_000, 64, 1)),
        );
    }
    for (paths, passes) in [(1_u32, 8192_u64), (64, 128), (1024, 8)] {
        let name = format!("hide_then_sync_new_record/{paths}");
        if wanted(&name) {
            measure(&name, rounds(20_000), hiding!(hide_then_sync(paths, passes)));
        }
    }
    for runs in [16_u64, 4096, 262_144] {
        for (label, marks) in [
            ("lookup_in_order_half_hidden", Marks::HalfHiddenLeft),
            ("lookup_from_hidden_half", Marks::HalfHiddenFrom),
            ("lookup_all_hidden", Marks::AllHidden),
        ] {
            let name = format!("{label}/{runs}");
            if wanted(&name) {
                measure(&name, rounds(20_000), hiding!(lookup(runs, 2048, marks)));
            }
        }
    }
}
