//! What the body of each kind of block takes: the one list of keys that its check
//! reads, and the text that the docs give.

use crate::Map;

/// What the body of one kind of block takes. Its check refuses each attribute and
/// block that it does not state, other than those that it gives to another check,
/// such as the kind of a `connector`. `foundation docs` states each form of each block
/// that a config file can hold and the body of each connector kind, each with the body
/// of each block inside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Body {
    /// One sentence on what the body defines, with the `kind` that picks it when its
    /// block has more than one form.
    pub summary: &'static str,
    /// Each attribute that the body takes, in the order that the docs give them.
    pub attributes: &'static [Attribute],
    /// Each block that the body can hold.
    pub blocks: &'static [Block],
    /// Examples in HCL that the check takes with no diagnostic. In a body that
    /// `foundation docs` states, each attribute and block above is in at least one, or,
    /// for the body of a block inside another body, in an example of that outer body.
    /// A body that it does not state, such as the body of a whole file, may have none.
    pub examples: &'static [&'static str],
}

/// One kind of block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    /// The keyword of the block, such as `channel`.
    pub keyword: &'static str,
    /// What the label names, or `None` for a block with no label.
    pub label: Option<&'static str>,
    /// The body of each form of the block, picked by its `kind`.
    pub forms: &'static [Body],
}

/// One attribute of a body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attribute {
    /// The key of the attribute.
    pub name: &'static str,
    /// The value it takes, as the docs state it, such as `a string`.
    pub value: &'static str,
    /// When the check needs the attribute.
    pub presence: Presence,
    /// One sentence on what the attribute sets.
    pub summary: &'static str,
}

/// When the check of a body needs an attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    /// Each body holds it.
    Required,
    /// Each body holds it, or each attribute that this names in its place.
    Unless(&'static [&'static str]),
    /// A body can leave it out, which gives this value, as the docs state it.
    Default(&'static str),
    /// A body can leave it out, which gives no value. The summary of the body states
    /// each rule that needs it with other attributes.
    Optional,
}

impl Presence {
    /// Whether a body whose attributes are `attributes` must hold the attribute:
    /// always for `Required`, for `Unless` when a key that it names is not in
    /// `attributes`, and never for `Default` or `Optional`.
    #[must_use]
    pub fn needed(self, attributes: &Map) -> bool {
        match self {
            Self::Required => true,
            Self::Unless(others) => {
                !others.iter().all(|other| attributes.get(other).is_some())
            }
            Self::Default(_) | Self::Optional => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Presence;
    use crate::value::{Kind, Value};
    use crate::{Attribute, Map};

    fn map(keys: &[&str]) -> Map {
        let attributes = keys.iter().map(|key| Attribute {
            key: (*key).into(),
            key_span: None,
            value: Value {
                kind: Kind::Bool(true),
                span: None,
            },
        });
        Map::new(attributes.collect()).expect("the keys are unique")
    }

    #[test]
    fn needs_an_attribute_unless_the_body_holds_each_one_in_its_place() {
        let unless = Presence::Unless(&["bucket", "org"]);
        let cases: [(&[&str], bool); 4] = [
            (&[], true),
            (&["bucket"], true),
            (&["org"], true),
            (&["bucket", "org"], false),
        ];
        for (keys, needed) in cases {
            assert_eq!(unless.needed(&map(keys)), needed, "{keys:?}");
        }
        assert!(Presence::Required.needed(&map(&["bucket"])));
        assert!(!Presence::Default("1").needed(&map(&[])));
        assert!(!Presence::Optional.needed(&map(&[])));
    }
}
