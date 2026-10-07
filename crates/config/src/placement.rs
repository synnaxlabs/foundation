use document::diagnostic::{Code, Diagnostic};
use document::{Block, read};
use spec::placement::{Error, Nodes, Policy};

use crate::{Entry, Found};

const ROLE_OVERLAP: Code = Code::new("config.role-overlap");
const ROLES: [&str; 3] = ["home", "standby", "copies"];
const KEYS: [&str; 4] = ["select", "home", "standby", "copies"];

/// Checks a `placement` block and adds its policy to the entries.
pub(crate) fn check<'a>(found: &mut Found<'a>, block: &'a Block) {
    let key = found.key(block);
    let unknown = found.unknown_attributes(block, &KEYS);
    found.unknown_blocks(block);
    let select = found.attribute(block, "select", read::selector);
    let home = found.attribute(block, "home", read::name);
    let standby = found.attribute(block, "standby", read::name);
    let copies = found.attribute(block, "copies", read::names);
    if matches!(select, Ok(None)) {
        let fix = "Add a `select` attribute with the connectors and indexes that it \
                   places, such as \"site_a.*\"";
        found.missing(block, &["select"], fix.into());
    }
    let (Ok(()), Ok(Some(select)), Ok(home), Ok(standby), Ok(copies)) =
        (unknown, select, home, standby, copies)
    else {
        return;
    };
    let nodes = Nodes {
        home,
        standby,
        copies: copies.unwrap_or_default(),
    };
    match Policy::new(select, nodes) {
        Ok(policy) => {
            if let Some((key, label_span)) = key {
                let definition = spec::definition::Definition::Placement(policy);
                found.entries.insert(
                    key,
                    Entry {
                        definition,
                        label_span,
                    },
                );
            }
        }
        Err(error) => refuse(found, block, &error),
    }
}

/// Reports a policy that `Policy::new` refuses.
fn refuse(found: &mut Found<'_>, block: &Block, error: &Error) {
    match error {
        Error::Empty => {
            let fix = "Add a `home`, `standby`, or `copies` attribute";
            found.missing(block, &ROLES, fix.into());
        }
        Error::Overlap(node) => {
            let span = ROLES
                .into_iter()
                .filter_map(|role| block.body.attributes.get(role))
                .filter(|attribute| {
                    read::names(&attribute.value)
                        .is_ok_and(|names| names.contains(node))
                })
                .filter_map(|attribute| attribute.value.span)
                .max_by_key(|span| span.start().offset);
            found.diagnostics.push(Diagnostic::new(
                ROLE_OVERLAP,
                span,
                error.to_string(),
                format!("Keep {node} in one of `home`, `standby`, and `copies`"),
            ));
        }
    }
}
