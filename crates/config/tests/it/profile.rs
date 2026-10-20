//! The profile of a program on a client host, as `config_hcl` reads and updates it.

use std::collections::BTreeMap;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};

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

const PASTED_KEY: &str = "Remove the private key from this file now, and write \
                          the path of its key file as `private_key`";

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
                "config.bad-strict",
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
        problems("subject = \"alice\"\nprivate_key = 3\n"),
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
                "Use the line that `foundation profile` prints",
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
fn refuses_a_private_key_with_no_subject_at_its_key() {
    let document = read("strict = true\nprivate_key = \"a.key\"\n");
    let at = document.attributes.get("private_key").unwrap().key_span;
    assert_eq!(
        profile::decode(&document),
        Err(vec![Diagnostic::new(
            document::diagnostic::Code::new("config.private-key-without-subject"),
            at,
            "the profile names `private_key` with no `subject`".into(),
            "Add `subject`, the name whose key the file holds, or remove `private_key`"
                .into(),
        )])
    );
}

/// A node key of another algorithm names a node, not a subject, and the fix of a
/// node.
#[test]
fn refuses_an_ssh_rsa_node_key_with_the_fix_of_a_node() {
    let text =
        "node \"10.0.0.5:7400\" {\n  public_key = \"ssh-rsa AAAAB3NzaC1yc2E\"\n}\n";
    assert_eq!(
        problems(text),
        [problem(
            "config.public-key-algorithm",
            "the public key is \"ssh-rsa\", and a node takes only `ssh-ed25519`",
            "Use the line that `foundation profile` prints",
        )]
    );
}

/// A node key that is not a string has the fix of a node.
#[test]
fn refuses_a_node_key_that_is_not_a_string() {
    let text = "node \"10.0.0.5:7400\" {\n  public_key = 3\n}\n";
    assert_eq!(
        problems(text),
        [problem(
            "config.bad-public-key",
            "a public key is a string, not an integer",
            "Use the line that `foundation profile` prints",
        )]
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
        document::diagnostic::Code::new("config.duplicate-node"),
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

/// A pin of a link-local address with a scope reads back as its address.
#[test]
fn a_pin_with_a_scope_reads_back() {
    let at = SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
        7400,
        0,
        3,
    ));
    let nodes = profile::decode(&read(&pinned("", at, key(1))))
        .unwrap()
        .nodes;
    assert_eq!(nodes, BTreeMap::from([(at, key(1))]));
}

/// `node` writes no flow label, so its block reads back with the flow label 0.
#[test]
fn a_pin_with_a_flow_label_reads_back_with_none() {
    let at = SocketAddrV6::new(Ipv6Addr::LOCALHOST, 7400, 5, 0);
    let nodes = profile::decode(&read(&pinned("", at.into(), key(1))))
        .unwrap()
        .nodes;
    let read = SocketAddrV6::new(Ipv6Addr::LOCALHOST, 7400, 0, 0);
    assert_eq!(nodes, BTreeMap::from([(read.into(), key(1))]));
}

/// A pin into a file of each odd end keeps its bytes and reads back with each other
/// pin.
#[test]
fn a_pin_into_odd_files_reads_back() {
    let line = openssh::line(key(1));
    let first = address("10.0.0.5:7400");
    let at = address("10.0.0.6:7400");
    let block = format!("node \"10.0.0.5:7400\" {{\n  public_key = \"{line}\"\n}}");
    for text in [
        "subject = \"alice\"".to_string(),
        "subject = \"alice\" # me".to_string(),
        "# only a comment".to_string(),
        "subject = \"alice\"\r\n".to_string(),
        block.clone(),
        format!("{block} # c"),
        format!("{}\r\n", block.replace('\n', "\r\n")),
    ] {
        let written = pinned(&text, at, key(2));
        let nodes = profile::decode(&read(&written)).unwrap().nodes;
        assert_eq!(nodes.get(&at), Some(&key(2)), "{text:?} -> {written:?}");
        assert_eq!(
            text.contains("node"),
            nodes.contains_key(&first),
            "{written:?}"
        );
        assert!(
            written.starts_with(text.trim_end()),
            "{text:?} -> {written:?}"
        );
    }
}

/// The text of a private key file pasted as `private_key`, which the attribute name
/// invites, gives only the alarm, and the alarm quotes none of it.
#[test]
fn a_private_key_pasted_as_the_path_is_only_an_alarm() {
    let file = openssh::key_file(&PrivateKey([1; 32]), "");
    let text = format!("private_key = <<EOT\n{file}EOT\nstrict = 3\n");
    let problems = problems(&text);
    assert_eq!(
        problems
            .iter()
            .map(|(code, _, fix)| (*code, fix.as_str()))
            .collect::<Vec<_>>(),
        [("config.private-key", PASTED_KEY)]
    );
    let body = file.lines().nth(1).unwrap();
    assert!(!problems[0].1.contains(body), "{problems:?}");
}

/// A private key in a `node` block and one outside it each get their own fix, in
/// source order.
#[test]
fn each_alarm_gets_the_fix_of_its_place() {
    let file = openssh::key_file(&PrivateKey([1; 32]), "");
    let text = format!(
        "node \"10.0.0.5:7400\" {{\n  public_key = <<EOT\n{file}EOT\n}}\n\
         private_key = <<EOT\n{file}EOT\n"
    );
    let fixes: Vec<_> = problems(&text).into_iter().map(|(_, _, fix)| fix).collect();
    let node = "Remove the private key from this file now, and use the line that \
                `foundation profile` prints";
    assert_eq!(fixes, [node, PASTED_KEY]);
}

/// A private key pasted as a node's `public_key` or into its label gives only the
/// alarm, as in a config file, so no message quotes it.
#[test]
fn a_private_key_in_a_node_block_is_only_an_alarm() {
    let file = openssh::key_file(&PrivateKey([1; 32]), "");
    let body = file.lines().nth(1).unwrap();
    let line = openssh::line(key(1));
    for text in [
        format!("node \"10.0.0.5:7400\" {{\n  public_key = <<EOT\n{file}EOT\n}}\n"),
        format!(
            "node \"-----BEGIN OPENSSH PRIVATE KEY----- {body}\" {{\n  \
             public_key = \"{line}\"\n}}\n"
        ),
    ] {
        let problems = problems(&text);
        let codes: Vec<_> = problems
            .iter()
            .map(|(code, _, fix)| (*code, fix.as_str()))
            .collect();
        let fix = "Remove the private key from this file now, and use the line that \
                   `foundation profile` prints";
        assert_eq!(codes, [("config.private-key", fix)], "{text}");
        assert!(!problems[0].1.contains(body), "{problems:?}");
    }
}
