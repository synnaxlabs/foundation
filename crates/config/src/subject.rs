use std::slice;

use base64ct::{Base64, Encoding};
use document::Block;
use document::diagnostic::{Code, Diagnostic, Note};
use document::value::{Kind, Value};
use spec::definition;
use spec::subject::{Error, Subject};
use types::ed25519::PublicKey;

use crate::{Definition, Found};

const BAD_PUBLIC_KEY: Code = Code::new("config.bad-public-key");
const PUBLIC_KEY_ALGORITHM: Code = Code::new("config.public-key-algorithm");
const PRIVATE_KEY: Code = Code::new("config.private-key");
const NO_PUBLIC_KEYS: Code = Code::new("config.no-public-keys");
const DUPLICATE_PUBLIC_KEY: Code = Code::new("config.duplicate-public-key");
/// Text that only a private key holds: the OpenSSH, PEM, and RFC 4716 forms, and a
/// `.ppk` file of `PuTTYgen`.
const PRIVATE_MARKS: [&str; 2] = ["PRIVATE KEY", "PuTTY-User-Key-File"];
const KEYS: [&str; 1] = ["keys"];
const ALGORITHM: &str = "ssh-ed25519";
/// The name of each other algorithm of an OpenSSH public key. A message names only
/// these, since another first word can be a secret.
const OTHER_ALGORITHMS: [&str; 17] = [
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
/// Each Unicode line break.
const LINE_BREAKS: [char; 7] =
    ['\n', '\x0b', '\x0c', '\r', '\u{85}', '\u{2028}', '\u{2029}'];
const NOT_A_LINE: &str = "the public key is not the line of a `.pub` file";
/// The decoded key of an Ed25519 line starts with the length and the name of its
/// algorithm, then the length of the key.
const BLOB_START: &[u8; 19] = b"\0\0\0\x0bssh-ed25519\0\0\0\x20";
/// The length of the decoded key of an Ed25519 line: [`BLOB_START`] and the key.
const BLOB_BYTES: usize = 51;

/// Checks a `subject` block and gives its subject.
pub(crate) fn check(found: &mut Found<'_>, block: &Block) -> Option<Definition> {
    let unknown = found.unknown(block, &KEYS);
    let fix = "Add a `keys` attribute with the line of a `.pub` file, such as \
               \"ssh-ed25519 AAAA... alice@laptop\"";
    let subject = found.required(block, "keys", subject, fix.into());
    let (Ok(()), Ok(subject)) = (unknown, subject) else {
        return None;
    };
    Some(Definition::Spec(definition::Definition::Subject(subject)))
}

/// Reads one public key or a list of them as a subject.
fn subject(value: &Value) -> Result<Subject, Diagnostic> {
    let items = match &value.kind {
        Kind::List(items) => items.as_slice(),
        _ => slice::from_ref(value),
    };
    no_private_key(value)?;
    let keys = items.iter().map(key).collect::<Result<_, _>>()?;
    Subject::new(keys).map_err(|error| {
        let (message, fix) = (error.to_string(), error.fix().into());
        match error {
            Error::Empty => Diagnostic::new(NO_PUBLIC_KEYS, value.span, message, fix),
            Error::Duplicate { first, second } => {
                let span = items[second].span;
                let mut diagnostic =
                    Diagnostic::new(DUPLICATE_PUBLIC_KEY, span, message, fix);
                diagnostic.notes.extend(items[first].span.map(|span| Note {
                    span,
                    text: "the earlier key".into(),
                }));
                diagnostic
            }
        }
    })
}

/// Refuses a value that holds a private key at any depth. The whole value is checked
/// before any key is read, so a bad item before a private key does not hide the alarm.
fn no_private_key(value: &Value) -> Result<(), Diagnostic> {
    match &value.kind {
        Kind::String(text) if PRIVATE_MARKS.iter().any(|mark| text.contains(mark)) => {
            Err(Diagnostic::new(
                PRIVATE_KEY,
                value.span,
                "the value is a private key, which must never be in a file".into(),
                "Remove the private key from this file now, and use the one line of \
                 its `.pub` file"
                    .into(),
            ))
        }
        Kind::List(items) => items.iter().try_for_each(no_private_key),
        Kind::Map(map) => map.iter().try_for_each(|item| no_private_key(&item.value)),
        Kind::Call(call) => call.arguments.iter().try_for_each(no_private_key),
        Kind::Bool(_)
        | Kind::Integer(_)
        | Kind::Float(_)
        | Kind::String(_)
        | Kind::Reference(_) => Ok(()),
    }
}

/// Reads the line of an OpenSSH `.pub` file of an Ed25519 key: `ssh-ed25519`, the
/// base64 of the key, and a comment, which is optional and not kept. A message quotes
/// no part of the value after its first word, since that part can be a secret.
fn key(value: &Value) -> Result<PublicKey, Diagnostic> {
    let bad = |message: &str| {
        let fix = "Use the one line of a `.pub` file, such as `ssh-ed25519 AAAA... \
                   alice@laptop`";
        Diagnostic::new(BAD_PUBLIC_KEY, value.span, message.into(), fix.into())
    };
    let Kind::String(text) = &value.kind else {
        let noun = value.kind.noun();
        return Err(bad(&format!("a public key is a string, not {noun}")));
    };
    let mut words = text.split_ascii_whitespace();
    let (Some(algorithm), Some(encoded)) = (words.next(), words.next()) else {
        return Err(bad(NOT_A_LINE));
    };
    if algorithm != ALGORITHM {
        if !OTHER_ALGORITHMS.contains(&algorithm) {
            return Err(bad(NOT_A_LINE));
        }
        return Err(Diagnostic::new(
            PUBLIC_KEY_ALGORITHM,
            value.span,
            format!(
                "the public key is {algorithm:?}, and a subject takes only \
                 `{ALGORITHM}`"
            ),
            "Make an Ed25519 key with `ssh-keygen -t ed25519`, and use the line of its \
             `.pub` file"
                .into(),
        ));
    }
    if text.trim().contains(LINE_BREAKS) {
        return Err(bad("the public key is more than one line"));
    }
    let mut blob = [0; BLOB_BYTES];
    let bytes = Base64::decode(encoded, &mut blob)
        .ok()
        .and_then(|blob| blob.strip_prefix(BLOB_START))
        .and_then(|key| <[u8; 32]>::try_from(key).ok())
        .ok_or_else(|| bad("the base64 of the public key is not an Ed25519 key"))?;
    PublicKey::new(bytes).map_err(|error| bad(&error.to_string()))
}
