use document::diagnostic::{Code, Diagnostic};
use document::{Block, read};
use spec::node_settings::{Error, Policy};

use crate::{Entry, Found};

const ZERO_SIZE: Code = Code::new("config.zero-size");
const KEYS: [&str; 3] = ["select", "disk", "pool"];

/// Checks a `node_settings` block and adds its policy to the entries. An unknown
/// attribute stops the budget check, because it may be a budget under a wrong key.
pub(crate) fn check<'a>(found: &mut Found<'a>, block: &'a Block) {
    let key = found.key(block);
    let unknown = found.unknown_attributes(block, &KEYS);
    found.unknown_blocks(block);
    let select = found.attribute(block, "select", read::selector);
    let disk = found.attribute(block, "disk", read::size);
    let pool = found.attribute(block, "pool", read::size);
    if matches!(select, Ok(None)) {
        let fix = "Add a `select` attribute with the nodes that it sets, such as \
                   \"site_a.*\"";
        found.missing(block, &["select"], fix.into());
    }
    let (Ok(()), Ok(Some(select)), Ok(disk), Ok(pool)) = (unknown, select, disk, pool)
    else {
        return;
    };
    match Policy::new(select, disk, pool) {
        Ok(policy) => {
            if let Some((key, label_span)) = key {
                let definition = spec::definition::Definition::NodeSettings(policy);
                found.entries.insert(
                    key,
                    Entry {
                        definition,
                        label_span,
                    },
                );
            }
        }
        Err(error) => refuse(found, block, error),
    }
}

/// Reports a policy that `Policy::new` refuses.
fn refuse(found: &mut Found<'_>, block: &Block, error: Error) {
    let zero = |key: &str| {
        let at = block.body.attributes.get(key);
        Diagnostic::new(
            ZERO_SIZE,
            at.and_then(|attribute| attribute.value.span),
            format!("the `{key}` budget is zero"),
            format!("Write a size above zero, or remove `{key}`"),
        )
    };
    match error {
        Error::ZeroDisk => found.diagnostics.push(zero("disk")),
        Error::ZeroPool => found.diagnostics.push(zero("pool")),
        Error::NoBudget => {
            let fix = "Add a `disk` attribute, a `pool` attribute, or both, with a \
                       size such as \"10GiB\"";
            found.missing(block, &["disk", "pool"], fix.into());
        }
    }
}
