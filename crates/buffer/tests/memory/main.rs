//! A trim gives back the room of the runs that it drops, so the logs keep the
//! memory bound of S4. This binary has no test harness: the count covers each
//! thread, and a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

/// The bytes of one run.
const RUN: usize = 24;
/// The records of a lap of `buffer::bench::Logs`, one per commit.
const LAP: u32 = 256;

/// A read that holds 16 records keeps 17 runs of one path, in room for 32. Then the
/// read holds `held` records. Gives the bytes that the trims of the next two laps
/// give back, and checks that the trims of the lap after them give back none.
fn freed(held: u64) -> usize {
    let mut logs = buffer::bench::Logs::new(1);
    for _ in 0..40 {
        logs.trim(1, 16);
    }
    let before = ALLOCATOR.held();
    for _ in 0..2 * LAP {
        logs.trim(1, held);
    }
    let after = ALLOCATOR.held();
    for _ in 0..LAP {
        logs.trim(1, held);
    }
    assert_eq!(
        ALLOCATOR.held(),
        after,
        "a lap that keeps the count of runs gave back bytes"
    );
    before - after
}

/// A read that holds 16 records keeps 17 runs of one path, in room for 32, for 300
/// commits. Then no read holds a record. Gives the number of the commit, from the
/// first, whose trim gives back bytes: the first trim at least one lap after the
/// start of its lap.
fn first_shrink() -> u32 {
    let mut logs = buffer::bench::Logs::new(1);
    for _ in 0..300 {
        logs.trim(1, 16);
    }
    for commit in 301..=4 * LAP {
        let before = ALLOCATOR.held();
        logs.trim(1, 0);
        if ALLOCATOR.held() < before {
            return commit;
        }
    }
    panic!("no trim up to commit {} gave back bytes", 4 * LAP);
}

/// S4: each path keeps at most 414 bytes, and room for four paths when there are
/// fewer. A carry record takes the tail of each of `paths` paths, and a trim to it
/// leaves each with no run. The holder items of the two records, 4 bytes each in a
/// deque that doubles, are a term of S4 per record and path. At 3585 paths the map
/// of places is at its least load, and at 4097 the vector of logs.
fn per_path(paths: u32) {
    let before = ALLOCATOR.held();
    let mut logs = buffer::bench::Logs::new(paths);
    let base = ALLOCATOR.held() - before;
    logs.carry();
    logs.commit(0);
    logs.commit(0);
    let held = ALLOCATOR.held() - before - base;
    let paths = usize::try_from(paths).expect("fits");
    let bound = 414 * paths.max(4) + 16 * paths;
    assert!(
        held <= bound,
        "{held} bytes for {paths} paths, over {bound}"
    );
    drop(logs);
}

fn main() {
    assert_eq!(freed(6), 0, "a lap of at most 8 runs keeps room for 32");
    assert_eq!(
        freed(4),
        (32 - 12) * RUN,
        "a lap of at most 6 runs keeps room for 12"
    );
    assert_eq!(
        freed(2),
        (32 - 8) * RUN,
        "a lap of at most 4 runs keeps room for 8"
    );
    assert_eq!(
        freed(0),
        (32 - 4) * RUN,
        "a lap of at most 2 runs keeps room for 4"
    );
    assert_eq!(
        first_shrink(),
        3 * LAP + 1,
        "the lap that starts at 512 ends at 768"
    );
    for paths in [1, 2, 3, 4, 5, 3585, 4097] {
        per_path(paths);
    }
}
