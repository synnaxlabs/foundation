use ::connector::kind::Channels;
use ::connector::status;
use document::diagnostic::{Code, Diagnostic, Note};
use document::encoding::Checked;
use document::{Block, Document, Map, Span, read};
use spec::channel::{Data, Kind};
use spec::connector::Connector;
use spec::data_type::DataType;
use spec::definition;
use std::collections::BTreeMap;
use types::name::{self, Name};

use crate::{Definition, Entry, Found, LONG_NAME, span};

const IMPLIED_CHANNEL: Code = Code::new("config.implied-channel");

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
    let fix = "Add a `kind` attribute with the connector's kind";
    let kind = found.required(block, "kind", read::name, fix.into());
    let fix = "Add a `node` attribute with the name of the node that runs it, such as \
               \"edge\"";
    let node = found.required(block, "node", read::name, fix.into());
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
    /// The channels that it implies, by name.
    implied: Vec<(Name, Kind<Name>)>,
}

impl Writes {
    /// Each channel that it writes: those of its kind, then those it implies.
    pub(crate) fn names(&self) -> impl Iterator<Item = &Name> {
        let implied = self.implied.iter().map(|(name, _)| name);
        self.channels.iter().chain(implied)
    }

    /// Each index that it implies.
    pub(crate) fn indexes(&self) -> impl Iterator<Item = &Name> {
        self.implied.iter().filter_map(|(name, kind)| match kind {
            Kind::Index { .. } => Some(name),
            Kind::Data(_) => None,
        })
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
    let index = Kind::Index {
        error: None,
        control: None,
    };
    let data = status.into_iter().map(|(name, sample)| {
        let data = Data::new(time.clone(), None, DataType::Sample(sample), None);
        let data = data.expect("invariant: a status channel has no unit");
        (name, Kind::Data(data))
    });
    let implied = std::iter::once((time.clone(), index)).chain(data).collect();
    Ok(Writes {
        channels: channels.writes,
        implied,
    })
}

/// Adds each channel that each connector implies to the entries, with the span of the
/// connector's label. Reports `config.implied-channel` at each block whose key is, in
/// any ASCII case, an implied channel, and as [`writers`] does.
pub(crate) fn imply(found: &mut Found<'_>) {
    for (connector, writes) in &found.writes {
        let at = found.entries[connector].label_span;
        for (channel, kind) in &writes.implied {
            let lower = channel.as_str().to_ascii_lowercase();
            let Some(labels) = found.labels.get(lower.as_str()) else {
                let definition = Definition::Channel(kind.clone());
                let entry = Entry {
                    definition,
                    label_span: at,
                };
                found.entries.insert(channel.clone(), entry);
                continue;
            };
            let owner = Owner {
                connector,
                channel,
                label: at,
            };
            for (label, _) in labels {
                found.diagnostics.push(owner.refuse(
                    label.span,
                    "a block cannot have its name",
                    "Give the block another name".into(),
                ));
            }
        }
    }
    let label = |connector: &Name| found.entries[connector].label_span;
    let refused = writers(&found.writes, label);
    found.diagnostics.extend(refused);
}

/// The `config.implied-channel` diagnostic of each connector of `writes` whose kind
/// writes, in any ASCII case, a channel that a connector of `writes` implies, at the
/// span that `label` gives for the connector.
pub(crate) fn writers(
    writes: &BTreeMap<Name, Writes>,
    label: impl Fn(&Name) -> Option<Span>,
) -> Vec<Diagnostic> {
    let mut owners = BTreeMap::new();
    for (connector, writes) in writes {
        for (channel, _) in &writes.implied {
            let owner = Owner {
                connector,
                channel,
                label: label(connector),
            };
            owners.insert(channel.as_str().to_ascii_lowercase(), owner);
        }
    }
    let mut diagnostics = Vec::new();
    for (writer, writes) in writes {
        for channel in &writes.channels {
            let lower = channel.as_str().to_ascii_lowercase();
            if let Some(owner) = owners.get(&lower) {
                diagnostics.push(owner.refuse(
                    label(writer),
                    &format!("the connector `{writer}` cannot write it"),
                    format!("Write another channel from the connector `{writer}`"),
                ));
            }
        }
    }
    diagnostics
}

/// A channel that a connector implies.
struct Owner<'a> {
    connector: &'a Name,
    channel: &'a Name,
    /// The span of the connector's label.
    label: Option<Span>,
}

impl Owner<'_> {
    /// The `config.implied-channel` diagnostic at `at`, which states that `refused`, with
    /// a note at the connector.
    fn refuse(&self, at: Option<Span>, refused: &str, fix: String) -> Diagnostic {
        let Owner {
            connector,
            channel,
            label,
        } = self;
        let mut diagnostic = Diagnostic::new(
            IMPLIED_CHANNEL,
            at,
            format!(
                "the connector `{connector}` implies the channel `{channel}`, so {refused}"
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
