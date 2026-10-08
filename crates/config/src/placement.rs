use std::slice;

use document::diagnostic::{Code, Diagnostic};
use document::{Block, Span, read, value};
use spec::definition;
use spec::placement::{Error, Nodes, Policy};
use types::name::Name;

use crate::{Definition, Found, span};

const EMPTY_PLACEMENT: Code = Code::new("config.empty-placement");
const ROLE_OVERLAP: Code = Code::new("config.role-overlap");
const KEYS: [&str; 4] = ["select", "home", "standby", "copies"];
const ROLES: [&str; 3] = ["home", "standby", "copies"];

/// Checks a `placement` block and gives its policy.
pub(crate) fn check(found: &mut Found<'_>, block: &Block) -> Option<Definition> {
    let unknown = found.unknown(block, &KEYS);
    let select =
        found.select(block, "connectors and indexes that it places", "site_a.*");
    let home = found.attribute(block, "home", read::name);
    let standby = found.attribute(block, "standby", read::name);
    let copies = found.attribute(block, "copies", read::names);
    let (Ok(()), Ok(select), Ok(home), Ok(standby), Ok(copies)) =
        (unknown, select, home, standby, copies)
    else {
        return None;
    };
    let nodes = Nodes {
        home,
        standby,
        copies: copies.unwrap_or_default(),
    };
    match Policy::new(select, nodes.clone()) {
        Ok(policy) => {
            found.nodes.extend(named(block));
            Some(Definition::Spec(definition::Definition::Placement(policy)))
        }
        Err(error) => {
            refuse(found, block, &nodes, &error);
            None
        }
    }
}

/// Reports a policy that `Policy::new` refuses for `nodes`.
fn refuse(found: &mut Found<'_>, block: &Block, nodes: &Nodes, error: &Error) {
    let diagnostic = match error {
        Error::Empty => Diagnostic::new(
            EMPTY_PLACEMENT,
            span(block, "copies").or(block.keyword_span),
            format!(
                "the `{}` block names no home, no standby, and no copy",
                block.keyword
            ),
            "Name a `home`, a `standby`, or a node in `copies`".into(),
        ),
        Error::Overlap(node) => {
            let roles = [
                ("home", nodes.home.as_ref() == Some(node)),
                ("standby", nodes.standby.as_ref() == Some(node)),
                ("copies", nodes.copies.contains(node)),
            ];
            let last = roles
                .into_iter()
                .filter(|(_, named)| *named)
                .filter_map(|(role, _)| span(block, role))
                .max_by_key(|span| span.start().offset);
            Diagnostic::new(
                ROLE_OVERLAP,
                last,
                error.to_string(),
                format!("Keep {node} in one of `home`, `standby`, and `copies`"),
            )
        }
    };
    found.diagnostics.push(diagnostic);
}

/// Each node that `block` names, with its span. [`check`] has read each one.
fn named(block: &Block) -> impl Iterator<Item = (Name, Option<Span>)> + '_ {
    let values = ROLES
        .iter()
        .filter_map(|role| block.body.attributes.get(role))
        .flat_map(|attribute| match &attribute.value.kind {
            value::Kind::List(items) => items.as_slice(),
            _ => slice::from_ref(&attribute.value),
        });
    values.map(|value| {
        let node = read::name(value).expect("invariant: `check` reads each node");
        (node, value.span)
    })
}
