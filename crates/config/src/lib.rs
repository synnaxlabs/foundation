//! Checks core definitions in Documents, expands templates, hands connector blocks to
//! kinds, and computes plans, explains, and exports.

mod node_settings;

use std::collections::{BTreeMap, btree_map};

use document::diagnostic::{Code, Diagnostic, Note};
use document::{Attribute, Block, Document, Label, Span, read};
use types::name::Name;

const UNKNOWN_BLOCK: Code = Code::new("config.unknown-block");
const UNKNOWN_ATTRIBUTE: Code = Code::new("config.unknown-attribute");
const LABEL_COUNT: Code = Code::new("config.label-count");
const DUPLICATE_NAME: Code = Code::new("config.duplicate-name");
const RESERVED_NAME: Code = Code::new("config.reserved-name");
const LONG_NAME: Code = Code::new("config.long-name");

/// A checked definition and the label that names it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Entry {
    /// The definition, as the spec tree stores it.
    pub definition: spec::definition::Definition,
    /// The label that names it, for `explain`.
    pub span: Option<Span>,
}

/// Checks the definitions in a mesh's Documents, one Document for each file, and
/// gives each by its tree key, `<label>.@<kind>`.
///
/// # Errors
///
/// Every problem in the Documents, in the order of `documents`, then in source order.
/// A value that a reader or a definition refuses gives only its first problem. A
/// policy's budgets are checked only after all its attributes read.
pub fn check(documents: &[Document]) -> Result<BTreeMap<Name, Entry>, Vec<Diagnostic>> {
    let mut check = Check::default();
    for document in documents {
        let start = check.diagnostics.len();
        for attribute in document.attributes.iter() {
            check.diagnostics.push(Diagnostic::new(
                UNKNOWN_ATTRIBUTE,
                attribute.key_span,
                format!("`{}` is an attribute outside a block", attribute.key),
                "Move it into the block that it sets, or remove it".into(),
            ));
        }
        for block in &document.blocks {
            match &*block.keyword {
                "node_settings" => node_settings::check(&mut check, block),
                keyword => check.diagnostics.push(Diagnostic::new(
                    UNKNOWN_BLOCK,
                    block.keyword_span,
                    format!("`{keyword}` is not a kind of block"),
                    "Use `node_settings`, or remove the block".into(),
                )),
            }
        }
        check.diagnostics[start..]
            .sort_by_key(|diagnostic| diagnostic.span.map(|span| span.start().offset));
    }
    if check.diagnostics.is_empty() {
        Ok(check.entries)
    } else {
        Err(check.diagnostics)
    }
}

/// What `check` has found so far.
#[derive(Debug, Default)]
struct Check<'a> {
    entries: BTreeMap<Name, Entry>,
    diagnostics: Vec<Diagnostic>,
    /// The label of each policy name so far, by block keyword and the name in
    /// lowercase, so that names that differ only in case collide.
    names: BTreeMap<(&'a str, Box<str>), &'a Label>,
}

impl<'a> Check<'a> {
    /// Reads the one label of a policy block as its name, unique among the policies
    /// of its kind, and gives the tree key and the label's span.
    fn key(&mut self, block: &'a Block) -> Option<(Name, Option<Span>)> {
        let keyword = &*block.keyword;
        let label = self.label(block)?;
        let name = self.report(read::name(label))?;
        if name.reserved() {
            self.diagnostics.push(Diagnostic::new(
                RESERVED_NAME,
                label.span,
                format!(
                    "{:?} has a segment that starts with `@`, which is reserved",
                    name.as_str()
                ),
                "Remove the `@` from each segment".into(),
            ));
            return None;
        }
        let suffix = format!(".@{keyword}");
        let most = Name::MAX_BYTES - suffix.len();
        let bytes = name.as_str().len();
        if bytes > most {
            self.diagnostics.push(Diagnostic::new(
                LONG_NAME,
                label.span,
                format!(
                    "the name {:?} is {bytes} bytes, and a `{keyword}` name holds at \
                     most {most}",
                    name.as_str()
                ),
                format!("Shorten the name to at most {most} bytes"),
            ));
            return None;
        }
        let folded = name.as_str().to_ascii_lowercase().into();
        let first = match self.names.entry((keyword, folded)) {
            btree_map::Entry::Occupied(first) => *first.get(),
            btree_map::Entry::Vacant(entry) => {
                entry.insert(label);
                let key = format!("{name}{suffix}").parse();
                let key =
                    key.expect("a name and a reserved keyword segment make a name");
                return Some((key, label.span));
            }
        };
        let mut diagnostic = Diagnostic::new(
            DUPLICATE_NAME,
            label.span,
            format!(
                "the name {:?} repeats the earlier `{keyword}` name {:?}",
                name.as_str(),
                first.text
            ),
            format!(
                "Give each `{keyword}` block a name that differs by more than case"
            ),
        );
        diagnostic.notes.extend(first.span.map(|span| Note {
            span,
            text: "the earlier name".into(),
        }));
        self.diagnostics.push(diagnostic);
        None
    }

    /// The one label of a policy block.
    fn label(&mut self, block: &'a Block) -> Option<&'a Label> {
        let keyword = &*block.keyword;
        let [label] = block.labels.as_slice() else {
            let at = block
                .labels
                .get(1)
                .map_or(block.keyword_span, |label| label.span);
            self.diagnostics.push(Diagnostic::new(
                LABEL_COUNT,
                at,
                format!(
                    "a `{keyword}` block has {} labels, and it needs one, its name",
                    block.labels.len()
                ),
                format!("Write one label, such as `{keyword} \"site_a.budget\"`"),
            ));
            return None;
        };
        Some(label)
    }

    /// The value that a reader gives, or `None` after it reports the reader's
    /// diagnostic.
    fn report<T>(&mut self, read: Result<T, Diagnostic>) -> Option<T> {
        read.map_err(|diagnostic| self.diagnostics.push(diagnostic))
            .ok()
    }

    /// Reports an attribute of a `keyword` block that is not one of `keys`.
    fn unknown_attribute(&mut self, keyword: &str, attribute: &Attribute, keys: &str) {
        self.diagnostics.push(Diagnostic::new(
            UNKNOWN_ATTRIBUTE,
            attribute.key_span,
            format!(
                "`{}` is not an attribute of a `{keyword}` block",
                attribute.key
            ),
            format!("Use {keys}, or remove it"),
        ));
    }

    /// Reports each block in the body of a `keyword` block, which holds none.
    fn unknown_blocks(&mut self, keyword: &str, body: &Document) {
        for block in &body.blocks {
            self.diagnostics.push(Diagnostic::new(
                UNKNOWN_BLOCK,
                block.keyword_span,
                format!(
                    "a `{keyword}` block cannot hold a `{}` block",
                    block.keyword
                ),
                "Remove it".into(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use document::value::{Kind, Value};
    use document::{Map, Position, Source};
    use proptest::prelude::*;
    use spec::node_settings::Policy;
    use types::byte;
    use types::name::Selector;

    use super::*;

    /// The position at `offset`, in a file of lines that are 100 bytes long.
    fn position(offset: u32) -> Position {
        Position {
            offset,
            line: offset / 100,
            column: offset % 100,
        }
    }

    /// A one-byte span at `offset` in file `file`.
    fn at(file: u32, offset: u32) -> Option<Span> {
        Span::new(Source(file), position(offset), position(offset + 1))
    }

    /// A block in file `file` at `offset`: the keyword at `offset`, label `i` at
    /// `offset + 1 + i`, the key of attribute `i` at `offset + 10 + 2i` and its value
    /// one byte later, and the end at `offset + 99`.
    fn block(
        file: u32,
        offset: u32,
        keyword: &str,
        labels: &[&str],
        attributes: &[(&str, Kind)],
    ) -> Block {
        let labels = (offset + 1..).zip(labels).map(|(offset, text)| Label {
            text: (*text).into(),
            span: at(file, offset),
        });
        let attributes = (offset + 10..).step_by(2).zip(attributes);
        let attributes = attributes.map(|(offset, (key, kind))| Attribute {
            key: (*key).into(),
            key_span: at(file, offset),
            value: Value {
                kind: kind.clone(),
                span: at(file, offset + 1),
            },
        });
        Block {
            keyword: keyword.into(),
            keyword_span: at(file, offset),
            labels: labels.collect(),
            body: Document {
                attributes: Map::new(attributes.collect()).unwrap(),
                blocks: Vec::new(),
            },
            span: Span::new(Source(file), position(offset), position(offset + 99)),
        }
    }

    fn document(blocks: Vec<Block>) -> Document {
        Document {
            attributes: Map::default(),
            blocks,
        }
    }

    fn string(text: &str) -> Kind {
        Kind::String(text.into())
    }

    /// A `node_settings` block in file `file` at `offset`, labeled `label`.
    fn settings(
        file: u32,
        offset: u32,
        label: &str,
        attributes: &[(&str, Kind)],
    ) -> Block {
        block(file, offset, "node_settings", &[label], attributes)
    }

    fn refused(
        code: &'static str,
        span: Option<Span>,
        message: &str,
        fix: &str,
    ) -> Diagnostic {
        Diagnostic::new(Code::new(code), span, message.into(), fix.into())
    }

    fn selector(patterns: &[&str]) -> Selector {
        Selector::new(patterns.iter().copied()).unwrap()
    }

    fn gib(count: u64) -> byte::Size {
        byte::Size::from_bytes(count << 30)
    }

    /// The entry of a `node_settings` policy, labeled at `span`.
    fn entry(
        select: &[&str],
        disk: Option<byte::Size>,
        pool: Option<byte::Size>,
        span: Option<Span>,
    ) -> Entry {
        let policy = Policy::new(selector(select), disk, pool).unwrap();
        Entry {
            definition: spec::definition::Definition::NodeSettings(policy),
            span,
        }
    }

    fn key(text: &str) -> Name {
        text.parse().unwrap()
    }

    #[test]
    fn takes_the_key_from_the_label() {
        let block = settings(
            0,
            0,
            "site_a.budget",
            &[
                ("select", string("site_a.*")),
                ("disk", string("200GiB")),
                ("pool", string("8GiB")),
            ],
        );
        assert_eq!(
            check(&[document(vec![block])]),
            Ok(BTreeMap::from([(
                key("site_a.budget.@node_settings"),
                entry(&["site_a.*"], Some(gib(200)), Some(gib(8)), at(0, 1)),
            )]))
        );
    }

    #[test]
    fn reads_one_budget() {
        let select = Kind::List(vec![Value {
            kind: string("site_a.*"),
            span: None,
        }]);
        let attributes = [("select", select), ("pool", string("8GiB"))];
        let block = settings(0, 0, "site_a.budget", &attributes);
        assert_eq!(
            check(&[document(vec![block])]),
            Ok(BTreeMap::from([(
                key("site_a.budget.@node_settings"),
                entry(&["site_a.*"], None, Some(gib(8)), at(0, 1)),
            )]))
        );
    }

    #[test]
    fn reads_policies_from_every_file() {
        let policy = [("select", string("site_a.*")), ("disk", string("1GiB"))];
        let documents = [
            document(vec![
                settings(0, 0, "b", &policy),
                settings(0, 100, "a", &policy),
            ]),
            document(vec![settings(1, 0, "c", &policy)]),
        ];
        let keys: Vec<String> = check(&documents)
            .unwrap()
            .keys()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            keys,
            ["a.@node_settings", "b.@node_settings", "c.@node_settings"]
        );
    }

    #[test]
    fn refuses_a_block_that_is_not_a_kind() {
        let documents = [document(vec![block(0, 0, "nodes", &["a"], &[])])];
        assert_eq!(
            check(&documents),
            Err(vec![refused(
                "config.unknown-block",
                at(0, 0),
                "`nodes` is not a kind of block",
                "Use `node_settings`, or remove the block",
            )])
        );
    }

    #[test]
    fn refuses_an_attribute_outside_a_block() {
        let attribute = Attribute {
            key: "disk".into(),
            key_span: at(0, 3),
            value: Value {
                kind: string("200GiB"),
                span: at(0, 9),
            },
        };
        let documents = [Document {
            attributes: Map::new(vec![attribute]).unwrap(),
            blocks: Vec::new(),
        }];
        assert_eq!(
            check(&documents),
            Err(vec![refused(
                "config.unknown-attribute",
                at(0, 3),
                "`disk` is an attribute outside a block",
                "Move it into the block that it sets, or remove it",
            )])
        );
    }

    #[test]
    fn refuses_a_policy_without_one_label() {
        let policy = [("select", string("site_a.*")), ("disk", string("1GiB"))];
        let fix = "Write one label, such as `node_settings \"site_a.budget\"`";
        let documents = [document(vec![
            block(0, 0, "node_settings", &[], &policy),
            block(0, 100, "node_settings", &["a", "b", "c"], &policy),
        ])];
        assert_eq!(
            check(&documents),
            Err(vec![
                refused(
                    "config.label-count",
                    at(0, 0),
                    "a `node_settings` block has 0 labels, and it needs one, its name",
                    fix,
                ),
                refused(
                    "config.label-count",
                    at(0, 102),
                    "a `node_settings` block has 3 labels, and it needs one, its name",
                    fix,
                ),
            ])
        );
    }

    #[test]
    fn refuses_a_name_that_repeats_in_another_file() {
        let policy = [("select", string("site_a.*")), ("disk", string("1GiB"))];
        let documents = [
            document(vec![settings(0, 0, "site_a.budget", &policy)]),
            document(vec![
                settings(1, 0, "site_a.other", &policy),
                settings(1, 100, "site_a.budget", &policy),
            ]),
        ];
        let mut repeat = refused(
            "config.duplicate-name",
            at(1, 101),
            "the name \"site_a.budget\" repeats the earlier `node_settings` name \
             \"site_a.budget\"",
            "Give each `node_settings` block a name that differs by more than case",
        );
        repeat.notes.push(Note {
            span: at(0, 1).unwrap(),
            text: "the earlier name".into(),
        });
        assert_eq!(check(&documents), Err(vec![repeat]));
    }

    #[test]
    fn refuses_a_name_that_repeats_in_other_case() {
        let policy = [("select", string("site_a.*")), ("disk", string("1GiB"))];
        let documents = [
            document(vec![settings(0, 0, "site_a.budget", &policy)]),
            document(vec![settings(1, 0, "Site_A.budget", &policy)]),
        ];
        let mut repeat = refused(
            "config.duplicate-name",
            at(1, 1),
            "the name \"Site_A.budget\" repeats the earlier `node_settings` name \
             \"site_a.budget\"",
            "Give each `node_settings` block a name that differs by more than case",
        );
        repeat.notes.push(Note {
            span: at(0, 1).unwrap(),
            text: "the earlier name".into(),
        });
        assert_eq!(check(&documents), Err(vec![repeat]));
    }

    #[test]
    fn refuses_a_reserved_name() {
        let policy = [("select", string("site_a.*")), ("disk", string("1GiB"))];
        let documents = [document(vec![settings(0, 0, "site_a.@changes", &policy)])];
        assert_eq!(
            check(&documents),
            Err(vec![refused(
                "config.reserved-name",
                at(0, 1),
                "\"site_a.@changes\" has a segment that starts with `@`, which is \
                 reserved",
                "Remove the `@` from each segment",
            )])
        );
    }

    #[test]
    fn refuses_what_a_node_settings_block_cannot_hold() {
        let mut block = settings(
            0,
            0,
            "a",
            &[("select", string("site_a.*")), ("disks", string("200GiB"))],
        );
        block.body.blocks.push(settings(0, 50, "b", &[]));
        assert_eq!(
            check(&[document(vec![block])]),
            Err(vec![
                refused(
                    "config.unknown-attribute",
                    at(0, 12),
                    "`disks` is not an attribute of a `node_settings` block",
                    "Use `select`, `disk`, or `pool`, or remove it",
                ),
                refused(
                    "config.unknown-block",
                    at(0, 50),
                    "a `node_settings` block cannot hold a `node_settings` block",
                    "Remove it",
                ),
            ])
        );
    }

    #[test]
    fn refuses_a_policy_without_select() {
        let documents = [document(vec![settings(
            0,
            0,
            "a",
            &[("disk", string("1GiB"))],
        )])];
        assert_eq!(
            check(&documents),
            Err(vec![refused(
                "config.missing-attribute",
                at(0, 0),
                "the `node_settings` block has no `select`",
                "Add the nodes that it sets, such as `select = \"site_a.*\"`",
            )])
        );
    }

    #[test]
    fn refuses_a_name_too_long_for_its_key() {
        // `.@node_settings` is 15 bytes, so a 240-byte name makes a 255-byte key.
        let policy = [("select", string("site_a.*")), ("disk", string("1GiB"))];
        let fits = format!("a.{}", "b".repeat(238));
        let long = format!("{fits}c");
        let documents = [document(vec![
            settings(0, 0, &fits, &policy),
            settings(0, 100, &long, &policy),
        ])];
        assert_eq!(
            check(&documents),
            Err(vec![refused(
                "config.long-name",
                at(0, 101),
                &format!(
                    "the name {long:?} is 241 bytes, and a `node_settings` name holds \
                     at most 240"
                ),
                "Shorten the name to at most 240 bytes",
            )])
        );
        let documents = [document(vec![settings(0, 0, &fits, &policy)])];
        let keys = check(&documents).unwrap().into_keys();
        assert_eq!(
            keys.map(|key| key.as_str().len()).collect::<Vec<_>>(),
            [255]
        );
    }

    #[test]
    fn refuses_a_policy_with_no_budget() {
        let documents = [document(vec![settings(
            0,
            0,
            "a",
            &[("select", string("site_a.*"))],
        )])];
        assert_eq!(
            check(&documents),
            Err(vec![refused(
                "config.missing-attribute",
                at(0, 0),
                "the `node_settings` block has no `disk` or `pool`",
                "Add `disk`, `pool`, or both, such as `disk = \"10GiB\"`",
            )])
        );
    }

    #[test]
    fn refuses_a_zero_disk_first() {
        let attributes = [
            ("select", string("site_a.*")),
            ("disk", string("0B")),
            ("pool", string("0GiB")),
        ];
        let documents = [document(vec![settings(0, 0, "a", &attributes)])];
        assert_eq!(
            check(&documents),
            Err(vec![refused(
                "config.zero-size",
                at(0, 13),
                "the `disk` budget is zero",
                "Write a size above zero, or remove `disk`",
            )])
        );
    }

    #[test]
    fn refuses_a_zero_pool() {
        let attributes = [
            ("select", string("site_a.*")),
            ("disk", string("1GiB")),
            ("pool", string("0GiB")),
        ];
        let documents = [document(vec![settings(0, 0, "a", &attributes)])];
        assert_eq!(
            check(&documents),
            Err(vec![refused(
                "config.zero-size",
                at(0, 15),
                "the `pool` budget is zero",
                "Write a size above zero, or remove `pool`",
            )])
        );
    }

    #[test]
    fn gives_only_the_size_diagnostic() {
        let attributes = [("select", string("site_a.*")), ("disk", Kind::Integer(1))];
        let documents = [document(vec![settings(0, 0, "a", &attributes)])];
        assert_eq!(
            check(&documents),
            Err(vec![refused(
                "document.bad-size",
                at(0, 13),
                "a byte size is a string, not an integer",
                "Write a string such as \"200GiB\"",
            )])
        );
    }

    #[test]
    fn gives_the_diagnostic_of_each_reader() {
        let attributes = [
            ("select", string("site_a.*")),
            ("disk", Kind::Integer(200)),
            ("pool", string("8GiB")),
        ];
        let documents = [document(vec![
            settings(0, 0, "site a", &attributes),
            settings(0, 100, "b", &[("select", string("a*b"))]),
        ])];
        assert_eq!(
            check(&documents),
            Err(vec![
                refused(
                    "document.bad-name",
                    at(0, 1),
                    "\"site a\" has a segment that is not valid: \"site a\"",
                    "Use one or more ASCII letters, digits, `_`, and `-` in that \
                     segment, and no other character",
                ),
                refused(
                    "document.bad-size",
                    at(0, 13),
                    "a byte size is a string, not an integer",
                    "Write a string such as \"200GiB\"",
                ),
                refused(
                    "document.bad-selector",
                    at(0, 111),
                    "\"a*b\" uses a wildcard where it cannot",
                    "Use `*` and `**` only as whole segments of a pattern, never in a \
                     name",
                ),
            ])
        );
    }

    #[test]
    fn reports_in_file_then_source_order() {
        // `select` is first in the file but last in the body's map, which sorts keys.
        // The block at 100 is on a later line, at a smaller column.
        let policy = settings(
            0,
            0,
            "a",
            &[("select", Kind::Integer(1)), ("disk", Kind::Integer(1))],
        );
        let documents = [
            document(vec![policy, block(0, 100, "nodes", &[], &[])]),
            document(vec![block(1, 0, "nodes", &[], &[])]),
        ];
        let spans: Vec<Option<Span>> = check(&documents)
            .unwrap_err()
            .iter()
            .map(|diagnostic| diagnostic.span)
            .collect();
        assert_eq!(spans, [at(0, 11), at(0, 13), at(0, 100), at(1, 0)]);
    }

    /// Policies with unique names, each a name and the text of its pattern, disk, and
    /// pool, with at least one budget.
    fn policies()
    -> impl Strategy<Value = Vec<(String, String, Option<u64>, Option<u64>)>> {
        let size = prop::option::of(1..=u64::MAX);
        let names =
            prop::collection::btree_set("[a-z_-]{1,4}(\\.[a-z0-9]{1,3}){0,2}", 0..6);
        names.prop_flat_map(move |names| {
            let policy = (
                "[a-z]{1,3}\\.(\\*|\\*\\*|[a-z]{1,3})",
                size.clone(),
                size.clone(),
            )
                .prop_filter("a policy sets a budget", |(_, disk, pool)| {
                    disk.is_some() || pool.is_some()
                });
            let each = prop::collection::vec(policy, names.len());
            (Just(names), each).prop_map(|(names, each)| {
                let each = names.into_iter().zip(each);
                each.map(|(name, (select, disk, pool))| (name, select, disk, pool))
                    .collect()
            })
        })
    }

    proptest! {
        #[test]
        fn reads_back_each_policy(policies in policies(), files in 1..4u32) {
            let mut documents = vec![document(Vec::new()); files as usize];
            let mut expected = BTreeMap::new();
            for (i, (name, select, disk, pool)) in (0..).zip(&policies) {
                let file = i % files;
                let size = |bytes: Option<u64>| bytes.map(byte::Size::from_bytes);
                let mut attributes = vec![("select", string(select))];
                for (key, bytes) in [("disk", disk), ("pool", pool)] {
                    if let Some(size) = size(*bytes) {
                        attributes.push((key, string(&size.to_string())));
                    }
                }
                let block = settings(file, i * 100, name, &attributes);
                expected.insert(
                    key(&format!("{name}.@node_settings")),
                    entry(&[select], size(*disk), size(*pool), at(file, i * 100 + 1)),
                );
                documents[file as usize].blocks.push(block);
            }
            prop_assert_eq!(check(&documents), Ok(expected));
        }
    }
}
