use document::diagnostic::{Code, Diagnostic};
use document::{Block, read};
use spec::definition::Definition;
use spec::placement::{Error, Nodes, Policy};

use crate::Found;

const EMPTY_PLACEMENT: Code = Code::new("config.empty-placement");
const ROLE_OVERLAP: Code = Code::new("config.role-overlap");
const KEYS: [&str; 4] = ["select", "home", "standby", "copies"];

/// Checks a `placement` block and adds its policy to the entries.
pub(crate) fn check<'a>(found: &mut Found<'a>, block: &'a Block) {
    let key = found.key(block);
    let unknown = found.unknown_attributes(block, &KEYS);
    found.unknown_blocks(block);
    let select = found.select(block, "connectors and indexes that it places");
    let home = found.attribute(block, "home", read::name);
    let standby = found.attribute(block, "standby", read::name);
    let copies = found.attribute(block, "copies", read::names);
    let (Ok(()), Ok(select), Ok(home), Ok(standby), Ok(copies)) =
        (unknown, select, home, standby, copies)
    else {
        return;
    };
    let nodes = Nodes {
        home,
        standby,
        copies: copies.unwrap_or_default(),
    };
    match Policy::new(select, nodes.clone()) {
        Ok(policy) => found.add(key, Definition::Placement(policy)),
        Err(error) => refuse(found, block, &nodes, &error),
    }
}

/// Reports a policy that `Policy::new` refuses for `nodes`.
fn refuse(found: &mut Found<'_>, block: &Block, nodes: &Nodes, error: &Error) {
    let span = |role: &str| block.body.attributes.get(role)?.value.span;
    let diagnostic = match error {
        Error::Empty => Diagnostic::new(
            EMPTY_PLACEMENT,
            span("copies").or(block.keyword_span),
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
                .filter_map(|(role, _)| span(role))
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
