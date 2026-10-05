//! The time to encode, validate, and decode series shaped like sensor data: ADC counts,
//! enum states, and timestamps. Each series holds 256 full vectors.

use std::f64::consts::TAU;
use std::fmt;

use codec::{Encoder, VECTOR_LEN, max_len};
use divan::Bencher;
use divan::counter::ItemsCount;
use types::sample::Scalar;

fn main() {
    divan::main();
}

/// Samples in each series.
const LEN: usize = 256 * VECTOR_LEN;

/// The first timestamp, in nanoseconds: September 2026.
const T0: i64 = 1_790_000_000_000_000_000;

/// The shape of a series.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// 16-bit counts of a slow sine with 1 count of noise.
    Adc16S1,
    /// 16-bit counts of a slow sine with 256 counts of noise.
    Adc16S256,
    /// Uniform 16-bit noise.
    Adc16White,
    /// One of 4 states, each held for 1,000 to 9,999 samples.
    U8State,
    /// A timestamp every 1 ms.
    TsFixed,
    /// A timestamp every 10 ms, late by a half-normal delay with a scale of 200 µs.
    TsSoft,
}

impl Shape {
    const ALL: [Self; 6] = [
        Self::Adc16S1,
        Self::Adc16S256,
        Self::Adc16White,
        Self::U8State,
        Self::TsFixed,
        Self::TsSoft,
    ];

    fn scalar(self) -> Scalar {
        match self {
            Self::Adc16S1 | Self::Adc16S256 | Self::Adc16White => Scalar::I16,
            Self::U8State => Scalar::U8,
            Self::TsFixed | Self::TsSoft => Scalar::Stamp,
        }
    }

    /// The little-endian samples, the same on each call.
    fn values(self) -> Vec<u8> {
        match self {
            Self::Adc16S1 => adc16(&mut Random(1), 1.0),
            Self::Adc16S256 => adc16(&mut Random(2), 256.0),
            Self::Adc16White => {
                let mut random = Random(3);
                (0..LEN)
                    .flat_map(|_| {
                        let [low, high, ..] = random.next().to_le_bytes();
                        [low, high]
                    })
                    .collect()
            }
            Self::U8State => {
                let mut random = Random(4);
                let mut state = 0;
                let mut left = 0;
                (0..LEN)
                    .map(|_| {
                        if left == 0 {
                            state = random.next().to_le_bytes()[0] % 4;
                            left = 1000 + random.next() % 9000;
                        }
                        left -= 1;
                        state
                    })
                    .collect()
            }
            Self::TsFixed => stamps(|i| T0 + i * 1_000_000),
            Self::TsSoft => {
                let mut random = Random(5);
                stamps(|i| {
                    T0 + i * 10_000_000 + round(200_000.0 * random.normal().abs())
                })
            }
        }
    }

    fn encoded(self) -> Vec<u8> {
        let mut out = vec![0; max_len(self.scalar(), LEN)];
        let len = Encoder::new(self.scalar()).encode(&self.values(), &mut out);
        out.truncate(len);
        out
    }
}

impl fmt::Display for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Adc16S1 => "adc16.s1",
            Self::Adc16S256 => "adc16.s256",
            Self::Adc16White => "adc16.white",
            Self::U8State => "u8.state",
            Self::TsFixed => "ts.fixed",
            Self::TsSoft => "ts.soft",
        })
    }
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

/// 16-bit counts of a sine with an amplitude of 20,000 counts and a period of 65,536
/// samples, plus normal noise of `noise` counts.
fn adc16(random: &mut Random, noise: f64) -> Vec<u8> {
    (0..LEN)
        .flat_map(|i| {
            let x =
                20_000.0 * (TAU * float(i) / 65_536.0).sin() + noise * random.normal();
            let count = round(x).clamp(i16::MIN.into(), i16::MAX.into());
            i16::try_from(count)
                .expect("invariant: clamped")
                .to_le_bytes()
        })
        .collect()
}

/// The little-endian timestamps `stamp(0)` to `stamp(LEN - 1)`.
fn stamps(mut stamp: impl FnMut(i64) -> i64) -> Vec<u8> {
    let len = i64::try_from(LEN).expect("invariant: `LEN` fits an `i64`");
    (0..len).flat_map(|i| stamp(i).to_le_bytes()).collect()
}

fn float(index: usize) -> f64 {
    f64::from(u32::try_from(index).expect("invariant: `LEN` fits a `u32`"))
}

#[expect(clippy::cast_possible_truncation, reason = "the values fit an `i64`")]
fn round(x: f64) -> i64 {
    x.round() as i64
}

#[divan::bench(args = Shape::ALL)]
fn encode(bencher: Bencher<'_, '_>, shape: Shape) {
    let values = shape.values();
    let mut out = vec![0; max_len(shape.scalar(), LEN)];
    let mut encoder = Encoder::new(shape.scalar());
    bencher
        .counter(ItemsCount::new(LEN))
        .bench_local(|| encoder.encode(divan::black_box(&values), &mut out));
}

#[divan::bench(args = Shape::ALL)]
fn validate(bencher: Bencher<'_, '_>, shape: Shape) {
    let bytes = shape.encoded();
    bencher
        .counter(ItemsCount::new(LEN))
        .bench_local(|| codec::validate(shape.scalar(), LEN, divan::black_box(&bytes)));
}

#[divan::bench(args = Shape::ALL)]
fn decode(bencher: Bencher<'_, '_>, shape: Shape) {
    let bytes = shape.encoded();
    let mut out = vec![0; LEN * shape.scalar().width()];
    bencher.counter(ItemsCount::new(LEN)).bench_local(|| {
        codec::decode(shape.scalar(), LEN, divan::black_box(&bytes), &mut out)
    });
}
