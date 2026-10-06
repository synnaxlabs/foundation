//! The definitions the spec tree stores, one per name, and their canonical bytes.
//! Each definition has exactly one encoding, and [`Definition::decode`] refuses every
//! byte string that [`Definition::encode`] cannot write.
//!
//! Format, with every integer little-endian:
//!
//! ```text
//! definition := version:u8 tag:u8 body
//! access     := subjects:patterns select:patterns allow:u8 authority:u8   tag 1
//! connector  := kind:text node:text length:u64 document                  tag 2
//! region     := epoch:u64 count:u64 text*                                tag 3
//! patterns   := count:u64 pattern*
//! pattern    := excluded:u8 length:u64 UTF-8 bytes
//! text       := length:u64 UTF-8 bytes
//! ```
//!
//! A `text` is a name. `document` is the canonical encoding of the connector config.
//! The voters of a region are in strict name order, and there is at least one.
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

use document::encoding;
use types::authority::Authority;
use types::name::{self, Name};

use crate::access::{Action, Actions, Policy};
use crate::connector::Connector;
use crate::patterns::Patterns;
use crate::region::{Delegation, NoVoters};

const VERSION: u8 = 1;
const ACCESS: u8 = 1;
const CONNECTOR: u8 = 2;
const REGION: u8 = 3;
/// The fewest bytes a text takes: its length.
const TEXT_MIN: usize = 8;
/// The fewest bytes a pattern takes: its flag and its length.
const PATTERN_MIN: usize = 9;

/// One definition: the value of one name in the spec tree. The name is the tree key,
/// so it is not part of the definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Definition {
    /// An access policy.
    Access(Policy),
    /// A connector.
    Connector(Connector),
    /// The record of a child region, in its parent's tree.
    Region(Delegation),
}

impl Definition {
    /// Writes the canonical bytes of the definition.
    #[must_use]
    #[expect(
        clippy::missing_panics_doc,
        reason = "`Connector::new` refuses a config with no encoding"
    )]
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
            Self::Connector(connector) => {
                out.push(CONNECTOR);
                text(&mut out, connector.kind().as_str());
                text(&mut out, connector.node().as_str());
                let config = encoding::encode(connector.config())
                    .expect("invariant: a connector's config has an encoding");
                count(&mut out, config.len());
                out.extend_from_slice(&config);
            }
            Self::Region(delegation) => {
                out.push(REGION);
                out.extend_from_slice(&delegation.epoch().to_le_bytes());
                count(&mut out, delegation.initial_voters().len());
                for voter in delegation.initial_voters() {
                    text(&mut out, voter.as_str());
                }
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
            CONNECTOR => Self::Connector(reader.connector()?),
            REGION => Self::Region(reader.region()?),
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

fn text(out: &mut Vec<u8>, text: &str) {
    count(out, text.len());
    out.extend_from_slice(text.as_bytes());
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

    fn u64(&mut self) -> Result<u64, Error> {
        let at = self.at();
        let (&bytes, rest) = self
            .rest
            .split_first_chunk()
            .ok_or(Error::Truncated { at })?;
        self.rest = rest;
        Ok(u64::from_le_bytes(bytes))
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
        usize::try_from(self.u64()?)
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
            let (start, text) = self.text()?;
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

    /// Reads a length and that many bytes of UTF-8. Returns where the bytes start.
    fn text(&mut self) -> Result<(usize, &'a str), Error> {
        let len = self.count(1)?;
        let start = self.at();
        let text = str::from_utf8(self.take(len)?).map_err(|e| Error::Utf8 {
            at: start
                .checked_add(e.valid_up_to())
                .expect("invariant: an offset into the input fits in usize"),
        })?;
        Ok((start, text))
    }

    fn name(&mut self) -> Result<Name, Error> {
        let at = self.at();
        self.text()?
            .1
            .parse()
            .map_err(|error| Error::Name { at, error })
    }

    #[expect(
        clippy::unwrap_in_result,
        reason = "`decode` refuses a document that nests deeper than `encode` writes"
    )]
    fn connector(&mut self) -> Result<Connector, Error> {
        let kind = self.name()?;
        let node = self.name()?;
        let len = self.count(1)?;
        let at = self.at();
        let config = encoding::decode(self.take(len)?)
            .map_err(|error| Error::Config { at, error })?;
        Ok(Connector::new(kind, node, config)
            .expect("invariant: a decoded document has an encoding"))
    }

    fn region(&mut self) -> Result<Delegation, Error> {
        let epoch = self.u64()?;
        let at = self.at();
        let n = self.count(TEXT_MIN)?;
        let mut voters = Vec::with_capacity(n);
        for _ in 0..n {
            let at = self.at();
            let voter = self.name()?;
            if voters.last().is_some_and(|last| *last >= voter) {
                return Err(Error::Order { at });
            }
            voters.push(voter);
        }
        Delegation::new(epoch, voters).map_err(|NoVoters| Error::NoVoters { at })
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
    /// A pattern or a name is not UTF-8.
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
    /// A name does not read.
    Name {
        /// Where the name's length is.
        at: usize,
        /// Why it does not read.
        error: name::Error,
    },
    /// A connector's config is not the encoding of a document.
    Config {
        /// Where the config starts. The offset in `error` counts from here.
        at: usize,
        /// Why it is not a document.
        error: encoding::Error,
    },
    /// A region's voters are not in strict name order.
    Order {
        /// Where the voter that is out of order is.
        at: usize,
    },
    /// A region has no voter.
    NoVoters {
        /// Where the count of voters is.
        at: usize,
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
            Self::Utf8 { at } => write!(f, "a text is not UTF-8 at byte {at}"),
            Self::Excluded { at, found } => {
                write!(f, "the exclusion flag {found} at byte {at} is not 0 or 1")
            }
            Self::Include { at } => {
                write!(f, "the included pattern at byte {at} starts with `!`")
            }
            Self::Pattern { at, error } => {
                write!(f, "the patterns at byte {at} do not read: {error}")
            }
            Self::Name { at, error } => {
                write!(f, "the name at byte {at} does not read: {error}")
            }
            Self::Config { at, error } => {
                write!(
                    f,
                    "the connector config at byte {at} does not read: {error}"
                )
            }
            Self::Order { at } => {
                write!(f, "the voter at byte {at} is not after the voter before it")
            }
            Self::NoVoters { at } => write!(f, "the region at byte {at} has no voter"),
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
