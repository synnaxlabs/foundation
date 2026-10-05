//! Resolves a named secret on the node that runs a connector, through store adapters
//! chosen by policy; `node` hands it the sealed ciphertexts it pulls from `mesh`.

use std::fmt;

use zeroize::Zeroizing;

pub mod seal;
pub mod store;

/// A secret value. `Debug` prints `<secret>`, never the bytes. Its buffer is
/// overwritten with zeros when it drops.
pub struct Value(Zeroizing<Vec<u8>>);

impl Value {
    /// Takes the bytes of a secret without a copy.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// The bytes. Send them only to the system they unlock.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<secret>")
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn debug_never_shows_the_bytes(bytes in prop::collection::vec(any::<u8>(), 1..64)) {
            let shown = format!("{:?}", Value::new(bytes.clone()));
            prop_assert_eq!(&shown, "<secret>");
        }
    }

    #[test]
    fn exposes_the_bytes_it_wraps() {
        assert_eq!(Value::new(b"token".to_vec()).expose(), b"token");
    }
}
