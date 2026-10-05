//! Compresses and checks one series: per-vector selection, codecs, header validation,
//! format version.
//!
//! A series is the little-endian samples of one [`Scalar`]. It encodes as vectors of
//! up to [`VECTOR_LEN`] samples, each with its own codec tag, so a series decodes by
//! itself. The sample count is not in the bytes: it travels with the series.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

mod bits;
mod int;
mod vector;
mod word;

use std::{fmt, iter, mem};

use types::sample::Scalar;

use crate::vector::Vector;

/// The format version of encoded vectors. A series does not carry it: each wire version
/// fixes one codec version, and disk footers record it.
pub const VERSION: u16 = 1;

/// Samples in a full vector. The last vector of a series may hold fewer.
pub const VECTOR_LEN: usize = 1024;

/// The most bytes [`Encoder::encode`] writes for `count` samples of `scalar`.
///
/// # Panics
///
/// Panics when the bound is more than `usize::MAX`.
#[must_use]
pub fn max_len(scalar: Scalar, count: usize) -> usize {
    Layout::of(scalar).max_len(count)
}

/// Encodes the series of one channel.
#[derive(Debug)]
pub struct Encoder {
    layout: Layout,
}

impl Encoder {
    /// Makes an encoder for samples of `scalar`.
    #[must_use]
    pub fn new(scalar: Scalar) -> Self {
        Self {
            layout: Layout::of(scalar),
        }
    }

    /// Encodes the little-endian samples in `values` into the front of `out` and
    /// returns the bytes written.
    ///
    /// # Panics
    ///
    /// Panics when `values` does not hold whole samples, or when `out` is shorter
    /// than [`max_len`].
    #[must_use]
    pub fn encode(&mut self, values: &[u8], out: &mut [u8]) -> usize {
        let width = self.layout.width();
        let count = values.len().div_euclid(width);
        assert!(
            values.len().is_multiple_of(width),
            "values hold {} bytes, which is not whole {width}-byte samples",
            values.len()
        );
        let max = self.layout.max_len(count);
        assert!(
            out.len() >= max,
            "out holds {} bytes, fewer than the {max} that {count} samples may need",
            out.len()
        );
        let mut rest = out;
        let mut written = 0_usize;
        for chunk in values.chunks(VECTOR_LEN.strict_mul(width)) {
            let len = self.layout.encode(chunk, rest);
            rest = mem::take(&mut rest).split_at_mut(len).1;
            written = written.strict_add(len);
        }
        written
    }
}

/// Checks the headers of `bytes`, an encoded series of `count` samples of `scalar`,
/// without decoding the samples.
///
/// # Errors
///
/// Returns the error of the first vector whose header or length is not valid, or
/// [`Error::Trailing`].
pub fn validate(scalar: Scalar, count: usize, bytes: &[u8]) -> Result<(), Error> {
    let layout = Layout::of(scalar);
    let mut rest = bytes;
    for (index, count) in counts(count).enumerate() {
        rest = vector::read(rest, layout, count, index)?.1;
    }
    end(rest)
}

/// Decodes `bytes`, an encoded series of `count` samples of `scalar`, into `out`. It
/// checks what [`validate`] checks.
///
/// # Errors
///
/// Returns the errors of [`validate`]. The contents of `out` are then unspecified.
///
/// # Panics
///
/// Panics when `out` does not hold exactly `count` samples.
pub fn decode(
    scalar: Scalar,
    count: usize,
    bytes: &[u8],
    out: &mut [u8],
) -> Result<(), Error> {
    let layout = Layout::of(scalar);
    let width = layout.width();
    assert_eq!(
        Some(out.len()),
        count.checked_mul(width),
        "out holds {} bytes, not {count} samples of {width} bytes",
        out.len()
    );
    let mut rest = bytes;
    let outs = out.chunks_mut(VECTOR_LEN.strict_mul(width));
    for ((index, count), out) in counts(count).enumerate().zip(outs) {
        let (vector, after) = vector::read(rest, layout, count, index)?;
        layout.decode(&vector, out);
        rest = after;
    }
    end(rest)
}

/// The sample count of each vector in a series of `count` samples.
fn counts(count: usize) -> impl Iterator<Item = usize> {
    iter::successors(Some(count), |left| left.checked_sub(VECTOR_LEN))
        .take_while(|left| *left > 0)
        .map(|left| left.min(VECTOR_LEN))
}

/// Checks that no bytes are left after the last vector.
fn end(rest: &[u8]) -> Result<(), Error> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(Error::Trailing { extra: rest.len() })
    }
}

/// How the codecs see a sample type.
#[derive(Clone, Copy, Debug)]
enum Layout {
    /// Integers of 8, 16, 32, or 64 bits. Every codec applies.
    Int8 {
        signed: bool,
    },
    Int16 {
        signed: bool,
    },
    Int32 {
        signed: bool,
    },
    Int64 {
        signed: bool,
    },
    /// Other samples of `width` bytes, stored raw.
    Raw {
        width: usize,
    },
}

impl Layout {
    fn of(scalar: Scalar) -> Self {
        match scalar {
            Scalar::I8 => Self::Int8 { signed: true },
            Scalar::U8 => Self::Int8 { signed: false },
            Scalar::I16 => Self::Int16 { signed: true },
            Scalar::U16 => Self::Int16 { signed: false },
            Scalar::I32 => Self::Int32 { signed: true },
            Scalar::U32 => Self::Int32 { signed: false },
            Scalar::I64 | Scalar::Stamp | Scalar::Span => Self::Int64 { signed: true },
            Scalar::U64 => Self::Int64 { signed: false },
            Scalar::Bool | Scalar::F32 | Scalar::F64 | Scalar::Uuid => Self::Raw {
                width: scalar.width(),
            },
        }
    }

    fn width(self) -> usize {
        match self {
            Self::Int8 { .. } => 1,
            Self::Int16 { .. } => 2,
            Self::Int32 { .. } => 4,
            Self::Int64 { .. } => 8,
            Self::Raw { width } => width,
        }
    }

    /// The raw size of `count` samples plus one raw header per vector. Selection never
    /// picks a codec larger than raw, so it bounds every encoding.
    fn max_len(self, count: usize) -> usize {
        let width = self.width();
        let header = vector::header_len(0, width);
        count
            .checked_mul(width)
            .and_then(|samples| {
                let headers = count.div_ceil(VECTOR_LEN).checked_mul(header)?;
                samples.checked_add(headers)
            })
            .unwrap_or_else(|| {
                panic!(
                    "the encoded size of {count} {width}-byte samples is more than \
                     usize::MAX"
                )
            })
    }

    /// Encodes one vector into the front of `out` and returns its length.
    fn encode(self, chunk: &[u8], out: &mut [u8]) -> usize {
        match self {
            Self::Int8 { signed } => {
                vector::write::<1>(chunk, int::plan::<1>(chunk, signed), out)
            }
            Self::Int16 { signed } => {
                vector::write::<2>(chunk, int::plan::<2>(chunk, signed), out)
            }
            Self::Int32 { signed } => {
                vector::write::<4>(chunk, int::plan::<4>(chunk, signed), out)
            }
            Self::Int64 { signed } => {
                vector::write::<8>(chunk, int::plan::<8>(chunk, signed), out)
            }
            Self::Raw { width } => vector::write_raw(chunk, width, out),
        }
    }

    /// Writes the samples of one vector into `out`, which holds exactly them.
    fn decode(self, vector: &Vector<'_>, out: &mut [u8]) {
        match self {
            Self::Int8 { .. } => vector.decode::<1>(out),
            Self::Int16 { .. } => vector.decode::<2>(out),
            Self::Int32 { .. } => vector.decode::<4>(out),
            Self::Int64 { .. } => vector.decode::<8>(out),
            Self::Raw { .. } => vector.copy(out),
        }
    }
}

/// An encoded series that is not valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The bytes end inside a vector.
    Truncated {
        /// The index of the vector in the series.
        vector: usize,
        /// The bytes the vector needs. When the bytes end before the tag, the bit
        /// width, or the RLE run count, this is a lower bound.
        needed: usize,
        /// The bytes left from the start of the vector.
        available: usize,
    },
    /// A vector's codec tag is unknown, or the sample type does not use it.
    Tag {
        /// The index of the vector in the series.
        vector: usize,
        /// The tag.
        tag: u8,
    },
    /// A vector's bit width is more than its codec allows.
    Width {
        /// The index of the vector in the series.
        vector: usize,
        /// The bit width.
        bits: u8,
        /// The largest bit width the codec allows.
        max: u8,
    },
    /// The run lengths of an RLE vector do not add up to its sample count.
    Runs {
        /// The index of the vector in the series.
        vector: usize,
        /// The sum of the run lengths.
        total: usize,
        /// The sample count of the vector.
        count: usize,
    },
    /// Bytes are left after the last vector.
    Trailing {
        /// The number of bytes left.
        extra: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated {
                vector,
                needed,
                available,
            } => write!(
                f,
                "vector {vector} needs {needed} bytes, but {available} are left"
            ),
            Self::Tag { vector, tag } => write!(
                f,
                "vector {vector} has tag {tag}, which this sample type does not use"
            ),
            Self::Width { vector, bits, max } => write!(
                f,
                "vector {vector} has bit width {bits}, more than its codec's {max}"
            ),
            Self::Runs {
                vector,
                total,
                count,
            } => write!(
                f,
                "vector {vector} has runs of {total} samples in total, not {count}"
            ),
            Self::Trailing { extra } => {
                write!(f, "bytes after the last vector: {extra}")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
#[expect(
    clippy::arithmetic_side_effects,
    reason = "tests compute expected values from small inputs"
)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const INTS: [Scalar; 10] = [
        Scalar::I8,
        Scalar::I16,
        Scalar::I32,
        Scalar::I64,
        Scalar::U8,
        Scalar::U16,
        Scalar::U32,
        Scalar::U64,
        Scalar::Stamp,
        Scalar::Span,
    ];
    const OTHERS: [Scalar; 4] = [Scalar::Bool, Scalar::F32, Scalar::F64, Scalar::Uuid];

    /// Encodes `values` and checks that the bytes do not depend on what `out` held.
    fn encode(scalar: Scalar, values: &[u8]) -> Vec<u8> {
        let count = values.len() / scalar.width();
        let [zeros, ones] = [0x00, 0xff].map(|fill| {
            let mut out = vec![fill; max_len(scalar, count)];
            let len = Encoder::new(scalar).encode(values, &mut out);
            out.truncate(len);
            out
        });
        assert_eq!(zeros, ones, "the encoding of {scalar:?} reads stale bytes");
        zeros
    }

    /// The little-endian bytes of `values` as samples of `width` bytes.
    fn bytes(width: usize, values: impl IntoIterator<Item = i128>) -> Vec<u8> {
        values
            .into_iter()
            .flat_map(|value| value.to_le_bytes().into_iter().take(width))
            .collect()
    }

    mod encode {
        use super::*;

        fn check(
            scalar: Scalar,
            values: impl IntoIterator<Item = i128>,
            expected: &[u8],
        ) {
            let values = bytes(scalar.width(), values);
            assert_eq!(encode(scalar, &values), expected, "{scalar:?} {values:?}");
        }

        #[test]
        fn writes_each_codec_byte_for_byte() {
            check(Scalar::U8, [5, 5, 5], &[1, 0, 5]);
            check(Scalar::I16, [-1, 0, 1, 2], &[1, 2, 0xff, 0xff, 0xe4, 0]);
            check(
                Scalar::U32,
                (0..8).map(|n| 1_000 + 10 * n),
                &[2, 0, 0xe8, 0x03, 0, 0, 0x0a, 0, 0, 0, 0, 0],
            );
            check(
                Scalar::U8,
                [[1; 100], [2; 100]].concat().into_iter().map(i128::from),
                &[3, 0, 2, 0, 1, 2, 100, 0, 100, 0],
            );
            check(Scalar::I8, [3, -4], &[0, 0, 3, 0xfc]);
            check(Scalar::F32, [1], &[0, 0, 0, 0, 1, 0, 0, 0]);
        }

        #[test]
        fn pads_headers_and_bodies_to_the_sample_width() {
            check(
                Scalar::I64,
                (0..4).map(|n| i128::from(i64::MIN) + n),
                &[
                    1, 2, 0, 0, 0, 0, 0, 0, 0, 0x80, 0, 0, 0, 0, 0, 0, //
                    0xe4, 0, 0, 0, 0, 0, 0, 0,
                ],
            );
            check(
                Scalar::U64,
                [[7; 100], [1 << 40; 100]].concat(),
                &[
                    3, 0, 2, 0, 0, 0, 0, 0, //
                    7, 0, 0, 0, 0, 0, 0, 0, //
                    0, 0, 0, 0, 0, 1, 0, 0, //
                    100, 0, 100, 0, 0, 0, 0, 0,
                ],
            );
        }

        #[test]
        fn orders_samples_by_signedness() {
            let values = bytes(2, (0..1_024).map(|n| 0x7ff0 + (n * 7) % 32));
            assert_eq!(encode(Scalar::U16, &values)[..4], [1, 5, 0xf0, 0x7f]);
            let values = bytes(8, (0..1_024).map(|n| if n % 2 == 0 { -1 } else { 1 }));
            for scalar in [Scalar::I64, Scalar::Stamp, Scalar::Span] {
                assert_eq!(encode(scalar, &values)[..2], [1, 2], "{scalar:?}");
            }
        }

        #[test]
        fn keeps_raw_unless_a_codec_saves_an_eighth() {
            check(
                Scalar::U8,
                [0, 31, 0, 31, 0, 31],
                &[1, 5, 0, 0xe0, 0x83, 0x0f, 0x3e],
            );
            check(
                Scalar::U8,
                [0, 63, 0, 63, 0, 63],
                &[0, 0, 0, 63, 0, 63, 0, 63],
            );
        }

        #[test]
        fn splits_series_into_vectors_of_1024() {
            // One sample never saves an eighth, so the last vector is raw.
            let values = bytes(2, (0..2_049).map(|n| n % 1_024));
            let vector = [2, 0, 0, 0, 1, 0];
            let mut expected = [vector, vector].concat();
            expected.extend([0, 0, 0, 0]);
            assert_eq!(encode(Scalar::U16, &values), expected);
        }

        #[test]
        fn writes_nothing_for_no_samples() {
            for scalar in INTS.into_iter().chain(OTHERS) {
                assert_eq!(encode(scalar, &[]), [], "{scalar:?}");
            }
        }

        #[test]
        #[should_panic(
            expected = "values hold 3 bytes, which is not whole 2-byte samples"
        )]
        fn panics_on_a_partial_sample() {
            std::hint::black_box(
                Encoder::new(Scalar::U16).encode(&[1, 2, 3], &mut [0; 16]),
            );
        }

        #[test]
        #[should_panic(
            expected = "out holds 4 bytes, fewer than the 5 that 3 samples may need"
        )]
        fn panics_when_out_may_be_short() {
            std::hint::black_box(
                Encoder::new(Scalar::U8).encode(&[5, 5, 5], &mut [0; 4]),
            );
        }
    }

    mod max_len {
        use super::*;

        #[test]
        fn is_raw_plus_one_raw_header_per_vector() {
            for (scalar, count, len) in [
                (Scalar::U8, 0, 0),
                (Scalar::U8, 1, 3),
                (Scalar::U8, 1_024, 1_026),
                (Scalar::U8, 1_025, 1_029),
                (Scalar::U32, 3, 16),
                (Scalar::I64, 2_048, 16_400),
                (Scalar::Uuid, 1, 32),
            ] {
                assert_eq!(max_len(scalar, count), len, "{scalar:?} {count}");
            }
        }

        #[test]
        #[should_panic(
            expected = "the encoded size of 18446744073709551615 2-byte samples"
        )]
        fn panics_past_usize() {
            std::hint::black_box(max_len(Scalar::U16, usize::MAX));
        }
    }

    mod round_trip {
        use proptest::sample::select;

        use super::*;

        /// Samples that the codec with `tag` encodes smallest, built from `words`.
        fn samples(scalar: Scalar, tag: u8, len: usize, words: &[u64]) -> Vec<u8> {
            let bits = u32::from(word::bits(scalar.width()));
            let low = word::mask(scalar.width()) >> 2;
            let base = words[0] & low;
            let values = (0..len).scan(base, |sum, n| {
                let word = words[n + 1];
                Some(match tag {
                    vector::RAW => word,
                    vector::FFOR => base + (word >> (64 - bits / 2)),
                    vector::DELTA => {
                        *sum = sum.wrapping_add(word >> (64 - bits / 4));
                        *sum
                    }
                    _ => words[1 + n / 128],
                })
            });
            values
                .flat_map(|value| value.to_le_bytes().into_iter().take(scalar.width()))
                .collect()
        }

        fn check(scalar: Scalar, values: &[u8]) -> Vec<u8> {
            let count = values.len() / scalar.width();
            let encoded = encode(scalar, values);
            assert_eq!(validate(scalar, count, &encoded), Ok(()));
            let mut out = vec![0; values.len()];
            assert_eq!(decode(scalar, count, &encoded, &mut out), Ok(()));
            assert_eq!(out, values, "{scalar:?}");
            encoded
        }

        proptest! {
            #[test]
            fn every_integer_codec(
                scalar in select(&INTS),
                tag in 0..4_u8,
                len in select(&[0, 1, 1_023, 1_024, 1_025]),
                words in proptest::collection::vec(any::<u64>(), 1_026),
            ) {
                let encoded = check(scalar, &samples(scalar, tag, len, &words));
                if len >= 1_023 {
                    prop_assert_eq!(encoded[0], tag, "{:?} at {}", scalar, len);
                }
            }

            #[test]
            fn any_integer_series(
                scalar in select(&INTS),
                values in proptest::collection::vec(any::<u8>(), 0..2_100 * 8),
            ) {
                let width = scalar.width();
                check(scalar, &values[..values.len() / width * width]);
            }

            #[test]
            fn other_samples_as_raw(
                scalar in select(&OTHERS),
                len in select(&[1, 1_023, 1_024, 1_025]),
                values in proptest::collection::vec(any::<u8>(), 1_025 * 16),
            ) {
                let encoded = check(scalar, &values[..len * scalar.width()]);
                prop_assert_eq!(encoded[0], vector::RAW);
            }
        }
    }

    mod validate {
        use proptest::sample::select;

        use super::*;

        fn check(
            scalar: Scalar,
            count: usize,
            bytes: &[u8],
            expected: &Result<(), Error>,
        ) {
            assert_eq!(
                &validate(scalar, count, bytes),
                expected,
                "{scalar:?} {bytes:?}"
            );
            let mut out = vec![0; count * scalar.width()];
            assert_eq!(
                &decode(scalar, count, bytes, &mut out),
                expected,
                "{scalar:?}"
            );
        }

        fn accepts(scalar: Scalar, count: usize, bytes: &[u8], samples: &[u8]) {
            assert_eq!(
                validate(scalar, count, bytes),
                Ok(()),
                "{scalar:?} {bytes:?}"
            );
            let mut out = vec![0; samples.len()];
            assert_eq!(decode(scalar, count, bytes, &mut out), Ok(()), "{scalar:?}");
            assert_eq!(out, samples, "{scalar:?} {bytes:?}");
        }

        fn truncated(
            vector: usize,
            needed: usize,
            available: usize,
        ) -> Result<(), Error> {
            Err(Error::Truncated {
                vector,
                needed,
                available,
            })
        }

        fn width(bits: u8, max: u8) -> Result<(), Error> {
            Err(Error::Width {
                vector: 0,
                bits,
                max,
            })
        }

        #[test]
        fn accepts_an_empty_series() {
            accepts(Scalar::U8, 0, &[], &[]);
        }

        #[test]
        fn accepts_the_largest_bit_width() {
            accepts(Scalar::U8, 1, &[1, 8, 0, 0xff], &[0xff]);
            let mut delta = [0; 32];
            delta[..2].copy_from_slice(&[2, 64]);
            delta[24..].fill(0xff);
            accepts(Scalar::U64, 2, &delta, &[[0; 8], [0xff; 8]].concat());
        }

        #[test]
        fn accepts_runs_of_zero_and_ignores_padding() {
            accepts(Scalar::U8, 3, &[3, 0, 2, 0, 7, 8, 0, 0, 3, 0], &[8, 8, 8]);
            accepts(
                Scalar::U32,
                3,
                &[3, 0, 1, 0, 7, 0, 0, 0, 3, 0, 0xff, 0xff],
                &[7, 0, 0, 0].repeat(3),
            );
        }

        #[test]
        fn rejects_truncated_vectors() {
            check(Scalar::U8, 3, &[], &truncated(0, 2, 0));
            check(Scalar::U8, 3, &[1], &truncated(0, 2, 1));
            check(Scalar::U8, 3, &[0, 0, 1, 2], &truncated(0, 5, 4));
            check(Scalar::U16, 4, &[1, 2, 0xff], &truncated(0, 6, 3));
            check(Scalar::U16, 4, &[1, 2, 0xff, 0xff], &truncated(0, 6, 4));
            check(
                Scalar::U32,
                8,
                &[2, 0, 0xe8, 3, 0, 0, 10, 0, 0, 0, 0],
                &truncated(0, 12, 11),
            );
            check(Scalar::U8, 3, &[3, 0, 1], &truncated(0, 4, 3));
            check(Scalar::U64, 3, &[3, 0, 1, 0, 0, 0, 0], &truncated(0, 24, 7));
            check(Scalar::U8, 3, &[3, 0, 1, 0, 7], &truncated(0, 7, 5));
            check(Scalar::Uuid, 1, &[0; 31], &truncated(0, 32, 31));
        }

        #[test]
        fn names_the_vector_that_is_truncated() {
            let values = vec![0; 1_025];
            let encoded = encode(Scalar::U8, &values);
            check(Scalar::U8, 1_025, &encoded[..3], &truncated(1, 2, 0));
            check(Scalar::U8, 2_049, &encoded, &truncated(1, 1_026, 3));
        }

        #[test]
        fn rejects_unknown_tags() {
            for tag in 4..=u8::MAX {
                check(
                    Scalar::U8,
                    1,
                    &[tag, 0, 0],
                    &Err(Error::Tag { vector: 0, tag }),
                );
            }
            for (scalar, tag) in
                OTHERS.into_iter().flat_map(|s| (1..4).map(move |t| (s, t)))
            {
                let mut bytes = [0; 32];
                bytes[0] = tag;
                check(scalar, 1, &bytes, &Err(Error::Tag { vector: 0, tag }));
            }
        }

        #[test]
        fn rejects_widths_the_codec_does_not_allow() {
            check(Scalar::U8, 1, &[1, 9, 0], &width(9, 8));
            check(Scalar::I16, 1, &[2, 17, 0, 0, 0, 0], &width(17, 16));
            check(Scalar::U64, 1, &[1, 65], &width(65, 64));
            check(Scalar::U8, 1, &[0, 1, 5], &width(1, 0));
            check(Scalar::F32, 1, &[0, 32, 0, 0, 0, 0, 0, 0], &width(32, 0));
            check(Scalar::U8, 1, &[3, 1, 1, 0, 5, 1, 0], &width(1, 0));
        }

        #[test]
        fn rejects_runs_that_do_not_add_up_to_the_count() {
            let runs = |total| {
                Err(Error::Runs {
                    vector: 0,
                    total,
                    count: 3,
                })
            };
            check(Scalar::U8, 3, &[3, 0, 1, 0, 7, 2, 0], &runs(2));
            check(Scalar::U8, 3, &[3, 0, 1, 0, 7, 4, 0], &runs(4));
            check(Scalar::U8, 3, &[3, 0, 0, 0], &runs(0));
            check(
                Scalar::U8,
                3,
                &[3, 0, 2, 0, 7, 8, 0xff, 0xff, 4, 0],
                &runs(65_539),
            );
        }

        #[test]
        fn rejects_bytes_after_the_last_vector() {
            check(
                Scalar::U8,
                1,
                &[1, 0, 5, 0],
                &Err(Error::Trailing { extra: 1 }),
            );
            check(Scalar::U8, 0, &[0], &Err(Error::Trailing { extra: 1 }));
        }

        #[test]
        fn describes_each_error() {
            for (error, text) in [
                (
                    Error::Truncated {
                        vector: 1,
                        needed: 6,
                        available: 4,
                    },
                    "vector 1 needs 6 bytes, but 4 are left",
                ),
                (
                    Error::Tag { vector: 0, tag: 9 },
                    "vector 0 has tag 9, which this sample type does not use",
                ),
                (
                    Error::Width {
                        vector: 2,
                        bits: 9,
                        max: 8,
                    },
                    "vector 2 has bit width 9, more than its codec's 8",
                ),
                (
                    Error::Runs {
                        vector: 0,
                        total: 2,
                        count: 3,
                    },
                    "vector 0 has runs of 2 samples in total, not 3",
                ),
                (
                    Error::Trailing { extra: 1 },
                    "bytes after the last vector: 1",
                ),
            ] {
                assert_eq!(error.to_string(), text);
            }
        }

        proptest! {
            #[test]
            fn never_panics_on_random_bytes(
                scalar in prop_oneof![select(&INTS), select(&OTHERS)],
                count in 0..2_100_usize,
                bytes in proptest::collection::vec(any::<u8>(), 0..64),
            ) {
                let mut out = vec![0; count * scalar.width()];
                prop_assert_eq!(
                    decode(scalar, count, &bytes, &mut out),
                    validate(scalar, count, &bytes)
                );
            }

            #[test]
            fn never_panics_on_changed_encodings(
                scalar in select(&INTS),
                values in proptest::collection::vec(0..4_u8, 1..2_100 * 8),
                at in any::<prop::sample::Index>(),
                byte in any::<u8>(),
            ) {
                let width = scalar.width();
                let values = &values[..values.len() / width * width];
                let count = values.len() / width;
                let mut encoded = encode(scalar, values);
                let index = at.index(encoded.len().max(1));
                if let Some(target) = encoded.get_mut(index) {
                    *target = byte;
                }
                let mut out = vec![0; values.len()];
                prop_assert_eq!(
                    decode(scalar, count, &encoded, &mut out),
                    validate(scalar, count, &encoded)
                );
            }
        }
    }

    mod decode {
        use super::*;

        #[test]
        #[should_panic(expected = "out holds 3 bytes, not 2 samples of 2 bytes")]
        fn panics_when_out_does_not_hold_count_samples() {
            let _result = decode(Scalar::U16, 2, &[], &mut [0; 3]);
        }
    }
}
