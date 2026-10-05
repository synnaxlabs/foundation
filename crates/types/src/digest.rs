//! Content digests.

use std::fmt;

/// The BLAKE3 hash of some bytes: the address of a spec chunk or a blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    /// Hashes `bytes`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }
}

impl fmt::Display for Digest {
    /// Writes the digest as 64 lowercase hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::Digest;

    #[test]
    fn hashes_with_blake3() {
        assert_eq!(
            Digest::of(b"").to_string(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn differs_for_different_bytes() {
        assert_ne!(Digest::of(b"a"), Digest::of(b"b"));
    }
}
