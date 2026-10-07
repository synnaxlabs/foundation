//! A simulated InfluxDB store for tests. It parses with the line protocol parser of
//! InfluxDB 3, so it does not share a mistake with our writer.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::Utf8Error;

use influxdb_line_protocol::{FieldValue, ParsedLine};
use types::time::Stamp;

pub use server::serve;

mod server;

/// The first and last time that InfluxDB stores, in nanoseconds since the Unix epoch.
const TIMES: std::ops::RangeInclusive<i64> = i64::MIN + 2..=i64::MAX - 1;

/// A simulated InfluxDB. A point is named by its measurement, tag set, and time, and a
/// later write of the same point replaces the fields that it sets.
///
/// It keeps the InfluxDB rules that a writer must keep: the syntax, the time range, the
/// reserved names, one use of each key, finite floats, and one type for each column,
/// where a tag is a type. Where InfluxDB versions differ in a rule that it keeps, it
/// keeps the strictest one, but it stores a `u` integer, which InfluxDB 1 OSS refuses.
/// It keeps no size limit.
#[derive(Debug, Default)]
pub struct Store {
    measurements: BTreeMap<String, Measurement>,
}

/// The points and field types of one measurement.
#[derive(Debug, Default)]
struct Measurement {
    points: BTreeMap<(Stamp, Tags), Fields>,
    kinds: BTreeMap<String, Kind>,
}

type Tags = BTreeMap<String, String>;
type Fields = BTreeMap<String, Field>;

impl Store {
    /// Stores each line of `body`. Like InfluxDB, it stores each valid line, also after
    /// a line that is not valid. It splits lines, and skips blank lines and comments,
    /// as InfluxDB 3 does.
    ///
    /// # Errors
    ///
    /// [`Error::Utf8`] for a body that is not UTF-8, and then no line is stored. Else
    /// the error of the first line that is not valid:
    /// - [`Error::Parse`] for a line that does not parse.
    /// - [`Error::Time`] for a line with no time, or a time that InfluxDB does not
    ///   store.
    /// - [`Error::Reserved`] for the key `time`, or a name or key that starts with
    ///   `_`.
    /// - [`Error::Duplicate`] for a key that comes more than once, in tags and fields
    ///   together.
    /// - [`Error::Infinite`] for a float that parses to infinity.
    /// - [`Error::Conflict`] for a tag or field whose type differs from the type stored
    ///   for that key in the measurement. A tag is a type.
    ///
    /// No field of a line that is not valid is stored.
    pub fn write(&mut self, body: &[u8]) -> Result<(), Error> {
        let body = std::str::from_utf8(body).map_err(|error| Error::Utf8 { error })?;
        let mut first = None;
        for text in influxdb_line_protocol::split_lines(body) {
            // `text` is one line: `split_lines` does not split its own line again.
            let Some(parsed) = influxdb_line_protocol::parse_lines(text).next() else {
                continue;
            };
            let stored = parsed
                .map_err(|error| Error::Parse {
                    line: text.into(),
                    message: error.to_string(),
                })
                .and_then(|parsed| self.store(text, &parsed));
            if let Err(error) = stored {
                first.get_or_insert(error);
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// The points of `measurement` that hold each tag of `tags`, in time order.
    pub fn points<'a>(
        &'a self,
        measurement: &str,
        tags: &[(&str, &str)],
    ) -> impl Iterator<Item = Point<'a>> {
        self.measurements
            .get(measurement)
            .into_iter()
            .flat_map(|measurement| &measurement.points)
            .filter(move |((_, held), _)| {
                tags.iter().all(|&(key, value)| {
                    held.get(key).is_some_and(|stored| stored == value)
                })
            })
            .map(|((time, tags), fields)| Point {
                time: *time,
                tags,
                fields,
            })
    }

    fn store(&mut self, text: &str, parsed: &ParsedLine<'_>) -> Result<(), Error> {
        let time = time(text, parsed.timestamp)?;
        let name = parsed.series.measurement.as_str();
        let reserved = |name: &str| Error::Reserved {
            line: text.into(),
            name: name.into(),
        };
        if name.starts_with('_') {
            return Err(reserved(name));
        }
        let mut keys = BTreeSet::new();
        let mut check = |key: &str| {
            if key == "time" || key.starts_with('_') {
                return Err(reserved(key));
            }
            if !keys.insert(key.to_owned()) {
                return Err(Error::Duplicate {
                    line: text.into(),
                    key: key.into(),
                });
            }
            Ok(())
        };
        let kinds = self.measurements.get(name).map(|stored| &stored.kinds);
        let conflict =
            |key: &str, written: Kind| match kinds.and_then(|kinds| kinds.get(key)) {
                Some(&stored) if stored != written => Err(Error::Conflict {
                    line: text.into(),
                    key: key.into(),
                    stored,
                    written,
                }),
                _ => Ok(()),
            };
        let mut tags = Tags::new();
        for (key, value) in parsed.series.tag_set.iter().flatten() {
            check(key.as_str())?;
            conflict(key.as_str(), Kind::Tag)?;
            tags.insert(key.as_str().into(), value.as_str().into());
        }
        let mut fields = Fields::new();
        for (key, value) in &parsed.field_set {
            let key = key.as_str();
            check(key)?;
            let field = field(value);
            if let Field::Float(float) = field
                && float.is_infinite()
            {
                return Err(Error::Infinite {
                    line: text.into(),
                    field: key.into(),
                });
            }
            conflict(key, field.kind())?;
            fields.insert(key.into(), field);
        }
        let measurement = self.measurements.entry(name.into()).or_default();
        for key in tags.keys() {
            measurement.kinds.insert(key.clone(), Kind::Tag);
        }
        for (key, field) in &fields {
            measurement.kinds.insert(key.clone(), field.kind());
        }
        measurement
            .points
            .entry((time, tags))
            .or_default()
            .extend(fields);
        Ok(())
    }
}

fn time(text: &str, time: Option<i64>) -> Result<Stamp, Error> {
    match time {
        Some(time) if TIMES.contains(&time) => Ok(Stamp::from_nanos(time)),
        time => Err(Error::Time {
            line: text.into(),
            time,
        }),
    }
}

/// One stored point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point<'a> {
    /// The time.
    pub time: Stamp,
    /// The tags, by key.
    pub tags: &'a BTreeMap<String, String>,
    /// The fields, by key.
    pub fields: &'a BTreeMap<String, Field>,
}

/// One field value, in InfluxDB's types.
#[derive(Clone, Debug, PartialEq)]
pub enum Field {
    /// A 64-bit float.
    Float(f64),
    /// A signed 64-bit integer.
    Integer(i64),
    /// An unsigned 64-bit integer.
    Unsigned(u64),
    /// A boolean.
    Boolean(bool),
    /// A string.
    String(String),
}

impl Field {
    const fn kind(&self) -> Kind {
        match self {
            Self::Float(_) => Kind::Float,
            Self::Integer(_) => Kind::Integer,
            Self::Unsigned(_) => Kind::Unsigned,
            Self::Boolean(_) => Kind::Boolean,
            Self::String(_) => Kind::String,
        }
    }
}

fn field(value: &FieldValue<'_>) -> Field {
    match value {
        FieldValue::F64(float) => Field::Float(*float),
        FieldValue::I64(integer) => Field::Integer(*integer),
        FieldValue::U64(unsigned) => Field::Unsigned(*unsigned),
        FieldValue::Boolean(boolean) => Field::Boolean(*boolean),
        FieldValue::String(string) => Field::String(string.as_str().into()),
    }
}

/// The type of a column: a tag, or the type of a field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A tag. No [`Field`] has it.
    Tag,
    /// [`Field::Float`].
    Float,
    /// [`Field::Integer`].
    Integer,
    /// [`Field::Unsigned`].
    Unsigned,
    /// [`Field::Boolean`].
    Boolean,
    /// [`Field::String`].
    String,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Tag => "tag",
            Self::Float => "float",
            Self::Integer => "integer",
            Self::Unsigned => "unsigned",
            Self::Boolean => "boolean",
            Self::String => "string",
        })
    }
}

/// Why the store refused a body or a line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The body is not UTF-8.
    Utf8 {
        /// Where the body stops being UTF-8.
        error: Utf8Error,
    },
    /// The line does not parse.
    Parse {
        /// The line.
        line: String,
        /// The parser's message.
        message: String,
    },
    /// The line has no time, or a time that InfluxDB does not store.
    Time {
        /// The line.
        line: String,
        /// The time in the line, or `None` when it has none.
        time: Option<i64>,
    },
    /// The line uses a name or key that InfluxDB keeps for itself: the key `time`, or
    /// one that starts with `_`.
    Reserved {
        /// The line.
        line: String,
        /// The name or key.
        name: String,
    },
    /// A key comes more than once in the line, in tags and fields together.
    Duplicate {
        /// The line.
        line: String,
        /// The key.
        key: String,
    },
    /// A float field parses to infinity.
    Infinite {
        /// The line.
        line: String,
        /// The field key.
        field: String,
    },
    /// A key's type differs from the type stored for that key in the measurement. A tag
    /// is a type, so a key is a tag or a field in each line.
    Conflict {
        /// The line.
        line: String,
        /// The tag or field key.
        key: String,
        /// The stored type.
        stored: Kind,
        /// The type in the line.
        written: Kind,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Utf8 { error } => write!(f, "the body is not UTF-8: {error}"),
            Self::Parse { line, message } => {
                write!(f, "the line {line:?} does not parse: {message}")
            }
            Self::Time { line, time: None } => {
                write!(f, "the line {line:?} has no time")
            }
            Self::Time {
                line,
                time: Some(time),
            } => write!(
                f,
                "the line {line:?} has time {time}, which InfluxDB does not store"
            ),
            Self::Reserved { line, name } => {
                write!(f, "the line {line:?} uses the reserved name {name:?}")
            }
            Self::Duplicate { line, key } => {
                write!(f, "the line {line:?} has the key {key:?} more than once")
            }
            Self::Infinite { line, field } => {
                write!(
                    f,
                    "the line {line:?} gives the field {field:?} an infinite float"
                )
            }
            Self::Conflict {
                line,
                key,
                stored,
                written,
            } => write!(
                f,
                "the line {line:?} writes the {stored} column {key:?} as {written}"
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
