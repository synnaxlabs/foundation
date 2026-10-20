//! The `access` block, a policy that allows subjects to do actions on names.

use std::slice;

use document::diagnostic::{Code, Diagnostic};
use document::encoding::Checked;
use document::schema::{self, Attribute, Body, Presence};
use document::value::{Kind, Value};
use document::{Block, Document, Label, Map, read};
use spec::access::{Action, Actions, Error, Policy};
use spec::definition;
use types::authority::Authority;
use types::ed25519::PublicKey;
use types::name::Name;

use crate::{Definition, Found, openssh};

const BAD_ACTION: Code = Code::new("config.bad-action");
const EMPTY_ALLOW: Code = Code::new("config.empty-allow");
const BAD_AUTHORITY: Code = Code::new("config.bad-authority");
const AUTHORITY_WITHOUT_WRITE: Code = Code::new("config.authority-without-write");
/// The `access` block.
pub(crate) const BLOCK: schema::Block = schema::Block {
    keyword: "access",
    label: Some("the name of the policy"),
    forms: &[BODY],
};

const BODY: Body = Body {
    summary: "Allows the subjects that it selects to do actions on names. A subject \
              may do what the union of its policies allows, and nothing else.",
    attributes: &[
        Attribute {
            name: "subjects",
            value: "a selector",
            presence: Presence::Required,
            summary: "The subjects that it allows.",
        },
        Attribute {
            name: "select",
            value: "a selector",
            presence: Presence::Required,
            summary: "The names that it allows them to use.",
        },
        Attribute {
            name: "allow",
            value: "an action, or a list of them: \"read\", \"write\", \"plan\", \
                    \"apply\", \"secret\", or \"admin\"",
            presence: Presence::Required,
            summary: "The actions that it allows.",
        },
        Attribute {
            name: "authority",
            value: "an integer, 0 to 255",
            presence: Presence::Default("0"),
            summary: "The highest control authority that a write may claim. Only a \
                      policy that allows \"write\" takes it.",
        },
    ],
    blocks: &[],
    examples: &[EXAMPLE],
};

const EXAMPLE: &str = "\
access \"site_a_operators\" {
  subjects  = \"site_a.operators.*\"
  select    = \"site_a.**\"
  allow     = [\"read\", \"write\"]
  authority = 100
}
";

/// The word of each action in a Document.
const ACTIONS: [(&str, Action); 6] = [
    ("read", Action::Read),
    ("write", Action::Write),
    ("plan", Action::Plan),
    ("apply", Action::Apply),
    ("secret", Action::Secret),
    ("admin", Action::Admin),
];

/// Checks an `access` block and gives its policy. With no `authority`, a write is
/// capped at the least authority. An `authority` with no `write` is refused.
pub(crate) fn check(
    found: &mut Found<'_>,
    block: &Block,
    _: Option<&Name>,
) -> Option<Definition> {
    let unknown = found.unknown(block, &BODY);
    let fix = "Add a `subjects` attribute with the subjects that it allows, such as \
               \"site_a.operators.*\"";
    let subjects = found.required(block, "subjects", read::selector, fix.into());
    let select = found.select(block, "names that it allows them to use", "site_a.**");
    let fix =
        "Add an `allow` attribute with the actions that it allows, such as \"read\"";
    let allow = found.required(block, "allow", actions, fix.into());
    let authority = found.attribute(block, "authority", authority);
    let (Ok(()), Ok(subjects), Ok(select), Ok(allow), Ok(authority)) =
        (unknown, subjects, select, allow, authority)
    else {
        return None;
    };
    let policy =
        match Policy::new(subjects, select, allow, authority.unwrap_or(Authority(0))) {
            Ok(policy) => policy,
            Err(Error::Empty) => {
                let at = block.body.attributes.get("allow");
                found.diagnostics.push(Diagnostic::new(
                    EMPTY_ALLOW,
                    at.and_then(|allow| allow.value.span),
                    "the `allow` list holds no action".into(),
                    "Add one or more actions, such as \"read\"".into(),
                ));
                return None;
            }
        };
    if authority.is_some() && !allow.contains(Action::Write) {
        let at = block.body.attributes.get("authority");
        found.diagnostics.push(Diagnostic::new(
            AUTHORITY_WITHOUT_WRITE,
            at.and_then(|authority| authority.value.span),
            "the policy has an `authority` and no `write` in `allow`, and only a \
             write uses an authority"
                .into(),
            "Add `write` to `allow`, or remove `authority`".into(),
        ));
        return None;
    }
    Some(Definition::Spec(definition::Definition::Access(policy)))
}

/// Reads one action or a list of actions, each a string or a reference.
fn actions(value: &Value) -> Result<Actions, Diagnostic> {
    let items = match &value.kind {
        Kind::List(items) => items,
        _ => slice::from_ref(value),
    };
    items.iter().map(action).collect()
}

fn action(value: &Value) -> Result<Action, Diagnostic> {
    let refuse = |message| {
        let words = ACTIONS.map(|(word, _)| word);
        let fix = format!("Use {}", read::one_of(&words));
        Diagnostic::new(BAD_ACTION, value.span, message, fix)
    };
    let word = value.kind.text().ok_or_else(|| {
        refuse(format!(
            "an action is a string or a reference, not {}",
            value.kind.noun()
        ))
    })?;
    ACTIONS
        .iter()
        .find(|(known, _)| *known == word)
        .map(|(_, action)| *action)
        .ok_or_else(|| refuse(format!("{word:?} is not an action")))
}

fn authority(value: &Value) -> Result<Authority, Diagnostic> {
    let message = match &value.kind {
        Kind::Integer(n) => match u8::try_from(*n) {
            Ok(n) => return Ok(Authority(n)),
            Err(_) => format!("the authority {n} is not from 0 to 255"),
        },
        kind => format!("an authority is an integer, not {}", kind.noun()),
    };
    let fix = "Write an integer from 0 to 255".into();
    Err(Diagnostic::new(BAD_AUTHORITY, value.span, message, fix))
}

/// The document of the `subject` and `access` blocks that admit `subject` with
/// `public_key` to read each channel.
#[must_use]
pub fn admit(subject: &Name, public_key: PublicKey) -> Checked {
    let string = |text: &str| Value {
        kind: Kind::String(text.into()),
        span: None,
    };
    let keys = [("keys", string(&openssh::line(public_key)))];
    let read = Value {
        kind: Kind::List(vec![string("read")]),
        span: None,
    };
    let policy = [
        ("subjects", string(subject.as_str())),
        ("select", string("**")),
        ("allow", read),
    ];
    let blocks = vec![
        block("subject", subject, keys.into()),
        block("access", subject, policy.into()),
    ];
    Checked::new(Document {
        attributes: Map::default(),
        blocks,
    })
    .unwrap_or_else(|_| unreachable!("invariant: two blocks of strings are shallow"))
}

/// The block `keyword` labeled `label`, of `attributes`.
fn block(keyword: &str, label: &Name, attributes: Vec<(&str, Value)>) -> Block {
    let attributes = attributes
        .into_iter()
        .map(|(key, value)| document::Attribute {
            key: key.into(),
            key_span: None,
            value,
        });
    Block {
        keyword: keyword.into(),
        keyword_span: None,
        labels: vec![Label {
            text: label.as_str().into(),
            span: None,
        }],
        body: Document {
            attributes: Map::new(attributes.collect()).unwrap_or_else(|_| {
                unreachable!("invariant: each key is written once")
            }),
            blocks: Vec::new(),
        },
        span: None,
    }
}
