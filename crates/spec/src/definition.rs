//! The definitions the spec tree stores, one per name, and their canonical bytes.
//! Each definition has exactly one encoding, and [`Definition::decode`] refuses every
//! byte string that [`Definition::encode`] cannot write.
//!
//! Format, with every integer little-endian:
//!
//! ```text
//! definition := version:u8 tag:u8 body
//! access     := subjects:patterns select:patterns allow:u8 authority:u8   tag 1
//! patterns   := count:u64 text*
//! text       := length:u64 UTF-8 bytes
//! ```
//!
//! `allow` holds bit `n` for the `n`th [`Action`](crate::access::Action), in
//! declaration order. `authority` is zero when `allow` does not hold `Write`.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

use std::{fmt, str};

use types::authority::Authority;
use types::name::{self, Selector};

use crate::access::{Action, Actions, Policy};

const VERSION: u8 = 1;
const ACCESS: u8 = 1;

/// One definition: the value of one name in the spec tree. The name is the tree key,
/// so it is not part of the definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Definition {
    /// An access policy.
    Access(Policy),
}

impl Definition {
    /// Writes the canonical bytes of the definition.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        match self {
            Self::Access(policy) => {
                out.push(ACCESS);
                patterns(&mut out, policy.subjects());
                patterns(&mut out, policy.select());
                out.push(policy.allow().bits());
                out.push(policy.authority().map_or(0, |a| a.0));
            }
        }
        out
    }

    /// Reads a definition from its canonical bytes.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when `bytes` are not the encoding of a definition.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader {
            rest: bytes,
            len: bytes.len(),
        };
        let version = reader.byte()?;
        if version > VERSION {
            return Err(Error::Newer { found: version });
        }
        if version != VERSION {
            return Err(Error::Version { found: version });
        }
        let at = reader.at();
        let definition = match reader.byte()? {
            ACCESS => Self::Access(reader.access()?),
            tag => return Err(Error::Kind { at, tag }),
        };
        if !reader.rest.is_empty() {
            return Err(Error::TrailingBytes { at: reader.at() });
        }
        Ok(definition)
    }
}

fn patterns(out: &mut Vec<u8>, patterns: &Patterns) {
    count(out, patterns.texts.len());
    for text in &patterns.texts {
        count(out, text.len());
        out.extend_from_slice(text.as_bytes());
    }
}

fn count(out: &mut Vec<u8>, n: usize) {
    let n = u64::try_from(n).expect("invariant: a length fits in 64 bits");
    out.extend_from_slice(&n.to_le_bytes());
}

struct Reader<'a> {
    rest: &'a [u8],
    len: usize,
}

impl<'a> Reader<'a> {
    fn at(&self) -> usize {
        self.len.saturating_sub(self.rest.len())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let at = self.at();
        let (taken, rest) = self
            .rest
            .split_at_checked(n)
            .ok_or(Error::Truncated { at })?;
        self.rest = rest;
        Ok(taken)
    }

    fn byte(&mut self) -> Result<u8, Error> {
        let at = self.at();
        let (&byte, rest) = self.rest.split_first().ok_or(Error::Truncated { at })?;
        self.rest = rest;
        Ok(byte)
    }

    /// Reads a count. A count larger than the bytes left cannot be valid, so it is
    /// refused before anything is allocated for it.
    fn count(&mut self) -> Result<usize, Error> {
        let at = self.at();
        let (&bytes, rest) = self
            .rest
            .split_first_chunk()
            .ok_or(Error::Truncated { at })?;
        self.rest = rest;
        usize::try_from(u64::from_le_bytes(bytes))
            .ok()
            .filter(|n| *n <= self.rest.len())
            .ok_or(Error::Truncated { at })
    }

    fn patterns(&mut self) -> Result<Patterns, Error> {
        let at = self.at();
        let n = self.count()?;
        let mut texts = Vec::with_capacity(n);
        for _ in 0..n {
            let len = self.count()?;
            let start = self.at();
            let bytes = self.take(len)?;
            let text = str::from_utf8(bytes).map_err(|e| Error::Utf8 {
                at: start.saturating_add(e.valid_up_to()),
            })?;
            texts.push(text);
        }
        Patterns::new(texts).map_err(|error| Error::Pattern { at, error })
    }

    fn access(&mut self) -> Result<Policy, Error> {
        let subjects = self.patterns()?;
        let select = self.patterns()?;
        let at = self.at();
        let bits = self.byte()?;
        let allow = Actions::from_bits(bits).ok_or(Error::Actions { at, bits })?;
        let at = self.at();
        let authority = Authority(self.byte()?);
        if authority.0 != 0 && !allow.contains(Action::Write) {
            return Err(Error::Authority {
                at,
                found: authority,
            });
        }
        Ok(Policy::new(subjects, select, allow, authority))
    }
}

/// The patterns of a selector as the file wrote them, in order, and the selector they
/// read as. Two lists of patterns that select the same names are still two values.
#[derive(Clone, PartialEq, Eq)]
pub struct Patterns {
    texts: Box<[Box<str>]>,
    selector: Selector,
}

impl Patterns {
    /// Reads the patterns. A pattern with a leading `!` excludes names.
    ///
    /// # Errors
    ///
    /// The error of [`Selector::new`] when the patterns do not read as a selector.
    pub fn new<'a>(
        texts: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, name::Error> {
        let texts = texts
            .into_iter()
            .map(Box::from)
            .collect::<Box<[Box<str>]>>();
        let selector = Selector::new(texts.iter().map(|t| &**t))?;
        Ok(Self { texts, selector })
    }

    /// The selector the patterns read as.
    #[must_use]
    pub const fn selector(&self) -> &Selector {
        &self.selector
    }

    /// The patterns as written, in order.
    #[must_use]
    pub fn texts(&self) -> impl ExactSizeIterator<Item = &str> {
        self.texts.iter().map(|t| &**t)
    }
}

impl fmt::Debug for Patterns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.texts()).finish()
    }
}

/// Bytes that are not the encoding of a definition. `at` is a byte offset into the
/// bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The bytes have a format version newer than this build reads.
    Newer {
        /// The version in the bytes.
        found: u8,
    },
    /// The bytes have a format version that does not exist.
    Version {
        /// The version in the bytes.
        found: u8,
    },
    /// The bytes end before the definition does, or a count is larger than the bytes
    /// left.
    Truncated {
        /// Where the part that runs past the end starts.
        at: usize,
    },
    /// Bytes follow the end of the definition.
    TrailingBytes {
        /// Where the extra bytes start.
        at: usize,
    },
    /// The tag names no kind of definition.
    Kind {
        /// Where the tag is.
        at: usize,
        /// The tag.
        tag: u8,
    },
    /// A pattern is not UTF-8.
    Utf8 {
        /// The first byte that is not UTF-8.
        at: usize,
    },
    /// The patterns do not read as a selector.
    Pattern {
        /// Where the patterns start.
        at: usize,
        /// Why they do not read.
        error: name::Error,
    },
    /// An action set has a bit that names no action.
    Actions {
        /// Where the set is.
        at: usize,
        /// The set's bits.
        bits: u8,
    },
    /// An access policy that does not allow `write` has an authority.
    Authority {
        /// Where the authority is.
        at: usize,
        /// The authority.
        found: Authority,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Newer { found } => write!(
                f,
                "the definition has format version {found}, newer than {VERSION}"
            ),
            Self::Version { found } => {
                write!(
                    f,
                    "the definition has format version {found}, which does not exist"
                )
            }
            Self::Truncated { at } => {
                write!(f, "the definition ends early at byte {at}")
            }
            Self::TrailingBytes { at } => {
                write!(f, "bytes follow the definition at byte {at}")
            }
            Self::Kind { at, tag } => {
                write!(f, "tag {tag} at byte {at} names no kind of definition")
            }
            Self::Utf8 { at } => write!(f, "a pattern is not UTF-8 at byte {at}"),
            Self::Pattern { at, error } => {
                write!(f, "the patterns at byte {at} do not read: {error}")
            }
            Self::Actions { at, bits } => {
                write!(f, "the actions {bits:#010b} at byte {at} name no action")
            }
            Self::Authority { at, found } => write!(
                f,
                "authority {found} at byte {at} is on a policy that does not allow write"
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
