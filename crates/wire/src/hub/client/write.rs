//! The messages of a write stream before its frames. The frames, and the replies of
//! the node, are in [`hub::write`](crate::hub::write).

use types::authority::Authority;
use types::name::Name;
use types::time::Span;

use super::{Body, Error, WRITE, stream, text};
use crate::common::{Fields, Writer};

pub use super::stream::{Opening, Start};

const NO_LEASE: u8 = 0;
const LEASE: u8 = 1;

/// The first message of a write stream. The open follows as a body of `length` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Write {
    /// The bytes of the open, at most [`OPEN_BYTES_MAX`](super::OPEN_BYTES_MAX).
    pub length: u64,
    /// The signature of the hello's key over the signed bytes of the open, of the
    /// kind of a write: the connection and the open.
    pub signature: [u8; 64],
}

impl Write {
    /// The bytes of an encoded write.
    pub const LEN: usize = stream::LEN;

    /// Writes the write into `out`.
    ///
    /// # Panics
    ///
    /// When `length` is over [`OPEN_BYTES_MAX`](super::OPEN_BYTES_MAX), or `out` is not
    /// [`Write::LEN`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        stream::encode(WRITE, self.length, &self.signature, out);
    }

    /// Decodes a write.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`], [`Error::Kind`] for a message that is not kind 7,
    /// [`Error::Length`], and [`Error::Oversize`] for an open over
    /// [`OPEN_BYTES_MAX`](super::OPEN_BYTES_MAX).
    pub fn decode(message: &[u8]) -> Result<Self, Error> {
        let (length, signature) = stream::decode(WRITE, message)?;
        Ok(Self { length, signature })
    }

    /// The open that follows this write.
    ///
    /// # Panics
    ///
    /// When `length` is over [`OPEN_BYTES_MAX`](super::OPEN_BYTES_MAX).
    #[must_use]
    pub fn body(&self) -> Body {
        stream::body(self.length)
    }
}

/// The open of a write: the body after a [`Write`], and what its signature covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Open {
    /// The writer's authority.
    pub authority: Authority,
    /// How long the writer may go without a write and keep control, or `None` for no
    /// limit.
    pub lease: Option<Span>,
    /// The channels the writer writes, at least one, on one index or more. The cap of
    /// the open keeps them under [`PLACES_MAX`](super::PLACES_MAX).
    pub channels: Vec<Name>,
}

impl Open {
    /// The bytes of the encoded open.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        let lease = if self.lease.is_some() { 10 } else { 2 };
        self.channels
            .iter()
            .map(|name| name.as_str().len().saturating_add(1))
            .fold(lease, usize::saturating_add)
    }

    /// Writes the open into `out`.
    ///
    /// # Panics
    ///
    /// When `channels` is empty, or `out` is not [`Open::encoded_len`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        assert!(
            !self.channels.is_empty(),
            "the open of a write names no channel"
        );
        let mut out = Writer::new(out, self.encoded_len());
        out.put(&[self.authority.0]);
        match self.lease {
            None => out.put(&[NO_LEASE]),
            Some(span) => {
                out.put(&[LEASE]);
                out.put(&span.nanos().to_le_bytes());
            }
        }
        for name in &self.channels {
            out.put(&[u8::try_from(name.as_str().len())
                .expect("invariant: a name is at most 255 bytes")]);
            out.put(name.as_str().as_bytes());
        }
    }

    /// Decodes the open in `bytes`, all the bytes of its body. An open that decodes
    /// encodes back to `bytes` with [`Open::encode`].
    ///
    /// # Errors
    ///
    /// In the order of the bytes: [`Error::Empty`] when `bytes` is empty,
    /// [`Error::Length`] when the open ends inside a field, [`Error::Lease`] when the
    /// lease byte is neither 0 nor 1, and [`Error::Channel`] when a name does not
    /// read. Then [`Error::Unnamed`] when it names no channel.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut fields = Fields::new(bytes);
        let authority = fields.kind().ok_or(Error::Empty)?;
        let lease = match *fields.take()? {
            [NO_LEASE] => None,
            [LEASE] => Some(Span::from_nanos(i64::from_le_bytes(*fields.take()?))),
            [byte] => return Err(Error::Lease { byte }),
        };
        let mut channels = Vec::new();
        while !fields.is_empty() {
            let [len] = *fields.take()?;
            let name = text::read(fields.take_slice(usize::from(len))?)
                .map_err(Error::Channel)?;
            channels.push(name);
        }
        if channels.is_empty() {
            return Err(Error::Unnamed);
        }
        Ok(Self {
            authority: Authority(authority),
            lease,
            channels,
        })
    }
}

#[cfg(test)]
mod tests;
