use document::diagnostic::{Code, Diagnostic};
use document::{Block, read};
use spec::definition;
use spec::retention::{Error, Policy};

use crate::{Definition, Found};

const NEGATIVE_SPAN: Code = Code::new("config.negative-span");
const KEYS: [&str; 2] = ["select", "keep"];

/// Checks a `retention` block and gives its policy.
pub(crate) fn check(found: &mut Found<'_>, block: &Block) -> Option<Definition> {
    let unknown = found.unknown(block, &KEYS);
    let select = found.select(block, "indexes that it caps", "site_a.**");
    let fix = "Add a `keep` attribute with a span such as \"3d\"";
    let keep = found.required(block, "keep", read::span, fix.into());
    let (Ok(()), Ok(select), Ok(keep)) = (unknown, select, keep) else {
        return None;
    };
    match Policy::new(select, keep) {
        Ok(policy) => Some(Definition::Spec(definition::Definition::Retention(policy))),
        Err(error @ Error::Negative(_)) => {
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
