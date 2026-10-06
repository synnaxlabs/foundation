use document::diagnostic::{Code, Diagnostic};
use document::{Attribute, Block, Span, read};
use types::byte;
use types::name::{Name, Selector};

use crate::Check;

const MISSING_ATTRIBUTE: Code = Code::new("config.missing-attribute");
const ZERO_SIZE: Code = Code::new("config.zero-size");

/// A policy that sets the disk and pool budgets of the nodes it selects.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct NodeSettings {
    /// The label: a name, unique among `node_settings` policies.
    pub name: Name,
    /// The nodes it selects.
    pub select: Selector,
    /// The disk budget. `None` leaves the budget to a less specific policy.
    pub disk: Option<byte::Size>,
    /// The pool budget. `None` leaves the budget to a less specific policy.
    pub pool: Option<byte::Size>,
    /// Where the block is, for `explain`.
    pub span: Option<Span>,
}

/// Checks a `node_settings` block and adds its policy to the definitions.
pub(crate) fn check<'a>(check: &mut Check<'a>, block: &'a Block) {
    let keyword = &*block.keyword;
    let name = check.name(block);
    let (mut select, mut disk, mut pool) = (None, None, None);
    for attribute in block.body.attributes.iter() {
        match &*attribute.key {
            "select" => select = check.report(read::selector(&attribute.value)),
            "disk" => disk = budget(check, attribute),
            "pool" => pool = budget(check, attribute),
            _ => check.unknown_attribute(
                keyword,
                attribute,
                "`select`, `disk`, or `pool`",
            ),
        }
    }
    check.unknown_blocks(keyword, &block.body);
    if block.body.attributes.get("select").is_none() {
        check.diagnostics.push(Diagnostic::new(
            MISSING_ATTRIBUTE,
            block.keyword_span,
            format!("the `{keyword}` block has no `select`"),
            "Add the nodes that it sets, such as `select = \"site_a.*\"`".into(),
        ));
    }
    if let (Some(name), Some(select)) = (name, select) {
        check.definitions.node_settings.push(NodeSettings {
            name,
            select,
            disk,
            pool,
            span: block.span,
        });
    }
}

/// Reads a budget, a size above zero.
fn budget(check: &mut Check<'_>, attribute: &Attribute) -> Option<byte::Size> {
    let size = check.report(read::size(&attribute.value))?;
    if size.bytes() == 0 {
        check.diagnostics.push(Diagnostic::new(
            ZERO_SIZE,
            attribute.value.span,
            format!("the `{}` budget is zero", attribute.key),
            format!("Write a size above zero, or remove `{}`", attribute.key),
        ));
        return None;
    }
    Some(size)
}
