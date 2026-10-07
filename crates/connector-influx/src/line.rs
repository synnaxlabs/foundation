//! InfluxDB line protocol: one line for each point, with names checked and escaped
//! once, before the first line.

use std::fmt;
use std::io::Write as _;

use types::time::Stamp;
use unicode_properties::{GeneralCategoryGroup, UnicodeGeneralCategory as _};

/// One measurement's name, tags, and field keys, checked and escaped once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Measurement {
    prefix: Vec<u8>,
    fields: Vec<Vec<u8>>,
}

impl Measurement {
    /// Checks and escapes `name`, `tags`, and the field keys in `fields`. The line
    /// sorts the tags by key, as InfluxDB asks for its fastest writes.
    ///
    /// # Errors
    ///
    /// - [`Error::Empty`] for an empty name, key, or tag value.
    /// - [`Error::Character`] for one with a backslash, a newline, a carriage
    ///   return, a tab, NUL, U+FFFD, or a character outside the general categories
    ///   L, M, N, P, and S other than the space U+0020.
    /// - [`Error::Reserved`] for a name or key that starts with `_`, or a key
    ///   `time`.
    /// - [`Error::Comment`] for a name that starts with `#`.
    /// - [`Error::Duplicate`] for a key that comes more than once, in tags and
    ///   fields together.
    /// - [`Error::NoField`] for no fields.
    pub fn new(
        name: &str,
        tags: &[(&str, &str)],
        fields: &[&str],
    ) -> Result<Self, Error> {
        unreserved(name)?;
        if name.starts_with('#') {
            return Err(Error::Comment(name.into()));
        }
        if fields.is_empty() {
            return Err(Error::NoField);
        }
        let mut keys: Vec<&str> = tags.iter().map(|&(key, _)| key).collect();
        keys.extend_from_slice(fields);
        keys.sort_unstable();
        if let Some([key, _]) = keys.array_windows().find(|[a, b]| a == b) {
            return Err(Error::Duplicate((*key).into()));
        }
        let mut prefix = Vec::new();
        escape(name, b", ", Part::Measurement, &mut prefix)?;
        let mut tags = tags.to_vec();
        tags.sort_unstable_by_key(|&(key, _)| key);
        for (key, value) in tags {
            unreserved_key(key)?;
            prefix.push(b',');
            escape(key, b",= ", Part::TagKey, &mut prefix)?;
            prefix.push(b'=');
            escape(value, b",= ", Part::TagValue(key.into()), &mut prefix)?;
        }
        let fields = fields.iter().map(|&key| {
            unreserved_key(key)?;
            let mut bytes = Vec::new();
            escape(key, b",= ", Part::FieldKey, &mut bytes)?;
            Ok(bytes)
        });
        Ok(Self {
            prefix,
            fields: fields.collect::<Result<_, _>>()?,
        })
    }

    /// Appends one line at `time` to `out`, with the value of each field at its
    /// position in `values`. A field with no value is left out, and with no value
    /// at all, nothing is written.
    ///
    /// # Panics
    ///
    /// When `values` and the fields differ in length.
    pub fn line(&self, out: &mut Vec<u8>, values: &[Option<Value>], time: Stamp) {
        assert_eq!(values.len(), self.fields.len(), "one value for each field");
        let start = out.len();
        out.extend_from_slice(&self.prefix);
        let mut separator = b' ';
        for (key, value) in self.fields.iter().zip(values) {
            let Some(value) = *value else { continue };
            out.push(separator);
            separator = b',';
            out.extend_from_slice(key);
            out.push(b'=');
            let wrote = match value {
                Value::Float(Float(float)) => write!(out, "{float:e}"),
                Value::Integer(integer) => write!(out, "{integer}i"),
                Value::Unsigned(unsigned) => write!(out, "{unsigned}u"),
                Value::Boolean(boolean) => {
                    out.push(if boolean { b't' } else { b'f' });
                    Ok(())
                }
            };
            wrote.expect("invariant: a write to a Vec never fails");
        }
        if separator == b' ' {
            out.truncate(start);
            return;
        }
        writeln!(out, " {}", time.nanos())
            .expect("invariant: a write to a Vec never fails");
    }
}

/// One field value, in InfluxDB's types.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    /// A 64-bit float.
    Float(Float),
    /// A signed 64-bit integer.
    Integer(i64),
    /// An unsigned 64-bit integer.
    Unsigned(u64),
    /// A boolean.
    Boolean(bool),
}

/// A finite 64-bit float. InfluxDB cannot store NaN or infinity.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct Float(f64);

impl Float {
    /// Gives `value`, or `None` when it is NaN or infinite.
    #[must_use]
    pub fn new(value: f64) -> Option<Self> {
        value.is_finite().then_some(Self(value))
    }

    /// Gives the value.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

/// Why a measurement is not valid line protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// An empty part.
    Empty(Part),
    /// A part with a character that [`Measurement::new`] refuses.
    Character {
        /// The part that holds `character`.
        part: Part,
        /// The text of the part, as given.
        text: String,
        /// The first refused character in `text`.
        character: char,
    },
    /// A name or key that InfluxDB keeps for itself: one that starts with `_`, or
    /// the key `time`.
    Reserved(String),
    /// A measurement name that starts with `#`, which makes each line a comment.
    Comment(String),
    /// A key that comes more than once, in tags and fields together.
    Duplicate(String),
    /// No fields. A line holds at least one.
    NoField,
}

/// A part of a measurement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    /// The measurement name.
    Measurement,
    /// A tag key.
    TagKey,
    /// The value of the tag with this key.
    TagValue(String),
    /// A field key.
    FieldKey,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty(Part::Measurement) => {
                write!(f, "the measurement name is empty")
            }
            Self::Empty(Part::TagKey) => write!(f, "a tag key is empty"),
            Self::Empty(Part::TagValue(key)) => {
                write!(f, "the value of the tag {key:?} is empty")
            }
            Self::Empty(Part::FieldKey) => write!(f, "a field key is empty"),
            Self::Character {
                part,
                text,
                character,
            } => {
                match part {
                    Part::Measurement => write!(f, "the measurement name {text:?}")?,
                    Part::TagKey => write!(f, "the tag key {text:?}")?,
                    Part::TagValue(key) => {
                        write!(f, "the value {text:?} of the tag {key:?}")?;
                    }
                    Part::FieldKey => write!(f, "the field key {text:?}")?,
                }
                write!(f, " holds {character:?}, which a line cannot hold")
            }
            Self::Reserved(name) => {
                write!(f, "InfluxDB keeps the name {name:?} for itself")
            }
            Self::Comment(name) => write!(
                f,
                "the name {name:?} starts with '#', which makes each line a comment"
            ),
            Self::Duplicate(key) => write!(f, "the key {key:?} comes more than once"),
            Self::NoField => write!(f, "a measurement needs at least one field"),
        }
    }
}

impl std::error::Error for Error {}

/// Refuses a key that InfluxDB keeps for itself.
fn unreserved_key(key: &str) -> Result<(), Error> {
    if key == "time" {
        return Err(Error::Reserved(key.into()));
    }
    unreserved(key)
}

/// Refuses a name or key that starts with `_`.
fn unreserved(name: &str) -> Result<(), Error> {
    if name.starts_with('_') {
        return Err(Error::Reserved(name.into()));
    }
    Ok(())
}

/// Appends `text`, the `part` of a measurement, to `out` with a backslash before
/// each byte in `special`.
fn escape(
    text: &str,
    special: &[u8],
    part: Part,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    if text.is_empty() {
        return Err(Error::Empty(part));
    }
    if let Some(character) = text.chars().find(|&c| refused(c)) {
        return Err(Error::Character {
            part,
            text: text.into(),
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

/// Whether a line refuses `c`. A control character, which holds the newline, the
/// carriage return, the tab, and NUL, is in the group `Other`. InfluxDB 1 and 2 with
/// `validate-keys` drop each character outside L, M, N, P, and S but U+0020, and
/// U+FFFD.
fn refused(c: char) -> bool {
    c == '\\'
        || c == char::REPLACEMENT_CHARACTER
        || c != ' '
            && matches!(
                c.general_category_group(),
                GeneralCategoryGroup::Separator | GeneralCategoryGroup::Other
            )
}

#[cfg(test)]
mod tests;
