//! The spec pointer of a region.

use std::fmt;

use types::digest::Digest;

/// The place of a region's spec in its history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pointer {
    /// The version of the spec: 0 before its first apply, and one more at each
    /// apply that changes the root or gives an index a home.
    pub version: u64,
    /// The root of the spec's tree.
    pub root: Digest,
}

impl Pointer {
    /// The pointer after an apply on this one that moves it, at `root`.
    ///
    /// # Panics
    ///
    /// When the version is `u64::MAX`.
    #[must_use]
    pub fn next(self, root: Digest) -> Self {
        let version = self
            .version
            .checked_add(1)
            .expect("invariant: fewer than 2^64 spec changes apply");
        Self { version, root }
    }
}

impl fmt::Display for Pointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "version {}, root {}", self.version, self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pointer_shows_its_version_and_root() {
        let pointer = Pointer {
            version: 3,
            root: Digest([0xab; 32]),
        };
        assert_eq!(
            pointer.to_string(),
            format!("version 3, root {}", "ab".repeat(32))
        );
    }

    #[test]
    fn next_gives_one_more_version_at_the_new_root() {
        let pointer = Pointer {
            version: 3,
            root: Digest([0xab; 32]),
        };
        let next = Pointer {
            version: 4,
            root: Digest([0xcd; 32]),
        };
        assert_eq!(pointer.next(Digest([0xcd; 32])), next);
    }

    #[test]
    #[should_panic(expected = "invariant: fewer than 2^64 spec changes apply")]
    fn next_panics_after_the_last_version() {
        let pointer = Pointer {
            version: u64::MAX,
            root: Digest([0; 32]),
        };
        assert_eq!(pointer.next(Digest([0; 32])).version, 0);
    }
}
