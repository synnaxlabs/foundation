//! Resolves the source of a secret on the node that runs a connector: a variable of the
//! node's environment, a file, or a name in a store adapter chosen by policy; `node`
//! hands it the sealed ciphertexts it pulls from `mesh`.

use std::fmt;

use zeroize::Zeroizing;

mod error;
mod reads;
mod resolver;
pub mod seal;
mod source;
pub mod store;

pub use error::Error;
pub use resolver::Resolver;
pub use source::{CALLS, Call, Source};

/// A secret value. `Debug` prints `<secret>`, never the bytes. Its buffer is
/// overwritten with zeros when it drops.
pub struct Value(Zeroizing<Vec<u8>>);

impl Value {
    /// Takes the bytes of a secret without a copy.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// The text that `Debug` prints in place of the bytes.
    pub const MARK: &'static str = "<secret>";

    /// The bytes. Send them only to the system they unlock.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(Self::MARK)
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
