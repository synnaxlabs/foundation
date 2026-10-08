//! The tree of one region's definitions.

use std::collections::BTreeMap;

use types::name::Name;

use crate::definition::Definition;
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

#[cfg(test)]
mod tests;
