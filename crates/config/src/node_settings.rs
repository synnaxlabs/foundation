use document::diagnostic::{Code, Diagnostic};
use document::{Block, read};
use spec::definition;
use spec::node_settings::{Error, Policy};

use crate::{Definition, Found};

const ZERO_SIZE: Code = Code::new("config.zero-size");
const KEYS: [&str; 3] = ["select", "disk", "pool"];

/// Checks a `node_settings` block and gives its policy. An unknown attribute stops the
/// budget check, because it may be a budget under a wrong key.
pub(crate) fn check(found: &mut Found<'_>, block: &Block) -> Option<Definition> {
    let unknown = found.unknown(block, &KEYS);
    let select = found.select(block, "nodes that it sets", "site_a.*");
    let disk = found.attribute(block, "disk", read::size);
    let pool = found.attribute(block, "pool", read::size);
    let (Ok(()), Ok(select), Ok(disk), Ok(pool)) = (unknown, select, disk, pool) else {
        return None;
    };
    match Policy::new(select, disk, pool) {
        Ok(policy) => Some(Definition::Spec(definition::Definition::NodeSettings(
            policy,
        ))),
        Err(error) => {
            refuse(found, block, error);
            None
        }
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
