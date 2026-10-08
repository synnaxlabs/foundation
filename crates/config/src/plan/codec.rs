//! The canonical bytes of a [`Plan`]: the plan file.

use std::collections::BTreeMap;
use std::fmt;

use spec::channel::{Data, Kind};
use spec::data_type::DataType;
use spec::definition;
use spec::unit::Unit;
use types::digest::Digest;
use types::name::Name;

use super::{Change, Plan};
use crate::{Definition, Entry};

/// The format version that this build writes and reads.
const VERSION: u8 = 1;

const SPEC: u8 = 0;
const CHANNEL: u8 = 1;
const INDEX: u8 = 0;
const DATA: u8 = 1;

/// Why bytes are not a plan.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Another build made the plan, with format version `found`. Plan again.
    Version {
        /// The format version in the bytes.
        found: u8,
    },
    /// The bytes at `at` are not what [`Plan::encode`] writes there.
    Malformed {
        /// The offset of the first wrong byte, or of the field that the bytes cut.
        at: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Version { found } => write!(
                f,
                "the plan has format version {found}, and this build reads only \
                 version {VERSION}; plan again with this build"
            ),
            Self::Malformed { at } => {
                write!(f, "the bytes are not a plan, from byte {at}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl Plan {
    /// The canonical bytes of the plan, which start with the plan format version. The
    /// bytes hold no span, so [`Plan::decode`] gives each `label_span` as `None`.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        out.extend_from_slice(&self.base.version.to_le_bytes());
        out.extend_from_slice(&self.base.root.0);
        count(&mut out, self.changes.len());
        for change in &self.changes {
            text(&mut out, change.name.as_str());
            match change.old {
                None => out.push(0),
                Some(old) => {
                    out.push(1);
                    out.extend_from_slice(&old.0);
                }
            }
            match &change.new {
                None => out.push(0),
                Some(entry) => {
                    out.push(1);
                    definition(&mut out, &entry.definition);
                }
            }
        }
        count(&mut out, self.homes.len());
        for (index, home) in &self.homes {
            text(&mut out, index.as_str());
            text(&mut out, home.as_str());
        }
        out
    }

    /// Reads the bytes of [`Plan::encode`]. Never panics: the bytes come from a user.
    /// A plan that it reads encodes to the same bytes.
    ///
    /// # Errors
    ///
    /// - [`Error::Version`] when the format version is not the one this build writes.
    /// - [`Error::Malformed`] at the first byte that [`Plan::encode`] does not write.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let mut reader = Reader { bytes, at: 0 };
        let version = reader.byte()?;
        if version != VERSION {
            return Err(Error::Version { found: version });
        }
        let base = spec::Pointer {
            version: u64::from_le_bytes(reader.array()?),
            root: Digest(reader.array()?),
        };
        let mut changes: Vec<Change> = Vec::new();
        for _ in 0..reader.count()? {
            let at = reader.at;
            let name = reader.name()?;
            if changes.last().is_some_and(|last| last.name >= name) {
                return Err(Error::Malformed { at });
            }
            let old = reader
                .flag()?
                .then(|| reader.array().map(Digest))
                .transpose()?;
            let at = reader.at;
            let new = reader.flag()?.then(|| reader.entry()).transpose()?;
            if old.is_none() && new.is_none() {
                return Err(Error::Malformed { at });
            }
            changes.push(Change { name, old, new });
        }
        let mut homes = BTreeMap::new();
        for _ in 0..reader.count()? {
            let at = reader.at;
            let index = reader.name()?;
            if homes
                .last_key_value()
                .is_some_and(|(last, _)| *last >= index)
            {
                return Err(Error::Malformed { at });
            }
            homes.insert(index, reader.name()?);
        }
        if reader.at != bytes.len() {
            return Err(Error::Malformed { at: reader.at });
        }
        Ok(Plan {
            base,
            changes,
            homes,
        })
    }
}

fn definition(out: &mut Vec<u8>, definition: &Definition) {
    match definition {
        Definition::Spec(definition) => {
            out.push(SPEC);
            let bytes = definition.encode();
            count(out, bytes.len());
            out.extend_from_slice(&bytes);
        }
        Definition::Channel(Kind::Index { error, control }) => {
            out.extend_from_slice(&[CHANNEL, INDEX]);
            optional(out, error.as_ref().map(Name::as_str));
            optional(out, control.as_ref().map(Name::as_str));
        }
        Definition::Channel(Kind::Data(data)) => {
            out.extend_from_slice(&[CHANNEL, DATA]);
            text(out, data.index().as_str());
            optional(out, data.quality().map(Name::as_str));
            text(out, &data.data_type().to_string());
            optional(out, data.unit().map(Unit::as_str));
        }
    }
}

fn count(out: &mut Vec<u8>, n: usize) {
    let n = u64::try_from(n).expect("invariant: a length fits in 64 bits");
    out.extend_from_slice(&n.to_le_bytes());
}

fn text(out: &mut Vec<u8>, text: &str) {
    count(out, text.len());
    out.extend_from_slice(text.as_bytes());
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

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn malformed<T>(&self) -> Result<T, Error> {
        Err(Error::Malformed { at: self.at })
    }

    fn take(&mut self, len: usize) -> Result<&'b [u8], Error> {
        let Some(taken) = self.bytes.get(self.at..).and_then(|rest| rest.get(..len))
        else {
            return self.malformed();
        };
        self.at += len;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let found = self.bytes.get(self.at..).and_then(<[u8]>::first_chunk);
        let Some(&found) = found else {
            return self.malformed();
        };
        self.at += N;
        Ok(found)
    }

    fn byte(&mut self) -> Result<u8, Error> {
        self.array::<1>().map(|[byte]| byte)
    }

    fn flag(&mut self) -> Result<bool, Error> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => {
                self.at -= 1;
                self.malformed()
            }
        }
    }

    fn count(&mut self) -> Result<u64, Error> {
        self.array().map(u64::from_le_bytes)
    }

    /// A count and the bytes it counts.
    fn counted(&mut self) -> Result<&'b [u8], Error> {
        let at = self.at;
        let len = usize::try_from(self.count()?).ok();
        let taken = len.and_then(|len| self.take(len).ok());
        taken.ok_or(Error::Malformed { at })
    }

    /// A counted text that `read` reads and that prints back as it is.
    fn text<T>(
        &mut self,
        read: impl FnOnce(&str) -> Option<T>,
        print: impl FnOnce(&T) -> String,
    ) -> Result<T, Error> {
        let at = self.at;
        let found = std::str::from_utf8(self.counted()?).ok();
        found
            .and_then(|text| read(text).filter(|read| print(read) == text))
            .ok_or(Error::Malformed { at })
    }

    fn name(&mut self) -> Result<Name, Error> {
        self.text(|text| text.parse().ok(), |name: &Name| name.as_str().into())
    }

    fn optional_name(&mut self) -> Result<Option<Name>, Error> {
        self.flag()?.then(|| self.name()).transpose()
    }

    fn entry(&mut self) -> Result<Entry, Error> {
        let at = self.at;
        let definition = match self.byte()? {
            SPEC => {
                let start = self.at;
                let bytes = self.counted()?;
                match definition::Definition::decode(bytes) {
                    Ok(definition::Definition::Channel(_)) | Err(_) => {
                        return Err(Error::Malformed { at: start });
                    }
                    Ok(definition) => Definition::Spec(definition),
                }
            }
            CHANNEL => Definition::Channel(self.kind()?),
            _ => return Err(Error::Malformed { at }),
        };
        Ok(Entry {
            definition,
            label_span: None,
        })
    }

    fn kind(&mut self) -> Result<Kind<Name>, Error> {
        let at = self.at;
        match self.byte()? {
            INDEX => Ok(Kind::Index {
                error: self.optional_name()?,
                control: self.optional_name()?,
            }),
            DATA => {
                let index = self.name()?;
                let quality = self.optional_name()?;
                let data_type: DataType =
                    self.text(|text| text.parse().ok(), DataType::to_string)?;
                let unit_at = self.at;
                let unit = self
                    .flag()?
                    .then(|| {
                        self.text(
                            |text| Unit::new(text).ok(),
                            |unit| unit.as_str().into(),
                        )
                    })
                    .transpose()?;
                let data = Data::new(index, quality, data_type, unit);
                data.map(Kind::Data)
                    .map_err(|_refused| Error::Malformed { at: unit_at })
            }
            _ => Err(Error::Malformed { at }),
        }
    }
}
