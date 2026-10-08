//! Plans the change from config files to the applied spec.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use document::diagnostic::Diagnostic;
use document::{Source, Span};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use spec::definition::{Definition, Kind};
use types::name::Name;

use crate::error::{Error, Note, Place, Problem};
use crate::front_end::{self, File, FrontEnd};

#[cfg(test)]
mod tests;

/// The change from the files to the spec at `base`. `Source(i)` is `files[i]`.
///
/// # Errors
///
/// [`Error::Config`] with each problem of [`front_end::read`], else with each problem
/// of [`config::plan`].
///
/// # Panics
///
/// As [`front_end::read`] panics.
pub(crate) fn plan(
    files: &[File],
    base: spec::Pointer,
    applied: &BTreeMap<Name, Definition>,
    members: &BTreeSet<Name>,
    front_ends: &BTreeMap<&'static str, FrontEnd>,
    kinds: &connector::kind::Table,
) -> Result<Output, Error> {
    let paths: Vec<PathBuf> = files.iter().map(|file| file.path.clone()).collect();
    let failed = |diagnostics: Vec<Diagnostic>| {
        let problems = diagnostics
            .into_iter()
            .map(|diagnostic| problem(diagnostic, &paths))
            .collect();
        Error::Config(problems)
    };
    let documents = front_end::read(files, front_ends).map_err(failed)?;
    let plan =
        config::plan(&documents, base, applied, members, kinds).map_err(failed)?;
    let mut changes: Vec<(Order, Change)> = plan
        .changes
        .iter()
        .map(|change| Change::of(change, applied, &paths))
        .collect();
    changes.sort_by(|(a, _), (b, _)| a.cmp(b));
    let changes: Vec<Change> = changes.into_iter().map(|(_, change)| change).collect();
    let count = |action| {
        changes
            .iter()
            .filter(|change| change.action == action)
            .count()
    };
    Ok(Output {
        base: Base {
            version: plan.base.version,
            root: plan.base.root.to_string(),
        },
        added: count(Action::Add),
        changed: count(Action::Change),
        removed: count(Action::Remove),
        homes: plan
            .homes
            .iter()
            .map(|(index, home)| (index.to_string(), home.to_string()))
            .collect(),
        changes,
    })
}

/// The change from the files to the spec, as `plan` gives it.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct Output {
    /// The spec that the plan changes.
    pub(crate) base: Base,
    /// Each change: the adds and changes in file order, then the removals.
    pub(crate) changes: Vec<Change>,
    /// The home node of each index that has none before the apply, by index name.
    pub(crate) homes: BTreeMap<String, String>,
    /// The count of changes with this action.
    pub(crate) added: usize,
    /// The count of changes with this action.
    pub(crate) changed: usize,
    /// The count of changes with this action.
    pub(crate) removed: usize,
}

impl Output {
    /// One line for each change, then the counts.
    pub(crate) fn text(&self) -> String {
        let lines: Vec<String> = self
            .changes
            .iter()
            .map(|change| {
                let (symbol, kind) = (change.action.symbol(), &change.kind);
                format!("{symbol} {kind} {}\n", change.name)
            })
            .collect();
        format!(
            "{}{} to add, {} to change, {} to remove.\n",
            lines.concat(),
            self.added,
            self.changed,
            self.removed
        )
    }
}

/// The version of a spec, and the root of its tree in lower-case hex.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct Base {
    /// The version of the spec: 0 before its first apply.
    pub(crate) version: u64,
    /// The root of the spec's tree, in lower-case hex.
    pub(crate) root: String,
}

/// One change of a plan.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct Change {
    /// What the apply does to the definition.
    pub(crate) action: Action,
    /// The kind of the definition, such as `channel`.
    pub(crate) kind: String,
    /// The label of the definition in the files.
    pub(crate) name: String,
    /// Where the label is in the files. A removal has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) place: Option<Place>,
}

/// Adds and changes in file order, then removals in tree key order.
type Order = (bool, Option<(Source, u32)>, Name);

impl Change {
    fn of(
        change: &config::Change,
        applied: &BTreeMap<Name, Definition>,
        paths: &[PathBuf],
    ) -> (Order, Self) {
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
        let order = (at.is_none(), at, change.name.clone());
        let change = Self {
            action,
            kind: kind.as_str().to_owned(),
            name: label.to_string(),
            place: span.map(|span| place(span, paths)),
        };
        (order, change)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Action {
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
}

/// The start of `span`, in the file of its source.
fn place(span: Span, paths: &[PathBuf]) -> Place {
    let path = usize::try_from(span.source().0)
        .ok()
        .and_then(|source| paths.get(source))
        .expect("invariant: a span is in a file of the plan");
    let start = span.start();
    Place {
        file: path
            .to_str()
            .unwrap_or_else(|| {
                unreachable!("invariant: `read` refuses a path that is not UTF-8")
            })
            .to_owned(),
        line: start.line + 1,
        column: start.column + 1,
    }
}

fn problem(diagnostic: Diagnostic, paths: &[PathBuf]) -> Problem {
    Problem {
        code: diagnostic.code.as_str().to_owned(),
        message: diagnostic.message,
        fix: diagnostic.fix,
        place: diagnostic.span.map(|span| place(span, paths)),
        notes: diagnostic
            .notes
            .into_iter()
            .map(|note| Note {
                text: note.text,
                place: place(note.span, paths),
            })
            .collect(),
    }
}
