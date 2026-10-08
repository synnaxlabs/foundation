//! `config::plan` against applied specs that the tests build and apply.

use std::collections::{BTreeMap, BTreeSet};

use config::{Definition, Entry, Plan};
use connector::cancel;
use connector::kind::{self, Channels, Context, Kind, Table};
use document::diagnostic::Diagnostic;
use document::{Document, Source, read as reader};
use spec::Pointer;
use spec::channel::Channel;
use spec::definition::Definition as Stored;
use spec::tree::{self, Chunks};
use types::channel::Key;
use types::digest::Digest;
use types::ed25519::PrivateKey;
use types::name::Name;

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

    fn plan(&self, texts: &[&str], members: &[&str]) -> Result<Plan, Vec<Diagnostic>> {
        let members = members.iter().map(|member| name(member)).collect();
        let applied = self.definitions();
        config::plan(
            &documents(texts),
            self.pointer,
            &applied,
            &members,
            &kinds(),
        )
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
        for change in &plan.changes {
            let channel = change.new.as_ref().is_some_and(|entry| {
                matches!(entry.definition, Definition::Channel(_))
            });
            if !channel {
                keys.remove(&change.name);
            } else if change.old.is_none() || !keys.contains_key(&change.name) {
                self.made += 1;
                // Version 7, as each stored key is.
                keys.insert(change.name.clone(), Key::from_u128((7 << 76) | self.made));
            }
        }
        let changes: Vec<_> = plan
            .changes
            .iter()
            .map(|change| match &change.new {
                None => tree::Change::Delete(change.name.clone()),
                Some(entry) => tree::Change::Set(
                    change.name.clone(),
                    encode(&change.name, entry, &keys),
                ),
            })
            .collect();
        self.set(changes);
    }
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
}

fn name(text: &str) -> Name {
    text.parse().expect("a name")
}

/// Each change as its tree key, its old digest, and its new entry.
fn changes(plan: &Plan) -> Vec<(&Name, Option<Digest>, Option<&Entry>)> {
    let changes = plan.changes.iter();
    changes
        .map(|change| (&change.name, change.old, change.new.as_ref()))
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
    let influx = plan.changes.last().and_then(|change| change.new.as_ref());
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
    assert_eq!(plan.homes, BTreeMap::new());
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
fn changes_each_channel_on_a_renamed_index() {
    let mut spec = Spec::create_empty();
    spec.apply(&spec.plan(&[EDGE], &["edge"]).expect("no problems"));
    let edge = EDGE.replace("\"edge.time\"", "\"edge.clock\"");
    let plan = spec.plan(&[&edge], &["edge"]).expect("no problems");
    let found: Vec<_> = plan
        .changes
        .iter()
        .map(|change| {
            (
                change.name.as_str(),
                change.old.is_some(),
                change.new.is_some(),
            )
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
        .map(|change| (change.name.as_str(), change.old))
        .collect();
    let added = ["a.@placement", "a.time", "a.value", "b.@placement"];
    assert_eq!(found, added.map(|name| (name, None)));
    assert_eq!(plan.homes, BTreeMap::from([(name("a.time"), name("n"))]));
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
    let keys: Vec<_> = plan
        .changes
        .iter()
        .map(|change| change.name.as_str())
        .collect();
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
    let keys: Vec<_> = plan
        .changes
        .iter()
        .map(|change| change.name.as_str())
        .collect();
    assert_eq!(keys, ["a.value"]);
    assert_eq!(plan.homes, BTreeMap::from([(name("a.value"), name("n"))]));
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
            "Remove the node from the placement",
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
        "Name `n` as the `home`, or leave out `home`",
    );
    assert_eq!(found, [expected]);
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
            "Remove the node from the placement",
        ),
    ];
    assert_eq!(found, expected);
}

/// A `config.split-placement` problem at the label of `placement` in `text`.
fn split(text: &str, placement: &str, message: &str, connector: &str) -> Problem {
    let fix = format!(
        "Select the connector `{connector}` and each index under its name with one \
         placement, such as `select = \"{connector}.**\"`"
    );
    problem(
        "config.split-placement",
        (0, label(text, placement)),
        message,
        &fix,
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
channel \"d.e.time\" {
  kind = \"index\"
}
placement \"d\" {
  select = \"d.**\"
  home = \"n\"
}
placement \"e\" {
  select = \"d.e.**\"
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
    let expected = [
        split(
            text,
            "a_time",
            "the placement `a_time` wins for the index `a.time`, but the placement `a` \
             wins for the connector `a`",
            "a",
        ),
        split(
            text,
            "e",
            "the placement `e` wins for the index `d.e.time`, but the placement `d` \
             wins for the connector `d`",
            "d",
        ),
    ];
    assert_eq!(found, expected);
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
            "b",
        ),
        split(
            text,
            "c",
            "the placement `c` wins for the index `c.time`, but no placement selects \
             the connector `c`",
            "c",
        ),
    ];
    assert_eq!(found, expected);
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
        let result = config::plan(
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
    let result = config::plan(
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
