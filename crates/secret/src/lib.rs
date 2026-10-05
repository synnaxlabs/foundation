//! Secret values and the seam each secret store implements: the sealed store, a node
//! environment variable or file, or an outside secret manager.

use std::fmt;

use types::name::Name;

pub mod store;

/// A secret value. `Debug` prints `<secret>`, never the bytes.
pub struct Value(Box<[u8]>);

impl Value {
    /// Wraps the bytes of a secret.
    pub fn new(bytes: impl Into<Box<[u8]>>) -> Self {
        Self(bytes.into())
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

/// A place that holds secret values: the sealed store, a node environment variable
/// or file, or an outside secret manager.
pub trait Store {
    /// Reads the value of `name`.
    ///
    /// # Errors
    ///
    /// A [`store::Error`] that tells the caller what to do next.
    fn get<'a>(&'a self, name: &'a Name) -> store::Request<'a>;
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
        assert_eq!(Value::new(*b"token").expose(), b"token");
    }
}
