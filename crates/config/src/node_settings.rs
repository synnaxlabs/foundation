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

impl<'a> Check<'a> {
    /// Checks a `node_settings` block and adds its policy to the definitions.
    pub(crate) fn node_settings(&mut self, block: &'a Block) {
        let keyword = &*block.keyword;
        let name = self.name(block);
        let (mut select, mut disk, mut pool) = (None, None, None);
        for attribute in block.body.attributes.iter() {
            match &*attribute.key {
                "select" => select = self.report(read::selector(&attribute.value)),
                "disk" => disk = self.budget(attribute),
                "pool" => pool = self.budget(attribute),
                _ => self.unknown_attribute(
                    keyword,
                    attribute,
                    "`select`, `disk`, or `pool`",
                ),
            }
        }
        self.unknown_blocks(keyword, &block.body);
        if block.body.attributes.get("select").is_none() {
            self.diagnostics.push(Diagnostic::new(
                MISSING_ATTRIBUTE,
                block.keyword_span,
                format!("the `{keyword}` block has no `select`"),
                "Add the nodes that it sets, such as `select = \"site_a.*\"`".into(),
            ));
        }
        if let (Some(name), Some(select)) = (name, select) {
            self.definitions.node_settings.push(NodeSettings {
                name,
                select,
                disk,
                pool,
                span: block.span,
            });
        }
    }

    /// Reads a budget, a size above zero.
    fn budget(&mut self, attribute: &Attribute) -> Option<byte::Size> {
        let size = self.report(read::size(&attribute.value))?;
        if size.bytes() == 0 {
            self.diagnostics.push(Diagnostic::new(
                ZERO_SIZE,
                attribute.value.span,
                format!("the `{}` budget is zero", attribute.key),
                format!("Write a size above zero, or remove `{}`", attribute.key),
            ));
            return None;
        }
        Some(size)
    }
}
