//! The time to write points as line protocol, into a body with room for each line.
//! Before it times anything, it checks that the simulated store stores each point of
//! each body.

use std::fmt;

use connector_influx::line::{Float, Measurement, Value};
use connector_influx::sim::Store;
use divan::Bencher;
use divan::counter::ItemsCount;
use types::time::Stamp;

fn main() {
    for shape in SHAPES {
        let mut store = Store::default();
        let body = shape.body(&shape.measurement(), Vec::new());
        store.write(&body).expect("each line stores");
        assert_eq!(
            store.points("m", &[]).count(),
            count(),
            "{shape}: each point"
        );
    }
    divan::main();
}

/// The points of each body.
const POINTS: u32 = 32768;

/// The first time, in nanoseconds: September 2026.
const T0: i64 = 1_790_000_000_000_000_000;

/// One measurement and the values of its `k`-th point.
#[derive(Clone, Copy)]
struct Shape {
    name: &'static str,
    fields: &'static [&'static str],
    /// Gives the values of the `k`-th point.
    values: fn(u32, &mut [Option<Value>]),
}

const SHAPES: [Shape; 3] = [
    Shape {
        name: "float",
        fields: &["value"],
        values: |k, values| values[0] = Some(float(k)),
    },
    Shape {
        name: "mixed",
        fields: &["value", "count", "open"],
        values: |k, values| {
            values.copy_from_slice(&[
                Some(float(k)),
                Some(Value::Integer(i64::from(k))),
                Some(Value::Boolean(k % 2 == 0)),
            ]);
        },
    },
    Shape {
        name: "sparse",
        fields: &["c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7"],
        values: |k, values| {
            values.fill(None);
            values[k as usize % 8] = Some(float(k));
        },
    },
];

/// `POINTS` as a count.
fn count() -> usize {
    usize::try_from(POINTS).expect("POINTS fits usize")
}

fn float(k: u32) -> Value {
    Value::Float(Float::new(f64::from(k) * 0.25).expect("k is finite"))
}

impl Shape {
    fn measurement(self) -> Measurement {
        Measurement::new("m", &[("node", "edge"), ("unit", "V")], self.fields)
            .expect("the names are valid")
    }

    /// Appends the `POINTS` lines of `measurement` to `body`.
    fn body(self, measurement: &Measurement, mut body: Vec<u8>) -> Vec<u8> {
        let mut values = vec![None; self.fields.len()];
        for k in 0..POINTS {
            (self.values)(k, &mut values);
            let time = T0 + i64::from(k);
            measurement.line(&mut body, &values, Stamp::from_nanos(time));
        }
        body
    }
}

impl fmt::Display for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)
    }
}

#[divan::bench(args = SHAPES)]
fn line(bencher: Bencher<'_, '_>, shape: Shape) {
    let measurement = shape.measurement();
    let room = shape.body(&measurement, Vec::new()).len();
    bencher
        .counter(ItemsCount::new(count()))
        .with_inputs(|| Vec::with_capacity(room))
        .bench_local_values(|body| shape.body(&measurement, body));
}
