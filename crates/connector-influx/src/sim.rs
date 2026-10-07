//! A simulated InfluxDB store for tests. It parses with the line protocol parser of
//! InfluxDB 3, so it does not share a mistake with our writer.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
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

/// The values of one field key in a chunk. `values[i]` is the value of the point at
/// index `points[i]` of the chunk, so only the points that set the field take room.
#[derive(Debug)]
struct Column {
    points: Vec<u16>,
    values: Values,
}

/// A field type is fixed for each measurement, so one typed `Vec` holds the values.
#[derive(Debug)]
enum Values {
    Float(Vec<f64>),
    Integer(Vec<i64>),
    Unsigned(Vec<u64>),
    Boolean(Vec<bool>),
    String(Vec<String>),
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
        // The least index wins a tie of times, so ties come in tag order.
        let mut next: BinaryHeap<_> = series
            .iter_mut()
            .enumerate()
            .filter_map(|(at, points)| Some(Reverse((points.peek()?.time, at))))
            .collect();
        std::iter::from_fn(move || {
            let Reverse((_, at)) = next.pop()?;
            let points = series.get_mut(at)?;
            let point = points.next();
            next.extend(points.peek().map(|point| Reverse((point.time, at))));
            point
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
        let mut chunk = self.take(time);
        match chunk.times.binary_search(&time) {
            Ok(at) => chunk.set(at, fields),
            Err(at) if chunk.times.len() < CHUNK => chunk.insert(at, time, fields),
            Err(at) => {
                let mut right = chunk.split();
                let half = chunk.times.len();
                match at.checked_sub(half) {
                    Some(at) => right.insert(at, time, fields),
                    None => chunk.insert(at, time, fields),
                }
                self.put(right);
            }
        }
        self.put(chunk);
    }

    /// Takes out the chunk for a point at `time`: the chunk that holds the times
    /// around it, or else a neighbor with room, or else a new chunk. So appends in
    /// either time order leave each chunk full.
    fn take(&mut self, time: Stamp) -> Chunk {
        let before = self
            .chunks
            .range(..=time)
            .next_back()
            .filter(|(_, chunk)| chunk.times.len() < CHUNK || time <= chunk.last());
        let key = before
            .or_else(|| {
                self.chunks
                    .range(time..)
                    .next()
                    .filter(|(_, chunk)| chunk.times.len() < CHUNK)
            })
            .map(|(&key, _)| key);
        key.and_then(|key| self.chunks.remove(&key))
            .unwrap_or_default()
    }

    fn put(&mut self, chunk: Chunk) {
        self.chunks.insert(chunk.first(), chunk);
    }
}

impl Chunk {
    fn first(&self) -> Stamp {
        *self.times.first().expect("a chunk holds a point")
    }

    fn last(&self) -> Stamp {
        *self.times.last().expect("a chunk holds a point")
    }

    fn set(&mut self, at: usize, fields: BTreeMap<String, Field>) {
        for (key, field) in fields {
            self.columns
                .entry(key)
                .or_insert_with(|| Column::new(&field))
                .set(at, field);
        }
    }

    fn insert(&mut self, at: usize, time: Stamp, fields: BTreeMap<String, Field>) {
        insert(&mut self.times, at, time);
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
    /// An empty column for the type of `field`.
    fn new(field: &Field) -> Self {
        let values = match field {
            Field::Float(_) => Values::Float(Vec::new()),
            Field::Integer(_) => Values::Integer(Vec::new()),
            Field::Unsigned(_) => Values::Unsigned(Vec::new()),
            Field::Boolean(_) => Values::Boolean(Vec::new()),
            Field::String(_) => Values::String(Vec::new()),
        };
        Self {
            points: Vec::new(),
            values,
        }
    }

    /// Moves each point at index `at` or later one index up, for a new point at `at`.
    fn insert(&mut self, at: usize) {
        let from = self
            .points
            .partition_point(|&point| usize::from(point) < at);
        for point in self.points.iter_mut().skip(from) {
            *point = point.strict_add(1);
        }
    }

    fn set(&mut self, at: usize, field: Field) {
        let at = u16::try_from(at).expect("a chunk holds at most CHUNK points");
        let slot = self.points.binary_search(&at);
        if let Err(i) = slot {
            insert(&mut self.points, i, at);
        }
        self.values.put(slot, field);
    }

    fn get(&self, at: usize) -> Option<Field> {
        let at = u16::try_from(at).ok()?;
        self.values.get(self.points.binary_search(&at).ok()?)
    }

    /// Moves the points at index `half` or later into a new column, `half` indexes
    /// down.
    fn split(&mut self, half: usize) -> Self {
        let from = self
            .points
            .partition_point(|&point| usize::from(point) < half);
        let half = u16::try_from(half).expect("a chunk holds at most CHUNK points");
        Self {
            points: split(&mut self.points, from)
                .into_iter()
                .map(|point| point.strict_sub(half))
                .collect(),
            values: self.values.split(from),
        }
    }
}

impl Values {
    /// Puts `field` at `slot`: replaces the value at `Ok(i)`, or inserts it at `Err(i)`.
    fn put(&mut self, slot: Result<usize, usize>, field: Field) {
        match (self, field) {
            (Self::Float(values), Field::Float(value)) => put(values, slot, value),
            (Self::Integer(values), Field::Integer(value)) => put(values, slot, value),
            (Self::Unsigned(values), Field::Unsigned(value)) => {
                put(values, slot, value);
            }
            (Self::Boolean(values), Field::Boolean(value)) => put(values, slot, value),
            (Self::String(values), Field::String(value)) => put(values, slot, value),
            (values, field) => {
                unreachable!("the Conflict check refused {field:?} into {values:?}")
            }
        }
    }

    fn get(&self, i: usize) -> Option<Field> {
        match self {
            Self::Float(values) => values.get(i).copied().map(Field::Float),
            Self::Integer(values) => values.get(i).copied().map(Field::Integer),
            Self::Unsigned(values) => values.get(i).copied().map(Field::Unsigned),
            Self::Boolean(values) => values.get(i).copied().map(Field::Boolean),
            Self::String(values) => values.get(i).cloned().map(Field::String),
        }
    }

    fn split(&mut self, from: usize) -> Self {
        match self {
            Self::Float(values) => Self::Float(split(values, from)),
            Self::Integer(values) => Self::Integer(split(values, from)),
            Self::Unsigned(values) => Self::Unsigned(split(values, from)),
            Self::Boolean(values) => Self::Boolean(split(values, from)),
            Self::String(values) => Self::String(split(values, from)),
        }
    }
}

fn put<T>(values: &mut Vec<T>, slot: Result<usize, usize>, value: T) {
    match slot {
        Ok(i) => *values.get_mut(i).expect("a value for each point") = value,
        Err(i) => insert(values, i, value),
    }
}

/// Moves `values[from..]` into a new `Vec`, and frees the spare capacity of `values`.
fn split<T>(values: &mut Vec<T>, from: usize) -> Vec<T> {
    let right = values.split_off(from);
    values.shrink_to_fit();
    right
}

/// Inserts `value` at `at`. A full `Vec` grows by an eighth, not by double, so a
/// chunk half that a split left full wastes little after one more point.
fn insert<T>(values: &mut Vec<T>, at: usize, value: T) {
    if values.len() == values.capacity() {
        values.reserve_exact(values.len().div_ceil(8).max(1));
    }
    values.insert(at, value);
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
/// keys to equal values. `Debug` prints them as a map.
#[derive(Clone, Copy)]
pub struct Fields<'a> {
    chunk: &'a Chunk,
    at: usize,
}

impl<'a> Fields<'a> {
    /// The value of `key`, or `None` when the point does not set it. A string value is
    /// a copy.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Field> {
        self.chunk.columns.get(key)?.get(self.at)
    }

    /// Each field that the point sets, in key order. Each string value is a copy.
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
