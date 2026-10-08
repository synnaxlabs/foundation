//! Building and reading a frame or a view, and building a frame from the ends and
//! series bytes that another node sends, make no heap allocation. This binary has
//! no test harness: the count covers each thread, and a harness allocates on its own
//! thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use types::channel::Key;
use types::frame::key_set::{Group, Interner, KeySet};
use types::frame::{self, Draft, Form, Layout, Mask, Path, Places, Range, View};
use types::sample::{Scalar, Type};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const F64: Type = Type::Scalar(Scalar::F64);

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );

    let data = [(Key::from_u128(2), F64), (Key::from_u128(3), F64)];
    let groups = [
        Group {
            index: Key::from_u128(1),
            data: &data,
        },
        Group {
            index: Key::from_u128(4),
            data: &[],
        },
    ];
    let mut interner = Interner::new();
    let set = interner.intern(&groups);
    let other = interner.intern(&groups[..1]);
    let config = block::Config { budget: 1 << 16 };
    let pool = block::Pool::new(config.clone(), block::Heap::new(config.reservation()));
    read_a_frame(&pool, &set);
    read_a_view(&pool, &set);
    receive_a_frame(&pool, &set);
    lay_places(&pool, &set, &other);
}

const SERIES: [(usize, usize); 2] = [(0, 16), (2, 16)];

fn read_a_frame(pool: &block::Pool, set: &KeySet) {
    let series = SERIES;
    let (sum, allocations) = ALLOCATOR.count(|| {
        let mut draft = Draft::new(pool, set, Form::Raw, &series)
            .expect("the pool holds the frame");
        for (entry, bytes) in draft.iter_mut() {
            bytes.fill(u8::try_from(entry).expect("entries are small"));
        }
        draft.series_mut(0).expect("entry 0 is present").fill(1);
        draft.set_count(0, 2);
        draft.set_seq(0, 9);
        let drafted: usize = draft.iter().map(|(_, bytes)| bytes.len()).sum();
        assert_eq!(drafted, 32, "the draft reads both series");
        let expected = Some(Range { seq: 9, count: 2 });
        assert_eq!(draft.range(0), expected, "the draft reads its range");
        assert!(
            draft.ranges().eq([(0, Range { seq: 9, count: 2 })]),
            "the draft reads its ranges"
        );
        let frame = draft.freeze(Path::Backfill);
        let copy = frame.clone();
        assert_eq!(frame.charge(), 192, "the frame charges its block");
        assert_eq!(frame.key_set(), set.key(), "the frame names its key set");
        assert_eq!(frame.path(), Path::Backfill, "the frame keeps its path");
        assert_eq!(frame.form(), Form::Raw, "the frame keeps its form");
        assert_eq!(frame.series(1), None, "entry 1 is absent");
        assert_eq!(frame.range(1), None, "group 1 is absent");
        assert!(
            frame.ranges().eq([(0, Range { seq: 9, count: 2 })]),
            "the frame reads its ranges"
        );
        assert_eq!(
            frame.series(2),
            Some([2; 16].as_slice()),
            "entry 2 reads back"
        );
        let body = frame.body();
        assert_eq!(body.len(), 32, "the body views both series");
        assert_eq!(frame::check(&body, frame.ends()), Ok(()), "the ends fit");
        assert_eq!(
            frame::split(&body, frame.ends())
                .map(|(_, bytes)| bytes.len())
                .sum::<usize>(),
            32,
            "the ends give both series"
        );
        let range = copy.range(0).expect("group 0 is present");
        copy.iter()
            .flat_map(|(_, bytes)| bytes)
            .map(|&byte| u64::from(byte))
            .sum::<u64>()
            + range.seq
    });
    assert_eq!(allocations, 0, "the hot path allocated");
    assert_eq!(
        sum,
        16 + 32 + 9,
        "the frame reads back what the draft wrote"
    );
}

fn read_a_view(pool: &block::Pool, set: &KeySet) {
    let frame = Draft::new(pool, set, Form::Raw, &SERIES)
        .expect("the pool holds the frame")
        .freeze(Path::Live);
    let narrow = Mask::new(set, [set.entries()[2].slot]);
    let full = Mask::new(set, set.entries().iter().map(|entry| entry.slot));
    let slot = |entry: usize| set.entries()[entry].slot;
    // Leaves out key 3, so the frame's only series left is the index.
    let most = Mask::new(set, [0, 1, 3].map(slot));
    // A mask holds the index of each channel it wants; places name it.
    let mut places = [
        Places::new([0, 2].map(slot).into()),
        Places::new(set.entries().iter().map(|entry| entry.slot).collect()),
        Places::new([0, 1, 3].map(slot).into()),
    ];
    for places in &mut places {
        places.lay(&frame, set);
    }
    let (read, allocations) = ALLOCATOR.count(|| {
        let view = View::new(&frame, &narrow);
        let read: usize = view.iter().map(|(_, bytes)| bytes.len()).sum();
        let view = View::new(&frame, &full);
        let full_read: usize = view.iter().map(|(_, bytes)| bytes.len()).sum();
        let view = View::new(&frame, &most);
        let mut most_read = 0;
        for (_, bytes) in view.iter() {
            most_read += bytes.len();
        }
        let bounded: usize = places
            .iter_mut()
            .map(|places| {
                let laid = places.lay(&frame, set).iter();
                laid.map(|placed| placed.bounds.len()).sum::<usize>()
            })
            .sum();
        (read, full_read, most_read, bounded)
    });
    assert_eq!(allocations, 0, "the view allocated");
    assert_eq!(
        read,
        (32, 32, 16, 80),
        "the views read both series, then the index, and bound what they read"
    );
}

fn receive_a_frame(pool: &block::Pool, set: &KeySet) {
    let lens = [(0, 3), (2, 16)];
    let mut home = Draft::new(pool, set, Form::Raw, &lens).expect("the pool holds it");
    home.series_mut(2).expect("entry 2 is present").fill(7);
    let home = home.freeze(Path::Live);
    let (received, allocations) = ALLOCATOR.count(|| {
        let mut ends = [(0, 0); 2];
        for (end, given) in ends.iter_mut().zip(frame::ends(lens)) {
            *end = given;
        }
        let layout = Layout::from_ends(set, &ends).expect("the ends fit");
        let charge = frame::charge(ends.len(), layout.body_len());
        let mut draft = layout.draft(pool, Form::Raw).expect("the pool holds it");
        draft.body_mut().copy_from_slice(&home.body());
        draft.set_count(0, 1);
        let frame = draft.freeze(Path::Live);
        assert_eq!(frame.charge(), charge, "both ends charge the frame alike");
        frame.series(2) == Some([7; 16].as_slice())
    });
    assert_eq!(allocations, 0, "the receive allocated");
    assert!(received, "the frame holds the bytes the home sent");
}

fn lay_places(pool: &block::Pool, set: &KeySet, other: &KeySet) {
    let frames = [
        (Draft::new(pool, set, Form::Raw, &SERIES), set),
        (Draft::new(pool, other, Form::Raw, &[(0, 8), (1, 8)]), other),
    ]
    .map(|(draft, set)| (draft.expect("the pool holds it").freeze(Path::Live), set));
    // Key 3, then key 1: not every entry, so the charge lays each frame.
    let slot = |entry: usize| set.entries()[entry].slot;
    let mut places = Places::new([slot(2), slot(0)].into());
    let mut charged = 0;
    for (frame, set) in &frames {
        places.lay(frame, set);
        charged += places.charge(frame, set);
    }
    let (laid, allocations) = ALLOCATOR.count(|| {
        let (mut laid, mut again) = (0, 0);
        for (frame, set) in &frames {
            laid += places.lay(frame, set).len();
            again += places.charge(frame, set);
        }
        (laid, again)
    });
    assert_eq!(allocations, 0, "the places allocated");
    assert_eq!(laid, (3, charged), "key 3 and key 1, then key 1");
    lay_sparse_and_dense_frames(pool);
}

/// Laying a sparse frame, then a dense one of the same key set, allocates only for
/// the first of each.
fn lay_sparse_and_dense_frames(pool: &block::Pool) {
    let data: Vec<(Key, Type)> = (2..42).map(|n| (Key::from_u128(n), F64)).collect();
    let mut interner = Interner::new();
    let set = interner.intern(&[Group {
        index: Key::from_u128(1),
        data: &data,
    }]);
    let frames = [
        vec![(0, 8), (30, 8)],
        (0..8).map(|entry| (entry, 8)).collect(),
    ]
    .map(|lens| {
        let draft = Draft::new(pool, &set, Form::Raw, &lens);
        draft.expect("the pool holds it").freeze(Path::Live)
    });
    let mut places = Places::new(set.entries().iter().rev().map(|e| e.slot).collect());
    for frame in &frames {
        places.lay(frame, &set);
    }
    let (laid, allocations) = ALLOCATOR.count(|| {
        let mut laid = 0;
        for frame in frames.iter().chain(&frames) {
            laid += places.lay(frame, &set).len();
        }
        laid
    });
    assert_eq!(allocations, 0, "the places allocated");
    assert_eq!(laid, 2 * (2 + 8), "both series, then each of 8");
}
