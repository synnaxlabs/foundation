use std::slice;

use document::diagnostic::{Code, Diagnostic, Note};
use document::value::{Kind, Value};
use document::{Block, Span};
use spec::definition;
use spec::subject::{Error, Subject};
use types::ed25519::PublicKey;

use crate::openssh::{self, Error as Line};
use crate::{Definition, Found};

const BAD_PUBLIC_KEY: Code = Code::new("config.bad-public-key");
const PUBLIC_KEY_ALGORITHM: Code = Code::new("config.public-key-algorithm");
const PRIVATE_KEY: Code = Code::new("config.private-key");
const NO_PUBLIC_KEYS: Code = Code::new("config.no-public-keys");
const DUPLICATE_PUBLIC_KEY: Code = Code::new("config.duplicate-public-key");
const SUBJECT_IS_CONNECTOR: Code = Code::new("config.subject-is-connector");
/// Text that only a private key holds: the OpenSSH, PEM, and RFC 4716 forms, and a
/// `.ppk` file of `PuTTYgen`.
const PRIVATE_MARKS: [&str; 2] = ["PRIVATE KEY", "PuTTY-User-Key-File"];
const KEYS: [&str; 1] = ["keys"];

/// Checks a `subject` block and gives its subject.
pub(crate) fn check(found: &mut Found<'_>, block: &Block) -> Option<Definition> {
    let unknown = found.unknown(block, &KEYS);
    let fix = "Add a `keys` attribute with the line of a `.pub` file, such as \
               \"ssh-ed25519 AAAA... alice@laptop\"";
    let subject = found.required(block, "keys", subject, fix.into());
    not_connector(found, block);
    let (Ok(()), Ok(subject)) = (unknown, subject) else {
        return None;
    };
    Some(Definition::Spec(definition::Definition::Subject(subject)))
}

/// Refuses a subject at the name of a connector in any ASCII case, since a connector
/// is a subject that its node vouches for.
fn not_connector(found: &mut Found<'_>, block: &Block) {
    let [label] = block.labels.as_slice() else {
        return;
    };
    let Some(connector) = found.connectors.get(&*label.text.to_ascii_lowercase())
    else {
        return;
    };
    let mut diagnostic = Diagnostic::new(
        SUBJECT_IS_CONNECTOR,
        label.span,
        format!("the subject {:?} has the name of a connector", label.text),
        "Rename the subject or the connector".into(),
    );
    diagnostic.notes.extend(connector.span.map(|span| Note {
        span,
        text: "the connector".into(),
    }));
    found.diagnostics.push(diagnostic);
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

/// Refuses a value that holds a private key at any depth, in a string or a map key.
/// The whole value is checked before any key is read, so a bad item before a private
/// key does not hide the alarm.
fn no_private_key(value: &Value) -> Result<(), Diagnostic> {
    match &value.kind {
        Kind::String(text) => no_private_text(text, value.span),
        Kind::List(items) => items.iter().try_for_each(no_private_key),
        Kind::Map(map) => map.iter().try_for_each(|item| {
            no_private_text(&item.key, item.key_span)?;
            no_private_key(&item.value)
        }),
        Kind::Call(call) => call.arguments.iter().try_for_each(no_private_key),
        Kind::Bool(_) | Kind::Integer(_) | Kind::Float(_) | Kind::Reference(_) => {
            Ok(())
        }
    }
}

fn no_private_text(text: &str, span: Option<Span>) -> Result<(), Diagnostic> {
    if !PRIVATE_MARKS.iter().any(|mark| text.contains(mark)) {
        return Ok(());
    }
    Err(Diagnostic::new(
        PRIVATE_KEY,
        span,
        "the value is a private key, which must never be in a file".into(),
        "Remove the private key from this file now, and use the one line of its \
         `.pub` file"
            .into(),
    ))
}

/// Reads a public key, which is the line of an OpenSSH `.pub` file of an Ed25519 key.
fn key(value: &Value) -> Result<PublicKey, Diagnostic> {
    let fix = "Use the one line of a `.pub` file, such as `ssh-ed25519 AAAA... \
               alice@laptop`";
    let Kind::String(text) = &value.kind else {
        let noun = value.kind.noun();
        let message = format!("a public key is a string, not {noun}");
        return Err(Diagnostic::new(
            BAD_PUBLIC_KEY,
            value.span,
            message,
            fix.into(),
        ));
    };
    openssh::public_key(text).map_err(|error| {
        let (code, fix) = match error {
            Line::Algorithm(_) => (
                PUBLIC_KEY_ALGORITHM,
                "Make an Ed25519 key with `ssh-keygen -t ed25519`, and use the line of \
                 its `.pub` file",
            ),
            Line::NotALine | Line::Lines | Line::NotEd25519 | Line::SmallOrder(_) => {
                (BAD_PUBLIC_KEY, fix)
            }
        };
        Diagnostic::new(code, value.span, error.to_string(), fix.into())
    })
}
