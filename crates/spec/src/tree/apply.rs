use std::collections::BTreeMap;

use types::name::Name;

use super::chunk::Node;
use super::chunker::{SCALE, Writer};
use types::digest::Digest;

use super::{Chunks, Error, empty};

/// One change to a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Sets the value of the entry with this name, and adds the entry if needed.
    Set(Name, Vec<u8>),
    /// Removes the entry with this name, if there is one.
    Delete(Name),
}

/// The result of [`apply`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    /// The root digest of the changed tree.
    pub root: Digest,
    /// The digest of each chunk that is in the changed tree and not in the tree
    /// before, in digest order. The chunks are now in the [`Chunks`].
    pub chunks: Vec<Digest>,
}

/// Changes the tree at `root`, and adds the chunks that it makes to `chunks`. The
/// last change to a name wins.
///
/// # Errors
///
/// [`Error::Missing`] if a chunk near a change is not in `chunks`.
/// [`Error::Corrupt`] if a chunk near a change does not fit in a tree. In both cases
/// `chunks` is not changed.
///
/// # Panics
///
/// If a value is 4 GiB or larger.
pub fn apply(
    chunks: &mut Chunks,
    root: Digest,
    changes: impl IntoIterator<Item = Change>,
) -> Result<Update, Error> {
    apply_at(chunks, SCALE, root, changes)
}

// A put or a delete of the entry with a key, at one level. The payload is a value
// for a leaf and a child digest above.
type Edit<P> = (Vec<u8>, Option<P>);

trait Payload {
    fn bytes(&self) -> &[u8];
}

impl Payload for Vec<u8> {
    fn bytes(&self) -> &[u8] {
        self
    }
}

impl Payload for Digest {
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

pub(super) fn apply_at(
    chunks: &mut Chunks,
    scale: u32,
    root: Digest,
    changes: impl IntoIterator<Item = Change>,
) -> Result<Update, Error> {
    let mut edits = BTreeMap::new();
    for change in changes {
        let (name, value) = match change {
            Change::Set(name, value) => (name, Some(value)),
            Change::Delete(name) => (name, None),
        };
        edits.insert(name.as_str().as_bytes().to_vec(), value);
    }
    let edits: Vec<Edit<Vec<u8>>> = edits.into_iter().collect();
    let mut fresh = BTreeMap::new();
    let top = chunks.node(root)?;
    let mut edits = rewrite(chunks, scale, root, 0, &edits, &mut fresh)?;
    for level in 1..=top.level {
        if edits.is_empty() {
            break;
        }
        edits = rewrite(chunks, scale, root, level, &edits, &mut fresh)?;
    }
    if edits.is_empty() {
        let chunks = Vec::new();
        return Ok(Update { root, chunks });
    }
    // The chunks of the top level: the old root, changed by the last edits.
    let mut tops = BTreeMap::new();
    if let Some(key) = top.last_key() {
        tops.insert(key.to_vec(), root);
    }
    for (key, digest) in edits {
        match digest {
            Some(digest) => tops.insert(key, digest),
            None => tops.remove(&key),
        };
    }
    let mut level = top.level;
    while tops.len() > 1 {
        level = level.checked_add(1).ok_or(Error::Corrupt(root))?;
        let mut writer = Writer::new(scale, level);
        for (key, digest) in &tops {
            writer.push(key, &digest.0);
        }
        tops = BTreeMap::new();
        for (key, bytes) in writer.finish() {
            let digest = Digest::of(&bytes);
            fresh.insert(digest, bytes);
            tops.insert(key, digest);
        }
    }
    let top = tops.into_values().next().unwrap_or_else(empty);
    let root = canonical(chunks, &mut fresh, top)?;
    let made = fresh.keys().copied().collect();
    chunks.0.extend(fresh);
    Ok(Update { root, chunks: made })
}

// A chunk with one child is not a root: its child is. Returns the root below `top`,
// and removes from `fresh` each chunk that it passes.
fn canonical(
    chunks: &Chunks,
    fresh: &mut BTreeMap<Digest, Vec<u8>>,
    top: Digest,
) -> Result<Digest, Error> {
    let mut root = top;
    let mut passed = Vec::new();
    loop {
        let node = match fresh.get(&root) {
            Some(bytes) => Node::read(root, bytes)?,
            None => chunks.node(root)?,
        };
        let [only] = node.entries.as_slice() else {
            break;
        };
        if node.level == 0 {
            break;
        }
        if !fresh.contains_key(&only.child()) {
            chunks.child(&node, 0)?;
        }
        passed.push(root);
        root = only.child();
    }
    for digest in passed {
        fresh.remove(&digest);
    }
    Ok(root)
}

// Applies `edits` (in key order) to the chunks of `level`. Puts each chunk that it
// makes into `fresh`, and returns the edits for the level above.
fn rewrite<P: Payload>(
    chunks: &Chunks,
    scale: u32,
    root: Digest,
    level: u8,
    mut edits: &[Edit<P>],
    fresh: &mut BTreeMap<Digest, Vec<u8>>,
) -> Result<Vec<Edit<Digest>>, Error> {
    let mut up = BTreeMap::new();
    while let Some((first, _)) = edits.first() {
        let mut cursor = Cursor::seek(chunks, root, level, first)?;
        let mut writer = Writer::new(scale, level);
        let mut old: BTreeMap<&[u8], Digest> = BTreeMap::new();
        loop {
            for entry in &cursor.node.entries {
                let mut replaced = false;
                while let Some(((key, payload), rest)) = edits.split_first()
                    && key.as_slice() <= entry.key
                {
                    replaced = key.as_slice() == entry.key;
                    if let Some(payload) = payload {
                        writer.push(key, payload.bytes());
                    }
                    edits = rest;
                }
                if !replaced {
                    writer.push(entry.key, entry.payload);
                }
            }
            if let Some(key) = cursor.node.last_key() {
                old.insert(key, cursor.node.digest);
                up.insert(key.to_vec(), None);
            }
            if !cursor.advance()? {
                for (key, payload) in std::mem::take(&mut edits) {
                    if let Some(payload) = payload {
                        writer.push(key, payload.bytes());
                    }
                }
                break;
            }
            // The old chunks and the new chunks end at the same entry here, so the
            // chunks after this point do not change.
            if writer.at_boundary() {
                break;
            }
        }
        for (key, bytes) in writer.finish() {
            let digest = Digest::of(&bytes);
            if old.get(key.as_slice()) == Some(&digest) {
                up.remove(&key);
            } else {
                up.insert(key, Some(digest));
                fresh.insert(digest, bytes);
            }
        }
    }
    Ok(up.into_iter().collect())
}

// A position on one chunk of one level, which can move to the next chunk.
pub(super) struct Cursor<'a> {
    chunks: &'a Chunks,
    // Each chunk above `node`, from the root down, and the index of the child in use.
    pub(super) path: Vec<(Node<'a>, usize)>,
    pub(super) node: Node<'a>,
}

impl<'a> Cursor<'a> {
    // Finds the chunk of `level` that holds `key`, or would hold it.
    pub(super) fn seek(
        chunks: &'a Chunks,
        root: Digest,
        level: u8,
        key: &[u8],
    ) -> Result<Self, Error> {
        let mut path = Vec::new();
        let mut node = chunks.node(root)?;
        while node.level > level {
            let index = node.entries.partition_point(|entry| entry.key < key);
            let index = index.min(node.entries.len().saturating_sub(1));
            let child = chunks.child(&node, index)?;
            path.push((node, index));
            node = child;
        }
        Ok(Self { chunks, path, node })
    }

    // Moves to the next chunk of the level. Returns false if there is none.
    pub(super) fn advance(&mut self) -> Result<bool, Error> {
        while let Some((node, index)) = self.path.last_mut() {
            let next = index.saturating_add(1);
            if next < node.entries.len() {
                *index = next;
                break;
            }
            self.path.pop();
        }
        let Some((node, index)) = self.path.last() else {
            return Ok(false);
        };
        let mut child = self.chunks.child(node, *index)?;
        while child.level > self.node.level {
            let first = self.chunks.child(&child, 0)?;
            self.path.push((child, 0));
            child = first;
        }
        self.node = child;
        Ok(true)
    }
}
