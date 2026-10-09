use std::slice;

use document::diagnostic::{Code, Diagnostic};
use document::value::{Kind, Value};
use document::{Block, read};
use spec::access::{Action, Actions, Policy};
use spec::definition;
use types::authority::Authority;
use types::name::Name;

use crate::{Definition, Found};

const BAD_ACTION: Code = Code::new("config.bad-action");
const EMPTY_ALLOW: Code = Code::new("config.empty-allow");
const BAD_AUTHORITY: Code = Code::new("config.bad-authority");
const AUTHORITY_WITHOUT_WRITE: Code = Code::new("config.authority-without-write");
const KEYS: [&str; 4] = ["subjects", "select", "allow", "authority"];

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
    let unknown = found.unknown(block, &KEYS);
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
    let authority = authority.unwrap_or(Authority(0));
    let policy = Policy::new(subjects, select, allow, authority);
    Some(Definition::Spec(definition::Definition::Access(policy)))
}

/// Reads one action or a list of actions, each a string or a reference.
fn actions(value: &Value) -> Result<Actions, Diagnostic> {
    let items = match &value.kind {
        Kind::List(items) if items.is_empty() => {
            return Err(Diagnostic::new(
                EMPTY_ALLOW,
                value.span,
                "the `allow` list holds no action".into(),
                "Add one or more actions, such as \"read\"".into(),
            ));
        }
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
