//! A simulated InfluxDB store for tests. It parses with the line protocol parser of
//! InfluxDB 3, so a writer bug and a parser bug cannot hide each other.

use std::collections::BTreeMap;
use std::fmt;

use influxdb_line_protocol::{FieldValue, ParsedLine};

/// The first and last time that InfluxDB stores, in nanoseconds since the Unix
/// epoch.
const TIMES: std::ops::RangeInclusive<i64> = i64::MIN + 2..=i64::MAX - 1;

/// A simulated InfluxDB. A point is named by its measurement, tag set, and time, and
/// a later write of the same point replaces the fields that it sets.
#[derive(Clone, Debug, Default)]
pub struct Store {
    measurements: BTreeMap<String, Measurement>,
}

/// The points and field types of one measurement.
#[derive(Clone, Debug, Default)]
struct Measurement {
    points: BTreeMap<(i64, Tags), Point>,
    kinds: BTreeMap<String, Kind>,
}

type Tags = BTreeMap<String, String>;

impl Store {
    /// Stores each line of `body`. Like InfluxDB, it stores each valid line, also
    /// after a line that is not valid.
    ///
    /// # Errors
    ///
    /// The error of the first line that is not valid:
    /// - [`Error::Parse`] for a line that does not parse.
    /// - [`Error::Time`] for a line with no time, or a time that InfluxDB does not
    ///   store.
    /// - [`Error::Conflict`] for a field whose type differs from the type stored for
    ///   that key in the measurement. No field of that line is stored.
    pub fn write(&mut self, body: &str) -> Result<(), Error> {
        let mut first = None;
        for text in influxdb_line_protocol::split_lines(body) {
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
    #[must_use]
    pub fn points(&self, measurement: &str, tags: &[(&str, &str)]) -> Vec<&Point> {
        let Some(measurement) = self.measurements.get(measurement) else {
            return Vec::new();
        };
        measurement
            .points
            .values()
            .filter(|point| {
                tags.iter().all(|&(key, value)| {
                    point.tags.get(key).is_some_and(|stored| stored == value)
                })
            })
            .collect()
    }

    fn store(&mut self, text: &str, parsed: &ParsedLine<'_>) -> Result<(), Error> {
        let time = parsed
            .timestamp
            .filter(|time| TIMES.contains(time))
            .ok_or_else(|| Error::Time(text.into()))?;
        let name = parsed.series.measurement.as_str();
        let measurement = self.measurements.entry(name.into()).or_default();
        for (key, value) in &parsed.field_set {
            let written = Field::from(value).kind();
            if let Some(&stored) = measurement.kinds.get(key.as_str())
                && stored != written
            {
                return Err(Error::Conflict {
                    line: text.into(),
                    field: key.as_str().into(),
                    stored,
                    written,
                });
            }
        }
        let tags: Tags = parsed
            .series
            .tag_set
            .iter()
            .flatten()
            .map(|(key, value)| (key.as_str().into(), value.as_str().into()))
            .collect();
        let point = measurement
            .points
            .entry((time, tags.clone()))
            .or_insert_with(|| Point {
                time,
                tags,
                fields: BTreeMap::new(),
            });
        for (key, value) in &parsed.field_set {
            let field = Field::from(value);
            measurement.kinds.insert(key.as_str().into(), field.kind());
            point.fields.insert(key.as_str().into(), field);
        }
        Ok(())
    }
}

/// One stored point.
#[derive(Clone, Debug, PartialEq)]
pub struct Point {
    /// The time, in nanoseconds since the Unix epoch.
    pub time: i64,
    /// The tags, by key.
    pub tags: BTreeMap<String, String>,
    /// The fields, by key.
    pub fields: BTreeMap<String, Field>,
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
    /// The type of the value.
    #[must_use]
    pub const fn kind(&self) -> Kind {
        match self {
            Self::Float(_) => Kind::Float,
            Self::Integer(_) => Kind::Integer,
            Self::Unsigned(_) => Kind::Unsigned,
            Self::Boolean(_) => Kind::Boolean,
            Self::String(_) => Kind::String,
        }
    }
}

impl From<&FieldValue<'_>> for Field {
    fn from(value: &FieldValue<'_>) -> Self {
        match value {
            FieldValue::F64(float) => Self::Float(*float),
            FieldValue::I64(integer) => Self::Integer(*integer),
            FieldValue::U64(unsigned) => Self::Unsigned(*unsigned),
            FieldValue::Boolean(boolean) => Self::Boolean(*boolean),
            FieldValue::String(string) => Self::String(string.as_str().into()),
        }
    }
}

/// The type of a field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
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
            Self::Float => "float",
            Self::Integer => "integer",
            Self::Unsigned => "unsigned",
            Self::Boolean => "boolean",
            Self::String => "string",
        })
    }
}

/// Why the store refused a line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The line does not parse.
    Parse {
        /// The line.
        line: String,
        /// The parser's message.
        message: String,
    },
    /// The line has no time, or a time that InfluxDB does not store.
    Time(String),
    /// A field's type differs from the type stored for that key in the measurement.
    Conflict {
        /// The line.
        line: String,
        /// The field key.
        field: String,
        /// The stored type.
        stored: Kind,
        /// The type in the line.
        written: Kind,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse { line, message } => {
                write!(f, "the line {line:?} does not parse: {message}")
            }
            Self::Time(line) => write!(
                f,
                "the line {line:?} has no time, or one that InfluxDB does not store"
            ),
            Self::Conflict {
                line,
                field,
                stored,
                written,
            } => write!(
                f,
                "the line {line:?} writes the {stored} field {field:?} as {written}"
            ),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
