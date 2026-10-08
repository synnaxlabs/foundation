//! The Ed25519 calls that the signed records of `mesh` share.

use aws_lc_rs::signature::Ed25519KeyPair;
use types::node::PrivateKey;

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
