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
        let (field, rest) = mem::take(&mut self.0).split_at_mut(bytes.len());
        field.copy_from_slice(bytes);
        self.0 = rest;
    }
}

/// The slots of one message of a run, `items` of `N` bytes each, to fill.
///
/// # Panics
///
/// When `items` is 0, or `out` is not `items * N` bytes.
pub(crate) fn slots<const N: usize>(out: &mut [u8], items: usize) -> &mut [[u8; N]] {
    assert!(items > 0, "a message of a run holds at least one item");
    Writer::new(out, items.saturating_mul(N));
    out.as_chunks_mut::<N>().0
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

    /// Checks that no byte follows the last field.
    pub(crate) fn end(&self) -> Result<(), E> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(self.length)
        }
    }
}
