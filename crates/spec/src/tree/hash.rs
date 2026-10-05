use std::fmt;

/// The BLAKE3 hash of a chunk's bytes. It is the chunk's address.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub(super) fn of(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }
}

impl AsRef<[u8]> for Hash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_a_hash_as_hex() {
        let hash = Hash([0xAB; 32]);
        assert_eq!(hash.to_string(), "ab".repeat(32));
        assert_eq!(format!("{hash:?}"), format!("Hash({})", "ab".repeat(32)));
    }
}
