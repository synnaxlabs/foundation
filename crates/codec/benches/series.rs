//! The time to encode, validate, and decode series shaped like sensor data, and
//! uniform series that select each class of bit width. Before it times anything, it
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
    /// Creates `LEN` little-endian samples.
    create: fn() -> Vec<u8>,
    /// The least ratio of raw to encoded bytes for `LEN` samples.
    ratio: f64,
    /// The series lengths to time.
    lens: &'static [usize],
}

const SHAPES: [Shape; 22] = [
    Shape {
        name: "adc16.s1",
        scalar: Scalar::I16,
        create: create_adc16_s1,
        ratio: 3.933,
        lens: EVERY,
    },
    Shape {
        name: "adc16.s256",
        scalar: Scalar::I16,
        create: create_adc16_s256,
        ratio: 1.352,
        lens: FULL,
    },
    Shape {
        name: "adc16.white",
        scalar: Scalar::I16,
        create: create_adc16_white,
        ratio: 0.994,
        lens: FULL,
    },
    Shape {
        name: "adc24.s6",
        scalar: Scalar::I32,
        create: create_adc24_s6,
        ratio: 4.615,
        lens: EVERY,
    },
    Shape {
        name: "adc24.s100",
        scalar: Scalar::I32,
        create: create_adc24_s100,
        ratio: 3.131,
        lens: FULL,
    },
    Shape {
        name: "u8.state",
        scalar: Scalar::U8,
        create: create_state::<1>,
        ratio: 229.0,
        lens: EVERY,
    },
    Shape {
        name: "u32.state",
        scalar: Scalar::U32,
        create: create_state::<4>,
        ratio: 422.0,
        lens: FULL,
    },
    Shape {
        name: "ts.fixed",
        scalar: Scalar::Stamp,
        create: create_ts_fixed,
        ratio: 339.6,
        lens: FULL,
    },
    Shape {
        name: "ts.soft",
        scalar: Scalar::Stamp,
        create: create_ts_soft,
        ratio: 3.018,
        lens: EVERY,
    },
    Shape {
        name: "u8.delta3",
        scalar: Scalar::U8,
        create: create_walk::<1, 3>,
        ratio: 2.626,
        lens: FULL,
    },
    Shape {
        name: "u8.ffor1",
        scalar: Scalar::U8,
        create: create_uniform::<1, 1>,
        ratio: 7.777,
        lens: FULL,
    },
    Shape {
        name: "u8.ffor3",
        scalar: Scalar::U8,
        create: create_uniform::<1, 3>,
        ratio: 2.632,
        lens: FULL,
    },
    Shape {
        name: "u8.ffor6",
        scalar: Scalar::U8,
        create: create_uniform::<1, 6>,
        ratio: 1.321,
        lens: FULL,
    },
    Shape {
        name: "u16.ffor1",
        scalar: Scalar::U16,
        create: create_uniform::<2, 1>,
        ratio: 15.43,
        lens: FULL,
    },
    Shape {
        name: "u16.ffor8",
        scalar: Scalar::U16,
        create: create_uniform::<2, 8>,
        ratio: 1.982,
        lens: FULL,
    },
    Shape {
        name: "u16.ffor13",
        scalar: Scalar::U16,
        create: create_uniform::<2, 13>,
        ratio: 1.221,
        lens: FULL,
    },
    Shape {
        name: "u32.ffor1",
        scalar: Scalar::U32,
        create: create_uniform::<4, 1>,
        ratio: 29.96,
        lens: FULL,
    },
    Shape {
        name: "u32.ffor16",
        scalar: Scalar::U32,
        create: create_uniform::<4, 16>,
        ratio: 1.982,
        lens: FULL,
    },
    Shape {
        name: "u32.ffor27",
        scalar: Scalar::U32,
        create: create_uniform::<4, 27>,
        ratio: 1.176,
        lens: FULL,
    },
    Shape {
        name: "u64.ffor1",
        scalar: Scalar::U64,
        create: create_uniform::<8, 1>,
        ratio: 56.6,
        lens: FULL,
    },
    Shape {
        name: "u64.ffor32",
        scalar: Scalar::U64,
        create: create_uniform::<8, 32>,
        ratio: 1.982,
        lens: FULL,
    },
    Shape {
        name: "u64.ffor55",
        scalar: Scalar::U64,
        create: create_uniform::<8, 55>,
        ratio: 1.155,
        lens: FULL,
    },
];

impl Shape {
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

    fn values(&self, len: usize) -> Vec<u8> {
        let mut values = (self.create)();
        values.truncate(len * self.scalar.width());
        values
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
        assert_eq!(
            codec::validate(self.scalar, len, &out),
            Ok(()),
            "{}",
            self.name
        );
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

/// The low `WIDTH` bytes of each value, little-endian.
fn bytes<const WIDTH: usize>(values: impl Iterator<Item = i64>) -> Vec<u8> {
    values
        .flat_map(|value| value.to_le_bytes().into_iter().take(WIDTH))
        .collect()
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

fn create_adc16_s1() -> Vec<u8> {
    bytes::<2>(adc(&mut Random(1), 16, 20_000.0, 1.0).into_iter())
}

fn create_adc16_s256() -> Vec<u8> {
    bytes::<2>(adc(&mut Random(2), 16, 20_000.0, 256.0).into_iter())
}

fn create_adc16_white() -> Vec<u8> {
    let mut random = Random(3);
    bytes::<2>((0..LEN).map(|_| random.next().cast_signed()))
}

fn create_adc24_s6() -> Vec<u8> {
    bytes::<4>(adc(&mut Random(6), 24, 4_000_000.0, 6.5).into_iter())
}

fn create_adc24_s100() -> Vec<u8> {
    bytes::<4>(adc(&mut Random(7), 24, 4_000_000.0, 100.0).into_iter())
}

/// One of 4 states, each held for 1,000 to 9,999 samples.
fn create_state<const WIDTH: usize>() -> Vec<u8> {
    let mut random = Random(4);
    let mut state = 0;
    let mut left = 0;
    let states = (0..LEN).map(|_| {
        if left == 0 {
            state = (state + 1 + random.next().cast_signed().rem_euclid(3)) % 4;
            left = 1000 + random.next() % 9000;
        }
        left -= 1;
        state
    });
    bytes::<WIDTH>(states)
}

/// A timestamp every 1 ms.
fn create_ts_fixed() -> Vec<u8> {
    bytes::<8>((0..).take(LEN).map(|i| T0 + i * 1_000_000))
}

/// A timestamp every 10 ms, late by a half-normal delay with a scale of 200 µs.
fn create_ts_soft() -> Vec<u8> {
    let mut random = Random(5);
    let stamps = (0..)
        .take(LEN)
        .map(|i| T0 + i * 10_000_000 + round(200_000.0 * random.normal().abs()));
    bytes::<8>(stamps)
}

/// A walk that wraps at the sample width, with uniform steps below `2^BITS`, which
/// delta packs at `BITS` bits.
fn create_walk<const WIDTH: usize, const BITS: u32>() -> Vec<u8> {
    let mut random = Random(10 + u64::from(BITS));
    let walk = (0..LEN).scan(0_i64, |value, _| {
        *value = value.wrapping_add((random.next() >> (64 - BITS)).cast_signed());
        Some(*value)
    });
    bytes::<WIDTH>(walk)
}

/// Uniform values below `2^BITS`, which FFOR packs at `BITS` bits.
fn create_uniform<const WIDTH: usize, const BITS: u32>() -> Vec<u8> {
    let mut random = Random(u64::from(BITS));
    bytes::<WIDTH>((0..LEN).map(|_| (random.next() >> (64 - BITS)).cast_signed()))
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
