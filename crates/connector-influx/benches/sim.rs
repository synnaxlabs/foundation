//! The time to write points to the simulated store, from an empty store each run, and
//! to read each field of each point back. Before it times anything, it checks that
//! each body stores each of its points.

use std::fmt;
use std::io::Write as _;

use connector_influx::sim::Store;
use divan::Bencher;
use divan::counter::ItemsCount;

fn main() {
    for shape in SHAPES {
        let store = shape.stored();
        assert_eq!(
            store.points("m", &[]).count(),
            count(),
            "{shape}: each point"
        );
    }
    divan::main();
}

/// The points of each body: 8 full chunks.
const POINTS: i64 = 8 * 4096;

/// The first time, in nanoseconds: September 2026.
const T0: i64 = 1_790_000_000_000_000_000;

/// `POINTS` as a count.
fn count() -> usize {
    usize::try_from(POINTS).expect("POINTS is above zero")
}

/// A body of `POINTS` lines, the same on each run.
#[derive(Clone, Copy)]
struct Shape {
    name: &'static str,
    /// Writes the `k`-th line.
    line: fn(i64, &mut Vec<u8>) -> std::io::Result<()>,
}

const LAB: Shape = Shape {
    name: "lab",
    line: |k, body| lab(k, k * 1000, body),
};

const SPARSE: Shape = Shape {
    name: "sparse",
    line: |k, body| writeln!(body, "m c{}={k} {}", k % 63, T0 + k),
};

const SHAPES: [Shape; 6] = [
    LAB,
    Shape {
        name: "newest_first",
        line: |k, body| lab(k, (POINTS - k) * 1000, body),
    },
    Shape {
        name: "evens_then_odds",
        line: |k, body| lab(k, (k % (POINTS / 2)) * 2 + k / (POINTS / 2), body),
    },
    SPARSE,
    Shape {
        name: "series",
        line: |k, body| writeln!(body, "m,s={} value={k} {}", k % 16, T0 + k),
    },
    Shape {
        name: "string",
        line: |k, body| writeln!(body, "m state=\"running {}\" {}", k % 8, T0 + k),
    },
];

/// Writes the lab's line for the `k`-th point, at `time` after `T0`.
fn lab(k: i64, time: i64, body: &mut Vec<u8>) -> std::io::Result<()> {
    writeln!(body, "m,node=edge,unit=V value={k} {}", T0 + time)
}

impl Shape {
    fn body(self) -> Vec<u8> {
        let mut body = Vec::new();
        for k in 0..POINTS {
            (self.line)(k, &mut body).expect("a Vec takes each write");
        }
        body
    }

    fn stored(self) -> Store {
        let mut store = Store::default();
        store.write(&self.body()).expect("valid lines");
        store
    }
}

impl fmt::Display for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

#[divan::bench(args = SHAPES)]
fn write(bencher: Bencher<'_, '_>, shape: Shape) {
    let body = shape.body();
    bencher
        .counter(ItemsCount::new(count()))
        .with_inputs(Store::default)
        .bench_local_values(|mut store| {
            store.write(divan::black_box(&body)).expect("valid lines");
            store
        });
}

/// Reads each field of each point, with `Fields::iter`.
#[divan::bench(args = [LAB, SPARSE])]
fn points(bencher: Bencher<'_, '_>, shape: Shape) {
    let store = shape.stored();
    bencher.counter(ItemsCount::new(count())).bench_local(|| {
        for point in divan::black_box(&store).points("m", &[]) {
            for field in point.fields.iter() {
                divan::black_box_drop(field);
            }
        }
    });
}
