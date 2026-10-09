use document::diagnostic::Diagnostic;
use document::encoding::Checked;
use document::{Block, Document, Map, read};
use spec::connector::Connector;
use spec::definition;

use crate::{Definition, Found, Writer, span};

/// The keys that `check` reads. The kind reads each other key and block.
const KEYS: [&str; 2] = ["kind", "node"];

/// Checks a `connector` block and gives its connector. The kind of the block checks
/// its config, which is the body without `kind` and `node`, also when `node` is
/// missing.
pub(crate) fn check(found: &mut Found<'_>, block: &Block) -> Option<Definition> {
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
    let checked = found.kinds.check(kind.as_str(), at, config.document());
    let channels = match checked {
        Ok(channels) => channels,
        Err(diagnostics) => {
            found.diagnostics.extend(diagnostics);
            return None;
        }
    };
    let node = node.ok()?;
    let at = span(block, "node");
    // A label that is no name has no entry, and `found.key` reports it.
    if let [label] = block.labels.as_slice()
        && let Ok(name) = read::label(label)
    {
        let writes = channels.writes;
        found.writers.insert(
            name,
            Writer {
                node: node.clone(),
                at,
                writes,
            },
        );
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
