use crate::{Document, Span};

/// A keyword, labels, and a body, such as `connector "opcua" "site_a.plc_7" { ... }`.
/// `==` does not read spans.
#[derive(Clone, Debug)]
pub struct Block {
    /// The word that opens the block, such as `connector`.
    pub keyword: Box<str>,
    /// Where the keyword is.
    pub keyword_span: Option<Span>,
    /// The labels after the keyword, in order.
    pub labels: Vec<Label>,
    /// The attributes and blocks inside the block.
    pub body: Document,
    /// Where the whole block is, from the keyword to the end of the body.
    pub span: Option<Span>,
}

impl PartialEq for Block {
    fn eq(&self, other: &Self) -> bool {
        self.keyword == other.keyword
            && self.labels == other.labels
            && self.body == other.body
    }
}

impl Eq for Block {}

/// One label of a block, such as `"opcua"`. `==` does not read spans.
#[derive(Clone, Debug)]
pub struct Label {
    /// The text of the label.
    pub text: Box<str>,
    /// Where the label is.
    pub span: Option<Span>,
}

impl PartialEq for Label {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

impl Eq for Label {}
