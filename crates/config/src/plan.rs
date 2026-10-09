//! The change from a mesh's spec to its config files, and the plan file that holds it.

mod codec;

use std::collections::{BTreeMap, BTreeSet};

use ::connector::kind::Table;
use document::diagnostic::{Code, Diagnostic};
use document::{Block, Document, Span};
use spec::channel::{Channel, Data, Problem};
use spec::definition;
use spec::placement::{Placed, Policy, Tie, place};
use types::channel::Key;
use types::digest::Digest;
use types::name::Name;

use crate::{
    Definition, Entry, Found, KINDS, channel, checked, duplicate, placement,
    private_key, sort, span, subject,
};

pub use codec::Error;

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
///   [`spec::placement::place`] gives a `Tie` for, or a `Homeless` `home`.
/// - `config.connector-home` at the `home` of a placement that wins for a connector and
///   names a node other than the connector's `node`.
/// - `config.split-placement` at each index whose writers, the connectors that write
///   it or a channel on it, are on one node: once for each writer whose winner is not
///   the winner of the index, at the label of the index's placement, or of the
///   writer's when no placement selects the index.
/// - `config.writer-nodes` at the `node` of the first connector, in name order, on a
///   second node that writes an index or a channel on it.
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
    let entries = found.entries.iter();
    let kinds = entries.filter_map(|(name, entry)| Some((name, kind(entry)?)));
    let channels = channels(kinds, BTreeMap::new(), applied, unheld(applied));
    let mut diagnostics = wrong(&found, &channels);
    let model = Model::found(&found);
    let indexes = rules(&model, members, &mut diagnostics);
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

/// Checks `definitions`, the definitions of a spec by tree key, with `members` and
/// `kinds` as [`plan`] takes them. Run it on the result of [`Plan::definitions`] before
/// an apply, because a plan file that `plan` did not make can hold what `plan` refuses.
///
/// # Errors
///
/// The problems of the first stage that has any, with no span or note:
///
/// 1. `config.private-key` for each string of a definition that holds a private key.
/// 2. The diagnostics of `kinds` for each connector whose kind or config it refuses,
///    then `config.duplicate-name`, whose earlier name is the first in name order,
///    and `config.subject-is-connector`.
/// 3. Each problem of the rules of [`plan`] from `config.unplaced` to
///    `config.unknown-node`.
pub fn check(
    definitions: &BTreeMap<Name, definition::Definition>,
    members: &BTreeSet<Name>,
    kinds: &Table,
) -> Result<(), Vec<Diagnostic>> {
    let mut diagnostics = problems(definitions, members, kinds);
    if diagnostics.is_empty() {
        return Ok(());
    }
    // A connector config that `plan` read holds the spans of files that the caller
    // of `check` does not have.
    for diagnostic in &mut diagnostics {
        diagnostic.span = None;
        diagnostic.notes.clear();
    }
    Err(diagnostics)
}

/// The problems of the first stage of [`check`] that has any.
fn problems(
    definitions: &BTreeMap<Name, definition::Definition>,
    members: &BTreeSet<Name>,
    kinds: &Table,
) -> Vec<Diagnostic> {
    let alarms = private_key::in_definitions(definitions);
    if !alarms.is_empty() {
        return alarms;
    }
    let mut diagnostics = Vec::new();
    let writes = writes(definitions, kinds, &mut diagnostics);
    diagnostics.extend(duplicate::in_definitions(definitions));
    diagnostics.extend(subject::not_connectors(definitions));
    if !diagnostics.is_empty() {
        return diagnostics;
    }
    let model = Model::definitions(definitions, &writes);
    rules(&model, members, &mut diagnostics);
    diagnostics
}

/// The change that an apply makes, and the spec it was planned on.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Plan {
    /// The pointer of the applied spec. An apply refuses the plan when the region's
    /// pointer is another one.
    pub base: spec::Pointer,
    /// Each change, by tree key. Empty when the files match the spec.
    pub changes: BTreeMap<Name, Change>,
    /// The home node of each index of the files, as the placements give it, by index
    /// name. The apply gives this home only to an index with no home, so an index with
    /// a home keeps it.
    pub homes: BTreeMap<Name, Name>,
}

impl Plan {
    /// The definitions of the spec after the plan, by tree key: `applied` with each
    /// change. A channel keeps the key of the stored channel at its name, and a new
    /// name gets a key from `key`, in name order. An edge to a name that is no channel
    /// after the plan gets a key from `key` too, which [`spec::region::check`] refuses
    /// as dangling. Each call of `key` must give a key that no channel holds and that
    /// no earlier call gave.
    ///
    /// # Errors
    ///
    /// [`Error::Mismatch`] at the first change in one of these cases, which [`plan`]
    /// never makes from `applied`, so only a hand-made file holds:
    ///
    /// - `old` is not the digest of the encoded definition at its name in `applied`:
    ///   `None` at a stored name, `Some` at a name with no stored definition, or
    ///   another digest.
    /// - The stored or the new definition is of a kind that no block of a file
    ///   defines, or its name is not the tree key of an unreserved label of its kind.
    ///
    /// A change with no old and no new definition, which [`Plan::decode`] refuses,
    /// gets it at a stored name and changes nothing at another name.
    pub fn definitions(
        &self,
        applied: &BTreeMap<Name, definition::Definition>,
        key: impl FnMut() -> Key,
    ) -> Result<BTreeMap<Name, definition::Definition>, Error> {
        let mut definitions = applied.clone();
        for (name, change) in &self.changes {
            let stored = definitions.remove(name);
            let new = change.new.as_ref().map(|entry| match &entry.definition {
                Definition::Spec(definition) => definition.kind(),
                Definition::Channel(_) => definition::Kind::Channel,
            });
            let kinds = stored
                .as_ref()
                .map(definition::Definition::kind)
                .into_iter();
            if change.old != stored.map(|stored| Digest::of(&stored.encode()))
                || !kinds.chain(new).all(|kind| planned(name, kind))
            {
                return Err(Error::Mismatch { name: name.clone() });
            }
        }
        let kept = definitions
            .iter()
            .filter_map(|(name, definition)| match definition {
                definition::Definition::Channel(channel) => Some((name, channel.key)),
                _ => None,
            })
            .collect();
        let kinds = self
            .changes
            .iter()
            .filter_map(|(name, change)| Some((name, kind(change.new.as_ref()?)?)));
        let channels = channels(kinds, kept, applied, key);
        for (name, change) in &self.changes {
            if let Some(Entry {
                definition: Definition::Spec(definition),
                ..
            }) = &change.new
            {
                definitions.insert(name.clone(), definition.clone());
            }
        }
        let channels = channels.into_iter();
        definitions.extend(
            channels.map(|(name, channel)| {
                (name, definition::Definition::Channel(channel))
            }),
        );
        Ok(definitions)
    }
}

/// One definition that the plan adds, changes, or removes.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Change {
    /// The digest of the stored definition, or `None` when the plan adds it.
    pub old: Option<Digest>,
    /// The definition in the files, with the span of its label, or `None` when the
    /// plan removes it.
    pub new: Option<Entry>,
}

fn kind(entry: &Entry) -> Option<&spec::channel::Kind<Name>> {
    match &entry.definition {
        Definition::Channel(kind) => Some(kind),
        Definition::Spec(_) => None,
    }
}

/// Gives `Key::from_u128(n)` for each `n` from 1 up that no channel of `applied`
/// holds.
fn unheld(applied: &BTreeMap<Name, definition::Definition>) -> impl FnMut() -> Key {
    let held: BTreeSet<Key> = applied
        .values()
        .filter_map(|definition| match definition {
            definition::Definition::Channel(channel) => Some(channel.key),
            _ => None,
        })
        .collect();
    let mut made = (1..)
        .map(Key::from_u128)
        .filter(move |key| !held.contains(key));
    move || made.next().expect("invariant: fewer than 2^128 channels")
}

/// The channel of each of `kinds`, which come in name order, by name. A channel
/// keeps the key of the channel that `applied` stores at its name, and else gets one
/// from `key`. An edge points at the channel at its name in `kinds` or `kept`, or else
/// at a key from `key`.
fn channels<'n>(
    kinds: impl Iterator<Item = (&'n Name, &'n spec::channel::Kind<Name>)>,
    mut kept: BTreeMap<&'n Name, Key>,
    applied: &BTreeMap<Name, definition::Definition>,
    mut key: impl FnMut() -> Key,
) -> BTreeMap<Name, Channel> {
    let kinds: Vec<_> = kinds.collect();
    for &(name, _) in &kinds {
        let stored = match applied.get(name) {
            Some(definition::Definition::Channel(channel)) => channel.key,
            _ => key(),
        };
        kept.insert(name, stored);
    }
    kinds
        .into_iter()
        .map(|(name, kind)| {
            let kind = kind
                .clone()
                .map(|to| kept.get(&to).copied().unwrap_or_else(&mut key));
            (
                name.clone(),
                Channel {
                    key: kept[name],
                    kind,
                },
            )
        })
        .collect()
}

/// What the rules of [`plan`] after `config.wrong-channel` read: the definitions by
/// tree key, with the span of each label and each `home` where a file gives one.
struct Model<'a> {
    /// Each placement, with its tree key, in name order.
    placements: Vec<(&'a Name, &'a Policy)>,
    /// Each index, in name order.
    indexes: Vec<&'a Name>,
    /// The index of each data channel whose index is a channel, by name.
    index_of: BTreeMap<&'a Name, &'a Name>,
    /// Each connector, in name order.
    connectors: Vec<Connector<'a>>,
    /// Each node that a placement names.
    nodes: Vec<(&'a Name, Option<Span>)>,
    /// The span of the label of each definition.
    labels: BTreeMap<&'a Name, Option<Span>>,
    /// The span of the `home` of each placement.
    homes: BTreeMap<&'a Name, Option<Span>>,
}

impl<'a> Model<'a> {
    fn new() -> Self {
        Self {
            placements: Vec::new(),
            indexes: Vec::new(),
            index_of: BTreeMap::new(),
            connectors: Vec::new(),
            nodes: Vec::new(),
            labels: BTreeMap::new(),
            homes: BTreeMap::new(),
        }
    }

    /// The model of the files, with their spans.
    fn found(found: &'a Found<'_>) -> Self {
        let mut model = Self::new();
        for (name, entry) in &found.entries {
            model.labels.insert(name, entry.label_span);
            match &entry.definition {
                Definition::Spec(definition) => {
                    let block = found.blocks[name];
                    model.add(name, definition, &found.writes, Some(block));
                }
                Definition::Channel(kind) => model.channel(name, kind, Some),
            }
        }
        model
    }

    /// The model of `definitions`, with what each connector writes by tree key in
    /// `writes`, and with no span. A data channel whose index is no channel has no
    /// index.
    fn definitions(
        definitions: &'a BTreeMap<Name, definition::Definition>,
        writes: &'a BTreeMap<Name, Vec<Name>>,
    ) -> Self {
        let keys: BTreeMap<Key, &Name> = definitions
            .iter()
            .filter_map(|(name, definition)| match definition {
                definition::Definition::Channel(channel) => Some((channel.key, name)),
                _ => None,
            })
            .collect();
        let mut model = Self::new();
        for (name, definition) in definitions {
            match definition {
                definition::Definition::Channel(channel) => {
                    model.channel(name, &channel.kind, |key| keys.get(key).copied());
                }
                definition => model.add(name, definition, writes, None),
            }
        }
        model
    }

    /// Adds a placement or a connector, with the spans of its `block` when a file
    /// gives it, and skips each other definition that is no channel.
    fn add(
        &mut self,
        name: &'a Name,
        definition: &'a definition::Definition,
        writes: &'a BTreeMap<Name, Vec<Name>>,
        block: Option<&Block>,
    ) {
        match definition {
            definition::Definition::Placement(policy) => {
                self.placements.push((name, policy));
                self.homes
                    .insert(name, block.and_then(|block| span(block, "home")));
                let spans = block.map(placement::spans).unwrap_or_default();
                let at = |node| spans.get(node).copied().flatten();
                let nodes = placement::nodes(policy).map(|node| (node, at(node)));
                self.nodes.extend(nodes);
            }
            definition::Definition::Connector(connector) => {
                self.connectors.push(Connector {
                    name,
                    node: connector.node(),
                    at: block.and_then(|block| span(block, "node")),
                    writes: writes
                        .get(name)
                        .expect("invariant: the kinds checked each connector"),
                });
            }
            _ => {}
        }
    }

    /// Adds a channel of `kind`, whose edges `index` gives the name of.
    fn channel<R>(
        &mut self,
        name: &'a Name,
        kind: &'a spec::channel::Kind<R>,
        index: impl Fn(&'a R) -> Option<&'a Name>,
    ) {
        match kind {
            spec::channel::Kind::Index { .. } => self.indexes.push(name),
            spec::channel::Kind::Data(data) => {
                if let Some(index) = index(Data::index(data)) {
                    self.index_of.insert(name, index);
                }
            }
        }
    }

    /// Reports whether `connector` writes `index` or a channel on it.
    fn writes(&self, connector: &Connector<'_>, index: &Name) -> bool {
        connector.writes.iter().any(|name| {
            name == index || self.index_of.get(name).copied() == Some(index)
        })
    }

    /// The span of the label of `name`.
    fn label(&self, name: &Name) -> Option<Span> {
        self.labels.get(name).copied().flatten()
    }
}

/// A connector, as its kind checks its config.
struct Connector<'a> {
    /// Its tree key.
    name: &'a Name,
    /// The node that runs it.
    node: &'a Name,
    /// Where the block names the node.
    at: Option<Span>,
    /// The channels that it writes to the mesh.
    writes: &'a [Name],
}

/// What each connector of `definitions` writes to the mesh, by tree key. It adds to
/// `diagnostics` the diagnostics of `kinds` for each connector whose kind or config it
/// refuses.
fn writes(
    definitions: &BTreeMap<Name, definition::Definition>,
    kinds: &Table,
    diagnostics: &mut Vec<Diagnostic>,
) -> BTreeMap<Name, Vec<Name>> {
    let mut writes = BTreeMap::new();
    for (name, definition) in definitions {
        let definition::Definition::Connector(connector) = definition else {
            continue;
        };
        let config = connector.config().document();
        match kinds.check(connector.kind().as_str(), None, config) {
            Ok(channels) => {
                writes.insert(name.clone(), channels.writes);
            }
            Err(found) => diagnostics.extend(found),
        }
    }
    writes
}

/// Runs each rule of [`plan`] after `config.wrong-channel` on `model`, and gives each
/// index with the node of its first writer and where [`place`] puts it.
fn rules<'a>(
    model: &'a Model<'a>,
    members: &BTreeSet<Name>,
    diagnostics: &mut Vec<Diagnostic>,
) -> BTreeMap<&'a Name, Index<'a>> {
    let indexes = indexes(model, diagnostics);
    connectors(model, &indexes, diagnostics);
    unknown(model, members, diagnostics);
    indexes
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

/// The node of an index's first writer, and where [`place`] puts the index.
type Index<'f> = (Option<&'f Name>, Result<Placed<'f>, Tie>);

/// Places each index, with the node of its first writer. Reports each index that two
/// writer nodes or [`place`] leave with no home.
fn indexes<'f>(
    model: &'f Model<'f>,
    diagnostics: &mut Vec<Diagnostic>,
) -> BTreeMap<&'f Name, Index<'f>> {
    let mut indexes = BTreeMap::new();
    for &index in &model.indexes {
        let writer = writer(model, index, diagnostics);
        let placed = place(index, model.placements.iter().copied(), writer);
        diagnostics.extend(unplaced(model.label(index), &placed));
        indexes.insert(index, (writer, placed));
    }
    indexes
}

/// The home of each placed index.
fn homes(indexes: BTreeMap<&Name, Index<'_>>) -> BTreeMap<Name, Name> {
    indexes
        .into_iter()
        .filter_map(|(index, (_, placed))| {
            Some((index.clone(), placed.ok()?.home.ok()?.clone()))
        })
        .collect()
}

/// A connector's key and node, and where [`place`] puts it.
type Located<'f> = (&'f Name, &'f Name, Result<Placed<'f>, Tie>);

/// An index, where [`place`] puts it, and a connector that writes it.
type Written<'f, 'c> = (&'f Name, &'f Result<Placed<'f>, Tie>, &'c Located<'f>);

/// Places each connector, with its `node` as the writer. Reports
/// `config.connector-home` at the `home` of a winner that names another node,
/// `config.unplaced` at each connector that [`place`] gives a tie or no home for, and
/// `config.split-placement` at each index whose writers are on one node, for each writer
/// with another winner.
fn connectors<'f>(
    model: &'f Model<'f>,
    indexes: &'f BTreeMap<&'f Name, Index<'f>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let placements = &model.placements;
    let connectors: Vec<_> = model
        .connectors
        .iter()
        .map(|connector| {
            let Connector { name, node, .. } = *connector;
            let placed = place(name, placements.iter().copied(), Some(node));
            (name, node, placed)
        })
        .collect();
    let written: Vec<_> = indexes
        .iter()
        .flat_map(|(index, (_, own))| {
            let writers = connectors.iter().zip(&model.connectors);
            writers
                .filter(|(_, connector)| model.writes(connector, index))
                .map(move |(located, _)| (*index, own, located))
        })
        .collect();
    let moves = moves(&connectors, &written, placements);
    for (name, node, placed) in &connectors {
        if let Ok(Placed {
            placement: Some(placement),
            home: Ok(home),
            ..
        }) = placed
            && home != node
        {
            diagnostics
                .push(connector_home(model, placement, home, name, node, &moves));
        } else {
            diagnostics.extend(unplaced(model.label(name), placed));
        }
    }
    let mut nodes = BTreeMap::<_, BTreeSet<_>>::new();
    for (index, _, (_, node, _)) in &written {
        nodes.entry(*index).or_default().insert(*node);
    }
    for (index, own, (connector, _, theirs)) in written {
        if let (Ok(own), Ok(theirs)) = (own, theirs)
            && nodes[index].len() == 1
        {
            let (own, theirs) = (own.placement, theirs.placement);
            diagnostics.extend(split(model, index, own, connector, theirs, &moves));
        }
    }
}

/// The one fix of each diagnostic of each connector that no placement can win for
/// with each of its indexes at the connector's node, by connector. `written` holds each
/// connector that writes each index. The fix names each winner, the connector's first.
fn moves<'f, 'c>(
    connectors: &'c [Located<'f>],
    written: &[Written<'f, 'c>],
    placements: &[(&'f Name, &'f Policy)],
) -> BTreeMap<&'c Name, Fix<'f>> {
    let mut nodes = BTreeMap::<_, BTreeSet<_>>::new();
    for (_, node, placed) in connectors {
        if let Some(placement) =
            placed.as_ref().ok().and_then(|placed| placed.placement)
        {
            nodes.entry(placement).or_default().insert(*node);
        }
    }
    let mut owners = BTreeMap::<_, BTreeSet<_>>::new();
    for (_, own, (connector, node, _)) in written {
        if let Some(own) = own.as_ref().ok().and_then(|own| own.placement) {
            owners.entry(*connector).or_default().insert(own);
            nodes.entry(own).or_default().insert(*node);
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
    for (name, node, placed) in connectors {
        let Ok(Placed { placement, .. }) = *placed else {
            continue;
        };
        let owners: Vec<_> = owners
            .remove(*name)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let spread = |p: &Name| nodes[p].iter().any(|other| other != node);
        let fix = match (placement, owners.as_slice()) {
            (Some(p), _) if elsewhere(p, node) && spread(p) => {
                let others = owners.iter().copied().filter(|owner| *owner != p);
                Fix::Exclude {
                    placements: [p].into_iter().chain(others).collect(),
                    node,
                }
            }
            (None, &[p]) if !elsewhere(p, node) => continue,
            (None, [_, ..]) => Fix::Regroup { owners, node },
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

/// The fix of a diagnostic of a connector. [`moves`] gives the first two, which move
/// the connector or its indexes to another placement.
enum Fix<'a> {
    /// Take the connector and its indexes out of `placements`, the connector's winner
    /// first, into a placement whose `home` is `node`.
    Exclude {
        placements: Vec<&'a Name>,
        node: &'a Name,
    },
    /// Take the indexes of the connector out of `owners`, the placements that win for
    /// them, into a placement for the connector whose `home` is `node`.
    Regroup {
        owners: Vec<&'a Name>,
        node: &'a Name,
    },
    /// Name `node` as the `home` of the placement that wins, and keep `node` out of its
    /// `standby` and `copies`.
    Home { node: &'a Name },
    /// Make `placement` win for the connector and its indexes.
    Win { placement: &'a Name },
}

/// The text of the fix of a diagnostic of `connector`: the fix that [`moves`] gives
/// `connector`, or else `otherwise`.
fn fix(
    moves: &BTreeMap<&Name, Fix<'_>>,
    connector: &Name,
    otherwise: &Fix<'_>,
) -> String {
    match moves.get(connector).unwrap_or(otherwise) {
        Fix::Exclude { placements, node } => format!(
            "Exclude the connector `{connector}` and its indexes from the `select` of \
             {}, and select them with another placement whose `home` is `{node}`",
            each(placements)
        ),
        Fix::Regroup { owners, node } => format!(
            "Exclude the indexes of the connector `{connector}` from the `select` of \
             {}, and select the connector and its indexes with another placement \
             whose `home` is `{node}`",
            each(owners)
        ),
        Fix::Home { node } => format!(
            "Name `{node}` as the `home`, and keep `{node}` out of `standby` and \
             `copies`"
        ),
        Fix::Win { placement } => format!(
            "Make the placement `{}` win for the connector `{connector}` and its \
             indexes",
            label(placement)
        ),
    }
}

/// The `config.connector-home` diagnostic of `connector` on `node`, whose winner
/// `placement` names `home`, with the fix that [`fix`] gives from `moves`.
fn connector_home(
    model: &Model<'_>,
    placement: &Name,
    home: &Name,
    connector: &Name,
    node: &Name,
    moves: &BTreeMap<&Name, Fix<'_>>,
) -> Diagnostic {
    let p = label(placement);
    let fix = fix(moves, connector, &Fix::Home { node });
    Diagnostic::new(
        CONNECTOR_HOME,
        model.homes.get(placement).copied().flatten(),
        format!(
            "the placement `{p}` names the home `{home}`, but the connector \
             `{connector}` runs on the node `{node}`"
        ),
        fix,
    )
}

/// A `config.split-placement` diagnostic when `own`, the placement that wins for
/// `index`, is not `theirs`, the one that wins for the connector `connector`, with the
/// fix that [`fix`] gives from `moves`.
fn split(
    model: &Model<'_>,
    index: &Name,
    own: Option<&Name>,
    connector: &Name,
    theirs: Option<&Name>,
    moves: &BTreeMap<&Name, Fix<'_>>,
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
    let placement = theirs.unwrap_or(at);
    let fix = fix(moves, connector, &Fix::Win { placement });
    Some(Diagnostic::new(
        SPLIT_PLACEMENT,
        model.label(at),
        message,
        fix,
    ))
}

/// The `config.unplaced` diagnostic at `at`, the label of the name that `placed`
/// places, when it has a tie or no home.
fn unplaced(at: Option<Span>, placed: &Result<Placed<'_>, Tie>) -> Option<Diagnostic> {
    let (message, fix) = match placed {
        Ok(Placed { home: Ok(_), .. }) => return None,
        Ok(Placed {
            home: Err(homeless),
            ..
        }) => (homeless.to_string(), homeless.fix()),
        Err(tie) => (tie.to_string(), tie.fix()),
    };
    Some(Diagnostic::new(UNPLACED, at, message, fix.into()))
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
    model: &'f Model<'f>,
    index: &Name,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<&'f Name> {
    let mut writers = model
        .connectors
        .iter()
        .filter(|writer| model.writes(writer, index));
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
    Some(first.node)
}

/// Reports `config.unknown-node` at each node that a connector or a placement names and
/// that is not in `members`.
fn unknown(
    model: &Model<'_>,
    members: &BTreeSet<Name>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let writers = model
        .connectors
        .iter()
        .map(|writer| (writer.node, writer.at));
    let placed = model.nodes.iter().copied();
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

/// Reports whether a plan holds a definition of `kind` at `name`: a kind that a block
/// of a file defines, at the tree key of an unreserved label.
fn planned(name: &Name, kind: definition::Kind) -> bool {
    KINDS.iter().any(|(block, _)| *block == kind)
        && kind.label(name).is_some_and(|label| !label.reserved())
}

/// The change of each definition whose bytes differ from the stored bytes, by tree
/// key. A stored definition that is not [`planned`] is never removed.
fn changes(
    entries: BTreeMap<Name, Entry>,
    mut channels: BTreeMap<Name, Channel>,
    applied: &BTreeMap<Name, definition::Definition>,
) -> BTreeMap<Name, Change> {
    let mut stored: BTreeMap<&Name, &definition::Definition> = applied
        .iter()
        .filter(|(name, definition)| planned(name, definition.kind()))
        .collect();
    let mut changes = BTreeMap::new();
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
            let old = old.as_deref().map(Digest::of);
            changes.insert(
                name,
                Change {
                    old,
                    new: Some(entry),
                },
            );
        }
    }
    changes.extend(stored.into_iter().map(|(name, old)| {
        let old = Some(Digest::of(&old.encode()));
        (name.clone(), Change { old, new: None })
    }));
    changes
}
