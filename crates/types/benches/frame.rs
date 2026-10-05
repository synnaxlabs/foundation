//! The time to build a frame (header, ranges, and descriptors, but not the series
//! bytes), to fill and read every series in order, to look each one up, and to give its
//! charge, for a dense frame and for frames of 100,000 channels.

use std::fmt;
use std::hint::black_box;
use std::sync::Arc;

use divan::Bencher;
use types::channel::Slot;
use types::frame::key_set::{Group, Interner, KeySet};
use types::frame::{Draft, Form, Frame, Label, Range};
use types::sample::{Scalar, Type};

const F64: Type = Type::Scalar(Scalar::F64);

fn main() {
    divan::main();
}

/// A key set and the series of one frame of it.
struct Case {
    name: &'static str,
    set: Arc<KeySet>,
    series: Vec<(usize, usize)>,
    /// The group of each present index.
    groups: Vec<u32>,
}

impl fmt::Display for Case {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

fn slot(n: usize) -> Slot {
    Slot::new(u32::try_from(n).expect("the cases use few slots"))
}

fn case(name: &'static str, set: Arc<KeySet>, series: Vec<(usize, usize)>) -> Case {
    let groups = series
        .iter()
        .filter(|&&(entry, _)| set.index(entry) == entry)
        .map(|&(entry, _)| set.entries()[entry].group)
        .collect();
    Case {
        name,
        set,
        series,
        groups,
    }
}

fn cases() -> Vec<Case> {
    let mut interner = Interner::new();
    let dense: Vec<_> = (1..16).map(|n| (slot(n), F64)).collect();
    let wide: Vec<_> = (1..100_000).map(|n| (slot(n), F64)).collect();
    let private: Vec<_> = (0..100_000)
        .map(|n| Group {
            index: slot(n),
            data: &[],
        })
        .collect();
    let one = |data| {
        [Group {
            index: slot(0),
            data,
        }]
    };
    let wide = interner.intern(&one(&wide));
    let tenth = |n: usize| (0..10).map(move |k| k * n / 10);
    vec![
        case(
            "16 of 16, 1024 samples",
            interner.intern(&one(&dense)),
            (0..16).map(|entry| (entry, 8 * 1024)).collect(),
        ),
        case(
            "10 of 100k in one group",
            Arc::clone(&wide),
            tenth(100_000).map(|entry| (entry, 8)).collect(),
        ),
        case(
            "10 of 100k private indexes",
            interner.intern(&private),
            tenth(100_000).map(|entry| (entry, 8)).collect(),
        ),
        case(
            "100k of 100k in one group",
            wide,
            (0..100_000).map(|entry| (entry, 8)).collect(),
        ),
    ]
}

fn pool() -> block::Pool {
    let config = block::Config { budget: 1 << 24 };
    let memory = block::Heap::new(config.reservation());
    block::Pool::new(config, memory)
}

fn draft(pool: &block::Pool, case: &Case) -> Draft {
    Draft::new(
        pool,
        black_box(&case.set),
        Label::Live,
        Form::Encoded,
        black_box(&case.series),
    )
    .expect("the pool holds the frame")
}

fn frame(pool: &block::Pool, case: &Case) -> Frame {
    let mut draft = draft(pool, case);
    for (_, bytes) in draft.iter_mut() {
        bytes.fill(1);
    }
    draft.freeze()
}

/// Builds a frame and sets each present group's range.
#[divan::bench(args = cases(), sample_count = 1000)]
fn build(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    bencher.bench_local(|| {
        let mut draft = draft(&pool, case);
        for &group in &case.groups {
            draft.set_range(group, Range { seq: 1, count: 1 });
        }
        drop(draft.freeze());
    });
}

/// Builds a frame and fills each series in order.
#[divan::bench(args = cases(), sample_count = 1000)]
fn fill(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    bencher.bench_local(|| {
        let mut draft = draft(&pool, case);
        for (_, bytes) in draft.iter_mut() {
            bytes.fill(1);
        }
        drop(draft.freeze());
    });
}

/// Sums every byte of every series, in order.
#[divan::bench(args = cases(), sample_count = 1000)]
fn read(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    bencher.bench_local(|| {
        black_box(&frame)
            .iter()
            .flat_map(|(_, bytes)| bytes)
            .map(|&byte| u64::from(byte))
            .sum::<u64>()
    });
}

/// Looks up each present series by entry and sums its length.
#[divan::bench(args = cases(), sample_count = 1000)]
fn lookup(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    bencher.bench_local(|| {
        case.series
            .iter()
            .filter_map(|&(entry, _)| black_box(&frame).series(entry))
            .map(<[u8]>::len)
            .sum::<usize>()
    });
}

/// Gives the frame's credit charge.
#[divan::bench(args = cases(), sample_count = 1000)]
fn charge(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    bencher.bench_local(|| black_box(&frame).charge());
}
