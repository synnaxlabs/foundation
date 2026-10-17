use ::connector::kind::Channels;
use ::connector::status;
use document::diagnostic::{Code, Diagnostic, Note};
use document::encoding::Checked;
use document::{Block, Document, Map, Span, read};
use spec::channel::{Data, Kind};
use spec::connector::Connector;
use spec::data_type::DataType;
use spec::definition;
use std::collections::{BTreeMap, BTreeSet};
use types::name::{self, Name};

use crate::{
    Definition, Entry, Found, LONG_NAME, duplicate, private_key, span, unfilled,
};

const IMPLIED_CHANNEL: Code = Code::new("config.implied-channel");
const NO_CONNECTOR: Code = Code::new("config.no-connector");

const KIND_FIX: &str = "Add a `kind` attribute with the connector's kind";
const NODE_FIX: &str =
    "Add a `node` attribute with the name of the node that runs it, such as \"edge\"";

/// The keys that `check` reads. The kind reads each other key and block.
const KEYS: [&str; 2] = ["kind", "node"];

/// Checks a `connector` block and gives its connector. The kind of the block checks
/// its config, which is the body without `kind` and `node`, also when `node` is
/// missing. Keeps what the connector writes at `key`.
pub(crate) fn check(
    found: &mut Found<'_>,
    block: &Block,
    key: Option<&Name>,
) -> Option<Definition> {
    let kind = found.required(block, "kind", read::name, KIND_FIX.into());
    let node = found.required(block, "node", read::name, NODE_FIX.into());
    let config = match Checked::new(config(block)) {
        Ok(config) => config,
        Err(error) => {
            found.diagnostics.push(Diagnostic::from(&error));
            return None;
        }
    };
    let kind = kind.ok()?;
    let at = span(block, "kind");
    let channels = match found.kinds.check(kind.as_str(), at, config.document()) {
        Ok(channels) => channels,
        Err(diagnostics) => {
            found.diagnostics.extend(diagnostics);
            return None;
        }
    };
    let node = node.ok()?;
    if let Some(key) = key {
        let at = block.labels.first().and_then(|label| label.span);
        match writes(key, channels, at) {
            Ok(writes) => {
                found.writes.insert(key.clone(), writes);
            }
            Err(diagnostic) => found.diagnostics.push(diagnostic),
        }
    }
    let connector = Connector::new(kind, node, config);
    Some(Definition::Spec(definition::Definition::Connector(
        connector,
    )))
}

/// The `connector` block with the label `name` in `documents`: its kind, its node, and
/// its body without `kind` and `node`. It runs no check of a kind. Of the other
/// blocks, it reads only private keys, labels, and the bodies of the `connector`
/// blocks whose label is `name` in any ASCII case.
///
/// # Errors
///
/// `config.private-key` at each private key in `documents`, and then no other problem.
/// Else the problems that [`check`](crate::check) gives at each `connector` block whose
/// label is `name` in any ASCII case, other than those of its kind and
/// `config.implied-channel`, which the other connectors decide, as it gives them when
/// no other block holds a `fill`: `config.unfilled` and `config.bad-fill` in its body,
/// and then no other problem; else `config.reserved-name` and `config.duplicate-name`
/// at its label; else `config.no-connector` when no `connector` block has the label
/// `name`, with the names of the connector blocks that `check` takes in the fix; else
/// the problems of `kind` and `node` in the block, and `document.too-deep` when its
/// body nests deeper than `document::encoding::Checked` takes, in span order; else
/// `config.long-name` at its label when a status name that it has with no kind is
/// longer than [`Name::MAX_BYTES`].
pub fn find(documents: &[Document], name: &Name) -> Result<Connector, Vec<Diagnostic>> {
    let alarms = private_key::alarms(documents);
    if !alarms.is_empty() {
        return Err(alarms);
    }
    let blocks = crate::blocks(documents);
    let connector = definition::Kind::Connector;
    let lower = name.as_str().to_ascii_lowercase();
    let at_name = |key: &Name| key.as_str().to_ascii_lowercase() == lower;
    let bodies = crate::names(&blocks, connector)
        .filter(|(key, ..)| at_name(key))
        .map(|(.., block)| &block.body);
    let unfilled = unfilled::problems(bodies);
    if !unfilled.is_empty() {
        return Err(unfilled);
    }
    let Labels {
        problems,
        names,
        found,
    } = labels(&blocks, &lower);
    if !problems.is_empty() {
        return Err(problems);
    }
    let Some(block) = found.and_then(|(key, block)| (&key == name).then_some(block))
    else {
        let fix = if names.is_empty() {
            format!("Add a `connector` block with the name {:?}", name.as_str())
        } else {
            let names: Vec<_> = names.iter().map(Name::as_str).collect();
            format!("Use one of {names:?}")
        };
        let message = format!(
            "no `connector` block of the files has the name {:?}",
            name.as_str()
        );
        return Err(vec![Diagnostic::new(NO_CONNECTOR, None, message, fix)]);
    };
    let kind = crate::required(block, "kind", read::name, KIND_FIX.into());
    let node = crate::required(block, "node", read::name, NODE_FIX.into());
    let config = Checked::new(config(block)).map_err(|error| Diagnostic::from(&error));
    match (kind, node, config) {
        (Ok(kind), Ok(node), Ok(config)) => {
            let at = block.labels.first().and_then(|label| label.span);
            writes(name, Channels::default(), at).map_err(|problem| vec![problem])?;
            Ok(Connector::new(kind, node, config))
        }
        (kind, node, config) => {
            let mut problems: Vec<_> = [kind.err(), node.err(), config.err()]
                .into_iter()
                .flatten()
                .collect();
            crate::sort(&mut problems);
            Err(problems)
        }
    }
}

/// What `check` makes of the labels of `blocks` at one name in ASCII lower case.
struct Labels<'a> {
    /// The problems at each `connector` label at the name.
    problems: Vec<Diagnostic>,
    /// The key of each `connector` block that `check` takes, at any name.
    names: BTreeSet<Name>,
    /// The first `connector` block that `check` takes at the name, with its key.
    found: Option<(Name, &'a Block)>,
}

/// The labels of `blocks`, in span order, at the name `lower` in ASCII lower case.
fn labels<'a>(blocks: &[&'a Block], lower: &str) -> Labels<'a> {
    let connector = definition::Kind::Connector;
    let at_name = |key: &Name| key.as_str().to_ascii_lowercase() == lower;
    let mut at = Vec::new();
    let mut seen = BTreeSet::new();
    let mut labels = Labels {
        problems: Vec::new(),
        names: BTreeSet::new(),
        found: None,
    };
    for &block in blocks {
        let (Some(&(kind, _)), [label]) = (
            crate::KINDS
                .iter()
                .find(|(kind, _)| kind.as_str() == &*block.keyword),
            block.labels.as_slice(),
        ) else {
            continue;
        };
        match crate::key(label, kind) {
            Ok(key) => {
                let first = seen.insert(key.as_str().to_ascii_lowercase());
                if kind == connector && first {
                    labels.names.insert(key.clone());
                }
                if at_name(&key) {
                    at.push((label, kind));
                    if kind == connector {
                        labels.found = labels.found.or(Some((key, block)));
                    }
                }
            }
            Err(problem) => {
                if kind == connector
                    && read::label(label).is_ok_and(|key| at_name(&key))
                {
                    labels.problems.push(problem);
                }
            }
        }
    }
    // `check` reports a repeat at the later block, so only a later `connector` counts.
    if let Some((&first, later)) = at.split_first() {
        let later = later.iter().filter(|(_, kind)| *kind == connector);
        let at = std::iter::once(first).chain(later.copied()).collect();
        let at = duplicate::Labels::from([(lower.into(), at)]);
        labels.problems.extend(duplicate::in_labels(&at));
    }
    labels
}

/// The body of `block` without the keys that `check` reads.
fn config(block: &Block) -> Document {
    let attributes = block.body.attributes.iter();
    let attributes = attributes.filter(|attribute| !KEYS.contains(&&*attribute.key));
    Document {
        attributes: Map::new(attributes.cloned().collect())
            .expect("the keys of a map are unique, so a part of one has unique keys"),
        blocks: block.body.blocks.clone(),
    }
}

/// What a connector writes to the mesh.
#[derive(Debug)]
pub(crate) struct Writes {
    /// The channels that its kind writes.
    channels: Vec<Name>,
    /// The status index that it implies.
    index: Name,
    /// The status data channels that it implies, by name.
    data: Vec<(Name, Data<Name>)>,
    /// The span of the connector's label.
    label: Option<Span>,
}

impl Writes {
    /// Each channel that its kind writes.
    pub(crate) fn channels(&self) -> impl Iterator<Item = &Name> {
        self.channels.iter()
    }

    /// Each channel that it implies: the status index, then the data.
    fn implied_names(&self) -> impl Iterator<Item = &Name> {
        let data = self.data.iter().map(|(name, _)| name);
        std::iter::once(&self.index).chain(data)
    }

    /// Each channel that it implies, with its kind: the status index, then the data.
    fn implied(&self) -> impl Iterator<Item = (&Name, Kind<Name>)> {
        let index = Kind::Index {
            error: None,
            control: None,
        };
        let data = self
            .data
            .iter()
            .map(|(name, data)| (name, Kind::Data(data.clone())));
        std::iter::once((&self.index, index)).chain(data)
    }

    /// The status index that it implies.
    pub(crate) fn index(&self) -> &Name {
        &self.index
    }
}

/// What the connector at `key` writes, whose kind gives `channels`.
///
/// # Errors
///
/// `config.long-name` at `at`, the connector's label, when a status name is longer
/// than [`Name::MAX_BYTES`].
pub(crate) fn writes(
    key: &Name,
    channels: Channels,
    at: Option<Span>,
) -> Result<Writes, Diagnostic> {
    let (time, status) = status::channels(key, &channels.counts).map_err(|error| {
        let name::Error::Long { bytes } = error else {
            unreachable!("invariant: each status name of a connector reads: {error}");
        };
        Diagnostic::new(
            LONG_NAME,
            at,
            format!(
                "the name of a status channel of the connector `{key}` is {bytes} \
                 bytes, and the most is {}",
                Name::MAX_BYTES
            ),
            "Shorten the name of the connector".into(),
        )
    })?;
    let data = status.into_iter().map(|(name, sample)| {
        let data = Data::new(time.clone(), None, DataType::Sample(sample), None);
        let data = data.expect("invariant: a status channel has no unit");
        (name, data)
    });
    let data = data.collect();
    Ok(Writes {
        channels: channels.writes,
        index: time,
        data,
        label: at,
    })
}

/// Adds each channel that each connector implies to the entries, with the span of the
/// connector's label. Reports `config.implied-channel` at each block whose key is, in
/// any ASCII case, an implied channel, and as [`implied_writes`] does.
pub(crate) fn imply(found: &mut Found<'_>) {
    for (connector, writes) in &found.writes {
        let at = writes.label;
        for (channel, kind) in writes.implied() {
            let lower = channel.as_str().to_ascii_lowercase();
            let Some(labels) = found.labels.get(lower.as_str()) else {
                let definition = Definition::Channel(kind);
                let entry = Entry {
                    definition,
                    label_span: at,
                };
                found.entries.insert(channel.clone(), entry);
                continue;
            };
            let implied = Implied {
                connector,
                channel,
                label: at,
            };
            for (label, _) in labels {
                found.diagnostics.push(implied.refuse(
                    label.span,
                    "a block cannot have its name",
                    "Give the block another name".into(),
                ));
            }
        }
    }
    let refused = implied_writes(&found.writes);
    found.diagnostics.extend(refused);
}

/// The `config.implied-channel` diagnostic, at the label of the connector that writes
/// it, of each channel that the kind of a connector of `writes` writes and that a
/// connector of `writes` implies, in any ASCII case. A connector gives one for each
/// such channel, however many times its kind writes it.
pub(crate) fn implied_writes(writes: &BTreeMap<Name, Writes>) -> Vec<Diagnostic> {
    let mut implied = BTreeMap::new();
    for (connector, writes) in writes {
        for channel in writes.implied_names() {
            let lower = channel.as_str().to_ascii_lowercase();
            let channel = Implied {
                connector,
                channel,
                label: writes.label,
            };
            implied.insert(lower, channel);
        }
    }
    let mut diagnostics = Vec::new();
    for (writer, writes) in writes {
        let mut refused = BTreeSet::new();
        for channel in &writes.channels {
            let lower = channel.as_str().to_ascii_lowercase();
            if let Some(implied) = implied.get(&lower)
                && refused.insert(lower)
            {
                diagnostics.push(implied.refuse(
                    writes.label,
                    &format!("the connector `{writer}` cannot write it"),
                    format!("Write another channel from the connector `{writer}`"),
                ));
            }
        }
    }
    diagnostics
}

/// The `config.implied-channel` diagnostic of each channel that a connector of
/// `writes` implies and that `definitions`, the definitions of a spec with the name
/// of each channel by key in `names`, lack or hold as another channel, then of each
/// channel that no connector implies and that has an edge to a channel that a
/// connector implies, once for each channel that it points at.
pub(crate) fn implied_definitions(
    definitions: &BTreeMap<Name, definition::Definition>,
    names: &BTreeMap<types::channel::Key, &Name>,
    writes: &BTreeMap<Name, Writes>,
) -> Vec<Diagnostic> {
    let implied: Vec<(Implied<'_>, Kind<Name>)> = writes
        .iter()
        .flat_map(|(connector, writes)| {
            writes.implied().map(move |(channel, kind)| {
                let implied = Implied {
                    connector,
                    channel,
                    label: writes.label,
                };
                (implied, kind)
            })
        })
        .collect();
    let mut diagnostics = Vec::new();
    for (implied, kind) in &implied {
        let held = match definitions.get(implied.channel) {
            Some(definition::Definition::Channel(stored)) => {
                // An edge to a key that is no channel is `None`, so it differs.
                let edges = stored
                    .kind
                    .clone()
                    .map(|key| names.get(&key).copied().cloned());
                edges == kind.clone().map(Some)
            }
            _ => false,
        };
        if !held {
            diagnostics.push(implied.refuse(
                None,
                "the spec must hold it as the connector gives it",
                "Make the plan file with `plan`".into(),
            ));
        }
    }
    let by_name: BTreeMap<&Name, &Implied<'_>> = implied
        .iter()
        .map(|(implied, _)| (implied.channel, implied))
        .collect();
    for (from, definition) in definitions {
        let definition::Definition::Channel(channel) = definition else {
            continue;
        };
        if by_name.contains_key(from) {
            continue;
        }
        let mut refused = BTreeSet::new();
        for (_, key) in channel.kind.edges() {
            if let Some(implied) = names.get(key).and_then(|to| by_name.get(*to))
                && refused.insert(implied.channel)
            {
                diagnostics.push(implied.refuse(
                    None,
                    &format!("the channel `{from}` cannot point at it"),
                    "Make the plan file with `plan`".into(),
                ));
            }
        }
    }
    diagnostics
}

/// A channel that a connector implies.
struct Implied<'a> {
    connector: &'a Name,
    channel: &'a Name,
    /// The span of the connector's label.
    label: Option<Span>,
}

impl Implied<'_> {
    /// The `config.implied-channel` diagnostic at `at`, which states that `refused`,
    /// with a note at the connector.
    fn refuse(&self, at: Option<Span>, refused: &str, fix: String) -> Diagnostic {
        let Implied {
            connector,
            channel,
            label,
        } = self;
        let mut diagnostic = Diagnostic::new(
            IMPLIED_CHANNEL,
            at,
            format!(
                "the connector `{connector}` implies the channel `{channel}`, so \
                 {refused}"
            ),
            fix,
        );
        diagnostic.notes.extend(label.map(|span| Note {
            span,
            text: "the connector".into(),
        }));
        diagnostic
    }
}
