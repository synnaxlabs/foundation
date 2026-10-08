use std::collections::{BTreeMap, BTreeSet};

use ::connector::kind::Table;
use document::Document;
use document::diagnostic::{Code, Diagnostic};
use spec::channel::{Channel, Problem};
use spec::definition;
use spec::placement::{Policy, place};
use spec::tree::{self, Chunks};
use types::channel::Key;
use types::digest::Digest;
use types::name::Name;

use crate::{Definition, Entry, Found, channel, checked, sort, span};

const UNKNOWN_NODE: Code = Code::new("config.unknown-node");
const UNPLACED: Code = Code::new("config.unplaced");
const WRITER_NODES: Code = Code::new("config.writer-nodes");
const WRONG_CHANNEL: Code = Code::new("config.wrong-channel");

/// The change from the applied spec of the root region to the definitions in
/// `documents`. `members` names each node in the mesh. `kinds` checks each connector
/// block and gives the channels it writes.
///
/// A channel keeps the key of the stored channel at its name, so a renamed channel is
/// removed and added, and each channel with an edge to it changes. A definition of the
/// applied spec whose label is reserved, which only Foundation makes, is never a
/// change.
///
/// # Errors
///
/// When the Documents have a problem, every problem as [`check`](crate::check) gives
/// them. Else:
///
/// - `config.wrong-channel` at each edge to a channel that is not what the edge needs.
/// - `config.unplaced` at the label of each index that
///   [`spec::placement::place`] cannot place.
/// - `config.writer-nodes` at the `node` of the first connector on a second node that
///   writes an index or a channel on it.
/// - `config.unknown-node` at each node that a connector or a placement names and that
///   is not in `members`. The fix names a member that is equal to it without case.
///
/// These come in the order of their [`document::Source`], then in source order.
///
/// # Panics
///
/// When `chunks` lacks a chunk of the tree at `applied.root`, or holds one that does
/// not decode. The caller gives the whole applied tree.
pub fn plan(
    documents: &[Document],
    applied: spec::Pointer,
    chunks: &Chunks,
    members: &BTreeSet<Name>,
    kinds: &Table,
) -> Result<Plan, Vec<Diagnostic>> {
    let found = checked(documents, kinds)?;
    let stored = stored(chunks, applied.root);
    let channels = channels(&found.entries, &stored);
    let mut diagnostics = wrong(&found, &channels);
    let homes = homes(&found, &stored, &mut diagnostics);
    unknown(&found, members, &mut diagnostics);
    if !diagnostics.is_empty() {
        sort(&mut diagnostics);
        return Err(diagnostics);
    }
    Ok(Plan {
        base: applied,
        changes: changes(found.entries, channels, stored),
        homes,
    })
}

/// The change that an apply makes, and the spec it was planned on.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Plan {
    /// The pointer of the applied spec. An apply refuses the plan when the region's
    /// pointer is another one.
    pub base: spec::Pointer,
    /// Each change, in tree key order. Empty when the files match the spec.
    pub changes: Vec<Change>,
    /// The home node of each index that has none before the apply, by index name: a
    /// new index, or a data channel that becomes one.
    pub homes: BTreeMap<Name, Name>,
}

/// One definition that the plan adds, changes, or removes.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Change {
    /// The tree key.
    pub name: Name,
    /// The digest of the stored definition, or `None` when the plan adds it.
    pub old: Option<Digest>,
    /// The definition in the files, with the span of its label, or `None` when the
    /// plan removes it.
    pub new: Option<Entry>,
}

/// The definitions of an applied tree, with their bytes, by tree key.
type Stored<'c> = BTreeMap<Name, (&'c [u8], definition::Definition)>;

/// Each definition of the tree at `root` whose label is not reserved.
fn stored(chunks: &Chunks, root: Digest) -> Stored<'_> {
    let diff = tree::diff(chunks, tree::empty(), root).unwrap_or_else(|error| {
        panic!("cannot read the applied tree at {root}: {error}")
    });
    let mut stored = Stored::new();
    for changed in diff.changes {
        let bytes = changed.new.expect("invariant: the empty tree has no entry");
        let definition =
            definition::Definition::decode(bytes).unwrap_or_else(|error| {
                panic!(
                    "the applied definition `{}` does not decode: {error}",
                    changed.name
                )
            });
        let label = definition.kind().label(&changed.name);
        if !label.is_some_and(|label| label.reserved()) {
            stored.insert(changed.name, (bytes, definition));
        }
    }
    stored
}

/// The channel of each `channel` entry. It keeps the key of the stored channel at its
/// name. Else it gets a key that no stored key can be, since each stored key is v7.
fn channels(
    entries: &BTreeMap<Name, Entry>,
    stored: &Stored<'_>,
) -> BTreeMap<Name, Channel> {
    let mut made = 0;
    let keys: BTreeMap<&Name, Key> = entries
        .iter()
        .filter(|(_, entry)| matches!(entry.definition, Definition::Channel(_)))
        .map(|(name, _)| {
            if let Some((_, definition::Definition::Channel(channel))) =
                stored.get(name)
            {
                return (name, channel.key);
            }
            made += 1;
            (name, Key::from_u128(made))
        })
        .collect();
    let mut channels = BTreeMap::new();
    for (name, entry) in entries {
        let Definition::Channel(kind) = &entry.definition else {
            continue;
        };
        let kind = kind.clone().map(|to| {
            *keys.get(&to).unwrap_or_else(|| {
                panic!(
                    "invariant: `check` refuses the edge to `{to}`, which no block \
                     defines"
                )
            })
        });
        channels.insert(
            name.clone(),
            Channel {
                key: keys[name],
                kind,
            },
        );
    }
    channels
}

/// A `config.wrong-channel` diagnostic for each edge to a channel that is not what the
/// edge needs.
fn wrong(found: &Found<'_>, channels: &BTreeMap<Name, Channel>) -> Vec<Diagnostic> {
    let diagnostic = |problem: Problem| {
        let Problem::Wrong { from, edge, .. } = &problem else {
            panic!(
                "invariant: each edge points at a channel of the files, not {problem:?}"
            );
        };
        let span = span(found.blocks[from], channel::attribute(*edge));
        Diagnostic::new(
            WRONG_CHANNEL,
            span,
            problem.to_string(),
            problem.fix().into(),
        )
    };
    spec::channel::check(channels)
        .into_iter()
        .map(diagnostic)
        .collect()
}

/// The home of each index that the stored spec has no index at. Reports each index
/// that two writer nodes or [`place`] leave with no home.
fn homes(
    found: &Found<'_>,
    stored: &Stored<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> BTreeMap<Name, Name> {
    let placements: Vec<(&Name, &Policy)> = found
        .entries
        .iter()
        .filter_map(|(name, entry)| match &entry.definition {
            Definition::Spec(definition::Definition::Placement(policy)) => {
                Some((name, policy))
            }
            _ => None,
        })
        .collect();
    let mut homes = BTreeMap::new();
    for (index, entry) in &found.entries {
        let Definition::Channel(spec::channel::Kind::Index { .. }) = entry.definition
        else {
            continue;
        };
        let writer = writer(found, index, diagnostics);
        match place(index, placements.iter().copied(), writer) {
            Ok(placed) => {
                let stored = stored.get(index).map(|(_, definition)| definition);
                let indexed = matches!(
                    stored,
                    Some(definition::Definition::Channel(Channel {
                        kind: spec::channel::Kind::Index { .. },
                        ..
                    }))
                );
                if !indexed {
                    homes.insert(index.clone(), placed.home.clone());
                }
            }
            Err(unplaced) => diagnostics.push(Diagnostic::new(
                UNPLACED,
                entry.label_span,
                unplaced.to_string(),
                unplaced.fix().into(),
            )),
        }
    }
    homes
}

/// The node of the first connector that writes `index` or a channel on it. Reports
/// `config.writer-nodes` at the first such connector on another node.
fn writer<'f>(
    found: &'f Found<'_>,
    index: &Name,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<&'f Name> {
    let on = |name: &Name| match &found.entries.get(name)?.definition {
        Definition::Channel(spec::channel::Kind::Data(data)) => Some(data.index()),
        _ => None,
    };
    let mut writers = found.writers.iter().filter(|writer| {
        writer
            .writes
            .iter()
            .any(|name| name == index || on(name) == Some(index))
    });
    let first = writers.next()?;
    if let Some(second) = writers.find(|writer| writer.node != first.node) {
        diagnostics.push(Diagnostic::new(
            WRITER_NODES,
            second.at,
            format!(
                "connectors on the nodes `{}` and `{}` write the index `{index}`, so \
                 it has no one home",
                first.node, second.node
            ),
            format!("Run each connector that writes `{index}` on one node"),
        ));
    }
    Some(&first.node)
}

/// Reports `config.unknown-node` at each node that a connector or a placement names and
/// that is not in `members`.
fn unknown(
    found: &Found<'_>,
    members: &BTreeSet<Name>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let writers = found.writers.iter().map(|writer| (&writer.node, writer.at));
    let placed = found.nodes.iter().map(|(node, at)| (node, *at));
    for (node, at) in writers.chain(placed) {
        if members.contains(node) {
            continue;
        }
        let member = members
            .iter()
            .find(|member| member.as_str().eq_ignore_ascii_case(node.as_str()));
        let fix = match member {
            Some(member) => format!("Write `{member}`, the name of the node"),
            None => "Name a node of the mesh".into(),
        };
        diagnostics.push(Diagnostic::new(
            UNKNOWN_NODE,
            at,
            format!("no node of the mesh is named `{node}`"),
            fix,
        ));
    }
}

/// The change of each definition whose bytes differ from the stored bytes, in tree key
/// order.
fn changes(
    entries: BTreeMap<Name, Entry>,
    mut channels: BTreeMap<Name, Channel>,
    mut stored: Stored<'_>,
) -> Vec<Change> {
    let mut changes = Vec::new();
    for (name, entry) in entries {
        let bytes = match &entry.definition {
            Definition::Spec(definition) => definition.encode(),
            Definition::Channel(_) => {
                let channel = channels.remove(&name);
                let channel =
                    channel.expect("invariant: each channel entry has a channel");
                definition::Definition::Channel(channel).encode()
            }
        };
        let old = stored.remove(&name).map(|(old, _)| old);
        if old != Some(bytes.as_slice()) {
            changes.push(Change {
                name,
                old: old.map(Digest::of),
                new: Some(entry),
            });
        }
    }
    changes.extend(stored.into_iter().map(|(name, (old, _))| Change {
        name,
        old: Some(Digest::of(old)),
        new: None,
    }));
    changes.sort_by(|a, b| a.name.cmp(&b.name));
    changes
}
