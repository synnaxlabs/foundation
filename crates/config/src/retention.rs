use document::{Block, read};
use spec::definition;
use spec::retention::Policy;

use crate::{Definition, Found};

const KEYS: [&str; 2] = ["select", "keep"];

/// Checks a `retention` block and gives its policy.
pub(crate) fn check(found: &mut Found<'_>, block: &Block) -> Option<Definition> {
    let unknown = found.unknown(block, &KEYS);
    let select = found.select(block, "indexes that it caps", "site_a.**");
    let fix = "Add a `keep` attribute with a span such as \"3d\"";
    let keep = found.required(block, "keep", read::duration, fix.into());
    let (Ok(()), Ok(select), Ok(keep)) = (unknown, select, keep) else {
        return None;
    };
    let policy = Policy::new(select, keep)
        .expect("invariant: `read::duration` reads a span of zero or more");
    Some(Definition::Spec(definition::Definition::Retention(policy)))
}
