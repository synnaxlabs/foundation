//! The spec pointer of a region.

use std::fmt;

use types::digest::Digest;

/// A region's spec at one version: the root digest of its tree. Version 0 is the tree
/// of the founding definitions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pointer {
    /// The count of spec changes that applied before it.
    pub version: u64,
    /// The root digest of the tree.
    pub root: Digest,
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
}
