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
//!
//! Fields are samples unless marked. Sample arithmetic is modulo 2^b, where b is the
//! bit count of a sample. Delta packs one residual for each sample after the first.
//! Packed values are least significant bit first.

use std::{iter, mem};

use crate::{Error, Layout, VECTOR_LEN, bits, word};

pub(crate) const RAW: u8 = 0;
pub(crate) const FFOR: u8 = 1;
pub(crate) const DELTA: u8 = 2;
const RLE: u8 = 3;

/// The codec of one vector and its header values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Plan {
    Raw,
    Ffor { reference: u64, bits: u8 },
    Delta { first: u64, base: u64, bits: u8 },
    Rle { runs: usize },
}

impl Plan {
    /// The bytes of a vector of `count` samples of `width` bytes.
    pub(crate) fn len(self, count: usize, width: usize) -> usize {
        self.header_len(width)
            .strict_add(self.body_len(count, width))
    }

    fn header_len(self, width: usize) -> usize {
        let fields = match self {
            Self::Raw => 0,
            Self::Ffor { .. } => width,
            Self::Delta { .. } => width.strict_mul(2),
            Self::Rle { .. } => 2,
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
        }
    }

    /// The largest bit width the codec allows for samples of `width` bytes.
    fn max_bits(self, width: usize) -> u8 {
        match self {
            Self::Raw | Self::Rle { .. } => 0,
            Self::Ffor { .. } | Self::Delta { .. } => word::bits(width),
        }
    }
}

/// The bytes of a header with `fields` bytes of fields, for samples of `width` bytes.
pub(crate) fn header_len(fields: usize, width: usize) -> usize {
    fields.strict_add(2).next_multiple_of(width)
}

/// Writes one vector of `W`-byte integers with `plan` into the front of `out` and
/// returns its length.
pub(crate) fn write<const W: usize>(chunk: &[u8], plan: Plan, out: &mut [u8]) -> usize {
    let mut out = Writer { out, len: 0 };
    let count = chunk.len().div_euclid(W);
    let samples = word::samples::<W>(chunk);
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
            out.put(
                &u16::try_from(runs)
                    .expect("invariant: a vector holds under 2^16 runs")
                    .to_le_bytes(),
            );
            out.pad(W);
            let values = out.take(runs.strict_mul(W)).as_chunks_mut::<W>().0;
            let lengths = out.take(runs.strict_mul(2)).as_chunks_mut::<2>().0;
            for ((run, length), (value, len)) in
                self::runs::<W>(chunk).zip(values.iter_mut().zip(lengths))
            {
                *value = word::store(run);
                *len = u16::try_from(length)
                    .expect("invariant: a run is at most 1024 samples")
                    .to_le_bytes();
            }
            out.pad(W);
        }
    }
    out.len
}

/// The runs of equal samples in `chunk`: each value and its length.
fn runs<const W: usize>(chunk: &[u8]) -> impl Iterator<Item = (u64, usize)> + '_ {
    let mut samples = word::samples::<W>(chunk).peekable();
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
    plan: Plan,
    body: &'a [u8],
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
    Ok((Vector { plan, body }, rest))
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

/// Reads header field `index`: `width` little-endian bytes after the tag and the bit
/// width. Bytes past the end read as zeros.
fn field(header: &[u8], index: usize, width: usize) -> u64 {
    let mut word = [0; 8];
    let bytes = header.iter().skip(index.strict_mul(width).strict_add(2));
    for (byte, value) in word.iter_mut().zip(bytes.take(width)) {
        *byte = *value;
    }
    u64::from_le_bytes(word)
}

impl Vector<'_> {
    /// Writes the samples into `out`, which holds exactly them.
    pub(crate) fn decode<const W: usize>(&self, out: &mut [u8]) {
        let body = self.body;
        let out = out.as_chunks_mut::<W>().0;
        let mut samples = out.iter_mut();
        match self.plan {
            Plan::Raw => {
                for (sample, value) in samples.zip(body.as_chunks::<W>().0) {
                    *sample = *value;
                }
            }
            Plan::Ffor { reference, bits: 0 } => out.fill(word::store(reference)),
            Plan::Ffor { reference, bits } => {
                for (sample, residual) in samples.zip(bits::unpack(body, bits)) {
                    *sample = word::store(reference.wrapping_add(residual));
                }
            }
            Plan::Delta { first, base, bits } => {
                let mut previous = first;
                if let Some(sample) = samples.next() {
                    *sample = word::store(first);
                }
                for (sample, residual) in samples.zip(bits::unpack(body, bits)) {
                    previous = previous.wrapping_add(base).wrapping_add(residual);
                    *sample = word::store(previous);
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
                    for sample in samples.by_ref().take(len) {
                        *sample = *value;
                    }
                }
            }
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
