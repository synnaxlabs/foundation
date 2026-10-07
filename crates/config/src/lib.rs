//! Checks core definitions in Documents, expands templates, hands connector blocks to
//! kinds, and computes plans, explains, and exports.

mod node_settings;
mod placement;
mod retention;

use std::collections::{BTreeMap, btree_map};

use document::diagnostic::{Code, Diagnostic, Note};
use document::value::Value;
use document::{Block, Document, Label, Span, read};
use spec::definition::Definition;
use types::name::{Name, Selector};

const UNKNOWN_BLOCK: Code = Code::new("config.unknown-block");
const UNKNOWN_ATTRIBUTE: Code = Code::new("config.unknown-attribute");
const MISSING_ATTRIBUTE: Code = Code::new("config.missing-attribute");
const LABEL_COUNT: Code = Code::new("config.label-count");
const DUPLICATE_NAME: Code = Code::new("config.duplicate-name");
const RESERVED_NAME: Code = Code::new("config.reserved-name");
const LONG_NAME: Code = Code::new("config.long-name");

/// The check of one kind of block: its definition, or `None` after it reports why the
/// block gives none.
type Check = fn(&mut Found<'_>, &Block) -> Option<Definition>;

/// Each kind of block, by keyword, and its check.
const KINDS: [(&str, Check); 3] = [
    ("node_settings", node_settings::check),
    ("placement", placement::check),
    ("retention", retention::check),
];

/// A checked definition and the label that names it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Entry {
    /// The definition, as the spec tree stores it.
    pub definition: Definition,
    /// Where the label is.
    pub label_span: Option<Span>,
}

/// Checks the definitions in a mesh's Documents, one Document for each file, and
/// gives each by its tree key, `<label>.@<kind>`.
///
/// # Errors
///
/// Every problem in the Documents, in the order of `documents`, then in source order.
/// A problem with no span has no defined place in that order. A value that a reader
/// or a definition refuses gives only its first problem. A definition is checked as a
/// whole (a policy's budgets, for example) only when each of its attributes is known
/// and reads, and the ones it needs are there.
pub fn check(documents: &[Document]) -> Result<BTreeMap<Name, Entry>, Vec<Diagnostic>> {
    let mut found = Found::default();
    for document in documents {
        let start = found.diagnostics.len();
        for attribute in document.attributes.iter() {
            found.diagnostics.push(Diagnostic::new(
                UNKNOWN_ATTRIBUTE,
                attribute.key_span,
                format!("`{}` is an attribute outside a block", attribute.key),
                "Move it into the block that it sets, or remove it".into(),
            ));
        }
        for block in &document.blocks {
            match KINDS
                .iter()
                .find(|(keyword, _)| *keyword == &*block.keyword)
            {
                Some((_, check_block)) => {
                    let key = found.key(block);
                    let definition = check_block(&mut found, block);
                    if let (Some((key, label_span)), Some(definition)) =
                        (key, definition)
                    {
                        let entry = Entry {
                            definition,
                            label_span,
                        };
                        found.entries.insert(key, entry);
                    }
                }
                None => found.diagnostics.push(Diagnostic::new(
                    UNKNOWN_BLOCK,
                    block.keyword_span,
                    format!("`{}` is not a kind of block", block.keyword),
                    format!(
                        "Use {}, or remove the block",
                        one_of(&KINDS.map(|(keyword, _)| keyword))
                    ),
                )),
            }
        }
        found.diagnostics[start..]
            .sort_by_key(|diagnostic| diagnostic.span.map(|span| span.start().offset));
    }
    if found.diagnostics.is_empty() {
        Ok(found.entries)
    } else {
        Err(found.diagnostics)
    }
}

/// `words` in backticks, as a list that ends with "or".
fn one_of(words: &[&str]) -> String {
    match words {
        [] => String::new(),
        [word] => format!("`{word}`"),
        [first, second] => format!("`{first}` or `{second}`"),
        [rest @ .., last] => {
            let rest: Vec<String> =
                rest.iter().map(|word| format!("`{word}`")).collect();
            format!("{}, or `{last}`", rest.join(", "))
        }
    }
}

/// What `check` has found so far.
#[derive(Debug, Default)]
struct Found<'a> {
    entries: BTreeMap<Name, Entry>,
    diagnostics: Vec<Diagnostic>,
    /// The label of each tree key so far, by the key in lowercase, so that keys that
    /// differ only in case collide.
    labels: BTreeMap<Box<str>, &'a Label>,
}

/// A problem that is already in the diagnostics.
#[derive(Debug)]
struct Reported;

impl<'a> Found<'a> {
    /// Reads the one label of a policy block as its name, and gives the tree key,
    /// unique in any case, and the label's span.
    fn key(&mut self, block: &'a Block) -> Option<(Name, Option<Span>)> {
        let keyword = &*block.keyword;
        let label = self.label(block)?;
        let suffix = format!(".@{keyword}");
        let most = Name::MAX_BYTES - suffix.len();
        let bytes = label.text.len();
        if bytes > most {
            self.diagnostics.push(Diagnostic::new(
                LONG_NAME,
                label.span,
                format!(
                    "the name {:?} is {bytes} bytes, and the most for the `{keyword}` \
                     block is {most}",
                    label.text
                ),
                format!("Shorten the name to at most {most} bytes"),
            ));
            return None;
        }
        let name = self.report(read::label(label)).ok()?;
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
        let key: Name = format!("{name}{suffix}")
            .parse()
            .expect("a name and a reserved keyword segment make a name");
        let first = match self.labels.entry(key.as_str().to_ascii_lowercase().into()) {
            btree_map::Entry::Occupied(first) => *first.get(),
            btree_map::Entry::Vacant(entry) => {
                entry.insert(label);
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
        let [label] = block.labels.as_slice() else {
            let at = block
                .labels
                .get(1)
                .map_or(block.keyword_span, |label| label.span);
            self.diagnostics.push(Diagnostic::new(
                LABEL_COUNT,
                at,
                format!(
                    "the `{}` block has {} labels, and it needs one, its name",
                    block.keyword,
                    block.labels.len()
                ),
                "Give the block one label, its name, such as \"site_a.budget\"".into(),
            ));
            return None;
        };
        Some(label)
    }

    /// The value that a reader gives, or `Reported` after it reports the reader's
    /// diagnostic.
    fn report<T>(&mut self, read: Result<T, Diagnostic>) -> Result<T, Reported> {
        read.map_err(|diagnostic| {
            self.diagnostics.push(diagnostic);
            Reported
        })
    }

    /// The attribute `key` of `block` as `read` reads it, or `None` when the block has
    /// no such attribute.
    fn attribute<T>(
        &mut self,
        block: &Block,
        key: &str,
        read: impl FnOnce(&Value) -> Result<T, Diagnostic>,
    ) -> Result<Option<T>, Reported> {
        let attribute = block.body.attributes.get(key);
        attribute
            .map(|attribute| self.report(read(&attribute.value)))
            .transpose()
    }

    /// Reports each attribute of `block` that is not one of `keys`.
    fn unknown_attributes(
        &mut self,
        block: &Block,
        keys: &[&str],
    ) -> Result<(), Reported> {
        let mut result = Ok(());
        for attribute in block.body.attributes.iter() {
            if keys.contains(&&*attribute.key) {
                continue;
            }
            self.diagnostics.push(Diagnostic::new(
                UNKNOWN_ATTRIBUTE,
                attribute.key_span,
                format!(
                    "`{}` is not an attribute of the `{}` block",
                    attribute.key, block.keyword
                ),
                format!("Use {}, or remove it", one_of(keys)),
            ));
            result = Err(Reported);
        }
        result
    }

    /// Reports each block in the body of `block`, which holds none.
    fn unknown_blocks(&mut self, block: &Block) {
        for inner in &block.body.blocks {
            self.diagnostics.push(Diagnostic::new(
                UNKNOWN_BLOCK,
                inner.keyword_span,
                format!(
                    "the `{}` block cannot hold the `{}` block",
                    block.keyword, inner.keyword
                ),
                "Remove it".into(),
            ));
        }
    }

    /// The `select` attribute of a policy block, as [`Found::required`] reads it. The
    /// fix is "Add a `select` attribute with the {selects}, such as "{example}"".
    fn select(
        &mut self,
        block: &Block,
        selects: &str,
        example: &str,
    ) -> Result<Selector, Reported> {
        let fix = format!(
            "Add a `select` attribute with the {selects}, such as \"{example}\""
        );
        self.required(block, "select", read::selector, fix)
    }

    /// The attribute `key` of `block` as `read` reads it. When the block has none, it
    /// reports `config.missing-attribute` with `fix`.
    fn required<T>(
        &mut self,
        block: &Block,
        key: &str,
        read: impl FnOnce(&Value) -> Result<T, Diagnostic>,
        fix: String,
    ) -> Result<T, Reported> {
        if let Some(value) = self.attribute(block, key, read)? {
            return Ok(value);
        }
        self.missing(block, &[key], fix);
        Err(Reported)
    }

    /// Reports that `block` has none of the attributes `keys`.
    fn missing(&mut self, block: &Block, keys: &[&str], fix: String) {
        self.diagnostics.push(Diagnostic::new(
            MISSING_ATTRIBUTE,
            block.keyword_span,
            format!("the `{}` block has no {}", block.keyword, one_of(keys)),
            fix,
        ));
    }
}

#[cfg(test)]
mod tests {
    use document::value::Kind;
    use document::{Attribute, Map, Position, Source};
    use proptest::prelude::*;
    use spec::node_settings::Policy;
    use types::byte;

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
            label_span: span,
        }
    }

    const NO_BUDGET_FIX: &str = "Add a `disk` attribute, a `pool` attribute, or \
                                 both, with a size such as \"10GiB\"";

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
                "Use `node_settings`, `placement`, or `retention`, or remove the block",
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
        let fix = "Give the block one label, its name, such as \"site_a.budget\"";
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
                    "the `node_settings` block has 0 labels, and it needs one, its \
                     name",
                    fix,
                ),
                refused(
                    "config.label-count",
                    at(0, 102),
                    "the `node_settings` block has 3 labels, and it needs one, its \
                     name",
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
                    "`disks` is not an attribute of the `node_settings` block",
                    "Use `select`, `disk`, or `pool`, or remove it",
                ),
                refused(
                    "config.unknown-block",
                    at(0, 50),
                    "the `node_settings` block cannot hold the `node_settings` block",
                    "Remove it",
                ),
            ])
        );
    }

    #[test]
    fn refuses_a_policy_without_select_before_its_budgets() {
        let documents = [document(vec![
            settings(0, 0, "a", &[("disk", string("1GiB"))]),
            settings(0, 100, "b", &[("disk", string("0B"))]),
            settings(0, 200, "c", &[]),
        ])];
        let missing = |offset| {
            refused(
                "config.missing-attribute",
                at(0, offset),
                "the `node_settings` block has no `select`",
                "Add a `select` attribute with the nodes that it sets, such as \
                 \"site_a.*\"",
            )
        };
        assert_eq!(
            check(&documents),
            Err(vec![missing(0), missing(100), missing(200)])
        );
    }

    #[test]
    fn checks_the_budgets_of_a_policy_that_holds_a_block() {
        let mut policy = settings(0, 0, "a", &[("select", string("site_a.*"))]);
        policy.body.blocks.push(block(0, 50, "inner", &[], &[]));
        assert_eq!(
            check(&[document(vec![policy])]),
            Err(vec![
                refused(
                    "config.missing-attribute",
                    at(0, 0),
                    "the `node_settings` block has no `disk` or `pool`",
                    NO_BUDGET_FIX,
                ),
                refused(
                    "config.unknown-block",
                    at(0, 50),
                    "the `node_settings` block cannot hold the `inner` block",
                    "Remove it",
                ),
            ])
        );
    }

    #[test]
    fn refuses_a_name_too_long_for_its_key() {
        // `.@node_settings` is 15 bytes, so a 240-byte name makes a 255-byte key.
        let policy = [("select", string("site_a.*")), ("disk", string("1GiB"))];
        let fits = format!("a.{}", "b".repeat(238));
        let long = format!("{fits}c");
        // Past the 255 bytes of any name, the key limit still names the problem.
        let longest = format!("{fits}{}", "c".repeat(16));
        let documents = [document(vec![
            settings(0, 0, &fits, &policy),
            settings(0, 100, &long, &policy),
            settings(0, 200, &longest, &policy),
        ])];
        assert_eq!(
            check(&documents),
            Err(vec![
                refused(
                    "config.long-name",
                    at(0, 101),
                    &format!(
                        "the name {long:?} is 241 bytes, and the most for the \
                         `node_settings` block is 240"
                    ),
                    "Shorten the name to at most 240 bytes",
                ),
                refused(
                    "config.long-name",
                    at(0, 201),
                    &format!(
                        "the name {longest:?} is 256 bytes, and the most for the \
                         `node_settings` block is 240"
                    ),
                    "Shorten the name to at most 240 bytes",
                ),
            ])
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
                NO_BUDGET_FIX,
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
                    "a segment is not valid: \"site a\" in \"site a\"",
                    "Use one or more ASCII letters, digits, `_`, and `-` in that \
                     segment, after an optional leading `@`",
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
                    "a wildcard is out of place: \"a*b\"",
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

    mod placements {
        use spec::placement::{Nodes, Policy};

        use super::*;

        const NAME_FIX: &str = "Write a name such as \"site_a.node_1\"";

        fn name(text: &str) -> Name {
            text.parse().unwrap()
        }

        fn nodes(home: Option<&str>, standby: Option<&str>, copies: &[&str]) -> Nodes {
            Nodes {
                home: home.map(name),
                standby: standby.map(name),
                copies: copies.iter().map(|copy| name(copy)).collect(),
            }
        }

        fn reference(text: &str) -> Kind {
            Kind::Reference(name(text))
        }

        /// A list of names, each at its own one-byte span from `offset` in file 0.
        fn list(offset: u32, items: &[Kind]) -> Kind {
            let items = (offset..).zip(items).map(|(offset, kind)| Value {
                kind: kind.clone(),
                span: at(0, offset),
            });
            Kind::List(items.collect())
        }

        /// A `placement` block in file 0 at offset 0, labeled `edge`.
        fn placement(attributes: &[(&str, Kind)]) -> [Document; 1] {
            [document(vec![block(
                0,
                0,
                "placement",
                &["edge"],
                attributes,
            )])]
        }

        /// The one entry of a placement labeled `edge` that selects `edge.*`.
        fn placed(nodes: Nodes) -> BTreeMap<Name, Entry> {
            let policy = Policy::new(selector(&["edge.*"]), nodes).unwrap();
            let entry = Entry {
                definition: spec::definition::Definition::Placement(policy),
                label_span: at(0, 1),
            };
            BTreeMap::from([(key("edge.@placement"), entry)])
        }

        #[test]
        fn reads_a_home() {
            let documents =
                placement(&[("select", string("edge.*")), ("home", string("edge"))]);
            assert_eq!(
                check(&documents),
                Ok(placed(nodes(Some("edge"), None, &[])))
            );
        }

        #[test]
        fn reads_each_role_alone_from_a_string_or_a_reference() {
            for node in [string("n_1"), reference("n_1")] {
                let cases = [
                    ("home", nodes(Some("n_1"), None, &[])),
                    ("standby", nodes(None, Some("n_1"), &[])),
                    ("copies", nodes(None, None, &["n_1"])),
                ];
                for (role, nodes) in cases {
                    let documents = placement(&[
                        ("select", string("edge.*")),
                        (role, node.clone()),
                    ]);
                    assert_eq!(check(&documents), Ok(placed(nodes)), "{role}");
                }
            }
        }

        #[test]
        fn reads_every_role() {
            let copies = list(50, &[string("n_3"), reference("n_4")]);
            let documents = placement(&[
                ("select", string("edge.*")),
                ("home", string("n_1")),
                ("standby", reference("n_2")),
                ("copies", copies),
            ]);
            let nodes = nodes(Some("n_1"), Some("n_2"), &["n_3", "n_4"]);
            assert_eq!(check(&documents), Ok(placed(nodes)));
        }

        #[test]
        fn refuses_a_placement_without_select() {
            let documents = placement(&[("home", string("edge"))]);
            assert_eq!(
                check(&documents),
                Err(vec![refused(
                    "config.missing-attribute",
                    at(0, 0),
                    "the `placement` block has no `select`",
                    "Add a `select` attribute with the connectors and indexes that it \
                     places, such as \"site_a.*\"",
                )])
            );
        }

        #[test]
        fn refuses_a_placement_that_names_no_node() {
            let empty = list(50, &[]);
            let cases = [
                (vec![("select", string("edge.*"))], 0),
                (vec![("select", string("edge.*")), ("copies", empty)], 13),
            ];
            for (attributes, offset) in cases {
                assert_eq!(
                    check(&placement(&attributes)),
                    Err(vec![refused(
                        "config.empty-placement",
                        at(0, offset),
                        "the `placement` block names no home, no standby, and no copy",
                        "Name a `home`, a `standby`, or a node in `copies`",
                    )]),
                    "{attributes:?}"
                );
            }
        }

        #[test]
        fn reads_no_copies_next_to_a_home() {
            let documents = placement(&[
                ("select", string("edge.*")),
                ("home", string("edge")),
                ("copies", list(50, &[])),
            ]);
            assert_eq!(
                check(&documents),
                Ok(placed(nodes(Some("edge"), None, &[])))
            );
        }

        /// The `role-overlap` diagnostic for `node` at `span`.
        fn overlap(node: &str, span: Option<Span>) -> Vec<Diagnostic> {
            vec![refused(
                "config.role-overlap",
                span,
                &format!("node {node} has more than one role in the placement"),
                &format!("Keep {node} in one of `home`, `standby`, and `copies`"),
            )]
        }

        #[test]
        fn refuses_a_node_with_two_roles_at_its_last_value() {
            let copies = || list(50, &[string("n_2"), string("n_1")]);
            // After `select`, the value of role `i` is at offset `13 + 2i`.
            let cases = [
                (
                    vec![("home", string("n_1")), ("standby", reference("n_1"))],
                    15,
                ),
                (
                    vec![("standby", string("n_1")), ("home", string("n_1"))],
                    15,
                ),
                (vec![("home", string("n_1")), ("copies", copies())], 15),
                (vec![("copies", copies()), ("home", string("n_1"))], 15),
                (vec![("standby", string("n_1")), ("copies", copies())], 15),
                (vec![("copies", copies()), ("standby", string("n_1"))], 15),
                (
                    vec![("copies", string("n_1")), ("standby", string("n_1"))],
                    15,
                ),
                (
                    vec![
                        ("home", string("n_1")),
                        ("copies", copies()),
                        ("standby", string("n_1")),
                    ],
                    17,
                ),
            ];
            for (roles, offset) in cases {
                let mut attributes = vec![("select", string("edge.*"))];
                attributes.extend(roles);
                assert_eq!(
                    check(&placement(&attributes)),
                    Err(overlap("n_1", at(0, offset))),
                    "{attributes:?}"
                );
            }
        }

        #[test]
        fn skips_a_later_value_that_does_not_name_the_node() {
            let attributes = [
                ("select", string("edge.*")),
                ("home", string("n_1")),
                ("standby", string("n_1")),
                ("copies", list(50, &[string("n_3")])),
            ];
            assert_eq!(
                check(&placement(&attributes)),
                Err(overlap("n_1", at(0, 15)))
            );
        }

        #[test]
        fn skips_a_later_home_or_standby_that_does_not_name_the_node() {
            for (role, other) in [("standby", "home"), ("home", "standby")] {
                let attributes = [
                    ("select", string("edge.*")),
                    (role, string("n_1")),
                    ("copies", list(50, &[string("n_1")])),
                    (other, string("n_9")),
                ];
                assert_eq!(
                    check(&placement(&attributes)),
                    Err(overlap("n_1", at(0, 15))),
                    "{attributes:?}"
                );
            }
        }

        #[test]
        fn names_the_home_when_the_home_has_two_roles() {
            let attributes = [
                ("select", string("edge.*")),
                ("home", string("n_1")),
                ("standby", string("n_2")),
                ("copies", list(50, &[string("n_2"), string("n_1")])),
            ];
            assert_eq!(
                check(&placement(&attributes)),
                Err(overlap("n_1", at(0, 17)))
            );
        }

        #[test]
        fn refuses_a_node_value_that_is_not_a_name() {
            let refused_name = |span, message: &str| {
                Err(vec![refused("document.bad-name", span, message, NAME_FIX)])
            };
            let cases = [
                (
                    ("home", Kind::Integer(7)),
                    refused_name(
                        at(0, 13),
                        "a name is a string or a reference, not an integer",
                    ),
                ),
                (
                    ("standby", Kind::Bool(true)),
                    refused_name(
                        at(0, 13),
                        "a name is a string or a reference, not a bool",
                    ),
                ),
                (
                    ("copies", list(50, &[string("n_1"), Kind::Integer(7)])),
                    refused_name(
                        at(0, 51),
                        "a name is a string or a reference, not an integer",
                    ),
                ),
            ];
            for (role, expected) in cases {
                let attributes = [("select", string("edge.*")), role];
                assert_eq!(check(&placement(&attributes)), expected, "{attributes:?}");
            }
        }

        #[test]
        fn refuses_a_name_that_name_refuses() {
            let attributes = [("select", string("edge.*")), ("home", string("a..b"))];
            let error = "a..b".parse::<Name>().unwrap_err();
            assert_eq!(
                check(&placement(&attributes)),
                Err(vec![refused(
                    "document.bad-name",
                    at(0, 13),
                    &error.to_string(),
                    error.fix(),
                )])
            );
        }

        #[test]
        fn refuses_only_the_unknown_attribute_of_a_placement_with_no_node() {
            let attributes = [("select", string("edge.*")), ("node", string("edge"))];
            assert_eq!(
                check(&placement(&attributes)),
                Err(vec![refused(
                    "config.unknown-attribute",
                    at(0, 12),
                    "`node` is not an attribute of the `placement` block",
                    "Use `select`, `home`, `standby`, or `copies`, or remove it",
                )])
            );
        }

        #[test]
        fn refuses_a_block_inside_a_placement() {
            let [mut documents] =
                placement(&[("select", string("edge.*")), ("home", string("edge"))]);
            documents.blocks[0]
                .body
                .blocks
                .push(block(0, 50, "inner", &[], &[]));
            assert_eq!(
                check(&[documents]),
                Err(vec![refused(
                    "config.unknown-block",
                    at(0, 50),
                    "the `placement` block cannot hold the `inner` block",
                    "Remove it",
                )])
            );
        }

        #[test]
        fn refuses_an_attribute_that_a_placement_does_not_have() {
            let attributes = [
                ("select", string("edge.*")),
                ("home", string("edge")),
                ("node", string("edge")),
            ];
            assert_eq!(
                check(&placement(&attributes)),
                Err(vec![refused(
                    "config.unknown-attribute",
                    at(0, 14),
                    "`node` is not an attribute of the `placement` block",
                    "Use `select`, `home`, `standby`, or `copies`, or remove it",
                )])
            );
        }
    }

    mod retentions {
        use spec::retention::Policy;
        use types::time;

        use super::*;

        /// A `retention` block in file 0 at offset 0, labeled `edge`.
        fn retention(attributes: &[(&str, Kind)]) -> [Document; 1] {
            [document(vec![block(
                0,
                0,
                "retention",
                &["edge"],
                attributes,
            )])]
        }

        /// The one entry of a retention labeled `edge` that selects `edge.**`.
        fn kept(keep: time::Span) -> BTreeMap<Name, Entry> {
            let policy = Policy::new(selector(&["edge.**"]), keep).unwrap();
            let entry = Entry {
                definition: Definition::Retention(policy),
                label_span: at(0, 1),
            };
            BTreeMap::from([(key("edge.@retention"), entry)])
        }

        #[test]
        fn reads_a_retention() {
            let three_days = time::Span::from_nanos(3 * time::Span::DAY.nanos());
            for (text, keep) in [("3d", three_days), ("0s", time::Span::ZERO)] {
                let documents =
                    retention(&[("select", string("edge.**")), ("keep", string(text))]);
                assert_eq!(check(&documents), Ok(kept(keep)), "{text:?}");
            }
        }

        #[test]
        fn refuses_a_retention_without_select_or_keep() {
            let select = refused(
                "config.missing-attribute",
                at(0, 0),
                "the `retention` block has no `select`",
                "Add a `select` attribute with the indexes that it caps, such as \
                 \"site_a.**\"",
            );
            let keep = refused(
                "config.missing-attribute",
                at(0, 0),
                "the `retention` block has no `keep`",
                "Add a `keep` attribute with a span such as \"3d\"",
            );
            let cases = [
                (vec![("keep", string("3d"))], vec![select.clone()]),
                (vec![("select", string("edge.**"))], vec![keep.clone()]),
                (vec![], vec![select, keep]),
            ];
            for (attributes, diagnostics) in cases {
                assert_eq!(
                    check(&retention(&attributes)),
                    Err(diagnostics),
                    "{attributes:?}"
                );
            }
        }

        #[test]
        fn refuses_a_negative_keep_at_its_value() {
            let documents =
                retention(&[("select", string("edge.**")), ("keep", string("-1s"))]);
            assert_eq!(
                check(&documents),
                Err(vec![refused(
                    "config.negative-span",
                    at(0, 13),
                    "a retention keeps -1s, which is below zero",
                    "Write a keep time of zero or more",
                )])
            );
        }

        #[test]
        fn refuses_a_keep_that_is_not_a_span() {
            let documents =
                retention(&[("select", string("edge.**")), ("keep", string("3 days"))]);
            assert_eq!(
                check(&documents),
                Err(vec![refused(
                    "document.bad-span",
                    at(0, 13),
                    "cannot read the span \"3 days\": a span is not a number and a \
                     unit",
                    "Write a span such as \"250us\", \"1.5s\", or \"3d\"",
                )])
            );
        }

        #[test]
        fn refuses_an_attribute_that_a_retention_does_not_have() {
            let documents = retention(&[
                ("select", string("edge.**")),
                ("keep", string("3d")),
                ("hold", string("1d")),
            ]);
            assert_eq!(
                check(&documents),
                Err(vec![refused(
                    "config.unknown-attribute",
                    at(0, 14),
                    "`hold` is not an attribute of the `retention` block",
                    "Use `select` or `keep`, or remove it",
                )])
            );
        }

        #[test]
        fn refuses_only_the_unknown_attribute_of_a_retention_with_a_negative_keep() {
            let documents = retention(&[
                ("select", string("edge.**")),
                ("keep", string("-1s")),
                ("hold", string("1d")),
            ]);
            assert_eq!(
                check(&documents),
                Err(vec![refused(
                    "config.unknown-attribute",
                    at(0, 14),
                    "`hold` is not an attribute of the `retention` block",
                    "Use `select` or `keep`, or remove it",
                )])
            );
        }

        #[test]
        fn refuses_a_block_inside_a_retention() {
            let [mut documents] =
                retention(&[("select", string("edge.**")), ("keep", string("3d"))]);
            documents.blocks[0]
                .body
                .blocks
                .push(block(0, 50, "inner", &[], &[]));
            assert_eq!(
                check(&[documents]),
                Err(vec![refused(
                    "config.unknown-block",
                    at(0, 50),
                    "the `retention` block cannot hold the `inner` block",
                    "Remove it",
                )])
            );
        }
    }
}
