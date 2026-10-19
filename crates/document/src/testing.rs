//! The walk of an example over the body that states it, for the tests of each crate
//! that states a body.

use crate::diagnostic::Diagnostic;
use crate::schema::{Body, Presence};
use crate::{Attribute, Document, Map};

/// `example`, which is of `body`, and each block inside it, each with the body of its
/// form and its path of block indexes from `example`.
///
/// # Panics
///
/// When a block of `example` is not a block of its body, or a block inside a body has
/// more than one form.
#[must_use]
pub fn sites(
    body: &'static Body,
    example: &Document,
) -> Vec<(&'static Body, Vec<usize>)> {
    let mut sites = Vec::new();
    walk(body, example, Vec::new(), &mut sites);
    sites
}

fn walk(
    body: &'static Body,
    document: &Document,
    path: Vec<usize>,
    sites: &mut Vec<(&'static Body, Vec<usize>)>,
) {
    for (index, inner) in document.blocks.iter().enumerate() {
        let of = body
            .blocks
            .iter()
            .find(|of| of.keyword == &*inner.keyword)
            .expect("a block of the body");
        let [form] = of.forms else {
            panic!("a block inside a body has one form: {}", of.keyword);
        };
        let mut at = path.clone();
        at.push(index);
        walk(form, &inner.body, at, sites);
    }
    sites.push((body, path));
}

/// The body of the block of `document` at `path`, or `document` when `path` is empty.
///
/// # Panics
///
/// When `path` names no block.
#[must_use]
pub fn at<'a>(document: &'a Document, path: &[usize]) -> &'a Document {
    path.iter().fold(document, |body, index| {
        &body.blocks.get(*index).expect("a block at the path").body
    })
}

/// The body of the block of `document` at `path`, or `document` when `path` is empty,
/// to change.
///
/// # Panics
///
/// When `path` names no block.
pub fn at_mut<'a>(document: &'a mut Document, path: &[usize]) -> &'a mut Document {
    path.iter().fold(document, |body, index| {
        &mut body
            .blocks
            .get_mut(*index)
            .expect("a block at the path")
            .body
    })
}

/// `document` with the attributes of its body at `path` changed by `change`.
///
/// # Panics
///
/// When `path` names no block, or `change` leaves two attributes with one key.
#[must_use]
pub fn change(
    mut document: Document,
    path: &[usize],
    change: impl FnOnce(&mut Vec<Attribute>),
) -> Document {
    let body = at_mut(&mut document, path);
    let mut attributes = body.attributes.iter().cloned().collect();
    change(&mut attributes);
    body.attributes = Map::new(attributes).expect("unique keys");
    document
}

/// `document` without the attribute `key` in its body at `path`, or `None` when that
/// body has none.
///
/// # Panics
///
/// When `path` names no block.
#[must_use]
pub fn without(document: Document, path: &[usize], key: &str) -> Option<Document> {
    at(&document, path).attributes.get(key)?;
    Some(change(document, path, |attributes| {
        attributes.retain(|attribute| &*attribute.key != key);
    }))
}

/// Whether `held`, of `body`, needs `key`: its absence gives a diagnostic.
///
/// # Panics
///
/// When `body` states no attribute `key`.
#[must_use]
pub fn needs(body: &Body, key: &str, held: &Document) -> bool {
    body.attributes
        .iter()
        .find(|attribute| attribute.name == key)
        .expect("a key of the body")
        .presence
        .needed(&held.attributes)
}

/// Asserts that each example holds to its body. At each site of each example,
/// `problems` finds none in the example, none with an attribute removed that its site
/// does not need, and none with each attribute with a default of its site removed. Each
/// name of each `Unless` of a site is an attribute of that site. Each item of
/// `examples` is a key that `problems` gets, the body, and the example.
///
/// # Panics
///
/// When an assert fails, or when no site has an attribute that it does not need, no
/// site has an attribute with a default, or no site has an `Unless`.
pub fn assert_examples<K>(
    examples: impl IntoIterator<Item = (K, &'static Body, Document)>,
    problems: impl Fn(&K, &Document) -> Vec<Diagnostic>,
) {
    let (mut optional, mut defaults, mut names) = (0_usize, 0_usize, 0_usize);
    for (key, outer, example) in examples {
        assert_eq!(problems(&key, &example), [], "{}", outer.summary);
        for (body, path) in sites(outer, &example) {
            for attribute in body.attributes {
                if needs(body, attribute.name, at(&example, &path)) {
                    continue;
                }
                let Some(example) = without(example.clone(), &path, attribute.name)
                else {
                    continue;
                };
                let problems = problems(&key, &example);
                assert_eq!(problems, [], "{}: {}", body.summary, attribute.name);
                optional = optional.saturating_add(1);
            }
            for attribute in body.attributes {
                let Presence::Unless(others) = attribute.presence else {
                    continue;
                };
                assert!(!others.is_empty(), "{}: {}", body.summary, attribute.name);
                for other in others {
                    let stated = body.attributes.iter().any(|key| key.name == *other);
                    assert!(stated, "{}: {}: {other}", body.summary, attribute.name);
                    names = names.saturating_add(1);
                }
            }
            let held: Vec<_> = body
                .attributes
                .iter()
                .filter(|attribute| matches!(attribute.presence, Presence::Default(_)))
                .map(|attribute| attribute.name)
                .collect();
            if held.is_empty() {
                continue;
            }
            let without = change(example.clone(), &path, |attributes| {
                attributes.retain(|attribute| !held.contains(&&*attribute.key));
            });
            let problems = problems(&key, &without);
            assert_eq!(problems, [], "{}: {held:?}", body.summary);
            defaults = defaults.saturating_add(held.len());
        }
    }
    let counts = [optional, defaults, names];
    assert!(
        !counts.contains(&0),
        "optional, defaults, names: {counts:?}"
    );
}

/// Whether `problem` is a missing attribute that names `key`.
#[must_use]
pub fn misses(problem: &Diagnostic, key: &str) -> bool {
    problem.code.as_str() == "document.missing-attribute"
        && problem
            .message
            .split_once(" has no ")
            .is_some_and(|(_, keys)| keys.contains(&format!("`{key}`")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Block;
    use crate::diagnostic::Code;
    use crate::read;
    use crate::schema;
    use crate::value::{Kind, Value};
    use std::cell::RefCell;

    const fn attribute(name: &'static str, presence: Presence) -> schema::Attribute {
        schema::Attribute {
            name,
            value: "an integer",
            presence,
            summary: "A number.",
        }
    }

    const fn holding(
        summary: &'static str,
        attributes: &'static [schema::Attribute],
        blocks: &'static [schema::Block],
    ) -> Body {
        Body {
            summary,
            attributes,
            blocks,
            examples: &[],
        }
    }

    const OTHER: Body = holding("Other.", &[attribute("d", Presence::Optional)], &[]);

    const INNER: Body = holding(
        "Inner.",
        &[attribute("c", Presence::Required)],
        &[schema::Block {
            keyword: "other",
            label: None,
            forms: &[OTHER],
        }],
    );

    const OUTER: Body = holding(
        "Outer.",
        &[
            attribute("a", Presence::Required),
            attribute("b", Presence::Default("1")),
            attribute("u", Presence::Unless(&["a"])),
        ],
        &[
            schema::Block {
                keyword: "inner",
                label: None,
                forms: &[INNER],
            },
            schema::Block {
                keyword: "other",
                label: None,
                forms: &[OTHER],
            },
        ],
    );

    const DEFAULTS: Body = holding(
        "Defaults.",
        &[
            attribute("b", Presence::Default("1")),
            attribute("e", Presence::Default("1")),
        ],
        &[],
    );

    const UNKNOWN: Body =
        holding("Unknown.", &[attribute("u", Presence::Unless(&["z"]))], &[]);

    const UNNAMED: Body =
        holding("Unnamed.", &[attribute("u", Presence::Unless(&[]))], &[]);

    fn problem() -> Diagnostic {
        Diagnostic::new(
            Code::new("document.test"),
            None,
            "a problem".into(),
            "Fix it".into(),
        )
    }

    fn document(keys: &[&str], blocks: Vec<Block>) -> Document {
        let attributes = keys
            .iter()
            .map(|key| Attribute {
                key: (*key).into(),
                key_span: None,
                value: Value {
                    kind: Kind::Integer(1),
                    span: None,
                },
            })
            .collect();
        Document {
            attributes: Map::new(attributes).expect("unique keys"),
            blocks,
        }
    }

    fn block(keyword: &str, body: Document) -> Block {
        Block {
            keyword: keyword.into(),
            keyword_span: None,
            labels: Vec::new(),
            body,
            span: None,
        }
    }

    /// `outer { a b inner { c other { d } } other { d } }`, with `other` last so
    /// that a site that `outer` gives `other` differs from one that it gives `inner`.
    fn example() -> Document {
        let other = || block("other", document(&["d"], Vec::new()));
        let inner = block("inner", document(&["c"], vec![other()]));
        document(&["a", "b"], vec![inner, other()])
    }

    #[test]
    fn gives_each_site_with_its_body_and_path() {
        assert_eq!(
            sites(&OUTER, &example()),
            [
                (&OTHER, vec![0, 0]),
                (&INNER, vec![0]),
                (&OTHER, vec![1]),
                (&OUTER, vec![]),
            ]
        );
    }

    #[test]
    fn gives_the_body_at_a_path() {
        let mut example = example();
        let deep = document(&["d"], Vec::new());
        assert_eq!(*at(&example, &[0, 0]), deep);
        assert_eq!(*at(&example, &[]), example);
        assert_eq!(*at_mut(&mut example, &[0, 0]), deep);
    }

    #[test]
    fn changes_the_attributes_of_the_body_at_a_path() {
        let changed = change(example(), &[1], |attributes| {
            attributes.retain(|attribute| &*attribute.key != "d");
        });
        let mut expected = example();
        expected.blocks[1].body = Document::default();
        assert_eq!(changed, expected);
    }

    #[test]
    fn removes_one_attribute_of_the_body_at_a_path() {
        let mut expected = example();
        expected.attributes = document(&["b"], Vec::new()).attributes;
        assert_eq!(without(example(), &[], "a"), Some(expected));
        assert_eq!(without(example(), &[1], "a"), None);
    }

    #[test]
    fn needs_an_attribute_as_its_presence_states() {
        let held = document(&["a"], Vec::new());
        assert!(needs(&OUTER, "a", &held));
        assert!(!needs(&OUTER, "b", &held));
        assert!(!needs(&OUTER, "u", &held));
        assert!(needs(&OUTER, "u", &Document::default()));
    }

    #[test]
    fn checks_each_example_without_each_attribute_it_does_not_need() {
        let checked = RefCell::new(Vec::new());
        assert_examples([("outer", &OUTER, example())], |key, document| {
            assert_eq!(*key, "outer");
            checked.borrow_mut().push(document.clone());
            Vec::new()
        });
        let without =
            |path: &[usize], key| without(example(), path, key).expect("held");
        assert_eq!(
            checked.into_inner(),
            [
                example(),
                without(&[0, 0], "d"),
                without(&[1], "d"),
                without(&[], "b"),
                without(&[], "b"),
            ]
        );
    }

    #[test]
    #[should_panic(expected = "failed: Outer.\n")]
    fn fails_an_example_with_a_problem() {
        assert_examples([((), &OUTER, example())], |(), _| vec![problem()]);
    }

    #[test]
    #[should_panic(expected = "failed: Other.: d\n")]
    fn fails_an_example_without_an_attribute_it_does_not_need() {
        assert_examples([((), &OUTER, example())], |(), document| {
            let deep = at(document, &[0, 0]);
            if *deep == Document::default() {
                vec![problem()]
            } else {
                Vec::new()
            }
        });
    }

    #[test]
    #[should_panic(expected = "failed: Defaults.: [\"b\", \"e\"]\n")]
    fn fails_an_example_without_each_attribute_with_a_default() {
        let example = document(&["b", "e"], Vec::new());
        assert_examples([((), &DEFAULTS, example)], |(), document| {
            if *document == Document::default() {
                vec![problem()]
            } else {
                Vec::new()
            }
        });
    }

    #[test]
    #[should_panic(expected = "Unknown.: u: z")]
    fn fails_an_unless_that_names_no_attribute_of_its_body() {
        let example = document(&[], Vec::new());
        assert_examples([((), &UNKNOWN, example)], |(), _| Vec::new());
    }

    #[test]
    #[should_panic(expected = "Unnamed.: u")]
    fn fails_an_unless_that_names_nothing() {
        let example = document(&[], Vec::new());
        assert_examples([((), &UNNAMED, example)], |(), _| Vec::new());
    }

    #[test]
    #[should_panic(expected = "optional, defaults, names: [0, 0, 0]")]
    fn fails_examples_that_check_nothing() {
        let examples: [((), &'static Body, Document); 0] = [];
        assert_examples(examples, |(), _| Vec::new());
    }

    #[test]
    fn misses_only_a_key_that_a_missing_attribute_names() {
        let problem = read::missing("the block", None, &["a", "u"], "Add one".into());
        assert!(misses(&problem, "a"));
        assert!(misses(&problem, "u"));
        assert!(!misses(&problem, "b"));
        let other = Diagnostic::new(
            Code::new("document.unknown-attribute"),
            None,
            "the block has no `a`".into(),
            "Remove it".into(),
        );
        assert!(!misses(&other, "a"));
    }
}
