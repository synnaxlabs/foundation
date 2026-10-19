//! A pool from `Pool::erased` leaves the bytes of its blocks in no freed allocation.
//! The scan covers every thread, so this binary has no test harness.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use block::{Config, Pool};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// The bytes that a case puts in a block and the scan looks for.
type Needle = [u8; 32];

/// What a case does with its pool before the pool and its blocks drop.
#[derive(Clone, Copy, Debug)]
enum Case {
    /// The pool drops while it lends a block of 32 bytes and one of `len` bytes. Block
    /// `at` holds the needle at its end.
    Lent { len: usize, at: usize },
    /// The pool carves a block again after a purge.
    Recarved,
    /// A block ends where the released span of a larger class starts.
    Pressed,
}

impl Case {
    fn budget(self) -> u64 {
        match self {
            Self::Lent { .. } | Self::Recarved => 1 << 14,
            Self::Pressed => 384,
        }
    }

    fn run(self, pool: Pool, needle: &Needle) {
        match self {
            Self::Lent { len, at } => lent(pool, len, at, needle),
            Self::Recarved => recarved(pool, needle),
            Self::Pressed => pressed(pool, needle),
        }
    }
}

/// The last case leaves its needle past the first page. With the heap cases first, an
/// erased case that commits only the first page of a 16 KiB budget meets that memory.
const CASES: [Case; 6] = [
    Case::Recarved,
    Case::Pressed,
    Case::Lent { len: 32, at: 0 },
    // Its needle is in a committed page that is not the first of its group of 64.
    Case::Lent { len: 128, at: 1 },
    Case::Lent { len: 3000, at: 0 },
    Case::Lent { len: 3000, at: 1 },
];

fn main() {
    // Miri stops at the read of a freed byte that the program never wrote (COUNTING
    // ALLOCATOR). The unit tests of `block` run the erase under Miri.
    if cfg!(miri) {
        return;
    }
    // A needle for each call, as an erased pool's memory can reuse memory that an
    // earlier pool freed with its needle in a page that the erased pool never commits.
    // The heap cases run first, so that the erased cases meet such memory.
    let mut needles = (0..=u8::MAX).map(needle);
    let mut next = || needles.next().expect("a needle for each call");
    for case in CASES {
        let held = freed(Pool::heap, case, &next());
        assert_eq!(
            held, 1,
            "a dropped `Pool::heap` frees the bytes of {case:?}"
        );
    }
    for case in CASES {
        let held = freed(Pool::erased, case, &next());
        assert_eq!(
            held, 0,
            "a dropped `Pool::erased` erases the bytes of {case:?}"
        );
    }
}

/// The needle of call `call`. No other call and no fill of one byte gives it.
fn needle(call: u8) -> Needle {
    let mut needle = *b"?a needle that no fill can give.";
    needle[0] = call;
    needle
}

/// Gives the count of freed allocations that hold `needle`, after `case` runs on the
/// pool that `pool` gives for the case's budget and drops it.
fn freed(pool: fn(Config) -> Pool, case: Case, needle: &Needle) -> u64 {
    let pool = pool(Config::new(case.budget()).expect("the budget fits"));
    let ((), found) = ALLOCATOR.freed_holding(needle, || case.run(pool, needle));
    found
}

/// Drops `pool` while it lends a block of 32 bytes and one of `len` bytes, then the
/// blocks. Block `at` holds `needle` at its end.
fn lent(pool: Pool, len: usize, at: usize, needle: &Needle) {
    let blocks: Vec<_> = [32, len]
        .into_iter()
        .enumerate()
        .map(|(index, len)| {
            let mut block = pool.alloc(len).expect("the budget has room");
            block.fill(0x11);
            if index == at {
                block[len - needle.len()..].copy_from_slice(needle);
            }
            block.freeze()
        })
        .collect();
    drop(pool);
    assert!(
        blocks.iter().any(|block| block.ends_with(needle)),
        "a lent block outlives its pool"
    );
}

/// Puts `needle` in a block, purges its class, and carves the class again with a
/// block that ends before `needle`.
fn recarved(pool: Pool, needle: &Needle) {
    let mut first = pool.alloc(32).expect("the budget has room");
    first.fill(0x11);
    let mut second = pool.alloc(32).expect("the budget has room");
    second.copy_from_slice(needle);
    drop(first);
    drop(second);
    pool.purge();
    pool.purge();
    drop(pool.alloc(32).expect("the budget has room"));
    drop(pool);
}

/// Carves class 2, then two blocks of class 1, whose second ends at the start of the
/// released span of class 2 and holds `needle` at its end.
fn pressed(pool: Pool, needle: &Needle) {
    drop(pool.alloc(192).expect("the budget has room"));
    let mut first = pool.alloc(128).expect("the budget has room");
    first.fill(0x11);
    let mut second = pool.alloc(128).expect("the budget has room");
    second.fill(0x11);
    second[128 - needle.len()..].copy_from_slice(needle);
    drop(pool);
    drop(first);
    drop(second);
}
