//! Program connections.

use std::fmt;

/// The key of one program connection at every owner: 16 random bytes that the
/// program picks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key(pub [u8; 16]);

impl fmt::Display for Key {
    /// Writes the 16 bytes in order as a lowercase hyphenated UUID string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        crate::uuid::write(u128::from_be_bytes(self.0), f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_bytes_in_order_as_a_uuid() {
        let key = Key(core::array::from_fn(|i| {
            u8::try_from(i * 17).expect("fits")
        }));
        assert_eq!(key.to_string(), "00112233-4455-6677-8899-aabbccddeeff");
        assert_eq!(
            Key([0xab; 16]).to_string(),
            "abababab-abab-abab-abab-abababababab"
        );
    }
}
