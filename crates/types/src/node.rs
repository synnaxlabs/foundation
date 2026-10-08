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

/// A node's X25519 seal key. Callers seal secret values to it. It is canonical and
/// never of small order, so one key has one encoding and a sealed value always
/// depends on the node's private key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SealKey([u8; 32]);

impl SealKey {
    /// Wraps a key's 32 bytes.
    ///
    /// # Errors
    ///
    /// [`BadSealKey`] when the top bit is set, when the u the bytes encode is p or
    /// more, or when that u is of small order. X25519 reads the first two as another
    /// key's u, so they would give one key two encodings.
    pub fn new(bytes: [u8; 32]) -> Result<Self, BadSealKey> {
        let at_least_p = bytes[31] == 0x7f
            && bytes[1..31].iter().all(|&byte| byte == 0xff)
            && bytes[0] >= 0xed;
        if bytes[31] & 0x80 != 0 || at_least_p || X25519_SMALL_ORDER.contains(&bytes) {
            return Err(BadSealKey);
        }
        Ok(Self(bytes))
    }

    /// The key's 32 bytes.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// The refusal of a seal key that is not canonical or is of small order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadSealKey;

impl fmt::Display for BadSealKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the seal key is not a canonical X25519 key, or is of small order")
    }
}

impl std::error::Error for BadSealKey {}

/// Each X25519 u of small order below p.
const X25519_SMALL_ORDER: [[u8; 32]; 5] = [
    [0; 32],
    [
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ],
    [
        0xe0, 0xeb, 0x7a, 0x7c, 0x3b, 0x41, 0xb8, 0xae, 0x16, 0x56, 0xe3, 0xfa, 0xf1,
        0x9f, 0xc4, 0x6a, 0xda, 0x09, 0x8d, 0xeb, 0x9c, 0x32, 0xb1, 0xfd, 0x86, 0x62,
        0x05, 0x16, 0x5f, 0x49, 0xb8, 0x00,
    ],
    [
        0x5f, 0x9c, 0x95, 0xbc, 0xa3, 0x50, 0x8c, 0x24, 0xb1, 0xd0, 0xb1, 0x55, 0x9c,
        0x83, 0xef, 0x5b, 0x04, 0x44, 0x5c, 0xc4, 0x58, 0x1c, 0x8e, 0x86, 0xd8, 0x22,
        0x4e, 0xdd, 0xd0, 0x9f, 0x11, 0x57,
    ],
    [
        0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
    ],
];

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
    use proptest::prelude::*;

    use super::*;

    fn bytes(hex: &str) -> [u8; 32] {
        let mut bytes = [0; 32];
        for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).unwrap();
            *byte = u8::from_str_radix(pair, 16).unwrap();
        }
        bytes
    }

    /// p - 1, little-endian.
    fn p_minus_one() -> [u8; 32] {
        let mut u = [0xff; 32];
        u[0] = 0xec;
        u[31] = 0x7f;
        u
    }

    #[test]
    fn refuses_a_seal_key_of_small_order() {
        let hexes = [
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0100000000000000000000000000000000000000000000000000000000000000",
            "e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800",
            "5f9c95bca3508c24b1d0b1559c83ef5b04445cc4581c8e86d8224eddd09f1157",
            "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        ];
        for hex in hexes {
            assert_eq!(SealKey::new(bytes(hex)), Err(BadSealKey), "{hex}");
        }
    }

    #[test]
    fn refuses_a_seal_key_with_the_top_bit_set() {
        let mut key = [9; 32];
        key[31] |= 0x80;
        assert_eq!(SealKey::new(key), Err(BadSealKey));
    }

    #[test]
    fn refuses_a_seal_key_of_p_or_more() {
        for add in 1..=19 {
            let mut u = p_minus_one();
            u[0] += add;
            assert_eq!(SealKey::new(u), Err(BadSealKey), "p + {}", add - 1);
        }
    }

    #[test]
    fn keeps_a_seal_key_just_below_p() {
        let mut low = p_minus_one();
        low[0] = 0xeb;
        assert_eq!(SealKey::new(low).map(SealKey::to_bytes), Ok(low));
        let mut high = p_minus_one();
        high[0] = 0xff;
        high[30] = 0xfe;
        assert_eq!(SealKey::new(high).map(SealKey::to_bytes), Ok(high));
    }

    #[test]
    fn names_the_seal_key_refusal() {
        assert_eq!(
            BadSealKey.to_string(),
            "the seal key is not a canonical X25519 key, or is of small order"
        );
    }

    proptest! {
        #[test]
        fn keeps_any_other_seal_key(mut key: [u8; 32]) {
            key[31] &= 0x7f;
            prop_assume!(key[31] != 0x7f && !X25519_SMALL_ORDER.contains(&key));
            prop_assert_eq!(SealKey::new(key).map(SealKey::to_bytes), Ok(key));
        }
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
