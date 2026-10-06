//! The connector definition: a set of devices that one kind drives on one node.

use document::Document;
use document::encoding::{self, TooDeep};
use types::name::Name;

/// A connector. Its name is the tree key, so it is not part of the definition. Only
/// the kind decodes `config`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connector {
    kind: Name,
    node: Name,
    config: Document,
}

impl Connector {
    /// Makes a connector of `kind` that runs on the node named `node`.
    ///
    /// # Errors
    ///
    /// Returns [`TooDeep`] when `config` nests deeper than a document may, so it has
    /// no encoding.
    pub fn new(kind: Name, node: Name, config: Document) -> Result<Self, TooDeep> {
        encoding::check(&config)?;
        Ok(Self { kind, node, config })
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
    pub const fn config(&self) -> &Document {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use document::encoding::DEPTH_MAX;
    use document::value::{Kind, Value};
    use document::{Attribute, Map};

    use super::*;

    fn name(text: &str) -> Name {
        text.parse().unwrap()
    }

    /// A config whose one attribute is a list nested `depth` levels deep.
    fn nested(depth: usize) -> Document {
        let mut value = Value {
            kind: Kind::Bool(true),
            span: None,
        };
        for _ in 0..depth {
            value = Value {
                kind: Kind::List(vec![value]),
                span: None,
            };
        }
        let attribute = Attribute {
            key: "a".into(),
            key_span: None,
            value,
        };
        Document {
            attributes: Map::new(vec![attribute]).unwrap(),
            blocks: Vec::new(),
        }
    }

    #[test]
    fn refuses_a_config_with_no_encoding() {
        let made = Connector::new(name("modbus"), name("gw_1"), nested(DEPTH_MAX));
        assert_eq!(made.unwrap().config(), &nested(DEPTH_MAX));
        let made = Connector::new(name("modbus"), name("gw_1"), nested(DEPTH_MAX + 1));
        assert_eq!(made, Err(TooDeep { span: None }));
    }
}
