//! Plans the change from config files to the applied spec.

use std::collections::{BTreeMap, BTreeSet};

use document::diagnostic::{Code, Diagnostic};
use document::value::Kind;
use document::{Document, Span};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use spec::definition::Definition;
use types::name::Name;

use crate::Node;
use crate::error::{Error, Place, Sources};
use crate::front_end::{self, File, FrontEnds};

/// A source of a secret of a connector of the planning node that gives no value.
const UNRESOLVED: Code = Code::new("secret.unresolved");

#[cfg(test)]
mod tests;

/// The config files of one plan, each read into its Document.
pub(crate) struct Documents {
    sources: Sources,
    documents: Vec<Document>,
}

impl Documents {
    /// Reads each of `files` with its front end. `Source(i)` is `files[i]`.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] with each problem of [`front_end::read`].
    ///
    /// # Panics
    ///
    /// As [`front_end::read`] panics.
    pub(crate) fn read(files: &[File], front_ends: &FrontEnds) -> Result<Self, Error> {
        let sources = Sources::of(files);
        match front_end::read(files, front_ends) {
            Ok(documents) => Ok(Self { sources, documents }),
            Err(diagnostics) => Err(Error::config(diagnostics, &sources)),
        }
    }

    /// The tree key of each definition of the files, in order.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] with each problem of [`config::check`].
    pub(crate) fn names(
        &self,
        kinds: &connector::kind::Table,
    ) -> Result<Vec<Name>, Error> {
        match config::check(&self.documents, kinds) {
            Ok(entries) => Ok(entries.into_keys().collect()),
            Err(diagnostics) => Err(Error::config(diagnostics, &self.sources)),
        }
    }

    /// The change from the files to the spec at `base` of `node`, and the plan that
    /// `apply` takes. Also reads the value of each source of a secret in the config of
    /// each connector of `node` whose name `readable` takes, through the resolver of
    /// `node` as a run does, and drops it. A source is a call of `env`, `file`, or
    /// `secret`, at any depth of the config, that [`secret::Source::read`] reads. Reads
    /// one source at a time: the connectors in name order, and the sources of each in
    /// the order of its config.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] with each problem of [`config::check`] and no read when it
    /// fails; else each problem of [`config::plan::plan`], then `secret.unresolved` at
    /// the call of each source that gives no value.
    pub(crate) async fn plan(
        &self,
        node: &Node,
        base: spec::Pointer,
        applied: &BTreeMap<Name, Definition>,
        readable: impl Fn(&Name) -> bool,
    ) -> Result<(Output, config::plan::Plan), Error> {
        let entries = config::check(&self.documents, node.supervisor.kinds())
            .map_err(|diagnostics| Error::config(diagnostics, &self.sources))?;
        let mut calls = Vec::new();
        for (name, entry) in &entries {
            if let config::Definition::Spec(Definition::Connector(connector)) =
                &entry.definition
                && connector.node() == &node.name
                && readable(name)
            {
                sources(connector.config().document(), &mut calls);
            }
        }
        let mut unresolved = Vec::new();
        for (source, span) in calls {
            if let Err(error) = node.supervisor.secrets().get(&source).await {
                unresolved.push(self::unresolved(
                    span,
                    format!("the secret {source}: {error}"),
                ));
            }
        }
        let members = node.mesh.names();
        match (
            self.changes(base, applied, &members, node.supervisor.kinds()),
            unresolved.is_empty(),
        ) {
            (Ok(planned), true) => Ok(planned),
            (Ok(_), false) => Err(Error::config(unresolved, &self.sources)),
            (Err(mut diagnostics), _) => {
                diagnostics.append(&mut unresolved);
                Err(Error::config(diagnostics, &self.sources))
            }
        }
    }

    /// The change from the files to the spec at `base`, and the plan that `apply`
    /// takes, with no read of a secret.
    ///
    /// # Errors
    ///
    /// Each problem of [`config::plan::plan`].
    pub(crate) fn changes(
        &self,
        base: spec::Pointer,
        applied: &BTreeMap<Name, Definition>,
        members: &BTreeSet<Name>,
        kinds: &connector::kind::Table,
    ) -> Result<(Output, config::plan::Plan), Vec<Diagnostic>> {
        let sources = &self.sources;
        let plan = config::plan::plan(&self.documents, base, applied, members, kinds)?;
        let mut changes: Vec<(Order, Change)> = lines(&plan, applied)
            .map(|(name, line)| Change::of(name, line, sources))
            .collect();
        changes.sort_by(|(a, _), (b, _)| a.cmp(b));
        let changes: Vec<Change> =
            changes.into_iter().map(|(_, change)| change).collect();
        let output = Output {
            base: Pointer::from(plan.base),
            counts: Counts::of(&plan, applied),
            homes: plan
                .homes
                .iter()
                .map(|(index, home)| (index.to_string(), home.to_string()))
                .collect(),
            changes,
            reserved: applied
                .iter()
                .filter_map(|(name, definition)| Reserved::of(name, definition))
                .collect(),
        };
        Ok((output, plan))
    }
}

/// The `secret.unresolved` problem at `span`, the call of a source that gives no value,
/// with the text `message`.
pub(crate) fn unresolved(span: Option<Span>, message: String) -> Diagnostic {
    Diagnostic::new(
        UNRESOLVED,
        span,
        message,
        "Give the node the value at the source, or change the source, then plan again"
            .to_owned(),
    )
}

/// Adds each source of a secret in `document` to `into`, with the span of its call, in
/// the order of the document.
fn sources(document: &Document, into: &mut Vec<(secret::Source, Option<Span>)>) {
    let start = into.len();
    block_sources(document, into);
    into[start..].sort_by_key(|(_, span)| *span);
}

/// Adds each source of a secret in `document` to `into`: its attributes in key order,
/// then its blocks.
fn block_sources(document: &Document, into: &mut Vec<(secret::Source, Option<Span>)>) {
    for attribute in document.attributes.iter() {
        value_sources(&attribute.value, into);
    }
    for block in &document.blocks {
        block_sources(&block.body, into);
    }
}

/// Adds each source of a secret in `value` to `into`, as [`block_sources`] does.
fn value_sources(
    value: &document::value::Value,
    into: &mut Vec<(secret::Source, Option<Span>)>,
) {
    match &value.kind {
        Kind::Call(_) => {
            if let Ok(source) = secret::Source::read(value) {
                into.push((source, value.span));
            }
        }
        Kind::List(values) => {
            for value in values {
                value_sources(value, into);
            }
        }
        Kind::Map(map) => {
            for attribute in map.iter() {
                value_sources(&attribute.value, into);
            }
        }
        Kind::Bool(_)
        | Kind::Integer(_)
        | Kind::Float(_)
        | Kind::String(_)
        | Kind::Reference(_) => {}
    }
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
    /// Each definition of the spec that Foundation made, in the order of its tree
    /// key. No file can hold its label, so the plan leaves it out.
    pub(crate) reserved: Vec<Reserved>,
    #[serde(flatten)]
    pub(crate) counts: Counts,
}

impl Output {
    /// One line for each change, with a `key` line under it for each fingerprint, then
    /// the counts, then one `note` line with the tree key of each definition that
    /// Foundation made, when the spec holds one.
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
        let made: Vec<&str> = self
            .reserved
            .iter()
            .map(|reserved| reserved.key.as_str())
            .collect();
        let note = match made.len() {
            0 => String::new(),
            1 => format!(
                "note: 1 definition that Foundation made is not in the files: {}\n",
                made[0]
            ),
            count => format!(
                "note: {count} definitions that Foundation made are not in the files: \
                 {}\n",
                made.join(", ")
            ),
        };
        format!(
            "{}{} to add, {} to change, {} to remove.\n{note}",
            lines.concat(),
            self.counts.added,
            self.counts.changed,
            self.counts.removed
        )
    }
}

/// A definition of the spec that Foundation made, such as the admin subject of the
/// first start.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct Reserved {
    /// The label of the definition, such as `@admin`.
    pub(crate) name: String,
    /// The kind of the definition, such as `subject`.
    pub(crate) kind: String,
    /// The tree key of the definition, such as `@admin.@subject`.
    pub(crate) key: String,
}

impl Reserved {
    /// The output of `definition` at the tree key `key`, when its label is reserved.
    fn of(key: &Name, definition: &Definition) -> Option<Self> {
        let kind = definition.kind();
        let label = kind.label(key).filter(Name::reserved)?;
        Some(Self {
            name: label.to_string(),
            kind: kind.as_str().to_owned(),
            key: key.to_string(),
        })
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
    fn of(name: &Name, line: Line<'_>, sources: &Sources) -> (Order, Self) {
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
            place: span.map(|span| sources.place(span)),
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
    const fn symbol(self) -> char {
        match self {
            Self::Add => '+',
            Self::Change => '~',
            Self::Remove => '-',
        }
    }
}
