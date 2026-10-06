//! InfluxDB line protocol: one line for each point, with names checked and escaped
//! once, before the first line.

use std::fmt;
use std::io::Write as _;

use types::time::Stamp;

/// The start of each line of one measurement: its name and its tags, checked and
/// escaped once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Measurement {
    prefix: Vec<u8>,
}

impl Measurement {
    /// Checks and escapes `name` and `tags`. The line sorts the tags by key, as
    /// InfluxDB asks for its fastest writes.
    ///
    /// # Errors
    ///
    /// - [`Error::Empty`] for an empty name, tag key, or tag value.
    /// - [`Error::Character`] for one with a backslash, a newline, or a carriage
    ///   return.
    /// - [`Error::Reserved`] for a name or tag key that starts with `_`.
    /// - [`Error::Duplicate`] for a tag key that comes more than once.
    pub fn new(name: &str, tags: &[(&str, &str)]) -> Result<Self, Error> {
        unreserved(name)?;
        let mut prefix = Vec::new();
        escape(name, b", ", &mut prefix)?;
        let mut tags = tags.to_vec();
        tags.sort_unstable_by_key(|&(key, _)| key);
        let mut last = None;
        for (key, value) in tags {
            if last == Some(key) {
                return Err(Error::Duplicate(key.into()));
            }
            last = Some(key);
            unreserved(key)?;
            prefix.push(b',');
            escape(key, b",= ", &mut prefix)?;
            prefix.push(b'=');
            escape(value, b",= ", &mut prefix)?;
        }
        Ok(Self { prefix })
    }

    /// Appends one line with `fields` at `time` to `out`, and gives the number of
    /// fields it wrote. A float that is not finite is no value: its field is left
    /// out, and with no field left, nothing is written.
    #[expect(clippy::missing_panics_doc, reason = "a write to a Vec never fails")]
    pub fn line<'a>(
        &self,
        out: &mut Vec<u8>,
        fields: impl IntoIterator<Item = (&'a Key, Value)>,
        time: Stamp,
    ) -> usize {
        let start = out.len();
        out.extend_from_slice(&self.prefix);
        let mut written = 0_usize;
        for (key, value) in fields {
            if let Value::Float(float) = value
                && !float.is_finite()
            {
                continue;
            }
            out.push(if written == 0 { b' ' } else { b',' });
            out.extend_from_slice(&key.0);
            out.push(b'=');
            let wrote = match value {
                Value::Float(float) => write!(out, "{float:e}"),
                Value::Integer(integer) => write!(out, "{integer}i"),
                Value::Unsigned(unsigned) => write!(out, "{unsigned}u"),
                Value::Boolean(boolean) => {
                    out.push(if boolean { b't' } else { b'f' });
                    Ok(())
                }
            };
            wrote.expect("invariant: a write to a Vec never fails");
            written = written.saturating_add(1);
        }
        if written == 0 {
            out.truncate(start);
            return 0;
        }
        writeln!(out, " {}", time.nanos())
            .expect("invariant: a write to a Vec never fails");
        written
    }
}

/// A field key, checked and escaped once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key(Vec<u8>);

impl Key {
    /// Checks and escapes `key`.
    ///
    /// # Errors
    ///
    /// As [`Measurement::new`] for a tag key.
    pub fn new(key: &str) -> Result<Self, Error> {
        unreserved(key)?;
        let mut bytes = Vec::new();
        escape(key, b",= ", &mut bytes)?;
        Ok(Self(bytes))
    }
}

/// One field value, in InfluxDB's types.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    /// A 64-bit float.
    Float(f64),
    /// A signed 64-bit integer.
    Integer(i64),
    /// An unsigned 64-bit integer.
    Unsigned(u64),
    /// A boolean.
    Boolean(bool),
}

/// Why a name is not valid line protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// An empty name, key, or tag value.
    Empty,
    /// A name with a character that line protocol cannot carry.
    Character {
        /// The name.
        name: String,
        /// The character.
        character: char,
    },
    /// A name or key that starts with `_`, which InfluxDB keeps for itself.
    Reserved(String),
    /// A tag key that comes more than once.
    Duplicate(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "a name is empty"),
            Self::Character { name, character } => write!(
                f,
                "the name {name:?} holds {character:?}, which line protocol cannot carry"
            ),
            Self::Reserved(name) => write!(
                f,
                "the name {name:?} starts with '_', which InfluxDB keeps for itself"
            ),
            Self::Duplicate(key) => {
                write!(f, "the tag key {key:?} comes more than once")
            }
        }
    }
}

impl std::error::Error for Error {}

/// Refuses a name or key that starts with `_`.
fn unreserved(name: &str) -> Result<(), Error> {
    if name.starts_with('_') {
        return Err(Error::Reserved(name.into()));
    }
    Ok(())
}

/// Appends `text` to `out` with a backslash before each byte in `special`.
fn escape(text: &str, special: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
    if text.is_empty() {
        return Err(Error::Empty);
    }
    if let Some(character) = text.chars().find(|c| matches!(c, '\\' | '\n' | '\r')) {
        return Err(Error::Character {
            name: text.into(),
            character,
        });
    }
    for &byte in text.as_bytes() {
        if special.contains(&byte) {
            out.push(b'\\');
        }
        out.push(byte);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
