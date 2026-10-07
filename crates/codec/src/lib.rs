//! Compresses and checks one series: per-vector selection, codecs, header validation,
//! format version.
//!
//! A series is the samples of one [`Type`]. A series of a [`Scalar`] encodes as vectors
//! of up to [`VECTOR_LEN`] samples, each with its own codec tag, so a series decodes by
//! itself. An array series encodes as the series of its elements. A `String`, `Bytes`,
//! or `List` series encodes as the `u32` series of its ends, then the series of its
//! elements. The sample count is not in the bytes: it travels with the series.

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

use types::sample::{Scalar, Type};

use crate::vector::Vector;

/// The format version of encoded vectors. A series does not carry it: each wire version
/// fixes one codec version, and disk footers record it.
pub const VERSION: u16 = 1;

/// Samples in a full vector. The last vector of a series may hold fewer.
pub const VECTOR_LEN: usize = 1024;

/// The most bytes [`Encoder::encode`] writes for `len` bytes of values of `data_type`.
///
/// # Panics
///
/// Panics when the bound is more than `usize::MAX`. No slice is that long.
#[must_use]
pub fn max_len(data_type: Type, len: usize) -> usize {
    Shape::of(data_type).max_len(len)
}

/// Encodes the series of one channel.
#[derive(Debug)]
pub struct Encoder {
    shape: Shape,
}

impl Encoder {
    /// Makes an encoder for samples of `data_type`.
    #[must_use]
    pub fn new(data_type: Type) -> Self {
        Self {
            shape: Shape::of(data_type),
        }
    }

    /// Encodes `values`, the raw bytes of `count` samples, into the front of `out` and
    /// returns the bytes written. It checks `values` before `out`.
    ///
    /// Raw samples are little-endian, and an array's elements are back to back. A
    /// `String`, `Bytes`, or `List` series is the `u32` end of each sample, counted in
    /// elements from the first, then padding up to a multiple of the element width or
    /// 8, whichever is less, then the elements. `encode` does not read the padding.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Overflow`] when `count` samples take more than `usize::MAX`
    /// bytes, [`Error::Length`] when `values` does not hold them, and [`Error::Ends`]
    /// or [`Error::Long`] when their ends are not valid. It writes nothing then.
    ///
    /// # Panics
    ///
    /// Panics when `out` is shorter than [`max_len`] of `values.len()`.
    pub fn encode(
        &mut self,
        count: usize,
        values: &[u8],
        out: &mut [u8],
    ) -> Result<usize, Error> {
        let (ends, elements) = self.shape.split(count, values)?;
        let max = self.shape.max_len(values.len());
        assert!(
            out.len() >= max,
            "out holds {} bytes, fewer than the {max} that {count} samples may need",
            out.len()
        );
        Ok(match self.shape {
            Shape::Fixed { element, .. } => element.write(elements, out),
            Shape::Variable { element, .. } => {
                let len = Layout::END.write(ends, out);
                len.strict_add(element.write(elements, out.split_at_mut(len).1))
            }
        })
    }
}

/// Checks `bytes`, an encoded series of `count` samples of `data_type`, and returns the
/// length of its raw bytes. It reads each vector header, and it decodes the ends of a
/// `String`, `Bytes`, or `List` series, but no other samples.
///
/// The bytes do not carry `count`. A wrong count passes when the vectors also parse
/// at it: an FFOR or delta vector with bit width 0 holds any count up to
/// [`VECTOR_LEN`], and padding can hide a few samples.
///
/// # Errors
///
/// Returns [`Error::Overflow`] when the samples take more than `usize::MAX` bytes, the
/// error of the first vector whose header or length is not valid, [`Error::Ends`] or
/// [`Error::Long`] for the first end that is not valid, or [`Error::Trailing`].
#[inline]
pub fn validate(data_type: Type, count: usize, bytes: &[u8]) -> Result<usize, Error> {
    let Type::Scalar(scalar) = data_type else {
        return validate_shape(data_type, count, bytes);
    };
    let layout = Layout::of(scalar);
    let len = layout.raw_len(count)?;
    trailing(layout.check(count, bytes, 0)?)?;
    Ok(len)
}

/// [`validate`] for any type. Out of line, so that the scalar path of [`validate`] is
/// small enough to inline.
#[inline(never)]
fn validate_shape(data_type: Type, count: usize, bytes: &[u8]) -> Result<usize, Error> {
    let (len, rest) = match Shape::of(data_type) {
        Shape::Fixed { element, len } => {
            let elements = elements(count, len)?;
            (
                element.raw_len(elements)?,
                element.check(elements, bytes, 0)?,
            )
        }
        Shape::Variable { element, max } => {
            let front = element.front(count)?;
            let (elements, rest) = ends(count, bytes, max, None)?;
            (
                front.raw_len(elements)?,
                element.check(elements, rest, vectors(count))?,
            )
        }
    };
    trailing(rest)?;
    Ok(len)
}

/// Decodes `bytes`, an encoded series of `count` samples of `data_type`, into `out`. It
/// checks what [`validate`] checks, and it writes the padding of a variable series as
/// zeros.
///
/// # Errors
///
/// Returns the errors of [`validate`], whatever the length of `out`. The contents of
/// `out` are then unspecified.
///
/// # Panics
///
/// Panics when `bytes` are valid and `out` is not the length that [`validate`]
/// returns.
#[inline]
pub fn decode(
    data_type: Type,
    count: usize,
    bytes: &[u8],
    out: &mut [u8],
) -> Result<(), Error> {
    let Type::Scalar(scalar) = data_type else {
        return decode_shape(data_type, count, bytes, out);
    };
    let layout = Layout::of(scalar);
    if out.len() != layout.raw_len(count)? {
        return misfit(data_type, count, bytes, out.len());
    }
    trailing(layout.fill(count, bytes, 0, out)?)
}

/// [`decode`] for any type. Out of line, so that the scalar path of [`decode`] is small
/// enough to inline.
#[inline(never)]
fn decode_shape(
    data_type: Type,
    count: usize,
    bytes: &[u8],
    out: &mut [u8],
) -> Result<(), Error> {
    let held = out.len();
    let rest = match Shape::of(data_type) {
        Shape::Fixed { element, len } => {
            let elements = elements(count, len)?;
            if held != element.raw_len(elements)? {
                return misfit(data_type, count, bytes, held);
            }
            element.fill(elements, bytes, 0, out)?
        }
        Shape::Variable { element, max } => {
            let front = element.front(count)?;
            let Some((front_out, out)) = out.split_at_mut_checked(front.start) else {
                return misfit(data_type, count, bytes, held);
            };
            let (ends_out, padding) = front_out.split_at_mut(front.ends);
            padding.fill(0);
            let (elements, rest) = ends(count, bytes, max, Some(ends_out))?;
            if out.len() != element.raw_len(elements)? {
                return misfit(data_type, count, bytes, held);
            }
            element.fill(elements, rest, vectors(count), out)?
        }
    };
    trailing(rest)
}

/// Returns the error of `bytes` when they are not valid. Otherwise it panics: `decode`
/// got an `out` of `held` bytes, which is not their raw length.
#[cold]
fn misfit(
    data_type: Type,
    count: usize,
    bytes: &[u8],
    held: usize,
) -> Result<(), Error> {
    let len = validate(data_type, count, bytes)?;
    panic!("out holds {held} bytes, not the {len} of {count} samples");
}

/// The sample count of each vector in a series of `count` samples.
fn counts(count: usize) -> impl Iterator<Item = usize> {
    iter::successors(Some(count), |left| left.checked_sub(VECTOR_LEN))
        .take_while(|left| *left > 0)
        .map(|left| left.min(VECTOR_LEN))
}

/// The vectors in a series of `count` samples.
fn vectors(count: usize) -> usize {
    count.div_ceil(VECTOR_LEN)
}

/// Checks that no bytes are left after the last vector.
fn trailing(rest: &[u8]) -> Result<(), Error> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(Error::Trailing { extra: rest.len() })
    }
}

/// Decodes an encoded series one vector at a time, so that the caller needs room for
/// only [`VECTOR_LEN`] samples.
#[derive(Debug)]
pub struct Decoder<'a> {
    layout: Layout,
    /// The samples of the vectors not yet read.
    left: usize,
    /// The index of the next vector.
    index: usize,
    /// The bytes after the vectors read.
    rest: &'a [u8],
}

impl<'a> Decoder<'a> {
    /// A decoder of `bytes`, an encoded series of `count` samples of `scalar`.
    #[must_use]
    pub fn new(scalar: Scalar, count: usize, bytes: &'a [u8]) -> Self {
        Self {
            layout: Layout::of(scalar),
            left: count,
            index: 0,
            rest: bytes,
        }
    }

    /// Decodes the next vector into the front of `out` and returns its samples.
    /// Returns `None` after the last vector and after an error.
    ///
    /// # Errors
    ///
    /// Returns the error of the vector it reads, as [`validate`] gives it, or
    /// [`Error::Trailing`] after the last vector. It never gives
    /// [`Error::Overflow`], because it needs no room for all the samples: for a
    /// count whose bytes pass `usize::MAX`, it gives the error of the vector where the
    /// bytes end.
    ///
    /// # Panics
    ///
    /// Panics when `out` holds fewer than [`VECTOR_LEN`] samples, whatever the next
    /// vector holds.
    #[must_use = "the result holds the error of the vector"]
    pub fn next<'o>(&mut self, out: &'o mut [u8]) -> Option<Result<&'o [u8], Error>> {
        let width = self.layout.width();
        assert!(
            out.len() >= VECTOR_LEN.strict_mul(width),
            "out holds {} bytes, fewer than {VECTOR_LEN} samples of {width} bytes",
            out.len()
        );
        if self.left == 0 {
            return trailing(mem::take(&mut self.rest)).err().map(Err);
        }
        let count = self.left.min(VECTOR_LEN);
        let (vector, rest) =
            match vector::read(self.rest, self.layout, count, self.index) {
                Ok(read) => read,
                Err(error) => {
                    (self.left, self.rest) = (0, &[]);
                    return Some(Err(error));
                }
            };
        self.left = self.left.strict_sub(count);
        self.index = self.index.strict_add(1);
        self.rest = rest;
        let samples = out.split_at_mut(count.strict_mul(width)).0;
        self.layout.decode(&vector, samples);
        Some(Ok(samples))
    }
}

/// The elements of `count` samples of `len` elements.
fn elements(count: usize, len: usize) -> Result<usize, Error> {
    count.checked_mul(len).ok_or(Error::Overflow)
}

/// Reads and checks the ends of a series of `count` variable samples of at most `max`
/// elements, at the front of `bytes`. It decodes them into `out` when given, which
/// then holds exactly them, or else into a buffer on the stack. Returns the elements
/// they end at and the bytes after them.
fn ends<'a>(
    count: usize,
    bytes: &'a [u8],
    max: u32,
    out: Option<&mut [u8]>,
) -> Result<(usize, &'a [u8]), Error> {
    const VECTOR: usize = Layout::END.width().strict_mul(VECTOR_LEN);
    let mut ends = Ends::new(max);
    let mut rest = bytes;
    let mut each = |index, count, out: &mut [u8]| {
        rest = Layout::END.fill(count, rest, index, out)?;
        ends.check(out)
    };
    if let Some(out) = out {
        let mut outs = out.chunks_mut(VECTOR);
        for (index, count) in counts(count).enumerate() {
            let out = outs.next().expect("invariant: out holds `count` ends");
            each(index, count, out)?;
        }
    } else {
        // Zeroing the scratch takes about 20 ns, so only `validate` makes it.
        let mut buffer = [0; VECTOR];
        for (index, count) in counts(count).enumerate() {
            let out = buffer.split_at_mut(Layout::END.raw_len(count)?).0;
            each(index, count, out)?;
        }
    }
    Ok((ends.elements(), rest))
}

/// How a series of a sample type holds its elements.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// `len` elements in each sample.
    Fixed { element: Layout, len: usize },
    /// The end of each sample, padding, then at most `max` elements in each sample.
    Variable { element: Layout, max: u32 },
}

impl Shape {
    fn of(data_type: Type) -> Self {
        match data_type {
            Type::Scalar(scalar) => Self::Fixed {
                element: Layout::of(scalar),
                len: 1,
            },
            Type::Array { element, len } => Self::Fixed {
                element: Layout::of(element),
                len: usize::try_from(len).expect("invariant: a usize holds a u32"),
            },
            Type::Matrix {
                element,
                rows,
                columns,
            } => Self::Fixed {
                element: Layout::of(element),
                len: usize::from(rows).strict_mul(usize::from(columns)),
            },
            Type::List { element, max } => Self::Variable {
                element: Layout::of(element),
                max,
            },
            Type::String | Type::Bytes => Self::Variable {
                element: Layout::of(Scalar::U8),
                max: u32::MAX,
            },
        }
    }

    /// Checks `values`, the raw bytes of `count` samples, and splits them into their
    /// ends and their elements, without the padding.
    fn split(self, count: usize, values: &[u8]) -> Result<(&[u8], &[u8]), Error> {
        let length = |expected| Error::Length {
            expected,
            actual: values.len(),
        };
        match self {
            Self::Fixed { element, len } => {
                let expected = element.raw_len(elements(count, len)?)?;
                if values.len() != expected {
                    return Err(length(expected));
                }
                Ok((&[], values))
            }
            Self::Variable { element, max } => {
                let front = element.front(count)?;
                let (ends, _) = values
                    .split_at_checked(front.ends)
                    .ok_or(length(front.ends))?;
                let mut check = Ends::new(max);
                check.check(ends)?;
                let expected = front.raw_len(check.elements())?;
                if values.len() != expected {
                    return Err(length(expected));
                }
                Ok((ends, values.split_at(front.start).1))
            }
        }
    }

    /// `len` bytes of values plus one raw header for each vector that the ends or the
    /// elements may need. Selection never picks a codec larger than raw, so it bounds
    /// every encoding.
    fn max_len(self, len: usize) -> usize {
        let headers = match self {
            Self::Fixed { element, .. } => element.headers(len),
            Self::Variable { element, .. } => {
                element.headers(len).strict_add(Layout::END.headers(len))
            }
        };
        len.checked_add(headers).unwrap_or_else(|| {
            panic!("the encoded size of {len} bytes of values is more than usize::MAX")
        })
    }
}

/// Checks the ends of a variable series in order, any number at a time.
#[derive(Debug)]
struct Ends {
    max: u32,
    last: u32,
    sample: usize,
}

impl Ends {
    fn new(max: u32) -> Self {
        Self {
            max,
            last: 0,
            sample: 0,
        }
    }

    /// Checks `ends`, the next little-endian ends.
    fn check(&mut self, ends: &[u8]) -> Result<(), Error> {
        for end in ends.as_chunks::<4>().0 {
            let end = u32::from_le_bytes(*end);
            let len = end.checked_sub(self.last).ok_or(Error::Ends {
                sample: self.sample,
                end,
                previous: self.last,
            })?;
            if len > self.max {
                return Err(Error::Long {
                    sample: self.sample,
                    len,
                    max: self.max,
                });
            }
            self.last = end;
            self.sample = self.sample.strict_add(1);
        }
        Ok(())
    }

    /// The elements that the ends so far end at.
    fn elements(&self) -> usize {
        usize::try_from(self.last).expect("invariant: a usize holds a u32")
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

/// The raw bytes of variable samples before their elements: the ends, then padding.
#[derive(Clone, Copy, Debug)]
struct Front {
    element: Layout,
    /// The bytes of the ends.
    ends: usize,
    /// Where the elements start: after the ends, padded to a multiple of the element
    /// width or 8, whichever is less. A frame starts each series on 8 bytes, so the
    /// elements are then aligned.
    start: usize,
}

impl Front {
    /// The raw bytes of the samples when they hold `elements` elements.
    fn raw_len(self, elements: usize) -> Result<usize, Error> {
        self.start
            .checked_add(self.element.raw_len(elements)?)
            .ok_or(Error::Overflow)
    }
}

impl Layout {
    /// The layout of the ends of a variable series.
    const END: Self = Self::Int32 { signed: false };

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

    const fn width(self) -> usize {
        match self {
            Self::Int8 { .. } => 1,
            Self::Int16 { .. } => 2,
            Self::Int32 { .. } => 4,
            Self::Int64 { .. } => 8,
            Self::Raw { width } => width,
        }
    }

    /// The bytes of `count` samples.
    fn raw_len(self, count: usize) -> Result<usize, Error> {
        count.checked_mul(self.width()).ok_or(Error::Overflow)
    }

    /// The ends and padding of `count` variable samples of elements of this layout.
    fn front(self, count: usize) -> Result<Front, Error> {
        let ends = Self::END.raw_len(count)?;
        let start = ends
            .checked_next_multiple_of(self.width().min(8))
            .ok_or(Error::Overflow)?;
        Ok(Front {
            element: self,
            ends,
            start,
        })
    }

    /// The bytes of the raw headers of the vectors of `len` bytes of values.
    fn headers(self, len: usize) -> usize {
        let width = self.width();
        vectors(len.div_ceil(width)).strict_mul(vector::header_len(0, width))
    }

    /// Encodes `values` as vectors into the front of `out` and returns the bytes
    /// written.
    fn write(self, values: &[u8], out: &mut [u8]) -> usize {
        let mut rest = out;
        let mut written = 0_usize;
        for chunk in values.chunks(VECTOR_LEN.strict_mul(self.width())) {
            let len = self.encode(chunk, rest);
            rest = mem::take(&mut rest).split_at_mut(len).1;
            written = written.strict_add(len);
        }
        written
    }

    /// Checks the headers of the vectors of `count` samples at the front of `bytes`,
    /// where the first is vector `first` of the series. Returns the bytes after them.
    fn check(self, count: usize, bytes: &[u8], first: usize) -> Result<&[u8], Error> {
        let mut rest = bytes;
        for (index, count) in counts(count).enumerate() {
            rest = vector::read(rest, self, count, first.strict_add(index))?.1;
        }
        Ok(rest)
    }

    /// Decodes the vectors of `count` samples at the front of `bytes` into `out`, which
    /// holds exactly the samples, where the first is vector `first` of the series.
    /// Returns the bytes after them.
    #[expect(
        clippy::inline_always,
        reason = "as a call, it made a decode of 10 samples about 6% slower"
    )]
    #[inline(always)]
    fn fill<'a>(
        self,
        count: usize,
        bytes: &'a [u8],
        first: usize,
        out: &mut [u8],
    ) -> Result<&'a [u8], Error> {
        let mut rest = bytes;
        let outs = out.chunks_mut(VECTOR_LEN.strict_mul(self.width()));
        for ((index, count), out) in counts(count).enumerate().zip(outs) {
            let (vector, after) =
                vector::read(rest, self, count, first.strict_add(index))?;
            self.decode(&vector, out);
            rest = after;
        }
        Ok(rest)
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

/// A series that is not valid. In a `String`, `Bytes`, or `List` series, vectors count
/// from the first vector of the ends to the last vector of the elements.
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
    /// The values to encode do not hold the samples.
    Length {
        /// The bytes the samples take. When the values end inside the ends of a
        /// variable series, the bytes of the ends.
        expected: usize,
        /// The bytes the values hold.
        actual: usize,
    },
    /// A sample of a `String`, `Bytes`, or `List` series ends before the sample
    /// before it.
    Ends {
        /// The index of the sample in the series.
        sample: usize,
        /// Its end.
        end: u32,
        /// The end of the sample before it.
        previous: u32,
    },
    /// A sample of a `List` series holds more elements than its type allows.
    Long {
        /// The index of the sample in the series.
        sample: usize,
        /// The elements it holds.
        len: u32,
        /// The most elements its type allows.
        max: u32,
    },
    /// The samples take more than `usize::MAX` bytes.
    Overflow,
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
            Self::Length { expected, actual } => write!(
                f,
                "the values hold {actual} bytes, but the samples take {expected}"
            ),
            Self::Ends {
                sample,
                end,
                previous,
            } => write!(
                f,
                "sample {sample} ends at element {end}, before the end of the sample \
                 before it at {previous}"
            ),
            Self::Long { sample, len, max } => write!(
                f,
                "sample {sample} holds {len} elements, more than its type's {max}"
            ),
            Self::Overflow => {
                f.write_str("the samples take more than usize::MAX bytes")
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
    use types::channel;
    use types::frame::key_set::{Group, Interner};
    use types::frame::{Draft, Form, Path};

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

    /// Encodes `count` samples of `data_type` in `values` and checks that the bytes do
    /// not depend on what `out` held.
    fn encode_type(data_type: Type, count: usize, values: &[u8]) -> Vec<u8> {
        let [zeros, ones] = [0x00, 0xff].map(|fill| {
            let mut out = vec![fill; max_len(data_type, values.len())];
            let len = Encoder::new(data_type)
                .encode(count, values, &mut out)
                .expect("the values hold the samples");
            out.truncate(len);
            out
        });
        assert_eq!(
            zeros, ones,
            "the encoding of {data_type:?} reads stale bytes"
        );
        zeros
    }

    /// Encodes `values` as samples of `scalar`.
    fn encode(scalar: Scalar, values: &[u8]) -> Vec<u8> {
        encode_type(Type::Scalar(scalar), values.len() / scalar.width(), values)
    }

    /// The little-endian bytes of `values` as samples of `width` bytes.
    fn bytes(width: usize, values: impl IntoIterator<Item = i128>) -> Vec<u8> {
        values
            .into_iter()
            .flat_map(|value| value.to_le_bytes().into_iter().take(width))
            .collect()
    }

    /// The raw bytes of a variable series with elements of `width` bytes: `ends`,
    /// zeros up to a multiple of `width` or 8, whichever is less, then `elements`.
    fn raw(ends: &[u32], width: usize, elements: &[u8]) -> Vec<u8> {
        let mut values: Vec<u8> =
            ends.iter().flat_map(|end| end.to_le_bytes()).collect();
        values.resize(values.len().next_multiple_of(width.min(8)), 0);
        values.extend(elements);
        values
    }

    /// The sample count and raw bytes of a variable series of `samples`, with elements
    /// of `width` bytes.
    fn variable<S: AsRef<[u8]>>(width: usize, samples: &[S]) -> (usize, Vec<u8>) {
        let ends: Vec<u32> = samples
            .iter()
            .scan(0, |end, sample| {
                *end += u32::try_from(sample.as_ref().len() / width).unwrap();
                Some(*end)
            })
            .collect();
        let elements: Vec<u8> =
            samples.iter().flat_map(|s| s.as_ref().to_vec()).collect();
        (samples.len(), raw(&ends, width, &elements))
    }

    /// Any sample type.
    fn any_type() -> impl Strategy<Value = Type> {
        let scalar = || prop::sample::select([INTS.as_slice(), &OTHERS].concat());
        prop_oneof![
            scalar().prop_map(Type::Scalar),
            (scalar(), 0..4_u32)
                .prop_map(|(element, len)| Type::Array { element, len }),
            (scalar(), 0..3_u16, 0..3_u16).prop_map(|(element, rows, columns)| {
                Type::Matrix {
                    element,
                    rows,
                    columns,
                }
            }),
            (scalar(), 0..6_u32).prop_map(|(element, max)| Type::List { element, max }),
            Just(Type::String),
            Just(Type::Bytes),
        ]
    }

    /// The raw bytes of `count` samples of `data_type`. A variable sample `n` holds
    /// `lens[n]` elements, cut to what its type allows. The bytes repeat `bytes`.
    fn samples_of(
        data_type: Type,
        count: usize,
        lens: &[u32],
        bytes: &[u8],
    ) -> Vec<u8> {
        let fill = |len| bytes.iter().copied().cycle().take(len).collect::<Vec<u8>>();
        let (width, max) = match data_type {
            Type::List { element, max } => (element.width(), max),
            Type::String | Type::Bytes => (1, u32::MAX),
            fixed => return fill(count * fixed.width().unwrap()),
        };
        let ends: Vec<u32> = lens[..count]
            .iter()
            .scan(0, |end, len| {
                *end += len % max.saturating_add(1);
                Some(*end)
            })
            .collect();
        let elements = usize::try_from(ends.last().copied().unwrap_or(0)).unwrap();
        raw(&ends, width, &fill(elements * width))
    }

    /// The count and series of a frame in `form` that holds `bytes` as the series of
    /// its one group, when the writer did not call `set_count`.
    fn uncounted(form: Form, bytes: &[u8]) -> (usize, Vec<u8>) {
        let set = Interner::new().intern(&[Group {
            index: channel::Key::from_u128(1),
            data: &[],
        }]);
        let config = block::Config { budget: 1 << 16 };
        let memory = block::Heap::new(config.reservation());
        let pool = block::Pool::new(config, memory);
        let mut draft = Draft::new(&pool, &set, form, &[(0, bytes.len())])
            .expect("the pool holds the frame");
        draft
            .series_mut(0)
            .expect("entry 0 is present")
            .copy_from_slice(bytes);
        let frame = draft.freeze(Path::Live);
        let range = frame.range(0).expect("group 0 is present");
        let count = usize::try_from(range.count).expect("the count fits");
        (count, frame.series(0).expect("entry 0 is present").to_vec())
    }

    #[test]
    fn refuses_a_series_whose_count_was_not_set() {
        let (count, series) = uncounted(Form::Raw, &[1; 16]);
        assert_eq!(
            Encoder::new(Type::Scalar(Scalar::Stamp)).encode(
                count,
                &series,
                &mut [0; 32]
            ),
            Err(Error::Length {
                expected: 0,
                actual: 16,
            })
        );
        let encoded = encode(Scalar::Stamp, &[1; 16]);
        let (count, series) = uncounted(Form::Encoded, &encoded);
        let trailing = Error::Trailing {
            extra: encoded.len(),
        };
        assert_eq!(
            validate(Type::Scalar(Scalar::Stamp), count, &series),
            Err(trailing.clone())
        );
        assert_eq!(
            decode(Type::Scalar(Scalar::Stamp), count, &series, &mut []),
            Err(trailing)
        );
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
        fn refuses_values_that_do_not_fit_the_count() {
            for scalar in INTS.into_iter().chain(OTHERS) {
                let width = scalar.width();
                let mut out = [7; 64];
                for (count, len) in
                    [(2, 2 * width - 1), (1, 2 * width), (3, width), (0, width)]
                {
                    assert_eq!(
                        Encoder::new(Type::Scalar(scalar)).encode(
                            count,
                            &vec![1; len],
                            &mut out
                        ),
                        Err(Error::Length {
                            expected: count * width,
                            actual: len,
                        }),
                        "{scalar:?} {count} {len}"
                    );
                }
                assert_eq!(out, [7; 64], "a refused {scalar:?} series wrote to out");
            }
        }

        #[test]
        fn sizes_out_from_the_values_not_the_count() {
            let values = [0; 16];
            let mut out = vec![0; max_len(Type::Scalar(Scalar::Uuid), values.len())];
            let count = usize::try_from(u32::MAX).expect("a u32 fits");
            assert_eq!(
                Encoder::new(Type::Scalar(Scalar::Uuid))
                    .encode(count, &values, &mut out),
                Err(Error::Length {
                    expected: 68_719_476_720,
                    actual: 16,
                })
            );
        }

        #[test]
        fn refuses_counts_past_usize() {
            assert_eq!(
                Encoder::new(Type::Scalar(Scalar::U16)).encode(
                    usize::MAX,
                    &[],
                    &mut []
                ),
                Err(Error::Overflow)
            );
        }

        #[test]
        fn checks_the_values_before_out() {
            assert_eq!(
                Encoder::new(Type::Scalar(Scalar::U8)).encode(1, &[], &mut []),
                Err(Error::Length {
                    expected: 1,
                    actual: 0,
                })
            );
        }

        #[test]
        #[should_panic(
            expected = "out holds 4 bytes, fewer than the 5 that 3 samples may need"
        )]
        fn panics_when_out_may_be_short() {
            let _result = Encoder::new(Type::Scalar(Scalar::U8)).encode(
                3,
                &[5, 5, 5],
                &mut [0; 4],
            );
        }
    }

    mod max_len {
        use super::*;

        #[test]
        fn is_raw_plus_one_raw_header_per_vector() {
            for (scalar, len, max) in [
                (Scalar::U8, 0, 0),
                (Scalar::U8, 1, 3),
                (Scalar::U8, 1_024, 1_026),
                (Scalar::U8, 1_025, 1_029),
                (Scalar::U16, 3, 5),
                (Scalar::U32, 12, 16),
                (Scalar::I64, 16_384, 16_400),
                (Scalar::Uuid, 16, 32),
            ] {
                assert_eq!(max_len(Type::Scalar(scalar), len), max, "{scalar:?} {len}");
            }
        }

        #[test]
        fn adds_a_raw_header_for_each_vector_of_ends_and_of_elements() {
            for (data_type, len, max) in [
                (
                    Type::Array {
                        element: Scalar::U16,
                        len: 3,
                    },
                    12,
                    14,
                ),
                (Type::String, 0, 0),
                (Type::String, 17, 23),
                (Type::Bytes, 5_000, 5_018),
                (
                    Type::List {
                        element: Scalar::Uuid,
                        max: 1,
                    },
                    20,
                    40,
                ),
            ] {
                assert_eq!(max_len(data_type, len), max, "{data_type:?} {len}");
            }
        }

        #[test]
        #[should_panic(
            expected = "the encoded size of 18446744073709551615 bytes of values"
        )]
        fn panics_past_usize() {
            std::hint::black_box(max_len(Type::Scalar(Scalar::U16), usize::MAX));
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
            assert_eq!(
                validate(Type::Scalar(scalar), count, &encoded),
                Ok(values.len())
            );
            let mut out = vec![0; values.len()];
            assert_eq!(
                decode(Type::Scalar(scalar), count, &encoded, &mut out),
                Ok(())
            );
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
            fn every_type(
                data_type in any_type(),
                count in 0..1_100_usize,
                lens in proptest::collection::vec(0..9_u32, 1_100),
                bytes in proptest::collection::vec(any::<u8>(), 1..64),
            ) {
                let values = samples_of(data_type, count, &lens, &bytes);
                let encoded = encode_type(data_type, count, &values);
                prop_assert_eq!(validate(data_type, count, &encoded), Ok(values.len()));
                let mut out = vec![0; values.len()];
                prop_assert_eq!(decode(data_type, count, &encoded, &mut out), Ok(()));
                prop_assert_eq!(out, values);
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

            #[test]
            fn constant_series_as_ffor_at_width_zero(
                scalar in select(&INTS),
                len in 2..2_100_usize,
                word in any::<u64>(),
            ) {
                let values = bytes(scalar.width(), iter::repeat_n(word.into(), len));
                let encoded = check(scalar, &values);
                prop_assert_eq!(&encoded[..2], &[vector::FFOR, 0], "{:?}", scalar);
            }

            #[test]
            fn fixed_steps_as_delta_at_width_zero(
                scalar in select(&INTS),
                // Under 17 samples, FFOR can cost no more than delta, and wins.
                len in 17..2_100_u64,
                first in any::<u64>(),
                step in any::<u64>(),
            ) {
                let width = scalar.width();
                prop_assume!(step & word::mask(width) != 0);
                let values = bytes(
                    width,
                    (0..len).map(|n| first.wrapping_add(n.wrapping_mul(step)).into()),
                );
                let encoded = check(scalar, &values);
                prop_assert_eq!(&encoded[..2], &[vector::DELTA, 0], "{:?}", scalar);
            }
        }
    }

    mod validate {
        use proptest::sample::select;

        use super::*;

        fn check(scalar: Scalar, count: usize, bytes: &[u8], expected: &Error) {
            assert_eq!(
                validate(Type::Scalar(scalar), count, bytes),
                Err(expected.clone()),
                "{scalar:?} {bytes:?}"
            );
            let mut out = vec![0; count * scalar.width()];
            assert_eq!(
                decode(Type::Scalar(scalar), count, bytes, &mut out),
                Err(expected.clone()),
                "{scalar:?}"
            );
        }

        fn accepts(scalar: Scalar, count: usize, bytes: &[u8], samples: &[u8]) {
            assert_eq!(
                validate(Type::Scalar(scalar), count, bytes),
                Ok(samples.len()),
                "{scalar:?} {bytes:?}"
            );
            let mut out = vec![0; samples.len()];
            assert_eq!(
                decode(Type::Scalar(scalar), count, bytes, &mut out),
                Ok(()),
                "{scalar:?}"
            );
            assert_eq!(out, samples, "{scalar:?} {bytes:?}");
        }

        fn truncated(vector: usize, needed: usize, available: usize) -> Error {
            Error::Truncated {
                vector,
                needed,
                available,
            }
        }

        fn width(bits: u8, max: u8) -> Error {
            Error::Width {
                vector: 0,
                bits,
                max,
            }
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
                check(Scalar::U8, 1, &[tag, 0, 0], &Error::Tag { vector: 0, tag });
            }
            for (scalar, tag) in
                OTHERS.into_iter().flat_map(|s| (1..4).map(move |t| (s, t)))
            {
                let mut bytes = [0; 32];
                bytes[0] = tag;
                check(scalar, 1, &bytes, &Error::Tag { vector: 0, tag });
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
            let runs = |total| Error::Runs {
                vector: 0,
                total,
                count: 3,
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
            check(Scalar::U8, 1, &[1, 0, 5, 0], &Error::Trailing { extra: 1 });
            check(Scalar::U8, 0, &[0], &Error::Trailing { extra: 1 });
        }

        #[test]
        fn refuses_counts_past_usize() {
            assert_eq!(
                validate(Type::Scalar(Scalar::U16), usize::MAX, &[]),
                Err(Error::Overflow)
            );
        }

        #[test]
        fn passes_any_count_a_vector_of_width_0_holds() {
            let second = 1_000_000_000;
            let encoded = encode(Scalar::Stamp, &bytes(8, (0..5).map(|n| n * second)));
            assert_eq!(encoded[..2], [2, 0], "five stamps make one delta vector");
            for count in [1, 5, 1_000, 1_024] {
                accepts(
                    Scalar::Stamp,
                    count,
                    &encoded,
                    &bytes(8, (0..count).map(|n| i128::try_from(n).unwrap() * second)),
                );
            }
            check(Scalar::Stamp, 1_025, &encoded, &truncated(1, 2, 0));
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
                (
                    Error::Length {
                        expected: 4,
                        actual: 3,
                    },
                    "the values hold 3 bytes, but the samples take 4",
                ),
                (
                    Error::Ends {
                        sample: 1,
                        end: 1,
                        previous: 2,
                    },
                    "sample 1 ends at element 1, before the end of the sample before \
                     it at 2",
                ),
                (
                    Error::Long {
                        sample: 1,
                        len: 3,
                        max: 2,
                    },
                    "sample 1 holds 3 elements, more than its type's 2",
                ),
                (
                    Error::Overflow,
                    "the samples take more than usize::MAX bytes",
                ),
            ] {
                assert_eq!(error.to_string(), text);
            }
        }

        /// Checks that `decode` gives the result of `validate`, with an `out` of `held`
        /// bytes when `bytes` are not valid, and that what it decodes encodes again.
        fn agree(
            data_type: Type,
            count: usize,
            bytes: &[u8],
            held: usize,
        ) -> Result<(), TestCaseError> {
            let validated = validate(data_type, count, bytes);
            let mut out = vec![0; *validated.as_ref().unwrap_or(&held)];
            prop_assert_eq!(
                decode(data_type, count, bytes, &mut out),
                validated.clone().map(|_| ())
            );
            if validated.is_ok() {
                let mut again = vec![0; max_len(data_type, out.len())];
                let encoded = Encoder::new(data_type).encode(count, &out, &mut again);
                prop_assert!(encoded.is_ok(), "{:?}", encoded);
            }
            Ok(())
        }

        proptest! {
            #[test]
            fn agrees_on_random_bytes_of_any_type(
                data_type in any_type(),
                count in 0..2_100_usize,
                bytes in proptest::collection::vec(any::<u8>(), 0..64),
                held in 0..64_usize,
            ) {
                agree(data_type, count, &bytes, held)?;
            }

            #[test]
            fn agrees_on_changed_encodings_of_any_type(
                data_type in any_type(),
                count in 0..1_100_usize,
                lens in proptest::collection::vec(0..9_u32, 1_100),
                bytes in proptest::collection::vec(0..4_u8, 1..64),
                at in any::<prop::sample::Index>(),
                byte in any::<u8>(),
                held in 0..64_usize,
            ) {
                let values = samples_of(data_type, count, &lens, &bytes);
                let mut encoded = encode_type(data_type, count, &values);
                let index = at.index(encoded.len().max(1));
                if let Some(target) = encoded.get_mut(index) {
                    *target = byte;
                }
                agree(data_type, count, &encoded, held)?;
            }

            #[test]
            fn never_panics_on_random_bytes(
                scalar in prop_oneof![select(&INTS), select(&OTHERS)],
                count in 0..2_100_usize,
                bytes in proptest::collection::vec(any::<u8>(), 0..64),
            ) {
                let mut out = vec![0; count * scalar.width()];
                let validated = validate(Type::Scalar(scalar), count, &bytes);
                if let Ok(len) = validated {
                    prop_assert_eq!(len, out.len());
                }
                prop_assert_eq!(
                    decode(Type::Scalar(scalar), count, &bytes, &mut out),
                    validated.map(|_| ())
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
                let validated = validate(Type::Scalar(scalar), count, &encoded);
                if let Ok(len) = validated {
                    prop_assert_eq!(len, out.len());
                }
                prop_assert_eq!(
                    decode(Type::Scalar(scalar), count, &encoded, &mut out),
                    validated.map(|_| ())
                );
            }
        }
    }

    mod decoder {
        use proptest::option::weighted;
        use proptest::sample::{Index, select};

        use super::*;

        /// The vectors that `Decoder` gives for a series, joined, or its first error.
        /// Checks that it then ends.
        fn joined(
            scalar: Scalar,
            count: usize,
            bytes: &[u8],
        ) -> Result<Vec<u8>, Error> {
            let mut decoder = Decoder::new(scalar, count, bytes);
            let mut out = vec![0; VECTOR_LEN * scalar.width()];
            let mut samples = Vec::new();
            let result = loop {
                match decoder.next(&mut out) {
                    Some(Ok(vector)) => samples.extend_from_slice(vector),
                    Some(Err(error)) => break Err(error),
                    None => break Ok(samples),
                }
            };
            assert_eq!(
                decoder.next(&mut out),
                None,
                "the decoder goes on after it ends"
            );
            result
        }

        #[test]
        fn gives_one_vector_at_a_time() {
            let values = bytes(2, 0..2_100);
            let series = encode(Scalar::U16, &values);
            let mut decoder = Decoder::new(Scalar::U16, 2_100, &series);
            let mut out = [0; VECTOR_LEN * 2];
            for chunk in values.chunks(VECTOR_LEN * 2) {
                assert_eq!(decoder.next(&mut out), Some(Ok(chunk)));
            }
            assert_eq!(decoder.next(&mut out), None);
        }

        #[test]
        fn ends_after_an_error() {
            let mut decoder = Decoder::new(Scalar::U8, 1, &[9, 0]);
            let mut out = [0; VECTOR_LEN];
            assert_eq!(
                decoder.next(&mut out),
                Some(Err(Error::Tag { vector: 0, tag: 9 }))
            );
            assert_eq!(decoder.next(&mut out), None);
        }

        #[test]
        fn refuses_bytes_after_the_last_vector_once() {
            let mut decoder = Decoder::new(Scalar::U8, 0, &[0, 0]);
            let mut out = [0; VECTOR_LEN];
            assert_eq!(
                decoder.next(&mut out),
                Some(Err(Error::Trailing { extra: 2 }))
            );
            assert_eq!(decoder.next(&mut out), None);
        }

        #[test]
        fn decodes_u32_max_samples_with_room_for_one_vector() {
            let count = usize::try_from(u32::MAX).expect("usize holds a u32");
            let runs = |len: usize| bytes(1, (0..len).map(|n| i128::from(n >= 512)));
            let full = encode(Scalar::U8, &runs(VECTOR_LEN));
            let last = encode(Scalar::U8, &runs(count % VECTOR_LEN));
            assert_eq!([full[0], last[0]], [vector::RLE; 2]);
            let series = [full.repeat(count / VECTOR_LEN), last].concat();
            let mut decoder = Decoder::new(Scalar::U8, count, &series);
            let expected = runs(VECTOR_LEN);
            let mut out = [0; VECTOR_LEN];
            let mut samples = 0;
            while let Some(vector) = decoder.next(&mut out) {
                let vector = vector.expect("the series is valid");
                assert_eq!(vector, &expected[..vector.len()]);
                samples += vector.len();
            }
            assert_eq!(samples, count);
        }

        #[test]
        #[should_panic(
            expected = "out holds 2047 bytes, fewer than 1024 samples of 2 bytes"
        )]
        fn panics_when_out_holds_less_than_a_vector() {
            let _vector = Decoder::new(Scalar::U16, 1, &[]).next(&mut [0; 2_047]);
        }

        #[test]
        #[should_panic(
            expected = "out holds 1023 bytes, fewer than 1024 samples of 1 bytes"
        )]
        fn panics_when_out_holds_less_than_a_vector_after_the_last() {
            let _vector = Decoder::new(Scalar::U8, 0, &[]).next(&mut [0; 1_023]);
        }

        #[test]
        fn gives_the_vector_error_for_a_count_past_usize_bytes() {
            let mut decoder = Decoder::new(Scalar::U64, usize::MAX, &[]);
            assert_eq!(
                validate(Type::Scalar(Scalar::U64), usize::MAX, &[]),
                Err(Error::Overflow)
            );
            assert_eq!(
                decoder.next(&mut [0; VECTOR_LEN * 8]),
                Some(Err(Error::Truncated {
                    vector: 0,
                    needed: 2,
                    available: 0
                }))
            );
        }

        proptest! {
            #[test]
            fn gives_what_decode_and_validate_give(
                scalar in prop_oneof![select(&INTS), select(&OTHERS)],
                values in proptest::collection::vec(0..4_u8, 0..2_100 * 8),
                skew in prop_oneof![3 => Just(0_isize), 1 => -2..=2_isize],
                change in weighted(0.25, (any::<Index>(), any::<u8>())),
                cut in weighted(0.25, any::<Index>()),
                extra in weighted(0.25, any::<u8>()),
            ) {
                let width = scalar.width();
                let values = &values[..values.len() / width * width];
                let mut series = encode(scalar, values);
                if let Some((at, byte)) = change
                    && !series.is_empty()
                {
                    let index = at.index(series.len());
                    series[index] = byte;
                }
                if let Some(at) = cut {
                    series.truncate(at.index(series.len() + 1));
                }
                series.extend(extra);
                let count = (values.len() / width).saturating_add_signed(skew);

                let mut out = vec![0; count * width];
                let decoded = decode(Type::Scalar(scalar), count, &series, &mut out)
                    .map(|()| out);
                prop_assert_eq!(
                    validate(Type::Scalar(scalar), count, &series),
                    decoded.as_ref().map(Vec::len).map_err(Clone::clone)
                );
                prop_assert_eq!(joined(scalar, count, &series), decoded);
            }
        }
    }

    mod array {
        use super::*;

        fn array(element: Scalar, len: u32) -> Type {
            Type::Array { element, len }
        }

        #[test]
        fn encodes_as_the_series_of_its_elements() {
            let values = bytes(2, (0..3_000).map(|n| n % 7));
            assert_eq!(
                encode_type(array(Scalar::U16, 3), 1_000, &values),
                encode(Scalar::U16, &values)
            );
        }

        #[test]
        fn encodes_a_matrix_as_the_array_of_its_elements() {
            let matrix = Type::Matrix {
                element: Scalar::F32,
                rows: 2,
                columns: 3,
            };
            let values = bytes(4, (0..6_000).map(|n| n % 11));
            assert_eq!(
                encode_type(matrix, 1_000, &values),
                encode_type(array(Scalar::F32, 6), 1_000, &values)
            );
        }

        #[test]
        fn refuses_values_that_do_not_fit_the_count() {
            for (data_type, count, len, expected) in [
                (array(Scalar::U16, 3), 2, 10, 12),
                (array(Scalar::U16, 3), 2, 14, 12),
                (array(Scalar::U8, 0), 5, 1, 0),
            ] {
                assert_eq!(
                    Encoder::new(data_type).encode(count, &vec![0; len], &mut [0; 64]),
                    Err(Error::Length {
                        expected,
                        actual: len,
                    }),
                    "{data_type:?} {count} {len}"
                );
            }
        }

        #[test]
        fn refuses_elements_past_usize() {
            let data_type = array(Scalar::U8, u32::MAX);
            let count = usize::MAX / 2;
            let encoded = Encoder::new(data_type).encode(count, &[], &mut []);
            assert_eq!(encoded, Err(Error::Overflow));
            assert_eq!(validate(data_type, count, &[]), Err(Error::Overflow));
            assert_eq!(decode(data_type, count, &[], &mut []), Err(Error::Overflow));
        }

        #[test]
        fn holds_any_count_of_empty_arrays() {
            let data_type = array(Scalar::U64, 0);
            assert_eq!(encode_type(data_type, 5, &[]), []);
            assert_eq!(validate(data_type, usize::MAX, &[]), Ok(0));
            assert_eq!(decode(data_type, usize::MAX, &[], &mut []), Ok(()));
        }
    }

    mod variable {
        use super::*;

        const LIST: Type = Type::List {
            element: Scalar::U16,
            max: 2,
        };
        const LIST_8: Type = Type::List {
            element: Scalar::F64,
            max: 2,
        };

        /// Checks that `encode` refuses the raw `ends` and `elements` of `data_type`
        /// with `expected`, and that `validate` and `decode` refuse them encoded.
        fn refuses(data_type: Type, ends: &[u32], elements: &[u8], expected: &Error) {
            let count = ends.len();
            let element = match data_type {
                Type::List { element, .. } => element,
                _ => Scalar::U8,
            };
            let values = raw(ends, element.width(), elements);
            let mut out = [7; 256];
            assert_eq!(
                Encoder::new(data_type).encode(count, &values, &mut out),
                Err(expected.clone()),
                "{data_type:?} raw"
            );
            assert_eq!(out, [7; 256], "a refused {data_type:?} series wrote to out");
            let encoded = [
                encode(Scalar::U32, &values[..4 * count]),
                encode(element, elements),
            ]
            .concat();
            assert_eq!(
                validate(data_type, count, &encoded),
                Err(expected.clone()),
                "{data_type:?}"
            );
            assert_eq!(
                decode(data_type, count, &encoded, &mut [0; 256]),
                Err(expected.clone()),
                "{data_type:?}"
            );
        }

        #[test]
        fn encodes_the_ends_then_the_elements() {
            let (count, values) = variable(1, &["ab", "", "cde"]);
            let encoded = [1, 2, 2, 0, 0, 0, 0, 0, 0x30, 0, 0, 0, 2, 0, 97, 1];
            for data_type in [Type::String, Type::Bytes] {
                assert_eq!(encode_type(data_type, count, &values), encoded);
                assert_eq!(validate(data_type, count, &encoded), Ok(17));
                let mut out = [0; 17];
                assert_eq!(decode(data_type, count, &encoded, &mut out), Ok(()));
                assert_eq!(out[..], values);
            }
        }

        #[test]
        fn counts_ends_in_elements() {
            let (count, values) = variable(2, &[vec![1, 0, 2, 0], vec![3, 0]]);
            assert_eq!(values[..8], [2, 0, 0, 0, 3, 0, 0, 0]);
            let encoded = encode_type(LIST, count, &values);
            let parts = [
                encode(Scalar::U32, &values[..8]),
                encode(Scalar::U16, &values[8..]),
            ];
            assert_eq!(encoded, parts.concat());
            assert_eq!(validate(LIST, count, &encoded), Ok(14));
        }

        #[test]
        fn pads_the_ends_to_the_element_width_or_8() {
            let value = 1.5_f64.to_le_bytes();
            let (count, values) = variable(8, &[value]);
            assert_eq!(values, [[1, 0, 0, 0, 0, 0, 0, 0], value].concat());
            let parts = [
                encode(Scalar::U32, &values[..4]),
                encode(Scalar::F64, &value),
            ];
            assert_eq!(encode_type(LIST_8, count, &values), parts.concat());
            for (element, count, start) in [
                (Scalar::U8, 1, 4),
                (Scalar::U16, 1, 4),
                (Scalar::F32, 1, 4),
                (Scalar::F64, 1, 8),
                (Scalar::F64, 2, 8),
                (Scalar::I64, 3, 16),
                (Scalar::Uuid, 1, 8),
            ] {
                let data_type = Type::List { element, max: 1 };
                let width = element.width();
                let mut values = vec![0xab; start + count * width];
                for (sample, end) in values.as_chunks_mut::<4>().0[..count]
                    .iter_mut()
                    .zip(1_u32..)
                {
                    *sample = end.to_le_bytes();
                }
                let encoded = encode_type(data_type, count, &values);
                let case = format!("{data_type:?}, {count} samples");
                assert_eq!(
                    validate(data_type, count, &encoded),
                    Ok(values.len()),
                    "{case}"
                );
                let mut out = vec![0xff; values.len()];
                assert_eq!(
                    decode(data_type, count, &encoded, &mut out),
                    Ok(()),
                    "{case}"
                );
                values[4 * count..start].fill(0);
                assert_eq!(out, values, "{case}");
            }
        }

        #[test]
        fn encodes_no_samples_as_no_bytes() {
            for data_type in [Type::String, Type::Bytes, LIST, LIST_8] {
                assert_eq!(encode_type(data_type, 0, &[]), [], "{data_type:?}");
                assert_eq!(validate(data_type, 0, &[]), Ok(0));
                assert_eq!(decode(data_type, 0, &[], &mut []), Ok(()));
            }
        }

        #[test]
        fn refuses_ends_that_decrease() {
            let ends = |sample, end, previous| Error::Ends {
                sample,
                end,
                previous,
            };
            refuses(Type::String, &[2, 1, 3], b"abc", &ends(1, 1, 2));
            refuses(LIST, &[1, 0], &[1, 0], &ends(1, 0, 1));
            let mut long = vec![5; 1_024];
            long.push(4);
            refuses(Type::Bytes, &long, b"abcde", &ends(1_024, 4, 5));
        }

        #[test]
        fn refuses_a_list_sample_longer_than_its_type_allows() {
            let long = |len, max| Error::Long {
                sample: 1,
                len,
                max,
            };
            refuses(LIST, &[2, 5], &[0; 10], &long(3, 2));
            let empty = Type::List {
                element: Scalar::U8,
                max: 0,
            };
            refuses(empty, &[0, 1], &[7], &long(1, 0));
        }

        #[test]
        fn refuses_values_that_do_not_fit_the_ends() {
            for (data_type, count, values, expected) in [
                (Type::String, 2, vec![0; 5], 8),
                (Type::String, 2, raw(&[1, 3], 1, b"ab"), 11),
                (Type::String, 2, raw(&[1, 3], 1, b"abcd"), 11),
                (LIST, 1, raw(&[1], 2, &[1, 0, 2]), 6),
                (LIST_8, 1, vec![1, 0, 0, 0, 0, 0], 16),
            ] {
                let mut out = [7; 64];
                assert_eq!(
                    Encoder::new(data_type).encode(count, &values, &mut out),
                    Err(Error::Length {
                        expected,
                        actual: values.len(),
                    }),
                    "{data_type:?} {values:?}"
                );
                assert_eq!(out, [7; 64], "a refused {data_type:?} series wrote to out");
            }
        }

        #[test]
        fn refuses_an_end_past_the_encoded_elements() {
            let encoded = [
                encode(Scalar::U32, &5_u32.to_le_bytes()),
                encode(Scalar::U8, &[0, 63, 0]),
            ]
            .concat();
            let truncated = Error::Truncated {
                vector: 1,
                needed: 7,
                available: 5,
            };
            assert_eq!(validate(Type::String, 1, &encoded), Err(truncated.clone()));
            assert_eq!(
                decode(Type::String, 1, &encoded, &mut [0; 9]),
                Err(truncated)
            );
        }

        #[test]
        fn numbers_vectors_across_the_ends_and_the_elements() {
            let mut ends = vec![0; 1_024];
            ends.push(1);
            let mut encoded = encode(Scalar::U32, &raw(&ends, 1, &[]));
            encoded.extend([9, 0, 120]);
            let tag = Error::Tag { vector: 2, tag: 9 };
            assert_eq!(validate(Type::String, 1_025, &encoded), Err(tag.clone()));
            let mut out = [0; 4_101];
            assert_eq!(decode(Type::String, 1_025, &encoded, &mut out), Err(tag));
        }

        #[test]
        fn refuses_ends_past_usize() {
            let encoded = Encoder::new(Type::String).encode(usize::MAX, &[], &mut []);
            assert_eq!(encoded, Err(Error::Overflow));
            assert_eq!(validate(Type::Bytes, usize::MAX, &[]), Err(Error::Overflow));
            assert_eq!(decode(LIST, usize::MAX, &[], &mut []), Err(Error::Overflow));
        }

        /// The ends fit in a `usize`, but not once padded to 8 bytes.
        #[test]
        fn refuses_padded_ends_past_usize() {
            let count = usize::MAX / 4;
            let encoded = Encoder::new(LIST_8).encode(count, &[], &mut []);
            assert_eq!(encoded, Err(Error::Overflow));
            assert_eq!(validate(LIST_8, count, &[]), Err(Error::Overflow));
            assert_eq!(decode(LIST_8, count, &[], &mut []), Err(Error::Overflow));
        }
    }

    mod decode {
        use super::*;

        #[test]
        #[should_panic(expected = "out holds 3 bytes, not the 4 of 2 samples")]
        fn panics_when_out_does_not_hold_the_samples() {
            let encoded = encode(Scalar::U16, &[1, 0, 2, 0]);
            let _result = decode(Type::Scalar(Scalar::U16), 2, &encoded, &mut [0; 3]);
        }

        #[test]
        #[should_panic(expected = "out holds 16 bytes, not the 17 of 3 samples")]
        fn panics_when_out_does_not_hold_the_elements() {
            let (count, values) = variable(1, &["ab", "", "cde"]);
            let encoded = encode_type(Type::String, count, &values);
            let _result = decode(Type::String, count, &encoded, &mut [0; 16]);
        }

        #[test]
        #[should_panic(expected = "out holds 4 bytes, not the 17 of 3 samples")]
        fn panics_when_out_does_not_hold_the_ends() {
            let (count, values) = variable(1, &["ab", "", "cde"]);
            let encoded = encode_type(Type::String, count, &values);
            let _result = decode(Type::String, count, &encoded, &mut [0; 4]);
        }

        #[test]
        fn refuses_bytes_that_are_not_valid_before_it_checks_out() {
            let truncated = Err(Error::Truncated {
                vector: 0,
                needed: 2,
                available: 0,
            });
            for held in [0, 3, 5, 9] {
                let mut out = vec![0; held];
                for data_type in [Type::Scalar(Scalar::U16), Type::String] {
                    assert_eq!(decode(data_type, 2, &[], &mut out), truncated);
                }
            }
        }

        #[test]
        fn refuses_counts_past_usize_before_it_checks_out() {
            assert_eq!(
                decode(Type::Scalar(Scalar::U16), usize::MAX, &[], &mut []),
                Err(Error::Overflow)
            );
        }
    }
}
