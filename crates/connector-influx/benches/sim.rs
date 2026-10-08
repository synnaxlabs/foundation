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
            POINTS,
            "{shape}: each point"
        );
    }
    divan::main();
}

/// The points of each body: 8 full chunks.
const POINTS: usize = 8 * 4096;

/// The first time, in nanoseconds: September 2026.
const T0: usize = 1_790_000_000_000_000_000;

/// A body of `POINTS` lines, the same on each run.
#[derive(Clone, Copy)]
struct Shape {
    name: &'static str,
    /// Writes the `k`-th line.
    line: fn(usize, &mut Vec<u8>) -> std::io::Result<()>,
}

const LAB: Shape = Shape::new("lab", |k, body| {
    writeln!(body, "m,node=edge,unit=V value={k} {}", T0 + k * 1000)
});

const SPARSE: Shape = Shape::new("sparse", |k, body| {
    writeln!(body, "m c{}={k} {}", k % 63, T0 + k)
});

const SHAPES: [Shape; 6] = [
    LAB,
    Shape::new("newest_first", |k, body| {
        writeln!(
            body,
            "m,node=edge,unit=V value={k} {}",
            T0 + (POINTS - k) * 1000
        )
    }),
    Shape::new("evens_then_odds", |k, body| {
        let time = (k % (POINTS / 2)) * 2 + k / (POINTS / 2);
        writeln!(body, "m,node=edge,unit=V value={k} {}", T0 + time)
    }),
    SPARSE,
    Shape::new("series", |k, body| {
        writeln!(body, "m,s={} value={k} {}", k % 16, T0 + k)
    }),
    Shape::new("string", |k, body| {
        writeln!(body, "m state=\"running {}\" {}", k % 8, T0 + k)
    }),
];

impl Shape {
    const fn new(
        name: &'static str,
        line: fn(usize, &mut Vec<u8>) -> std::io::Result<()>,
    ) -> Self {
        Self { name, line }
    }

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
        .counter(ItemsCount::new(POINTS))
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
    bencher.counter(ItemsCount::new(POINTS)).bench_local(|| {
        for point in divan::black_box(&store).points("m", &[]) {
            for field in point.fields.iter() {
                divan::black_box_drop(field);
            }
        }
    });
}
