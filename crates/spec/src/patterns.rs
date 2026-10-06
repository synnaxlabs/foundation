//! The patterns of a selector, as a definition stores them.

use std::fmt;

use types::name::{self, Selector};

/// The patterns of a selector as the file wrote them, in order, and the selector they
/// read as. Two lists of patterns that select the same names are two values with two
/// hashes, so a diff shows a rewrite of the patterns as a change.
#[derive(Clone, PartialEq, Eq)]
pub struct Patterns {
    texts: Box<[Box<str>]>,
    // A function of `texts`, so the derived equality compares `texts`.
    selector: Selector,
}

impl Patterns {
    /// Reads the patterns. A pattern with a leading `!` excludes names.
    ///
    /// # Errors
    ///
    /// The error of [`Selector::new`] when the patterns do not read as a selector.
    pub fn new<'a>(
        texts: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, name::Error> {
        let texts = texts
            .into_iter()
            .map(Box::from)
            .collect::<Box<[Box<str>]>>();
        let selector = Selector::new(texts.iter().map(|t| &**t))?;
        Ok(Self { texts, selector })
    }

    /// The selector the patterns read as.
    #[must_use]
    pub const fn selector(&self) -> &Selector {
        &self.selector
    }

    /// The patterns as written, in order.
    #[must_use]
    pub fn texts(&self) -> impl ExactSizeIterator<Item = &str> {
        self.texts.iter().map(|t| &**t)
    }
}

impl fmt::Debug for Patterns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.texts()).finish()
    }
}
