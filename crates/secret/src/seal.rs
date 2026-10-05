//! Seals a secret value to a node's seal key, and opens it on that node.
//!
//! The scheme is HPKE base mode with DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, and
//! ChaCha20-Poly1305. A sealed value is the 32-byte encapsulated key, then the
//! ciphertext and its 16-byte tag. The associated data is the secret's version and full
//! name, so a sealed value copied to another name or version does not open.

use std::fmt;

use aws_lc_rs::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use aws_lc_rs::agreement::{self, PrivateKey, UnparsedPublicKey, X25519};
use aws_lc_rs::hmac;
use env::entropy::Entropy;
use types::name::Name;
use types::node::SealKey;
use zeroize::Zeroizing;

use crate::Value;

const INFO: &[u8] = b"foundation/secret/1";
const ENC_LEN: usize = 32;
const TAG_LEN: usize = 16;
const KEM_SUITE: &[u8] = b"KEM\x00\x20";
const HPKE_SUITE: &[u8] = b"HPKE\x00\x20\x00\x01\x00\x03";

/// Seals `value` to the node that holds the private half of `to`, bound to the
/// secret's full `name` and its `version`. Returns 48 bytes more than the value. Draws
/// 32 bytes from `entropy`.
///
/// # Panics
///
/// Never: any 32 bytes are an X25519 private key, and a [`SealKey`] is never of
/// small order.
#[must_use]
pub fn seal(
    to: &SealKey,
    name: &Name,
    version: u64,
    value: &Value,
    entropy: &Entropy,
) -> Vec<u8> {
    let mut bytes = Zeroizing::new([0; 32]);
    entropy.fill(bytes.as_mut_slice());
    let ephemeral = PrivateKey::from_private_key(&X25519, bytes.as_slice())
        .expect("invariant: any 32 bytes are an X25519 private key");
    seal_with(
        &ephemeral,
        &to.to_bytes(),
        INFO,
        &aad(name, version),
        value.expose(),
    )
    .expect("invariant: a seal key is never of small order")
}

/// A node's X25519 seal private key. It opens values sealed to its public key.
/// `Debug` never shows the key, and the key is overwritten with zeros when it drops.
pub struct Opener(Box<Zeroizing<[u8; 32]>>);

impl Opener {
    /// Makes a new key from 32 bytes of `entropy`.
    #[must_use]
    pub fn generate(entropy: &Entropy) -> Self {
        let mut bytes = Box::new(Zeroizing::new([0; 32]));
        entropy.fill(bytes.as_mut_slice());
        Self(bytes)
    }

    /// Takes the bytes of a key from [`Opener::expose`].
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Box::new(Zeroizing::new(bytes)))
    }

    /// The bytes to keep on the node's disk. Write them nowhere else.
    #[must_use]
    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }

    /// The public key to publish in the node record.
    ///
    /// # Panics
    ///
    /// Never for a key from this crate: X25519 maps no private key to a point of
    /// small order.
    #[must_use]
    pub fn public(&self) -> SealKey {
        let public = self
            .key()
            .compute_public_key()
            .expect("invariant: an X25519 private key has a public key");
        let bytes = public
            .as_ref()
            .try_into()
            .expect("invariant: an X25519 public key is 32 bytes");
        SealKey::new(bytes).expect("invariant: X25519 maps no key to small order")
    }

    /// Opens `sealed` for the secret `name` at `version`. Pass the version stored
    /// with `sealed` in region state, never one from another source.
    ///
    /// # Errors
    ///
    /// [`Error::Refused`] when `sealed` was sealed to another key, name, or version,
    /// or was changed or cut.
    pub fn open(
        &self,
        name: &Name,
        version: u64,
        sealed: &[u8],
    ) -> Result<Value, Error> {
        let public = self.public().to_bytes();
        open_with(&self.key(), &public, INFO, &aad(name, version), sealed)
            .map(Value::new)
    }

    fn key(&self) -> PrivateKey {
        PrivateKey::from_private_key(&X25519, self.0.as_slice())
            .expect("invariant: any 32 bytes are an X25519 private key")
    }
}

impl fmt::Debug for Opener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Opener(..)")
    }
}

/// Why a sealed value did not open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The value was sealed to another key, name, or version, or was changed.
    Refused,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused => f.write_str(
                "the sealed value does not open with this node's seal key for this \
                 name and version. Set the secret again so it is sealed to this node",
            ),
        }
    }
}

impl std::error::Error for Error {}

/// The version, 8 bytes big-endian, then the name.
fn aad(name: &Name, version: u64) -> Vec<u8> {
    [&version.to_be_bytes()[..], name.as_str().as_bytes()].concat()
}

/// Seals with a given ephemeral key. `None` when `to` is of small order.
fn seal_with(
    ephemeral: &PrivateKey,
    to: &[u8; 32],
    info: &[u8],
    aad: &[u8],
    plain: &[u8],
) -> Option<Vec<u8>> {
    let enc = ephemeral.compute_public_key().ok()?;
    let (key, nonce) = schedule(ephemeral, to, enc.as_ref(), to, info)?;
    // Sized for the tag, so the plain bytes are encrypted where they are and no
    // reallocation frees a copy of them.
    let mut sealed = Vec::with_capacity(ENC_LEN + plain.len() + TAG_LEN);
    sealed.extend_from_slice(enc.as_ref());
    sealed.extend_from_slice(plain);
    let tag = key
        .seal_in_place_separate_tag(nonce, Aad::from(aad), &mut sealed[ENC_LEN..])
        .expect("invariant: a secret is far below the AEAD's size limit");
    sealed.extend_from_slice(tag.as_ref());
    Some(sealed)
}

fn open_with(
    own: &PrivateKey,
    own_public: &[u8],
    info: &[u8],
    aad: &[u8],
    sealed: &[u8],
) -> Result<Vec<u8>, Error> {
    let (enc, body) = sealed.split_at_checked(ENC_LEN).ok_or(Error::Refused)?;
    let (key, nonce) =
        schedule(own, enc, enc, own_public, info).ok_or(Error::Refused)?;
    let mut body = Zeroizing::new(body.to_vec());
    let plain_len = key
        .open_in_place(nonce, Aad::from(aad), &mut body)
        .map_err(|_unspecified| Error::Refused)?
        .len();
    body.truncate(plain_len);
    Ok(std::mem::take(&mut *body))
}

/// Runs the KEM and the base-mode key schedule. `peer` is the public key to agree
/// with; `enc` and `recipient` form the KEM context. `None` when the agreement fails,
/// as it does for a peer of small order.
fn schedule(
    own: &PrivateKey,
    peer: &[u8],
    enc: &[u8],
    recipient: &[u8],
    info: &[u8],
) -> Option<(LessSafeKey, Nonce)> {
    let dh = agreement::agree(own, UnparsedPublicKey::new(&X25519, peer), (), |dh| {
        Ok(Zeroizing::new(dh.to_vec()))
    })
    .ok()?;
    let eae_prk = extract(KEM_SUITE, &[], b"eae_prk", &dh);
    let context = [enc, recipient].concat();
    let shared = expand(KEM_SUITE, &eae_prk, b"shared_secret", &context, 32);

    let psk_id_hash = extract(HPKE_SUITE, &[], b"psk_id_hash", &[]);
    let info_hash = extract(HPKE_SUITE, &[], b"info_hash", info);
    let context = [&[0][..], &psk_id_hash, &info_hash].concat();
    let secret = extract(HPKE_SUITE, &shared, b"secret", &[]);
    let key = expand(HPKE_SUITE, &secret, b"key", &context, 32);
    let nonce = expand(HPKE_SUITE, &secret, b"base_nonce", &context, 12);

    let key = UnboundKey::new(&CHACHA20_POLY1305, &key)
        .expect("invariant: the key is 32 bytes");
    let nonce = Nonce::try_assume_unique_for_key(&nonce)
        .expect("invariant: the nonce is 12 bytes");
    Some((LessSafeKey::new(key), nonce))
}

/// HKDF-Extract over the labeled input.
fn extract(suite: &[u8], salt: &[u8], label: &[u8], ikm: &[u8]) -> Zeroizing<Vec<u8>> {
    let key = hmac::Key::new(hmac::HMAC_SHA256, salt);
    let mut ctx = hmac::Context::with_key(&key);
    for part in [b"HPKE-v1", suite, label, ikm] {
        ctx.update(part);
    }
    Zeroizing::new(ctx.sign().as_ref().to_vec())
}

/// HKDF-Expand over the labeled info, for at most one hash length.
fn expand(
    suite: &[u8],
    prk: &[u8],
    label: &[u8],
    info: &[u8],
    len: u16,
) -> Zeroizing<Vec<u8>> {
    assert!(len <= 32, "one HMAC-SHA256 block holds {len} bytes");
    let key = hmac::Key::new(hmac::HMAC_SHA256, prk);
    let mut ctx = hmac::Context::with_key(&key);
    for part in [&len.to_be_bytes()[..], b"HPKE-v1", suite, label, info, &[1]] {
        ctx.update(part);
    }
    Zeroizing::new(ctx.sign().as_ref()[..usize::from(len)].to_vec())
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    fn key(text: &str) -> PrivateKey {
        PrivateKey::from_private_key(&X25519, &hex(text)).unwrap()
    }

    fn entropy(value: u64) -> Entropy {
        let mut sim = sim::Sim::new(sim::Config {
            seed: value,
            ..sim::Config::default()
        });
        sim.node(sim::node::Config::default()).entropy()
    }

    fn name(text: &str) -> Name {
        text.parse().unwrap()
    }

    /// Base mode, DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305.
    mod vector {
        pub(super) const INFO: &str = "4f6465206f6e2061204772656369616e2055726e";
        pub(super) const SK_E: &str =
            "f4ec9b33b792c372c1d2c2063507b684ef925b8c75a42dbcbf57d63ccd381600";
        pub(super) const SK_R: &str =
            "8057991eef8f1f1af18f4a9491d16a1ce333f695d4db8e38da75975c4478e0fb";
        pub(super) const PK_R: &str =
            "4310ee97d88cc1f088a5576c77ab0cf5c3ac797f3d95139c6c84b5429c59662a";
        pub(super) const ENC: &str =
            "1afa08d3dec047a643885163f1180476fa7ddb54c6a8029ea33f95796bf2ac4a";
        pub(super) const AAD: &str = "436f756e742d30";
        pub(super) const PT: &str =
            "4265617574792069732074727574682c20747275746820626561757479";
        pub(super) const CT: &str = "\
            1c5250d8034ec2b784ba2cfd69dbdb8af406cfe3ff938e131f0def8c8b60b4db\
            21993c62ce81883d2dd1b51a28";
    }

    #[test]
    fn seals_as_the_published_vector() {
        let to: [u8; 32] = hex(vector::PK_R).try_into().unwrap();
        let sealed = seal_with(
            &key(vector::SK_E),
            &to,
            &hex(vector::INFO),
            &hex(vector::AAD),
            &hex(vector::PT),
        )
        .unwrap();
        assert_eq!(sealed, [hex(vector::ENC), hex(vector::CT)].concat());
    }

    #[test]
    fn opens_the_published_vector() {
        let sealed = [hex(vector::ENC), hex(vector::CT)].concat();
        let plain = open_with(
            &key(vector::SK_R),
            &hex(vector::PK_R),
            &hex(vector::INFO),
            &hex(vector::AAD),
            &sealed,
        );
        assert_eq!(plain, Ok(hex(vector::PT)));
    }

    #[test]
    fn publishes_the_public_half_of_its_key() {
        let opener = Opener::from_bytes(hex(vector::SK_R).try_into().unwrap());
        assert_eq!(opener.public().to_bytes().to_vec(), hex(vector::PK_R));
    }

    #[test]
    fn keeps_the_key_bytes_it_was_given() {
        let opener = Opener::generate(&entropy(7));
        let again = Opener::from_bytes(*opener.expose());
        assert_eq!(again.public(), opener.public());
        assert_ne!(Opener::generate(&entropy(8)).public(), opener.public());
    }

    #[test]
    fn never_shows_the_key_in_debug() {
        assert_eq!(format!("{:?}", Opener::generate(&entropy(1))), "Opener(..)");
    }

    #[test]
    fn names_the_refusal() {
        assert_eq!(
            Error::Refused.to_string(),
            "the sealed value does not open with this node's seal key for this name \
             and version. Set the secret again so it is sealed to this node"
        );
    }

    proptest! {
        #[test]
        fn opens_what_it_sealed(value: Vec<u8>, draw: u64) {
            let opener = Opener::generate(&entropy(draw));
            let token = name("site.secrets.token");
            let plain = Value::new(value.clone());
            let sealed = seal(&opener.public(), &token, 1, &plain, &entropy(!draw));
            prop_assert_eq!(sealed.len(), value.len() + 48);
            let opened = opener.open(&token, 1, &sealed).unwrap();
            prop_assert_eq!(opened.expose(), &value[..]);
        }

        #[test]
        fn refuses_a_changed_or_cut_value(value: Vec<u8>, at: usize, bit in 0..8u8) {
            let opener = Opener::generate(&entropy(3));
            let token = name("site.secrets.token");
            let plain = Value::new(value);
            let sealed = seal(&opener.public(), &token, 1, &plain, &entropy(4));
            let mut flipped = sealed.clone();
            flipped[at % sealed.len()] ^= 1 << bit;
            prop_assert_eq!(opener.open(&token, 1, &flipped).unwrap_err(), Error::Refused);
            let cut = &sealed[..at % sealed.len()];
            prop_assert_eq!(opener.open(&token, 1, cut).unwrap_err(), Error::Refused);
        }
    }

    #[test]
    fn refuses_another_key_name_or_version() {
        let opener = Opener::generate(&entropy(5));
        let token = name("site.secrets.token");
        let sealed = seal(
            &opener.public(),
            &token,
            2,
            &Value::new(b"t".to_vec()),
            &entropy(6),
        );
        let other = Opener::generate(&entropy(9));
        assert_eq!(other.open(&token, 2, &sealed).unwrap_err(), Error::Refused);
        let key = name("site.secrets.key");
        assert_eq!(opener.open(&key, 2, &sealed).unwrap_err(), Error::Refused);
        for version in [1, 3, u64::MAX] {
            let refused = opener.open(&token, version, &sealed).unwrap_err();
            assert_eq!(refused, Error::Refused, "version {version}");
        }
    }

    #[test]
    fn refuses_an_encapsulated_key_of_small_order() {
        let own = key(vector::SK_R);
        for small in [
            "00",
            "01",
            "e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800",
        ] {
            let mut enc = hex(small);
            enc.resize(32, 0);
            let pk_r = hex(vector::PK_R);
            assert!(schedule(&own, &enc, &enc, &pk_r, INFO).is_none(), "{small}");
        }
    }

    #[test]
    fn refuses_a_public_key_with_the_top_bit_set() {
        let mut bytes = Opener::generate(&entropy(21)).public().to_bytes();
        bytes[31] |= 0x80;
        assert_eq!(SealKey::new(bytes), Err(types::node::BadSealKey));
    }
}
