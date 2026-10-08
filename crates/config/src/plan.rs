use std::collections::{BTreeMap, BTreeSet};

use ::connector::kind::Table;
use document::diagnostic::{Code, Diagnostic};
use document::{Document, Span};
use spec::channel::{Channel, Problem};
use spec::definition;
use spec::placement::{Placed, Policy, Unplaced, place};
use types::channel::Key;
use types::digest::Digest;
use types::name::Name;

use crate::{Definition, Entry, Found, KINDS, channel, checked, sort, span};

const CONNECTOR_HOME: Code = Code::new("config.connector-home");
const SPLIT_PLACEMENT: Code = Code::new("config.split-placement");
const UNKNOWN_NODE: Code = Code::new("config.unknown-node");
const UNPLACED: Code = Code::new("config.unplaced");
const WRITER_NODES: Code = Code::new("config.writer-nodes");
const WRONG_CHANNEL: Code = Code::new("config.wrong-channel");

/// The change from the applied spec of the root region to the definitions in
/// `documents`. `applied` is the definitions of the spec at `base`, by tree key, with
/// no problem from [`spec::region::check`]: the spec that a node uses. `members` names
/// each node in the mesh. `kinds` checks each connector block and gives the channels
/// it writes.
///
/// A channel keeps the key of the stored channel at its name, so a renamed channel is
/// removed and added, and each channel with an edge to it changes. A definition of the
/// applied spec whose label is reserved, which only Foundation makes, or whose kind no
/// block of a file defines, is never a change.
///
/// # Errors
///
/// When the Documents have a problem, every problem as [`check`](crate::check) gives
/// them. Else:
///
/// - `config.wrong-channel` at each edge to a channel that is not what the edge needs.
/// - `config.unplaced` at the label of each index and each connector that
///   [`spec::placement::place`] cannot place.
/// - `config.connector-home` at the `home` of a placement that wins for a connector and
///   names a node other than the connector's `node`.
/// - `config.split-placement` at each index when the placement that wins for it is not
///   the one that wins for its nearest connector, the connector with the longest name
///   above it: at the label of the index's placement, or of the connector's when no
///   placement selects the index.
/// - `config.writer-nodes` at the `node` of the first connector on a second node that
///   writes an index or a channel on it.
/// - `config.unknown-node` at each node that a connector or a placement names and that
///   is not in `members`. The fix names a member that is equal to it without case.
///
/// These come in the order of their [`document::Source`], then in source order.
pub fn plan(
    documents: &[Document],
    base: spec::Pointer,
    applied: &BTreeMap<Name, definition::Definition>,
    members: &BTreeSet<Name>,
    kinds: &Table,
) -> Result<Plan, Vec<Diagnostic>> {
    let found = checked(documents, kinds)?;
    let channels = channels(&found.entries, applied);
    let mut diagnostics = wrong(&found, &channels);
    let placements = placements(&found);
    let indexes = indexes(&found, &placements, &mut diagnostics);
    connectors(&found, &placements, &indexes, &mut diagnostics);
    unknown(&found, members, &mut diagnostics);
    if !diagnostics.is_empty() {
        sort(&mut diagnostics);
        return Err(diagnostics);
    }
    let homes = homes(indexes);
    Ok(Plan {
        base,
        changes: changes(found.entries, channels, applied),
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
    /// The home node of each index of the files, as the placements give it, by index
    /// name. The apply gives this home only to an index with no home, so an index with
    /// a home keeps it.
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

/// The channel of each `channel` entry. It keeps the key of the stored channel at its
/// name. Else it gets `Key::from_u128(n)` for the least `n` that no stored channel or
/// earlier entry holds.
fn channels(
    entries: &BTreeMap<Name, Entry>,
    applied: &BTreeMap<Name, definition::Definition>,
) -> BTreeMap<Name, Channel> {
    let held: BTreeSet<Key> = applied
        .values()
        .filter_map(|definition| match definition {
            definition::Definition::Channel(channel) => Some(channel.key),
            _ => None,
        })
        .collect();
    let mut made = (1..).map(Key::from_u128).filter(|key| !held.contains(key));
    let keys: BTreeMap<&Name, Key> = entries
        .iter()
        .filter(|(_, entry)| matches!(entry.definition, Definition::Channel(_)))
        .map(|(name, _)| match applied.get(name) {
            Some(definition::Definition::Channel(channel)) => (name, channel.key),
            _ => (
                name,
                made.next().expect("invariant: fewer than 2^128 channels"),
            ),
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

/// Each placement in the files, with its tree key.
fn placements<'f>(found: &'f Found<'_>) -> Vec<(&'f Name, &'f Policy)> {
    found
        .entries
        .iter()
        .filter_map(|(name, entry)| match &entry.definition {
            Definition::Spec(definition::Definition::Placement(policy)) => {
                Some((name, policy))
            }
            _ => None,
        })
        .collect()
}

/// The node of an index's first writer, and where [`place`] puts the index.
type Index<'f> = (Option<&'f Name>, Result<Placed<'f>, Unplaced>);

/// Places each index, with the node of its first writer. Reports each index that two
/// writer nodes or [`place`] leave with no home.
fn indexes<'f>(
    found: &'f Found<'_>,
    placements: &[(&'f Name, &'f Policy)],
    diagnostics: &mut Vec<Diagnostic>,
) -> BTreeMap<&'f Name, Index<'f>> {
    let mut indexes = BTreeMap::new();
    for (index, entry) in &found.entries {
        let Definition::Channel(spec::channel::Kind::Index { .. }) = entry.definition
        else {
            continue;
        };
        let writer = writer(found, index, diagnostics);
        let placed = place(index, placements.iter().copied(), writer);
        if let Err(problem) = &placed {
            diagnostics.push(unplaced(entry.label_span, problem));
        }
        indexes.insert(index, (writer, placed));
    }
    indexes
}

/// The home of each placed index.
fn homes(indexes: BTreeMap<&Name, Index<'_>>) -> BTreeMap<Name, Name> {
    indexes
        .into_iter()
        .filter_map(|(index, (_, placed))| {
            Some((index.clone(), placed.ok()?.home.clone()))
        })
        .collect()
}

/// A connector's key, entry, and node, and where [`place`] puts it.
type Connector<'f> = (&'f Name, &'f Entry, &'f Name, Result<Placed<'f>, Unplaced>);

/// An index, the node of its first writer, where [`place`] puts it, and its nearest
/// connector.
type Nearest<'f, 'c> = (
    &'f Name,
    Option<&'f Name>,
    &'f Result<Placed<'f>, Unplaced>,
    &'c Connector<'f>,
);

/// Places each connector, with its `node` as the writer. Reports
/// `config.connector-home` at the `home` of a winner that names another node,
/// `config.unplaced` at each connector that [`place`] cannot place, and
/// `config.split-placement` at each index whose nearest connector above its name has
/// another winner.
fn connectors<'f>(
    found: &'f Found<'_>,
    placements: &[(&'f Name, &'f Policy)],
    indexes: &'f BTreeMap<&'f Name, Index<'f>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let connectors: Vec<_> = found
        .entries
        .iter()
        .filter_map(|(name, entry)| match &entry.definition {
            Definition::Spec(definition::Definition::Connector(connector)) => {
                let node = connector.node();
                let placed = place(name, placements.iter().copied(), Some(node));
                Some((name, entry, node, placed))
            }
            _ => None,
        })
        .collect();
    let nearest: Vec<_> = indexes
        .iter()
        .filter_map(|(index, (writer, own))| {
            let connector = connectors
                .iter()
                .filter(|(name, ..)| index.starts_with(name))
                .max_by_key(|(name, ..)| name.segments().count())?;
            Some((*index, *writer, own, connector))
        })
        .collect();
    let moves = moves(&connectors, &nearest, placements);
    for (name, entry, node, placed) in &connectors {
        match placed {
            Ok(Placed {
                placement: Some(placement),
                home,
                ..
            }) if home != node => {
                let fix = moves.get(name).map(String::as_str);
                diagnostics
                    .push(connector_home(found, placement, home, name, node, fix));
            }
            Ok(_) => {}
            Err(problem) => diagnostics.push(unplaced(entry.label_span, problem)),
        }
    }
    for (index, _, own, (connector, _, _, theirs)) in nearest {
        if let (Ok(own), Ok(theirs)) = (winner(own), winner(theirs)) {
            let fix = moves.get(connector).map(String::as_str);
            diagnostics.extend(split(found, index, own, connector, theirs, fix));
        }
    }
}

/// The one fix of each diagnostic of each connector that no placement can win for
/// with each of its indexes at the connector's node, or whose placement to win names no
/// `home` while an index of the connector has no writer, by connector. `nearest` holds
/// the nearest connector of each index. The fix names each winner, the connector's
/// first.
fn moves<'c>(
    connectors: &'c [Connector<'_>],
    nearest: &[Nearest<'_, 'c>],
    placements: &[(&Name, &Policy)],
) -> BTreeMap<&'c Name, String> {
    let mut nodes = BTreeMap::<_, BTreeSet<_>>::new();
    for (_, _, node, placed) in connectors {
        if let Ok(Some(placement)) = winner(placed) {
            nodes.entry(placement).or_default().insert(*node);
        }
    }
    let mut owners = BTreeMap::<_, BTreeSet<_>>::new();
    let mut unwritten = BTreeSet::new();
    for (_, writer, own, (connector, _, node, _)) in nearest {
        if let Ok(Some(own)) = winner(own) {
            owners.entry(*connector).or_default().insert(own);
            nodes.entry(own).or_default().insert(*node);
        }
        if writer.is_none() {
            unwritten.insert(*connector);
        }
    }
    let home = |placement: &Name| {
        placements
            .iter()
            .find(|(key, _)| *key == placement)
            .and_then(|(_, policy)| policy.home())
    };
    let elsewhere = |placement: &Name, node: &Name| {
        home(placement).is_some_and(|home| home != node)
    };
    let mut moves = BTreeMap::new();
    for (name, _, node, placed) in connectors {
        let Ok(placement) = winner(placed) else {
            continue;
        };
        let owners: Vec<_> = owners
            .remove(*name)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let spread = |p: &Name| nodes[p].iter().any(|other| other != node);
        let unhoused = |p: &Name| unwritten.contains(*name) && home(p).is_none();
        let fix = match (placement, owners.as_slice()) {
            (Some(p), _) if (elsewhere(p, node) || unhoused(p)) && spread(p) => {
                let others = owners.iter().copied().filter(|owner| *owner != p);
                format!(
                    "Exclude the connector `{name}` and its indexes from the \
                     `select` of {}, and select them with another placement whose \
                     `home` is `{node}`",
                    each(&[p].into_iter().chain(others).collect::<Vec<_>>())
                )
            }
            (Some(p), _) | (None, &[p]) if unhoused(p) && !spread(p) => {
                let p = label(p);
                format!(
                    "Name `{node}` as the `home` of `{p}`, keep `{node}` out of its \
                     `standby` and `copies`, and make `{p}` win for the connector \
                     `{name}` and its indexes"
                )
            }
            (None, &[p]) if !elsewhere(p, node) && !unhoused(p) => continue,
            (None, [_, ..]) => format!(
                "Exclude the indexes of the connector `{name}` from the `select` of \
                 {}, and select the connector and its indexes with another placement \
                 whose `home` is `{node}`",
                each(&owners)
            ),
            _ => continue,
        };
        moves.insert(*name, fix);
    }
    moves
}

/// Names each placement in `keys`, in order, by its label: "`p`", "`p` and `q`", or
/// "`p`, `q`, and `r`".
fn each(keys: &[&Name]) -> String {
    let labels: Vec<_> = keys.iter().map(|key| format!("`{}`", label(key))).collect();
    match labels.as_slice() {
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
        [] => unreachable!("invariant: a moved connector has a winner"),
    }
}

/// The `config.connector-home` diagnostic of `connector` on `node`, whose winner
/// `placement` names `home`. `moved` is the fix of each diagnostic of `connector` when
/// `placement` also wins for a connector, or an index of one, on another node, which a
/// new `home` would move the problem to.
fn connector_home(
    found: &Found<'_>,
    placement: &Name,
    home: &Name,
    connector: &Name,
    node: &Name,
    moved: Option<&str>,
) -> Diagnostic {
    let p = label(placement);
    let fix = moved.map_or_else(
        || {
            format!(
                "Name `{node}` as the `home`, and keep `{node}` out of `standby` and \
                 `copies`"
            )
        },
        str::to_owned,
    );
    Diagnostic::new(
        CONNECTOR_HOME,
        span(found.blocks[placement], "home"),
        format!(
            "the placement `{p}` names the home `{home}`, but the connector \
             `{connector}` runs on the node `{node}`"
        ),
        fix,
    )
}

/// A `config.split-placement` diagnostic when `own`, the placement that wins for
/// `index`, is not `theirs`, the one that wins for the connector `connector`. `moved`
/// is the fix that [`moves`] gives `connector`, if any.
fn split(
    found: &Found<'_>,
    index: &Name,
    own: Option<&Name>,
    connector: &Name,
    theirs: Option<&Name>,
    moved: Option<&str>,
) -> Option<Diagnostic> {
    let (at, message) = match (own, theirs) {
        (Some(own), Some(theirs)) if own != theirs => (
            own,
            format!(
                "the placement `{}` wins for the index `{index}`, but the placement \
                 `{}` wins for the connector `{connector}`",
                label(own),
                label(theirs)
            ),
        ),
        (Some(own), None) => (
            own,
            format!(
                "the placement `{}` wins for the index `{index}`, but no placement \
                 selects the connector `{connector}`",
                label(own)
            ),
        ),
        (None, Some(theirs)) => (
            theirs,
            format!(
                "no placement selects the index `{index}`, but the placement `{}` \
                 wins for the connector `{connector}`",
                label(theirs)
            ),
        ),
        _ => return None,
    };
    let fix = moved.map_or_else(
        || {
            format!(
                "Make the placement `{}` win for the connector `{connector}` and its \
                 indexes",
                label(theirs.unwrap_or(at))
            )
        },
        str::to_owned,
    );
    Some(Diagnostic::new(
        SPLIT_PLACEMENT,
        found.entries[at].label_span,
        message,
        fix,
    ))
}

/// The tree key of the placement that wins for the name that `placed` places, or
/// `None` when no placement selects it. Gives the tie when two placements tie.
fn winner<'p>(
    placed: &'p Result<Placed<'_>, Unplaced>,
) -> Result<Option<&'p Name>, &'p Unplaced> {
    match placed {
        Ok(placed) => Ok(placed.placement),
        Err(Unplaced::NoHome { placement }) => Ok(placement.as_ref()),
        Err(Unplaced::Overlap { placement, .. }) => Ok(Some(placement)),
        Err(tie @ Unplaced::Tie { .. }) => Err(tie),
    }
}

/// A `config.unplaced` diagnostic at `at`, the label of the name that `problem` names.
fn unplaced(at: Option<Span>, problem: &Unplaced) -> Diagnostic {
    Diagnostic::new(UNPLACED, at, problem.to_string(), problem.fix().into())
}

/// The label of the placement at the tree key `key`.
fn label(key: &Name) -> Name {
    definition::Kind::Placement
        .label(key)
        .expect("invariant: the key of a placement block has its label form")
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
/// order. A stored definition whose label is reserved, or whose kind no block defines,
/// is never removed.
fn changes(
    entries: BTreeMap<Name, Entry>,
    mut channels: BTreeMap<Name, Channel>,
    applied: &BTreeMap<Name, definition::Definition>,
) -> Vec<Change> {
    let mut stored: BTreeMap<&Name, &definition::Definition> = applied
        .iter()
        .filter(|(name, definition)| {
            let kind = definition.kind();
            KINDS.iter().any(|(block, _)| *block == kind)
                && !kind.label(name).is_some_and(|label| label.reserved())
        })
        .collect();
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
        let old = stored.remove(&name).map(definition::Definition::encode);
        if old.as_ref() != Some(&bytes) {
            changes.push(Change {
                name,
                old: old.as_deref().map(Digest::of),
                new: Some(entry),
            });
        }
    }
    changes.extend(stored.into_iter().map(|(name, old)| Change {
        name: name.clone(),
        old: Some(Digest::of(&old.encode())),
        new: None,
    }));
    changes.sort_by(|a, b| a.name.cmp(&b.name));
    changes
}
