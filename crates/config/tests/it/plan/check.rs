//! The rules of `config::check` on names and private keys, which `config::plan::check`
//! holds too.

use std::collections::BTreeSet;
use std::slice;

use document::Source;
use document::encoding::Checked;
use spec::access::Action;
use spec::channel::{Data, Kind as ChannelKind};
use spec::data_type::DataType;
use spec::definition::{Definition as Stored, Kind};
use spec::time::{self, Peers};
use spec::unit::Unit;
use spec::{compression, connector, node_settings, placement, region, retention};
use types::authority::Authority;
use types::byte;
use types::channel::Key;
use types::ed25519::PrivateKey;
use types::name::{Name, Selector};
use types::sample::{self, Scalar};

use super::{Problem, documents, kinds, name, problems, read, unreachable};

/// Text that only an OpenSSH private key holds, and that a name can hold.
const MARK: &str = "b3BlbnNzaC1rZXktdjEA";

/// The problems that `config::plan::check` gives for `definitions`, with the one member
/// `n`.
fn checked(definitions: impl IntoIterator<Item = (Name, Stored)>) -> Vec<Problem> {
    let definitions = definitions.into_iter().collect();
    let members = BTreeSet::from([name("n")]);
    let found = config::plan::check(&definitions, &members, &kinds());
    problems(found.map(|()| unreachable()))
}

/// The problems that `config::check` gives for `texts`, with no span.
fn checked_files(texts: &[&str]) -> Vec<Problem> {
    let found = config::check(&documents(texts), &kinds());
    let mut found = problems(found.map(|_| unreachable()));
    for problem in &mut found {
        problem.1 = None;
    }
    found
}

fn key(kind: Kind, label: &str) -> Name {
    kind.key(label).expect("a tree key")
}

fn selector(patterns: &[&str]) -> Selector {
    Selector::new(patterns.iter().copied()).expect("a selector")
}

/// A connector at `label` of `kind` on `node`, with the config `config`.
fn connector(label: &str, kind: &str, node: &str, config: &str) -> (Name, Stored) {
    let config = Checked::new(read(0, config)).expect("a shallow config");
    let connector = connector::Connector::new(name(kind), name(node), config);
    (name(label), Stored::Connector(connector))
}

/// A `writer` connector at `label` on `n` that writes nothing.
fn writer(label: &str) -> (Name, Stored) {
    connector(label, "writer", "n", "writes = []")
}

fn writer_text(label: &str) -> String {
    format!(
        "connector {label:?} {{\n  kind = \"writer\"\n  node = \"n\"\n  writes = []\n}}\n"
    )
}

fn placement(label: &str, select: &[&str], nodes: placement::Nodes) -> (Name, Stored) {
    let policy = placement::Policy::new(selector(select), nodes).expect("a policy");
    (key(Kind::Placement, label), Stored::Placement(policy))
}

fn home(node: &str) -> placement::Nodes {
    placement::Nodes {
        home: Some(name(node)),
        ..placement::Nodes::default()
    }
}

fn subject(label: &str) -> (Name, Stored) {
    let keys = vec![PrivateKey([7; 32]).public()];
    let subject = spec::subject::Subject::new(keys).expect("a subject");
    (key(Kind::Subject, label), Stored::Subject(subject))
}

fn duplicate(name: &str, earlier: &str, first: &str, blocks: &str) -> Problem {
    (
        "config.duplicate-name",
        None,
        format!("the name {name:?} repeats the earlier `{earlier}` name {first:?}"),
        format!("Give each {blocks} block a name that differs by more than case"),
    )
}

fn named_connector(subject: &str) -> Problem {
    (
        "config.subject-is-connector",
        None,
        format!("the subject {subject:?} has the name of a connector"),
        "Rename the subject or the connector".into(),
    )
}

fn alarm() -> Problem {
    (
        "config.private-key",
        None,
        "the value is a private key, which must never be in a file".into(),
        "Remove the private key from this file now, and use the one line of its `.pub` \
         file"
            .into(),
    )
}

#[test]
fn refuses_tree_keys_that_differ_only_in_case_as_check_of_the_files_does() {
    let expected = duplicate("w", "connector", "W", "`connector`");
    assert_eq!(
        checked([writer("W"), writer("w")]),
        slice::from_ref(&expected)
    );
    let texts = [writer_text("W"), writer_text("w")];
    assert_eq!(checked_files(&[&texts[0], &texts[1]]), [expected]);
}

#[test]
fn refuses_the_name_of_a_channel_and_a_connector_that_differ_only_in_case() {
    let index = ChannelKind::Index {
        error: None,
        control: None,
    };
    let channel = Stored::Channel(spec::channel::Channel {
        key: Key::from_u128(1),
        kind: index,
    });
    let found = checked([(name("Plant.x"), channel), writer("plant.x")]);
    let expected =
        duplicate("plant.x", "channel", "Plant.x", "`channel` and `connector`");
    assert_eq!(found, [expected]);
}

#[test]
fn refuses_the_labels_of_two_placements_that_differ_only_in_case() {
    let found = checked([
        placement("A", &["a.*"], home("n")),
        placement("a", &["a.*"], home("n")),
    ]);
    assert_eq!(found, [duplicate("a", "placement", "A", "`placement`")]);
}

#[test]
fn refuses_a_subject_at_the_name_of_a_connector_in_any_case_as_check_of_the_files_does()
{
    let expected = named_connector("W");
    assert_eq!(
        checked([writer("w"), subject("W")]),
        slice::from_ref(&expected)
    );
    let subject = "subject \"W\" {\n  keys = \"ssh-ed25519 \
                   AAAAC3NzaC1lZDI1NTE5AAAAIGVVuOR8JKYpAcWLMUveadmJ1wUAmYGgIDtqlhFe7Yhg \
                   alice@laptop\"\n}\n";
    assert_eq!(checked_files(&[&writer_text("w"), subject]), [expected]);
}

#[test]
fn gives_the_kind_name_and_subject_problems_together_and_no_rule_of_plan() {
    let found = checked([
        connector("a", "nothing", "n", ""),
        writer("W"),
        writer("w"),
        subject("W"),
    ]);
    let unknown = (
        "connector.unknown-kind",
        None,
        "this build has no connector kind \"nothing\"".into(),
        "Use one of [\"commander\", \"influx\", \"writer\"]".into(),
    );
    let expected = [
        unknown,
        duplicate("w", "connector", "W", "`connector`"),
        named_connector("W"),
    ];
    assert_eq!(found, expected);
}

fn access(subjects: &[&str], select: &[&str]) -> (Name, Stored) {
    let allow = [Action::Read].into_iter().collect();
    let policy = spec::access::Policy::new(
        selector(subjects),
        selector(select),
        allow,
        Authority(0),
    );
    (
        key(Kind::Access, "x"),
        Stored::Access(policy.expect("a policy")),
    )
}

/// A data channel at `x` in the unit `unit`.
fn measured(unit: &str) -> (Name, Stored) {
    let numbers = DataType::Sample(sample::Type::Scalar(Scalar::F64));
    let unit = Some(Unit::new(unit).expect("a unit"));
    let data = Data::new(Key::from_u128(1), None, numbers, unit).expect("a channel");
    let channel = spec::channel::Channel {
        key: Key::from_u128(2),
        kind: ChannelKind::Data(data),
    };
    (name("x"), Stored::Channel(channel))
}

/// One definition for each string that a definition holds, with [`MARK`] in that
/// string alone.
fn marked() -> Vec<(Name, Stored)> {
    let settings = |select: &[&str]| {
        let disk = Some(byte::Size::from_bytes(1));
        let policy = node_settings::Policy::new(selector(select), disk, None);
        let policy = policy.expect("a policy");
        (key(Kind::NodeSettings, "x"), Stored::NodeSettings(policy))
    };
    let compression = |select: &[&str]| {
        let policy = compression::Policy {
            select: selector(select),
            mode: compression::Mode::default(),
        };
        (key(Kind::Compression, "x"), Stored::Compression(policy))
    };
    let time = |select: &[&str], peers: Peers| {
        let policy = time::Policy::new(selector(select), peers);
        (key(Kind::Time, "x"), Stored::Time(policy))
    };
    let retention = |select: &[&str]| {
        let keep = types::time::Span::from_nanos(1);
        let policy = retention::Policy::new(selector(select), keep);
        let policy = policy.expect("a policy");
        (key(Kind::Retention, "x"), Stored::Retention(policy))
    };
    let voters = region::Delegation::new(1, [name(MARK)]).expect("a delegation");
    let exclude = format!("!{MARK}");
    vec![
        writer(MARK),
        access(&[MARK], &["x"]),
        access(&["x"], &[MARK]),
        access(&["x"], &["x.*", &exclude]),
        measured(MARK),
        connector("x", MARK, "n", ""),
        connector("x", "writer", MARK, "writes = []"),
        connector("x", "writer", "n", "note = \"-----BEGIN PRIVATE KEY-----\""),
        (key(Kind::Region, "x"), Stored::Region(voters)),
        settings(&[MARK]),
        compression(&[MARK]),
        placement("x", &[MARK], home("n")),
        placement("x", &["x"], home(MARK)),
        placement(
            "x",
            &["x"],
            placement::Nodes {
                standby: Some(name(MARK)),
                ..home("n")
            },
        ),
        placement(
            "x",
            &["x"],
            placement::Nodes {
                copies: vec![name(MARK)],
                ..home("n")
            },
        ),
        time(&[MARK], Peers::Voters),
        time(&["x"], Peers::Listed(vec![name(MARK)])),
        retention(&[MARK]),
    ]
}

#[test]
fn gives_only_one_private_key_problem_for_a_mark_in_any_string_of_a_definition() {
    for definition in marked() {
        let at = format!("{definition:?}");
        let mut found = checked([definition, connector("y", "nothing", "n", "")]);
        for problem in &mut found {
            problem.1 = None;
        }
        assert_eq!(found, [alarm()], "{at}");
    }
}

#[test]
fn gives_a_private_key_problem_in_a_connector_config_at_its_span() {
    let found = checked([connector("x", "writer", "n", "note = \"PRIVATE KEY\"")]);
    let mut expected = alarm();
    expected.1 = Some((Source(0), 7));
    assert_eq!(found, [expected]);
}

#[test]
fn gives_a_private_key_problem_for_each_string_that_holds_one() {
    let found = checked([writer(MARK), placement("x", &[MARK], home(MARK))]);
    assert_eq!(found, [alarm(), alarm(), alarm()]);
}
