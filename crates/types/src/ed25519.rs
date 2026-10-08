//! Ed25519 keys.

use std::fmt;

use aws_lc_rs::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};

/// An Ed25519 public key, of a node or of a subject. The value that holds it gives its
/// role. It is never a point of small order: a signature for such a key passes with
/// no private key. Keys order by their bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

    /// Checks that `signature` is an Ed25519 signature of `message` by this key.
    ///
    /// # Errors
    ///
    /// [`BadSignature`] when it is not.
    pub fn verify(
        &self,
        message: &[u8],
        signature: &[u8; 64],
    ) -> Result<(), BadSignature> {
        UnparsedPublicKey::new(&ED25519, self.0)
            .verify(message, signature)
            .map_err(|_unspecified| BadSignature)
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

/// The refusal of a signature that is not of the message by the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadSignature;

impl fmt::Display for BadSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the signature is not of the message by the key")
    }
}

impl std::error::Error for BadSignature {}

/// An Ed25519 private key, of a node or of a subject. Its `Debug` never writes the
/// key, and it has no `Display` and no equality, so a log line or a timing difference
/// cannot show it.
#[derive(Clone)]
pub struct PrivateKey(pub [u8; 32]);

impl PrivateKey {
    /// The Ed25519 public key of this private key. Each call derives it again: a
    /// holder that signs keeps a [`Pair`].
    #[must_use]
    pub fn public(&self) -> PublicKey {
        Pair::new(self).public
    }
}

impl fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrivateKey(..)")
    }
}

/// An Ed25519 private key with its public key, derived once, for a holder that signs.
/// Its `Debug` writes only the public key.
pub struct Pair {
    pair: Ed25519KeyPair,
    public: PublicKey,
}

impl Pair {
    /// The pair of `private`. Derives the public key, a scalar multiplication.
    #[must_use]
    #[expect(
        clippy::missing_panics_doc,
        reason = "the public half of any 32 bytes is not of small order"
    )]
    pub fn new(private: &PrivateKey) -> Self {
        let pair = Ed25519KeyPair::from_seed_unchecked(&private.0)
            .expect("invariant: any 32 bytes are an Ed25519 private key");
        let bytes = pair.public_key().as_ref().try_into();
        let public = PublicKey::new(
            bytes.expect("invariant: an Ed25519 public key is 32 bytes"),
        )
        .expect("invariant: the public half of a private key is not of small order");
        Self { pair, public }
    }

    /// The public key of the pair.
    #[must_use]
    pub const fn public(&self) -> PublicKey {
        self.public
    }

    /// The Ed25519 signature of `message` by the private key.
    #[must_use]
    #[expect(
        clippy::missing_panics_doc,
        reason = "an Ed25519 signature is always 64 bytes"
    )]
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.pair
            .sign(message)
            .as_ref()
            .try_into()
            .expect("invariant: an Ed25519 signature is 64 bytes")
    }
}

impl fmt::Debug for Pair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Pair").field(&self.public).finish()
    }
}

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

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::common::bytes;

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

        #[test]
        fn orders_keys_by_their_bytes(a: [u8; 32], b: [u8; 32]) {
            let (Ok(x), Ok(y)) = (PublicKey::new(a), PublicKey::new(b)) else {
                return Ok(());
            };
            prop_assert_eq!(x.cmp(&y), a.cmp(&b));
        }
    }

    /// The public key and signature of RFC 8032, section 7.1, test 1, whose message
    /// is empty.
    fn rfc_test_1() -> (PublicKey, [u8; 64]) {
        let key =
            bytes("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
        let signature = signature(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155\
             5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        );
        (PublicKey::new(key).unwrap(), signature)
    }

    /// The 64 bytes that the 128 hex digits `hex` give.
    fn signature(hex: &str) -> [u8; 64] {
        let mut signature = [0; 64];
        signature[..32].copy_from_slice(&bytes(&hex[..64]));
        signature[32..].copy_from_slice(&bytes(&hex[64..]));
        signature
    }

    #[test]
    fn verifies_the_signature_of_the_rfc_vector() {
        let (key, signature) = rfc_test_1();
        assert_eq!(key.verify(b"", &signature), Ok(()));
    }

    /// RFC 8032, section 7.1, test 2, with each bit of its one-byte message changed.
    #[test]
    fn refuses_a_message_with_one_bit_changed() {
        let key =
            bytes("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c");
        let key = PublicKey::new(key).unwrap();
        let signature = signature(
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da\
             085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
        );
        assert_eq!(key.verify(&[0x72], &signature), Ok(()));
        for bit in 0..8 {
            let message = [0x72 ^ (1 << bit)];
            assert_eq!(key.verify(&message, &signature), Err(BadSignature), "{bit}");
        }
    }

    #[test]
    fn refuses_a_signature_with_one_bit_changed() {
        let (key, signature) = rfc_test_1();
        for bit in 0..512 {
            let mut changed = signature;
            changed[bit / 8] ^= 1 << (bit % 8);
            assert_eq!(key.verify(b"", &changed), Err(BadSignature), "{bit}");
        }
    }

    #[test]
    fn names_the_refusal_of_a_signature() {
        assert_eq!(
            BadSignature.to_string(),
            "the signature is not of the message by the key"
        );
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

    /// RFC 8032, section 7.1, test 1.
    #[test]
    fn derives_the_public_key_of_the_rfc_vector() {
        let private = PrivateKey(bytes(
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        ));
        let public =
            bytes("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
        assert_eq!(private.public().to_bytes(), public);
    }

    proptest! {
        #[test]
        fn derives_a_key_for_any_private_key(private: [u8; 32]) {
            let key = PrivateKey(private).public();
            prop_assert_eq!(PublicKey::new(key.to_bytes()), Ok(key));
        }
    }

    /// RFC 8032, section 7.1, test 1.
    #[test]
    fn signs_the_rfc_vector() {
        let pair = Pair::new(&PrivateKey(bytes(
            "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        )));
        let (key, signature) = rfc_test_1();
        assert_eq!(pair.public(), key);
        assert_eq!(pair.sign(b""), signature);
    }

    #[test]
    fn writes_only_the_public_key_of_a_pair_in_debug() {
        let private = PrivateKey([0xcd; 32]);
        let text = format!("{:?}", Pair::new(&private));
        assert_eq!(text, format!("Pair({:?})", private.public()));
    }

    proptest! {
        #[test]
        fn signs_what_the_public_key_verifies(
            private: [u8; 32],
            message: Vec<u8>,
        ) {
            let pair = Pair::new(&PrivateKey(private));
            prop_assert_eq!(pair.public(), PrivateKey(private).public());
            let signature = pair.sign(&message);
            prop_assert_eq!(pair.public().verify(&message, &signature), Ok(()));
        }
    }
}
