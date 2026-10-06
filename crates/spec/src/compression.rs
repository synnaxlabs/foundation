//! The compression policy: how the encoder compresses the indexes it selects.

use types::name::Selector;

/// Sets the compression mode of the indexes that `select` matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    select: Selector,
    mode: Mode,
}

impl Policy {
    /// Makes a policy.
    #[must_use]
    pub const fn new(select: Selector, mode: Mode) -> Self {
        Self { select, mode }
    }

    /// The indexes the policy applies to.
    #[must_use]
    pub const fn select(&self) -> &Selector {
        &self.select
    }

    /// The compression mode.
    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.mode
    }
}

/// Which codecs the encoder may pick from. In each mode it still picks per vector,
/// and raw is always a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    /// The default codecs.
    Auto,
    /// No compression.
    Raw,
    /// The default codecs and the slower ones that compress more.
    Max,
}
