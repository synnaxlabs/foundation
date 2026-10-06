//! The prolly tree that holds the spec of one region: entries in name order, cut into
//! chunks at boundaries that the content decides, each chunk addressed by its digest.
//!
//! The tree is a function of its entries. The same entries give the same chunks and
//! the same root digest, in any order of changes. A change rewrites only the chunks
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

use std::collections::BTreeMap;
use std::fmt;

use types::digest::Digest;
use types::name::Name;

use chunk::Node;

pub use apply::{Change, Update, apply};
pub use diff::{Changed, Diff, diff};

const EMPTY: &[u8] = &[0];

/// The root digest of the tree with no entries. A [`Chunks`] does not need its chunk.
#[must_use]
pub fn empty() -> Digest {
    Digest::of(EMPTY)
}

/// A tree operation that cannot finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The operation needs a chunk that the [`Chunks`] does not hold. Add the chunk
    /// and run the operation again. Each run names one chunk.
    Missing(Digest),
    /// The bytes with this digest are not a chunk that fits at its place in a tree.
    Corrupt(Digest),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(digest) => write!(f, "chunk {digest} is not here"),
            Self::Corrupt(digest) => {
                write!(f, "chunk {digest} is not a chunk of a spec tree")
            }
        }
    }
}

impl std::error::Error for Error {}

/// The chunks that tree operations can read, by digest.
#[derive(Clone, Debug, Default)]
pub struct Chunks(BTreeMap<Digest, Vec<u8>>);

impl Chunks {
    /// Adds a chunk and returns its digest.
    pub fn insert(&mut self, bytes: Vec<u8>) -> Digest {
        let digest = Digest::of(&bytes);
        self.0.insert(digest, bytes);
        digest
    }

    /// Returns the bytes of a chunk.
    #[must_use]
    pub fn get(&self, digest: Digest) -> Option<&[u8]> {
        self.0.get(&digest).map(Vec::as_slice)
    }

    fn node(&self, digest: Digest) -> Result<Node<'_>, Error> {
        let bytes = match self.0.get(&digest) {
            Some(bytes) => bytes,
            None if digest == empty() => EMPTY,
            None => return Err(Error::Missing(digest)),
        };
        Node::read(digest, bytes)
    }

    // Reads the child that entry `index` of `parent` names, and checks that it fits
    // there. The floor of a first child comes from the chunks above its parent.
    fn child<'a>(&'a self, parent: &Node<'a>, index: usize) -> Result<Node<'a>, Error> {
        let entry = parent.entries.get(index);
        let entry = entry.expect("invariant: the index names an entry of the parent");
        let before = index.checked_sub(1).and_then(|at| parent.entries.get(at));
        let mut child = self.node(entry.child())?;
        child.floor = before.map(|before| before.key).or(parent.floor);
        // `None` sorts first, so a child with no entries fails and no floor passes.
        let first = child.entries.first().map(|first| first.key);
        let fits = parent.level.checked_sub(1) == Some(child.level)
            && child.last_key() == Some(entry.key)
            && first > child.floor;
        if fits {
            Ok(child)
        } else {
            Err(Error::Corrupt(child.digest))
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
    root: Digest,
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
        node = chunks.child(&node, index)?;
    }
}

#[cfg(test)]
#[expect(clippy::arithmetic_side_effects, reason = "test code")]
mod tests;
