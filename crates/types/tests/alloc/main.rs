//! Building and reading a frame or a view makes no heap allocation. This binary has
//! no test harness: the count covers each thread, and a harness allocates on its own
//! thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use types::channel::Key;
use types::frame::key_set::{Group, Interner, KeySet};
use types::frame::{self, Draft, Form, Mask, Path, Range, View};
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
    let set = Interner::new().intern(&groups);
    let config = block::Config { budget: 1 << 16 };
    let pool = block::Pool::new(config.clone(), block::Heap::new(config.reservation()));
    read_a_frame(&pool, &set);
    read_a_view(&pool, &set);
    check_a_search();
}

/// The index slots of both groups come first, so the data of group 0 search for their
/// index.
fn check_a_search() {
    let keys = [10, 11, 12].map(Key::from_u128);
    let mut interner = Interner::new();
    for key in keys {
        interner.slots().assign(key);
    }
    let data = [(keys[2], F64)];
    let set = interner.intern(&[
        Group {
            index: keys[0],
            data: &data,
        },
        Group {
            index: keys[1],
            data: &[],
        },
    ]);
    assert_eq!(set.index(2), 0, "entry 2 is data of group 0");
    let series = [(0, 8), (1, 8), (2, 8)];
    let (body, allocations) = ALLOCATOR
        .count(|| frame::Layout::new(&set, &series).map(|layout| layout.body_len()));
    assert_eq!(allocations, 0, "the search allocated");
    assert_eq!(body, Ok(24), "the layout holds each series");
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
    let index = Mask::new(set, [slot(0)]);
    let (read, allocations) = ALLOCATOR.count(|| {
        let view = View::new(&frame, &narrow);
        let read: usize = view.iter().map(|(_, bytes)| bytes.len()).sum();
        assert_eq!(view.charge(), 192, "the view charges both series");
        let view = View::new(&frame, &full);
        let full_read: usize = view.iter().map(|(_, bytes)| bytes.len()).sum();
        assert_eq!(
            view.charge(),
            frame.charge(),
            "a full view charges the frame"
        );
        let view = View::new(&frame, &most);
        let mut most_read = 0;
        for (_, bytes) in view.iter() {
            most_read += bytes.len();
        }
        assert_eq!(
            view.charge(),
            View::new(&frame, &index).charge(),
            "a view of most charges as a view of the series it holds"
        );
        (read, full_read, most_read)
    });
    assert_eq!(allocations, 0, "the view allocated");
    assert_eq!(
        read,
        (32, 32, 16),
        "the views read both series, then the index"
    );
}
