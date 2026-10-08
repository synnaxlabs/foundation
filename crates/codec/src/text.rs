//! The check that each sample of a `String` series is UTF-8.

use types::sample::Scalar;

use crate::{Decoder, Error, Layout, VECTOR_LEN};

/// Checks that each sample of a `String` series is UTF-8, given its raw `ends`, which
/// are valid, and its `elements`.
pub(crate) fn raw(ends: &[u8], elements: &[u8]) -> Result<(), Error> {
    if elements.is_ascii() {
        return Ok(());
    }
    check(Pieces::Raw(Some(ends)), 0, elements, Pieces::Raw(None))
}

/// Checks that each sample of an encoded `String` series of `count` samples is UTF-8,
/// given its encoded `ends` and the encoded `vectors` of its `elements`, which are
/// valid. It decodes each vector once, and the ends only when a vector is not ASCII.
#[expect(clippy::unwrap_in_result, reason = "the caller checked the vectors")]
pub(crate) fn encoded(
    count: usize,
    ends: &[u8],
    elements: usize,
    vectors: &[u8],
) -> Result<(), Error> {
    let mut vectors = Decoder::new(Scalar::U8, elements, vectors);
    let mut out = [0; VECTOR_LEN];
    let mut from = 0;
    while let Some(vector) = vectors.next(&mut out) {
        let vector = vector.expect("invariant: the vectors were checked");
        if !vector.is_ascii() {
            let ends = Pieces::Encoded(Decoder::new(Scalar::U32, count, ends));
            return check(ends, from, vector, Pieces::Encoded(vectors));
        }
        from = from.strict_add(vector.len());
    }
    Ok(())
}

/// Checks each sample that the `ends` cut from the elements, given that each element
/// before `from` is ASCII, `piece` holds the elements from `from` on, and `rest` gives
/// the elements after `piece`.
#[expect(
    clippy::unwrap_in_result,
    reason = "each expect is an invariant that the caller checked"
)]
fn check(
    mut ends: Pieces<'_>,
    from: usize,
    piece: &[u8],
    mut rest: Pieces<'_>,
) -> Result<(), Error> {
    let mut ends_out = [0; Layout::END.width().strict_mul(VECTOR_LEN)];
    let mut rest_out = [0; VECTOR_LEN];
    let mut text = Text::default();
    let mut piece = piece;
    let mut start = from;
    while let Some(decoded) = ends.next(&mut ends_out) {
        for end in decoded.as_chunks::<4>().0 {
            let end = usize::try_from(u32::from_le_bytes(*end))
                .expect("invariant: a usize holds a u32");
            // An ASCII prefix does not change whether the rest of a sample is UTF-8.
            let mut left = end.saturating_sub(start);
            while left > 0 {
                if piece.is_empty() {
                    piece = rest
                        .next(&mut rest_out)
                        .expect("invariant: the elements hold each end");
                }
                let (head, tail) = piece.split_at(left.min(piece.len()));
                text.elements(head)?;
                left = left.strict_sub(head.len());
                piece = tail;
            }
            text.end()?;
            start = start.max(end);
        }
    }
    Ok(())
}

/// Raw bytes, or encoded vectors of them, a piece at a time.
enum Pieces<'a> {
    /// The bytes, until taken.
    Raw(Option<&'a [u8]>),
    Encoded(Decoder<'a>),
}

impl<'a> Pieces<'a> {
    /// The next piece, decoded into `out` when encoded.
    fn next<'o>(&mut self, out: &'o mut [u8]) -> Option<&'o [u8]>
    where
        'a: 'o,
    {
        match self {
            Self::Raw(bytes) => bytes.take(),
            Self::Encoded(vectors) => vectors
                .next(out)
                .map(|vector| vector.expect("invariant: the vectors were checked")),
        }
    }
}

/// Checks that each sample of a `String` series is UTF-8, given its elements a piece
/// at a time, so that a sample and a char may span pieces.
#[derive(Debug, Default)]
struct Text {
    /// The index of the current sample.
    sample: usize,
    /// The first bytes of a char that the last piece ended inside.
    char: [u8; 4],
    /// The bytes of `char` held, or 0.
    held: usize,
    /// The bytes of the char in `char`.
    width: usize,
}

impl Text {
    /// Checks `piece`, the next elements of the current sample.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a char takes at most 4 bytes, so each expect holds"
    )]
    fn elements(&mut self, piece: &[u8]) -> Result<(), Error> {
        let mut piece = piece;
        if self.held > 0 {
            let missing = self.width.strict_sub(self.held);
            let (head, tail) = piece.split_at(missing.min(piece.len()));
            let filled = self.held.strict_add(head.len());
            self.char
                .get_mut(self.held..filled)
                .expect("invariant: a char takes at most 4 bytes")
                .copy_from_slice(head);
            self.held = filled;
            let char = self
                .char
                .get(..filled)
                .expect("invariant: a char fills 4 bytes at most");
            match str::from_utf8(char) {
                Ok(_) => self.held = 0,
                Err(error) if error.error_len().is_none() => return Ok(()),
                Err(_) => return Err(self.error()),
            }
            piece = tail;
        }
        match str::from_utf8(piece) {
            Ok(_) => Ok(()),
            Err(error) if error.error_len().is_none() => {
                // The piece ends inside a char, whose first byte gives its width.
                let tail = piece.split_at(error.valid_up_to()).1;
                let first = tail.first().expect("invariant: a char starts the tail");
                self.width = usize::try_from(first.leading_ones())
                    .expect("invariant: a usize holds a u32");
                self.held = tail.len();
                self.char
                    .get_mut(..self.held)
                    .expect("invariant: a char takes at most 4 bytes")
                    .copy_from_slice(tail);
                Ok(())
            }
            Err(_) => Err(self.error()),
        }
    }

    /// Ends the current sample.
    fn end(&mut self) -> Result<(), Error> {
        if self.held > 0 {
            return Err(self.error());
        }
        self.sample = self.sample.strict_add(1);
        Ok(())
    }

    fn error(&self) -> Error {
        Error::Utf8 {
            sample: self.sample,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_a_char_a_byte_at_a_time() {
        let mut text = Text::default();
        for byte in "\u{1f600}".bytes() {
            assert_eq!(text.elements(&[byte]), Ok(()));
        }
        assert_eq!(text.end(), Ok(()));
        let refused = [b"\xf0", b"\x9f", b"A"].map(|byte| text.elements(byte));
        assert_eq!(refused, [Ok(()), Ok(()), Err(Error::Utf8 { sample: 1 })]);
    }

    #[test]
    fn refuses_a_sample_that_ends_inside_a_char() {
        let mut text = Text::default();
        assert_eq!(text.elements(b"ab"), Ok(()));
        assert_eq!(text.end(), Ok(()));
        assert_eq!(text.elements(b"ab\xe2\x82"), Ok(()));
        assert_eq!(text.end(), Err(Error::Utf8 { sample: 1 }));
    }
}
