//! Plans the change from config files to the applied spec.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use document::diagnostic::{Code, Diagnostic};
use document::{Document, Source, Span};
use serde_json::{Map, Value, json};
use spec::definition::{Definition, Kind};
use types::name::Name;

use crate::error::escape;

#[cfg(test)]
mod tests;

const UNKNOWN_EXTENSION: Code = Code::new("ops.unknown-extension");

/// One syntax of config files.
#[derive(Clone, Copy, Debug)]
pub struct FrontEnd {
    /// Reads the text of one file into a Document whose spans hold `source`.
    ///
    /// # Errors
    ///
    /// Each problem in the text, with its span.
    pub read: fn(source: Source, text: &str) -> Result<Document, Vec<Diagnostic>>,
}

/// One config file: its path, as the user or a directory listing gave it, and its text.
pub(crate) struct File {
    pub(crate) path: PathBuf,
    pub(crate) text: String,
}

/// The change from the files to the spec at `base`. `Source(i)` is `files[i]`, and the
/// extension of its path picks its front end.
///
/// # Errors
///
/// `ops.unknown-extension` for each file whose extension has no front end, and each
/// problem of a front end, in file order. Else each problem of [`config::plan`].
///
/// # Panics
///
/// When `front_ends` is empty.
pub(crate) fn plan(
    files: &[File],
    base: spec::Pointer,
    applied: &BTreeMap<Name, Definition>,
    members: &BTreeSet<Name>,
    front_ends: &BTreeMap<&'static str, FrontEnd>,
    kinds: &connector::kind::Table,
) -> Result<Planned, Problems> {
    assert!(
        !front_ends.is_empty(),
        "invariant: `node` gives a front end"
    );
    let paths: Vec<PathBuf> = files.iter().map(|file| file.path.clone()).collect();
    let mut documents = Vec::new();
    let mut diagnostics = Vec::new();
    for (file, source) in files.iter().zip(0..) {
        let front_end = file
            .path
            .extension()
            .and_then(|extension| front_ends.get(extension.to_str()?));
        let Some(front_end) = front_end else {
            diagnostics.push(unknown(&file.path, front_ends));
            continue;
        };
        match (front_end.read)(Source(source), &file.text) {
            Ok(document) => documents.push(document),
            Err(problems) => diagnostics.extend(problems),
        }
    }
    if !diagnostics.is_empty() {
        return Err(Problems { diagnostics, paths });
    }
    let plan = config::plan(&documents, base, applied, members, kinds).map_err(
        |diagnostics| Problems {
            diagnostics,
            paths: paths.clone(),
        },
    )?;
    let mut lines: Vec<Line> = plan
        .changes
        .iter()
        .map(|change| Line::of(change, applied, &paths))
        .collect();
    lines.sort_by(|a, b| a.order.cmp(&b.order));
    Ok(Planned {
        base,
        homes: plan.homes,
        lines,
    })
}

/// The `ops.unknown-extension` diagnostic of `path`.
pub(crate) fn unknown(
    path: &Path,
    front_ends: &BTreeMap<&'static str, FrontEnd>,
) -> Diagnostic {
    let extensions: Vec<String> = front_ends
        .keys()
        .map(|extension| format!("`.{extension}`"))
        .collect();
    let extensions = match extensions.as_slice() {
        [one] => one.clone(),
        [first, second] => format!("{first} or {second}"),
        [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
        [] => unreachable!("invariant: `plan` checks the table"),
    };
    Diagnostic::new(
        UNKNOWN_EXTENSION,
        None,
        format!("no config syntax reads `{}`", path.display()),
        format!("Use a file that ends in {extensions}"),
    )
}

/// A plan, with what the text and the JSON show of it.
#[derive(Debug)]
pub(crate) struct Planned {
    base: spec::Pointer,
    homes: BTreeMap<Name, Name>,
    /// Each change, in the order the text shows it.
    lines: Vec<Line>,
}

/// One line for each change, then the counts.
impl fmt::Display for Planned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for line in &self.lines {
            let (symbol, kind) = (line.action.symbol(), line.kind.as_str());
            writeln!(f, "{symbol} {kind} {}", line.label)?;
        }
        let [added, changed, removed] = self.counts();
        writeln!(
            f,
            "{added} to add, {changed} to change, {removed} to remove."
        )
    }
}

impl Planned {
    pub(crate) fn json(&self) -> Value {
        let lines: Vec<Value> = self
            .lines
            .iter()
            .map(|line| {
                let mut change = Map::new();
                change.insert("action".into(), line.action.word().into());
                change.insert("kind".into(), line.kind.as_str().into());
                change.insert("name".into(), line.label.as_str().into());
                if let Some(place) = &line.place {
                    place.insert(&mut change);
                }
                Value::Object(change)
            })
            .collect();
        let homes: Map<String, Value> = self
            .homes
            .iter()
            .map(|(index, home)| (index.to_string(), home.as_str().into()))
            .collect();
        let [added, changed, removed] = self.counts();
        json!({
            "base": { "version": self.base.version, "root": self.base.root.to_string() },
            "changes": lines,
            "homes": homes,
            "added": added,
            "changed": changed,
            "removed": removed,
        })
    }

    /// The number of adds, changes, and removals.
    fn counts(&self) -> [usize; 3] {
        let count = |action| {
            self.lines
                .iter()
                .filter(|line| line.action == action)
                .count()
        };
        [
            count(Action::Add),
            count(Action::Change),
            count(Action::Remove),
        ]
    }
}

/// What the text shows of one change.
#[derive(Debug)]
struct Line {
    action: Action,
    kind: Kind,
    label: Name,
    place: Option<Place>,
    /// Adds and changes in file order, then removals in tree key order.
    order: (bool, Option<(Source, u32)>, Name),
}

impl Line {
    fn of(
        change: &config::Change,
        applied: &BTreeMap<Name, Definition>,
        paths: &[PathBuf],
    ) -> Self {
        let (action, kind, span) = if let Some(entry) = &change.new {
            let kind = match &entry.definition {
                config::Definition::Spec(definition) => definition.kind(),
                config::Definition::Channel(_) => Kind::Channel,
                _ => unreachable!("invariant: `ops` knows each kind of definition"),
            };
            let action = if change.old.is_some() {
                Action::Change
            } else {
                Action::Add
            };
            (action, kind, entry.label_span)
        } else {
            let stored = applied
                .get(&change.name)
                .expect("invariant: a removal is of an applied definition");
            (Action::Remove, stored.kind(), None)
        };
        let label = kind
            .label(&change.name)
            .expect("invariant: a planned change is at a tree key of its kind");
        let at = span.map(|span| (span.source(), span.start().offset));
        Self {
            action,
            kind,
            label,
            place: span.map(|span| Place::of(span, paths)),
            order: (at.is_none(), at, change.name.clone()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Add,
    Change,
    Remove,
}

impl Action {
    const fn symbol(self) -> char {
        match self {
            Self::Add => '+',
            Self::Change => '~',
            Self::Remove => '-',
        }
    }

    const fn word(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Change => "change",
            Self::Remove => "remove",
        }
    }
}

/// A file, line, and column, each line and column from 1.
#[derive(Debug)]
struct Place {
    file: String,
    line: u32,
    column: u32,
}

impl Place {
    /// The start of `span`, in the file of its source.
    fn of(span: Span, paths: &[PathBuf]) -> Self {
        let path = usize::try_from(span.source().0)
            .ok()
            .and_then(|source| paths.get(source))
            .expect("invariant: a span is in a file of the plan");
        let start = span.start();
        Self {
            file: path.display().to_string(),
            line: start.line + 1,
            column: start.column + 1,
        }
    }

    fn insert(&self, object: &mut Map<String, Value>) {
        object.insert("file".into(), self.file.as_str().into());
        object.insert("line".into(), self.line.into());
        object.insert("column".into(), self.column.into());
    }
}

/// The problems that stopped a plan, with the paths that their spans name.
#[derive(Debug)]
pub(crate) struct Problems {
    diagnostics: Vec<Diagnostic>,
    paths: Vec<PathBuf>,
}

/// `  --> <file>:<line>:<column>`, as rustc writes a place, and a new line.
impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file = escape(&self.file);
        writeln!(f, "  --> {file}:{}:{}", self.line, self.column)
    }
}

/// Each diagnostic as `error[<code>]: <message>`, its place, `fix: <fix>`, then each
/// note and its place, with one empty line between two diagnostics.
impl fmt::Display for Problems {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (diagnostic, i) in self.diagnostics.iter().zip(0..) {
            if i > 0 {
                writeln!(f)?;
            }
            writeln!(
                f,
                "error[{}]: {}",
                diagnostic.code,
                escape(&diagnostic.message)
            )?;
            if let Some(span) = diagnostic.span {
                write!(f, "{}", Place::of(span, &self.paths))?;
            }
            writeln!(f, "fix: {}", escape(&diagnostic.fix))?;
            for note in &diagnostic.notes {
                writeln!(f, "note: {}", escape(&note.text))?;
                write!(f, "{}", Place::of(note.span, &self.paths))?;
            }
        }
        Ok(())
    }
}

impl Problems {
    pub(crate) fn json(&self) -> Value {
        let errors: Vec<Value> = self
            .diagnostics
            .iter()
            .map(|diagnostic| {
                let mut error = Map::new();
                error.insert("code".into(), diagnostic.code.as_str().into());
                error.insert("message".into(), diagnostic.message.as_str().into());
                error.insert("fix".into(), diagnostic.fix.as_str().into());
                if let Some(span) = diagnostic.span {
                    Place::of(span, &self.paths).insert(&mut error);
                }
                let notes: Vec<Value> = diagnostic
                    .notes
                    .iter()
                    .map(|note| {
                        let mut object = Map::new();
                        object.insert("text".into(), note.text.as_str().into());
                        Place::of(note.span, &self.paths).insert(&mut object);
                        Value::Object(object)
                    })
                    .collect();
                error.insert("notes".into(), notes.into());
                Value::Object(error)
            })
            .collect();
        json!({ "errors": errors })
    }
}
