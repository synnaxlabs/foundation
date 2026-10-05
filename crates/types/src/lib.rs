//! Byte-level values shared by every crate: time, sample types, keys, names,
//! selectors, and quality. Frames, series, and key sets join them here.
//!
//! These types describe layout only. What a value means (enum names, units) lives in
//! `spec`.

pub mod channel;
pub mod name;
pub mod node;
pub mod quality;
pub mod sample;
pub mod time;

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
