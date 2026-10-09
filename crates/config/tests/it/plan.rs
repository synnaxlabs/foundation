//! `config::plan` against applied specs that the tests build and apply.

use std::collections::{BTreeMap, BTreeSet};
use std::slice;

use config::plan::Plan;
use config::{Definition, Entry};
use connector::cancel;
use connector::kind::{self, Channels, Context, Kind, Table};
use document::diagnostic::Diagnostic;
use document::encoding::Checked;
use document::{Document, Source, read as reader};
use spec::Pointer;
use spec::channel::Channel;
use spec::compression::{self, Mode};
use spec::definition::Definition as Stored;
use spec::time::{self, Peers};
use spec::tree::{self, Chunks};
use types::channel::Key;
use types::digest::Digest;
use types::ed25519::PrivateKey;
use types::name::{Name, Selector};

mod codec;

const EDGE: &str = include_str!("../../../acceptance/tests/it/fixtures/edge.hcl");
const INFLUX: &str = include_str!("../../../acceptance/tests/it/fixtures/influx.hcl");

/// One index, placed on `n`, and a data channel on it.
const PLANT: &str = "\
channel \"a.time\" {
  kind = \"index\"
}
channel \"a.value\" {
  data_type = \"f64\"
  index = \"a.time\"
}
placement \"a\" {
  select = \"a.*\"
  home = \"n\"
}
";

/// A kind whose config is one attribute, `writes`: the channels it writes to the mesh.
struct Writer;

impl Kind for Writer {
    type Config = Vec<Name>;

    fn parse(&self, config: &Document) -> Result<Vec<Name>, Vec<Diagnostic>> {
        let writes = config
            .attributes
            .get("writes")
            .expect("a `writes` attribute");
        reader::names(&writes.value).map_err(|diagnostic| vec![diagnostic])
    }

    fn check(&self, writes: &Vec<Name>) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels {
            reads: Vec::new(),
            writes: writes.clone(),
        })
    }

    fn discover(
        &self,
        _: &cancel::Token,
    ) -> impl Future<Output = Result<Vec<Document>, kind::Error>> {
        std::future::ready(Ok(Vec::new()))
    }

    fn run(
        &self,
        _: Context<Vec<Name>>,
    ) -> impl Future<Output = Result<(), kind::Error>> {
        std::future::ready(Ok(()))
    }
}

/// An applied spec: its pointer and each chunk of its tree.
struct Spec {
    pointer: Pointer,
    chunks: Chunks,
    /// The keys made so far.
    made: u128,
}

impl Spec {
    fn create_empty() -> Self {
        Self {
            pointer: Pointer {
                version: 0,
                root: tree::empty(),
            },
            chunks: Chunks::default(),
            made: 0,
        }
    }

    /// Plans `texts` on the nodes `members`, and asserts that `check` agrees.
    fn plan(&self, texts: &[&str], members: &[&str]) -> Result<Plan, Vec<Diagnostic>> {
        let members = members.iter().map(|member| name(member)).collect();
        let applied = self.definitions();
        let documents = documents(texts);
        let planned =
            config::plan::plan(&documents, self.pointer, &applied, &members, &kinds());
        agrees(&documents, &applied, &planned, &members);
        planned
    }

    /// Each stored definition, by tree key.
    fn definitions(&self) -> BTreeMap<Name, Stored> {
        let all = tree::diff(&self.chunks, tree::empty(), self.pointer.root);
        let all = all.expect("a whole tree").changes.into_iter();
        all.map(|changed| {
            let bytes = changed.new.expect("an entry");
            (changed.name, Stored::decode(bytes).expect("decodes"))
        })
        .collect()
    }

    /// Changes the tree as one apply.
    fn set(&mut self, changes: impl IntoIterator<Item = tree::Change>) {
        let update = tree::apply(&mut self.chunks, self.pointer.root, changes);
        self.pointer = Pointer {
            version: self.pointer.version + 1,
            root: update.expect("a whole tree").root,
        };
    }

    /// The stored bytes of `key`.
    fn get(&self, key: &str) -> &[u8] {
        let bytes = tree::get(&self.chunks, self.pointer.root, &name(key));
        bytes.expect("a whole tree").expect("a stored definition")
    }

    /// Applies `plan`: a channel keeps the stored key at its name, and each other
    /// channel gets a new v7 key.
    fn apply(&mut self, plan: &Plan) {
        let mut keys: BTreeMap<Name, Key> = BTreeMap::new();
        for (name, stored) in self.definitions() {
            if let Stored::Channel(channel) = stored {
                keys.insert(name, channel.key);
            }
        }
        for (name, change) in &plan.changes {
            let channel = change.new.as_ref().is_some_and(|entry| {
                matches!(entry.definition, Definition::Channel(_))
            });
            if !channel {
                keys.remove(name);
            } else if change.old.is_none() || !keys.contains_key(name) {
                self.made += 1;
                // Version 7, as each stored key is.
                keys.insert(name.clone(), Key::from_u128((7 << 76) | self.made));
            }
        }
        let changes: Vec<_> = plan
            .changes
            .iter()
            .map(|(name, change)| match &change.new {
                None => tree::Change::Delete(name.clone()),
                Some(entry) => {
                    tree::Change::Set(name.clone(), encode(name, entry, &keys))
                }
            })
            .collect();
        self.set(changes);
    }
}

/// Asserts that `config::plan::check` gives each problem of `planned` but
/// `config.wrong-channel`, with no span, when `config::check` finds none in
/// `documents`. It checks the definitions of the plan, or, when `plan` refuses, the
/// definitions that the files make.
fn agrees(
    documents: &[Document],
    applied: &BTreeMap<Name, Stored>,
    planned: &Result<Plan, Vec<Diagnostic>>,
    members: &BTreeSet<Name>,
) {
    let Ok(entries) = config::check(documents, &kinds()) else {
        return;
    };
    let (definitions, mut expected) = match planned {
        Ok(plan) => {
            let mut made = 0;
            let key = || {
                made += 1;
                Key::from_u128((1 << 100) | made)
            };
            (
                plan.definitions(applied, key).expect("definitions"),
                Vec::new(),
            )
        }
        Err(diagnostics) => {
            let rules = diagnostics.iter().cloned();
            let rules =
                rules.filter(|found| found.code.as_str() != "config.wrong-channel");
            (made(&entries), problems(Err(rules.collect())))
        }
    };
    let checked = config::plan::check(&definitions, members, &kinds());
    let mut found = match checked {
        Ok(()) => Vec::new(),
        Err(diagnostics) => problems(Err(diagnostics)),
    };
    for problem in &mut expected {
        problem.1 = None;
    }
    expected.sort();
    found.sort();
    assert_eq!(found, expected);
}

/// The definitions of `entries`. Each channel has the key of its place in name order,
/// and an edge to a name that no entry holds has the key 0.
fn made(entries: &BTreeMap<Name, Entry>) -> BTreeMap<Name, Stored> {
    let keys: BTreeMap<_, _> = entries.keys().zip(1..).collect();
    let key = |name: &Name| Key::from_u128(keys.get(name).copied().unwrap_or(0));
    let made = entries.iter().map(|(name, entry)| {
        let definition = match &entry.definition {
            Definition::Spec(definition) => definition.clone(),
            Definition::Channel(kind) => Stored::Channel(Channel {
                key: key(name),
                kind: kind.clone().map(|to| key(&to)),
            }),
            definition => panic!("a new kind of definition: {definition:?}"),
        };
        (name.clone(), definition)
    });
    made.collect()
}

fn encode(name: &Name, entry: &Entry, keys: &BTreeMap<Name, Key>) -> Vec<u8> {
    match &entry.definition {
        Definition::Spec(definition) => definition.encode(),
        Definition::Channel(kind) => {
            let channel = Channel {
                key: keys[name],
                kind: kind.clone().map(|to| keys[&to]),
            };
            Stored::Channel(channel).encode()
        }
        definition => panic!("a new kind of definition: {definition:?}"),
    }
}

/// A kind whose one attribute, `writes`, names the channels that it reads from the mesh
/// and writes to its device.
struct Commander;

impl Kind for Commander {
    type Config = Vec<Name>;

    fn parse(&self, config: &Document) -> Result<Vec<Name>, Vec<Diagnostic>> {
        Writer.parse(config)
    }

    fn check(&self, writes: &Vec<Name>) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels {
            reads: writes.clone(),
            writes: Vec::new(),
        })
    }

    fn discover(
        &self,
        _: &cancel::Token,
    ) -> impl Future<Output = Result<Vec<Document>, kind::Error>> {
        std::future::ready(Ok(Vec::new()))
    }

    fn run(
        &self,
        _: Context<Vec<Name>>,
    ) -> impl Future<Output = Result<(), kind::Error>> {
        std::future::ready(Ok(()))
    }
}

fn read(source: u32, text: &str) -> Document {
    config_hcl::read(Source(source), text).expect("the text is HCL")
}

/// One Document for each text, with its index as its source.
fn documents(texts: &[&str]) -> Vec<Document> {
    texts
        .iter()
        .zip(0..)
        .map(|(text, source)| read(source, text))
        .collect()
}

fn kinds() -> Table {
    Table::new()
        .with("influx", connector_influx::Kind::default())
        .with("writer", Writer)
        .with("commander", Commander)
}

fn name(text: &str) -> Name {
    text.parse().expect("a name")
}

/// Each change as its tree key, its old digest, and its new entry.
fn changes(plan: &Plan) -> Vec<(&Name, Option<Digest>, Option<&Entry>)> {
    let changes = plan.changes.iter();
    changes
        .map(|(name, change)| (name, change.old, change.new.as_ref()))
        .collect()
}

/// A problem as its code, its source and offset, its message, and its fix.
type Problem = (&'static str, Option<(Source, u32)>, String, String);

fn problems(result: Result<Plan, Vec<Diagnostic>>) -> Vec<Problem> {
    let diagnostics = result.expect_err("problems");
    diagnostics
        .into_iter()
        .map(|diagnostic| {
            let span = diagnostic
                .span
                .map(|span| (span.source(), span.start().offset));
            (
                diagnostic.code.as_str(),
                span,
                diagnostic.message,
                diagnostic.fix,
            )
        })
        .collect()
}

fn problem(code: &'static str, at: (u32, u32), message: &str, fix: &str) -> Problem {
    (code, Some((Source(at.0), at.1)), message.into(), fix.into())
}

fn offset(text: &str, needle: &str) -> u32 {
    let offset = text.find(needle).expect("the needle is in the text");
    u32::try_from(offset).expect("a short text")
}

/// The offset of `value` in the first attribute `key = value` of `text`.
fn value(text: &str, key: &str, value: &str) -> u32 {
    let attribute = format!("{key} = {value}");
    offset(text, &attribute) + offset(&attribute, value)
}

/// The offset of the label of the first block that `label` names.
fn label(text: &str, label: &str) -> u32 {
    offset(text, &format!("\"{label}\" {{"))
}

#[test]
fn adds_each_definition_of_the_fixtures_to_the_empty_spec() {
    let spec = Spec::create_empty();
    let plan = spec
        .plan(&[EDGE, INFLUX], &["cloud", "edge"])
        .expect("no problems");
    let entries = config::check(&documents(&[EDGE, INFLUX]), &kinds()).expect("valid");
    let added: Vec<_> = entries
        .iter()
        .map(|(key, entry)| (key, None, Some(entry)))
        .collect();
    assert_eq!(changes(&plan), added);
    let influx = plan
        .changes
        .values()
        .last()
        .and_then(|change| change.new.as_ref());
    let influx = influx.and_then(|entry| entry.label_span);
    assert_eq!(influx.map(document::Span::source), Some(Source(1)));
    assert_eq!(
        plan.homes,
        BTreeMap::from([(name("edge.time"), name("edge"))])
    );
    assert_eq!(plan.base, spec.pointer);
}

#[test]
fn plans_no_change_after_its_apply() {
    let mut spec = Spec::create_empty();
    let members = ["cloud", "edge"];
    spec.apply(&spec.plan(&[EDGE, INFLUX], &members).expect("no problems"));
    let plan = spec.plan(&[EDGE, INFLUX], &members).expect("no problems");
    assert_eq!(changes(&plan), []);
    let homes = BTreeMap::from([(name("edge.time"), name("edge"))]);
    assert_eq!(plan.homes, homes);
    assert_eq!(plan.base.version, 1);
    assert_eq!(plan.base, spec.pointer);
}

#[test]
fn changes_a_definition_with_its_old_digest() {
    let mut spec = Spec::create_empty();
    spec.apply(&spec.plan(&[EDGE], &["edge"]).expect("no problems"));
    let edge = EDGE.replace("\"f64\"", "\"f32\"");
    let plan = spec.plan(&[&edge], &["edge"]).expect("no problems");
    let entries = config::check(&documents(&[&edge]), &kinds()).expect("valid");
    let value = name("edge.value");
    let old = Digest::of(spec.get("edge.value"));
    assert_eq!(
        changes(&plan),
        [(&value, Some(old), Some(&entries[&value]))]
    );
}

#[test]
fn removes_a_definition_that_no_file_holds() {
    let mut spec = Spec::create_empty();
    let members = ["cloud", "edge"];
    spec.apply(&spec.plan(&[EDGE, INFLUX], &members).expect("no problems"));
    let plan = spec.plan(&[EDGE], &members).expect("no problems");
    let old = Digest::of(spec.get("influx"));
    assert_eq!(changes(&plan), [(&name("influx"), Some(old), None)]);
}

#[test]
fn keeps_a_stored_definition_whose_kind_no_block_defines() {
    let mut spec = Spec::create_empty();
    let select = Selector::new(["a.**"]).expect("a selector");
    let time = Stored::Time(time::Policy::new(select.clone(), Peers::Voters));
    let mode = Mode::Raw;
    let compression = Stored::Compression(compression::Policy { select, mode });
    spec.set([time, compression].map(|definition| {
        let key = definition.kind().key("a").expect("a key");
        tree::Change::Set(key, definition.encode())
    }));
    assert_eq!(changes(&spec.plan(&[], &[]).expect("no problems")), []);
}

#[test]
fn changes_each_channel_on_a_renamed_index() {
    let mut spec = Spec::create_empty();
    spec.apply(&spec.plan(&[EDGE], &["edge"]).expect("no problems"));
    let edge = EDGE.replace("\"edge.time\"", "\"edge.clock\"");
    let plan = spec.plan(&[&edge], &["edge"]).expect("no problems");
    let found: Vec<_> = plan
        .changes
        .iter()
        .map(|(name, change)| {
            (name.as_str(), change.old.is_some(), change.new.is_some())
        })
        .collect();
    let expected = [
        ("edge.clock", false, true),
        ("edge.time", true, false),
        ("edge.time_error", true, true),
        ("edge.value", true, true),
    ];
    assert_eq!(found, expected);
    assert_eq!(
        plan.homes,
        BTreeMap::from([(name("edge.clock"), name("edge"))])
    );
}

#[test]
fn gives_a_new_channel_a_key_that_no_stored_channel_holds() {
    let mut spec = Spec::create_empty();
    let index = spec::channel::Kind::Index {
        error: None,
        control: None,
    };
    let key = Key::from_u128(1);
    let stored = Stored::Channel(Channel { key, kind: index });
    spec.set([tree::Change::Set(name("b.time"), stored.encode())]);
    let b = "\
channel \"b.time\" {
  kind = \"index\"
}
placement \"b\" {
  select = \"b.*\"
  home = \"n\"
}
";
    let plan = spec.plan(&[PLANT, b], &["n"]).expect("no problems");
    let found: Vec<_> = plan
        .changes
        .iter()
        .map(|(name, change)| (name.as_str(), change.old))
        .collect();
    let added = ["a.@placement", "a.time", "a.value", "b.@placement"];
    assert_eq!(found, added.map(|name| (name, None)));
    let homes = [(name("a.time"), name("n")), (name("b.time"), name("n"))];
    assert_eq!(plan.homes, BTreeMap::from(homes));
}

#[test]
fn gives_only_the_problems_of_check_when_a_file_has_one() {
    let text = PLANT.replace("index = \"a.time\"", "index = \"a.clock\"");
    let found = Spec::create_empty()
        .plan(&[&text], &["n"])
        .expect_err("problems");
    let checked = config::check(&documents(&[&text]), &kinds()).expect_err("problems");
    assert_eq!(found, checked);
    assert_eq!(found.len(), 1, "{found:?}");
}

#[test]
fn leaves_out_the_founding_definitions() {
    let mut spec = Spec::create_empty();
    let founding = spec::founding::create(PrivateKey([7; 32]).public());
    spec.set(
        founding
            .into_iter()
            .map(|(key, definition)| tree::Change::Set(key, definition.encode())),
    );
    let plan = spec.plan(&[], &[]).expect("no problems");
    assert_eq!(changes(&plan), []);
    let plan = spec.plan(&[PLANT], &["n"]).expect("no problems");
    let keys: Vec<_> = plan.changes.keys().map(Name::as_str).collect();
    assert_eq!(keys, ["a.@placement", "a.time", "a.value"]);
}

#[test]
fn refuses_an_edge_to_a_channel_that_is_not_what_it_needs() {
    let other =
        "channel \"a.other\" {\n  data_type = \"f64\"\n  index = \"a.value\"\n}\n";
    let text = format!("{PLANT}{other}");
    let found = problems(Spec::create_empty().plan(&[&text], &["n"]));
    let expected = problem(
        "config.wrong-channel",
        (0, value(&text, "index", "\"a.value\"")),
        "the index channel of `a.other` is `a.value`, which is not an index channel",
        "Point it at an index channel",
    );
    assert_eq!(found, [expected]);
}

#[test]
fn places_a_data_channel_that_becomes_an_index() {
    let mut spec = Spec::create_empty();
    spec.apply(&spec.plan(&[PLANT], &["n"]).expect("no problems"));
    let text = PLANT.replace(
        "data_type = \"f64\"\n  index = \"a.time\"",
        "kind = \"index\"",
    );
    let plan = spec.plan(&[&text], &["n"]).expect("no problems");
    let keys: Vec<_> = plan.changes.keys().map(Name::as_str).collect();
    assert_eq!(keys, ["a.value"]);
    let homes = [(name("a.time"), name("n")), (name("a.value"), name("n"))];
    assert_eq!(plan.homes, BTreeMap::from(homes));
}

#[test]
fn takes_the_home_of_an_index_from_the_node_of_its_writers() {
    let text = "\
channel \"a.time\" {
  kind = \"index\"
}
channel \"a.value\" {
  data_type = \"f64\"
  index = \"a.time\"
}
connector \"w1\" {
  kind = \"writer\"
  node = \"w\"
  writes = [\"a.value\"]
}
connector \"w2\" {
  kind = \"writer\"
  node = \"w\"
  writes = [\"a.time\"]
}
";
    let plan = Spec::create_empty()
        .plan(&[text], &["w"])
        .expect("no problems");
    assert_eq!(plan.homes, BTreeMap::from([(name("a.time"), name("w"))]));
}

#[test]
fn makes_no_writer_of_a_connector_that_writes_to_its_device() {
    let text = format!(
        "{PLANT}\
connector \"c1\" {{
  kind = \"commander\"
  node = \"n1\"
  writes = [\"a.value\"]
}}
connector \"c2\" {{
  kind = \"commander\"
  node = \"n2\"
  writes = [\"a.value\"]
}}
"
    );
    let plan = Spec::create_empty()
        .plan(&[&text], &["n", "n1", "n2"])
        .expect("no problems");
    assert_eq!(plan.homes, BTreeMap::from([(name("a.time"), name("n"))]));
}

/// The fix of `config.unplaced` when the node of a connector has a second role in the
/// placement that wins for the name.
const OVERLAP: &str = "Move the node to `home` when it is the one node of the \
                       placement, else remove it from the placement";

#[test]
fn refuses_each_index_that_cannot_be_placed() {
    let text = "\
channel \"b.time\" {
  kind = \"index\"
}
channel \"c.time\" {
  kind = \"index\"
}
placement \"c\" {
  select = \"c.*\"
  standby = \"n\"
}
channel \"d.time\" {
  kind = \"index\"
}
placement \"d_1\" {
  select = \"d.*\"
  home = \"n\"
}
placement \"d_2\" {
  select = \"d.*\"
  home = \"m\"
}
channel \"e.time\" {
  kind = \"index\"
}
placement \"e\" {
  select = \"e.*\"
  standby = \"n\"
}
connector \"w\" {
  kind = \"writer\"
  node = \"n\"
  writes = [\"e.time\"]
}
";
    let found = problems(Spec::create_empty().plan(&[text], &["m", "n"]));
    let unplaced = |index, message: &str, fix: &str| {
        problem("config.unplaced", (0, label(text, index)), message, fix)
    };
    let expected = [
        unplaced(
            "b.time",
            "no placement selects the index, and no connector writes it",
            "Select the index with a placement that names a `home`, or write it with a \
             connector",
        ),
        unplaced(
            "c.time",
            "the placement `c` wins for the index and names no home, and no connector \
             writes the index",
            "Name a `home` in the placement, or write the index with a connector",
        ),
        unplaced(
            "d.time",
            "the placements `d_1` and `d_2` select the name with the same specificity",
            "Change the `select` of one of the two placements, so that one selects the \
             name more specifically",
        ),
        unplaced(
            "e.time",
            "the node `n` of a connector is the home and has another role in the \
             placement `e`",
            OVERLAP,
        ),
    ];
    assert_eq!(found, expected);
}

#[test]
fn refuses_each_node_that_is_not_a_member() {
    let text = "\
channel \"a.time\" {
  kind = \"index\"
}
placement \"a\" {
  select = \"a.*\"
  home = \"Edge\"
  standby = \"@n\"
  copies = [\"edge_2\", \"cloud\"]
}
connector \"w\" {
  kind = \"writer\"
  node = \"plc\"
  writes = []
}
";
    let found = problems(Spec::create_empty().plan(&[text], &["cloud", "edge", "n"]));
    let unknown = |key, node: &str, fix: &str| {
        let message = format!("no node of the mesh is named `{node}`");
        let at = value(text, key, &format!("\"{node}\""));
        problem("config.unknown-node", (0, at), &message, fix)
    };
    let edge_2 = offset(text, "\"edge_2\"");
    let expected = [
        unknown("home", "Edge", "Write `edge`, the name of the node"),
        unknown("standby", "@n", "Name a node of the mesh"),
        problem(
            "config.unknown-node",
            (0, edge_2),
            "no node of the mesh is named `edge_2`",
            "Name a node of the mesh",
        ),
        unknown("node", "plc", "Name a node of the mesh"),
    ];
    assert_eq!(found, expected);
}

#[test]
fn refuses_an_index_that_connectors_on_two_nodes_write() {
    let text = format!(
        "{PLANT}\
connector \"w1\" {{
  kind = \"writer\"
  node = \"n1\"
  writes = [\"a.value\"]
}}
connector \"w3\" {{
  kind = \"writer\"
  node = \"n1\"
  writes = [\"a.value\"]
}}
connector \"w2\" {{
  kind = \"writer\"
  node = \"n2\"
  writes = [\"a.time\"]
}}
"
    );
    let found = problems(Spec::create_empty().plan(&[&text], &["n", "n1", "n2"]));
    let expected = problem(
        "config.writer-nodes",
        (0, value(&text, "node", "\"n2\"")),
        "connectors on the nodes `n1` and `n2` write the index `a.time`, so it has no \
         one home",
        "Run each connector that writes `a.time` on one node",
    );
    assert_eq!(found, [expected]);
}

#[test]
fn refuses_a_placement_that_names_another_home_for_a_connector() {
    let text = "\
channel \"a.time\" {
  kind = \"index\"
}
placement \"a\" {
  select = \"a.**\"
  home = \"m\"
}
connector \"a\" {
  kind = \"writer\"
  node = \"n\"
  writes = [\"a.time\"]
}
";
    let found = problems(Spec::create_empty().plan(&[text], &["m", "n"]));
    let expected = problem(
        "config.connector-home",
        (0, value(text, "home", "\"m\"")),
        "the placement `a` names the home `m`, but the connector `a` runs on the node \
         `n`",
        "Name `n` as the `home`, and keep `n` out of `standby` and `copies`",
    );
    assert_eq!(found, [expected]);
}

/// The definitions of one connector, `rogue`, of `kind` on the node `nowhere`, with
/// `config`.
fn rogue(kind: &str, config: &str) -> BTreeMap<Name, Stored> {
    let config = Checked::new(read(0, config)).expect("a shallow config");
    let connector =
        spec::connector::Connector::new(name(kind), name("nowhere"), config);
    BTreeMap::from([(name("rogue"), Stored::Connector(connector))])
}

#[test]
fn check_refuses_only_a_connector_of_a_kind_that_the_build_lacks() {
    let members = BTreeSet::from([name("n")]);
    let select = Selector::new(["rogue"]).expect("a selector");
    let nodes = spec::placement::Nodes {
        home: Some(name("n")),
        ..spec::placement::Nodes::default()
    };
    let policy = spec::placement::Policy::new(select, nodes).expect("a policy");
    let mut definitions = rogue("nothing", "");
    definitions.insert(name("rogue.@placement"), Stored::Placement(policy));
    let found = config::plan::check(&definitions, &members, &kinds());
    let expected = (
        "connector.unknown-kind",
        None,
        "this build has no connector kind \"nothing\"".into(),
        "Use one of [\"commander\", \"influx\", \"writer\"]".into(),
    );
    assert_eq!(problems(found.map(|()| unreachable())), [expected]);
}

#[test]
fn check_refuses_only_a_config_that_its_kind_refuses_as_plan_does() {
    let text = "\
connector \"rogue\" {
  kind = \"writer\"
  node = \"nowhere\"
  writes = 1
}
";
    let planned = problems(Spec::create_empty().plan(&[text], &["n"]));
    let members = BTreeSet::from([name("n")]);
    let found = config::plan::check(&rogue("writer", "writes = 1"), &members, &kinds());
    let found = problems(found.map(|()| unreachable()));
    let text = |problems: Vec<Problem>| {
        let problems = problems.into_iter();
        problems
            .map(|(code, _, message, fix)| (code, message, fix))
            .collect::<Vec<_>>()
    };
    let codes: Vec<_> = planned.iter().map(|problem| problem.0).collect();
    assert_eq!(codes, ["document.bad-name"]);
    assert_eq!(text(found), text(planned));
}

/// A plan for `problems` to refuse, which a `check` that refuses never gives.
fn unreachable() -> Plan {
    unreachable!("check refuses")
}

#[test]
fn names_the_nodes_of_writer_nodes_in_the_order_of_the_connector_names() {
    let text = format!(
        "{PLANT}\
connector \"w2\" {{
  kind = \"writer\"
  node = \"n2\"
  writes = [\"a.time\"]
}}
connector \"w1\" {{
  kind = \"writer\"
  node = \"n1\"
  writes = [\"a.value\"]
}}
"
    );
    let members = ["n", "n1", "n2"];
    let planned = problems(Spec::create_empty().plan(&[&text], &members));
    let entries = config::check(&documents(&[&text]), &kinds()).expect("entries");
    let members = members.iter().map(|member| name(member)).collect();
    let found = config::plan::check(&made(&entries), &members, &kinds());
    let message = |first, second| {
        format!(
            "connectors on the nodes `{first}` and `{second}` write the index \
             `a.time`, so it has no one home"
        )
    };
    let fix = "Run each connector that writes `a.time` on one node";
    let at = (0, value(&text, "node", "\"n2\""));
    let expected = problem("config.writer-nodes", at, &message("n1", "n2"), fix);
    assert_eq!(planned, [expected]);
    let expected = ("config.writer-nodes", None, message("n1", "n2"), fix.into());
    assert_eq!(problems(found.map(|()| unreachable())), [expected]);
}

/// Asserts that `refused` gives each code and fix of `expected`, and that `fixed`, the
/// text after the fixes, plans, each on the nodes `k`, `m`, and `n`.
fn plans_after(refused: &str, expected: &[(&str, String)], fixed: &str) {
    let nodes = ["k", "m", "n"];
    let found = problems(Spec::create_empty().plan(&[refused], &nodes));
    let found: Vec<_> = found.into_iter().map(|p| (p.0, p.3)).collect();
    assert_eq!(found, expected, "{refused}");
    let plan = Spec::create_empty().plan(&[fixed], &nodes);
    assert!(plan.is_ok(), "{fixed}: {plan:?}");
}

/// Asserts that `refused` gives `config.connector-home` with each of `texts` as its
/// fix, and that `fixed`, the text after the fixes, plans.
fn plans_after_connector_home(refused: &str, texts: &[String], fixed: &str) {
    let expected: Vec<_> = texts
        .iter()
        .map(|fix| ("config.connector-home", fix.clone()))
        .collect();
    plans_after(refused, &expected, fixed);
}

const RENAMED: &str =
    "Name `n` as the `home`, and keep `n` out of `standby` and `copies`";

#[test]
fn plans_a_connector_after_the_connector_home_fix() {
    let text = |placement: &str| {
        format!(
            "\
connector \"a\" {{
  kind = \"writer\"
  node = \"n\"
  writes = []
}}
placement \"a\" {{
  select = \"a\"
{placement}}}
"
        )
    };
    let cases = [
        ("  home = \"m\"\n", "  home = \"n\"\n"),
        ("  home = \"m\"\n  standby = \"n\"\n", "  home = \"n\"\n"),
        (
            "  home = \"m\"\n  copies = [\"n\", \"k\"]\n",
            "  home = \"n\"\n  copies = [\"k\"]\n",
        ),
    ];
    for (refused, fixed) in cases {
        plans_after_connector_home(&text(refused), &[RENAMED.into()], &text(fixed));
    }
}

/// The fix of each diagnostic of `connector` on `node` when its placement wins for a
/// connector on another node, where `winners` lists each placement that wins for
/// `connector` or one of its indexes.
fn exclude(connector: &str, winners: &str, node: &str) -> String {
    format!(
        "Exclude the connector `{connector}` and its indexes from the `select` of \
         {winners}, and select them with another placement whose `home` is `{node}`"
    )
}

/// The connectors `p.a` and `p.c` on `n` and `p.b` on `b`, the placement `p` of
/// `select` and `home`, then `more`.
fn two_nodes(b: &str, select: &str, home: &str, more: &str) -> String {
    format!(
        "\
channel \"p.a.time\" {{
  kind = \"index\"
}}
connector \"p.a\" {{
  kind = \"writer\"
  node = \"n\"
  writes = [\"p.a.time\"]
}}
connector \"p.b\" {{
  kind = \"writer\"
  node = \"{b}\"
  writes = []
}}
connector \"p.c\" {{
  kind = \"writer\"
  node = \"n\"
  writes = []
}}
placement \"p\" {{
  select = {select}
  home = \"{home}\"
}}
{more}"
    )
}

/// The placements that the fix of `two_nodes` adds for `p.a` and `p.c`.
const TWO_NODES_FIXED: &str = "\
placement \"a\" {
  select = [\"p.a\", \"p.a.time\"]
  home = \"n\"
}
placement \"c\" {
  select = \"p.c\"
  home = \"n\"
}
";

#[test]
fn plans_connectors_after_the_connector_home_fix_of_a_placement_of_two_nodes() {
    let all = "\"p.**\"";
    plans_after_connector_home(
        &two_nodes("m", all, "m", ""),
        &[exclude("p.a", "`p`", "n"), exclude("p.c", "`p`", "n")],
        &two_nodes(
            "m",
            "[\"p.**\", \"!p.a\", \"!p.a.time\", \"!p.c\"]",
            "m",
            TWO_NODES_FIXED,
        ),
    );
    plans_after_connector_home(
        &two_nodes("n", all, "m", ""),
        &[RENAMED.into(), RENAMED.into(), RENAMED.into()],
        &two_nodes("n", all, "n", ""),
    );
}

#[test]
fn plans_connectors_after_the_connector_home_fix_of_a_placement_at_a_third_home() {
    let b = "\
placement \"b\" {
  select = \"p.b\"
  home = \"m\"
}
";
    plans_after_connector_home(
        &two_nodes("m", "\"p.**\"", "k", ""),
        &[
            exclude("p.a", "`p`", "n"),
            exclude("p.b", "`p`", "m"),
            exclude("p.c", "`p`", "n"),
        ],
        &two_nodes(
            "m",
            "[\"p.**\", \"!p.a\", \"!p.a.time\", \"!p.b\", \"!p.c\"]",
            "k",
            &(TWO_NODES_FIXED.to_owned() + b),
        ),
    );
}

#[test]
fn plans_after_the_connector_home_fix_of_a_connector_that_its_placement_names() {
    let text = |select: &str, more: &str| {
        format!(
            "\
channel \"x.time\" {{
  kind = \"index\"
}}
connector \"x\" {{
  kind = \"writer\"
  node = \"n\"
  writes = [\"x.time\"]
}}
connector \"y\" {{
  kind = \"writer\"
  node = \"m\"
  writes = []
}}
placement \"p\" {{
  select = [{select}]
  home = \"m\"
}}
{more}"
        )
    };
    let fixed = "\
placement \"x\" {
  select = [\"x\", \"x.time\"]
  home = \"n\"
}
";
    plans_after_connector_home(
        &text("\"x\", \"x.*\", \"y\"", ""),
        &[exclude("x", "`p`", "n")],
        &text("\"y\"", fixed),
    );
}

#[test]
fn plans_nested_connectors_on_two_nodes_after_the_connector_home_fix() {
    let text = |home: &str, select: &str, more: &str| {
        format!(
            "\
channel \"d.e.time\" {{
  kind = \"index\"
}}
connector \"d\" {{
  kind = \"writer\"
  node = \"n\"
  writes = []
}}
connector \"d.e\" {{
  kind = \"writer\"
  node = \"m\"
  writes = [\"d.e.time\"]
}}
placement \"d\" {{
  select = [{select}]
  home = \"{home}\"
}}
{more}"
        )
    };
    let e = "\
placement \"e\" {
  select = [\"d.e\", \"d.e.time\"]
  home = \"m\"
}
";
    plans_after_connector_home(
        &text("n", "\"d.**\"", ""),
        &[exclude("d.e", "`d`", "m")],
        &text("n", "\"d.**\", \"!d.e\", \"!d.e.time\"", e),
    );
    let d = "\
placement \"on_d\" {
  select = \"d\"
  home = \"n\"
}
";
    plans_after_connector_home(
        &text("m", "\"d.**\"", ""),
        &[exclude("d", "`d`", "n")],
        &text("m", "\"d.**\", \"!d\"", d),
    );
}

#[test]
fn plans_nested_connectors_with_no_inner_index_after_the_connector_home_fix() {
    let text = |channel: &str, placement: &str, more: &str| {
        format!(
            "\
{channel}connector \"d\" {{
  kind = \"writer\"
  node = \"n\"
  writes = []
}}
connector \"d.e\" {{
  kind = \"writer\"
  node = \"m\"
  writes = []
}}
placement \"d\" {{
{placement}}}
{more}"
        )
    };
    let e = "\
placement \"e\" {
  select = \"d.e\"
  home = \"m\"
}
";
    let outer = "channel \"d.f.time\" {\n  kind = \"index\"\n}\n";
    for (channel, roles) in [
        ("", "  home = \"n\"\n"),
        (outer, "  home = \"n\"\n  standby = \"k\"\n"),
    ] {
        plans_after_connector_home(
            &text(channel, &format!("  select = \"d.**\"\n{roles}"), ""),
            &[exclude("d.e", "`d`", "m")],
            &text(
                channel,
                &format!("  select = [\"d.**\", \"!d.e\"]\n{roles}"),
                e,
            ),
        );
    }
}

#[test]
fn places_a_connector_at_its_node_with_its_placement() {
    let text = "\
channel \"a.time\" {
  kind = \"index\"
}
placement \"a\" {
  select = \"a.**\"
  home = \"n\"
}
connector \"a\" {
  kind = \"writer\"
  node = \"n\"
  writes = [\"a.time\"]
}
channel \"b.time\" {
  kind = \"index\"
}
placement \"b\" {
  select = \"b.**\"
  standby = \"m\"
}
connector \"b\" {
  kind = \"writer\"
  node = \"n\"
  writes = [\"b.time\"]
}
channel \"c.time\" {
  kind = \"index\"
}
connector \"c\" {
  kind = \"writer\"
  node = \"m\"
  writes = [\"c.time\"]
}
";
    let plan = Spec::create_empty()
        .plan(&[text], &["m", "n"])
        .expect("no problems");
    let homes = [("a.time", "n"), ("b.time", "n"), ("c.time", "m")];
    let homes = homes.map(|(index, home)| (name(index), name(home)));
    assert_eq!(plan.homes, BTreeMap::from(homes));
}

#[test]
fn refuses_each_connector_that_cannot_be_placed() {
    let text = "\
placement \"c_1\" {
  select = \"c\"
  home = \"n\"
}
placement \"c_2\" {
  select = \"c\"
  standby = \"m\"
}
connector \"c\" {
  kind = \"writer\"
  node = \"n\"
  writes = []
}
placement \"on_d\" {
  select = \"d\"
  standby = \"n\"
}
connector \"d\" {
  kind = \"writer\"
  node = \"n\"
  writes = []
}
";
    let found = problems(Spec::create_empty().plan(&[text], &["m", "n"]));
    let expected = [
        problem(
            "config.unplaced",
            (0, label(text, "c")),
            "the placements `c_1` and `c_2` select the name with the same specificity",
            "Change the `select` of one of the two placements, so that one selects the \
             name more specifically",
        ),
        problem(
            "config.unplaced",
            (0, label(text, "d")),
            "the node `n` of a connector is the home and has another role in the \
             placement `on_d`",
            OVERLAP,
        ),
    ];
    assert_eq!(found, expected);
}

/// A `config.split-placement` problem at the label of `at` in `text`, whose fix
/// names `winner` and `connector`.
fn split(
    text: &str,
    at: &str,
    message: &str,
    winner: &str,
    connector: &str,
) -> Problem {
    problem(
        "config.split-placement",
        (0, label(text, at)),
        message,
        &win(winner, connector),
    )
}

/// The `config.split-placement` fix that makes `winner` win for `connector` and its
/// indexes.
fn win(winner: &str, connector: &str) -> String {
    format!(
        "Make the placement `{winner}` win for the connector `{connector}` and its \
         indexes"
    )
}

#[test]
fn refuses_an_index_whose_placement_is_not_its_connectors() {
    let text = "\
channel \"a.time\" {
  kind = \"index\"
}
placement \"a_time\" {
  select = \"a.time\"
  home = \"n\"
}
placement \"a\" {
  select = \"a.**\"
  home = \"n\"
}
connector \"a\" {
  kind = \"writer\"
  node = \"n\"
  writes = [\"a.time\"]
}
";
    let found = problems(Spec::create_empty().plan(&[text], &["n"]));
    let expected = split(
        text,
        "a_time",
        "the placement `a_time` wins for the index `a.time`, but the placement `a` \
         wins for the connector `a`",
        "a",
        "a",
    );
    assert_eq!(found, [expected]);
}

#[test]
fn checks_an_index_only_against_the_nearest_connector_above_it() {
    let text = |e: &str| {
        format!(
            "\
placement \"d\" {{
  select = \"d.**\"
  home = \"n\"
}}
placement \"e\" {{
  select = \"{e}\"
  home = \"m\"
}}
channel \"d.e.time\" {{
  kind = \"index\"
}}
connector \"d\" {{
  kind = \"writer\"
  node = \"n\"
  writes = []
}}
connector \"d.e\" {{
  kind = \"writer\"
  node = \"m\"
  writes = [\"d.e.time\"]
}}
"
        )
    };
    let nodes = ["m", "n"];
    let plan = Spec::create_empty()
        .plan(&[&text("d.e.**")], &nodes)
        .expect("no problems");
    assert_eq!(plan.homes, BTreeMap::from([(name("d.e.time"), name("m"))]));
    let text = text("d.e");
    let found = problems(Spec::create_empty().plan(&[&text], &nodes));
    let expected = split(
        &text,
        "d",
        "the placement `d` wins for the index `d.e.time`, but the placement `e` wins \
         for the connector `d.e`",
        "e",
        "d.e",
    );
    assert_eq!(found, [expected]);
}

#[test]
fn refuses_an_index_or_a_connector_that_no_placement_selects_when_the_other_is() {
    let text = "\
channel \"b.time\" {
  kind = \"index\"
}
placement \"only_b\" {
  select = \"b\"
  home = \"n\"
}
connector \"b\" {
  kind = \"writer\"
  node = \"n\"
  writes = [\"b.time\"]
}
channel \"c.time\" {
  kind = \"index\"
}
placement \"c\" {
  select = \"c.*\"
  home = \"n\"
}
connector \"c\" {
  kind = \"writer\"
  node = \"n\"
  writes = [\"c.time\"]
}
channel \"fx.time\" {
  kind = \"index\"
}
placement \"fx\" {
  select = \"fx.*\"
  home = \"n\"
}
connector \"f\" {
  kind = \"writer\"
  node = \"n\"
  writes = [\"fx.time\"]
}
";
    let found = problems(Spec::create_empty().plan(&[text], &["n"]));
    let expected = [
        split(
            text,
            "only_b",
            "no placement selects the index `b.time`, but the placement `only_b` wins \
             for the connector `b`",
            "only_b",
            "b",
        ),
        split(
            text,
            "c",
            "the placement `c` wins for the index `c.time`, but no placement selects \
             the connector `c`",
            "c",
            "c",
        ),
    ];
    assert_eq!(found, expected);
}

#[test]
fn plans_after_the_split_placement_fix() {
    let placement = |at: &str, select: &str| {
        format!("placement \"{at}\" {{\n  select = \"{select}\"\n  home = \"n\"\n}}\n")
    };
    let connector = |name: &str, writes: &str| {
        format!(
            "connector \"{name}\" {{\n  kind = \"writer\"\n  node = \"n\"\n  \
             writes = [{writes}]\n}}\n"
        )
    };
    let index = |name: &str| format!("channel \"{name}\" {{\n  kind = \"index\"\n}}\n");
    let a = index("a.time") + &connector("a", "\"a.time\"");
    let d = index("d.e.time") + &connector("d", "") + &connector("d.e", "\"d.e.time\"");
    let fixed_a = a.clone() + &placement("a", "a.**");
    let a_fix = win("a", "a");
    let cases = [
        (
            a.clone() + &placement("a_time", "a.time") + &placement("a", "a.**"),
            a_fix.clone(),
            &fixed_a,
        ),
        (
            a.clone() + &placement("a_time", "a.time") + &placement("a", "a"),
            a_fix.clone(),
            &fixed_a,
        ),
        (a.clone() + &placement("a", "a.*"), a_fix.clone(), &fixed_a),
        (a.clone() + &placement("a", "a"), a_fix, &fixed_a),
        (
            d.clone() + &placement("d", "d.**") + &placement("e", "d.e"),
            win("e", "d.e"),
            &(d.clone() + &placement("d", "d.**") + &placement("e", "d.e.**")),
        ),
    ];
    for (refused, fix, fixed) in cases {
        let found = problems(Spec::create_empty().plan(&[&refused], &["n"]));
        let found: Vec<_> = found.into_iter().map(|p| (p.0, p.3)).collect();
        assert_eq!(found, [("config.split-placement", fix)], "{refused}");
        let plan = Spec::create_empty().plan(&[fixed], &["n"]);
        assert!(plan.is_ok(), "{fixed}: {plan:?}");
    }
}

#[test]
fn plans_after_the_split_placement_fix_of_indexes_placed_at_another_home() {
    let text = |indexes: &[&str], excluded: bool| {
        let mut channels = Vec::new();
        let mut writes = Vec::new();
        let mut out = Vec::new();
        for index in indexes {
            channels.push(format!("channel \"{index}\" {{\n  kind = \"index\"\n}}\n"));
            writes.push(format!("\"{index}\""));
            out.push(format!("\"!{index}\""));
        }
        let (channels, writes) = (channels.concat(), writes.join(", "));
        let (select, a) = if excluded {
            let a = format!(
                "placement \"a\" {{\n  select = [\"a\", {writes}]\n  home = \"n\"\n}}\n"
            );
            (format!("{writes}, \"z\", {}", out.join(", ")), a)
        } else {
            (format!("{writes}, \"z\""), String::new())
        };
        format!(
            "\
{channels}connector \"a\" {{
  kind = \"writer\"
  node = \"n\"
  writes = [{writes}]
}}
connector \"z\" {{
  kind = \"writer\"
  node = \"m\"
  writes = []
}}
placement \"p\" {{
  select = [{select}]
  home = \"m\"
}}
{a}"
        )
    };
    let nodes = ["m", "n"];
    let fix = "Exclude the indexes of the connector `a` from the `select` of `p`, and \
               select the connector and its indexes with another placement whose \
               `home` is `n`";
    for indexes in [&["a.time"][..], &["a.time", "a.value"]] {
        let refused = text(indexes, false);
        let found = problems(Spec::create_empty().plan(&[&refused], &nodes));
        let found: Vec<_> = found.into_iter().map(|p| (p.0, p.3)).collect();
        let expected = vec![("config.split-placement", fix.to_owned()); indexes.len()];
        assert_eq!(found, expected, "{refused}");
        let fixed = text(indexes, true);
        let plan = Spec::create_empty().plan(&[&fixed], &nodes);
        assert!(plan.is_ok(), "{fixed}: {plan:?}");
    }
}

#[test]
fn plans_after_the_split_placement_fix_of_indexes_of_more_than_one_placement() {
    let text = |placed: &[(&str, &str, &str)], excluded: bool| {
        let mut parts = Vec::new();
        let mut writes = Vec::new();
        let mut out = Vec::new();
        for (index, _, _) in placed {
            parts.push(format!("channel \"{index}\" {{\n  kind = \"index\"\n}}\n"));
            writes.push(format!("\"{index}\""));
            out.push(format!(", \"!{index}\""));
        }
        let out = if excluded {
            out.concat()
        } else {
            String::new()
        };
        let writes = writes.join(", ");
        parts.push(format!(
            "connector \"a\" {{\n  kind = \"writer\"\n  node = \"n\"\n  \
             writes = [{writes}]\n}}\n"
        ));
        for (index, at, home) in placed {
            parts.push(format!(
                "placement \"{at}\" {{\n  select = [\"{index}\"{out}]\n  \
                 home = \"{home}\"\n}}\n"
            ));
        }
        if excluded {
            parts.push(format!(
                "placement \"a\" {{\n  select = [\"a\", {writes}]\n  home = \"n\"\n}}\n"
            ));
        }
        parts.concat()
    };
    let cases: [(&[_], _); 4] = [
        (
            &[("a.time", "p", "m"), ("a.value", "q", "n")],
            "`p` and `q`",
        ),
        (
            &[("a.time", "p", "m"), ("a.value", "q", "k")],
            "`p` and `q`",
        ),
        (
            &[("a.time", "p", "n"), ("a.value", "q", "n")],
            "`p` and `q`",
        ),
        (
            &[
                ("a.time", "p", "m"),
                ("a.value", "q", "n"),
                ("a.x", "r", "k"),
            ],
            "`p`, `q`, and `r`",
        ),
    ];
    let nodes = ["k", "m", "n"];
    for (placed, of) in cases {
        let refused = text(placed, false);
        let found = problems(Spec::create_empty().plan(&[&refused], &nodes));
        let found: Vec<_> = found.into_iter().map(|p| (p.0, p.3)).collect();
        let fix = format!(
            "Exclude the indexes of the connector `a` from the `select` of {of}, and \
             select the connector and its indexes with another placement whose `home` \
             is `n`"
        );
        let expected = vec![("config.split-placement", fix); placed.len()];
        assert_eq!(found, expected, "{refused}");
        let fixed = text(placed, true);
        let plan = Spec::create_empty().plan(&[&fixed], &nodes);
        assert!(plan.is_ok(), "{fixed}: {plan:?}");
    }
}

#[test]
fn plans_after_the_one_fix_of_a_connector_whose_index_another_placement_wins() {
    let text = |home: &str, excluded: bool| {
        let (out, a) = if excluded {
            (
                ", \"!a\", \"!a.time\"",
                "placement \"a\" {\n  select = [\"a\", \"a.time\"]\n  \
                 home = \"n\"\n}\n",
            )
        } else {
            ("", "")
        };
        format!(
            "\
channel \"a.time\" {{
  kind = \"index\"
}}
connector \"a\" {{
  kind = \"writer\"
  node = \"n\"
  writes = [\"a.time\"]
}}
connector \"b\" {{
  kind = \"writer\"
  node = \"m\"
  writes = []
}}
placement \"t\" {{
  select = [\"a\", \"b\"{out}]
  home = \"m\"
}}
placement \"r\" {{
  select = [\"a.time\"{out}]
  home = \"{home}\"
}}
{a}"
        )
    };
    let nodes = ["k", "m", "n"];
    let fix = exclude("a", "`t` and `r`", "n");
    for home in nodes {
        let refused = text(home, false);
        let found = problems(Spec::create_empty().plan(&[&refused], &nodes));
        let found: Vec<_> = found.into_iter().map(|p| (p.0, p.3)).collect();
        let expected = [
            ("config.connector-home", fix.clone()),
            ("config.split-placement", fix.clone()),
        ];
        assert_eq!(found, expected, "{refused}");
        let fixed = text(home, true);
        let plan = Spec::create_empty().plan(&[&fixed], &nodes);
        assert!(plan.is_ok(), "{fixed}: {plan:?}");
    }
}

/// The placement `at` with the `select` list `select` and the `home` `home`.
fn placement(at: &str, select: &str, home: &str) -> String {
    format!("placement \"{at}\" {{\n  select = [{select}]\n  home = \"{home}\"\n}}\n")
}

#[test]
fn plans_after_the_one_fix_of_a_connector_whose_placement_wins_an_index_elsewhere() {
    let common = "\
channel \"b.time\" {
  kind = \"index\"
}
connector \"a\" {
  kind = \"writer\"
  node = \"n\"
  writes = []
}
connector \"b\" {
  kind = \"writer\"
  node = \"m\"
  writes = [\"b.time\"]
}
";
    let refused = common.to_owned() + &placement("t", "\"a\", \"b.time\"", "m");
    let fixed = common.to_owned()
        + &placement("t", "\"b\", \"b.time\"", "m")
        + &placement("a", "\"a\"", "n");
    let expected = [
        ("config.split-placement", win("t", "b")),
        ("config.connector-home", exclude("a", "`t`", "n")),
    ];
    plans_after(&refused, &expected, &fixed);
}

#[test]
fn plans_after_the_split_placement_fix_of_a_connector_with_an_unselected_index() {
    let text = |select: &str| {
        format!(
            "\
channel \"a.time\" {{
  kind = \"index\"
}}
channel \"a.x\" {{
  kind = \"index\"
}}
connector \"a\" {{
  kind = \"writer\"
  node = \"n\"
  writes = [\"a.time\", \"a.x\"]
}}
placement \"p\" {{
  select = [{select}]
  home = \"n\"
}}
"
        )
    };
    let expected = [("config.split-placement", win("p", "a"))];
    plans_after(
        &text("\"a.time\""),
        &expected,
        &text("\"a\", \"a.time\", \"a.x\""),
    );
}

#[test]
fn plans_after_the_split_placement_fix_of_a_placement_at_the_connectors_node() {
    let common = "\
channel \"a.time\" {
  kind = \"index\"
}
connector \"a\" {
  kind = \"writer\"
  node = \"n\"
  writes = [\"a.time\"]
}
connector \"b\" {
  kind = \"writer\"
  node = \"m\"
  writes = []
}
";
    let refused = common.to_owned()
        + &placement("t", "\"a\", \"b\"", "n")
        + &placement("r", "\"a.time\"", "n");
    let fixed = common.to_owned()
        + &placement("t", "\"a\", \"a.time\"", "n")
        + &placement("b", "\"b\"", "m");
    let expected = [
        ("config.connector-home", exclude("b", "`t`", "m")),
        ("config.split-placement", win("t", "a")),
    ];
    plans_after(&refused, &expected, &fixed);
}

/// The `config.unplaced` problem at the label `at` of `text`, where the placements
/// `first` and `second` tie.
fn tie(text: &str, at: &str, first: &str, second: &str) -> Problem {
    problem(
        "config.unplaced",
        (0, label(text, at)),
        &format!(
            "the placements `{first}` and `{second}` select the name with the same \
             specificity"
        ),
        "Change the `select` of one of the two placements, so that one selects the \
         name more specifically",
    )
}

#[test]
fn gives_no_split_placement_after_a_tie() {
    let text = "\
channel \"a.time\" {
  kind = \"index\"
}
placement \"a_1\" {
  select = \"a.*\"
  home = \"n\"
}
placement \"a_2\" {
  select = \"a.*\"
  home = \"n\"
}
placement \"a\" {
  select = \"a\"
  home = \"n\"
}
connector \"a\" {
  kind = \"writer\"
  node = \"n\"
  writes = [\"a.time\"]
}
channel \"b.time\" {
  kind = \"index\"
}
placement \"b_time\" {
  select = \"b.time\"
  home = \"n\"
}
placement \"b_1\" {
  select = \"b\"
  home = \"n\"
}
placement \"b_2\" {
  select = \"b\"
  home = \"n\"
}
connector \"b\" {
  kind = \"writer\"
  node = \"n\"
  writes = [\"b.time\"]
}
";
    let found = problems(Spec::create_empty().plan(&[text], &["n"]));
    let expected = [
        tie(text, "a.time", "a_1", "a_2"),
        tie(text, "b", "b_1", "b_2"),
    ];
    assert_eq!(found, expected);
}

#[test]
fn gives_no_split_placement_after_a_tie_at_the_nearest_connector() {
    let text = "\
channel \"d.e.time\" {
  kind = \"index\"
}
placement \"d\" {
  select = \"d.**\"
  home = \"n\"
}
placement \"e_1\" {
  select = \"d.e\"
  home = \"n\"
}
placement \"e_2\" {
  select = \"d.e\"
  home = \"n\"
}
placement \"t\" {
  select = \"d.e.time\"
  home = \"n\"
}
connector \"d\" {
  kind = \"writer\"
  node = \"n\"
  writes = []
}
connector \"d.e\" {
  kind = \"writer\"
  node = \"n\"
  writes = [\"d.e.time\"]
}
";
    let found = problems(Spec::create_empty().plan(&[text], &["n"]));
    assert_eq!(found, [tie(text, "d.e", "e_1", "e_2")]);
}

#[test]
fn gives_split_placement_after_an_index_with_no_home() {
    let text = "\
channel \"c.time\" {
  kind = \"index\"
}
placement \"c_time\" {
  select = \"c.time\"
  standby = \"n\"
}
placement \"c\" {
  select = \"c\"
  home = \"n\"
}
connector \"c\" {
  kind = \"writer\"
  node = \"n\"
  writes = []
}
channel \"e.time\" {
  kind = \"index\"
}
placement \"e\" {
  select = \"e.*\"
  standby = \"n\"
}
connector \"e\" {
  kind = \"writer\"
  node = \"n\"
  writes = [\"e.time\"]
}
";
    let found = problems(Spec::create_empty().plan(&[text], &["n"]));
    let expected = [
        problem(
            "config.unplaced",
            (0, label(text, "c.time")),
            "the placement `c_time` wins for the index and names no home, and no \
             connector writes the index",
            "Name a `home` in the placement, or write the index with a connector",
        ),
        split(
            text,
            "c_time",
            "the placement `c_time` wins for the index `c.time`, but the placement `c` \
             wins for the connector `c`",
            "c",
            "c",
        ),
        problem(
            "config.unplaced",
            (0, label(text, "e.time")),
            "the node `n` of a connector is the home and has another role in the \
             placement `e`",
            OVERLAP,
        ),
        split(
            text,
            "e",
            "the placement `e` wins for the index `e.time`, but no placement selects \
             the connector `e`",
            "e",
            "e",
        ),
    ];
    assert_eq!(found, expected);
}

#[test]
fn plans_after_the_overlap_fix_and_the_split_placement_fix() {
    let text = |placement: &str| {
        format!(
            "\
channel \"a.time\" {{
  kind = \"index\"
}}
connector \"a\" {{
  kind = \"writer\"
  node = \"n\"
  writes = [\"a.time\"]
}}
placement \"p\" {{
{placement}}}
"
        )
    };
    let cases = [
        (
            "  select = [\"a.time\"]\n  standby = \"n\"\n",
            "  select = [\"a\", \"a.time\"]\n  home = \"n\"\n",
        ),
        (
            "  select = [\"a.time\"]\n  standby = \"n\"\n  copies = [\"k\"]\n",
            "  select = [\"a\", \"a.time\"]\n  copies = [\"k\"]\n",
        ),
    ];
    let expected = [
        ("config.unplaced", OVERLAP.to_owned()),
        ("config.split-placement", win("p", "a")),
    ];
    for (refused, fixed) in cases {
        plans_after(&text(refused), &expected, &text(fixed));
    }
}

#[test]
fn plans_after_the_overlap_fixes_that_empty_a_placement_and_its_empty_fix() {
    let text = |nodes: &str| {
        format!(
            "\
connector \"a\" {{
  kind = \"writer\"
  node = \"n\"
  writes = []
}}
connector \"b\" {{
  kind = \"writer\"
  node = \"k\"
  writes = []
}}
placement \"p\" {{
  select = [\"a\", \"b\"]
{nodes}}}
"
        )
    };
    let overlaps = [
        ("config.unplaced", OVERLAP.to_owned()),
        ("config.unplaced", OVERLAP.to_owned()),
    ];
    let refused = text("  standby = \"n\"\n  copies = [\"k\"]\n");
    let named = text("  standby = \"m\"\n");
    plans_after(&refused, &overlaps, &named);
    // Each node of `p` holds a connector of `p`, so the two removals leave no node.
    let empty = [(
        "config.empty-placement",
        "Name a `home`, a `standby`, or a node in `copies`".to_owned(),
    )];
    plans_after(&text(""), &empty, &named);
}

#[test]
fn plans_after_the_overlap_move_and_its_connector_home_fix() {
    let standby = |select: &str| {
        format!("placement \"p\" {{\n  select = [{select}]\n  standby = \"k\"\n}}\n")
    };
    let index = "\
channel \"c.time\" {
  kind = \"index\"
}
connector \"c\" {
  kind = \"writer\"
  node = \"n\"
  writes = []
}
connector \"w\" {
  kind = \"writer\"
  node = \"k\"
  writes = [\"c.time\"]
}
";
    let connectors = "\
connector \"c\" {
  kind = \"writer\"
  node = \"n\"
  writes = []
}
connector \"d\" {
  kind = \"writer\"
  node = \"k\"
  writes = []
}
";
    let index_fixed = index.to_owned() + &placement("p", "\"c\", \"c.time\"", "n");
    let connectors_fixed = connectors.to_owned()
        + &placement("p", "\"d\"", "k")
        + &placement("q", "\"c\"", "n");
    // `k` is the one node of `p`, and `p` wins for `c` on `n`. In the first case, each
    // name that `p` wins is on `n`.
    let excluded = exclude("c", "`p`", "n");
    let cases = [
        (index, "\"c\", \"c.time\"", RENAMED.to_owned(), index_fixed),
        (connectors, "\"c\", \"d\"", excluded, connectors_fixed),
    ];
    let overlap = [("config.unplaced", OVERLAP.to_owned())];
    for (common, select, fix, fixed) in cases {
        let refused = common.to_owned() + &standby(select);
        let found = problems(Spec::create_empty().plan(&[&refused], &["k", "m", "n"]));
        let found: Vec<_> = found.into_iter().map(|p| (p.0, p.3)).collect();
        assert_eq!(found, overlap, "{refused}");
        let moved = common.to_owned() + &placement("p", select, "k");
        plans_after_connector_home(&moved, &[fix], &fixed);
    }
}

/// The fix of each diagnostic of `connector` on `node` when its placement to win,
/// `winner`, names no `home` and an index of `connector` has no writer.
fn homed(winner: &str, connector: &str, node: &str) -> String {
    format!(
        "Name `{node}` as the `home` of `{winner}`, keep `{node}` out of its `standby` \
         and `copies`, and make `{winner}` win for the connector `{connector}` and its \
         indexes"
    )
}

/// The index `c.time` and the connectors `c` on `n` and `d` on `m`, which write
/// nothing, then `placements`.
fn unwritten(placements: &str) -> String {
    format!(
        "\
channel \"c.time\" {{
  kind = \"index\"
}}
connector \"c\" {{
  kind = \"writer\"
  node = \"n\"
  writes = []
}}
connector \"d\" {{
  kind = \"writer\"
  node = \"m\"
  writes = []
}}
{placements}"
    )
}

#[test]
fn plans_after_the_split_placement_fix_that_names_a_home_for_an_unwritten_index() {
    let t = "\
placement \"t\" {
  select = [\"c\"]
  standby = \"k\"
}
";
    let r = "\
placement \"r\" {
  select = [\"c.time\"]
  home = \"n\"
}
";
    let fixed = unwritten(
        "\
placement \"t\" {
  select = [\"c\", \"c.time\"]
  home = \"n\"
  standby = \"k\"
}
",
    );
    let split = ("config.split-placement", homed("t", "c", "n"));
    let selected = unwritten(&format!("{t}{r}"));
    plans_after(&selected, slice::from_ref(&split), &fixed);
    // No placement selects `c.time`.
    let unselected = (
        "config.unplaced",
        "Select the index with a placement that names a `home`, or write it with a \
         connector"
            .to_owned(),
    );
    plans_after(&unwritten(t), &[unselected, split], &fixed);
}

#[test]
fn plans_after_the_split_placement_fix_of_a_connector_whose_placement_overlaps() {
    let refused = unwritten(
        "\
placement \"p\" {
  select = [\"c\"]
  standby = \"n\"
}
placement \"r\" {
  select = [\"c.time\"]
  home = \"n\"
}
",
    );
    let fixed = unwritten(
        "\
placement \"p\" {
  select = [\"c\", \"c.time\"]
  home = \"n\"
}
",
    );
    let expected = [
        ("config.unplaced", OVERLAP.to_owned()),
        ("config.split-placement", homed("p", "c", "n")),
    ];
    plans_after(&refused, &expected, &fixed);
}

#[test]
fn plans_after_the_split_placement_fix_that_names_a_home_with_no_connector_placement() {
    let refused = unwritten(
        "\
placement \"o\" {
  select = [\"c.time\"]
  standby = \"k\"
}
",
    );
    let fixed = unwritten(
        "\
placement \"o\" {
  select = [\"c\", \"c.time\"]
  home = \"n\"
  standby = \"k\"
}
",
    );
    let expected = [
        (
            "config.unplaced",
            "Name a `home` in the placement, or write the index with a connector"
                .into(),
        ),
        ("config.split-placement", homed("o", "c", "n")),
    ];
    plans_after(&refused, &expected, &fixed);
}

#[test]
fn moves_an_unwritten_index_when_its_placement_wins_on_another_node() {
    let refused = unwritten(
        "\
placement \"t\" {
  select = [\"c\", \"d\"]
  standby = \"k\"
}
placement \"r\" {
  select = [\"c.time\"]
  home = \"n\"
}
",
    );
    let fixed = unwritten(
        "\
placement \"t\" {
  select = [\"c\", \"d\", \"!c\", \"!c.time\"]
  standby = \"k\"
}
placement \"r\" {
  select = [\"c.time\", \"!c\", \"!c.time\"]
  home = \"n\"
}
placement \"c\" {
  select = [\"c\", \"c.time\"]
  home = \"n\"
}
",
    );
    let expected = [("config.split-placement", exclude("c", "`t` and `r`", "n"))];
    plans_after(&refused, &expected, &fixed);
}

#[test]
fn moves_an_unwritten_index_when_its_own_placement_wins_on_another_node() {
    let refused = unwritten(
        "\
placement \"o\" {
  select = [\"c.time\", \"d\"]
  standby = \"k\"
}
",
    );
    let fixed = unwritten(
        "\
placement \"o\" {
  select = [\"c.time\", \"d\", \"!c.time\"]
  standby = \"k\"
}
placement \"c\" {
  select = [\"c\", \"c.time\"]
  home = \"n\"
}
",
    );
    let expected = [
        (
            "config.unplaced",
            "Name a `home` in the placement, or write the index with a connector"
                .into(),
        ),
        (
            "config.split-placement",
            "Exclude the indexes of the connector `c` from the `select` of `o`, and \
             select the connector and its indexes with another placement whose `home` \
             is `n`"
                .into(),
        ),
    ];
    plans_after(&refused, &expected, &fixed);
}

#[test]
fn plans_after_the_split_placement_fix_of_an_index_and_the_one_fix_of_a_connector() {
    let common = "\
channel \"b.time\" {
  kind = \"index\"
}
connector \"a\" {
  kind = \"writer\"
  node = \"n\"
  writes = []
}
connector \"b\" {
  kind = \"writer\"
  node = \"m\"
  writes = [\"b.time\"]
}
";
    let refused = common.to_owned()
        + &placement("t", "\"a\", \"b.time\"", "m")
        + &placement("q", "\"b\"", "m");
    let fixed = common.to_owned()
        + &placement("t", "\"a\", \"b.time\", \"!a\", \"!b.time\"", "m")
        + &placement("q", "\"b\", \"b.time\"", "m")
        + &placement("a", "\"a\"", "n");
    let expected = [
        ("config.split-placement", win("q", "b")),
        ("config.connector-home", exclude("a", "`t`", "n")),
    ];
    plans_after(&refused, &expected, &fixed);
}

#[test]
fn takes_the_first_writer_in_source_order_in_any_order_of_the_files() {
    let first = format!(
        "{PLANT}\
connector \"w1\" {{
  kind = \"writer\"
  node = \"n2\"
  writes = [\"a.value\"]
}}
"
    );
    let second = "\
connector \"w2\" {
  kind = \"writer\"
  node = \"n1\"
  writes = [\"a.time\"]
}
";
    let members = BTreeSet::from([name("n"), name("n1"), name("n2")]);
    let spec = Spec::create_empty();
    let expected = [problem(
        "config.writer-nodes",
        (1, value(second, "node", "\"n1\"")),
        "connectors on the nodes `n2` and `n1` write the index `a.time`, so it has no \
         one home",
        "Run each connector that writes `a.time` on one node",
    )];
    let mut documents = [read(0, &first), read(1, second)];
    for _ in 0..2 {
        let result = config::plan::plan(
            &documents,
            spec.pointer,
            &spec.definitions(),
            &members,
            &kinds(),
        );
        assert_eq!(problems(result), expected);
        documents.reverse();
    }
}

#[test]
fn gives_the_problems_in_source_then_source_order() {
    let placement = "\
placement \"a\" {
  select = \"a.*\"
  home = \"x_1\"
  copies = [\"x_2\"]
}
channel \"b.time\" {
  kind = \"index\"
}
";
    let channels = "\
channel \"a.time\" {
  kind = \"index\"
}
channel \"a.value\" {
  data_type = \"f64\"
  index = \"a.time\"
}
channel \"a.other\" {
  data_type = \"f64\"
  index = \"a.value\"
}
";
    let documents = [read(1, channels), read(0, placement)];
    let members = BTreeSet::from([name("n")]);
    let spec = Spec::create_empty();
    let result = config::plan::plan(
        &documents,
        spec.pointer,
        &spec.definitions(),
        &members,
        &kinds(),
    );
    let codes: Vec<_> = problems(result)
        .into_iter()
        .map(|(code, at, ..)| (code, at))
        .collect();
    let expected = [
        (
            "config.unknown-node",
            Some((Source(0), value(placement, "home", "\"x_1\""))),
        ),
        (
            "config.unknown-node",
            Some((Source(0), offset(placement, "\"x_2\""))),
        ),
        (
            "config.unplaced",
            Some((Source(0), label(placement, "b.time"))),
        ),
        (
            "config.wrong-channel",
            Some((Source(1), value(channels, "index", "\"a.value\""))),
        ),
    ];
    assert_eq!(codes, expected);
}

#[test]
fn check_gives_the_codes_of_plan_when_the_first_writer_in_the_files_differs() {
    let text = "\
channel \"a.time\" {
  kind = \"index\"
}
channel \"a.value\" {
  data_type = \"f64\"
  index = \"a.time\"
}
placement \"a\" {
  select = \"a.*\"
  standby = \"n2\"
}
connector \"w2\" {
  kind = \"writer\"
  node = \"n2\"
  writes = [\"a.time\"]
}
connector \"w1\" {
  kind = \"writer\"
  node = \"n1\"
  writes = [\"a.value\"]
}
";
    let planned = problems(Spec::create_empty().plan(&[text], &["n1", "n2"]));
    let codes: Vec<_> = planned.iter().map(|problem| problem.0).collect();
    assert_eq!(codes, ["config.writer-nodes"]);
}
