use document::diagnostic::{Code, Diagnostic};
use document::{Block, read};
use spec::node_settings::{Error, Policy};

use crate::{Check, Entry};

const MISSING_ATTRIBUTE: Code = Code::new("config.missing-attribute");
const ZERO_SIZE: Code = Code::new("config.zero-size");

/// Checks a `node_settings` block and adds its policy to the entries.
pub(crate) fn check<'a>(check: &mut Check<'a>, block: &'a Block) {
    let keyword = &*block.keyword;
    let key = check.key(block);
    let before = check.diagnostics.len();
    let (mut select, mut disk, mut pool) = (None, None, None);
    for attribute in block.body.attributes.iter() {
        match &*attribute.key {
            "select" => select = check.report(read::selector(&attribute.value)),
            "disk" => disk = check.report(read::size(&attribute.value)),
            "pool" => pool = check.report(read::size(&attribute.value)),
            _ => check.unknown_attribute(
                keyword,
                attribute,
                "`select`, `disk`, or `pool`",
            ),
        }
    }
    let refused = check.diagnostics.len() > before;
    check.unknown_blocks(keyword, &block.body);
    if block.body.attributes.get("select").is_none() {
        check.diagnostics.push(Diagnostic::new(
            MISSING_ATTRIBUTE,
            block.keyword_span,
            format!("the `{keyword}` block has no `select`"),
            "Add the nodes that it sets, such as `select = \"site_a.*\"`".into(),
        ));
    }
    if refused {
        return;
    }
    let Some(select) = select else {
        return;
    };
    match Policy::new(select, disk, pool) {
        Ok(policy) => {
            if let Some((key, span)) = key {
                let definition = spec::definition::Definition::NodeSettings(policy);
                check.entries.insert(key, Entry { definition, span });
            }
        }
        Err(error) => check.diagnostics.push(refusal(block, error)),
    }
}

/// The diagnostic for a policy that `Policy::new` refuses.
fn refusal(block: &Block, error: Error) -> Diagnostic {
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
        Error::ZeroDisk => zero("disk"),
        Error::ZeroPool => zero("pool"),
        Error::NoBudget => Diagnostic::new(
            MISSING_ATTRIBUTE,
            block.keyword_span,
            format!("the `{}` block has no `disk` or `pool`", block.keyword),
            "Add `disk`, `pool`, or both, such as `disk = \"10GiB\"`".into(),
        ),
    }
}
