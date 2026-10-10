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
    Definition, Entry, Found, KINDS, channel, checked, connector, duplicate, placement,
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
/// block of a file defines, is never a change. The status index of a connector takes
/// the connector's placement, whatever placement selects the index.
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
///   it or a channel on it, are on one node, when the winner of a writer is not the
///   winner of the index: once, naming each such writer, at the label of the index's
///   placement, or of the first such writer's when no placement selects the index.
///   Its fix is the one fix of the unit of the index: its writers, each connector
///   that writes another index of theirs, and so on.
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
///    and `config.long-name` for each with a status name that is too long, then
///    `config.duplicate-name`, whose earlier name is the first in name order,
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
            let new = change.new.as_ref().map(|entry| entry.definition.kind());
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
        writes: &'a BTreeMap<Name, connector::Writes>,
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
        writes: &'a BTreeMap<Name, connector::Writes>,
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
                let writes = writes
                    .get(name)
                    .expect("invariant: the kinds checked each connector");
                self.connectors.push(Connector {
                    name,
                    node: connector.node(),
                    at: block.and_then(|block| span(block, "node")),
                    writes: &writes.channels,
                    status: writes.index(),
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
    fn feeds(&self, connector: &Connector<'_>, index: &Name) -> bool {
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
    /// The index of its status channels.
    status: &'a Name,
}

/// What each connector of `definitions` writes to the mesh, its status channels with
/// it, by tree key. It adds to `diagnostics` the diagnostics of `kinds` for each
/// connector whose kind or config it refuses, and `config.long-name` for each whose
/// status names are too long.
fn writes(
    definitions: &BTreeMap<Name, definition::Definition>,
    kinds: &Table,
    diagnostics: &mut Vec<Diagnostic>,
) -> BTreeMap<Name, connector::Writes> {
    let mut writes = BTreeMap::new();
    for (name, definition) in definitions {
        let definition::Definition::Connector(connector) = definition else {
            continue;
        };
        let config = connector.config().document();
        let found = kinds
            .check(connector.kind().as_str(), None, config)
            .and_then(|channels| {
                connector::writes(name, channels, None).map_err(|found| vec![found])
            });
        match found {
            Ok(found) => {
                writes.insert(name.clone(), found);
            }
            Err(found) => diagnostics.extend(found),
        }
    }
    writes
}

/// Runs each rule of [`plan`] after `config.wrong-channel` on `model`, and gives each
/// index.
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

/// An index, as the rules after `config.wrong-channel` read it.
struct Index<'f> {
    /// Each connector that writes the index or a channel on it, in name order.
    writers: Vec<&'f Connector<'f>>,
    /// Where [`place`] puts the index, with the node of its first writer.
    placed: Result<Placed<'f>, Tie>,
}

impl<'f> Index<'f> {
    /// The first writer that runs on another node than the first writer.
    fn apart(&self) -> Option<&'f Connector<'f>> {
        let (first, rest) = self.writers.split_first()?;
        rest.iter()
            .copied()
            .find(|writer| writer.node != first.node)
    }
}

/// Places each index. The status index of a connector takes the connector's
/// placement, and the connector reports when it has none. Reports each index that two
/// writer nodes or [`place`] leave with no home.
fn indexes<'f>(
    model: &'f Model<'f>,
    diagnostics: &mut Vec<Diagnostic>,
) -> BTreeMap<&'f Name, Index<'f>> {
    let statuses: BTreeMap<_, _> = model
        .connectors
        .iter()
        .map(|connector| (connector.status, connector))
        .collect();
    let mut indexes = BTreeMap::new();
    for &name in &model.indexes {
        let writers: Vec<_> = model
            .connectors
            .iter()
            .filter(|writer| model.feeds(writer, name))
            .collect();
        let placements = model.placements.iter().copied();
        let index = if let Some(connector) = statuses.get(name) {
            let placed = place(connector.name, placements, Some(connector.node));
            Index { writers, placed }
        } else {
            let node = writers.first().map(|writer| writer.node);
            let placed = place(name, placements, node);
            let index = Index { writers, placed };
            diagnostics.extend(unplaced(model.label(name), &index.placed));
            index
        };
        diagnostics.extend(writer_nodes(name, &index));
        indexes.insert(name, index);
    }
    indexes
}

/// The home of each placed index.
fn homes(indexes: BTreeMap<&Name, Index<'_>>) -> BTreeMap<Name, Name> {
    indexes
        .into_iter()
        .filter_map(|(index, Index { placed, .. })| {
            Some((index.clone(), placed.ok()?.home.ok()?.clone()))
        })
        .collect()
}

/// A connector, and where [`place`] puts it.
struct Located<'f> {
    connector: &'f Connector<'f>,
    placed: Result<Placed<'f>, Tie>,
}

impl<'f> Located<'f> {
    /// The placement that wins for the connector, if one does.
    fn winner(&self) -> Option<&'f Name> {
        self.placed
            .as_ref()
            .ok()
            .and_then(|placed| placed.placement)
    }
}

/// A unit of failover: connectors on one node, linked by the indexes that they write.
/// An index joins the unit of its writers when they are on one node.
struct Unit<'f> {
    linked: Linked<'f>,
    /// The fix of each of its diagnostics, or `None` when no placement wins for a
    /// connector or an index of it.
    fix: Option<Fix<'f>>,
}

/// Connectors and the indexes that link them.
struct Linked<'f> {
    /// Its connectors, in name order.
    connectors: Vec<&'f Name>,
    /// Its indexes, in name order.
    indexes: Vec<&'f Name>,
}

/// Places each connector, with its `node` as the writer. Reports
/// `config.connector-home` at the `home` of a winner that names another node,
/// `config.unplaced` at each connector that [`place`] gives a tie or no home for, and
/// `config.split-placement` at each index whose writers are on one node and do not
/// all have its winner.
fn connectors<'f>(
    model: &'f Model<'f>,
    indexes: &BTreeMap<&'f Name, Index<'f>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let placements = &model.placements;
    let located: BTreeMap<_, _> = model
        .connectors
        .iter()
        .map(|connector| {
            let Connector { name, node, .. } = *connector;
            let placed = place(name, placements.iter().copied(), Some(node));
            (name, Located { connector, placed })
        })
        .collect();
    let (units, of) = units(&located, indexes, placements);
    for Located { connector, placed } in located.values() {
        let Connector { name, node, .. } = **connector;
        if let Ok(Placed {
            placement: Some(placement),
            home: Ok(home),
            ..
        }) = placed
            && *home != node
        {
            let unit = &units[of[name]];
            let fix = match &unit.fix {
                Some(Fix::Win { placement: t }) if t == placement => {
                    &Fix::Home { node }
                }
                Some(fix) => fix,
                None => unreachable!(
                    "invariant: the unit of {name} at {placement} has no fix"
                ),
            };
            let p = label(placement);
            diagnostics.push(Diagnostic::new(
                CONNECTOR_HOME,
                model.homes.get(placement).copied().flatten(),
                format!(
                    "the placement `{p}` names the home `{home}`, but the connector \
                     `{name}` runs on the node `{node}`"
                ),
                text(fix, &unit.linked.connectors),
            ));
        } else {
            diagnostics.extend(unplaced(model.label(name), placed));
        }
    }
    let joined = units
        .iter()
        .flat_map(|unit| unit.linked.indexes.iter().map(move |name| (*name, unit)));
    for (name, unit) in joined {
        let index = &indexes[name];
        let Ok(placed) = &index.placed else {
            continue;
        };
        let theirs = index.writers.iter().filter_map(|writer| {
            let theirs = located[writer.name].placed.as_ref().ok()?;
            Some((writer.name, theirs.placement))
        });
        let Some((at, message)) = split(name, placed.placement, theirs) else {
            continue;
        };
        let Some(fix) = &unit.fix else {
            unreachable!("invariant: the unit of the index {name} has no fix");
        };
        diagnostics.push(Diagnostic::new(
            SPLIT_PLACEMENT,
            model.label(at),
            message,
            text(fix, &unit.linked.connectors),
        ));
    }
}

/// Each unit of the connectors of `located`, with its fix, and the position of the
/// unit of each connector. The fix of a unit names one target: the winner of its first
/// connector that a placement wins for, or the one placement that wins for its indexes.
/// When no placement can win for each of its connectors and indexes at its node, the
/// fix names each winner, the target first.
fn units<'f>(
    located: &BTreeMap<&'f Name, Located<'f>>,
    indexes: &BTreeMap<&'f Name, Index<'f>>,
    placements: &[(&'f Name, &'f Policy)],
) -> (Vec<Unit<'f>>, BTreeMap<&'f Name, usize>) {
    let mut nodes = BTreeMap::<_, BTreeSet<_>>::new();
    for located in located.values() {
        if let Some(winner) = located.winner() {
            let node = located.connector.node;
            nodes.entry(winner).or_default().insert(node);
        }
    }
    for index in indexes.values() {
        if let Ok(Placed {
            placement: Some(own),
            ..
        }) = index.placed
        {
            let writers = index.writers.iter().map(|writer| writer.node);
            nodes.entry(own).or_default().extend(writers);
        }
    }
    let (linked, of) = link(located, indexes);
    let home = |placement: &Name| {
        placements
            .iter()
            .find(|(key, _)| *key == placement)
            .and_then(|(_, policy)| policy.home())
    };
    let elsewhere = |placement: &Name, node: &Name| {
        home(placement).is_some_and(|home| home != node)
    };
    let unit = |linked: Linked<'f>| {
        let connectors = &linked.connectors;
        let node = located[connectors[0]].connector.node;
        let mut winners = connectors.iter().filter_map(|c| located[c].winner());
        let target = winners.next();
        let spread = |p: &Name| nodes[p].iter().any(|other| *other != node);
        let owners: BTreeSet<_> = linked
            .indexes
            .iter()
            .filter_map(|name| indexes[name].placed.as_ref().ok()?.placement)
            .collect();
        let owners: Vec<_> = owners.into_iter().collect();
        let fix = match (target, owners.as_slice()) {
            (Some(t), _) if elsewhere(t, node) && spread(t) => {
                let others: BTreeSet<_> = winners.chain(owners).collect();
                let others = others.into_iter().filter(|other| *other != t);
                Some(Fix::Exclude {
                    placements: [t].into_iter().chain(others).collect(),
                    node,
                })
            }
            (Some(placement), _) => Some(Fix::Win { placement }),
            (None, &[placement]) if !elsewhere(placement, node) => {
                Some(Fix::Win { placement })
            }
            (None, []) => None,
            (None, _) => Some(Fix::Regroup { owners, node }),
        };
        Unit { linked, fix }
    };
    (linked.into_iter().map(unit).collect(), of)
}

/// The connectors of `located` by unit, with the indexes that link them, and the
/// position of the unit of each connector. Each index whose writers are on one node
/// links them.
fn link<'f>(
    located: &BTreeMap<&'f Name, Located<'f>>,
    indexes: &BTreeMap<&'f Name, Index<'f>>,
) -> (Vec<Linked<'f>>, BTreeMap<&'f Name, usize>) {
    let joining: Vec<_> = indexes
        .iter()
        .filter(|(_, index)| index.apart().is_none())
        .collect();
    let position: BTreeMap<_, _> = located.keys().copied().zip(0..).collect();
    let mut parent: Vec<usize> = (0..located.len()).collect();
    let root = |parent: &mut Vec<usize>, mut i: usize| {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    };
    for (_, index) in &joining {
        if let Some((first, rest)) = index.writers.split_first() {
            let first = root(&mut parent, position[first.name]);
            for writer in rest {
                let other = root(&mut parent, position[writer.name]);
                parent[other] = first;
            }
        }
    }
    let mut of = BTreeMap::new();
    let mut roots = BTreeMap::new();
    let mut units = Vec::new();
    for (name, i) in position {
        let unit = *roots.entry(root(&mut parent, i)).or_insert_with(|| {
            units.push(Linked {
                connectors: Vec::new(),
                indexes: Vec::new(),
            });
            units.len() - 1
        });
        units[unit].connectors.push(name);
        of.insert(name, unit);
    }
    for (name, index) in joining {
        if let Some(first) = index.writers.first() {
            units[of[first.name]].indexes.push(*name);
        }
    }
    (units, of)
}

/// Joins `items`, in order: "a", "a and b", or "a, b, and c".
fn list(items: &[String]) -> String {
    match items {
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
        [] => unreachable!("invariant: a list names an item"),
    }
}

/// Names each placement in `keys`, in order, by its label: "`p`", "`p` and `q`", or
/// "`p`, `q`, and `r`".
fn each(keys: &[&Name]) -> String {
    let labels: Vec<_> = keys.iter().map(|key| format!("`{}`", label(key))).collect();
    list(&labels)
}

/// The fix of a diagnostic of a unit. The first two move the unit or its indexes to
/// another placement.
enum Fix<'a> {
    /// Take the connectors and their indexes out of `placements`, the target first,
    /// into a placement whose `home` is `node`.
    Exclude {
        placements: Vec<&'a Name>,
        node: &'a Name,
    },
    /// Take the indexes of the connectors out of `owners`, the placements that win for
    /// them, into a placement for the connectors whose `home` is `node`.
    Regroup {
        owners: Vec<&'a Name>,
        node: &'a Name,
    },
    /// Name `node` as the `home` of the placement that wins, and keep `node` out of its
    /// `standby` and `copies`.
    Home { node: &'a Name },
    /// Make `placement` win for the connectors and their indexes.
    Win { placement: &'a Name },
}

/// The text of `fix` for the unit of `connectors`. A unit of one connector names "the
/// connector `c`", and a larger one "the connectors `a` and `b`".
fn text(fix: &Fix<'_>, connectors: &[&Name]) -> String {
    let names: Vec<_> = connectors.iter().map(|name| format!("`{name}`")).collect();
    let (them, theirs, each_one) = match connectors {
        [_] => ("the connector", "its", "the connector"),
        _ => ("the connectors", "their", "the connectors"),
    };
    let names = list(&names);
    match fix {
        Fix::Exclude { placements, node } => format!(
            "Exclude {them} {names} and {theirs} indexes from the `select` of {}, and \
             select them with another placement whose `home` is `{node}`",
            each(placements)
        ),
        Fix::Regroup { owners, node } => format!(
            "Exclude the indexes of {them} {names} from the `select` of {}, and select \
             {each_one} and {theirs} indexes with another placement whose `home` is \
             `{node}`",
            each(owners)
        ),
        Fix::Home { node } => format!(
            "Name `{node}` as the `home`, and keep `{node}` out of `standby` and \
             `copies`"
        ),
        Fix::Win { placement } => format!(
            "Make the placement `{}` win for {them} {names} and {theirs} indexes",
            label(placement)
        ),
    }
}

/// Where and what a `config.split-placement` of `index` reports, when `own`, the
/// placement that wins for it, is not each of `theirs`, the placements that win for its
/// writers: at `own`, or at the first other winner when no placement selects `index`.
fn split<'a>(
    index: &Name,
    own: Option<&'a Name>,
    theirs: impl Iterator<Item = (&'a Name, Option<&'a Name>)>,
) -> Option<(&'a Name, String)> {
    let mut at = own;
    let mut clauses = Vec::new();
    for (connector, theirs) in theirs {
        let clause = match (own, theirs) {
            (_, Some(theirs)) if own != Some(theirs) => {
                at = at.or(Some(theirs));
                format!(
                    "the placement `{}` wins for the connector `{connector}`",
                    label(theirs)
                )
            }
            (Some(_), None) => {
                format!("no placement selects the connector `{connector}`")
            }
            _ => continue,
        };
        clauses.push(clause);
    }
    let head = match own {
        Some(own) => format!(
            "the placement `{}` wins for the index `{index}`",
            label(own)
        ),
        None => format!("no placement selects the index `{index}`"),
    };
    let at = at.filter(|_| !clauses.is_empty())?;
    Some((at, format!("{head}, but {}", clauses.join(", and "))))
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

/// The `config.writer-nodes` diagnostic of `index` at its first writer on another node
/// than its first writer.
fn writer_nodes(name: &Name, index: &Index<'_>) -> Option<Diagnostic> {
    let second = index.apart()?;
    let first = index.writers[0];
    Some(Diagnostic::new(
        WRITER_NODES,
        second.at,
        format!(
            "connectors on the nodes `{}` and `{}` write the index `{name}`, so it has \
             no one home",
            first.node, second.node
        ),
        format!("Run each connector that writes `{name}` on one node"),
    ))
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
