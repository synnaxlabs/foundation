//! What the message codecs of this crate have in common: a writer that fills one
//! message field by field, and a reader of the fields of one message.

use std::mem;

/// Fills `out` from the front, one field at a time.
pub(crate) struct Writer<'o>(&'o mut [u8]);

impl<'o> Writer<'o> {
    /// A writer for a message of `len` bytes.
    ///
    /// # Panics
    ///
    /// When `out` is not `len` bytes.
    pub(crate) fn new(out: &'o mut [u8], len: usize) -> Self {
        assert!(
            out.len() == len,
            "out has {} bytes, and the message has {len}",
            out.len()
        );
        Self(out)
    }

    /// Writes `bytes` as the next field.
    pub(crate) fn put(&mut self, bytes: &[u8]) {
        self.field(bytes.len()).copy_from_slice(bytes);
    }

    /// The next field of `len` bytes, for the caller to fill.
    pub(crate) fn field(&mut self, len: usize) -> &mut [u8] {
        let (field, rest) = mem::take(&mut self.0).split_at_mut(len);
        self.0 = rest;
        field
    }
}

/// Reads the fields of one message from the front. A message that ends inside a
/// field, or that has bytes after its last field, gives `length`: the error of the
/// caller that names the message's length.
pub(crate) struct Fields<'b, E> {
    rest: &'b [u8],
    length: E,
}

impl<'b, E: Copy> Fields<'b, E> {
    /// A reader of the fields in `rest`, which gives `length` for a wrong length.
    pub(crate) fn new(rest: &'b [u8], length: E) -> Self {
        Self { rest, length }
    }

    /// Takes the next field of `N` bytes.
    pub(crate) fn take<const N: usize>(&mut self) -> Result<[u8; N], E> {
        let (&field, rest) = self.rest.split_first_chunk().ok_or(self.length)?;
        self.rest = rest;
        Ok(field)
    }

    /// Takes the next field of `len` bytes.
    pub(crate) fn take_slice(&mut self, len: usize) -> Result<&'b [u8], E> {
        let (field, rest) = self.rest.split_at_checked(len).ok_or(self.length)?;
        self.rest = rest;
        Ok(field)
    }

    /// Checks that no byte follows the last field.
    pub(crate) fn end(&self) -> Result<(), E> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(self.length)
        }
    }
}

/// The rest of a body in transit: messages back to back, none empty, that hold
/// exactly the bytes of the body.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Body {
    len: usize,
    remain: usize,
}

/// Why [`Body::take`] refused a message. Each codec maps it to its own error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The message is empty.
    Empty,
    /// The message has `len` bytes, more than the `remain` of the body.
    Over { len: usize, remain: usize },
}

impl Body {
    /// A body of `len` bytes.
    pub(crate) fn new(len: usize) -> Self {
        Self { len, remain: len }
    }

    /// Takes `message` as the next part of the body. Gives whether the body ends
    /// with it.
    pub(crate) fn take(&mut self, message: &[u8]) -> Result<bool, Refusal> {
        let (len, remain) = (message.len(), self.remain);
        if len == 0 {
            return Err(Refusal::Empty);
        }
        self.remain = remain
            .checked_sub(len)
            .ok_or(Refusal::Over { len, remain })?;
        Ok(self.remain == 0)
    }

    /// The bytes that remain.
    pub(crate) fn remain(&self) -> usize {
        self.remain
    }

    /// Where in the body the next message starts.
    pub(crate) fn at(&self) -> usize {
        self.len
            .checked_sub(self.remain)
            .expect("invariant: the rest of the body is no longer than the body")
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn refuses_an_empty_message_and_keeps_the_rest() {
        let mut body = Body::new(4);
        assert_eq!(body.take(&[]), Err(Refusal::Empty));
        assert_eq!((body.remain(), body.at()), (4, 0));
    }

    #[test]
    fn refuses_a_message_past_the_rest_and_keeps_the_rest() {
        let mut body = Body::new(4);
        assert_eq!(body.take(&[0; 3]), Ok(false));
        assert_eq!(body.take(&[0; 2]), Err(Refusal::Over { len: 2, remain: 1 }));
        assert_eq!((body.remain(), body.at()), (1, 3));
        assert_eq!(body.take(&[0]), Ok(true));
        assert_eq!(body.take(&[0]), Err(Refusal::Over { len: 1, remain: 0 }));
    }

    proptest! {
        #[test]
        fn ends_at_the_last_part_of_any_cut(
            parts in proptest::collection::vec(1..40_usize, 1..8),
        ) {
            let len = parts.iter().sum();
            let mut body = Body::new(len);
            let mut at = 0;
            for (i, &part) in parts.iter().enumerate() {
                prop_assert_eq!(body.at(), at);
                prop_assert_eq!(body.take(&vec![0; part]), Ok(i == parts.len() - 1));
                at += part;
                prop_assert_eq!(body.remain(), len - at);
            }
        }
    }
}
