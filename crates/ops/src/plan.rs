//! Plans the change from config files to the applied spec.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use document::Span;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use spec::definition::Definition;
use types::name::Name;

use crate::error::{Error, Place};
use crate::front_end::{self, File, FrontEnds};

#[cfg(test)]
mod tests;

/// The change from the files to the spec at `base`, and the plan that `apply` takes.
/// `Source(i)` is `files[i]`.
///
/// # Errors
///
/// [`Error::Config`] with each problem of [`front_end::read`], else with each problem
/// of [`config::plan::plan`].
///
/// # Panics
///
/// As [`front_end::read`] panics.
pub(crate) fn plan(
    files: &[File],
    base: spec::Pointer,
    applied: &BTreeMap<Name, Definition>,
    members: &BTreeSet<Name>,
    front_ends: &FrontEnds,
    kinds: &connector::kind::Table,
) -> Result<(Output, config::plan::Plan), Error> {
    let paths: Vec<PathBuf> = files.iter().map(|file| file.path.clone()).collect();
    let failed = |diagnostics| Error::config(diagnostics, &paths);
    let documents = front_end::read(files, front_ends).map_err(failed)?;
    let plan = config::plan::plan(&documents, base, applied, members, kinds)
        .map_err(failed)?;
    let mut changes: Vec<(Order, Change)> = lines(&plan, applied)
        .map(|(name, line)| Change::of(name, line, &paths))
        .collect();
    changes.sort_by(|(a, _), (b, _)| a.cmp(b));
    let changes: Vec<Change> = changes.into_iter().map(|(_, change)| change).collect();
    let output = Output {
        base: Pointer::from(plan.base),
        counts: Counts::of(&plan, applied),
        homes: plan
            .homes
            .iter()
            .map(|(index, home)| (index.to_string(), home.to_string()))
            .collect(),
        changes,
    };
    Ok((output, plan))
}

/// The change from the files to the spec, as `plan` gives it.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct Output {
    /// The spec that the plan changes.
    pub(crate) base: Pointer,
    /// Each change: the adds and changes in file order, then the removals.
    pub(crate) changes: Vec<Change>,
    /// The home node of each index of the files, as the placements give it, by index
    /// name. The apply gives this home only to an index with no home, so an index with
    /// a home keeps it.
    pub(crate) homes: BTreeMap<String, String>,
    #[serde(flatten)]
    pub(crate) counts: Counts,
}

impl Output {
    /// The output as JSON.
    pub(crate) fn json(&self) -> Value {
        serde_json::to_value(self).expect("invariant: an output is plain JSON data")
    }

    /// One line for each change, with a `key` line under it for each fingerprint, then
    /// the counts.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the table entries of #1744 call it")
    )]
    pub(crate) fn text(&self) -> String {
        let lines: Vec<String> = self
            .changes
            .iter()
            .map(|change| {
                let (symbol, kind) = (change.action.symbol(), &change.kind);
                let lines: String = change
                    .fingerprints
                    .iter()
                    .flat_map(|fingerprint| ["    key ", fingerprint, "\n"])
                    .collect();
                format!("{symbol} {kind} {}\n{lines}", change.name)
            })
            .collect();
        format!(
            "{}{} to add, {} to change, {} to remove.\n",
            lines.concat(),
            self.counts.added,
            self.counts.changed,
            self.counts.removed
        )
    }
}

/// The count of changes with each action.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct Counts {
    /// The count of new definitions.
    pub(crate) added: usize,
    /// The count of definitions that stay with a new value.
    pub(crate) changed: usize,
    /// The count of definitions that go.
    pub(crate) removed: usize,
}

impl Counts {
    /// The count of each action that `plan` shows for `planned` on `applied`, the
    /// definitions at its base.
    ///
    /// # Panics
    ///
    /// When `planned` changes or removes a definition that `applied` does not hold,
    /// which [`config::plan::Plan::definitions`] refuses.
    pub(crate) fn of(
        planned: &config::plan::Plan,
        applied: &BTreeMap<Name, Definition>,
    ) -> Self {
        let mut counts = Self {
            added: 0,
            changed: 0,
            removed: 0,
        };
        for (_, line) in lines(planned, applied) {
            let count = match line.action() {
                Action::Add => &mut counts.added,
                Action::Change => &mut counts.changed,
                Action::Remove => &mut counts.removed,
            };
            *count += 1;
        }
        counts
    }
}

/// The version of a spec, and the root of its tree in lower-case hex.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct Pointer {
    /// The version of the spec: 0 before its first apply.
    pub(crate) version: u64,
    /// The root of the spec's tree, in lower-case hex.
    pub(crate) root: String,
}

impl From<spec::Pointer> for Pointer {
    fn from(pointer: spec::Pointer) -> Self {
        Self {
            version: pointer.version,
            root: pointer.root.to_string(),
        }
    }
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
    /// The `SHA256:` fingerprint of each key of a subject, as `ssh-keygen -l` writes
    /// it: after the apply, or before it for a removal. Empty for each other kind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) fingerprints: Vec<String>,
}

/// Adds and changes in file order, then removals in tree key order.
type Order = (bool, Option<Span>, Name);

/// One line of a plan at a tree key.
#[derive(Clone, Copy)]
enum Line<'a> {
    /// A new definition.
    Add(&'a config::Entry),
    /// A new value of a stored definition of the same kind.
    Change(&'a config::Entry),
    /// The removal of a stored definition.
    Remove(&'a Definition),
}

impl Line<'_> {
    const fn action(self) -> Action {
        match self {
            Self::Add(_) => Action::Add,
            Self::Change(_) => Action::Change,
            Self::Remove(_) => Action::Remove,
        }
    }
}

/// Each line of `planned` on `applied`, by tree key. A change that replaces a stored
/// definition of another kind gives a removal and an add.
fn lines<'a>(
    planned: &'a config::plan::Plan,
    applied: &'a BTreeMap<Name, Definition>,
) -> impl Iterator<Item = (&'a Name, Line<'a>)> {
    planned.changes.iter().flat_map(|(name, change)| {
        let stored = || {
            applied
                .get(name)
                .unwrap_or_else(|| panic!("invariant: no applied definition at {name}"))
        };
        let lines = match (&change.old, &change.new) {
            (None, Some(new)) => vec![Line::Add(new)],
            (Some(_), Some(new)) => {
                let stored = stored();
                if stored.kind() == new.definition.kind() {
                    vec![Line::Change(new)]
                } else {
                    vec![Line::Remove(stored), Line::Add(new)]
                }
            }
            (_, None) => vec![Line::Remove(stored())],
        };
        lines.into_iter().map(move |line| (name, line))
    })
}

impl Change {
    /// The output of `line` at the tree key `name`.
    fn of(name: &Name, line: Line<'_>, paths: &[PathBuf]) -> (Order, Self) {
        let (kind, span, definition) = match line {
            Line::Add(entry) | Line::Change(entry) => {
                let definition =
                    if let config::Definition::Spec(definition) = &entry.definition {
                        Some(definition)
                    } else {
                        None
                    };
                (entry.definition.kind(), entry.label_span, definition)
            }
            Line::Remove(stored) => (stored.kind(), None, Some(stored)),
        };
        let fingerprints = match definition {
            Some(Definition::Subject(subject)) => subject
                .keys()
                .iter()
                .map(|&key| config::openssh::fingerprint(key))
                .collect(),
            _ => Vec::new(),
        };
        let label = kind
            .label(name)
            .expect("invariant: a planned change is at a tree key of its kind");
        let order = (span.is_none(), span, name.clone());
        let change = Self {
            action: line.action(),
            kind: kind.as_str().to_owned(),
            name: label.to_string(),
            place: span.map(|span| Place::of(span, paths)),
            fingerprints,
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the table entries of #1744 call it")
    )]
    const fn symbol(self) -> char {
        match self {
            Self::Add => '+',
            Self::Change => '~',
            Self::Remove => '-',
        }
    }
}
