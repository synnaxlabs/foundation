use std::borrow::Cow;
use std::fmt;
use std::path::PathBuf;

use document::Span;
use document::diagnostic::{Code, Diagnostic};
use mesh::used::{Behind, Cause};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Failure;

const ARGUMENT: Code = Code::new("ops.argument");
const UNKNOWN: Code = Code::new("ops.unknown-operation");
const INPUT: Code = Code::new("ops.input");
const OUTPUT: Code = Code::new("ops.output");
const BAD_PLAN: Code = Code::new("ops.bad-plan");
const BEHIND: Code = Code::new("ops.behind");
const STALE_PLAN: Code = Code::new("ops.stale-plan");
const APPLY: Code = Code::new("ops.apply");
const STOPPED: Code = Code::new("ops.stopped");
/// The fix of [`ARGUMENT`].
const HELP: &str =
    "Match the arguments to `foundation --help` or `foundation <operation> --help`";

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
    /// The plan file holds no plan that `plan` makes.
    Plan(config::plan::Error),
    /// The node does not use the newest spec, so it can neither plan nor apply.
    Behind(Box<Behind>),
    /// The spec in use is at `pointer`, not at `base`, the spec that the plan changes.
    Stale {
        base: spec::Pointer,
        pointer: spec::Pointer,
    },
    /// The region did not apply the plan.
    Apply(mesh::Error),
    /// The node did not start, or stopped with an error.
    Start(Failure),
    /// The group of the mesh stopped, so the node can neither plan nor apply.
    Stopped(mesh::Stopped),
}

impl Error {
    /// `Config` with the problem of each of `diagnostics`, whose spans are in the files
    /// at `paths`, by source.
    pub(crate) fn config(diagnostics: Vec<Diagnostic>, paths: &[PathBuf]) -> Self {
        Self::Config(
            diagnostics
                .into_iter()
                .map(|diagnostic| Problem::of(diagnostic, paths))
                .collect(),
        )
    }

    pub(crate) fn status(&self) -> u8 {
        match self {
            Self::Argument { .. }
            | Self::Unknown { .. }
            | Self::Config(_)
            | Self::Plan(_) => 2,
            Self::Input { .. }
            | Self::Output { .. }
            | Self::Behind(_)
            | Self::Stale { .. }
            | Self::Apply(_)
            | Self::Start(_)
            | Self::Stopped(_) => 1,
        }
    }

    /// Each problem: for each error but `Config`, one with no place and no note.
    pub(crate) fn problems(&self) -> Cow<'_, [Problem]> {
        let (code, fix) = match self {
            Self::Config(problems) => return Cow::Borrowed(problems),
            Self::Start(failure) => (failure.code, failure.fix.clone()),
            Self::Argument { .. } => (ARGUMENT, HELP.to_owned()),
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
            Self::Plan(_) => (
                BAD_PLAN,
                "Make a plan with `foundation plan`, and apply it with no edits"
                    .to_owned(),
            ),
            Self::Behind(_) => (BEHIND, "Fix the cause, then plan again".to_owned()),
            Self::Stale { .. } => (STALE_PLAN, "Plan again".to_owned()),
            Self::Apply(_) => (
                APPLY,
                "Fix the cause in the message, then plan and apply again".to_owned(),
            ),
            Self::Stopped(_) => (
                STOPPED,
                "Fix the cause in the message, then start the node and plan again"
                    .to_owned(),
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
    /// The problem of `diagnostic`, whose spans are in the files at `paths`, by
    /// source.
    pub(crate) fn of(diagnostic: Diagnostic, paths: &[PathBuf]) -> Self {
        Self {
            code: diagnostic.code.as_str().to_owned(),
            message: diagnostic.message,
            fix: diagnostic.fix,
            place: diagnostic.span.map(|span| Place::of(span, paths)),
            notes: diagnostic
                .notes
                .into_iter()
                .map(|note| Note {
                    text: note.text,
                    place: Place::of(note.span, paths),
                })
                .collect(),
        }
    }

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
    /// The exact path, as the user or a directory listing gave it. A path that is not
    /// UTF-8 gives `ops.path-not-utf8` instead.
    pub(crate) file: String,
    /// The line, from 1.
    pub(crate) line: u32,
    /// The column, from 1, in Unicode scalar values.
    pub(crate) column: u32,
}

impl Place {
    /// The start of `span`, in the file at `paths` of its source.
    pub(crate) fn of(span: Span, paths: &[PathBuf]) -> Self {
        let path = usize::try_from(span.source().0)
            .ok()
            .and_then(|source| paths.get(source))
            .expect("invariant: a span is in a file of the plan");
        let start = span.start();
        Self {
            file: path
                .to_str()
                .expect("invariant: `read` refuses a path that is not UTF-8")
                .to_owned(),
            line: start.line + 1,
            column: start.column + 1,
        }
    }
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
            Self::Plan(error) => error.fmt(f),
            Self::Behind(behind) => {
                let Behind { pointer, cause } = &**behind;
                write!(f, "the node does not use the newest spec, at {pointer}: ")?;
                match cause {
                    Cause::Read(error) => write!(f, "its tree does not read: {error}"),
                    Cause::Problems(problems) => {
                        f.write_str("it has problems at this build: ")?;
                        for (i, problem) in problems.iter().enumerate() {
                            let between = if i == 0 { "" } else { "; " };
                            write!(f, "{between}{problem}")?;
                        }
                        Ok(())
                    }
                    Cause::Blob(error) => {
                        write!(f, "a call of the store failed: {error}")
                    }
                    Cause::Files(error) => write!(
                        f,
                        "the file of the pointer in use was not made durable: {error}"
                    ),
                }
            }
            Self::Stale { base, pointer } => write!(
                f,
                "the spec changed: the spec is at {pointer}, not at the base {base} \
                 of the plan"
            ),
            Self::Apply(error) => error.fmt(f),
            Self::Start(failure) => f.write_str(&failure.message),
            Self::Stopped(stopped) => stopped.fmt(f),
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
