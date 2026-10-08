//! The tree of one region's definitions, and its inverse.

use std::collections::BTreeMap;
use std::fmt;

use types::digest::Digest;
use types::name::Name;

use crate::definition::{self, Definition};
use crate::tree::{self, Change, Chunks, Update};

/// The tree of a region's `definitions`: each definition's canonical bytes at its tree
/// key. Adds each chunk of the tree to `chunks`, and gives the root and the digest of
/// each chunk, in digest order. No definitions give [`tree::empty`] and no chunk. It
/// does not check the definitions: [`check`](super::check) does.
///
/// # Panics
///
/// If a definition encodes to 4 GiB or more.
pub fn tree(chunks: &mut Chunks, definitions: &BTreeMap<Name, Definition>) -> Update {
    let sets = definitions
        .iter()
        .map(|(key, definition)| Change::Set(key.clone(), definition.encode()));
    tree::apply(chunks, tree::empty(), sets)
        .expect("invariant: a tree from the empty tree reads no chunk")
}

/// The definitions of the region tree at `root`, by tree key: the inverse of
/// [`tree`]. It does not check the definitions: [`check`](super::check) does.
///
/// # Errors
///
/// [`Error::Tree`] when `chunks` lacks a chunk of the tree, or a chunk does not fit
/// in a tree. [`Error::Definition`] when the value at a tree key is not a definition.
#[expect(
    clippy::missing_panics_doc,
    reason = "each entry of a diff from the empty tree is new"
)]
pub fn definitions(
    chunks: &Chunks,
    root: Digest,
) -> Result<BTreeMap<Name, Definition>, Error> {
    let diff = tree::diff(chunks, tree::empty(), root).map_err(Error::Tree)?;
    diff.changes
        .into_iter()
        .map(|changed| {
            let bytes = changed
                .new
                .expect("invariant: each entry of a diff from the empty tree is new");
            match Definition::decode(bytes) {
                Ok(definition) => Ok((changed.name, definition)),
                Err(error) => Err(Error::Definition {
                    key: changed.name,
                    error,
                }),
            }
        })
        .collect()
}

/// Why [`definitions`] cannot read a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A read of the tree failed.
    Tree(tree::Error),
    /// The value at `key` is not a definition.
    Definition {
        /// The tree key of the value.
        key: Name,
        /// Why the value does not decode.
        error: definition::Error,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tree(error) => write!(f, "{error}"),
            Self::Definition { key, error } => {
                write!(f, "the value at {key} is not a definition: {error}")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
