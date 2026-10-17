//! The byte format of one vector.
//!
//! A vector is a tag byte, a bit width byte, the codec's header fields, zeros up to a
//! multiple of the sample width, then the body, padded the same way:
//!
//! | Tag | Codec | Fields | Body |
//! |-----|-------|--------|------|
//! | 0 | raw | none | the samples |
//! | 1 | FFOR | reference | `sample - reference`, packed |
//! | 2 | delta | first, base | `sample - previous - base` after the first, packed |
//! | 3 | RLE | run count (`u16`) | the run values, then the run lengths (`u16`) |
//! | 4 | ALP | reference, `e`, `f`, exception count (`u16`) | below |
//!
//! Fields are samples unless marked. Each field, sample, and `u16` is little-endian. A
//! vector holds at most 1024 samples. Sample arithmetic is modulo 2^b, where b is the
//! bit count of a sample. Delta packs one residual for each sample after the first.
//! Packed values are least significant bit first. A float vector of tag 1 to 3 holds
//! the sortable integers of its samples, in unsigned order: a float with its sign bit
//! set has each bit flipped, and any other only its sign bit (`float::sortable`).
//!
//! ALP is for `f32` and `f64` only. The exponent `e` and the factor `f` are `u8`. The
//! body is `n - reference` for each sample, packed and padded to the sample width, then
//! one value (a sample) for each exception, then one position (`u16`) for each. The
//! reference and each `n` are signed integers of the sample width. Sample `i` is `n` in
//! the sample's float type `F`, times `P[f]`, times `N[e]`, where `n` is
//! `reference + residual i`. The conversion and each product round to nearest, ties to
//! even, with no fused multiply-add. `P[i]` and `N[i]` are the values of `F` nearest to
//! `10^i` and `10^-i`, each rounded once from the exact value: for `f32`, not the `f64`
//! value rounded again. Then each exception value replaces the sample at its position.
//! `e` is at most 18 for `f64` and 10 for `f32`, `f` is at most `e`, and the positions
//! strictly increase and are under the sample count.

use std::{iter, mem};

use crate::alp::{self, Pair};
use crate::{Error, Layout, Number, VECTOR_LEN, bits, word};

pub(crate) const RAW: u8 = 0;
pub(crate) const FFOR: u8 = 1;
pub(crate) const DELTA: u8 = 2;
pub(crate) const RLE: u8 = 3;
pub(crate) const ALP: u8 = 4;

/// The codec of one vector and its header values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Raw,
    Ffor {
        reference: u64,
        bits: u8,
    },
    Delta {
        first: u64,
        base: u64,
        bits: u8,
    },
    Rle {
        runs: usize,
    },
    Alp {
        reference: u64,
        bits: u8,
        pair: Pair,
        exceptions: usize,
    },
}

impl Plan {
    /// The bytes of a vector of `count` samples of `width` bytes.
    pub(crate) fn len(self, count: usize, width: usize) -> usize {
        self.header_len(width)
            .strict_add(self.body_len(count, width))
    }

    #[inline]
    fn header_len(self, width: usize) -> usize {
        let fields = match self {
            Self::Raw => 0,
            Self::Ffor { .. } => width,
            Self::Delta { .. } => width.strict_mul(2),
            Self::Rle { .. } => 2,
            Self::Alp { .. } => width.strict_add(4),
        };
        header_len(fields, width)
    }

    fn body_len(self, count: usize, width: usize) -> usize {
        match self {
            Self::Raw => count.strict_mul(width),
            Self::Ffor { bits, .. } => bits::len(count, bits).next_multiple_of(width),
            Self::Delta { bits, .. } => {
                bits::len(count.strict_sub(1), bits).next_multiple_of(width)
            }
            Self::Rle { runs } => runs
                .strict_mul(width)
                .strict_add(runs.strict_mul(2).next_multiple_of(width)),
            Self::Alp {
                bits, exceptions, ..
            } => bits::len(count, bits)
                .next_multiple_of(width)
                .strict_add(exceptions.strict_mul(width))
                .strict_add(exceptions.strict_mul(2).next_multiple_of(width)),
        }
    }

    /// The largest bit width the codec allows for samples of `width` bytes.
    fn max_bits(self, width: usize) -> u8 {
        match self {
            Self::Raw | Self::Rle { .. } => 0,
            Self::Ffor { .. } | Self::Delta { .. } | Self::Alp { .. } => {
                word::bits(width)
            }
        }
    }
}

/// The bytes of a header with `fields` bytes of fields, for samples of `width` bytes.
#[inline]
pub(crate) fn header_len(fields: usize, width: usize) -> usize {
    fields.strict_add(2).next_multiple_of(width)
}

/// Writes one vector of `W`-byte samples with `plan` into the front of `out` and
/// returns its length. Each codec but raw and ALP writes the integers that `sortable`
/// gives.
pub(crate) fn write<const W: usize>(
    chunk: &[u8],
    plan: Plan,
    sortable: impl Fn(u64) -> u64 + Copy,
    out: &mut [u8],
) -> usize {
    let mut out = Writer { out, len: 0 };
    let count = chunk.len().div_euclid(W);
    let samples = word::samples::<W>(chunk).map(sortable);
    let mask = word::mask(W);
    match plan {
        Plan::Raw => out.raw(chunk, W),
        Plan::Ffor { reference, bits } => {
            out.header::<W>(FFOR, bits, &[reference]);
            let residuals = samples.map(|sample| sample.wrapping_sub(reference) & mask);
            out.packed::<W>(residuals, count, bits);
        }
        Plan::Delta { first, base, bits } => {
            out.header::<W>(DELTA, bits, &[first, base]);
            let residuals =
                samples
                    .clone()
                    .zip(samples.skip(1))
                    .map(|(previous, sample)| {
                        sample.wrapping_sub(previous).wrapping_sub(base) & mask
                    });
            out.packed::<W>(residuals, count.strict_sub(1), bits);
        }
        Plan::Rle { runs } => {
            out.put(&[RLE, 0]);
            out.put(&count16(runs).to_le_bytes());
            out.pad(W);
            let values = out.take(runs.strict_mul(W)).as_chunks_mut::<W>().0;
            let lengths = out.take(runs.strict_mul(2)).as_chunks_mut::<2>().0;
            for ((run, length), (value, len)) in
                self::runs(samples).zip(values.iter_mut().zip(lengths))
            {
                *value = word::store(run);
                *len = u16::try_from(length)
                    .expect("invariant: a run is at most 1024 samples")
                    .to_le_bytes();
            }
            out.pad(W);
        }
        Plan::Alp {
            reference,
            bits,
            pair,
            exceptions,
        } => out.alp::<W>(chunk, reference, bits, pair, exceptions),
    }
    out.len
}

/// `count` as a `u16` field.
fn count16(count: usize) -> u16 {
    u16::try_from(count)
        .expect("invariant: a vector holds under 2^16 runs or exceptions")
}

/// The runs of equal `samples`: each value and its length.
fn runs(samples: impl Iterator<Item = u64>) -> impl Iterator<Item = (u64, usize)> {
    let mut samples = samples.peekable();
    iter::from_fn(move || {
        let value = samples.next()?;
        let mut len = 1_usize;
        while samples.next_if_eq(&value).is_some() {
            len = len.strict_add(1);
        }
        Some((value, len))
    })
}

/// Writes one vector of `width`-byte samples raw into the front of `out` and returns
/// its length.
pub(crate) fn write_raw(chunk: &[u8], width: usize, out: &mut [u8]) -> usize {
    let mut out = Writer { out, len: 0 };
    out.raw(chunk, width);
    out.len
}

/// Writes a vector front to back.
struct Writer<'a> {
    out: &'a mut [u8],
    len: usize,
}

impl<'a> Writer<'a> {
    fn take(&mut self, len: usize) -> &'a mut [u8] {
        let (head, rest) = mem::take(&mut self.out).split_at_mut(len);
        self.out = rest;
        self.len = self.len.strict_add(len);
        head
    }

    fn put(&mut self, bytes: &[u8]) {
        self.take(bytes.len()).copy_from_slice(bytes);
    }

    fn pad(&mut self, width: usize) {
        let len = self.len.next_multiple_of(width).strict_sub(self.len);
        self.take(len).fill(0);
    }

    fn raw(&mut self, chunk: &[u8], width: usize) {
        self.put(&[RAW, 0]);
        self.pad(width);
        self.put(chunk);
    }

    fn header<const W: usize>(&mut self, tag: u8, bits: u8, fields: &[u64]) {
        self.put(&[tag, bits]);
        for field in fields {
            self.put(&word::store::<W>(*field));
        }
        self.pad(W);
    }

    /// Writes an ALP vector of the floats of `chunk`.
    fn alp<const W: usize>(
        &mut self,
        chunk: &[u8],
        reference: u64,
        bits: u8,
        pair: Pair,
        exceptions: usize,
    ) {
        self.put(&[ALP, bits]);
        self.put(&word::store::<W>(reference));
        self.put(&[pair.exponent, pair.factor]);
        self.put(&count16(exceptions).to_le_bytes());
        self.pad(W);
        let coder = alp::Coder::<W>::new(pair);
        let mask = word::mask(W);
        let mut positions = [0_u16; VECTOR_LEN];
        let mut len = 0;
        let residuals = word::samples::<W>(chunk).zip(0..).map(|(sample, index)| {
            if let Some(n) = coder.exact(sample) {
                return n.cast_unsigned().wrapping_sub(reference) & mask;
            }
            *positions
                .get_mut(len)
                .expect("invariant: a vector holds at most 1024 samples") = index;
            len = len.strict_add(1);
            0
        });
        self.packed::<W>(residuals, chunk.len().div_euclid(W), bits);
        let positions = positions.split_at(len).0;
        let samples = chunk.as_chunks::<W>().0;
        for position in positions {
            self.put(
                samples
                    .get(usize::from(*position))
                    .expect("invariant: a position is under the count"),
            );
        }
        for position in positions {
            self.put(&position.to_le_bytes());
        }
        self.pad(W);
    }

    fn packed<const W: usize>(
        &mut self,
        values: impl Iterator<Item = u64>,
        count: usize,
        bits: u8,
    ) {
        bits::pack(values, bits, self.take(bits::len(count, bits)));
        self.pad(W);
    }
}

/// One vector read from a series, with its header and length checked.
#[derive(Debug)]
pub(crate) struct Vector<'a> {
    pub(crate) plan: Plan,
    pub(crate) body: &'a [u8],
}

/// Reads vector `index` of a series from the front of `bytes`. The vector holds `count`
/// samples. Returns it and the bytes after it.
///
/// # Panics
///
/// Panics when `count` is over [`VECTOR_LEN`].
pub(crate) fn read(
    bytes: &[u8],
    layout: Layout,
    count: usize,
    index: usize,
) -> Result<(Vector<'_>, &[u8]), Error> {
    assert!(
        count <= VECTOR_LEN,
        "invariant: vector {index} holds {count} samples, over {VECTOR_LEN}"
    );
    let width = layout.width();
    let plan = header(bytes, layout, index)?;
    let len = plan.len(count, width);
    let (vector, rest) = bytes.split_at_checked(len).ok_or(Error::Truncated {
        vector: index,
        needed: len,
        available: bytes.len(),
    })?;
    let body = vector.split_at(plan.header_len(width)).1;
    if let Plan::Rle { runs } = plan {
        let lengths = body.split_at(runs.strict_mul(width)).1;
        // At most 2^16 runs of at most 2^16 samples: the sum fits in 32 bits.
        let total = lengths
            .as_chunks::<2>()
            .0
            .iter()
            .take(runs)
            .fold(0, |total, len| {
                usize::strict_add(total, u16::from_le_bytes(*len).into())
            });
        if total != count {
            return Err(Error::Runs {
                vector: index,
                total,
                count,
            });
        }
    }
    if let Plan::Alp {
        bits, exceptions, ..
    } = plan
    {
        positions(body, width, count, bits, exceptions, index)?;
    }
    Ok((Vector { plan, body }, rest))
}

/// Checks that the exception positions of ALP `body` strictly increase and are under
/// `count`. Out of line, to keep [`read`] short for the other tags.
#[inline(never)]
fn positions(
    body: &[u8],
    width: usize,
    count: usize,
    bits: u8,
    exceptions: usize,
    index: usize,
) -> Result<(), Error> {
    let packed = bits::len(count, bits).next_multiple_of(width);
    let positions = body
        .split_at(packed.strict_add(exceptions.strict_mul(width)))
        .1;
    let mut least = 0;
    for position in positions.as_chunks::<2>().0.iter().take(exceptions) {
        let position = usize::from(u16::from_le_bytes(*position));
        if position < least || position >= count {
            return Err(Error::Position {
                vector: index,
                position,
                count,
            });
        }
        least = position.strict_add(1);
    }
    Ok(())
}

/// Reads and checks the header at the front of `bytes`. The bytes may end inside the
/// header: missing field bytes read as zeros, and [`read`] then rejects the length.
fn header(bytes: &[u8], layout: Layout, index: usize) -> Result<Plan, Error> {
    let width = layout.width();
    let truncated = |needed| Error::Truncated {
        vector: index,
        needed,
        available: bytes.len(),
    };
    let [tag, bits, ..] = *bytes else {
        return Err(truncated(2));
    };
    let plan = match tag {
        RAW => Plan::Raw,
        _ if matches!(layout, Layout::Raw { .. }) => {
            return Err(Error::Tag { vector: index, tag });
        }
        FFOR => Plan::Ffor {
            reference: field(bytes, 0, width),
            bits,
        },
        DELTA => Plan::Delta {
            first: field(bytes, 0, width),
            base: field(bytes, 1, width),
            bits,
        },
        RLE => {
            let [_, _, low, high, ..] = *bytes else {
                return Err(truncated(header_len(2, width)));
            };
            Plan::Rle {
                runs: u16::from_le_bytes([low, high]).into(),
            }
        }
        ALP if matches!(
            layout,
            Layout::Bits32 {
                number: Number::Float
            } | Layout::Bits64 {
                number: Number::Float
            }
        ) =>
        {
            alp_header(bytes, width, bits, index)?
        }
        _ => return Err(Error::Tag { vector: index, tag }),
    };
    let max = plan.max_bits(width);
    if bits > max {
        return Err(Error::Width {
            vector: index,
            bits,
            max,
        });
    }
    Ok(plan)
}

/// Reads and checks the fields of an ALP header at the front of `bytes`, for samples
/// of `width` bytes, with bit width `bits`. Out of line, as [`positions`].
#[inline(never)]
fn alp_header(
    bytes: &[u8],
    width: usize,
    bits: u8,
    index: usize,
) -> Result<Plan, Error> {
    let len = header_len(width.strict_add(4), width);
    let fields = bytes.get(width.strict_add(2)..len);
    let Some(&[exponent, factor, low, high]) = fields.and_then(<[u8]>::first_chunk)
    else {
        return Err(Error::Truncated {
            vector: index,
            needed: len,
            available: bytes.len(),
        });
    };
    let max = alp::max_exponent(width);
    if exponent > max || factor > exponent {
        return Err(Error::Exponent {
            vector: index,
            exponent,
            factor,
            max,
        });
    }
    Ok(Plan::Alp {
        reference: field(bytes, 0, width),
        bits,
        pair: Pair { exponent, factor },
        exceptions: u16::from_le_bytes([low, high]).into(),
    })
}

/// Reads header field `index`: `width` little-endian bytes after the tag and the bit
/// width. Bytes past the end read as zeros.
#[inline]
fn field(header: &[u8], index: usize, width: usize) -> u64 {
    let mut word = [0; 8];
    let bytes = header.iter().skip(index.strict_mul(width).strict_add(2));
    for (byte, value) in word.iter_mut().zip(bytes.take(width)) {
        *byte = *value;
    }
    u64::from_le_bytes(word)
}

impl Vector<'_> {
    /// Writes the samples into `out`, which holds exactly them. Each codec but raw
    /// gives the samples of the integers through `sample`, the inverse of the
    /// `sortable` of [`write`].
    ///
    /// # Panics
    ///
    /// Panics on an ALP vector, which [`alp::decode`] decodes.
    pub(crate) fn decode<const W: usize>(
        &self,
        out: &mut [u8],
        sample: impl Fn(u64) -> u64,
    ) {
        let store = |value| word::store::<W>(sample(value));
        let body = self.body;
        let mut samples = out.as_chunks_mut::<W>().0.iter_mut();
        match self.plan {
            Plan::Raw => {
                for (sample, value) in samples.zip(body.as_chunks::<W>().0) {
                    *sample = *value;
                }
            }
            Plan::Ffor { reference, bits: 0 } => {
                samples.into_slice().fill(store(reference));
            }
            Plan::Ffor { reference, bits } => {
                for (sample, residual) in samples.zip(bits::unpack(body, bits)) {
                    *sample = store(reference.wrapping_add(residual));
                }
            }
            Plan::Delta {
                first,
                base,
                bits: 0,
            } => {
                let mut value = first;
                for sample in samples {
                    *sample = store(value);
                    value = value.wrapping_add(base);
                }
            }
            Plan::Delta { first, base, bits } => {
                let mut previous = first;
                if let Some(sample) = samples.next() {
                    *sample = store(first);
                }
                for (sample, residual) in samples.zip(bits::unpack(body, bits)) {
                    previous = previous.wrapping_add(base).wrapping_add(residual);
                    *sample = store(previous);
                }
            }
            Plan::Rle { runs } => {
                let (values, lengths) = body.split_at(runs.strict_mul(W));
                let runs = values
                    .as_chunks::<W>()
                    .0
                    .iter()
                    .zip(lengths.as_chunks::<2>().0);
                for (value, len) in runs {
                    let len = usize::from(u16::from_le_bytes(*len));
                    let value = store(word::load(value));
                    for sample in samples.by_ref().take(len) {
                        *sample = value;
                    }
                }
            }
            Plan::Alp { .. } => panic!("invariant: float::decode decodes ALP vectors"),
        }
    }

    /// Writes the samples of a raw vector into `out`, which holds exactly them.
    pub(crate) fn copy(&self, out: &mut [u8]) {
        assert_eq!(
            self.plan,
            Plan::Raw,
            "invariant: raw layouts read only raw vectors"
        );
        out.copy_from_slice(self.body);
    }
}

#[cfg(test)]
mod tests {
    use types::sample::Scalar;

    use super::*;

    #[test]
    #[should_panic(expected = "invariant: vector 2 holds 1025 samples, over 1024")]
    fn panics_past_one_vector() {
        let _result = read(&[RAW, 0], Layout::of(Scalar::U8), VECTOR_LEN + 1, 2);
    }
}
