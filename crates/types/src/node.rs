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

/// A node's Ed25519 public key. The transport authenticates peers with it. It is never
/// a point of small order: a signature for such a key passes with no private key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PublicKey([u8; 32]);

impl PublicKey {
    /// Wraps a key's 32 bytes.
    ///
    /// # Errors
    ///
    /// [`SmallOrder`] when the bytes encode a point of small order with either sign of
    /// x, or y = p or p + 1, which some decoders read as 0 and 1.
    pub fn new(bytes: [u8; 32]) -> Result<Self, SmallOrder> {
        let mut y = bytes;
        y[31] &= 0x7f;
        if SMALL_ORDER.contains(&y) {
            return Err(SmallOrder);
        }
        Ok(Self(bytes))
    }

    /// The key's 32 bytes.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Display for PublicKey {
    /// Writes the key as 64 lowercase hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// The refusal of a public key that is a point of small order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmallOrder;

impl fmt::Display for SmallOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the public key is a point of small order")
    }
}

impl std::error::Error for SmallOrder {}

/// The y of each Ed25519 point of small order, and p and p + 1, which a decoder that
/// does not refuse y >= p reads as 0 and 1.
const SMALL_ORDER: [[u8; 32]; 7] = [
    // 0 and p: order 4.
    [0; 32],
    [
        0xed, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
    ],
    // 1 and p + 1: the identity.
    [
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ],
    [
        0xee, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
    ],
    // p - 1: order 2.
    [
        0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
    ],
    // Order 8.
    [
        0x26, 0xe8, 0x95, 0x8f, 0xc2, 0xb2, 0x27, 0xb0, 0x45, 0xc3, 0xf4, 0x89, 0xf2,
        0xef, 0x98, 0xf0, 0xd5, 0xdf, 0xac, 0x05, 0xd3, 0xc6, 0x33, 0x39, 0xb1, 0x38,
        0x02, 0x88, 0x6d, 0x53, 0xfc, 0x05,
    ],
    [
        0xc7, 0x17, 0x6a, 0x70, 0x3d, 0x4d, 0xd8, 0x4f, 0xba, 0x3c, 0x0b, 0x76, 0x0d,
        0x10, 0x67, 0x0f, 0x2a, 0x20, 0x53, 0xfa, 0x2c, 0x39, 0xcc, 0xc6, 0x4e, 0xc7,
        0xfd, 0x77, 0x92, 0xac, 0x03, 0x7a,
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

    /// Each encoding of a point of small order: each y of the list, with either sign
    /// of x.
    const ENCODINGS: [&str; 14] = [
        "0100000000000000000000000000000000000000000000000000000000000000",
        "0100000000000000000000000000000000000000000000000000000000000080",
        "eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        "eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0000000000000000000000000000000000000000000000000000000000000080",
        "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
        "26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05",
        "26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc85",
        "c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a",
        "c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac03fa",
    ];

    fn bytes(hex: &str) -> [u8; 32] {
        let mut bytes = [0; 32];
        for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks(2)) {
            let pair = std::str::from_utf8(pair).unwrap();
            *byte = u8::from_str_radix(pair, 16).unwrap();
        }
        bytes
    }

    #[test]
    fn refuses_each_encoding_of_a_point_of_small_order() {
        for hex in ENCODINGS {
            assert_eq!(PublicKey::new(bytes(hex)), Err(SmallOrder), "{hex}");
        }
    }

    #[test]
    fn names_the_refusal() {
        assert_eq!(
            SmallOrder.to_string(),
            "the public key is a point of small order"
        );
    }

    #[test]
    fn keeps_a_key_one_bit_from_small_order() {
        let refused: Vec<[u8; 32]> = ENCODINGS.map(bytes).to_vec();
        for &encoding in &refused {
            for bit in 0..255 {
                let mut key = encoding;
                key[bit / 8] ^= 1 << (bit % 8);
                if !refused.contains(&key) {
                    assert_eq!(PublicKey::new(key).map(PublicKey::to_bytes), Ok(key));
                }
            }
        }
    }

    proptest! {
        #[test]
        fn keeps_any_other_key(key: [u8; 32]) {
            prop_assume!(ENCODINGS.iter().all(|&hex| bytes(hex) != key));
            prop_assert_eq!(PublicKey::new(key).map(PublicKey::to_bytes), Ok(key));
        }
    }

    #[test]
    fn displays_a_public_key_as_hex() {
        let mut bytes = [0xab; 32];
        bytes[0] = 0x01;
        let text = PublicKey::new(bytes).unwrap().to_string();
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
