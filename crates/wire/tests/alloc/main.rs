//! The hub messages on a frame's path make no heap allocation. This binary has no
//! test harness: the count covers each thread, and a harness allocates on its own
//! thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use types::frame::{Path, Range};
use wire::hub::{Credit, Head, Reply, ends};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );

    let head = Reply::Head(Head {
        path: Path::Backfill,
        range: Range { seq: 7, count: 1 },
        series: 3,
    });
    let mut out = [0; 18];
    let ((), allocations) = ALLOCATOR.count(|| head.encode(&mut out));
    assert_eq!(allocations, 0, "the head encode allocated");
    let (decoded, allocations) = ALLOCATOR.count(|| Reply::decode(&out));
    assert_eq!(allocations, 0, "the head decode allocated");
    assert_eq!(decoded, Ok(head), "the head round trips");

    let credit = Credit { limit_bytes: 7 };
    let mut out = [0; Credit::LEN];
    let ((), allocations) = ALLOCATOR.count(|| credit.encode(&mut out));
    assert_eq!(allocations, 0, "the credit encode allocated");
    let (decoded, allocations) = ALLOCATOR.count(|| Credit::decode(&out));
    assert_eq!(allocations, 0, "the credit decode allocated");
    assert_eq!(decoded, Ok(credit), "the credit round trips");

    for series in [1, 1_000, 100_000_u32] {
        let mut run = vec![0; usize::try_from(series).expect("a u32 fits a usize") * 8];
        let ends = (0..series).map(|place| (place, place.wrapping_add(1)));
        let ((), allocations) = ALLOCATOR.count(|| ends::encode(ends, &mut run));
        assert_eq!(allocations, 0, "the encode of {series} ends allocated");
        let (last, allocations) =
            ALLOCATOR.count(|| ends::decode(&run).map(Iterator::last));
        assert_eq!(allocations, 0, "the decode of {series} ends allocated");
        assert_eq!(
            last,
            Ok(Some((series - 1, series))),
            "the last end round trips"
        );
    }
}
