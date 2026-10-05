//! The time to build a frame (header, masks, ranges, and descriptors, but not the
//! series bytes) and to read every present series back, for a dense frame and for two
//! sparse frames of 100,000 channels.

use std::fmt;
use std::hint::black_box;
use std::sync::Arc;

use divan::Bencher;
use types::channel::Slot;
use types::frame::key_set::{Group, Interner, KeySet};
use types::frame::{Draft, Form, Path, Range};
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
}

impl fmt::Display for Case {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

fn slot(n: usize) -> Slot {
    Slot::new(u32::try_from(n).expect("the cases use few slots"))
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
    let tenth = |n: usize| (0..10).map(move |k| k * n / 10);
    vec![
        Case {
            name: "16 of 16, 1024 samples",
            set: interner.intern(&one(&dense)),
            series: (0..16).map(|entry| (entry, 8 * 1024)).collect(),
        },
        Case {
            name: "10 of 100k in one group",
            set: interner.intern(&one(&wide)),
            series: tenth(100_000).map(|entry| (entry, 8)).collect(),
        },
        Case {
            name: "10 of 100k private indexes",
            set: interner.intern(&private),
            series: tenth(100_000).map(|entry| (entry, 8)).collect(),
        },
    ]
}

fn pool() -> block::Pool {
    let config = block::Config { budget: 1 << 24 };
    let memory = block::Heap::new(config.reservation());
    block::Pool::new(config, memory)
}

#[divan::bench(args = cases(), sample_count = 1000)]
fn build(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    bencher.bench_local(|| {
        let mut draft = Draft::new(
            &pool,
            black_box(&case.set),
            Path::Live,
            Form::Encoded,
            black_box(&case.series),
        )
        .expect("the pool holds the frame");
        draft.set_range(0, Range { seq: 1, count: 1 });
        draft.freeze()
    });
}

#[divan::bench(args = cases(), sample_count = 1000)]
fn read(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let draft = Draft::new(&pool, &case.set, Path::Live, Form::Encoded, &case.series)
        .expect("the pool holds the frame");
    let frame = draft.freeze();
    bencher.bench_local(|| {
        black_box(&frame)
            .iter()
            .map(|(entry, bytes)| entry + bytes.len())
            .sum::<usize>()
    });
}
