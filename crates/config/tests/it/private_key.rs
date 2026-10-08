//! A private key anywhere in a mesh's files.

use connector::kind::Table;
use document::diagnostic::Code;
use document::{Document, Source};

const PEM: &str = r#""-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA==\n""#;

/// The base64 body of a private key with no header lines: OpenSSH, then Ed25519
/// PKCS #8 v1 (`openssl genpkey`), v2 with the public key (`ring`), v2 with
/// attributes (the sample of RFC 8410), and v2 bodies of 134 and 265 bytes, whose
/// outer lengths take 2 and 3 bytes.
const BODIES: [&str; 6] = [
    r#""b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMw""#,
    r#""MC4CAQAwBQYDK2VwBCIEIAcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcH""#,
    concat!(
        r#""MFMCAQEwBQYDK2VwBCIEIAcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc"#,
        r#"HoSMDIQAJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQ==""#,
    ),
    concat!(
        r#""MHICAQEwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVh"#,
        r#"CoB8wHQYKKoZIhvcNAQkJFDEPDA1DdXJkbGUgQ2hhaXJzgSEAGb9ECWmEzf6FQbr"#,
        r#"BZ9w7lshQhqowtrbLDFw4rXAxZuE=""#,
    ),
    concat!(
        r#""MIGDAgEBMAUGAytlcAQiBCAHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHB6Aw"#,
        r#"MC4GCiqGSIb3DQEJCRQxIAweQ3VyZGxlIENoYWlycyBvZiB0aGUgTG9uZyBOYW1lgSEA"#,
        r#"CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=""#,
    ),
    concat!(
        r#""MIIBBQIBATAFBgMrZXAEIgQgBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcH"#,
        r#"BweggbEwga4GCiqGSIb3DQEJCRQxgZ8MgZx4eHh4eHh4eHh4eHh4eHh4eHh4eHh4"#,
        r#"eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4"#,
        r#"eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4"#,
        r#"eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHh4eHiBIQAJCQkJCQkJ"#,
        r#"CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQ==""#,
    ),
];

fn read(source: u32, text: &str) -> Document {
    config_hcl::read(Source(source), text).expect("the text is HCL")
}

fn kinds() -> Table {
    Table::new().with("influx", connector_influx::Kind::default())
}

/// The file and the text at the span of each problem of `texts`, which must each be
/// `config.private-key`.
fn alarms(texts: &[&str]) -> Vec<(u32, String)> {
    let files: Vec<_> = (0..).zip(texts).map(|(i, text)| read(i, text)).collect();
    let Err(diagnostics) = config::check(&files, &kinds()) else {
        panic!("a private key passed");
    };
    diagnostics
        .into_iter()
        .map(|diagnostic| {
            assert_eq!(diagnostic.code, Code::new("config.private-key"));
            assert_eq!(
                diagnostic.message,
                "the value is a private key, which must never be in a file"
            );
            assert_eq!(
                diagnostic.fix,
                "Remove the private key from this file now, and use the one line of \
                 its `.pub` file"
            );
            assert_eq!(diagnostic.notes, []);
            let span = diagnostic.span.expect("a span");
            let text = texts[usize::try_from(span.source().0).unwrap()];
            let range = span.start().offset as usize..span.end().offset as usize;
            (span.source().0, text[range].to_owned())
        })
        .collect()
}

#[test]
fn alarms_alone_at_a_misspelled_attribute_of_a_subject() {
    let text = format!("subject \"alice\" {{\n  kyes = {PEM}\n}}\n");
    assert_eq!(alarms(&[&text]), [(0, PEM.to_owned())]);
}

#[test]
fn alarms_alone_in_a_block_inside_a_connector() {
    let text = format!(
        "connector \"plc\" {{\n  kind = \"influx\"\n  node = \"edge\"\n  \
         address = \"http://influx:8086\"\n  select = \"edge.*\"\n  \
         auth {{\n    token = {PEM}\n  }}\n}}\n"
    );
    assert_eq!(alarms(&[&text]), [(0, PEM.to_owned())]);
}

#[test]
fn alarms_alone_at_the_body_of_a_private_key_with_no_header_lines() {
    for body in BODIES {
        let subject = format!("subject \"alice\" {{\n  keys = {body}\n}}\n");
        let connector = format!(
            "connector \"plc\" {{\n  kind = \"influx\"\n  node = \"edge\"\n  \
             address = \"http://influx:8086\"\n  select = \"edge.*\"\n  \
             auth {{\n    token = {body}\n  }}\n}}\n"
        );
        assert_eq!(alarms(&[&subject]), [(0, body.to_owned())]);
        assert_eq!(alarms(&[&connector]), [(0, body.to_owned())]);
    }
}

#[test]
fn alarms_alone_at_a_map_key() {
    let text = format!("subject \"alice\" {{\n  tags = {{ {PEM} = 1 }}\n}}\n");
    assert_eq!(alarms(&[&text]), [(0, PEM.to_owned())]);
}

#[test]
fn alarms_alone_at_each_string_in_source_order() {
    let text = format!(
        "subject \"PuTTY-User-Key-File\" {{\n  keys = [{PEM}]\n}}\n\
         zone = PuTTY-User-Key-File({PEM})\nPuTTY-User-Key-File {{}}\n\
         site = PuTTY-User-Key-File\n"
    );
    let putty = "PuTTY-User-Key-File";
    let quoted = format!("{putty:?}");
    let expected =
        [&quoted, PEM, putty, PEM, putty, putty].map(|text| (0, text.to_owned()));
    assert_eq!(alarms(&[&text]), expected);
}

#[test]
fn alarms_alone_over_two_files() {
    let key = format!("subject \"alice\" {{\n  keys = {PEM}\n}}\n");
    let unknown = "subject \"bob\" {\n  colour = \"blue\"\n}\n";
    assert_eq!(alarms(&[unknown, &key]), [(1, PEM.to_owned())]);
    assert_eq!(alarms(&[&key, unknown]), [(0, PEM.to_owned())]);
}

#[test]
fn alarms_in_the_order_of_the_sources_in_any_order_of_the_files() {
    let key = format!("subject \"alice\" {{\n  keys = {PEM}\n}}\n");
    let files = [read(1, &key), read(0, &key)];
    let Err(diagnostics) = config::check(&files, &kinds()) else {
        panic!("a private key passed");
    };
    let sources: Vec<_> = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.span.map(document::Span::source))
        .collect();
    assert_eq!(sources, [Some(Source(0)), Some(Source(1))]);
}

#[test]
fn alarms_in_the_order_of_the_files_then_of_the_source() {
    let late = format!("subject \"alice\" {{\n  keys = [\"a\", \"b\", {PEM}]\n}}\n");
    let early = format!("subject \"bob\" {{\n  keys = {PEM}\n}}\n");
    let expected = [(0, PEM.to_owned()), (1, PEM.to_owned())];
    assert_eq!(alarms(&[&late, &early]), expected);
    assert_eq!(alarms(&[&early, &late]), expected);
}
