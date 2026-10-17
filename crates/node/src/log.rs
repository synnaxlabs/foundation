//! The log of a node: one line for each `tracing` event.

mod queue;
mod span;

use std::fmt;
use std::io;
use std::panic::PanicHookInfo;

use clock::log::{Mesh, Time};
use serde::ser::{Serialize, SerializeMap, Serializer};
use tracing::field::{Field, Visit};
use tracing::{Dispatch, Event, Subscriber};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::{FmtContext, layer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::{LookupSpan, Scope};
use types::time::Stamp;

pub use self::queue::Flush;
pub(crate) use self::queue::Hold;
use self::queue::Pipe;

/// Where and how the node writes its log: one line for each event at `level` or
/// above, with the stamp, the level, the target, the message, the fields of each
/// span of the event at `level` or above, outer first, and the fields of the event.
pub struct Log {
    /// Where each line goes, with one write for each line. `main` gives stderr. The
    /// thread `log` writes each line, so no event waits on a write. A write that fails
    /// drops its line, and one that panics ends the log. A line drops when 1024 lines
    /// wait, when it does not fit in 1 MiB with the lines that wait, or after a drop
    /// until the thread `log` takes the lines that wait. The thread writes at most one
    /// batch beside the lines that wait, so the log holds at most 2048 lines and 2 MiB,
    /// beside the panic lines. WARN `dropped` with the field `lines`, the count of
    /// these lines, comes after the lines before them.
    pub writer: Box<dyn io::Write + Send>,
    /// The form of each line, text or JSON, and the least level of an event that the
    /// log holds.
    pub format: ops::Log,
}

impl fmt::Debug for Log {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Log")
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

/// The log of `log`, with no thread `log` yet ([`Flush::hold`]): each event of `log`'s
/// level or above, stamped with the time of `wall` and the mesh time of `mesh`. Its
/// threads start with `threads`.
pub(crate) fn open(
    log: Log,
    threads: &env::threads::Threads,
    wall: env::wall::Wall,
    mesh: clock::Reader,
) -> Flush {
    let Log {
        writer,
        format: ops::Log { json, level },
    } = log;
    let line = Line {
        json,
        time: clock::log::Reader::new(wall, mesh),
    };
    Flush::new(writer, line, level, threads)
}

/// The fields of a span for `fmt`, which formats none: [`span::Keep`] keeps them.
struct Unformatted;

impl<'w> FormatFields<'w> for Unformatted {
    fn format_fields<R: RecordFields>(&self, _: Writer<'w>, _: R) -> fmt::Result {
        Ok(())
    }
}

/// The dispatch that puts each event of `level` or above in `queue`, as one `line`.
fn dispatch(line: Line, level: tracing::Level, queue: Pipe) -> Dispatch {
    // The doc of `fmt` gives true as the default, though 0.3.23 sets false.
    let lines = layer()
        .fmt_fields(Unformatted)
        .event_format(line)
        .with_writer(queue)
        .log_internal_errors(false);
    let registry = tracing_subscriber::registry();
    let filtered = registry.with(LevelFilter::from_level(level));
    Dispatch::new(filtered.with(span::Keep).with(lines))
}

/// Writes each event as one line, in text or in JSON.
#[derive(Clone, Debug)]
struct Line {
    json: bool,
    time: clock::log::Reader,
}

impl<S, N> FormatEvent<S, N> for Line
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        #[expect(clippy::disallowed_methods, reason = "only the log stamps lines")]
        let time = self.time.now();
        let mut fields = Fields::default();
        let spans = ctx.event_scope().into_iter().flat_map(Scope::from_root);
        for span in spans {
            let extensions = span.extensions();
            let kept = extensions.get::<span::Fields>();
            let kept = kept.expect("span::Keep keeps the fields of each span");
            for (name, value) in &kept.0 {
                fields.put(name, value.clone());
            }
        }
        event.record(&mut Visitor(|field: &Field, value| {
            fields.add(field, value);
        }));
        let metadata = event.metadata();
        let entry = Entry {
            time,
            level: *metadata.level(),
            target: metadata.target(),
            fields,
        };
        self.write(entry, &mut writer)
    }
}

impl Line {
    /// The count line: WARN `dropped` with the field `lines`, the count `lines`.
    fn dropped(&self, lines: u64) -> String {
        let field = ("lines", Value::Unsigned(lines.into()));
        self.line(tracing::Level::WARN, "node::log", "dropped", field)
    }

    /// The panic line: ERROR `panicked` with the field `panic`, the text of `info`.
    fn panicked(&self, info: &PanicHookInfo<'_>) -> String {
        let field = ("panic", Value::Text(info.to_string()));
        self.line(tracing::Level::ERROR, "node", "panicked", field)
    }

    /// The line of an event with `message` and one `field`, stamped now.
    fn line(
        &self,
        level: tracing::Level,
        target: &str,
        message: &str,
        field: (&'static str, Value),
    ) -> String {
        #[expect(clippy::disallowed_methods, reason = "only the log stamps lines")]
        let time = self.time.now();
        let entry = Entry {
            time,
            level,
            target,
            fields: Fields {
                message: Some(message.to_owned()),
                others: vec![field],
            },
        };
        let mut text = String::new();
        self.write(entry, &mut Writer::new(&mut text))
            .expect("invariant: a write to a String does not fail");
        text
    }

    fn write(&self, entry: Entry<'_>, w: &mut Writer<'_>) -> fmt::Result {
        if self.json {
            entry.json(w)
        } else {
            entry.text(w)
        }
    }
}

/// The fields of the spans of one event, outer first, then those of the event, each
/// key once, with the message of the event apart.
#[derive(Default)]
struct Fields {
    message: Option<String>,
    others: Vec<(&'static str, Value)>,
}

impl Fields {
    /// Adds a field of the event: a text `message` is the message.
    fn add(&mut self, field: &Field, value: Value) {
        match (field.name(), value) {
            ("message", Value::Text(message)) => {
                self.others.retain(|(other, _)| *other != "message");
                self.message = Some(message);
            }
            (name, value) => self.put(name, value),
        }
    }

    /// Adds the field `name` last, in place of a field of that name before it.
    fn put(&mut self, name: &'static str, value: Value) {
        if name == "message" {
            self.message = None;
        }
        self.others.retain(|(other, _)| *other != name);
        self.others.push((name, value));
    }
}

/// Gives each field that it visits, with its [`Value`], to its function.
struct Visitor<F>(F);

impl<F: FnMut(&Field, Value)> Visit for Visitor<F> {
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0(field, Value::Signed(value.into()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0(field, Value::Unsigned(value.into()));
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        self.0(field, Value::Signed(value));
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.0(field, Value::Unsigned(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0(field, Value::Bool(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0(field, Value::Text(value.to_owned()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0(field, Value::Text(format!("{value:?}")));
    }
}

/// The value of one field. Integers and booleans keep their type, so JSON writes them
/// as numbers and booleans; each other value is text.
#[derive(Clone)]
enum Value {
    Signed(i128),
    Unsigned(u128),
    Bool(bool),
    Text(String),
}

impl Value {
    /// Writes the value as a text line holds it.
    fn write(&self, w: &mut Writer<'_>) -> fmt::Result {
        match self {
            Self::Signed(n) => write!(w, "{n}"),
            Self::Unsigned(n) => write!(w, "{n}"),
            Self::Bool(b) => write!(w, "{b}"),
            Self::Text(text) => value(w, text),
        }
    }
}

/// One event, stamped.
struct Entry<'a> {
    time: Time,
    level: tracing::Level,
    target: &'a str,
    fields: Fields,
}

impl Entry<'_> {
    /// `<stamp> <LEVEL> <target>: <message> <key>=<value>... [mesh_time=<time>|none]`.
    /// The message has `\` and each control character escaped, and each string value
    /// is quoted as logfmt does ([`value`]), so the line stays one line.
    fn text(self, w: &mut Writer<'_>) -> fmt::Result {
        let Self {
            time,
            level,
            target,
            fields,
        } = self;
        write!(w, "{} {level} {target}:", micros(time.wall))?;
        if let Some(message) = fields.message {
            w.write_char(' ')?;
            for c in message.chars() {
                escape(w, c, c == '\\')?;
            }
        }
        // The `mesh_time` of the line comes last, so it replaces a field of that name.
        for (name, field) in fields
            .others
            .iter()
            .filter(|(name, _)| *name != "mesh_time")
        {
            write!(w, " {name}=")?;
            field.write(w)?;
        }
        match time.mesh {
            Mesh::Near => {}
            Mesh::Apart(mesh) => write!(w, " mesh_time={}", micros(mesh))?,
            Mesh::Unsynced => w.write_str(" mesh_time=none")?,
        }
        w.write_char('\n')
    }

    /// One JSON object with the keys `timestamp`, `level`, `target`, `fields` (the
    /// message first), and `mesh_time`, in that order.
    #[expect(
        clippy::unwrap_in_result,
        reason = "only a defect of `Serialize` fails, and `fmt` drops it with no word"
    )]
    fn json(&self, w: &mut Writer<'_>) -> fmt::Result {
        // `serde_json` writes bytes and `Writer` takes `str`. An adapter checks the
        // UTF-8 of each piece; `to_string` builds the line with no check, for one copy.
        let line = serde_json::to_string(self)
            .expect("invariant: a map with string keys serializes to a String");
        w.write_str(&line)?;
        w.write_char('\n')
    }
}

/// `text` as logfmt writes a value: in double quotes when it is empty or holds a
/// space, `=`, `"`, `\`, or a control character, with `"`, `\`, and each control
/// character escaped; else plain.
fn value(w: &mut Writer<'_>, text: &str) -> fmt::Result {
    let quoted = |c: char| matches!(c, ' ' | '=' | '"' | '\\') || c.is_control();
    if !text.is_empty() && !text.chars().any(quoted) {
        return w.write_str(text);
    }
    w.write_char('"')?;
    for c in text.chars() {
        escape(w, c, matches!(c, '"' | '\\'))?;
    }
    w.write_char('"')
}

/// Writes `c`, as `char::escape_debug` gives it when it is `escaped` or a control
/// character.
fn escape(w: &mut Writer<'_>, c: char, escaped: bool) -> fmt::Result {
    if escaped || c.is_control() {
        write!(w, "{}", c.escape_debug())
    } else {
        w.write_char(c)
    }
}

/// A map keeps the order of its entries, where `json!` sorts its keys.
impl Serialize for Entry<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(None)?;
        map.serialize_entry("timestamp", &micros(self.time.wall))?;
        map.serialize_entry("level", self.level.as_str())?;
        map.serialize_entry("target", self.target)?;
        map.serialize_entry("fields", &self.fields)?;
        match self.time.mesh {
            Mesh::Near => {}
            Mesh::Apart(mesh) => map.serialize_entry("mesh_time", &micros(mesh))?,
            Mesh::Unsynced => map.serialize_entry("mesh_time", &())?,
        }
        map.end()
    }
}

impl Serialize for Fields {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(None)?;
        if let Some(message) = &self.message {
            map.serialize_entry("message", message)?;
        }
        for (name, value) in &self.others {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

impl Serialize for Value {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Signed(n) => s.serialize_i128(*n),
            Self::Unsigned(n) => s.serialize_u128(*n),
            Self::Bool(b) => s.serialize_bool(*b),
            Self::Text(text) => s.serialize_str(text),
        }
    }
}

/// `stamp` in RFC 3339 UTC with six fraction digits, floored.
fn micros(stamp: Stamp) -> String {
    let mut text = stamp.to_string();
    let end = text.len() - 1;
    text.replace_range(end - 3..end, "");
    text
}
