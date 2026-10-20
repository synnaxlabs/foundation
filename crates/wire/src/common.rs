//! What the message codecs of this crate have in common: a writer that fills one
//! message field by field, a reader of the fields of one message, the count of a
//! body, and the count of a run.

use std::mem;

pub(crate) mod body;

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

/// A message that ends inside a field, or that has bytes after its last field. Each
/// codec's error takes it through `From`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Length {
    /// The bytes of the message.
    pub(crate) len: usize,
}

/// A message of a run that holds more items than remain in the run. Each codec's
/// error takes it through `From`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    /// The items of the message.
    pub(crate) items: usize,
    /// The items that remain in the run.
    pub(crate) remain: u32,
}

/// The items that remain in a run of `remain` after a message of `items` items.
pub(crate) fn rest_of_run(remain: u32, items: usize) -> Result<u32, Run> {
    match u32::try_from(items)
        .ok()
        .and_then(|count| remain.checked_sub(count))
    {
        Some(rest) => Ok(rest),
        None => Err(Run { items, remain }),
    }
}

/// Reads the fields of one message from the front.
pub(crate) struct Fields<'b> {
    rest: &'b [u8],
    len: usize,
}

impl<'b> Fields<'b> {
    /// A reader of the fields of `message`.
    pub(crate) fn new(message: &'b [u8]) -> Self {
        Self {
            rest: message,
            len: message.len(),
        }
    }

    /// Takes the kind byte, or `None` when no byte remains.
    pub(crate) fn kind(&mut self) -> Option<u8> {
        let (&kind, rest) = self.rest.split_first()?;
        self.rest = rest;
        Some(kind)
    }

    /// Whether no byte remains.
    pub(crate) fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }

    /// Takes the next field of `N` bytes.
    pub(crate) fn take<const N: usize>(&mut self) -> Result<&'b [u8; N], Length> {
        let (field, rest) = self.rest.split_first_chunk().ok_or(self.length())?;
        self.rest = rest;
        Ok(field)
    }

    /// Takes the next field of `len` bytes.
    pub(crate) fn take_slice(&mut self, len: usize) -> Result<&'b [u8], Length> {
        let (field, rest) = self.rest.split_at_checked(len).ok_or(self.length())?;
        self.rest = rest;
        Ok(field)
    }

    /// Takes the rest: one or more fields of `N` bytes, and no byte after the last.
    pub(crate) fn take_all<const N: usize>(self) -> Result<&'b [[u8; N]], Length> {
        let (all, tail) = self.rest.as_chunks();
        if all.is_empty() || !tail.is_empty() {
            return Err(self.length());
        }
        Ok(all)
    }

    /// Checks that no byte follows the last field.
    pub(crate) fn end(&self) -> Result<(), Length> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(self.length())
        }
    }

    fn length(&self) -> Length {
        Length { len: self.len }
    }
}

/// The messages of a fuzz input, as `fuzz::messages` cuts them: each a length byte
/// and then that many bytes, or the bytes that are left.
#[cfg(test)]
pub(crate) fn messages(mut bytes: &[u8]) -> Vec<&[u8]> {
    let mut messages = Vec::new();
    while let Some((&len, rest)) = bytes.split_first() {
        let (message, rest) = rest.split_at(usize::from(len).min(rest.len()));
        messages.push(message);
        bytes = rest;
    }
    messages
}
