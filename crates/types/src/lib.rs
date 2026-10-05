//! Byte-level values shared by every crate: time, sample types, keys, key sets, names,
//! selectors, quality, control authority, and content digests. Frames and series join
//! them here.
//!
//! These types describe layout only. What a value means (enum names, units) lives in
//! `spec`.

pub mod authority;
pub mod channel;
pub mod digest;
pub mod frame;
pub mod hash;
pub mod name;
pub mod node;
pub mod quality;
pub mod sample;
pub mod time;
mod uuid;

use std::fmt;

/// A value that could not be read from text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// The text that was read.
    pub input: String,
    /// What the text should look like.
    pub expected: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot read {:?}: expected {}",
            self.input, self.expected
        )
    }
}

impl std::error::Error for ParseError {}
