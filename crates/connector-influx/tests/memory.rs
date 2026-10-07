//! A store holds each point in a few tens of bytes, so the STORE AND FORWARD scenario
//! fits a CI runner. The count covers each thread, so this binary has no test harness.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::io::Write as _;

use connector_influx::sim::Store;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// The points of each write order. The lab's data lines take `LAB` points.
const POINTS: usize = 200_000;
const LAB: usize = 1_000_000;

/// The most points that a chunk of the store holds.
const CHUNK: usize = 4096;

/// The most heap bytes a point with one float field may take: 8 of time, 8 of value,
/// and 2 of index, with room for the chunks.
const BUDGET: usize = 24;

/// The most that SIM INFLUX allows, which also holds with 255 sparse fields: each
/// chunk keeps a column for each field it holds.
const SPARSE: usize = 32;

/// The fewest: 8 of time and 8 of value.
const FLOOR: usize = 16;

fn main() {
    check("the lab's data lines", LAB, BUDGET, |k, line| {
        let time = 1_000_000 + k * 1_000;
        writeln!(line, "m,node=edge,unit=V value={k} {time}")
    });
    for (fields, budget) in [(8, BUDGET), (63, BUDGET), (255, SPARSE)] {
        let name = format!("{fields} fields, each set by every {fields}th point");
        check(&name, POINTS, budget, |k, line| {
            writeln!(line, "m c{}={k} {}", k % fields, 1_000_000 + k)
        });
    }
    check("newest first", POINTS, BUDGET, |k, line| {
        writeln!(line, "m value={k} {}", POINTS - k)
    });
    check("each pair of times swapped", POINTS, BUDGET, |k, line| {
        writeln!(line, "m value={k} {}", 1_000_000 + (k ^ 1))
    });
    between();
    check(
        "a full chunk, then the rest newest first",
        POINTS,
        BUDGET,
        |k, line| {
            let time = if k < CHUNK { k } else { 1_000_000 + POINTS - k };
            writeln!(line, "m value={k} {time}")
        },
    );
    check(
        "live, then an outage newest first",
        POINTS,
        BUDGET,
        |k, line| {
            let live = POINTS / 2;
            let time = match k {
                k if k < CHUNK => k,
                k if k < live => live + k,
                k => POINTS + CHUNK - 1 - k,
            };
            writeln!(line, "m value={k} {time}")
        },
    );
}

/// The orders that write a time between two stored times, in a full chunk.
fn between() {
    check(
        "the even times, then the odd times between them",
        POINTS,
        BUDGET,
        |k, line| {
            let time = (k % (POINTS / 2)) * 2 + k / (POINTS / 2);
            writeln!(line, "m value={k} {time}")
        },
    );
    check(
        "the even times, then one odd time in each chunk",
        POINTS,
        BUDGET,
        |k, line| {
            let evens = POINTS - POINTS / (CHUNK + 1);
            let time = match k.checked_sub(evens) {
                None => 2 * k,
                Some(chunk) => 2 * CHUNK * chunk + CHUNK + 1,
            };
            writeln!(line, "m value={k} {time}")
        },
    );
    check(
        "the even times, then one odd time in each half of each chunk",
        POINTS,
        BUDGET,
        |k, line| {
            let evens = POINTS - 2 * (POINTS / (CHUNK + 2));
            let time = match k.checked_sub(evens) {
                None => 2 * k,
                Some(odd) => 2 * CHUNK * (odd / 2) + 1 + (odd % 2) * 6000,
            };
            writeln!(line, "m value={k} {time}")
        },
    );
}

/// Writes `points` points to a store, the `k`-th by `line(k, body)`, and checks that
/// the store holds `FLOOR` to `budget` heap bytes a point.
fn check(
    name: &str,
    points: usize,
    budget: usize,
    line: impl Fn(usize, &mut Vec<u8>) -> std::io::Result<()>,
) {
    let mut store = Store::default();
    let mut body = Vec::new();
    let before = ALLOCATOR.held();
    for k in 0..points {
        line(k, &mut body).expect("a Vec takes each write");
        if body.len() > 1 << 20 {
            store.write(&body).expect("valid lines");
            body.clear();
        }
    }
    store.write(&body).expect("valid lines");
    drop(body);
    let held = ALLOCATOR.held().strict_sub(before);
    assert_eq!(store.points("m", &[]).count(), points, "{name}: each point");
    assert!(
        (FLOOR * points..=budget * points).contains(&held),
        "{name}: the store holds {held} bytes, not {FLOOR} to {budget} a point"
    );
}
