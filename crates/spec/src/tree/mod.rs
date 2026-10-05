//! The prolly tree that holds the spec of one region: entries in name order, cut into
//! chunks at boundaries that the content decides, each chunk addressed by its hash.
//!
//! The tree is a function of its entries. The same entries give the same chunks and
//! the same root hash, in any order of changes. A change rewrites only the chunks
//! near it and their parents.
//!
//! The tree does no I/O. The caller keeps the chunk bytes, puts the chunks that an
//! operation needs into a [`Chunks`], and stores the chunks that [`apply`] adds.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

mod apply;
mod chunk;
mod chunker;
mod diff;
mod hash;

use std::collections::BTreeMap;
use std::fmt;

use types::name::Name;

use chunk::{Entry, Node};

pub use apply::{Change, Update, apply};
pub use diff::{Changed, Diff, diff};
pub use hash::Hash;

const EMPTY: &[u8] = &[0];

/// The root hash of the tree with no entries. A [`Chunks`] does not need its chunk.
#[must_use]
pub fn empty() -> Hash {
    Hash::of(EMPTY)
}

/// A tree operation that cannot finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The operation needs a chunk that the [`Chunks`] does not hold. Add the chunk
    /// and run the operation again. Each run names one chunk.
    Missing(Hash),
    /// The bytes with this hash are not a chunk that fits at its place in a tree.
    Corrupt(Hash),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(hash) => write!(f, "chunk {hash} is not here"),
            Self::Corrupt(hash) => {
                write!(f, "chunk {hash} is not a chunk of a spec tree")
            }
        }
    }
}

impl std::error::Error for Error {}

/// The chunks that tree operations can read, by hash.
#[derive(Clone, Debug, Default)]
pub struct Chunks(BTreeMap<Hash, Vec<u8>>);

impl Chunks {
    /// Adds a chunk and returns its hash.
    pub fn insert(&mut self, bytes: Vec<u8>) -> Hash {
        let hash = Hash::of(&bytes);
        self.0.insert(hash, bytes);
        hash
    }

    /// Returns the bytes of a chunk.
    #[must_use]
    pub fn get(&self, hash: Hash) -> Option<&[u8]> {
        self.0.get(&hash).map(Vec::as_slice)
    }

    fn node(&self, hash: Hash) -> Result<Node<'_>, Error> {
        let bytes = match self.0.get(&hash) {
            Some(bytes) => bytes,
            None if hash == empty() => EMPTY,
            None => return Err(Error::Missing(hash)),
        };
        Node::read(hash, bytes)
    }

    // Reads the child of `parent` that `entry` names, and checks that it fits there.
    fn child(&self, parent: &Node<'_>, entry: &Entry<'_>) -> Result<Node<'_>, Error> {
        let child = self.node(entry.child())?;
        let fits = parent.level.checked_sub(1) == Some(child.level)
            && child.last_key() == Some(entry.key);
        if fits {
            Ok(child)
        } else {
            Err(Error::Corrupt(child.hash))
        }
    }
}

/// Returns the value of `name` in the tree at `root`.
///
/// # Errors
///
/// [`Error::Missing`] if a chunk on the path to `name` is not in `chunks`.
/// [`Error::Corrupt`] if a chunk on that path does not fit in a tree.
pub fn get<'a>(
    chunks: &'a Chunks,
    root: Hash,
    name: &Name,
) -> Result<Option<&'a [u8]>, Error> {
    let key = name.as_str().as_bytes();
    let mut node = chunks.node(root)?;
    loop {
        let index = node.entries.partition_point(|entry| entry.key < key);
        let Some(entry) = node.entries.get(index) else {
            return Ok(None);
        };
        if node.level == 0 {
            return Ok((entry.key == key).then_some(entry.payload));
        }
        node = chunks.child(&node, entry)?;
    }
}

#[cfg(test)]
#[expect(clippy::arithmetic_side_effects, reason = "test code")]
mod tests;
