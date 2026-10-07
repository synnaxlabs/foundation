use document::diagnostic::{Code, Diagnostic};
use document::{Block, read};
use spec::definition::Definition;
use spec::retention::Policy;

use crate::Found;

const NEGATIVE_SPAN: Code = Code::new("config.negative-span");
const KEYS: [&str; 2] = ["select", "keep"];

/// Checks a `retention` block and gives its policy.
pub(crate) fn check(found: &mut Found<'_>, block: &Block) -> Option<Definition> {
    let unknown = found.unknown_attributes(block, &KEYS);
    found.unknown_blocks(block);
    let select = found.select(block, "indexes that it caps");
    let keep = found.attribute(block, "keep", read::span);
    if let Ok(None) = keep {
        let fix = "Add a `keep` attribute with a span such as \"3d\"";
        found.missing(block, &["keep"], fix.into());
    }
    let (Ok(()), Ok(select), Ok(Some(keep))) = (unknown, select, keep) else {
        return None;
    };
    match Policy::new(select, keep) {
        Ok(policy) => Some(Definition::Retention(policy)),
        Err(error) => {
            let at = block
                .body
                .attributes
                .get("keep")
                .and_then(|keep| keep.value.span);
            let fix = error.fix().into();
            found.diagnostics.push(Diagnostic::new(
                NEGATIVE_SPAN,
                at,
                error.to_string(),
                fix,
            ));
            None
        }
    }
}
