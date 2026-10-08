//! The subject: a person, an agent, or a program that authenticates with its keys.

use std::fmt;

use types::ed25519::PublicKey;
use types::hash::Set;

/// A person, an agent, or a program that authenticates with one of its keys. A
/// connector is not one: its node vouches for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subject {
    keys: Vec<PublicKey>,
}

impl Subject {
    /// Makes a subject that holds `keys`, sorted by their bytes, so the order of a
    /// file does not change the definition.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `keys` is empty, and [`Error::Duplicate`] at the first
    /// key, in the order given, that is a copy of a key before it.
    pub fn new(mut keys: Vec<PublicKey>) -> Result<Self, Error> {
        if keys.is_empty() {
            return Err(Error::Empty);
        }
        let mut seen = Set::default();
        if let Some((index, &key)) =
            keys.iter().enumerate().find(|&(_, &key)| !seen.insert(key))
        {
            return Err(Error::Duplicate { index, key });
        }
        keys.sort_unstable_by_key(|key| key.to_bytes());
        Ok(Self { keys })
    }

    /// The keys, sorted by their bytes. Never empty, each distinct.
    #[must_use]
    pub fn keys(&self) -> &[PublicKey] {
        &self.keys
    }
}

/// A key list that makes no subject. `Display` gives the message: a lower-case clause
/// with no final period. [`Error::fix`] gives what to do instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The list has no key.
    Empty,
    /// A key appears twice in the list.
    Duplicate {
        /// Where the second copy is in the list.
        index: usize,
        /// The key.
        key: PublicKey,
    },
}

impl Error {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(self) -> &'static str {
        match self {
            Self::Empty => "Add at least one public key",
            Self::Duplicate { .. } => "Remove the second copy of the key",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the subject has no public key"),
            Self::Duplicate { key, .. } => {
                write!(f, "the public key {key} appears twice")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn key(byte: u8) -> PublicKey {
        PublicKey::new([byte; 32]).unwrap()
    }

    #[test]
    fn sorts_the_keys_by_their_bytes() {
        let subject = Subject::new(vec![key(9), key(3), key(7)]).unwrap();
        assert_eq!(subject.keys(), [key(3), key(7), key(9)]);
    }

    #[test]
    fn refuses_an_empty_list() {
        assert_eq!(Subject::new(Vec::new()), Err(Error::Empty));
        assert_eq!(Error::Empty.to_string(), "the subject has no public key");
        assert_eq!(Error::Empty.fix(), "Add at least one public key");
    }

    #[test]
    fn refuses_the_second_copy_that_comes_first() {
        let keys = vec![key(3), key(9), key(9), key(3)];
        assert_eq!(
            Subject::new(keys),
            Err(Error::Duplicate {
                index: 2,
                key: key(9)
            })
        );
        let error = Error::Duplicate {
            index: 1,
            key: key(0xab),
        };
        assert_eq!(
            error.to_string(),
            format!("the public key {} appears twice", "ab".repeat(32))
        );
        assert_eq!(error.fix(), "Remove the second copy of the key");
    }

    proptest! {
        #[test]
        fn keeps_each_list_of_distinct_keys_in_any_order(
            (sorted, keys) in prop::collection::btree_set(any::<[u8; 32]>(), 1..8)
                .prop_flat_map(|bytes| {
                    let sorted: Vec<_> = bytes
                        .into_iter()
                        .filter_map(|b| PublicKey::new(b).ok())
                        .collect();
                    (Just(sorted.clone()), Just(sorted).prop_shuffle())
                }),
        ) {
            prop_assume!(!sorted.is_empty());
            let subject = Subject::new(keys).unwrap();
            prop_assert_eq!(subject.keys(), &sorted[..]);
        }
    }
}
