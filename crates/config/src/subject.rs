use std::collections::{BTreeMap, BTreeSet};
use std::slice;

use document::diagnostic::{Code, Diagnostic, Note};
use document::value::{Kind, Value};
use document::{Block, Span};
use spec::definition;
use spec::subject::{Error, Subject};
use types::ed25519::PublicKey;
use types::name::Name;

use crate::openssh::{self, Error as Line};
use crate::{Definition, Found};

const BAD_PUBLIC_KEY: Code = Code::new("config.bad-public-key");
const PUBLIC_KEY_ALGORITHM: Code = Code::new("config.public-key-algorithm");
const NO_PUBLIC_KEYS: Code = Code::new("config.no-public-keys");
const DUPLICATE_PUBLIC_KEY: Code = Code::new("config.duplicate-public-key");
const SUBJECT_IS_CONNECTOR: Code = Code::new("config.subject-is-connector");
const KEYS: [&str; 1] = ["keys"];

/// Checks a `subject` block and gives its subject.
pub(crate) fn check(
    found: &mut Found<'_>,
    block: &Block,
    _: Option<&Name>,
) -> Option<Definition> {
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
    if let Some(connector) = found.connectors.get(&*label.text.to_ascii_lowercase()) {
        let diagnostic = named_connector(&label.text, label.span, connector.span);
        found.diagnostics.push(diagnostic);
    }
}

/// `config.subject-is-connector` at each subject of `definitions` whose label is the
/// name of a connector in any ASCII case, in name order.
pub(crate) fn not_connectors(
    definitions: &BTreeMap<Name, definition::Definition>,
) -> Vec<Diagnostic> {
    let connectors: BTreeSet<String> = definitions
        .iter()
        .filter(|(_, definition)| {
            matches!(definition, definition::Definition::Connector(_))
        })
        .map(|(key, _)| key.as_str().to_ascii_lowercase())
        .collect();
    let subjects = definitions
        .iter()
        .filter(|(_, definition)| {
            matches!(definition, definition::Definition::Subject(_))
        })
        .map(|(key, _)| crate::label(definition::Kind::Subject, key));
    subjects
        .filter(|label| connectors.contains(&label.as_str().to_ascii_lowercase()))
        .map(|label| named_connector(label.as_str(), None, None))
        .collect()
}

/// `config.subject-is-connector` at the subject `text`, with a note at `connector`.
fn named_connector(
    text: &str,
    at: Option<Span>,
    connector: Option<Span>,
) -> Diagnostic {
    let mut diagnostic = Diagnostic::new(
        SUBJECT_IS_CONNECTOR,
        at,
        format!("the subject {text:?} has the name of a connector"),
        "Rename the subject or the connector".into(),
    );
    diagnostic.notes.extend(connector.map(|span| Note {
        span,
        text: "the connector".into(),
    }));
    diagnostic
}

/// Reads one public key or a list of them as a subject.
fn subject(value: &Value) -> Result<Subject, Diagnostic> {
    let items = match &value.kind {
        Kind::List(items) => items.as_slice(),
        _ => slice::from_ref(value),
    };
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
