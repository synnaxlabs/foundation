//! Plans the change from config files to the applied spec.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use document::Source;
use document::diagnostic::Diagnostic;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use spec::definition::{Definition, Kind};
use types::name::Name;

use crate::error::{Error, Place, Problem};
use crate::front_end::{self, File, FrontEnd};

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
    front_ends: &BTreeMap<&'static str, FrontEnd>,
    kinds: &connector::kind::Table,
) -> Result<(Output, config::plan::Plan), Error> {
    let paths: Vec<PathBuf> = files.iter().map(|file| file.path.clone()).collect();
    let failed = |diagnostics: Vec<Diagnostic>| {
        let problems = diagnostics
            .into_iter()
            .map(|diagnostic| Problem::of(diagnostic, &paths))
            .collect();
        Error::Config(problems)
    };
    let documents = front_end::read(files, front_ends).map_err(failed)?;
    let plan = config::plan::plan(&documents, base, applied, members, kinds)
        .map_err(failed)?;
    let mut changes: Vec<(Order, Change)> = plan
        .changes
        .iter()
        .map(|(name, change)| Change::of(name, change, applied, &paths))
        .collect();
    changes.sort_by(|(a, _), (b, _)| a.cmp(b));
    let changes: Vec<Change> = changes.into_iter().map(|(_, change)| change).collect();
    let output = Output {
        base: Pointer::from(plan.base),
        counts: Counts::of(changes.iter().map(|change| change.action)),
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
    /// The count of each action in `actions`.
    pub(crate) fn of(actions: impl Iterator<Item = Action>) -> Self {
        let mut counts = Self {
            added: 0,
            changed: 0,
            removed: 0,
        };
        for action in actions {
            let count = match action {
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
type Order = (bool, Option<(Source, u32)>, Name);

impl Change {
    fn of(
        name: &Name,
        change: &config::plan::Change,
        applied: &BTreeMap<Name, Definition>,
        paths: &[PathBuf],
    ) -> (Order, Self) {
        let (action, kind, span, definition) = if let Some(entry) = &change.new {
            let (kind, definition) = match &entry.definition {
                config::Definition::Spec(definition) => {
                    (definition.kind(), Some(definition))
                }
                config::Definition::Channel(_) => (Kind::Channel, None),
                _ => unreachable!("invariant: `ops` knows each kind of definition"),
            };
            (Action::of(change), kind, entry.label_span, definition)
        } else {
            let stored = applied
                .get(name)
                .expect("invariant: a removal is of an applied definition");
            (Action::of(change), stored.kind(), None, Some(stored))
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
        let at = span.map(|span| (span.source(), span.start().offset));
        let order = (at.is_none(), at, name.clone());
        let change = Self {
            action,
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
    /// What `change` does: it removes with no new definition, else changes with an
    /// old one, else adds.
    pub(crate) const fn of(change: &config::plan::Change) -> Self {
        match (&change.old, &change.new) {
            (_, None) => Self::Remove,
            (Some(_), Some(_)) => Self::Change,
            (None, Some(_)) => Self::Add,
        }
    }

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
