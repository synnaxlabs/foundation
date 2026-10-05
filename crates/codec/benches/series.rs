//! The time to encode, validate, and decode series shaped like sensor data, and
//! series that FFOR and delta pack at chosen bit widths. Before it times anything, it
//! checks that each full series compresses at least as well as its floor.

use std::f64::consts::TAU;
use std::fmt;

use codec::{Encoder, VECTOR_LEN, max_len};
use divan::Bencher;
use divan::counter::ItemsCount;
use types::sample::Scalar;

fn main() {
    for shape in &SHAPES {
        shape.check_ratio();
    }
    divan::main();
}

/// Samples in a full series.
const LEN: usize = 256 * VECTOR_LEN;

const FULL: &[usize] = &[LEN];

/// A full series and three shorter ones, the last of which ends inside a vector.
const EVERY: &[usize] = &[10, 100, 1000, LEN];

/// The first timestamp, in nanoseconds: September 2026.
const T0: i64 = 1_790_000_000_000_000_000;

/// A kind of series, the same on each run.
struct Shape {
    name: &'static str,
    scalar: Scalar,
    /// Creates `LEN` samples, each cut to the scalar's width.
    create: fn() -> Vec<i64>,
    /// The least ratio of raw to encoded bytes for `LEN` samples.
    ratio: f64,
    /// The series lengths to time.
    lens: &'static [usize],
}

const SHAPES: [Shape; 22] = [
    Shape::new("adc16.s1", Scalar::I16, create_adc16_s1, 3.933, EVERY),
    Shape::new("adc16.s256", Scalar::I16, create_adc16_s256, 1.352, FULL),
    Shape::new("adc16.white", Scalar::I16, create_adc16_white, 0.994, FULL),
    Shape::new("adc24.s6", Scalar::I32, create_adc24_s6, 4.615, EVERY),
    Shape::new("adc24.s100", Scalar::I32, create_adc24_s100, 3.131, FULL),
    Shape::new("u8.state", Scalar::U8, create_state, 229.0, EVERY),
    Shape::new("u32.state", Scalar::U32, create_state, 422.0, FULL),
    Shape::new("ts.fixed", Scalar::Stamp, create_ts_fixed, 339.6, FULL),
    Shape::new("ts.soft", Scalar::Stamp, create_ts_soft, 3.018, EVERY),
    Shape::new("u8.delta3", Scalar::U8, create_walk::<3>, 2.626, FULL),
    Shape::new("u8.ffor1", Scalar::U8, create_uniform::<1>, 7.777, FULL),
    Shape::new("u8.ffor3", Scalar::U8, create_uniform::<3>, 2.632, FULL),
    Shape::new("u8.ffor6", Scalar::U8, create_uniform::<6>, 1.321, FULL),
    Shape::new("u16.ffor1", Scalar::U16, create_uniform::<1>, 15.43, FULL),
    Shape::new("u16.ffor8", Scalar::U16, create_uniform::<8>, 1.982, FULL),
    Shape::new("u16.ffor13", Scalar::U16, create_uniform::<13>, 1.221, FULL),
    Shape::new("u32.ffor1", Scalar::U32, create_uniform::<1>, 29.96, FULL),
    Shape::new("u32.ffor16", Scalar::U32, create_uniform::<16>, 1.982, FULL),
    Shape::new("u32.ffor27", Scalar::U32, create_uniform::<27>, 1.176, FULL),
    Shape::new("u64.ffor1", Scalar::U64, create_uniform::<1>, 56.6, FULL),
    Shape::new("u64.ffor32", Scalar::U64, create_uniform::<32>, 1.982, FULL),
    Shape::new("u64.ffor55", Scalar::U64, create_uniform::<55>, 1.155, FULL),
];

impl Shape {
    const fn new(
        name: &'static str,
        scalar: Scalar,
        create: fn() -> Vec<i64>,
        ratio: f64,
        lens: &'static [usize],
    ) -> Self {
        Self {
            name,
            scalar,
            create,
            ratio,
            lens,
        }
    }

    /// # Panics
    ///
    /// If the full series compresses worse than `self.ratio`.
    fn check_ratio(&self) {
        let raw = LEN * self.scalar.width();
        #[expect(clippy::cast_precision_loss, reason = "both lengths are under 2^53")]
        let ratio = raw as f64 / self.encoded(LEN).len() as f64;
        assert!(
            ratio >= self.ratio,
            "{} compresses {ratio:.3} to 1, less than its floor of {}",
            self.name,
            self.ratio
        );
    }

    /// The first `len` samples, little-endian.
    fn values(&self, len: usize) -> Vec<u8> {
        let width = self.scalar.width();
        (self.create)()
            .into_iter()
            .take(len)
            .flat_map(|sample| sample.to_le_bytes().into_iter().take(width))
            .collect()
    }

    /// The first `len` samples, encoded.
    ///
    /// # Panics
    ///
    /// If the encoding does not validate or decode to the samples.
    fn encoded(&self, len: usize) -> Vec<u8> {
        let values = self.values(len);
        let mut out = vec![0; max_len(self.scalar, len)];
        let written = Encoder::new(self.scalar).encode(&values, &mut out);
        out.truncate(written);
        let mut decoded = vec![0; values.len()];
        let valid = codec::validate(self.scalar, len, &out);
        assert_eq!(valid, Ok(()), "{}", self.name);
        let result = codec::decode(self.scalar, len, &out, &mut decoded);
        assert_eq!(result, Ok(()), "{}", self.name);
        assert!(decoded == values, "{} decodes to other samples", self.name);
        out
    }
}

/// A shape at one length.
#[derive(Clone, Copy)]
struct Case {
    shape: &'static Shape,
    len: usize,
}

impl fmt::Display for Case {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.len == LEN {
            f.write_str(self.shape.name)
        } else {
            write!(f, "{}/{}", self.shape.name, self.len)
        }
    }
}

fn cases() -> impl Iterator<Item = Case> {
    SHAPES
        .iter()
        .flat_map(|shape| shape.lens.iter().map(move |&len| Case { shape, len }))
}

/// A splitmix64 generator.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `[0, 1)`.
    #[expect(clippy::cast_precision_loss, reason = "53 bits fit an f64 exactly")]
    fn uniform(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1_u64 << 53) as f64
    }

    /// A standard normal value, by Box-Muller.
    fn normal(&mut self) -> f64 {
        let radius = (-2.0 * self.uniform().max(f64::MIN_POSITIVE).ln()).sqrt();
        radius * (TAU * self.uniform()).cos()
    }
}

#[expect(clippy::cast_possible_truncation, reason = "the values fit an `i64`")]
fn round(x: f64) -> i64 {
    x.round() as i64
}

/// Counts of a `bits`-bit ADC: a sine with a period of 65,536 samples, plus normal
/// noise. `noise` is the standard deviation, in counts.
fn adc(random: &mut Random, bits: u32, amplitude: f64, noise: f64) -> Vec<i64> {
    let max = (1 << (bits - 1)) - 1;
    (0..LEN)
        .map(|i| {
            let i = f64::from(u32::try_from(i).expect("invariant: `LEN` fits a `u32`"));
            let x = amplitude * (TAU * i / 65_536.0).sin() + noise * random.normal();
            round(x).clamp(-max - 1, max)
        })
        .collect()
}

fn create_adc16_s1() -> Vec<i64> {
    adc(&mut Random(1), 16, 20_000.0, 1.0)
}

fn create_adc16_s256() -> Vec<i64> {
    adc(&mut Random(2), 16, 20_000.0, 256.0)
}

fn create_adc16_white() -> Vec<i64> {
    let mut random = Random(3);
    (0..LEN).map(|_| random.next().cast_signed()).collect()
}

fn create_adc24_s6() -> Vec<i64> {
    adc(&mut Random(6), 24, 4_000_000.0, 6.5)
}

fn create_adc24_s100() -> Vec<i64> {
    adc(&mut Random(7), 24, 4_000_000.0, 100.0)
}

/// One of 4 states, each held for 1,000 to 9,999 samples.
fn create_state() -> Vec<i64> {
    let mut random = Random(4);
    let mut state = 0;
    let mut left = 0;
    (0..LEN)
        .map(|_| {
            if left == 0 {
                state = (state + 1 + random.next().cast_signed().rem_euclid(3)) % 4;
                left = 1000 + random.next() % 9000;
            }
            left -= 1;
            state
        })
        .collect()
}

/// A timestamp every 1 ms.
fn create_ts_fixed() -> Vec<i64> {
    (0..).take(LEN).map(|i| T0 + i * 1_000_000).collect()
}

/// A timestamp every 10 ms, late by a half-normal delay with a scale of 200 µs.
fn create_ts_soft() -> Vec<i64> {
    let mut random = Random(5);
    (0..)
        .take(LEN)
        .map(|i| T0 + i * 10_000_000 + round(200_000.0 * random.normal().abs()))
        .collect()
}

/// A walk that wraps at the sample width, with uniform steps below `2^BITS`, which
/// delta packs at `BITS` bits.
fn create_walk<const BITS: u32>() -> Vec<i64> {
    let mut random = Random(10 + u64::from(BITS));
    (0..LEN)
        .scan(0_i64, |value, _| {
            *value = value.wrapping_add((random.next() >> (64 - BITS)).cast_signed());
            Some(*value)
        })
        .collect()
}

/// Uniform values below `2^BITS`, which FFOR packs at `BITS` bits.
fn create_uniform<const BITS: u32>() -> Vec<i64> {
    let mut random = Random(u64::from(BITS));
    (0..LEN)
        .map(|_| (random.next() >> (64 - BITS)).cast_signed())
        .collect()
}

#[divan::bench(args = cases(), sample_count = 1000)]
fn encode(bencher: Bencher<'_, '_>, case: Case) {
    let values = case.shape.values(case.len);
    let mut out = vec![0; max_len(case.shape.scalar, case.len)];
    let mut encoder = Encoder::new(case.shape.scalar);
    bencher.counter(ItemsCount::new(case.len)).bench_local(|| {
        encoder.encode(divan::black_box(&values), divan::black_box(&mut out))
    });
}

#[divan::bench(args = cases(), sample_count = 1000)]
fn validate(bencher: Bencher<'_, '_>, case: Case) {
    let bytes = case.shape.encoded(case.len);
    let scalar = case.shape.scalar;
    bencher.counter(ItemsCount::new(case.len)).bench_local(|| {
        codec::validate(scalar, divan::black_box(case.len), divan::black_box(&bytes))
    });
}

#[divan::bench(args = cases(), sample_count = 1000)]
fn decode(bencher: Bencher<'_, '_>, case: Case) {
    let bytes = case.shape.encoded(case.len);
    let scalar = case.shape.scalar;
    let mut out = vec![0; case.len * scalar.width()];
    bencher.counter(ItemsCount::new(case.len)).bench_local(|| {
        let len = divan::black_box(case.len);
        codec::decode(
            scalar,
            len,
            divan::black_box(&bytes),
            divan::black_box(&mut out),
        )
    });
}
