//! The Ed25519 calls that the signed records of `mesh` share.

use aws_lc_rs::signature::{ED25519, Ed25519KeyPair, UnparsedPublicKey};
use types::node::{PrivateKey, PublicKey};

/// The key pair of `private`.
pub(crate) fn pair(private: &PrivateKey) -> Ed25519KeyPair {
    Ed25519KeyPair::from_seed_unchecked(&private.0)
        .expect("invariant: any 32 bytes are an Ed25519 private key")
}

/// Signs `statement` with `pair`.
pub(crate) fn sign(pair: &Ed25519KeyPair, statement: &[u8]) -> [u8; 64] {
    pair.sign(statement)
        .as_ref()
        .try_into()
        .expect("invariant: an Ed25519 signature is 64 bytes")
}

/// Whether `signature` over `statement` holds for `public`.
pub(crate) fn holds(public: PublicKey, statement: &[u8], signature: &[u8; 64]) -> bool {
    UnparsedPublicKey::new(&ED25519, public.to_bytes())
        .verify(statement, signature)
        .is_ok()
}
