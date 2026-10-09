//! Checks core definitions in Documents, expands templates, hands connector blocks to
//! kinds, and computes plans, explains, and exports.

mod access;
mod channel;
mod connector;
mod duplicate;
mod node_settings;
pub mod openssh;
mod placement;
pub mod plan;
mod private_key;
mod retention;
mod subject;

use std::collections::{BTreeMap, BTreeSet};

use ::connector::kind::Table;
use document::diagnostic::{Code, Diagnostic};
use document::value::Value;
use document::{Block, Document, Label, Span, read};
use spec::definition::Kind;
use spec::key;
use types::name::{Name, Selector};

const RESERVED_NAME: Code = Code::new("config.reserved-name");
const LONG_NAME: Code = Code::new("config.long-name");

/// The check of one kind of block, with its tree key when its label gives one: its
/// definition, or `None` after it reports why the block gives none.
type Check = fn(&mut Found<'_>, &Block, Option<&Name>) -> Option<Definition>;

/// Each kind of block, whose name is its keyword, and its check.
const KINDS: [(Kind, Check); 7] = [
    (Kind::Access, access::check),
    (Kind::Channel, channel::check),
    (Kind::Connector, connector::check),
    (Kind::NodeSettings, node_settings::check),
    (Kind::Placement, placement::check),
    (Kind::Retention, retention::check),
    (Kind::Subject, subject::check),
];

/// What one block defines. A channel's edges are names until `plan` gives each
/// channel its key.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Definition {
    /// A definition other than a channel, as the spec tree stores it.
    Spec(spec::definition::Definition),
    /// A channel, with each edge as the name of the channel that it points at.
    Channel(spec::channel::Kind<Name>),
}

/// A checked definition and the label that names it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Entry {
    /// The definition.
    pub definition: Definition,
    /// Where the label is.
    pub label_span: Option<Span>,
}

/// Checks the definitions in a mesh's Documents, one Document for each file, and
/// gives each by its tree key: the name of a channel or a connector, or
/// `<label>.@<kind>` for each other block. The kind in `kinds` that a `connector`
/// block names checks its config. Each order of `documents` gives the same entries,
/// or each gives problems.
///
/// # Errors
///
/// A private key anywhere in the Documents gives only `config.private-key`, once for
/// each string that holds one.
/// Each other problem in the Documents, in the order of their [`document::Source`],
/// then in source order.
/// A problem with no span has no defined place in that order. A value that a reader
/// or a definition refuses gives only its first problem. A definition is checked as a
/// whole (a policy's budgets, for example) only when each of its attributes is known
/// and reads, and the ones it needs are there. A block directly inside a definition
/// other than a `connector` does not stop that check: only a connector's kind reads
/// blocks, so each such block is one problem, and nothing inside it but a private key
/// is checked.
/// A bad `kind` of channel hides the problems of each other attribute that a kind of
/// channel knows. A `kind` of connector that is missing, is not a name, or is not in
/// `kinds` hides each problem of the connector's config, and so does a config nested
/// deeper than `document::encoding::Checked` takes.
pub fn check(
    documents: &[Document],
    kinds: &Table,
) -> Result<BTreeMap<Name, Entry>, Vec<Diagnostic>> {
    checked(documents, kinds).map(|found| found.entries)
}

/// What [`check`] finds in `documents`, with the block of each entry and the writes of
/// each connector, or the problems that `check` gives.
fn checked<'a>(
    documents: &'a [Document],
    kinds: &'a Table,
) -> Result<Found<'a>, Vec<Diagnostic>> {
    let alarms = private_key::alarms(documents);
    if !alarms.is_empty() {
        return Err(alarms);
    }
    let mut found = Found {
        entries: BTreeMap::new(),
        diagnostics: Vec::new(),
        labels: BTreeMap::new(),
        channels: names(documents, Kind::Channel)
            .map(|(name, _)| name)
            .collect(),
        connectors: BTreeMap::new(),
        kinds,
        blocks: BTreeMap::new(),
        writes: BTreeMap::new(),
    };
    let mut connectors: Vec<_> = names(documents, Kind::Connector).collect();
    connectors.sort_by_key(|(_, label)| order(label.span));
    for (name, label) in connectors {
        let lower = name.as_str().to_ascii_lowercase().into();
        found.connectors.entry(lower).or_insert(label);
    }
    let keywords = KINDS.map(|(kind, _)| kind.as_str());
    for document in documents {
        found
            .diagnostics
            .extend(read::unknown(document, "a file", &[], &keywords));
        for block in &document.blocks {
            let Some((kind, check_block)) = KINDS
                .iter()
                .find(|(kind, _)| kind.as_str() == &*block.keyword)
            else {
                // `read::unknown` reported it.
                continue;
            };
            let key = found.key(block, *kind);
            let name = key.as_ref().map(|(name, _)| name);
            let definition = check_block(&mut found, block, name);
            if let (Some((key, label_span)), Some(definition)) = (key, definition) {
                let entry = Entry {
                    definition,
                    label_span,
                };
                found.blocks.insert(key.clone(), block);
                found.entries.insert(key, entry);
            }
        }
    }
    let repeats = duplicate::in_labels(&mut found.labels);
    found.diagnostics.extend(repeats);
    if found.diagnostics.is_empty() {
        Ok(found)
    } else {
        sort(&mut found.diagnostics);
        Err(found.diagnostics)
    }
}

/// The label of the definition of `kind` at tree key `key`, or `key` when it has no
/// label form.
pub(crate) fn label(kind: Kind, key: &Name) -> Name {
    kind.label(key).unwrap_or_else(|| key.clone())
}

/// Sorts `diagnostics` by the [`order`] of each span.
fn sort(diagnostics: &mut [Diagnostic]) {
    diagnostics.sort_by_key(|diagnostic| order(diagnostic.span));
}

/// The key that orders `span` by its [`document::Source`], then in source order. No
/// span comes first.
fn order(span: Option<Span>) -> Option<(document::Source, u32)> {
    span.map(|span| (span.source(), span.start().offset))
}

/// The name and the label of each block of `kind` in `documents` whose one label
/// reads as a name. `check` reports each other label.
fn names(documents: &[Document], kind: Kind) -> impl Iterator<Item = (Name, &Label)> {
    let blocks = documents.iter().flat_map(|document| &document.blocks);
    blocks
        .filter(move |block| &*block.keyword == kind.as_str())
        .filter_map(|block| match block.labels.as_slice() {
            [label] => Some((read::label(label).ok()?, label)),
            _ => None,
        })
}

/// The channel and connector names of the Documents, the connector kinds, and what
/// `check` has found so far.
#[derive(Debug)]
struct Found<'a> {
    entries: BTreeMap<Name, Entry>,
    diagnostics: Vec<Diagnostic>,
    /// Each label of each tree key so far.
    labels: duplicate::Labels<'a>,
    /// The name of each channel that a `channel` block in any Document defines.
    channels: BTreeSet<Name>,
    /// The label of the first connector in [`order`] that a `connector` block in any
    /// Document defines at each name, by the name in lowercase.
    connectors: BTreeMap<Box<str>, &'a Label>,
    /// The kinds that check each `connector` block's config.
    kinds: &'a Table,
    /// The block of each entry, by tree key.
    blocks: BTreeMap<Name, &'a Block>,
    /// The channels that each connector writes to the mesh, as its kind checks them, by
    /// tree key.
    writes: BTreeMap<Name, Vec<Name>>,
}

/// A problem that is already in the diagnostics.
#[derive(Debug)]
struct Reported;

impl<'a> Found<'a> {
    /// Reads the one label of a block of `kind` as its name, and gives the tree key and
    /// the label's span. [`Found::repeats`] reports a key that repeats.
    fn key(&mut self, block: &'a Block, kind: Kind) -> Option<(Name, Option<Span>)> {
        let keyword = kind.as_str();
        let fix = "Give the block one label, its name, such as \"site_a.budget\"";
        let [label] = self.report(read::labels::<1>(block, fix.into())).ok()?;
        let key = match kind.key(&label.text) {
            Ok(key) => key,
            Err(key::Error::Long { most }) => {
                self.diagnostics.push(Diagnostic::new(
                    LONG_NAME,
                    label.span,
                    format!(
                        "the name {:?} is {} bytes, and the most for the `{keyword}` \
                         block is {most}",
                        label.text,
                        label.text.len()
                    ),
                    format!("Shorten the name to at most {most} bytes"),
                ));
                return None;
            }
            Err(key::Error::Name(_)) => {
                self.report(read::label(label)).ok()?;
                unreachable!("`read::label` refuses each label that `Kind::key` does");
            }
            Err(error @ key::Error::Reserved) => {
                self.diagnostics.push(Diagnostic::new(
                    RESERVED_NAME,
                    label.span,
                    format!(
                        "{:?} has a segment that starts with `@`, which is reserved",
                        label.text
                    ),
                    error.fix().into(),
                ));
                return None;
            }
        };
        duplicate::add(&mut self.labels, &key, label, kind);
        Some((key, label.span))
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

    /// Reports each attribute of `block` that is not one of `keys`, and each block in
    /// its body, since a block that `config` checks holds none.
    ///
    /// # Errors
    ///
    /// `Reported` when an attribute is unknown. A block inside does not stop the
    /// check of the definition.
    fn unknown(&mut self, block: &Block, keys: &[&str]) -> Result<(), Reported> {
        let found = read::unknown(&block.body, &of(block), keys, &[]);
        // Each block inside gives one diagnostic, so any more are attributes.
        let attributes = found.len() > block.body.blocks.len();
        self.diagnostics.extend(found);
        if attributes { Err(Reported) } else { Ok(()) }
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
    /// reports `document.missing-attribute` with `fix`.
    fn required<T>(
        &mut self,
        block: &Block,
        key: &str,
        read: impl FnOnce(&Value) -> Result<T, Diagnostic>,
        fix: String,
    ) -> Result<T, Reported> {
        let at = block.keyword_span;
        self.report(read::required(&block.body, &of(block), at, key, read, fix))
    }

    /// Reports that `block` has none of the attributes `keys`.
    fn missing(&mut self, block: &Block, keys: &[&str], fix: String) {
        let missing = read::missing(&of(block), block.keyword_span, keys, fix);
        self.diagnostics.push(missing);
    }
}

/// The name of `block` in a message: "the `retention` block".
fn of(block: &Block) -> String {
    format!("the `{}` block", block.keyword)
}

/// The span of the value of `key` in `block`.
fn span(block: &Block, key: &str) -> Option<Span> {
    block.body.attributes.get(key)?.value.span
}

#[cfg(test)]
mod tests {
    use document::diagnostic::Note;
    use document::value::Kind;
    use document::{Attribute, Map, Position, Source};
    use proptest::prelude::*;
    use spec::definition;
    use spec::node_settings::Policy;
    use types::byte;

    use super::*;

    /// Checks `documents` with no connector kinds.
    fn check(documents: &[Document]) -> Result<BTreeMap<Name, Entry>, Vec<Diagnostic>> {
        super::check(documents, &Table::new())
    }

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

    /// Asserts that each of two blocks inside a `keyword` block with `attributes` adds
    /// one `document.unknown-block` diagnostic to what `check` gives without them, and
    /// that an attribute or a block inside each adds none.
    fn assert_inner_blocks_refused(keyword: &str, attributes: &[(&str, Kind)]) {
        let mut policy = block(0, 0, keyword, &["edge"], attributes);
        let mut expected = check(&[document(vec![policy.clone()])])
            .err()
            .unwrap_or_default();
        for (offset, inner) in [(90, "inner"), (95, "other")] {
            let mut inner_block =
                block(0, offset, inner, &[], &[("size", string("1"))]);
            let deeper = block(0, offset + 1, "deeper", &[], &[]);
            inner_block.body.blocks.push(deeper);
            policy.body.blocks.push(inner_block);
            expected.push(refused(
                "document.unknown-block",
                at(0, offset),
                &format!("the `{keyword}` block cannot hold the `{inner}` block"),
                "Remove it",
            ));
        }
        assert_eq!(
            check(&[document(vec![policy])]),
            Err(expected),
            "{attributes:?}"
        );
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
            definition: Definition::Spec(definition::Definition::NodeSettings(policy)),
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
                "document.unknown-block",
                at(0, 0),
                "a file cannot hold the `nodes` block",
                "Use `access`, `channel`, `connector`, `node_settings`, `placement`, \
                 `retention`, or `subject`, or remove it",
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
                "document.unknown-attribute",
                at(0, 3),
                "`disk` is not an attribute of a file",
                "Move it into the `access`, `channel`, `connector`, `node_settings`, \
                 `placement`, `retention`, or `subject` block that it sets, or remove \
                 it",
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
                    "document.label-count",
                    at(0, 0),
                    "the `node_settings` block has no labels, and it takes 1 label",
                    fix,
                ),
                refused(
                    "document.label-count",
                    at(0, 102),
                    "the `node_settings` block has 3 labels, and it takes 1 label",
                    fix,
                ),
            ])
        );
    }

    #[test]
    fn refuses_a_name_that_repeats_in_another_file() {
        let policy = [("select", string("site_a.*")), ("disk", string("1GiB"))];
        let mut documents = [
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
        for _ in 0..2 {
            assert_eq!(check(&documents), Err(vec![repeat.clone()]));
            documents.reverse();
        }
    }

    #[test]
    fn refuses_each_later_name_at_the_first_in_any_order_of_the_files() {
        let policy = [("select", string("site_a.*")), ("disk", string("1GiB"))];
        let mut documents = [
            document(vec![settings(0, 0, "site_a.budget", &policy)]),
            document(vec![settings(1, 0, "Site_A.budget", &policy)]),
            document(vec![settings(2, 0, "SITE_A.budget", &policy)]),
        ];
        let repeats = [(1, "Site_A.budget"), (2, "SITE_A.budget")].map(|(file, text)| {
            let mut repeat = refused(
                "config.duplicate-name",
                at(file, 1),
                &format!(
                    "the name {text:?} repeats the earlier `node_settings` name \
                     \"site_a.budget\""
                ),
                "Give each `node_settings` block a name that differs by more than case",
            );
            repeat.notes.push(Note {
                span: at(0, 1).unwrap(),
                text: "the earlier name".into(),
            });
            repeat
        });
        for _ in 0..2 {
            assert_eq!(check(&documents), Err(repeats.to_vec()));
            documents.reverse();
        }
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
    fn refuses_the_later_name_in_the_documents_when_neither_has_a_span() {
        let policy = [("select", string("site_a.*")), ("disk", string("1GiB"))];
        let mut documents = [
            document(vec![settings(0, 0, "site_a.budget", &policy)]),
            document(vec![settings(1, 0, "Site_A.budget", &policy)]),
        ];
        for document in &mut documents {
            document.blocks[0].labels[0].span = None;
        }
        let repeat = refused(
            "config.duplicate-name",
            None,
            "the name \"Site_A.budget\" repeats the earlier `node_settings` name \
             \"site_a.budget\"",
            "Give each `node_settings` block a name that differs by more than case",
        );
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
                    "document.unknown-attribute",
                    at(0, 12),
                    "`disks` is not an attribute of the `node_settings` block",
                    "Use `select`, `disk`, or `pool`, or remove it",
                ),
                refused(
                    "document.unknown-block",
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
                "document.missing-attribute",
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
    fn refuses_a_block_inside_each_kind_but_a_connector() {
        for (kind, _) in KINDS {
            if kind != definition::Kind::Connector {
                assert_inner_blocks_refused(kind.as_str(), &[]);
            }
        }
    }

    #[test]
    fn refuses_a_block_inside_node_settings_with_any_attributes() {
        let select = || ("select", string("site_a.*"));
        let cases = [
            vec![select(), ("disk", string("1GiB"))],
            vec![],
            vec![("disk", string("1GiB"))],
            vec![("select", Kind::Integer(7)), ("disk", string("1GiB"))],
            vec![select()],
            vec![select(), ("disk", Kind::Integer(7))],
            vec![select(), ("disk", string("nope"))],
            vec![select(), ("pool", Kind::Integer(7))],
            vec![select(), ("disk", string("0B"))],
            vec![select(), ("disk", string("1GiB")), ("size", string("1GiB"))],
        ];
        for attributes in cases {
            assert_inner_blocks_refused("node_settings", &attributes);
        }
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
                "document.missing-attribute",
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
        let mut documents = [
            document(vec![policy, block(0, 100, "nodes", &[], &[])]),
            document(vec![block(1, 0, "nodes", &[], &[])]),
        ];
        for _ in 0..2 {
            let spans: Vec<Option<Span>> = check(&documents)
                .unwrap_err()
                .iter()
                .map(|diagnostic| diagnostic.span)
                .collect();
            assert_eq!(spans, [at(0, 11), at(0, 13), at(0, 100), at(1, 0)]);
            documents.reverse();
        }
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
                definition: Definition::Spec(definition::Definition::Placement(policy)),
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
                    "document.missing-attribute",
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
                    "document.unknown-attribute",
                    at(0, 12),
                    "`node` is not an attribute of the `placement` block",
                    "Use `select`, `home`, `standby`, or `copies`, or remove it",
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
                    "document.unknown-attribute",
                    at(0, 14),
                    "`node` is not an attribute of the `placement` block",
                    "Use `select`, `home`, `standby`, or `copies`, or remove it",
                )])
            );
        }

        #[test]
        fn refuses_a_block_inside_a_placement_with_any_attributes() {
            let select = || ("select", string("edge.*"));
            let cases = [
                vec![select(), ("home", string("edge"))],
                vec![],
                vec![("home", string("edge"))],
                vec![("select", Kind::Integer(7)), ("home", string("edge"))],
                vec![select(), ("home", Kind::Integer(7))],
                vec![select(), ("standby", Kind::Bool(true))],
                vec![select(), ("copies", list(50, &[Kind::Integer(7)]))],
                vec![select()],
                vec![
                    select(),
                    ("home", string("n_1")),
                    ("standby", string("n_1")),
                ],
                vec![select(), ("home", string("edge")), ("node", string("edge"))],
            ];
            for attributes in cases {
                assert_inner_blocks_refused("placement", &attributes);
            }
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
                definition: Definition::Spec(definition::Definition::Retention(policy)),
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
                "document.missing-attribute",
                at(0, 0),
                "the `retention` block has no `select`",
                "Add a `select` attribute with the indexes that it caps, such as \
                 \"site_a.**\"",
            );
            let keep = refused(
                "document.missing-attribute",
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
                    "document.negative-span",
                    at(0, 13),
                    "the span -1s is below zero",
                    "Write a span of zero or more",
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
                    "document.unknown-attribute",
                    at(0, 14),
                    "`hold` is not an attribute of the `retention` block",
                    "Use `select` or `keep`, or remove it",
                )])
            );
        }

        #[test]
        fn refuses_the_unknown_attribute_and_the_negative_keep_of_a_retention() {
            let documents = retention(&[
                ("select", string("edge.**")),
                ("keep", string("-1s")),
                ("hold", string("1d")),
            ]);
            assert_eq!(
                check(&documents),
                Err(vec![
                    refused(
                        "document.negative-span",
                        at(0, 13),
                        "the span -1s is below zero",
                        "Write a span of zero or more",
                    ),
                    refused(
                        "document.unknown-attribute",
                        at(0, 14),
                        "`hold` is not an attribute of the `retention` block",
                        "Use `select` or `keep`, or remove it",
                    ),
                ])
            );
        }

        #[test]
        fn refuses_a_block_inside_a_retention_with_any_attributes() {
            let select = || ("select", string("edge.**"));
            let cases = [
                vec![select(), ("keep", string("3d"))],
                vec![],
                vec![("keep", string("3d"))],
                vec![("select", Kind::Integer(7)), ("keep", string("3d"))],
                vec![select()],
                vec![select(), ("keep", Kind::Integer(3))],
                vec![select(), ("keep", string("nope"))],
                vec![select(), ("keep", string("-3d"))],
                vec![select(), ("keep", string("3d")), ("hold", string("1d"))],
            ];
            for attributes in cases {
                assert_inner_blocks_refused("retention", &attributes);
            }
        }
    }

    mod accesses {
        use spec::access::{Action, Actions, Policy};
        use types::authority::Authority;

        use super::*;

        const ACTION_FIX: &str = "Use `read`, `write`, `plan`, `apply`, `secret`, or \
                                  `admin`";
        const AUTHORITY_FIX: &str = "Write an integer from 0 to 255";

        /// An `access` block in file 0 at offset 0, labeled `edge`.
        fn access(attributes: &[(&str, Kind)]) -> [Document; 1] {
            [document(vec![block(0, 0, "access", &["edge"], attributes)])]
        }

        /// A list of `items`, the item `i` at offset `50 + i`.
        fn list(items: Vec<Kind>) -> Kind {
            let items = (50..).zip(items).map(|(offset, kind)| Value {
                kind,
                span: at(0, offset),
            });
            Kind::List(items.collect())
        }

        fn reference(text: &str) -> Kind {
            Kind::Reference(text.parse().unwrap())
        }

        /// The attributes of a policy that allows `allow`, with `authority` when given.
        fn attributes(
            allow: Kind,
            authority: Option<i128>,
        ) -> Vec<(&'static str, Kind)> {
            let mut attributes = vec![
                ("subjects", string("site_a.operators.*")),
                ("select", string("edge.**")),
                ("allow", allow),
            ];
            attributes.extend(authority.map(|n| ("authority", Kind::Integer(n))));
            attributes
        }

        /// The one entry of an access policy labeled `edge`.
        fn allowed(actions: &[Action], authority: u8) -> BTreeMap<Name, Entry> {
            let policy = Policy::new(
                selector(&["site_a.operators.*"]),
                selector(&["edge.**"]),
                actions.iter().copied().collect(),
                Authority(authority),
            )
            .unwrap();
            let entry = Entry {
                definition: Definition::Spec(definition::Definition::Access(policy)),
                label_span: at(0, 1),
            };
            BTreeMap::from([(key("edge.@access"), entry)])
        }

        fn policy(entries: &BTreeMap<Name, Entry>) -> &Policy {
            match &entries[&key("edge.@access")].definition {
                Definition::Spec(definition::Definition::Access(policy)) => policy,
                definition => panic!("not an access policy: {definition:?}"),
            }
        }

        #[test]
        fn reads_an_access_policy_from_strings_and_bare_words() {
            let both = [Action::Read, Action::Write];
            let cases = [
                (list(vec![string("read"), string("write")]), &both[..]),
                (list(vec![reference("read"), reference("write")]), &both[..]),
                (list(vec![string("write"), reference("read")]), &both[..]),
                (string("read"), &[Action::Read][..]),
                (string("write"), &[Action::Write][..]),
                (string("plan"), &[Action::Plan][..]),
                (reference("apply"), &[Action::Apply][..]),
                (string("secret"), &[Action::Secret][..]),
                (reference("admin"), &[Action::Admin][..]),
                (
                    list(vec![string("read"), reference("read")]),
                    &[Action::Read][..],
                ),
            ];
            for (allow, actions) in cases {
                let written = actions.contains(&Action::Write).then_some(200);
                let documents = access(&attributes(allow.clone(), written));
                assert_eq!(check(&documents), Ok(allowed(actions, 200)), "{allow:?}");
            }
            let every = ["read", "write", "plan", "apply", "secret", "admin"];
            let documents = access(&attributes(
                list(every.iter().map(|word| string(word)).collect()),
                Some(255),
            ));
            let all = [
                Action::Read,
                Action::Write,
                Action::Plan,
                Action::Apply,
                Action::Secret,
                Action::Admin,
            ];
            assert_eq!(check(&documents), Ok(allowed(&all, 255)));
        }

        #[test]
        fn caps_a_write_at_the_least_authority_by_default() {
            let read = check(&access(&attributes(string("write"), None))).unwrap();
            assert_eq!(read, allowed(&[Action::Write], 0));
            assert_eq!(policy(&read).authority(), Some(Authority(0)));
            let zero = check(&access(&attributes(string("write"), Some(0)))).unwrap();
            assert_eq!(zero, read);
        }

        #[test]
        fn reads_no_authority_without_write_as_none() {
            let read = check(&access(&attributes(string("read"), None))).unwrap();
            assert_eq!(policy(&read).authority(), None);
            assert_eq!(
                policy(&read).allow(),
                Actions::NONE.union([Action::Read].into_iter().collect())
            );
        }

        #[test]
        fn refuses_an_authority_without_write() {
            let message = "the policy has an `authority` and no `write` in `allow`, \
                           and only a write uses an authority";
            let fix = "Add `write` to `allow`, or remove `authority`";
            for (allow, authority) in [
                (string("read"), 200),
                (list(vec![string("read"), reference("plan")]), 0),
            ] {
                assert_eq!(
                    check(&access(&attributes(allow, Some(authority)))),
                    Err(vec![refused(
                        "config.authority-without-write",
                        at(0, 17),
                        message,
                        fix,
                    )]),
                    "{authority}"
                );
            }
        }

        #[test]
        fn refuses_only_a_missing_subjects_beside_an_authority_without_write() {
            let documents = access(&[
                ("select", string("edge.**")),
                ("allow", string("read")),
                ("authority", Kind::Integer(5)),
            ]);
            assert_eq!(
                check(&documents),
                Err(vec![refused(
                    "document.missing-attribute",
                    at(0, 0),
                    "the `access` block has no `subjects`",
                    "Add a `subjects` attribute with the subjects that it allows, \
                     such as \"site_a.operators.*\"",
                )])
            );
        }

        #[test]
        fn refuses_only_the_action_of_a_bad_allow_with_an_authority() {
            assert_eq!(
                check(&access(&attributes(string("erase"), Some(5)))),
                Err(vec![refused(
                    "config.bad-action",
                    at(0, 15),
                    "\"erase\" is not an action",
                    ACTION_FIX,
                )])
            );
        }

        #[test]
        fn quotes_a_word_that_is_not_an_action_so_that_it_cannot_name_another() {
            assert_eq!(
                check(&access(&attributes(string("x` or `read"), None))),
                Err(vec![refused(
                    "config.bad-action",
                    at(0, 15),
                    "\"x` or `read\" is not an action",
                    ACTION_FIX,
                )])
            );
        }

        #[test]
        fn refuses_a_word_that_is_not_an_action() {
            let cases = [
                (string("erase"), at(0, 15), "\"erase\" is not an action"),
                (reference("Read"), at(0, 15), "\"Read\" is not an action"),
                (
                    reference("site_a.read"),
                    at(0, 15),
                    "\"site_a.read\" is not an action",
                ),
                (
                    list(vec![string("read"), string("erase")]),
                    at(0, 51),
                    "\"erase\" is not an action",
                ),
                (
                    list(vec![Kind::Integer(1), string("erase")]),
                    at(0, 50),
                    "an action is a string or a reference, not an integer",
                ),
                (
                    Kind::Bool(true),
                    at(0, 15),
                    "an action is a string or a reference, not a bool",
                ),
            ];
            for (allow, span, message) in cases {
                assert_eq!(
                    check(&access(&attributes(allow, None))),
                    Err(vec![refused(
                        "config.bad-action",
                        span,
                        message,
                        ACTION_FIX
                    )]),
                    "{message}"
                );
            }
        }

        #[test]
        fn refuses_an_empty_allow() {
            for authority in [None, Some(3)] {
                assert_eq!(
                    check(&access(&attributes(list(vec![]), authority))),
                    Err(vec![refused(
                        "config.empty-allow",
                        at(0, 15),
                        "the `allow` list holds no action",
                        "Add one or more actions, such as \"read\"",
                    )]),
                    "{authority:?}"
                );
            }
        }

        #[test]
        fn refuses_an_empty_allow_only_when_each_other_attribute_reads() {
            let mut attributes = attributes(list(vec![]), None);
            attributes[0].1 = Kind::Integer(5);
            let codes = check(&access(&attributes)).map_err(|found| {
                found.iter().map(|d| d.code.as_str()).collect::<Vec<_>>()
            });
            assert_eq!(codes, Err(vec!["document.bad-selector"]));
        }

        #[test]
        fn refuses_an_empty_allow_only_when_each_attribute_is_known() {
            let mut attributes = attributes(list(vec![]), None);
            attributes.push(("deny", string("write")));
            let codes = check(&access(&attributes)).map_err(|found| {
                found.iter().map(|d| d.code.as_str()).collect::<Vec<_>>()
            });
            assert_eq!(codes, Err(vec!["document.unknown-attribute"]));
        }

        #[test]
        fn refuses_an_authority_that_is_not_from_0_to_255() {
            for (authority, message) in [
                (Kind::Integer(256), "the authority 256 is not from 0 to 255"),
                (Kind::Integer(-1), "the authority -1 is not from 0 to 255"),
                (string("high"), "an authority is an integer, not a string"),
            ] {
                let mut attributes = attributes(string("write"), None);
                attributes.push(("authority", authority));
                assert_eq!(
                    check(&access(&attributes)),
                    Err(vec![refused(
                        "config.bad-authority",
                        at(0, 17),
                        message,
                        AUTHORITY_FIX,
                    )]),
                    "{message}"
                );
            }
        }

        #[test]
        fn refuses_an_access_without_subjects_select_or_allow() {
            let subjects = refused(
                "document.missing-attribute",
                at(0, 0),
                "the `access` block has no `subjects`",
                "Add a `subjects` attribute with the subjects that it allows, such as \
                 \"site_a.operators.*\"",
            );
            let select = refused(
                "document.missing-attribute",
                at(0, 0),
                "the `access` block has no `select`",
                "Add a `select` attribute with the names that it allows them to use, \
                 such as \"site_a.**\"",
            );
            let allow = refused(
                "document.missing-attribute",
                at(0, 0),
                "the `access` block has no `allow`",
                "Add an `allow` attribute with the actions that it allows, such as \
                 \"read\"",
            );
            let full = attributes(string("read"), None);
            for (i, diagnostic) in [subjects.clone(), select.clone(), allow.clone()]
                .into_iter()
                .enumerate()
            {
                let mut attributes = full.clone();
                attributes.remove(i);
                assert_eq!(
                    check(&access(&attributes)),
                    Err(vec![diagnostic.clone()]),
                    "{diagnostic:?}"
                );
            }
            assert_eq!(
                check(&access(&[("authority", Kind::Integer(1))])),
                Err(vec![subjects, select, allow])
            );
        }

        #[test]
        fn refuses_what_an_access_block_cannot_hold() {
            // An unknown attribute also hides an `authority` with no `write`.
            for (authority, key) in [(None, 16), (Some(5), 18)] {
                let mut attributes = attributes(string("read"), authority);
                attributes.push(("deny", string("write")));
                assert_eq!(
                    check(&access(&attributes)),
                    Err(vec![refused(
                        "document.unknown-attribute",
                        at(0, key),
                        "`deny` is not an attribute of the `access` block",
                        "Use `subjects`, `select`, `allow`, or `authority`, or remove \
                         it",
                    )])
                );
            }
            assert_inner_blocks_refused(
                "access",
                &self::attributes(string("read"), None),
            );
            assert_inner_blocks_refused(
                "access",
                &self::attributes(string("read"), Some(5)),
            );
        }

        #[test]
        fn reads_an_access_and_a_node_settings_with_one_label() {
            let documents = [document(vec![
                block(0, 0, "access", &["edge"], &attributes(string("read"), None)),
                settings(
                    0,
                    100,
                    "edge",
                    &[("select", string("edge.*")), ("disk", string("1GiB"))],
                ),
            ])];
            let keys: Vec<String> = check(&documents)
                .unwrap()
                .keys()
                .map(ToString::to_string)
                .collect();
            assert_eq!(keys, ["edge.@access", "edge.@node_settings"]);
        }
    }

    mod channels {
        use spec::channel::{self, Data};
        use spec::data_type::DataType;
        use spec::unit::Unit;
        use types::sample::{self, Scalar};

        use super::*;

        /// A `channel` block in file `file` at `offset`, labeled `label`.
        fn channel(
            file: u32,
            offset: u32,
            label: &str,
            attributes: &[(&str, Kind)],
        ) -> Block {
            block(file, offset, "channel", &[label], attributes)
        }

        fn reference(text: &str) -> Kind {
            Kind::Reference(key(text))
        }

        fn scalar(element: Scalar) -> DataType {
            DataType::Sample(sample::Type::Scalar(element))
        }

        fn data(index: &str, quality: Option<&str>, data_type: DataType) -> Definition {
            let data =
                Data::new(key(index), quality.map(key), data_type, None).unwrap();
            Definition::Channel(channel::Kind::Data(data))
        }

        fn entry(definition: Definition, label_span: Option<Span>) -> Entry {
            Entry {
                definition,
                label_span,
            }
        }

        /// The data channel `edge.value` in file 0 with `attributes`.
        fn value(attributes: &[(&str, Kind)]) -> [Document; 1] {
            let time = channel(0, 0, "edge.time", &[("kind", string("index"))]);
            let value = channel(0, 100, "edge.value", attributes);
            [document(vec![time, value])]
        }

        fn unknown(span: Option<Span>, message: &str) -> Diagnostic {
            refused(
                "config.unknown-channel",
                span,
                message,
                "Name a channel that a `channel` block defines",
            )
        }

        #[test]
        fn reads_the_channels_of_the_edge_fixture() {
            let documents = [document(vec![
                channel(
                    0,
                    0,
                    "edge.time",
                    &[
                        ("kind", string("index")),
                        ("error", string("edge.time_error")),
                    ],
                ),
                channel(
                    0,
                    100,
                    "edge.time_error",
                    &[("data_type", string("u64")), ("index", string("edge.time"))],
                ),
                channel(
                    0,
                    200,
                    "edge.value",
                    &[("data_type", string("f64")), ("index", string("edge.time"))],
                ),
                block(
                    0,
                    300,
                    "placement",
                    &["edge"],
                    &[("select", string("edge.*")), ("home", string("edge"))],
                ),
            ])];
            let index = channel::Kind::Index {
                error: Some(key("edge.time_error")),
                control: None,
            };
            let nodes = spec::placement::Nodes {
                home: Some(key("edge")),
                standby: None,
                copies: Vec::new(),
            };
            let placement =
                spec::placement::Policy::new(selector(&["edge.*"]), nodes).unwrap();
            let placement = definition::Definition::Placement(placement);
            assert_eq!(
                check(&documents),
                Ok(BTreeMap::from([
                    (
                        key("edge.time"),
                        entry(Definition::Channel(index), at(0, 1)),
                    ),
                    (
                        key("edge.time_error"),
                        entry(data("edge.time", None, scalar(Scalar::U64)), at(0, 101)),
                    ),
                    (
                        key("edge.value"),
                        entry(data("edge.time", None, scalar(Scalar::F64)), at(0, 201)),
                    ),
                    (
                        key("edge.@placement"),
                        entry(Definition::Spec(placement), at(0, 301)),
                    ),
                ]))
            );
        }

        #[test]
        fn reads_each_attribute_from_a_reference() {
            let documents = [document(vec![
                channel(
                    0,
                    0,
                    "t",
                    &[("kind", reference("index")), ("control", reference("c"))],
                ),
                channel(
                    0,
                    100,
                    "q",
                    &[
                        ("data_type", reference("quality")),
                        ("index", reference("t")),
                    ],
                ),
                channel(
                    0,
                    200,
                    "c",
                    &[
                        ("kind", reference("data")),
                        ("data_type", reference("f32")),
                        ("index", reference("t")),
                        ("quality", reference("q")),
                        ("unit", reference("kPa")),
                    ],
                ),
            ])];
            let entries = check(&documents).unwrap();
            let unit = Some(Unit::new("kPa").unwrap());
            let c = Data::new(key("t"), Some(key("q")), scalar(Scalar::F32), unit);
            let kinds = [
                ("c", channel::Kind::Data(c.unwrap())),
                (
                    "q",
                    channel::Kind::Data(
                        Data::new(key("t"), None, DataType::Quality, None).unwrap(),
                    ),
                ),
                (
                    "t",
                    channel::Kind::Index {
                        error: None,
                        control: Some(key("c")),
                    },
                ),
            ];
            for (name, kind) in kinds {
                assert_eq!(
                    entries[&key(name)].definition,
                    Definition::Channel(kind),
                    "{name}"
                );
            }
        }

        #[test]
        fn reads_a_channel_and_a_policy_with_one_label() {
            let documents = [document(vec![
                channel(0, 0, "edge", &[("kind", string("index"))]),
                block(
                    0,
                    100,
                    "placement",
                    &["edge"],
                    &[("select", string("edge.*")), ("home", string("edge"))],
                ),
            ])];
            let keys: Vec<String> = check(&documents)
                .unwrap()
                .keys()
                .map(ToString::to_string)
                .collect();
            assert_eq!(keys, ["edge", "edge.@placement"]);
        }

        #[test]
        fn takes_a_channel_name_of_the_full_length_as_its_key() {
            let index = [("kind", string("index"))];
            let fits = format!("a.{}", "b".repeat(253));
            let long = format!("{fits}c");
            let documents = [document(vec![
                channel(0, 0, &fits, &index),
                channel(0, 100, &long, &index),
            ])];
            assert_eq!(
                check(&documents),
                Err(vec![refused(
                    "config.long-name",
                    at(0, 101),
                    &format!(
                        "the name {long:?} is 256 bytes, and the most for the \
                         `channel` block is 255"
                    ),
                    "Shorten the name to at most 255 bytes",
                )])
            );
            let documents = [document(vec![channel(0, 0, &fits, &index)])];
            assert_eq!(
                check(&documents).unwrap().into_keys().collect::<Vec<_>>(),
                [key(&fits)]
            );
        }

        #[test]
        fn refuses_a_channel_name_that_repeats_in_other_case() {
            let index = [("kind", string("index"))];
            let documents = [
                document(vec![channel(0, 0, "edge.time", &index)]),
                document(vec![channel(1, 0, "Edge.time", &index)]),
            ];
            let mut repeat = refused(
                "config.duplicate-name",
                at(1, 1),
                "the name \"Edge.time\" repeats the earlier `channel` name \
                 \"edge.time\"",
                "Give each `channel` block a name that differs by more than case",
            );
            repeat.notes.push(Note {
                span: at(0, 1).unwrap(),
                text: "the earlier name".into(),
            });
            assert_eq!(check(&documents), Err(vec![repeat]));
        }

        #[test]
        fn finds_a_channel_that_a_later_file_defines() {
            let documents = [
                document(vec![channel(
                    0,
                    0,
                    "edge.value",
                    &[("data_type", string("f64")), ("index", string("edge.time"))],
                )]),
                document(vec![channel(
                    1,
                    0,
                    "edge.time",
                    &[("kind", string("index"))],
                )]),
            ];
            let entries = check(&documents).unwrap();
            assert_eq!(
                entries[&key("edge.value")].definition,
                data("edge.time", None, scalar(Scalar::F64))
            );
        }

        #[test]
        fn refuses_each_edge_of_an_index_to_an_unknown_channel() {
            let documents = [document(vec![channel(
                0,
                0,
                "edge.time",
                &[
                    ("kind", string("index")),
                    ("control", string("edge.ctl")),
                    ("error", string("edge.err")),
                ],
            )])];
            assert_eq!(
                check(&documents),
                Err(vec![
                    unknown(
                        at(0, 13),
                        "no `channel` block defines the control channel `edge.ctl`",
                    ),
                    unknown(
                        at(0, 15),
                        "no `channel` block defines the error channel `edge.err`",
                    ),
                ])
            );
        }

        #[test]
        fn refuses_each_edge_of_a_data_channel_in_source_order() {
            let documents = value(&[
                ("data_type", string("f64")),
                ("quality", string("edge.q")),
                ("index", string("edge.tim")),
            ]);
            assert_eq!(
                check(&documents),
                Err(vec![
                    unknown(
                        at(0, 113),
                        "no `channel` block defines the quality channel `edge.q`",
                    ),
                    unknown(
                        at(0, 115),
                        "no `channel` block defines the index channel `edge.tim`",
                    ),
                ])
            );
        }

        #[test]
        fn refuses_a_unit_on_a_data_type_that_holds_no_number() {
            let documents = value(&[
                ("data_type", string("bool")),
                ("index", string("edge.time")),
                ("unit", string("V")),
            ]);
            assert_eq!(
                check(&documents),
                Err(vec![refused(
                    "config.bad-unit",
                    at(0, 115),
                    "a unit is on a data type that holds no number: the data type is \
                     \"bool\"",
                    "Remove the unit, or give the channel a numeric type",
                )])
            );
        }

        #[test]
        fn refuses_a_data_type_that_does_not_read() {
            let cases = [
                (
                    "F64",
                    "cannot read the data type \"F64\": expected a data type such as \
                     f64, f32[3], list<u8, 16>, string, bytes, or quality",
                    "Use one of the forms that the message names, with exact case and \
                     a space only after the comma of a list",
                ),
                (
                    "f32[2][3][4]",
                    "cannot read the data type \"f32[2][3][4]\": expected one or two \
                     array lengths",
                    "Use one or two array lengths, such as f32[3] or f32[2][3]",
                ),
            ];
            for (text, message, fix) in cases {
                let documents = value(&[
                    ("data_type", string(text)),
                    ("index", string("edge.time")),
                ]);
                assert_eq!(
                    check(&documents),
                    Err(vec![refused(
                        "config.bad-data-type",
                        at(0, 111),
                        message,
                        fix
                    )]),
                    "{text}"
                );
            }
        }

        #[test]
        fn refuses_a_unit_that_does_not_read() {
            let documents = value(&[
                ("data_type", string("f64")),
                ("index", string("edge.time")),
                ("unit", string("k Pa")),
            ]);
            assert_eq!(
                check(&documents),
                Err(vec![refused(
                    "config.bad-unit",
                    at(0, 115),
                    "cannot read the unit \"k Pa\": a unit has the character ' ' at \
                     byte 1, which is not printable ASCII",
                    "Use only printable ASCII characters with no space, such as m/s2",
                )])
            );
        }

        #[test]
        fn refuses_a_value_that_is_not_text() {
            let index = ("index", string("edge.time"));
            let f64 = ("data_type", string("f64"));
            let cases = [
                (
                    vec![index.clone(), ("data_type", Kind::Integer(7))],
                    at(0, 113),
                    "config.bad-data-type",
                    "the data type is a string or a reference, not an integer",
                    "Write a string such as \"f64\"",
                ),
                (
                    vec![f64.clone(), index.clone(), ("unit", Kind::Bool(true))],
                    at(0, 115),
                    "config.bad-unit",
                    "the unit is a string or a reference, not a bool",
                    "Write a string such as \"kPa\"",
                ),
                (
                    vec![f64, index, ("kind", Kind::List(Vec::new()))],
                    at(0, 115),
                    "config.bad-channel-kind",
                    "the channel kind is a string or a reference, not a list",
                    "Write a string such as \"index\"",
                ),
            ];
            for (attributes, span, code, message, fix) in cases {
                assert_eq!(
                    check(&value(&attributes)),
                    Err(vec![refused(code, span, message, fix)]),
                    "{code}"
                );
            }
        }

        #[test]
        fn refuses_an_edge_that_is_not_a_name() {
            let documents =
                value(&[("index", Kind::Integer(7)), ("data_type", string("f64"))]);
            assert_eq!(
                check(&documents),
                Err(vec![refused(
                    "document.bad-name",
                    at(0, 111),
                    "a name is a string or a reference, not an integer",
                    "Write a name such as \"site_a.node_1\"",
                )])
            );
        }

        #[test]
        fn refuses_an_error_edge_of_an_index_that_is_not_a_name() {
            let time = channel(
                0,
                0,
                "edge.time",
                &[("kind", string("index")), ("error", Kind::Integer(7))],
            );
            assert_eq!(
                check(&[document(vec![time])]),
                Err(vec![refused(
                    "document.bad-name",
                    at(0, 13),
                    "a name is a string or a reference, not an integer",
                    "Write a name such as \"site_a.node_1\"",
                )])
            );
        }

        #[test]
        fn leaves_the_edges_after_a_bad_kind() {
            let documents = value(&[
                ("kind", string("stream")),
                ("other", string("x")),
                ("index", string("edge.tim")),
                ("quality", string("edge.q")),
                ("error", string("edge.e")),
                ("control", string("edge.c")),
            ]);
            assert_eq!(
                check(&documents),
                Err(vec![
                    refused(
                        "config.bad-channel-kind",
                        at(0, 111),
                        "\"stream\" is not a kind of channel",
                        "Write \"index\" or \"data\"",
                    ),
                    refused(
                        "document.unknown-attribute",
                        at(0, 112),
                        "`other` is not an attribute of the `channel` block",
                        "Use `control`, `data_type`, `error`, `index`, `kind`, \
                         `quality`, or `unit`, or remove it",
                    ),
                ])
            );
        }

        #[test]
        fn leaves_an_attribute_that_a_kind_knows_after_a_bad_kind() {
            let documents = value(&[
                ("kind", string("stream")),
                ("data_type", string("f65")),
                ("control", Kind::Integer(7)),
            ]);
            assert_eq!(
                check(&documents),
                Err(vec![refused(
                    "config.bad-channel-kind",
                    at(0, 111),
                    "\"stream\" is not a kind of channel",
                    "Write \"index\" or \"data\"",
                ),])
            );
        }

        #[test]
        fn refuses_an_attribute_of_the_other_kind() {
            let documents = [document(vec![
                channel(
                    0,
                    0,
                    "edge.time",
                    &[("kind", string("index")), ("index", string("edge.time"))],
                ),
                channel(
                    0,
                    100,
                    "edge.value",
                    &[
                        ("data_type", string("f64")),
                        ("index", string("edge.time")),
                        ("error", string("edge.time")),
                    ],
                ),
            ])];
            assert_eq!(
                check(&documents),
                Err(vec![
                    refused(
                        "document.unknown-attribute",
                        at(0, 12),
                        "`index` is not an attribute of the `channel` block",
                        "Use `kind`, `error`, or `control`, or remove it",
                    ),
                    refused(
                        "document.unknown-attribute",
                        at(0, 114),
                        "`error` is not an attribute of the `channel` block",
                        "Use `kind`, `data_type`, `index`, `quality`, or `unit`, or \
                         remove it",
                    ),
                ])
            );
        }

        #[test]
        fn refuses_a_data_channel_without_its_index_or_data_type() {
            assert_eq!(
                check(&value(&[])),
                Err(vec![
                    refused(
                        "document.missing-attribute",
                        at(0, 100),
                        "the `channel` block has no `index`",
                        "Add an `index` attribute with the name of an index channel, \
                         such as \"edge.time\"",
                    ),
                    refused(
                        "document.missing-attribute",
                        at(0, 100),
                        "the `channel` block has no `data_type`",
                        "Add a `data_type` attribute such as \"f64\"",
                    ),
                ])
            );
        }

        #[test]
        fn refuses_a_block_inside_a_channel() {
            assert_inner_blocks_refused("channel", &[("kind", string("index"))]);
            assert_inner_blocks_refused("channel", &[("kind", string("stream"))]);
            assert_inner_blocks_refused("channel", &[("data_type", string("f64"))]);
            assert_inner_blocks_refused(
                "channel",
                &[
                    ("data_type", string("bool")),
                    ("index", string("edge")),
                    ("unit", string("V")),
                ],
            );
        }

        #[test]
        fn refuses_an_edge_to_the_label_of_a_policy() {
            let placement = block(
                0,
                0,
                "placement",
                &["site"],
                &[("select", string("site.*")), ("home", string("site"))],
            );
            let documents = [document(vec![
                placement,
                channel(
                    0,
                    100,
                    "v",
                    &[("data_type", string("f64")), ("index", string("site"))],
                ),
            ])];
            assert_eq!(
                check(&documents),
                Err(vec![unknown(
                    at(0, 113),
                    "no `channel` block defines the index channel `site`",
                )])
            );
        }

        #[test]
        fn refuses_an_unknown_edge_of_a_block_with_two_labels() {
            let documents = [document(vec![block(
                0,
                0,
                "channel",
                &["v", "w"],
                &[("data_type", string("f64")), ("index", string("nope"))],
            )])];
            assert_eq!(
                check(&documents),
                Err(vec![
                    refused(
                        "document.label-count",
                        at(0, 2),
                        "the `channel` block has 2 labels, and it takes 1 label",
                        "Give the block one label, its name, such as \"site_a.budget\"",
                    ),
                    unknown(
                        at(0, 13),
                        "no `channel` block defines the index channel `nope`",
                    ),
                ])
            );
        }

        #[test]
        fn refuses_an_unknown_edge_beside_a_bad_data_type() {
            let documents =
                value(&[("data_type", string("F64")), ("index", string("nope"))]);
            assert_eq!(
                check(&documents),
                Err(vec![
                    refused(
                        "config.bad-data-type",
                        at(0, 111),
                        "cannot read the data type \"F64\": expected a data type such \
                         as f64, f32[3], list<u8, 16>, string, bytes, or quality",
                        "Use one of the forms that the message names, with exact case \
                         and a space only after the comma of a list",
                    ),
                    unknown(
                        at(0, 113),
                        "no `channel` block defines the index channel `nope`",
                    ),
                ])
            );
        }

        #[test]
        fn checks_no_unit_after_an_unknown_edge() {
            let documents = value(&[
                ("data_type", string("string")),
                ("index", string("nope")),
                ("unit", string("kPa")),
            ]);
            assert_eq!(
                check(&documents),
                Err(vec![unknown(
                    at(0, 113),
                    "no `channel` block defines the index channel `nope`",
                )])
            );
        }

        #[test]
        fn refuses_an_unknown_edge_beside_a_bad_unit() {
            let documents = value(&[
                ("data_type", string("f64")),
                ("index", string("edge.tim")),
                ("unit", string("k Pa")),
            ]);
            assert_eq!(
                check(&documents),
                Err(vec![
                    unknown(
                        at(0, 113),
                        "no `channel` block defines the index channel `edge.tim`",
                    ),
                    refused(
                        "config.bad-unit",
                        at(0, 115),
                        "cannot read the unit \"k Pa\": a unit has the character ' ' \
                         at byte 1, which is not printable ASCII",
                        "Use only printable ASCII characters with no space, such as \
                         m/s2",
                    ),
                ])
            );
        }

        #[test]
        fn refuses_an_unknown_edge_beside_an_unknown_attribute() {
            let documents = [document(vec![channel(
                0,
                0,
                "edge.time",
                &[
                    ("kind", string("index")),
                    ("error", string("edge.err")),
                    ("unit", string("s")),
                ],
            )])];
            assert_eq!(
                check(&documents),
                Err(vec![
                    unknown(
                        at(0, 13),
                        "no `channel` block defines the error channel `edge.err`",
                    ),
                    refused(
                        "document.unknown-attribute",
                        at(0, 14),
                        "`unit` is not an attribute of the `channel` block",
                        "Use `kind`, `error`, or `control`, or remove it",
                    ),
                ])
            );
        }

        #[test]
        fn refuses_an_unknown_edge_of_a_data_channel_beside_an_unknown_attribute() {
            let documents = value(&[
                ("data_type", string("f64")),
                ("index", string("edge.tim")),
                ("unit2", string("x")),
            ]);
            assert_eq!(
                check(&documents),
                Err(vec![
                    unknown(
                        at(0, 113),
                        "no `channel` block defines the index channel `edge.tim`",
                    ),
                    refused(
                        "document.unknown-attribute",
                        at(0, 114),
                        "`unit2` is not an attribute of the `channel` block",
                        "Use `kind`, `data_type`, `index`, `quality`, or `unit`, or \
                         remove it",
                    ),
                ])
            );
        }
    }

    mod subjects {
        use base64ct::{Base64, Encoding};
        use spec::subject::Subject;
        use types::ed25519::PublicKey;

        use super::*;

        /// A line that `ssh-keygen -t ed25519` wrote.
        const ALICE: &str = concat!(
            "ssh-ed25519 ",
            "AAAAC3NzaC1lZDI1NTE5AAAAIGVVuOR8JKYpAcWLMUveadmJ1wUAmYGgIDtqlhFe7Yhg",
            " alice@laptop",
        );
        const ALICE_KEY: [u8; 32] = [
            0x65, 0x55, 0xb8, 0xe4, 0x7c, 0x24, 0xa6, 0x29, 0x01, 0xc5, 0x8b, 0x31,
            0x4b, 0xde, 0x69, 0xd9, 0x89, 0xd7, 0x05, 0x00, 0x99, 0x81, 0xa0, 0x20,
            0x3b, 0x6a, 0x96, 0x11, 0x5e, 0xed, 0x88, 0x60,
        ];
        const BAD_FIX: &str = "Use the one line of a `.pub` file, such as \
                               `ssh-ed25519 AAAA... alice@laptop`";
        const NOT_A_LINE: &str = "the public key is not the line of a `.pub` file";
        const NOT_ED25519: &str = "the base64 of the public key is not an Ed25519 key";

        /// A `subject` block in file 0 at offset 0, labeled `alice`.
        fn subject(attributes: &[(&str, Kind)]) -> [Document; 1] {
            [document(vec![block(
                0,
                0,
                "subject",
                &["alice"],
                attributes,
            )])]
        }

        /// A list of `items`, the item `i` at offset `50 + i`.
        fn list(items: &[Kind]) -> Kind {
            let items = (50..).zip(items).map(|(offset, kind)| Value {
                kind: kind.clone(),
                span: at(0, offset),
            });
            Kind::List(items.collect())
        }

        /// The one entry of a subject labeled `alice` with `keys`.
        fn keyed(keys: &[[u8; 32]]) -> BTreeMap<Name, Entry> {
            let keys = keys.iter().map(|&key| PublicKey::new(key).unwrap());
            let subject = Subject::new(keys.collect()).unwrap();
            let entry = Entry {
                definition: Definition::Spec(definition::Definition::Subject(subject)),
                label_span: at(0, 1),
            };
            BTreeMap::from([(key("alice.@subject"), entry)])
        }

        /// A `.pub` line of `algorithm` whose base64 holds `blob`.
        fn line(algorithm: &str, blob: &[u8]) -> String {
            let mut text = [0; 128];
            let encoded = Base64::encode(blob, &mut text).unwrap();
            format!("{algorithm} {encoded} bob@site_a")
        }

        /// The `.pub` line of the Ed25519 key `key`.
        fn ed25519(key: [u8; 32]) -> String {
            line(
                "ssh-ed25519",
                &[&b"\0\0\0\x0bssh-ed25519\0\0\0\x20"[..], &key].concat(),
            )
        }

        fn bad(span: Option<Span>, message: &str) -> Diagnostic {
            refused("config.bad-public-key", span, message, BAD_FIX)
        }

        #[test]
        fn reads_the_line_of_a_pub_file() {
            let bare = ALICE.trim_end_matches(" alice@laptop");
            let cases = [
                string(ALICE),
                string(bare),
                string(&format!("  {ALICE}\n")),
                string(&format!("{bare} a comment with words")),
                string(&ALICE.replace(' ', "\t")),
                string(&ALICE.replace(' ', "  ")),
                list(&[string(ALICE)]),
            ];
            for keys in cases {
                let documents = subject(&[("keys", keys.clone())]);
                assert_eq!(check(&documents), Ok(keyed(&[ALICE_KEY])), "{keys:?}");
            }
            assert_eq!(ed25519(ALICE_KEY).split(' ').nth(1), bare.split(' ').nth(1));
        }

        #[test]
        fn gives_the_fingerprint_that_ssh_keygen_gives() {
            let key = PublicKey::new(ALICE_KEY).expect("a key");
            assert_eq!(
                crate::openssh::fingerprint(key),
                "SHA256:AaHjcjahcS7PIOJwyahzFqtJH7PJ8NKy89OZdEKcurc"
            );
        }

        #[test]
        fn reads_a_list_of_keys_in_byte_order() {
            let keys = list(&[string(&ed25519([9; 32])), string(ALICE)]);
            assert_eq!(
                check(&subject(&[("keys", keys)])),
                Ok(keyed(&[[9; 32], ALICE_KEY]))
            );
        }

        #[test]
        fn reads_the_rest_of_the_line_as_the_comment() {
            let bob = ed25519([9; 32]).replace("bob@site_a", "bob@desk");
            let line = format!("{ALICE} {bob}");
            assert_eq!(
                check(&subject(&[("keys", string(&line))])),
                Ok(keyed(&[ALICE_KEY]))
            );
        }

        #[test]
        fn refuses_a_subject_without_keys() {
            assert_eq!(
                check(&subject(&[])),
                Err(vec![refused(
                    "document.missing-attribute",
                    at(0, 0),
                    "the `subject` block has no `keys`",
                    "Add a `keys` attribute with the line of a `.pub` file, such as \
                     \"ssh-ed25519 AAAA... alice@laptop\"",
                )])
            );
        }

        #[test]
        fn refuses_an_empty_list_at_the_list() {
            assert_eq!(
                check(&subject(&[("keys", Kind::List(Vec::new()))])),
                Err(vec![refused(
                    "config.no-public-keys",
                    at(0, 11),
                    "the subject has no public key",
                    "Add at least one public key",
                )])
            );
        }

        #[test]
        fn refuses_a_repeated_key_at_its_second_copy() {
            let again = ALICE.replace("alice@laptop", "alice@desk");
            let keys =
                list(&[string(ALICE), string(&ed25519([9; 32])), string(&again)]);
            let mut twice = refused(
                "config.duplicate-public-key",
                at(0, 52),
                "a public key repeats an earlier one",
                "Remove the second copy of the key",
            );
            twice.notes.push(Note {
                span: at(0, 50).unwrap(),
                text: "the earlier key".into(),
            });
            assert_eq!(check(&subject(&[("keys", keys)])), Err(vec![twice]));
        }

        #[test]
        fn refuses_a_private_key_and_quotes_none_of_it() {
            let openssh = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
                           b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMw\n\
                           -----END OPENSSH PRIVATE KEY-----\n";
            let rsa = "  -----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n";
            let ssh2 = "---- BEGIN SSH2 ENCRYPTED PRIVATE KEY ----\n\
                        Comment: \"rsa-key-20261008\"\nP2/56wAAA+wAAAA3aWYtbW9kbntz\n";
            let ppk = "PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: none\n\
                       Comment: alice@laptop\nPublic-Lines: 2\n";
            let comment = format!("{ALICE} PRIVATE KEY");
            let cases = [
                (string(openssh), at(0, 11)),
                (string(rsa), at(0, 11)),
                (string(ssh2), at(0, 11)),
                (string(ppk), at(0, 11)),
                (string(&comment), at(0, 11)),
                (list(&[string(ALICE), string(openssh)]), at(0, 51)),
                (list(&[Kind::Integer(7), string(openssh)]), at(0, 51)),
            ];
            for (keys, span) in cases {
                assert_eq!(
                    check(&subject(&[("keys", keys.clone())])),
                    Err(vec![refused(
                        "config.private-key",
                        span,
                        "the value is a private key, which must never be in a file",
                        "Remove the private key from this file now, and use the one \
                         line of its `.pub` file",
                    )]),
                    "{keys:?}"
                );
            }
        }

        #[test]
        fn refuses_a_private_key_at_any_depth() {
            let private = Value {
                kind: string("-----BEGIN OPENSSH PRIVATE KEY-----\nb3Bl\n"),
                span: at(0, 70),
            };
            let call = Kind::Call(document::value::Call {
                function: "secret".into(),
                function_span: at(0, 60),
                arguments: vec![private.clone()],
            });
            let map = Map::new(vec![Attribute {
                key: "-----BEGIN OPENSSH PRIVATE KEY-----\nb3Bl\n".into(),
                key_span: at(0, 70),
                value: Value {
                    kind: Kind::Integer(1),
                    span: at(0, 80),
                },
            }])
            .unwrap();
            let in_value = Map::new(vec![Attribute {
                key: "a".into(),
                key_span: at(0, 60),
                value: private.clone(),
            }])
            .unwrap();
            let cases = [
                list(&[Kind::List(vec![private])]),
                list(&[string(ALICE), call]),
                Kind::Map(map.clone()),
                list(&[Kind::Map(map)]),
                Kind::Map(in_value),
            ];
            for keys in cases {
                assert_eq!(
                    check(&subject(&[("keys", keys.clone())])),
                    Err(vec![refused(
                        "config.private-key",
                        at(0, 70),
                        "the value is a private key, which must never be in a file",
                        "Remove the private key from this file now, and use the one \
                         line of its `.pub` file",
                    )]),
                    "{keys:?}"
                );
            }
        }

        #[test]
        fn refuses_a_private_key_in_a_block_inside_a_definition() {
            let mut policy = settings(
                0,
                0,
                "edge",
                &[("select", string("edge")), ("disk", string("1GiB"))],
            );
            let private = string("-----BEGIN OPENSSH PRIVATE KEY-----");
            let inner = block(0, 90, "inner", &[], &[("note", private)]);
            policy.body.blocks.push(inner);
            assert_eq!(
                check(&[document(vec![policy])]),
                Err(vec![refused(
                    "config.private-key",
                    at(0, 101),
                    "the value is a private key, which must never be in a file",
                    "Remove the private key from this file now, and use the one line \
                     of its `.pub` file",
                )])
            );
        }

        #[test]
        fn refuses_a_key_of_another_algorithm_by_its_name() {
            let cases = [
                "ssh-rsa",
                "ssh-dss",
                "ecdsa-sha2-nistp256",
                "ecdsa-sha2-nistp384",
                "ecdsa-sha2-nistp521",
                "sk-ecdsa-sha2-nistp256@openssh.com",
                "sk-ssh-ed25519@openssh.com",
                "ssh-rsa-cert-v01@openssh.com",
                "ssh-dss-cert-v01@openssh.com",
                "ecdsa-sha2-nistp256-cert-v01@openssh.com",
                "ecdsa-sha2-nistp384-cert-v01@openssh.com",
                "ecdsa-sha2-nistp521-cert-v01@openssh.com",
                "sk-ecdsa-sha2-nistp256-cert-v01@openssh.com",
                "ssh-ed25519-cert-v01@openssh.com",
                "sk-ssh-ed25519-cert-v01@openssh.com",
                "ssh-xmss@openssh.com",
                "ssh-xmss-cert-v01@openssh.com",
            ];
            for algorithm in cases {
                let keys = string(&line(algorithm, &[0; 51]));
                assert_eq!(
                    check(&subject(&[("keys", keys)])),
                    Err(vec![refused(
                        "config.public-key-algorithm",
                        at(0, 11),
                        &format!(
                            "the public key is \"{algorithm}\", and a subject takes \
                             only `ssh-ed25519`"
                        ),
                        "Make an Ed25519 key with `ssh-keygen -t ed25519`, and use the \
                         line of its `.pub` file",
                    )]),
                    "{algorithm}"
                );
            }
        }

        #[test]
        fn refuses_a_value_that_is_not_the_line_of_an_ed25519_key() {
            let start = &b"\0\0\0\x0bssh-ed25519\0\0\0\x20"[..];
            let mut identity = [0; 32];
            identity[0] = 1;
            let cases = [
                (Kind::Integer(7), "a public key is a string, not an integer"),
                (string(""), NOT_A_LINE),
                (string("ssh-ed25519"), NOT_A_LINE),
                (string("ssh-\x1b[2Jok AAAA"), NOT_A_LINE),
                (string("sk-proj-0123456789abcdef AAAA"), NOT_A_LINE),
                (string("ssh-rsa2 AAAA"), NOT_A_LINE),
                (string(&ALICE[12..]), NOT_A_LINE),
                (
                    string("-----BEGIN PUBLIC KEY----- MCowBQYDK2VwAyEA"),
                    NOT_A_LINE,
                ),
                (
                    string("---- BEGIN SSH2 PUBLIC KEY ----\nAAAAC3NzaC1lZDI1NTE5"),
                    NOT_A_LINE,
                ),
                (string("ssh-ed25519 !!!!"), NOT_ED25519),
                (string(&ALICE.replace(" alice", "= alice")), NOT_ED25519),
                (
                    string(&line("ssh-ed25519", &[start, &[9; 31]].concat())),
                    NOT_ED25519,
                ),
                (
                    string(&line("ssh-ed25519", &[start, &[9; 33]].concat())),
                    NOT_ED25519,
                ),
                (
                    string(&line("ssh-ed25519", &[&start[..18], &[9; 33]].concat())),
                    NOT_ED25519,
                ),
                (string(&line("ssh-ed25519", &[9; 51])), NOT_ED25519),
                (
                    string(&ed25519(identity)),
                    "the public key is a point of small order",
                ),
            ];
            for (keys, message) in cases {
                let documents = subject(&[("keys", keys.clone())]);
                assert_eq!(
                    check(&documents),
                    Err(vec![bad(at(0, 11), message)]),
                    "{keys:?}"
                );
            }
        }

        #[test]
        fn refuses_a_key_length_longer_than_the_key() {
            let start = &b"\0\0\0\x0bssh-ed25519\0\0\0"[..];
            for length in [0x21, 0xff] {
                let blob = [start, &[length], &ALICE_KEY].concat();
                let keys = string(&line("ssh-ed25519", &blob));
                assert_eq!(
                    check(&subject(&[("keys", keys.clone())])),
                    Err(vec![bad(at(0, 11), NOT_ED25519)]),
                    "{keys:?}"
                );
            }
        }

        #[test]
        fn refuses_two_lines_split_by_any_line_break() {
            for split in ['\n', '\x0b', '\x0c', '\r', '\u{85}', '\u{2028}', '\u{2029}']
            {
                let keys = string(&format!("{ALICE}{split}{}", ed25519([9; 32])));
                assert_eq!(
                    check(&subject(&[("keys", keys)])),
                    Err(vec![bad(at(0, 11), "the public key is more than one line")]),
                    "{split:?}"
                );
            }
        }

        #[test]
        fn refuses_the_first_bad_item_of_a_list_at_the_item() {
            let keys = list(&[string(ALICE), Kind::Integer(7), string("ssh-ed25519")]);
            assert_eq!(
                check(&subject(&[("keys", keys)])),
                Err(vec![bad(
                    at(0, 51),
                    "a public key is a string, not an integer"
                )])
            );
        }

        #[test]
        fn refuses_an_attribute_that_a_subject_does_not_have() {
            let documents =
                subject(&[("keys", string(ALICE)), ("name", string("Alice"))]);
            assert_eq!(
                check(&documents),
                Err(vec![refused(
                    "document.unknown-attribute",
                    at(0, 12),
                    "`name` is not an attribute of the `subject` block",
                    "Use `keys`, or remove it",
                )])
            );
        }

        #[test]
        fn refuses_a_block_inside_a_subject_with_any_attributes() {
            let cases = [
                vec![("keys", string(ALICE))],
                vec![],
                vec![("keys", Kind::List(Vec::new()))],
                vec![("keys", string("ssh-rsa AAAA"))],
                vec![("keys", string(ALICE)), ("name", string("Alice"))],
            ];
            for attributes in cases {
                assert_inner_blocks_refused("subject", &attributes);
            }
        }

        proptest! {
            #[test]
            fn reads_distinct_keys_in_any_order(
                (sorted, keys) in prop::collection::btree_set(any::<[u8; 32]>(), 1..6)
                    .prop_filter_map("a key of small order", |keys| {
                        let valid = keys.iter().all(|&key| PublicKey::new(key).is_ok());
                        valid.then(|| keys.into_iter().collect::<Vec<_>>())
                    })
                    .prop_flat_map(|sorted| {
                        (Just(sorted.clone()), Just(sorted).prop_shuffle())
                    }),
            ) {
                let items: Vec<_> =
                    keys.iter().map(|&key| string(&ed25519(key))).collect();
                let documents = subject(&[("keys", list(&items))]);
                prop_assert_eq!(check(&documents), Ok(keyed(&sorted)));
            }
        }
    }
}
