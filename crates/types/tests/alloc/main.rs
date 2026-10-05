//! Building and reading a frame makes no heap allocation. This binary has no test
//! harness: the count covers each thread, and a harness allocates on its own thread at
//! any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use types::channel::Slot;
use types::frame::key_set::{Group, Interner};
use types::frame::{Draft, Form, Path, Range};
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

    let data = [(Slot::new(2), F64), (Slot::new(3), F64)];
    let set = Interner::new().intern(&[
        Group {
            index: Slot::new(1),
            data: &data,
        },
        Group {
            index: Slot::new(4),
            data: &[],
        },
    ]);
    let config = block::Config { budget: 1 << 16 };
    let pool = block::Pool::new(config.clone(), block::Heap::new(config.reservation()));
    let series = [(0, 16), (2, 16)];
    let (sum, allocations) = ALLOCATOR.count(|| {
        let mut draft = Draft::new(&pool, &set, Form::Raw, &series)
            .expect("the pool holds the frame");
        for (entry, bytes) in draft.iter_mut() {
            bytes.fill(u8::try_from(entry).expect("entries are small"));
        }
        draft.series(0).expect("entry 0 is present").fill(1);
        draft.set_count(0, 2);
        draft.set_seq(0, 9);
        assert_eq!(
            draft.range(0),
            Some(Range { seq: 9, count: 2 }),
            "the draft reads its range"
        );
        let frame = draft.freeze(Path::Backfill);
        let copy = frame.clone();
        assert_eq!(frame.charge(), 192, "the frame charges its block");
        assert_eq!(frame.key_set(), set.key(), "the frame names its key set");
        assert_eq!(frame.path(), Path::Backfill, "the frame keeps its path");
        assert_eq!(frame.form(), Form::Raw, "the frame keeps its form");
        assert_eq!(frame.series(1), None, "entry 1 is absent");
        assert_eq!(frame.range(1), None, "group 1 is absent");
        assert_eq!(
            frame.series(2),
            Some([2; 16].as_slice()),
            "entry 2 reads back"
        );
        assert_eq!(frame.body().len(), 32, "the body views both series");
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
