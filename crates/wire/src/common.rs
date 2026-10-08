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
