use std::borrow::Cow;
use std::fmt;

use document::diagnostic::Code;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const ARGUMENT: Code = Code::new("ops.argument");
const UNKNOWN: Code = Code::new("ops.unknown-operation");
const INPUT: Code = Code::new("ops.input");
const OUTPUT: Code = Code::new("ops.output");

/// Why a command line did not run to its end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// An argument is missing, unknown, or has a bad value.
    Argument { message: String },
    /// No operation has this name. `closest` is the nearest name, if one is near.
    Unknown {
        name: String,
        closest: Option<String>,
    },
    /// Standard input could not be read.
    Input { message: String },
    /// Standard output could not be written.
    Output { message: String },
    /// The config files have problems: at least one.
    Config(Vec<Problem>),
}

impl Error {
    pub(crate) fn status(&self) -> u8 {
        match self {
            Self::Argument { .. } | Self::Unknown { .. } | Self::Config(_) => 2,
            Self::Input { .. } | Self::Output { .. } => 1,
        }
    }

    /// Each problem: for each error but `Config`, one with no place and no note.
    pub(crate) fn problems(&self) -> Cow<'_, [Problem]> {
        let (code, fix) = match self {
            Self::Config(problems) => return Cow::Borrowed(problems),
            Self::Argument { .. } => (
                ARGUMENT,
                "Match the arguments to the operation in `foundation docs`".to_owned(),
            ),
            Self::Unknown {
                closest: Some(closest),
                ..
            } => (UNKNOWN, format!("Use `{closest}`, the closest name")),
            Self::Unknown { closest: None, .. } => {
                (UNKNOWN, "Use a name from `foundation docs`".to_owned())
            }
            Self::Input { .. } => (
                INPUT,
                "Give standard input a source that can be read".to_owned(),
            ),
            Self::Output { .. } => (
                OUTPUT,
                "Give standard output a destination that can be written".to_owned(),
            ),
        };
        Cow::Owned(vec![Problem {
            code: code.as_str().to_owned(),
            message: self.to_string(),
            fix,
            place: None,
            notes: Vec::new(),
        }])
    }

    /// `{"errors": [...]}`, with each problem.
    pub(crate) fn json(&self) -> Value {
        json!({ "errors": self.problems() })
    }

    /// Each problem for a terminal, with one empty line between two. In `Config`, each
    /// control character is written as its Rust escape, because a config producer
    /// quotes the text of a file itself. In each other error, so are a backslash and
    /// each character that does not print, because that text holds the caller's text
    /// as given.
    pub(crate) fn text(&self) -> String {
        let escape: fn(&str) -> String = match self {
            Self::Config(_) => escape_controls,
            _ => escape,
        };
        let problems: Vec<String> = self
            .problems()
            .iter()
            .map(|problem| problem.text(escape))
            .collect();
        problems.join("\n")
    }
}

/// One problem, as the JSON and the text of each error give it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct Problem {
    /// What kind of problem it is, such as `ops.argument`.
    pub(crate) code: String,
    /// What is wrong.
    pub(crate) message: String,
    /// What to do about it.
    pub(crate) fix: String,
    /// Where the problem is, if it is in a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) place: Option<Place>,
    /// Other places that the problem names.
    pub(crate) notes: Vec<Note>,
}

impl Problem {
    /// `error[<code>]: <message>`, its place, `fix: <fix>`, then each note and its
    /// place, with `escape` on each text.
    fn text(&self, escape: fn(&str) -> String) -> String {
        let place = self.place.as_ref().map(Place::to_string);
        let notes: Vec<String> = self
            .notes
            .iter()
            .map(|note| format!("note: {}\n{}", escape(&note.text), note.place))
            .collect();
        format!(
            "error[{}]: {}\n{}fix: {}\n{}",
            self.code,
            escape(&self.message),
            place.unwrap_or_default(),
            escape(&self.fix),
            notes.concat()
        )
    }
}

/// A place that a problem names, and what is there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct Note {
    /// What is at the place.
    pub(crate) text: String,
    /// The place.
    pub(crate) place: Place,
}

/// A file, and a line and a column in it, each from 1. The column counts Unicode
/// scalar values, as rustc does.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct Place {
    /// The path of the file, as the user or a directory listing gave it, as
    /// `Path::display` writes it: each part that is not UTF-8 is U+FFFD, so two paths
    /// can give one text.
    pub(crate) file: String,
    /// The line, from 1.
    pub(crate) line: u32,
    /// The column, from 1, in Unicode scalar values.
    pub(crate) column: u32,
}

/// `  --> <file>:<line>:<column>`, as rustc writes a place, and a new line.
impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file = escape(&self.file);
        writeln!(f, "  --> {file}:{}:{}", self.line, self.column)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Argument { message } => f.write_str(message),
            Self::Unknown { name, .. } => write!(f, "no operation is named `{name}`"),
            Self::Input { message } => {
                write!(f, "standard input could not be read: {message}")
            }
            Self::Output { message } => {
                write!(f, "standard output could not be written: {message}")
            }
            Self::Config(problems) => match problems.as_slice() {
                [one] => f.write_str(&one.message),
                _ => write!(f, "the config files have {} problems", problems.len()),
            },
        }
    }
}

/// `text` with each control character escaped as `char::escape_debug` does, so it
/// stays one line. For a message or a fix whose producer quoted the text of a file.
pub(crate) fn escape_controls(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_debug().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// `text` escaped as `char::escape_debug` does, with quotes kept, so it stays one line.
pub(crate) fn escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '"' | '\'' => c.to_string(),
            _ => c.escape_debug().to_string(),
        })
        .collect()
}
