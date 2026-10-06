//! The definitions the spec tree stores, one per name, and their canonical bytes.
//! Each definition has exactly one encoding, and [`Definition::decode`] refuses every
//! byte string that [`Definition::encode`] cannot write.
//!
//! Format, with every integer little-endian:
//!
//! ```text
//! definition := version:u8 tag:u8 body
//! access     := subjects:patterns select:patterns allow:u8 authority:u8   tag 1
//! patterns   := count:u64 pattern*
//! pattern    := excluded:u8 length:u64 UTF-8 bytes
//! ```
//!
//! `excluded` is 1 for an exclusion, which a file writes with a leading `!`, and 0
//! otherwise. The stored text has no `!`.
//!
//! `allow` holds one bit per action: read 0, write 1, plan 2, apply 3, secret 4, and
//! admin 5. `authority` is zero when `allow` does not hold write.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

use std::{fmt, str};

use types::authority::Authority;
use types::name::{self, Patterns};

use crate::access::{Action, Actions, Policy};

const VERSION: u8 = 1;
const ACCESS: u8 = 1;
/// The fewest bytes a pattern takes: its flag and its length.
const PATTERN_MIN: usize = 9;

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
    count(out, patterns.texts().len());
    for text in patterns.texts() {
        let (excluded, body) = text.strip_prefix('!').map_or((0, text), |b| (1, b));
        out.push(excluded);
        count(out, body.len());
        out.extend_from_slice(body.as_bytes());
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
        self.len
            .checked_sub(self.rest.len())
            .expect("invariant: the bytes left are a suffix of the input")
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

    /// Reads a count of items that take at least `size` bytes each. A count whose
    /// items cannot fit in the bytes left is refused before anything is allocated.
    fn count(&mut self, size: usize) -> Result<usize, Error> {
        let at = self.at();
        let (&bytes, rest) = self
            .rest
            .split_first_chunk()
            .ok_or(Error::Truncated { at })?;
        self.rest = rest;
        usize::try_from(u64::from_le_bytes(bytes))
            .ok()
            .filter(|n| n.checked_mul(size).is_some_and(|b| b <= self.rest.len()))
            .ok_or(Error::Truncated { at })
    }

    fn patterns(&mut self) -> Result<Patterns, Error> {
        let at = self.at();
        let n = self.count(PATTERN_MIN)?;
        let mut texts = Vec::with_capacity(n);
        for _ in 0..n {
            let flag = self.at();
            let excluded = match self.byte()? {
                0 => false,
                1 => true,
                found => return Err(Error::Excluded { at: flag, found }),
            };
            let len = self.count(1)?;
            let start = self.at();
            let bytes = self.take(len)?;
            let text = str::from_utf8(bytes).map_err(|e| Error::Utf8 {
                at: start
                    .checked_add(e.valid_up_to())
                    .expect("invariant: an offset into the input fits in usize"),
            })?;
            if excluded {
                texts.push(format!("!{text}"));
            } else if text.starts_with('!') {
                return Err(Error::Include { at: start });
            } else {
                texts.push(text.to_owned());
            }
        }
        Patterns::new(texts.iter().map(|t| &**t))
            .map_err(|error| Error::Pattern { at, error })
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

/// Bytes that are not the encoding of a definition. `at` is a byte offset into the
/// bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The bytes have a format version newer than this build reads. A newer node
    /// wrote them; update this node.
    Newer {
        /// The version in the bytes.
        found: u8,
    },
    /// The bytes have a format version older than any that exists, so no node wrote
    /// them: they are corrupt.
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
    /// A pattern's exclusion flag is not 0 or 1.
    Excluded {
        /// Where the flag is.
        at: usize,
        /// The flag.
        found: u8,
    },
    /// A pattern that is not an exclusion starts with `!`.
    Include {
        /// Where the text starts.
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
            Self::Excluded { at, found } => {
                write!(f, "the exclusion flag {found} at byte {at} is not 0 or 1")
            }
            Self::Include { at } => {
                write!(f, "the included pattern at byte {at} starts with `!`")
            }
            Self::Pattern { at, error } => {
                write!(f, "the patterns at byte {at} do not read: {error}")
            }
            Self::Actions { at, bits } => {
                write!(f, "the actions {bits:#010b} at byte {at} name no action")
            }
            Self::Authority { at, found } => write!(
                f,
                "authority {found} at byte {at} is on a policy without write"
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
