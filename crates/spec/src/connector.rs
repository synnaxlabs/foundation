//! The connector definition: a set of devices that one kind drives on one node.

use document::encoding::Checked;
use types::name::Name;

/// A connector. Its name is the tree key, so it is not part of the definition. Only
/// the kind decodes `config`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connector {
    kind: Name,
    node: Name,
    config: Checked,
}

impl Connector {
    /// Makes a connector of `kind` that runs on the node named `node`.
    #[must_use]
    pub const fn new(kind: Name, node: Name, config: Checked) -> Self {
        Self { kind, node, config }
    }

    /// The kind that drives the connector.
    #[must_use]
    pub const fn kind(&self) -> &Name {
        &self.kind
    }

    /// The name of the node the connector runs on.
    #[must_use]
    pub const fn node(&self) -> &Name {
        &self.node
    }

    /// The config, which only the kind reads. It keeps the spans it was made with;
    /// the encoding drops them, so a decoded config has none.
    #[must_use]
    pub const fn config(&self) -> &Checked {
        &self.config
    }
}
