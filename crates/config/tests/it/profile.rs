//! The profile of a program on a client host, as `config_hcl` reads and updates it.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use config::openssh;
use config::profile::{self, Profile};
use document::diagnostic::Diagnostic;
use document::encoding::Checked;
use document::{Document, Source};
use proptest::prelude::*;
use types::ed25519::{PrivateKey, PublicKey};

fn read(text: &str) -> Document {
    config_hcl::read(Source(0), text).expect("the text is HCL")
}

fn address(text: &str) -> SocketAddr {
    text.parse().unwrap()
}

fn key(byte: u8) -> PublicKey {
    PrivateKey([byte; 32]).public()
}

/// Each problem of `text` as its code, message, and fix.
fn problems(text: &str) -> Vec<(&'static str, String, String)> {
    let Err(diagnostics) = profile::decode(&read(text)) else {
        panic!("the profile decodes: {text}");
    };
    diagnostics
        .into_iter()
        .map(|problem| (problem.code.as_str(), problem.message, problem.fix))
        .collect()
}

fn problem(
    code: &'static str,
    message: &str,
    fix: &str,
) -> (&'static str, String, String) {
    (code, message.into(), fix.into())
}

/// `text` with the `node` block of `public_key` at `at` written in.
fn pinned(text: &str, at: SocketAddr, public_key: PublicKey) -> String {
    let mut document = read(text);
    document.blocks.push(profile::node(at, public_key));
    let document = Checked::new(document).expect("a profile is shallow");
    config_hcl::update(Source(0), text, &document).expect("the update writes")
}

#[test]
fn an_empty_profile_names_nothing_and_is_not_strict() {
    assert_eq!(profile::decode(&read("")), Ok(Profile::default()));
}

#[test]
fn decodes_each_attribute_and_each_node() {
    let text = format!(
        "subject = \"alice\"\nprivate_key = \"/home/alice/a.key\"\nstrict = true\n\n\
         node \"10.0.0.5:7400\" {{\n  public_key = \"{}\"\n}}\n\
         node \"[::1]:7401\" {{\n  public_key = \"{} bob@host\"\n}}\n",
        openssh::line(key(1)),
        openssh::line(key(2)),
    );
    let expected = Profile {
        subject: Some("alice".parse().unwrap()),
        private_key: Some("/home/alice/a.key".into()),
        strict: true,
        nodes: BTreeMap::from([
            (address("10.0.0.5:7400"), key(1)),
            (address("[::1]:7401"), key(2)),
        ]),
    };
    assert_eq!(profile::decode(&read(&text)), Ok(expected));
}

#[test]
fn refuses_each_wrong_attribute_with_its_fix() {
    let text = "subject = \"\"\nprivate_key = \"\"\nstrict = \"no\"\nuser = \"a\"\n";
    assert_eq!(
        problems(text),
        [
            problem(
                "document.bad-name",
                "a name or pattern is empty",
                "Write at least one segment",
            ),
            problem(
                "config.bad-path",
                "the path is empty",
                "Write the path of the key file, such as \
                 \"/home/alice/.foundation/alice.key\"",
            ),
            problem(
                "config.not-bool",
                "`strict` is a bool, not a string",
                "Write true or false",
            ),
            problem(
                "document.unknown-attribute",
                "`user` is not an attribute of the profile",
                "Use `subject`, `private_key`, or `strict`, or remove it",
            ),
        ]
    );
}

#[test]
fn refuses_a_path_that_is_not_a_string() {
    assert_eq!(
        problems("private_key = 3\n"),
        [problem(
            "config.bad-path",
            "a path is a string, not an integer",
            "Write the path of the key file, such as \
             \"/home/alice/.foundation/alice.key\"",
        )]
    );
}

#[test]
fn refuses_each_wrong_node_block_with_its_fix() {
    let line = openssh::line(key(1));
    let text = format!(
        "node {{\n  public_key = \"{line}\"\n}}\n\
         node \"10.0.0.5\" {{\n  public_key = \"{line}\"\n}}\n\
         node \"10.0.0.6:7400\" {{\n}}\n\
         node \"10.0.0.7:7400\" {{\n  public_key = \"ssh-ed25519\"\n  key = \"a\"\n}}\n\
         thing {{\n}}\n"
    );
    assert_eq!(
        problems(&text),
        [
            problem(
                "document.label-count",
                "the `node` block has no labels, and it takes 1 label",
                "Give the block one label, the address of the node, such as \
                 \"10.0.0.5:7400\"",
            ),
            problem(
                "config.bad-address",
                "the address \"10.0.0.5\" is not ip:port: invalid socket address \
                 syntax",
                "Write the address that the program dials, such as \"10.0.0.5:7400\" \
                 or \"[::1]:7400\"",
            ),
            problem(
                "document.missing-attribute",
                "the `node` block has no `public_key`",
                "Add a `public_key` attribute with the line that `foundation profile` \
                 prints",
            ),
            problem(
                "config.bad-public-key",
                "the public key is not the line of a `.pub` file",
                "Use the one line of a `.pub` file, such as `ssh-ed25519 AAAA... \
                 alice@laptop`",
            ),
            problem(
                "document.unknown-attribute",
                "`key` is not an attribute of the `node` block",
                "Use `public_key`, or remove it",
            ),
            problem(
                "document.unknown-block",
                "the profile cannot hold the `thing` block",
                "Use `node`, or remove it",
            ),
        ]
    );
}

#[test]
fn refuses_a_second_node_block_of_an_address_with_a_note_at_the_first() {
    let line = openssh::line(key(1));
    let block = format!("node \"10.0.0.5:7400\" {{\n  public_key = \"{line}\"\n}}\n");
    let text = format!("{block}{block}");
    let document = read(&text);
    let Err(diagnostics) = profile::decode(&document) else {
        panic!("a repeated node decodes");
    };
    let label = |i: usize| document.blocks[i].labels[0].span;
    let mut expected = Diagnostic::new(
        document::diagnostic::Code::new("config.repeated-node"),
        label(1),
        "the profile has another `node` block of 10.0.0.5:7400".into(),
        "Remove one of the two".into(),
    );
    expected.notes.push(document::diagnostic::Note {
        span: label(0).unwrap(),
        text: "the first `node` block".into(),
    });
    assert_eq!(diagnostics, [expected]);
}

#[test]
fn a_pin_keeps_each_byte_of_the_other_blocks_and_the_comments() {
    let line = openssh::line(key(1));
    let text = format!(
        "# the plant\nsubject = \"alice\" # me\n\n\
         node \"10.0.0.5:7400\" {{\n  # the first\n  public_key = \"{line}\"\n}}\n"
    );
    let at = address("[::1]:7401");
    let written = pinned(&text, at, key(2));
    assert!(written.starts_with(&text), "{written}");
    let nodes = profile::decode(&read(&written)).unwrap().nodes;
    assert_eq!(
        nodes,
        BTreeMap::from([(address("10.0.0.5:7400"), key(1)), (at, key(2))])
    );
}

proptest! {
    #[test]
    fn a_pin_reads_back_as_its_key(
        ip in any::<std::net::IpAddr>(),
        port in any::<u16>(),
        bytes in any::<[u8; 32]>(),
    ) {
        let at = SocketAddr::new(ip, port);
        let public_key = PrivateKey(bytes).public();
        let profile = profile::decode(&read(&pinned("", at, public_key))).unwrap();
        prop_assert_eq!(profile.nodes, BTreeMap::from([(at, public_key)]));
    }
}
