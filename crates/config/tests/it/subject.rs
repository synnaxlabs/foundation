//! The `subject` block, next to the `connector` blocks of a mesh.

use connector::kind::Table;
use document::diagnostic::{Code, Diagnostic, Note};
use document::{Document, Source};
use types::name::Name;

const CONNECTOR: &str = "connector \"plc\" {\n  kind = \"influx\"\n  node = \"edge\"\n  \
                         address = \"http://influx:8086\"\n  select = \"edge.*\"\n}\n";
const SUBJECT: &str = "subject \"plc\" {\n  keys = \"ssh-ed25519 \
                       AAAAC3NzaC1lZDI1NTE5AAAAIGVVuOR8JKYpAcWLMUveadmJ1wUAmYGgIDtqlhFe7Yhg \
                       alice@laptop\"\n}\n";

fn read(source: u32, text: &str) -> Document {
    config_hcl::read(Source(source), text).expect("the text is HCL")
}

fn kinds() -> Table {
    Table::new().with("influx", connector_influx::Kind::default())
}

/// `config.subject-is-connector` at the label of the subject, with a note at the label
/// of the connector.
fn refused(subject: &Document, connector: &Document) -> Vec<Diagnostic> {
    let label = |document: &Document, keyword: &str| {
        let block = document.blocks.iter().find(|b| &*b.keyword == keyword);
        block.expect("the block").labels[0].span
    };
    let mut diagnostic = Diagnostic::new(
        Code::new("config.subject-is-connector"),
        label(subject, "subject"),
        "the subject \"plc\" has the name of a connector".into(),
        "Rename the subject or the connector".into(),
    );
    diagnostic.notes.push(Note {
        span: label(connector, "connector").expect("a span"),
        text: "the connector".into(),
    });
    vec![diagnostic]
}

#[test]
fn checks_the_connector_and_the_subject_alone() {
    for (text, key) in [(CONNECTOR, "plc"), (SUBJECT, "plc.@subject")] {
        let entries = config::check(&[read(0, text)], &kinds()).expect("no problems");
        let keys: Vec<_> = entries.keys().map(Name::as_str).collect();
        assert_eq!(keys, [key]);
    }
}

#[test]
fn refuses_a_subject_at_the_name_of_a_connector_in_one_file() {
    for text in [[CONNECTOR, SUBJECT].concat(), [SUBJECT, CONNECTOR].concat()] {
        let file = read(0, &text);
        assert_eq!(
            config::check(slice(&file), &kinds()),
            Err(refused(&file, &file)),
            "{text}"
        );
    }
}

#[test]
fn refuses_a_subject_at_the_name_of_a_connector_in_another_file() {
    let (connector, subject) = (read(0, CONNECTOR), read(1, SUBJECT));
    let expected = Err(refused(&subject, &connector));
    let files = [connector.clone(), subject.clone()];
    assert_eq!(config::check(&files, &kinds()), expected);
    let (subject, connector) = (read(0, SUBJECT), read(1, CONNECTOR));
    let expected = Err(refused(&subject, &connector));
    let files = [subject, connector];
    assert_eq!(config::check(&files, &kinds()), expected);
}

fn slice(file: &Document) -> &[Document] {
    std::slice::from_ref(file)
}
