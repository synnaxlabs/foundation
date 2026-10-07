//! Most-specific-wins over the setting policies that select a name.

use types::name::{Name, Selector};

/// The two policies, the first two in key order of those tied, that select a name with
/// the top specificity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Tie {
    pub(crate) first: Name,
    pub(crate) second: Name,
}

/// The policy in `policies`, keyed by name, whose selector (from `select`) matches
/// `name` most specifically, or `None` when none matches. Keys are distinct: a key
/// given twice at the top specificity ties with itself.
pub(crate) fn resolve<'a, P>(
    name: &Name,
    policies: impl IntoIterator<Item = (&'a Name, &'a P)>,
    select: impl Fn(&P) -> &Selector,
) -> Result<Option<(&'a Name, &'a P)>, Tie> {
    let mut best = None;
    let mut top = Vec::new();
    for (key, policy) in policies {
        let Some(specificity) = select(policy).matches(name) else {
            continue;
        };
        if best < Some(specificity) {
            best = Some(specificity);
            top.clear();
        }
        if best == Some(specificity) {
            top.push((key, policy));
        }
    }
    top.sort_unstable_by_key(|(key, _)| *key);
    match top.as_slice() {
        [] => Ok(None),
        [winner] => Ok(Some(*winner)),
        [(first, _), (second, _), ..] => Err(Tie {
            first: (*first).clone(),
            second: (*second).clone(),
        }),
    }
}
