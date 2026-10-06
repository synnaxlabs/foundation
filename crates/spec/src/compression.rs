//! The compression policy: how the encoder compresses the indexes it selects.

use types::name::Selector;

/// Sets the compression mode of the indexes that `select` matches. Every selector and
/// mode make a valid policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The indexes the policy applies to.
    pub select: Selector,
    /// The compression mode.
    pub mode: Mode,
}

/// Which codecs the encoder may use for the selected indexes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Mode {
    /// The default codecs.
    #[default]
    Auto,
    /// No compression.
    Raw,
    /// The default codecs and the slower ones that compress more.
    Max,
}
