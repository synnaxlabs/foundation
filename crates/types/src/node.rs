//! Node identity.

use std::fmt;

/// A node's stable identity, a UUIDv7. It stays the same when the node rotates its
/// public key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(u128);

impl Key {
    /// Wraps a key's 128 bits.
    #[must_use]
    pub const fn from_u128(bits: u128) -> Self {
        Self(bits)
    }

    /// The key's 128 bits.
    #[must_use]
    pub const fn as_u128(self) -> u128 {
        self.0
    }
}

impl fmt::Display for Key {
    /// Writes the key as a lowercase hyphenated UUID string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        crate::uuid::write(self.0, f)
    }
}

/// A node's Ed25519 public key. The transport authenticates peers with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PublicKey(pub [u8; 32]);

impl fmt::Display for PublicKey {
    /// Writes the key as 64 lowercase hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// A node's Ed25519 private key. Its `Debug` never writes the key, and it has no
/// `Display` and no equality, so a log line or a timing difference cannot show it.
#[derive(Clone)]
pub struct PrivateKey(pub [u8; 32]);

impl fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrivateKey(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_a_public_key_as_hex() {
        let mut bytes = [0xab; 32];
        bytes[0] = 0x01;
        let text = PublicKey(bytes).to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(&text[..6], "01abab");
    }

    #[test]
    fn hides_a_private_key_in_debug() {
        let text = format!("{:?}", PrivateKey([0xcd; 32]));
        assert_eq!(text, "PrivateKey(..)");
    }

    #[test]
    fn displays_as_a_hyphenated_uuid() {
        let key = Key::from_u128(0x017f_22e2_79b0_7cc3_98c4_dc0c_0c07_398f);
        assert_eq!(key.to_string(), "017f22e2-79b0-7cc3-98c4-dc0c0c07398f");
    }
}
