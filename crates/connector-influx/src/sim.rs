//! A simulated InfluxDB store for tests. It parses with the line protocol parser of
//! InfluxDB 3, so it does not share a mistake with our writer.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::Utf8Error;

use influxdb_line_protocol::{FieldValue, ParsedLine};
use types::time::Stamp;

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

/// The series and field types of one measurement.
#[derive(Debug, Default)]
struct Measurement {
    series: BTreeMap<Tags, Series>,
    kinds: BTreeMap<String, Kind>,
}

type Tags = BTreeMap<String, String>;

/// The most points that a chunk holds.
const CHUNK: usize = 4096;

/// The points of one tag set, in chunks keyed by the first time of each.
#[derive(Debug, Default)]
struct Series {
    chunks: BTreeMap<Stamp, Chunk>,
}

/// Points in time order, with one typed column for each field key.
#[derive(Debug, Default)]
struct Chunk {
    times: Vec<Stamp>,
    columns: BTreeMap<String, Column>,
}

/// The values of one field key in a chunk. A field type is fixed for each
/// measurement, so one typed column holds it.
#[derive(Debug)]
enum Column {
    Float(Typed<f64>),
    Integer(Typed<i64>),
    Unsigned(Typed<u64>),
    Boolean(Typed<bool>),
    String(Typed<String>),
}

/// `set[i]` says whether point `i` sets the field, and then `values[i]` holds it.
#[derive(Debug)]
struct Typed<T> {
    set: Vec<bool>,
    values: Vec<T>,
}

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
        let mut series: Vec<_> = self
            .measurements
            .get(measurement)
            .into_iter()
            .flat_map(|measurement| &measurement.series)
            .filter(|(held, _)| {
                tags.iter().all(|&(key, value)| {
                    held.get(key).is_some_and(|stored| stored == value)
                })
            })
            .map(|(tags, series)| series.points(tags).peekable())
            .collect();
        std::iter::from_fn(move || {
            // The first of equal times wins, so ties come in tag order.
            let next = series
                .iter_mut()
                .enumerate()
                .filter_map(|(at, points)| Some((points.peek()?.time, at)))
                .min()?
                .1;
            series.get_mut(next)?.next()
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
        let mut fields: BTreeMap<String, Field> = BTreeMap::new();
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
            .series
            .entry(tags)
            .or_default()
            .write(time, fields);
        Ok(())
    }
}

impl Series {
    fn points<'a>(&'a self, tags: &'a Tags) -> impl Iterator<Item = Point<'a>> {
        self.chunks.values().flat_map(move |chunk| {
            chunk
                .times
                .iter()
                .enumerate()
                .map(move |(at, &time)| Point {
                    time,
                    tags,
                    fields: Fields { chunk, at },
                })
        })
    }

    /// Sets `fields` on the point at `time`, and adds the point if it is new.
    fn write(&mut self, time: Stamp, fields: BTreeMap<String, Field>) {
        let Some((first, found, len)) = self
            .chunks
            .range(..=time)
            .next_back()
            .or_else(|| self.chunks.first_key_value())
            .map(|(&first, chunk)| {
                (first, chunk.times.binary_search(&time), chunk.times.len())
            })
        else {
            return self.insert(time, 0, time, fields);
        };
        match found {
            Ok(at) => self.chunk(first).set(at, fields),
            Err(at) if len < CHUNK => self.insert(first, at, time, fields),
            // A time before or after a full chunk starts a chunk, so appends in time
            // order leave each chunk full.
            Err(0 | CHUNK) => self.insert(time, 0, time, fields),
            Err(_) => {
                let right = self.chunk(first).split();
                self.chunks.insert(right.first(), right);
                self.write(time, fields);
            }
        }
    }

    /// Adds the point at `time` at index `at` of the chunk keyed `first`, or of a new
    /// chunk when none is keyed `first`.
    fn insert(
        &mut self,
        first: Stamp,
        at: usize,
        time: Stamp,
        fields: BTreeMap<String, Field>,
    ) {
        if at == 0 {
            let mut chunk = self.chunks.remove(&first).unwrap_or_default();
            chunk.insert(0, time, fields);
            self.chunks.insert(time, chunk);
        } else {
            self.chunk(first).insert(at, time, fields);
        }
    }

    fn chunk(&mut self, first: Stamp) -> &mut Chunk {
        self.chunks
            .get_mut(&first)
            .expect("a chunk that was just found")
    }
}

impl Chunk {
    fn first(&self) -> Stamp {
        *self.times.first().expect("a chunk holds a point")
    }

    fn set(&mut self, at: usize, fields: BTreeMap<String, Field>) {
        let len = self.times.len();
        for (key, field) in fields {
            self.columns
                .entry(key)
                .or_insert_with(|| Column::new(&field, len))
                .set(at, field);
        }
    }

    fn insert(&mut self, at: usize, time: Stamp, fields: BTreeMap<String, Field>) {
        self.times.insert(at, time);
        for column in self.columns.values_mut() {
            column.insert(at);
        }
        self.set(at, fields);
    }

    /// Moves the later half of the points into a new chunk.
    fn split(&mut self) -> Self {
        let half = self.times.len() / 2;
        Self {
            times: split(&mut self.times, half),
            columns: self
                .columns
                .iter_mut()
                .map(|(key, column)| (key.clone(), column.split(half)))
                .collect(),
        }
    }
}

impl Column {
    /// A column of `len` points, none of which sets it, for the type of `field`.
    fn new(field: &Field, len: usize) -> Self {
        match field {
            Field::Float(_) => Self::Float(Typed::new(len)),
            Field::Integer(_) => Self::Integer(Typed::new(len)),
            Field::Unsigned(_) => Self::Unsigned(Typed::new(len)),
            Field::Boolean(_) => Self::Boolean(Typed::new(len)),
            Field::String(_) => Self::String(Typed::new(len)),
        }
    }

    fn insert(&mut self, at: usize) {
        match self {
            Self::Float(typed) => typed.insert(at),
            Self::Integer(typed) => typed.insert(at),
            Self::Unsigned(typed) => typed.insert(at),
            Self::Boolean(typed) => typed.insert(at),
            Self::String(typed) => typed.insert(at),
        }
    }

    fn set(&mut self, at: usize, field: Field) {
        match (self, field) {
            (Self::Float(typed), Field::Float(value)) => typed.set(at, value),
            (Self::Integer(typed), Field::Integer(value)) => typed.set(at, value),
            (Self::Unsigned(typed), Field::Unsigned(value)) => typed.set(at, value),
            (Self::Boolean(typed), Field::Boolean(value)) => typed.set(at, value),
            (Self::String(typed), Field::String(value)) => typed.set(at, value),
            (column, field) => {
                unreachable!("the Conflict check refused {field:?} into {column:?}")
            }
        }
    }

    fn get(&self, at: usize) -> Option<Field> {
        match self {
            Self::Float(typed) => typed.get(at).copied().map(Field::Float),
            Self::Integer(typed) => typed.get(at).copied().map(Field::Integer),
            Self::Unsigned(typed) => typed.get(at).copied().map(Field::Unsigned),
            Self::Boolean(typed) => typed.get(at).copied().map(Field::Boolean),
            Self::String(typed) => typed.get(at).cloned().map(Field::String),
        }
    }

    fn split(&mut self, half: usize) -> Self {
        match self {
            Self::Float(typed) => Self::Float(typed.split(half)),
            Self::Integer(typed) => Self::Integer(typed.split(half)),
            Self::Unsigned(typed) => Self::Unsigned(typed.split(half)),
            Self::Boolean(typed) => Self::Boolean(typed.split(half)),
            Self::String(typed) => Self::String(typed.split(half)),
        }
    }
}

impl<T: Clone + Default> Typed<T> {
    fn new(len: usize) -> Self {
        Self {
            set: vec![false; len],
            values: vec![T::default(); len],
        }
    }

    fn insert(&mut self, at: usize) {
        self.set.insert(at, false);
        self.values.insert(at, T::default());
    }

    fn set(&mut self, at: usize, value: T) {
        *self.set.get_mut(at).expect("a point of the chunk") = true;
        *self.values.get_mut(at).expect("a point of the chunk") = value;
    }

    fn get(&self, at: usize) -> Option<&T> {
        self.set
            .get(at)
            .is_some_and(|set| *set)
            .then(|| self.values.get(at))
            .flatten()
    }

    fn split(&mut self, half: usize) -> Self {
        Self {
            set: split(&mut self.set, half),
            values: split(&mut self.values, half),
        }
    }
}

/// Moves `values[half..]` into a new `Vec`, and frees the spare capacity of `values`.
fn split<T>(values: &mut Vec<T>, half: usize) -> Vec<T> {
    let right = values.split_off(half);
    values.shrink_to_fit();
    right
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
    /// The fields that the point sets.
    pub fields: Fields<'a>,
}

/// The fields that one stored point sets, by key. Two are equal when they set the same
/// keys to equal values.
#[derive(Clone, Copy)]
pub struct Fields<'a> {
    chunk: &'a Chunk,
    at: usize,
}

impl<'a> Fields<'a> {
    /// The value of `key`, or `None` when the point does not set it.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Field> {
        self.chunk.columns.get(key)?.get(self.at)
    }

    /// Each field that the point sets, in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&'a str, Field)> + 'a {
        let at = self.at;
        self.chunk
            .columns
            .iter()
            .filter_map(move |(key, column)| Some((key.as_str(), column.get(at)?)))
    }
}

impl PartialEq for Fields<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

impl PartialEq<BTreeMap<String, Field>> for Fields<'_> {
    fn eq(&self, other: &BTreeMap<String, Field>) -> bool {
        self.iter().eq(other
            .iter()
            .map(|(key, field)| (key.as_str(), field.clone())))
    }
}

impl fmt::Debug for Fields<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
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
