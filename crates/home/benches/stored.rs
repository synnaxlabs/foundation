//! The time to make the stored entry of an index frame and drop it, and to read each
//! series of its body, for frames of 16, 1000, and 100,000 series of one sample: each
//! an `f32`, or a mix of each kind of type.

use std::fmt;
use std::sync::Arc;

use divan::Bencher;
use divan::counter::ItemsCount;
use types::channel;
use types::frame::key_set::{Group, Interner, KeySet};
use types::frame::{Draft, Form, Frame, Path};
use types::sample::{Scalar, Type};
use types::time::Stamp;

fn main() {
    divan::main();
}

/// The data types that a mixed frame cycles through.
const MIXED: [Type; 9] = [
    Type::Scalar(Scalar::F32),
    Type::Scalar(Scalar::F64),
    Type::Scalar(Scalar::I16),
    Type::Scalar(Scalar::U8),
    Type::Scalar(Scalar::Stamp),
    Type::Array {
        element: Scalar::F32,
        len: 6,
    },
    Type::List {
        element: Scalar::U16,
        max: 8,
    },
    Type::String,
    Type::Bytes,
];

/// Bytes of one sample of a type that has no width.
const VARIABLE: usize = 16;

/// A key set of one group: an index, and data series whose types cycle through the
/// types given to [`Case::new`].
struct Case {
    name: &'static str,
    series: usize,
    set: Arc<KeySet>,
}

impl fmt::Display for Case {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.series, self.name)
    }
}

impl Case {
    fn new(name: &'static str, series: usize, types: &[Type]) -> Self {
        let key = |n: usize| channel::Key::from_u128(u128::try_from(n).expect("few"));
        let data: Vec<_> = (1..series)
            .map(|n| (key(n), types[(n - 1) % types.len()]))
            .collect();
        let set = Interner::new().intern(&[Group {
            index: key(0),
            data: &data,
        }]);
        Self { name, series, set }
    }

    /// An encoded index frame with one sample of each series.
    fn frame(&self, pool: &block::Pool) -> Frame {
        let lens: Vec<_> = (0..self.series)
            .map(|entry| {
                let data_type = self.set.entries()[entry].data_type;
                (entry, data_type.width().unwrap_or(VARIABLE))
            })
            .collect();
        let mut draft =
            Draft::new(pool, &self.set, Form::Encoded, &lens).expect("room");
        draft.set_count(0, 1);
        draft.set_seq(0, 1);
        draft.freeze(Path::Live)
    }
}

fn pool() -> block::Pool {
    let config = block::Config { budget: 1 << 24 };
    let memory = block::Heap::new(config.reservation());
    block::Pool::new(config, memory)
}

fn store(pool: &block::Pool, frame: &Frame, set: &KeySet) -> buffer::Entry {
    let (last, stored_at) = (Stamp::from_nanos(7), Stamp::from_nanos(9));
    home::bench::entry(pool, frame, set, last, stored_at).expect("room")
}

fn cases() -> Vec<Case> {
    [16, 1000, 100_000]
        .into_iter()
        .flat_map(|series| {
            [
                Case::new("f32", series, &[Type::Scalar(Scalar::F32)]),
                Case::new("mixed", series, &MIXED),
            ]
        })
        .collect()
}

#[divan::bench(args = cases(), sample_count = 1000)]
fn entry(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = case.frame(&pool);
    bencher
        .counter(ItemsCount::new(case.series))
        .bench_local(|| drop(divan::black_box(store(&pool, &frame, &case.set))));
}

#[divan::bench(args = cases(), sample_count = 1000)]
fn read(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = case.frame(&pool);
    let parts = store(&pool, &frame, &case.set).parts;
    let body: Vec<u8> = parts.into_iter().flat_map(|part| part.to_vec()).collect();
    let read: Vec<_> = home::bench::read(&body).collect();
    let entries = case.set.entries();
    let stored: Vec<_> = frame
        .iter()
        .map(|(entry, bytes)| (entries[entry].key, entries[entry].data_type, bytes))
        .collect();
    assert_eq!(
        stored.len(),
        case.series,
        "{case}: the frame lacks a series"
    );
    assert_eq!(read, stored, "{case}: the body does not hold each series");
    bencher
        .counter(ItemsCount::new(case.series))
        .bench_local(|| {
            for series in home::bench::read(divan::black_box(&body)) {
                divan::black_box(series);
            }
        });
}
