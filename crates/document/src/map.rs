use crate::value::Value;
use crate::{Error, Span};

/// Attributes sorted by key, with unique keys. A document's attributes and a map value
/// are each a map.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Map(Vec<Attribute>);

/// A key and its value. `==` does not read spans.
#[derive(Clone, Debug)]
pub struct Attribute {
    /// The key.
    pub key: Box<str>,
    /// Where the key is.
    pub key_span: Option<Span>,
    /// The value.
    pub value: Value,
}

impl PartialEq for Attribute {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.value == other.value
    }
}

impl Eq for Attribute {}

impl Map {
    /// Sorts attributes by key.
    ///
    /// # Errors
    ///
    /// Returns one [`Error::DuplicateKey`] for each attribute whose key an earlier
    /// attribute has. `first` is the span of the earliest attribute with that key.
    pub fn new(mut attributes: Vec<Attribute>) -> Result<Self, Vec<Error>> {
        attributes.sort_by(|a, b| a.key.cmp(&b.key));
        let mut errors = Vec::new();
        for run in attributes.chunk_by(|a, b| a.key == b.key) {
            if let [first, rest @ ..] = run {
                errors.extend(rest.iter().map(|repeat| Error::DuplicateKey {
                    key: repeat.key.clone(),
                    first: first.key_span,
                    second: repeat.key_span,
                }));
            }
        }
        if errors.is_empty() {
            Ok(Self(attributes))
        } else {
            Err(errors)
        }
    }

    /// The attribute with `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Attribute> {
        let i = self.0.binary_search_by(|a| (*a.key).cmp(key)).ok()?;
        self.0.get(i)
    }

    /// The attributes, by key.
    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Attribute> {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Kind;
    use crate::{Position, Source};
    use proptest::prelude::*;

    fn span(offset: u32) -> Span {
        let at = Position {
            offset,
            line: 0,
            column: offset,
        };
        Span::new(Source(0), at, at).unwrap()
    }

    fn attribute(key: &str, offset: u32) -> Attribute {
        Attribute {
            key: key.into(),
            key_span: Some(span(offset)),
            value: Value {
                kind: Kind::Integer(offset.into()),
                span: None,
            },
        }
    }

    fn keys(map: &Map) -> Vec<&str> {
        map.iter().map(|a| &*a.key).collect()
    }

    fn duplicate(key: &str, first: u32, second: u32) -> Error {
        Error::DuplicateKey {
            key: key.into(),
            first: Some(span(first)),
            second: Some(span(second)),
        }
    }

    mod new {
        use super::*;

        #[test]
        fn sorts_by_key() {
            let map =
                Map::new(vec![attribute("url", 0), attribute("node", 1)]).unwrap();
            assert_eq!(keys(&map), ["node", "url"]);
        }

        #[test]
        fn allows_no_attributes() {
            assert_eq!(Map::new(Vec::new()).unwrap(), Map::default());
        }

        #[test]
        fn reports_a_duplicate_with_both_spans_in_given_order() {
            let err = Map::new(vec![
                attribute("url", 0),
                attribute("node", 1),
                attribute("url", 2),
            ])
            .unwrap_err();
            assert_eq!(err, [duplicate("url", 0, 2)]);
        }

        #[test]
        fn reports_each_repeat_against_the_first() {
            let err = Map::new(vec![
                attribute("b", 0),
                attribute("a", 1),
                attribute("b", 2),
                attribute("a", 3),
                attribute("b", 4),
            ])
            .unwrap_err();
            assert_eq!(
                err,
                [
                    duplicate("a", 1, 3),
                    duplicate("b", 0, 2),
                    duplicate("b", 0, 4)
                ]
            );
        }

        proptest! {
            #[test]
            fn gives_one_map_for_every_order(
                (sorted, shuffled) in prop::collection::btree_set("[a-z_]{1,6}", 0..12)
                    .prop_map(Vec::from_iter)
                    .prop_flat_map(|keys: Vec<String>| {
                        (Just(keys.clone()), Just(keys).prop_shuffle())
                    }),
            ) {
                let attributes = |keys: &[String]| -> Vec<Attribute> {
                    keys.iter().map(|k| attribute(k, 0)).collect()
                };
                let map = Map::new(attributes(&shuffled)).unwrap();
                prop_assert_eq!(&map, &Map::new(attributes(&sorted)).unwrap());
                prop_assert_eq!(keys(&map), sorted);
            }

            #[test]
            fn reports_every_repeat_and_nothing_else(
                keys in prop::collection::vec("[ab]{1,2}", 0..12),
            ) {
                let given: Vec<_> = keys
                    .iter()
                    .zip(0u32..)
                    .map(|(k, offset)| attribute(k, offset))
                    .collect();
                let repeats = keys
                    .iter()
                    .enumerate()
                    .filter(|(i, k)| keys[..*i].contains(k))
                    .count();
                match Map::new(given) {
                    Ok(map) => prop_assert_eq!((map.iter().len(), repeats), (keys.len(), 0)),
                    Err(errors) => {
                        prop_assert_eq!(errors.len(), repeats);
                        for error in errors {
                            let Error::DuplicateKey { key, first, second } = error;
                            let first = usize::try_from(first.unwrap().start().offset).unwrap();
                            let second = usize::try_from(second.unwrap().start().offset).unwrap();
                            prop_assert!(first < second, "{first} is not before {second}");
                            prop_assert_eq!(keys.iter().position(|k| **k == *key), Some(first));
                            prop_assert_eq!(&*keys[second], &*key);
                        }
                    }
                }
            }
        }
    }

    mod get {
        use super::*;

        #[test]
        fn finds_each_key() {
            let map =
                Map::new(vec![attribute("url", 0), attribute("node", 1)]).unwrap();
            assert_eq!(map.get("url"), Some(&attribute("url", 0)));
            assert_eq!(map.get("node"), Some(&attribute("node", 1)));
        }

        #[test]
        fn returns_none_for_a_missing_key() {
            let map = Map::new(vec![attribute("url", 0)]).unwrap();
            assert_eq!(map.get("node"), None);
        }
    }
}
