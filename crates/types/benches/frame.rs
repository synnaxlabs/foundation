//! The time to build a frame (header, ranges, and descriptors, but not the series
//! bytes), to set each seq on a built draft, to fill and read every series in order,
//! to look each one up, to give its charge, to view the series bytes, to give the end
//! of each series, to check and walk the series from stored ends, to make a view
//! through a full, a narrow, an almost full, and a half full mask and walk or charge
//! it, to walk a narrow view one series at a time, and to make a narrow and an almost
//! full mask, for a dense frame and for frames of 100,000 channels.

use std::fmt;
use std::hint::black_box;
use std::iter;
use std::sync::Arc;

use divan::Bencher;
use types::channel;
use types::frame::key_set::{Group, Interner, KeySet};
use types::frame::{self, Draft, Form, Frame, Mask, Path, View};
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

fn key(n: usize) -> channel::Key {
    channel::Key::from_u128(u128::try_from(n).expect("the cases use few keys"))
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
    let dense: Vec<_> = (1..16).map(|n| (key(n), F64)).collect();
    let wide: Vec<_> = (1..100_000).map(|n| (key(n), F64)).collect();
    let private: Vec<_> = (0..100_000)
        .map(|n| Group {
            index: key(n),
            data: &[],
        })
        .collect();
    let one = |data| {
        [Group {
            index: key(0),
            data,
        }]
    };
    let wide = interner.intern(&one(&wide));
    let private = interner.intern(&private);
    let tenth = |n: usize| (0..10).map(move |k| k * n / 10);
    let mut cases = vec![
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
            Arc::clone(&private),
            tenth(100_000).map(|entry| (entry, 8)).collect(),
        ),
        case(
            "100k of 100k in one group",
            wide,
            (0..100_000).map(|entry| (entry, 8)).collect(),
        ),
        case(
            "100k of 100k in one group, index last",
            index_last(&mut interner),
            (0..100_000).map(|entry| (entry, 8)).collect(),
        ),
        case(
            "100k of 100k private indexes",
            private,
            (0..100_000).map(|entry| (entry, 8)).collect(),
        ),
    ];
    cases.extend(turns(&mut interner));
    cases
}

/// Frames whose data take turns among groups.
fn turns(interner: &mut Interner) -> Vec<Case> {
    let apart = apart(interner);
    let [few, some, many] = [(16, 900_000), (32, 500_000), (1_024, 1_000_000)]
        .map(|(count, first)| spread(interner, count, first));
    vec![
        case(
            "100k of 100k in two groups, data in turn",
            in_turn(interner, 2),
            (0..100_000).map(|entry| (entry, 8)).collect(),
        ),
        case(
            "50k of 100k in 32 groups, data in turn",
            in_turn(interner, 32),
            (0..50_000).map(|entry| (entry, 8)).collect(),
        ),
        case(
            "99,999 of 100k in 32 groups, data in turn, indexes spread",
            Arc::clone(&some),
            (0..99_999).map(|entry| (entry, 8)).collect(),
        ),
        half(
            "half of 100k in 16 groups, data in turn, indexes spread",
            few,
        ),
        half(
            "half of 100k in 32 groups, data in turn, indexes spread",
            Arc::clone(&some),
        ),
        half(
            "half of 100k in 1,024 groups, data in turn, indexes spread",
            many,
        ),
        case(
            "groups 0 and 16 in turn, indexes apart",
            Arc::clone(&apart),
            iter::once(0)
                .chain(16..apart.entries().len())
                .map(|entry| (entry, 8))
                .collect(),
        ),
        case(
            "groups 0 and 16 in turn, indexes apart, index of group 1 last passed",
            Arc::clone(&apart),
            [0, 1]
                .into_iter()
                .chain(16..apart.entries().len())
                .map(|entry| (entry, 8))
                .collect(),
        ),
    ]
}

/// One group whose data slots come before its index slot.
fn index_last(interner: &mut Interner) -> Arc<KeySet> {
    let data: Vec<_> = (100_001..200_000).map(|n| (key(n), F64)).collect();
    for &(key, _) in &data {
        interner.slots().assign(key);
    }
    interner.intern(&[Group {
        index: key(200_000),
        data: &data,
    }])
}

/// `count` groups of 100,000 entries in all: each index slot first, then the data
/// slots of the groups in turn.
fn in_turn(interner: &mut Interner, count: usize) -> Arc<KeySet> {
    for n in 300_000..400_000 {
        interner.slots().assign(key(n));
    }
    let data: Vec<Vec<_>> = (0..count)
        .map(|group| {
            (300_000 + count + group..400_000)
                .step_by(count)
                .map(|n| (key(n), F64))
                .collect()
        })
        .collect();
    let groups: Vec<_> = data
        .iter()
        .enumerate()
        .map(|(group, data)| Group {
            index: key(300_000 + group),
            data,
        })
        .collect();
    interner.intern(&groups)
}

/// Each even entry of a set from [`spread`], and each index.
fn half(name: &'static str, set: Arc<KeySet>) -> Case {
    let step = 100_000 / set.groups().len();
    let series = (0..100_000)
        .filter(|entry| entry % 2 == 0 || entry % step == 0)
        .map(|entry| (entry, 8))
        .collect();
    case(name, set, series)
}

/// `count` groups of 100,000 entries in all, keyed from `first`: the data slots of
/// the groups in turn, with an index slot each 100,000 / `count` slots.
fn spread(interner: &mut Interner, count: usize, first: usize) -> Arc<KeySet> {
    let step = 100_000 / count;
    let mut data = vec![Vec::new(); count];
    let mut turn = 0;
    for n in 0..100_000 {
        interner.slots().assign(key(first + n));
        if n % step != 0 || n / step >= count {
            data[turn % count].push((key(first + n), F64));
            turn += 1;
        }
    }
    let groups: Vec<_> = data
        .iter()
        .enumerate()
        .map(|(group, data)| Group {
            index: key(first + group * step),
            data,
        })
        .collect();
    interner.intern(&groups)
}

/// 17 groups. The index slots of groups 0 to 15 come first and the index slot of group
/// 16 last. Between them, the data slots of groups 0 and 16 take turns.
fn apart(interner: &mut Interner) -> Arc<KeySet> {
    for n in 700_000..=800_000 {
        interner.slots().assign(key(n));
    }
    let data = |first| -> Vec<_> {
        (first..800_000).step_by(2).map(|n| (key(n), F64)).collect()
    };
    let (first, last) = (data(700_016), data(700_017));
    let groups: Vec<_> = (0..17)
        .map(|group| match group {
            0 => Group {
                index: key(700_000),
                data: &first,
            },
            16 => Group {
                index: key(800_000),
                data: &last,
            },
            _ => Group {
                index: key(700_000 + group),
                data: &[],
            },
        })
        .collect();
    interner.intern(&groups)
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
    draft.freeze(Path::Live)
}

/// Builds a frame and sets each present group's count.
#[divan::bench(args = cases(), sample_count = 1000)]
fn build(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    bencher.bench_local(|| {
        let mut draft = draft(&pool, case);
        for &group in &case.groups {
            draft.set_count(group, 1);
        }
        drop(draft.freeze(Path::Live));
    });
}

/// Reads each present group's count and sets its seq, on a built draft.
#[divan::bench(args = cases(), sample_count = 1000)]
fn seq(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let mut draft = draft(&pool, case);
    bencher.bench_local(|| {
        let draft = black_box(&mut draft);
        for &group in &case.groups {
            let range = draft.range(group).expect("the group is present");
            draft.set_seq(group, u64::from(range.count));
        }
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
        drop(draft.freeze(Path::Live));
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

/// Views the series bytes, then drops the view.
#[divan::bench(args = cases(), sample_count = 1000)]
fn body(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    bencher.bench_local(|| black_box(&frame).body().len());
}

/// Sums the end of every series.
#[divan::bench(args = cases(), sample_count = 1000)]
fn ends(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    bencher.bench_local(|| black_box(&frame).ends().map(|(_, end)| end).sum::<usize>());
}

/// Sums the length of every series, in order.
#[divan::bench(args = cases(), sample_count = 1000)]
fn walk(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    bencher.bench_local(|| {
        black_box(&frame)
            .iter()
            .map(|(_, bytes)| bytes.len())
            .sum::<usize>()
    });
}

/// Checks stored ends against the series bytes.
#[divan::bench(args = cases(), sample_count = 1000)]
fn check(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    let (body, ends): (_, Vec<_>) = (frame.body(), frame.ends().collect());
    bencher.bench_local(|| {
        frame::check(black_box(&body), black_box(&ends).iter().copied())
    });
}

/// Sums the length of every series, in order, from stored ends.
#[divan::bench(args = cases(), sample_count = 1000)]
fn stored(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    let (body, ends): (_, Vec<_>) = (frame.body(), frame.ends().collect());
    bencher.bench_local(|| {
        frame::split(black_box(&body), black_box(&ends).iter().copied())
            .map(|(_, bytes)| bytes.len())
            .sum::<usize>()
    });
}

/// Makes a view through a mask that wants every channel, then sums the length of each
/// of its series.
#[divan::bench(args = cases(), sample_count = 1000)]
fn full_walk(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    let mask = Mask::new(&case.set, every(case));
    bencher.bench_local(|| walk_view(View::new(black_box(&frame), black_box(&mask))));
}

/// Makes a view through a mask that wants every channel, then gives its charge.
#[divan::bench(args = cases(), sample_count = 1000)]
fn full_charge(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    let mask = Mask::new(&case.set, every(case));
    bencher.bench_local(|| View::new(black_box(&frame), black_box(&mask)).charge());
}

/// Makes a view through a mask that wants only the last present channel, then sums
/// the length of each of its series.
#[divan::bench(args = cases(), sample_count = 1000)]
fn narrow_walk(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    let mask = Mask::new(&case.set, last(case));
    bencher.bench_local(|| walk_view(View::new(black_box(&frame), black_box(&mask))));
}

/// Makes a view through a mask that wants only the last present channel, then sums
/// the length of each of its series one at a time.
#[divan::bench(args = cases(), sample_count = 1000)]
fn narrow_next(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    let mask = Mask::new(&case.set, last(case));
    bencher.bench_local(|| {
        let mut len = 0;
        for (_, bytes) in View::new(black_box(&frame), black_box(&mask)).iter() {
            len += bytes.len();
        }
        len
    });
}

/// Makes a view through a mask that wants only the last present channel, then gives
/// its charge.
#[divan::bench(args = cases(), sample_count = 1000)]
fn narrow_charge(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    let mask = Mask::new(&case.set, last(case));
    bencher.bench_local(|| View::new(black_box(&frame), black_box(&mask)).charge());
}

/// Makes a view through a mask that wants every channel but one, then sums the length
/// of each of its series.
#[divan::bench(args = cases(), sample_count = 1000)]
fn most_walk(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    let mask = Mask::new(&case.set, most(case));
    bencher.bench_local(|| walk_view(View::new(black_box(&frame), black_box(&mask))));
}

/// Makes a view through a mask that wants every channel but one, then gives its
/// charge.
#[divan::bench(args = cases(), sample_count = 1000)]
fn most_charge(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    let mask = Mask::new(&case.set, most(case));
    bencher.bench_local(|| View::new(black_box(&frame), black_box(&mask)).charge());
}

/// Makes a view through a mask that wants just over half the channels, in turn with
/// those it leaves out, then sums the length of each of its series.
#[divan::bench(args = cases(), sample_count = 1000)]
fn half_walk(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    let mask = Mask::new(&case.set, alternate(case));
    bencher.bench_local(|| walk_view(View::new(black_box(&frame), black_box(&mask))));
}

/// Makes a view through a mask that wants just over half the channels, in turn with
/// those it leaves out, then gives its charge.
#[divan::bench(args = cases(), sample_count = 1000)]
fn half_charge(bencher: Bencher<'_, '_>, case: &Case) {
    let pool = pool();
    let frame = frame(&pool, case);
    let mask = Mask::new(&case.set, alternate(case));
    bencher.bench_local(|| View::new(black_box(&frame), black_box(&mask)).charge());
}

/// Makes a mask that wants only the last present channel.
#[divan::bench(args = cases(), sample_count = 1000)]
fn narrow_mask(bencher: Bencher<'_, '_>, case: &Case) {
    bencher.bench_local(|| Mask::new(black_box(&case.set), last(case)));
}

/// Makes a mask that wants every channel but one.
#[divan::bench(args = cases(), sample_count = 1000)]
fn most_mask(bencher: Bencher<'_, '_>, case: &Case) {
    bencher.bench_local(|| Mask::new(black_box(&case.set), most(case)));
}

/// Each channel of `case`.
fn every(case: &Case) -> impl Iterator<Item = channel::Slot> {
    case.set.entries().iter().map(|entry| entry.slot)
}

/// Each channel of `case` but one that the mask cannot hold again as an index: the
/// last data channel, or the last channel when each is an index without data.
fn most(case: &Case) -> impl Iterator<Item = channel::Slot> {
    let set = &case.set;
    let skip = (0..set.entries().len())
        .rev()
        .find(|&entry| set.index(entry) != entry)
        .or(set.entries().len().checked_sub(1));
    let entries = set.entries().iter().enumerate();
    entries
        .filter(move |&(entry, _)| Some(entry) != skip)
        .map(|(_, entry)| entry.slot)
}

/// Entry 0 and each odd entry of `case`: just over half of its channels.
fn alternate(case: &Case) -> impl Iterator<Item = channel::Slot> {
    let entries = case.set.entries().iter().enumerate();
    entries
        .filter(|&(entry, _)| entry == 0 || entry % 2 == 1)
        .map(|(_, entry)| entry.slot)
}

/// The channel of the last present series of `case`.
fn last(case: &Case) -> [channel::Slot; 1] {
    let (entry, _) = *case.series.last().expect("each case has a series");
    [case.set.entries()[entry].slot]
}

fn walk_view(view: View<'_>) -> usize {
    view.iter().map(|(_, bytes)| bytes.len()).sum()
}
