//! The profile of a program on a client host: the subject that it acts as, and the
//! public key of each node that it connects to.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::net::SocketAddr;
use std::path::PathBuf;

use document::diagnostic::{Code, Diagnostic, Note};
use document::schema::{self, Body, Presence};
use document::value::{Kind, Value};
use document::{Attribute, Block, Document, Label, Map, read};
use types::ed25519::PublicKey;
use types::name::Name;

use crate::openssh::{self, Holder};

const BAD_ADDRESS: Code = Code::new("config.bad-address");
const BAD_PATH: Code = Code::new("config.bad-path");
const BAD_STRICT: Code = Code::new("config.bad-strict");
const DUPLICATE_NODE: Code = Code::new("config.duplicate-node");
const PRIVATE_KEY_WITHOUT_SUBJECT: Code =
    Code::new("config.private-key-without-subject");
const PRINTED: &str = "Use the line that `foundation profile` prints";
const NODE_KEY: Holder = Holder {
    noun: "a node",
    fix: PRINTED,
    algorithm_fix: PRINTED,
};

/// The fix of a pasted private key: the one key that a profile names is the key file
/// of its subject.
const PASTED_KEY: &str = "Remove the private key from this file now, and write \
                          the path of its key file as `private_key`";
const PASTED_NODE_KEY: &str = "Remove the private key from this file now, and use the \
                               line that `foundation profile` prints";
const BODY: Body = Body {
    summary: "The subject that a program on a client host acts as, and the public \
              key of each node that it connects to.",
    attributes: &[
        schema::Attribute {
            name: "subject",
            value: "a name",
            presence: Presence::Optional,
            summary: "The subject that the program acts as. Without it, `connect` \
                      takes the next source: the first admin of the data directory, \
                      else the login name of the user.",
        },
        schema::Attribute {
            name: "private_key",
            value: "the path of an OpenSSH Ed25519 private key file",
            presence: Presence::Optional,
            summary: "The key file of `subject`, which it needs. A relative path is \
                      relative to the directory of the profile, and a path that \
                      starts with `~/` to the home directory. Without it, \
                      `~/.foundation/<subject>.key`, made when it is not there.",
        },
        schema::Attribute {
            name: "strict",
            value: "a bool",
            presence: Presence::Default("false"),
            summary: "Whether a node that no source pins is refused, not trusted on \
                      first use.",
        },
    ],
    blocks: &[NODE],
    examples: &[EXAMPLE],
};

const NODE: schema::Block = schema::Block {
    keyword: "node",
    label: Some("the address that the program dials, as ip:port"),
    forms: &[NODE_BODY],
};

const NODE_BODY: Body = Body {
    summary: "The public key of the node at an address.",
    attributes: &[schema::Attribute {
        name: "public_key",
        value: "the line of an OpenSSH ed25519 `.pub` file",
        presence: Presence::Required,
        summary: "The public key that the node proves in the handshake.",
    }],
    blocks: &[],
    examples: &[],
};

const EXAMPLE: &str = "\
subject = \"alice\"
strict  = false

node \"10.0.0.5:7400\" {
  public_key = \"ssh-ed25519 \
              AAAAC3NzaC1lZDI1NTE5AAAAIGVVuOR8JKYpAcWLMUveadmJ1wUAmYGgIDtqlhFe7Yhg\"
}
";

/// What a profile gives a program on a client host: its subject, and the public key of
/// each node that it connects to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    /// The subject that a program acts as, if the profile names one.
    pub subject: Option<Name>,
    /// The key file of `subject` as the profile writes it, if it names one. A relative
    /// path is relative to the directory of the profile, and a path that starts with
    /// `~/` to the home directory; the program joins it. When it is `Some`, so is
    /// `subject`.
    pub private_key: Option<PathBuf>,
    /// Whether a node that no source pins is refused, not trusted on first use.
    pub strict: bool,
    /// The public key that each node proves in the handshake, by the address that a
    /// program dials.
    pub nodes: BTreeMap<SocketAddr, PublicKey>,
}

/// The profile that `document` holds.
///
/// # Errors
///
/// Only `config.private-key` at each string that holds a private key, when one does.
/// Else the diagnostic of each problem, by span: an attribute or block that a profile
/// does not take, a value of a wrong type or that does not parse, a `private_key` with
/// no `subject`, a `node` block without one label or its `public_key`, and each later
/// `node` block of an address.
pub fn decode(document: &Document) -> Result<Profile, Vec<Diagnostic>> {
    let alarms = alarms(document);
    if !alarms.is_empty() {
        return Err(alarms);
    }
    let mut diagnostics = read::unknown(document, "the profile", &BODY);
    let mut profile = Profile::default();
    let attributes = &document.attributes;
    let mut keep = |read: Result<(), Diagnostic>| {
        if let Err(diagnostic) = read {
            diagnostics.push(diagnostic);
        }
    };
    if let Some(subject) = attributes.get("subject") {
        keep(read::name(&subject.value).map(|name| profile.subject = Some(name)));
    }
    if let Some(path) = attributes.get("private_key") {
        keep(self::path(&path.value).map(|path| profile.private_key = Some(path)));
        if attributes.get("subject").is_none() {
            keep(Err(Diagnostic::new(
                PRIVATE_KEY_WITHOUT_SUBJECT,
                path.key_span,
                "the profile names `private_key` with no `subject`".into(),
                "Add `subject`, the name whose key the file holds, or remove \
                 `private_key`"
                    .into(),
            )));
        }
    }
    if let Some(value) = attributes.get("strict") {
        keep(strict(&value.value).map(|strict| profile.strict = strict));
    }
    let mut firsts = BTreeMap::new();
    for block in &document.blocks {
        if &*block.keyword != NODE.keyword {
            continue;
        }
        let Some((address, public_key)) = node_block(block, &mut diagnostics) else {
            continue;
        };
        match firsts.entry(address) {
            Entry::Vacant(first) => {
                first.insert(block.labels[0].span);
                profile.nodes.insert(address, public_key);
            }
            Entry::Occupied(first) => {
                diagnostics.push(duplicate(address, block, *first.get()));
            }
        }
    }
    if diagnostics.is_empty() {
        return Ok(profile);
    }
    crate::sort(&mut diagnostics);
    Err(diagnostics)
}

/// `config.private-key` at each string of `document` that holds a private key, in
/// source order, with the fix of its place: in a `node` block or outside one.
fn alarms(document: &Document) -> Vec<Diagnostic> {
    let (nodes, rest) = document
        .blocks
        .iter()
        .cloned()
        .partition(|block| *block.keyword == *NODE.keyword);
    let rest = Document {
        attributes: document.attributes.clone(),
        blocks: rest,
    };
    let nodes = Document {
        blocks: nodes,
        ..Document::default()
    };
    let mut alarms = Vec::new();
    for (part, fix) in [(rest, PASTED_KEY), (nodes, PASTED_NODE_KEY)] {
        for mut alarm in crate::private_key::alarms(&[part]) {
            alarm.fix = fix.into();
            alarms.push(alarm);
        }
    }
    crate::sort(&mut alarms);
    alarms
}

/// The `node` block that pins `public_key` at `address`. [`decode`] reads it back, with
/// the flow label of a V6 address as 0.
#[must_use]
#[expect(
    clippy::missing_panics_doc,
    reason = "a map of one attribute has no repeated key"
)]
pub fn node(address: SocketAddr, public_key: PublicKey) -> Block {
    let attribute = Attribute {
        key: "public_key".into(),
        key_span: None,
        value: Value {
            kind: Kind::String(openssh::line(public_key).into()),
            span: None,
        },
    };
    Block {
        keyword: NODE.keyword.into(),
        keyword_span: None,
        labels: vec![Label {
            text: address.to_string().into(),
            span: None,
        }],
        body: Document {
            attributes: Map::new(vec![attribute])
                .expect("invariant: each key is written once"),
            blocks: Vec::new(),
        },
        span: None,
    }
}

/// The address and the public key of a `node` block, or `None` with each problem in
/// `diagnostics`.
fn node_block(
    block: &Block,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<(SocketAddr, PublicKey)> {
    let of = "the `node` block";
    diagnostics.extend(read::unknown(&block.body, of, &NODE_BODY));
    let fix = "Give the block one label, the address of the node, such as \
               \"10.0.0.5:7400\"";
    let address = read::labels::<1>(block, fix.into()).and_then(|[label]| {
        label.text.parse::<SocketAddr>().map_err(|error| {
            Diagnostic::new(
                BAD_ADDRESS,
                label.span,
                format!("the address {:?} is not ip:port: {error}", label.text),
                "Write the address that the program dials, such as \
                 \"10.0.0.5:7400\" or \"[::1]:7400\""
                    .into(),
            )
        })
    });
    let fix = "Add a `public_key` attribute with the line that `foundation profile` \
               prints";
    let at = block.keyword_span;
    let public_key = read::required(
        &block.body,
        of,
        at,
        "public_key",
        |value| openssh::key(value, NODE_KEY),
        fix.into(),
    );
    match (address, public_key) {
        (Ok(address), Ok(public_key)) => Some((address, public_key)),
        (address, public_key) => {
            diagnostics.extend(address.err());
            diagnostics.extend(public_key.err());
            None
        }
    }
}

/// Reads a path: a string that is not empty.
fn path(value: &Value) -> Result<PathBuf, Diagnostic> {
    let fix =
        "Write the path of the key file, such as \"/home/alice/.foundation/alice.key\"";
    match &value.kind {
        Kind::String(text) if !text.is_empty() => Ok(PathBuf::from(&**text)),
        Kind::String(_) => Err(Diagnostic::new(
            BAD_PATH,
            value.span,
            "the path is empty".into(),
            fix.into(),
        )),
        kind => Err(Diagnostic::new(
            BAD_PATH,
            value.span,
            format!("a path is a string, not {}", kind.noun()),
            fix.into(),
        )),
    }
}

/// Reads `strict`: `true` or `false`.
fn strict(value: &Value) -> Result<bool, Diagnostic> {
    match value.kind {
        Kind::Bool(strict) => Ok(strict),
        ref kind => Err(Diagnostic::new(
            BAD_STRICT,
            value.span,
            format!("`strict` is a bool, not {}", kind.noun()),
            "Write true or false".into(),
        )),
    }
}

/// `config.duplicate-node` at the label of `block`, a later `node` block of `address`,
/// with a note at `first`, the label of the first.
fn duplicate(
    address: SocketAddr,
    block: &Block,
    first: Option<document::Span>,
) -> Diagnostic {
    let mut diagnostic = Diagnostic::new(
        DUPLICATE_NODE,
        block.labels[0].span,
        format!("the profile has another `node` block of {address}"),
        "Remove one of the two".into(),
    );
    diagnostic.notes.extend(first.map(|span| Note {
        span,
        text: "the first `node` block".into(),
    }));
    diagnostic
}

#[cfg(test)]
mod tests {
    use document::Source;

    use super::*;

    /// The example of the reference decodes to what it shows.
    #[test]
    fn the_example_decodes() {
        let document =
            config_hcl::read(Source(0), EXAMPLE).expect("the example is HCL");
        let profile = decode(&document).expect("the example decodes");
        assert_eq!(profile.subject, Some("alice".parse().unwrap()));
        assert!(!profile.strict);
        let address: SocketAddr = "10.0.0.5:7400".parse().unwrap();
        assert_eq!(profile.nodes.keys().collect::<Vec<_>>(), [&address]);
    }
}
