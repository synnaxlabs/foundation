//! Byte-level values shared by every crate: time, byte sizes, sample types, keys, key
//! sets, names, selectors, quality, control authority, and content digests. Frames and
//! series join them here.
//!
//! These types describe layout only. What a value means (enum names, units) lives in
//! `spec`.

pub mod authority;
pub mod byte;
pub mod channel;
pub mod digest;
pub mod ed25519;
pub mod frame;
pub mod hash;
pub mod name;
pub mod node;
pub mod quality;
mod quantity;
pub mod sample;
pub mod time;
pub mod uuid;

#[cfg(test)]
mod common {
    /// Asserts that `message` is a lower-case clause and `fix` a sentence, neither with
    /// a final period, as a diagnostic shows them.
    pub(crate) fn assert_stated(message: &str, fix: &str) {
        assert!(
            message.starts_with(|c: char| c.is_ascii_lowercase()),
            "{message}"
        );
        assert!(fix.starts_with(|c: char| c.is_ascii_uppercase()), "{fix}");
        assert!(!message.ends_with('.'), "{message}");
        assert!(!fix.ends_with('.'), "{fix}");
    }
}
