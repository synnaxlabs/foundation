use std::fmt;

use types::node::PublicKey;

/// A node's Ed25519 key pair. Peers authenticate the node by its public key.
///
/// ```
/// fn load(private_key: [u8; 32]) -> types::node::PublicKey {
///     transport::Identity::new(private_key).public()
/// }
/// ```
#[derive(Clone)]
pub struct Identity {
    private_key: [u8; 32],
}

impl Identity {
    /// Makes the key pair for a 32-byte Ed25519 private key.
    ///
    /// ```
    /// let identity = transport::Identity::new([7; 32]);
    /// ```
    #[must_use]
    pub fn new(private_key: [u8; 32]) -> Self {
        Self { private_key }
    }

    /// The public key.
    ///
    /// ```
    /// fn key(identity: &transport::Identity) -> types::node::PublicKey {
    ///     identity.public()
    /// }
    /// ```
    #[must_use]
    pub fn public(&self) -> PublicKey {
        let _ = self.private_key;
        todo!("#54")
    }
}

impl fmt::Debug for Identity {
    /// Never writes the private key.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_writes_the_private_key() {
        let identity = Identity::new([0xab; 32]);
        assert_eq!(format!("{identity:?}"), "Identity { .. }");
    }
}
