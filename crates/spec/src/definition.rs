//! The definitions the spec tree stores, one per name, and their canonical bytes.
//! Each definition has exactly one encoding, and [`Definition::decode`] refuses every
//! byte string that [`Definition::encode`] cannot write.
//!
//! Format, with every integer little-endian:
//!
//! ```text
//! definition    := version:u8 tag:u8 body
//! access        := subjects:patterns select:patterns allow:u8 authority:u8   tag 1
//! connector     := kind:text node:text length:u64 document                  tag 2
//! region        := epoch:u64 count:u64 text*                                tag 3
//! node_settings := select:patterns disk:u64 pool:u64                       tag 4
//! compression   := select:patterns mode:u8                                  tag 5
//! placement     := select:patterns home:optional standby:optional           tag 6
//!                  copies:names
//! time          := select:patterns peers:optional_names                     tag 7
//! channel       := key kind                                                 tag 8
//! kind          := 0 error:optional_key control:optional_key                index
//!                | 1 index:key quality:optional_key data_type unit:optional  data
//! data_type     := 0 scalar:u8 | 1 scalar:u8 len:u32 | 2 scalar:u8 max:u32 | 3 | 4 | 5
//! patterns      := count:u64 pattern*
//! pattern       := excluded:u8 length:u64 UTF-8 bytes
//! text          := length:u64 UTF-8 bytes
//! names         := count:u64 text*
//! optional      := 0 | 1 text
//! optional_names := 0 | 1 names
//! key           := u128
//! optional_key  := 0 | 1 key
//! ```
//!
//! A `text` is a name, except a `unit`, which is the text of a [`Unit`]. `document`
//! is the canonical encoding of the connector config. The `names` of a list are in
//! strict name order. A region has at least one voter.
//!
//! `excluded` is 1 for an exclusion, which a file writes with a leading `!`, and 0
//! otherwise. The stored text has no `!`. Patterns keep the order and form written,
//! so a rewrite that matches the same names still changes the bytes, and `plan` shows
//! it.
//!
//! `allow` holds one bit per action: read 0, write 1, plan 2, apply 3, secret 4, and
//! admin 5. `authority` is zero when `allow` does not hold write.
//!
//! A node settings budget of 0 bytes is no budget, because a policy cannot hold zero.
//! A policy sets at least one budget.
//!
//! A compression `mode` is 0 auto, 1 raw, or 2 max.
//!
//! A `data_type` is a scalar, an array, a list, a string, bytes, or quality, in that
//! order from 0. A `scalar` is bool 0, i8 1, i16 2, i32 3, i64 4, u8 5, u16 6, u32 7,
//! u64 8, f32 9, f64 10, stamp 11, span 12, or uuid 13. Only a scalar from 1 to 10,
//! or an array or list of one, has a unit.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

use std::{fmt, str};

use document::encoding;
use types::authority::Authority;
use types::byte;
use types::channel::Key;
use types::name::{self, Name, Selector, Written};
use types::sample::{self, Scalar};

use crate::access::{Action, Actions, Policy};
use crate::channel::{self, Channel, Data, DataType};
use crate::compression::{self, Mode};
use crate::connector::Connector;
use crate::node_settings;
use crate::placement;
use crate::region::{Delegation, NoVoters};
use crate::time;
use crate::unit::{self, Unit};

const VERSION: u8 = 1;
const ACCESS: u8 = 1;
const CONNECTOR: u8 = 2;
const REGION: u8 = 3;
const NODE_SETTINGS: u8 = 4;
const COMPRESSION: u8 = 5;
const PLACEMENT: u8 = 6;
const TIME: u8 = 7;
const CHANNEL: u8 = 8;
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
    /// A node settings policy.
    NodeSettings(node_settings::Policy),
    /// A compression policy.
    Compression(compression::Policy),
    /// A placement policy.
    Placement(placement::Policy),
    /// A time policy.
    Time(time::Policy),
    /// A channel.
    Channel(Channel),
}

/// The kind of a definition. Its tree key names it, except for a connector or a
/// channel, which is at its own name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// An access policy.
    Access,
    /// A connector.
    Connector,
    /// A channel.
    Channel,
    /// The record of a child region.
    Region,
    /// A node settings policy.
    NodeSettings,
    /// A compression policy.
    Compression,
    /// A placement policy.
    Placement,
    /// A time policy.
    Time,
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
            Self::Connector(connector) => {
                out.push(CONNECTOR);
                text(&mut out, connector.kind().as_str());
                text(&mut out, connector.node().as_str());
                let config = connector.config().encode();
                count(&mut out, config.len());
                out.extend_from_slice(&config);
            }
            Self::Region(delegation) => {
                out.push(REGION);
                out.extend_from_slice(&delegation.epoch().to_le_bytes());
                names(&mut out, delegation.initial_voters());
            }
            Self::NodeSettings(policy) => {
                out.push(NODE_SETTINGS);
                patterns(&mut out, policy.select());
                for budget in [policy.disk(), policy.pool()] {
                    let bytes = budget.map_or(0, byte::Size::bytes);
                    out.extend_from_slice(&bytes.to_le_bytes());
                }
            }
            Self::Compression(policy) => {
                out.push(COMPRESSION);
                patterns(&mut out, &policy.select);
                out.push(mode_byte(policy.mode));
            }
            Self::Placement(policy) => {
                out.push(PLACEMENT);
                patterns(&mut out, policy.select());
                optional(&mut out, policy.home().map(Name::as_str));
                optional(&mut out, policy.standby().map(Name::as_str));
                names(&mut out, policy.copies());
            }
            Self::Time(policy) => {
                out.push(TIME);
                patterns(&mut out, policy.select());
                match policy.peers() {
                    time::Peers::Voters => out.push(0),
                    time::Peers::Listed(peers) => {
                        out.push(1);
                        names(&mut out, peers);
                    }
                }
            }
            Self::Channel(definition) => {
                out.push(CHANNEL);
                channel(&mut out, definition);
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
            NODE_SETTINGS => Self::NodeSettings(reader.node_settings()?),
            COMPRESSION => Self::Compression(reader.compression()?),
            PLACEMENT => Self::Placement(reader.placement()?),
            TIME => Self::Time(reader.time()?),
            CHANNEL => Self::Channel(reader.channel()?),
            tag => return Err(Error::Kind { at, tag }),
        };
        if !reader.rest.is_empty() {
            return Err(Error::TrailingBytes { at: reader.at() });
        }
        Ok(definition)
    }
}

/// Every compression mode, for decoding by [`mode_byte`].
const MODES: [Mode; 3] = [Mode::Auto, Mode::Raw, Mode::Max];

/// The byte that encodes `mode`.
const fn mode_byte(mode: Mode) -> u8 {
    match mode {
        Mode::Auto => 0,
        Mode::Raw => 1,
        Mode::Max => 2,
    }
}

const SCALAR: u8 = 0;
const ARRAY: u8 = 1;
const LIST: u8 = 2;
const STRING: u8 = 3;
const BYTES: u8 = 4;
const QUALITY: u8 = 5;

/// The code of a scalar. [`scalar`] is its inverse.
const fn code(scalar: Scalar) -> u8 {
    match scalar {
        Scalar::Bool => 0,
        Scalar::I8 => 1,
        Scalar::I16 => 2,
        Scalar::I32 => 3,
        Scalar::I64 => 4,
        Scalar::U8 => 5,
        Scalar::U16 => 6,
        Scalar::U32 => 7,
        Scalar::U64 => 8,
        Scalar::F32 => 9,
        Scalar::F64 => 10,
        Scalar::Stamp => 11,
        Scalar::Span => 12,
        Scalar::Uuid => 13,
    }
}

/// The scalar with the code `code`, if any. [`code`] is its inverse.
const fn scalar(code: u8) -> Option<Scalar> {
    Some(match code {
        0 => Scalar::Bool,
        1 => Scalar::I8,
        2 => Scalar::I16,
        3 => Scalar::I32,
        4 => Scalar::I64,
        5 => Scalar::U8,
        6 => Scalar::U16,
        7 => Scalar::U32,
        8 => Scalar::U64,
        9 => Scalar::F32,
        10 => Scalar::F64,
        11 => Scalar::Stamp,
        12 => Scalar::Span,
        13 => Scalar::Uuid,
        _ => return None,
    })
}

fn data_type(out: &mut Vec<u8>, data_type: &DataType) {
    match *data_type {
        DataType::Sample(sample::Type::Scalar(element)) => {
            out.extend_from_slice(&[SCALAR, code(element)]);
        }
        DataType::Sample(sample::Type::Array { element, len }) => {
            out.extend_from_slice(&[ARRAY, code(element)]);
            out.extend_from_slice(&len.to_le_bytes());
        }
        DataType::Sample(sample::Type::List { element, max }) => {
            out.extend_from_slice(&[LIST, code(element)]);
            out.extend_from_slice(&max.to_le_bytes());
        }
        DataType::Sample(sample::Type::String) => out.push(STRING),
        DataType::Sample(sample::Type::Bytes) => out.push(BYTES),
        DataType::Quality => out.push(QUALITY),
    }
}

fn channel(out: &mut Vec<u8>, definition: &Channel) {
    key(out, definition.key);
    match &definition.kind {
        channel::Kind::Index { error, control } => {
            out.push(0);
            optional_key(out, *error);
            optional_key(out, *control);
        }
        channel::Kind::Data(data) => {
            out.push(1);
            key(out, *data.index());
            optional_key(out, data.quality().copied());
            data_type(out, data.data_type());
            optional(out, data.unit().map(Unit::as_str));
        }
    }
}

fn key(out: &mut Vec<u8>, key: Key) {
    out.extend_from_slice(&key.as_u128().to_le_bytes());
}

fn optional_key(out: &mut Vec<u8>, found: Option<Key>) {
    match found {
        None => out.push(0),
        Some(found) => {
            out.push(1);
            key(out, found);
        }
    }
}

fn patterns(out: &mut Vec<u8>, selector: &Selector) {
    count(out, selector.written().len());
    for pattern in selector.written() {
        let (excluded, body) = match pattern {
            Written::Include(body) => (0, body),
            Written::Exclude(body) => (1, body),
        };
        out.push(excluded);
        count(out, body.len());
        out.extend_from_slice(body.as_bytes());
    }
}

fn optional(out: &mut Vec<u8>, found: Option<&str>) {
    match found {
        None => out.push(0),
        Some(found) => {
            out.push(1);
            text(out, found);
        }
    }
}

fn names(out: &mut Vec<u8>, names: &[Name]) {
    count(out, names.len());
    for name in names {
        text(out, name.as_str());
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

    fn u32(&mut self) -> Result<u32, Error> {
        let at = self.at();
        let (&bytes, rest) = self
            .rest
            .split_first_chunk()
            .ok_or(Error::Truncated { at })?;
        self.rest = rest;
        Ok(u32::from_le_bytes(bytes))
    }

    fn key(&mut self) -> Result<Key, Error> {
        let at = self.at();
        let (&bytes, rest) = self
            .rest
            .split_first_chunk()
            .ok_or(Error::Truncated { at })?;
        self.rest = rest;
        Ok(Key::from_u128(u128::from_le_bytes(bytes)))
    }

    fn optional_key(&mut self) -> Result<Option<Key>, Error> {
        Ok(if self.flag()? {
            Some(self.key()?)
        } else {
            None
        })
    }

    fn byte(&mut self) -> Result<u8, Error> {
        let at = self.at();
        let (&byte, rest) = self.rest.split_first().ok_or(Error::Truncated { at })?;
        self.rest = rest;
        Ok(byte)
    }

    fn flag(&mut self) -> Result<bool, Error> {
        let at = self.at();
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            found => Err(Error::Flag { at, found }),
        }
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

    fn patterns(&mut self) -> Result<Selector, Error> {
        let at = self.at();
        let n = self.count(PATTERN_MIN)?;
        let mut written = Vec::with_capacity(n);
        for _ in 0..n {
            let excluded = self.flag()?;
            let text = self.text()?;
            written.push(if excluded {
                Written::Exclude(text)
            } else {
                Written::Include(text)
            });
        }
        Selector::from_written(written).map_err(|error| Error::Pattern { at, error })
    }

    /// Reads a length and that many bytes of UTF-8.
    fn text(&mut self) -> Result<&'a str, Error> {
        let len = self.count(1)?;
        let start = self.at();
        let text = str::from_utf8(self.take(len)?).map_err(|e| Error::Utf8 {
            at: start
                .checked_add(e.valid_up_to())
                .expect("invariant: an offset into the input fits in usize"),
        })?;
        Ok(text)
    }

    fn name(&mut self) -> Result<Name, Error> {
        let at = self.at();
        self.text()?
            .parse()
            .map_err(|error| Error::Name { at, error })
    }

    fn optional_name(&mut self) -> Result<Option<Name>, Error> {
        if self.flag()? {
            self.name().map(Some)
        } else {
            Ok(None)
        }
    }

    /// Reads a list of names in strict name order.
    fn names(&mut self) -> Result<Vec<Name>, Error> {
        let n = self.count(TEXT_MIN)?;
        let mut names = Vec::with_capacity(n);
        for _ in 0..n {
            let at = self.at();
            let name = self.name()?;
            if names.last().is_some_and(|last| *last >= name) {
                return Err(Error::Order { at });
            }
            names.push(name);
        }
        Ok(names)
    }

    fn connector(&mut self) -> Result<Connector, Error> {
        let kind = self.name()?;
        let node = self.name()?;
        let len = self.count(1)?;
        let at = self.at();
        let config = encoding::decode(self.take(len)?)
            .map_err(|error| Error::Config { at, error })?;
        Ok(Connector::new(kind, node, config))
    }

    fn region(&mut self) -> Result<Delegation, Error> {
        let epoch = self.u64()?;
        let at = self.at();
        let voters = self.names()?;
        Delegation::new(epoch, voters).map_err(|NoVoters| Error::NoVoters { at })
    }

    fn node_settings(&mut self) -> Result<node_settings::Policy, Error> {
        let select = self.patterns()?;
        let at = self.at();
        let mut budget = || {
            self.u64()
                .map(|bytes| (bytes != 0).then_some(byte::Size::from_bytes(bytes)))
        };
        let disk = budget()?;
        let pool = budget()?;
        node_settings::Policy::new(select, disk, pool)
            .map_err(|error| Error::Budget { at, error })
    }

    fn compression(&mut self) -> Result<compression::Policy, Error> {
        let select = self.patterns()?;
        let at = self.at();
        let found = self.byte()?;
        let mode = MODES
            .into_iter()
            .find(|mode| mode_byte(*mode) == found)
            .ok_or(Error::Mode { at, found })?;
        Ok(compression::Policy { select, mode })
    }

    fn placement(&mut self) -> Result<placement::Policy, Error> {
        let select = self.patterns()?;
        let at = self.at();
        let nodes = placement::Nodes {
            home: self.optional_name()?,
            standby: self.optional_name()?,
            copies: self.names()?,
        };
        placement::Policy::new(select, nodes)
            .map_err(|error| Error::Placement { at, error })
    }

    fn time(&mut self) -> Result<time::Policy, Error> {
        let select = self.patterns()?;
        let peers = if self.flag()? {
            time::Peers::Listed(self.names()?)
        } else {
            time::Peers::Voters
        };
        Ok(time::Policy::new(select, peers))
    }

    fn channel(&mut self) -> Result<Channel, Error> {
        let key = self.key()?;
        let at = self.at();
        let kind = match self.byte()? {
            0 => channel::Kind::Index {
                error: self.optional_key()?,
                control: self.optional_key()?,
            },
            1 => channel::Kind::Data(self.data()?),
            found => return Err(Error::ChannelKind { at, found }),
        };
        Ok(Channel { key, kind })
    }

    fn data(&mut self) -> Result<Data, Error> {
        let index = self.key()?;
        let quality = self.optional_key()?;
        let at = self.at();
        let data_type = self.data_type()?;
        let unit = if self.flag()? {
            let at = self.at();
            Some(Unit::new(self.text()?).map_err(|error| Error::Unit { at, error })?)
        } else {
            None
        };
        Data::new(index, quality, data_type, unit)
            .map_err(|error| Error::Channel { at, error })
    }

    fn data_type(&mut self) -> Result<DataType, Error> {
        let at = self.at();
        let sample = match self.byte()? {
            SCALAR => sample::Type::Scalar(self.scalar()?),
            ARRAY => sample::Type::Array {
                element: self.scalar()?,
                len: self.u32()?,
            },
            LIST => sample::Type::List {
                element: self.scalar()?,
                max: self.u32()?,
            },
            STRING => sample::Type::String,
            BYTES => sample::Type::Bytes,
            QUALITY => return Ok(DataType::Quality),
            found => return Err(Error::DataType { at, found }),
        };
        Ok(DataType::Sample(sample))
    }

    fn scalar(&mut self) -> Result<Scalar, Error> {
        let at = self.at();
        let found = self.byte()?;
        scalar(found).ok_or(Error::Scalar { at, found })
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
    /// A pattern's exclusion flag or a presence flag is not 0 or 1.
    Flag {
        /// Where the flag is.
        at: usize,
        /// The flag.
        found: u8,
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
    /// A list of names is not in strict name order.
    Order {
        /// Where the name that is out of order is.
        at: usize,
    },
    /// A region has no voter.
    NoVoters {
        /// Where the count of voters is.
        at: usize,
    },
    /// A compression mode byte names no mode.
    Mode {
        /// Where the mode is.
        at: usize,
        /// The mode byte.
        found: u8,
    },
    /// An access policy that does not allow `write` has an authority.
    Authority {
        /// Where the authority is.
        at: usize,
        /// The authority.
        found: Authority,
    },
    /// The budgets of a node settings policy make no policy. A zero budget reads as
    /// no budget, so `error` is always [`node_settings::Error::NoBudget`].
    Budget {
        /// Where the budgets start.
        at: usize,
        /// Why they make no policy.
        error: node_settings::Error,
    },
    /// A placement's nodes make no policy.
    Placement {
        /// Where the home presence flag is.
        at: usize,
        /// Why they make no policy.
        error: placement::Error,
    },
    /// A channel kind byte is not 0 (index) or 1 (data).
    ChannelKind {
        /// Where the byte is.
        at: usize,
        /// The byte.
        found: u8,
    },
    /// A data type byte names no data type.
    DataType {
        /// Where the byte is.
        at: usize,
        /// The byte.
        found: u8,
    },
    /// A scalar byte names no scalar.
    Scalar {
        /// Where the byte is.
        at: usize,
        /// The byte.
        found: u8,
    },
    /// A unit does not read.
    Unit {
        /// Where the unit's length is.
        at: usize,
        /// Why it does not read.
        error: unit::Error,
    },
    /// A data channel cannot exist.
    Channel {
        /// Where the data type is.
        at: usize,
        /// Why it cannot exist.
        error: channel::Error,
    },
}

impl fmt::Display for Error {
    #[expect(clippy::too_many_lines, reason = "one arm for each error")]
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
            Self::Flag { at, found } => {
                write!(f, "the flag {found} at byte {at} is not 0 or 1")
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
                write!(f, "the name at byte {at} is not after the name before it")
            }
            Self::NoVoters { at } => write!(f, "the region at byte {at} has no voter"),
            Self::Actions { at, bits } => {
                write!(f, "the actions {bits:#010b} at byte {at} name no action")
            }
            Self::Authority { at, found } => write!(
                f,
                "authority {found} at byte {at} is on a policy without write"
            ),
            Self::Budget { at, error } => {
                write!(f, "the budgets at byte {at}: {error}")
            }
            Self::Placement { at, error } => {
                write!(f, "the placement at byte {at}: {error}")
            }
            Self::Mode { at, found } => {
                write!(
                    f,
                    "compression mode {found} at byte {at} is not a known mode"
                )
            }
            Self::ChannelKind { at, found } => {
                write!(f, "the channel kind {found} at byte {at} is not 0 or 1")
            }
            Self::DataType { at, found } => {
                write!(f, "the data type {found} at byte {at} is not a known type")
            }
            Self::Scalar { at, found } => {
                write!(f, "the scalar {found} at byte {at} is not a known scalar")
            }
            Self::Unit { at, error } => {
                write!(f, "the unit at byte {at} does not read: {error}")
            }
            Self::Channel { at, error } => {
                write!(f, "the data channel at byte {at}: {error}")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
