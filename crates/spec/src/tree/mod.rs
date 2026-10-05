//! The prolly tree that holds the spec of one region: entries in name order, cut into
//! chunks at boundaries that the content decides, each chunk addressed by its hash.
//!
//! The tree is a function of its entries. The same entries give the same chunks and
//! the same root hash, in any order of changes. A change rewrites only the chunks
//! near it and their parents.
//!
//! The tree does no I/O. The caller keeps the chunk bytes, puts the chunks that an
//! operation needs into a [`Chunks`], and stores the chunks that [`apply`] adds.

mod chunk;
mod chunker;

use std::collections::BTreeMap;
use std::fmt;

use types::hash::{Map, Set};
use types::name::Name;

use chunk::{Entry, Node};
use chunker::{SCALE, Writer};

pub use chunk::Hash;

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
    /// and run the operation again.
    Missing(Hash),
    /// The bytes with this hash are not a chunk of a tree.
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
pub struct Chunks(Map<Hash, Vec<u8>>);

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
}

/// Returns the value of `name` in the tree at `root`.
///
/// # Errors
///
/// [`Error::Missing`] if a chunk on the path to `name` is not in `chunks`.
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
        node = chunks.node(entry.child())?;
    }
}

/// The result of [`apply`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Update {
    /// The root hash of the changed tree.
    pub root: Hash,
    /// The hash of each chunk that is in the changed tree and not in the tree
    /// before, in hash order. The chunks are now in the [`Chunks`].
    pub chunks: Vec<Hash>,
}

/// Changes the tree at `root`, and adds the chunks that it makes to `chunks`. A
/// change with a value sets the entry of that name. A change with `None` deletes it.
/// The last change to a name wins.
///
/// # Errors
///
/// [`Error::Missing`] if a chunk near a change is not in `chunks`. `chunks` is then
/// not changed.
pub fn apply(
    chunks: &mut Chunks,
    root: Hash,
    changes: impl IntoIterator<Item = (Name, Option<Vec<u8>>)>,
) -> Result<Update, Error> {
    apply_at(chunks, SCALE, root, changes)
}

// A put or a delete of the entry with a key, at one level.
type Edit = (Vec<u8>, Option<Vec<u8>>);

fn apply_at(
    chunks: &mut Chunks,
    scale: u32,
    root: Hash,
    changes: impl IntoIterator<Item = (Name, Option<Vec<u8>>)>,
) -> Result<Update, Error> {
    let changes = changes.into_iter();
    let edits: BTreeMap<_, _> = changes
        .map(|(name, value)| (name.as_str().as_bytes().to_vec(), value))
        .collect();
    let mut edits: Vec<Edit> = edits.into_iter().collect();
    let mut fresh = BTreeMap::new();
    let top = chunks.node(root)?;
    let mut level = top.level;
    for level in 0..=level {
        edits = rewrite(chunks, scale, root, level, &edits, &mut fresh)?;
        if edits.is_empty() {
            let chunks = Vec::new();
            return Ok(Update { root, chunks });
        }
    }
    // The chunks of the top level: the old root, changed by the last edits.
    let mut tops = BTreeMap::new();
    if let Some(key) = top.last_key() {
        tops.insert(key.to_vec(), root);
    }
    for (key, hash) in edits {
        match hash {
            Some(hash) => tops.insert(key, as_hash(&hash)),
            None => tops.remove(&key),
        };
    }
    while tops.len() > 1 {
        level += 1;
        let mut writer = Writer::new(scale, level);
        for (key, hash) in &tops {
            writer.push(key, &hash.0);
        }
        tops = BTreeMap::new();
        for (key, bytes) in writer.finish() {
            let hash = Hash::of(&bytes);
            fresh.insert(hash, bytes);
            tops.insert(key, hash);
        }
    }
    let mut root = tops.into_values().next().unwrap_or_else(empty);
    // A chunk with one child is not a root: its child is.
    loop {
        let node = match fresh.get(&root) {
            Some(bytes) => Node::read(root, bytes)?,
            None => chunks.node(root)?,
        };
        let [only] = node.entries[..] else { break };
        if node.level == 0 {
            break;
        }
        let child = only.child();
        fresh.remove(&root);
        root = child;
    }
    let made = fresh.keys().copied().collect();
    chunks.0.extend(fresh);
    Ok(Update { root, chunks: made })
}

fn as_hash(payload: &[u8]) -> Hash {
    let bytes = payload.try_into();
    Hash(bytes.expect("invariant: an entry above a leaf holds a 32-byte hash"))
}

// Applies `edits` (in key order) to the chunks of `level`. Puts each chunk that it
// makes into `fresh`, and returns the edits for the level above.
fn rewrite(
    chunks: &Chunks,
    scale: u32,
    root: Hash,
    level: u8,
    mut edits: &[Edit],
    fresh: &mut BTreeMap<Hash, Vec<u8>>,
) -> Result<Vec<Edit>, Error> {
    let mut up = BTreeMap::new();
    while let Some((first, _)) = edits.first() {
        let mut cursor = Cursor::seek(chunks, root, level, first)?;
        let mut writer = Writer::new(scale, level);
        let mut old: Map<&[u8], Hash> = Map::default();
        loop {
            for entry in &cursor.node.entries {
                let mut replaced = false;
                while let Some(((key, payload), rest)) = edits.split_first()
                    && key.as_slice() <= entry.key
                {
                    replaced = key.as_slice() == entry.key;
                    if let Some(payload) = payload {
                        writer.push(key, payload);
                    }
                    edits = rest;
                }
                if !replaced {
                    writer.push(entry.key, entry.payload);
                }
            }
            if let Some(key) = cursor.node.last_key() {
                old.insert(key, cursor.node.hash);
                up.insert(key.to_vec(), None);
            }
            if !cursor.advance()? {
                for (key, payload) in std::mem::take(&mut edits) {
                    if let Some(payload) = payload {
                        writer.push(key, payload);
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
            let hash = Hash::of(&bytes);
            if old.get(key.as_slice()) == Some(&hash) {
                up.remove(&key);
            } else {
                up.insert(key, Some(hash.0.to_vec()));
                fresh.insert(hash, bytes);
            }
        }
    }
    Ok(up.into_iter().collect())
}

// A position on one chunk of one level, which can move to the next chunk.
struct Cursor<'a> {
    chunks: &'a Chunks,
    // Each chunk above `node`, from the root down, and the index of the child in use.
    path: Vec<(Node<'a>, usize)>,
    node: Node<'a>,
}

impl<'a> Cursor<'a> {
    // Finds the chunk of `level` that holds `key`, or would hold it.
    fn seek(
        chunks: &'a Chunks,
        root: Hash,
        level: u8,
        key: &[u8],
    ) -> Result<Self, Error> {
        let mut path = Vec::new();
        let mut node = chunks.node(root)?;
        while node.level > level {
            let index = node.entries.partition_point(|entry| entry.key < key);
            let index = index.min(node.entries.len() - 1);
            let child = chunks.node(node.entries[index].child())?;
            path.push((node, index));
            node = child;
        }
        Ok(Self { chunks, path, node })
    }

    // Moves to the next chunk of the level. Returns false if there is none.
    fn advance(&mut self) -> Result<bool, Error> {
        while let Some((node, index)) = self.path.last_mut() {
            if *index + 1 < node.entries.len() {
                *index += 1;
                break;
            }
            self.path.pop();
        }
        let Some((node, index)) = self.path.last() else {
            return Ok(false);
        };
        let mut child = self.chunks.node(node.entries[*index].child())?;
        while child.level > self.node.level {
            let first = self.chunks.node(child.entries[0].child())?;
            self.path.push((child, 0));
            child = first;
        }
        self.node = child;
        Ok(true)
    }
}

/// The result of [`diff`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff {
    /// Each name whose entry is in only one tree, or has a different value in the
    /// two trees, in name order.
    pub names: Vec<Name>,
    /// The hash of each chunk that is in the new tree and not in the old tree.
    pub chunks: Vec<Hash>,
}

/// Compares the trees at `old` and `new`. It reads only the chunks that are in one
/// tree and not in the other.
///
/// # Errors
///
/// [`Error::Missing`] if one of those chunks is not in `chunks`.
pub fn diff(chunks: &Chunks, old: Hash, new: Hash) -> Result<Diff, Error> {
    let mut diff = Diff::default();
    if old == new {
        return Ok(diff);
    }
    let mut olds = vec![chunks.node(old)?];
    let mut news = vec![chunks.node(new)?];
    loop {
        let old_level = olds.first().map_or(0, |node| node.level);
        let new_level = news.first().map_or(0, |node| node.level);
        if old_level == new_level {
            let same: Set<Hash> = olds.iter().map(|node| node.hash).collect();
            let same: Set<Hash> = news
                .iter()
                .map(|node| node.hash)
                .filter(|hash| same.contains(hash))
                .collect();
            olds.retain(|node| !same.contains(&node.hash));
            news.retain(|node| !same.contains(&node.hash));
        }
        if new_level >= old_level {
            let made = news.iter().filter(|node| !node.entries.is_empty());
            diff.chunks.extend(made.map(|node| node.hash));
        }
        if old_level == 0 && new_level == 0 {
            break;
        }
        if old_level >= new_level {
            olds = children(chunks, &olds)?;
        }
        if new_level >= old_level {
            news = children(chunks, &news)?;
        }
    }
    let mut olds = entries(&olds).peekable();
    let mut news = entries(&news).peekable();
    loop {
        let (hash, key) = match (olds.peek(), news.peek()) {
            (Some(&(hash, old)), Some(&(_, new))) if old.key == new.key => {
                olds.next();
                news.next();
                if old.payload == new.payload {
                    continue;
                }
                (hash, old.key)
            }
            (Some(&(hash, old)), Some(&(_, new))) if old.key < new.key => {
                olds.next();
                (hash, old.key)
            }
            (Some(&(hash, old)), None) => {
                olds.next();
                (hash, old.key)
            }
            (_, Some(&(hash, new))) => {
                news.next();
                (hash, new.key)
            }
            (None, None) => break,
        };
        let name = str::from_utf8(key).ok().and_then(|name| name.parse().ok());
        diff.names.push(name.ok_or(Error::Corrupt(hash))?);
    }
    Ok(diff)
}

fn children<'a>(
    chunks: &'a Chunks,
    nodes: &[Node<'a>],
) -> Result<Vec<Node<'a>>, Error> {
    let entries = nodes.iter().flat_map(|node| &node.entries);
    entries.map(|entry| chunks.node(entry.child())).collect()
}

fn entries<'a, 'b>(
    nodes: &'b [Node<'a>],
) -> impl Iterator<Item = (Hash, Entry<'a>)> + 'b {
    nodes
        .iter()
        .flat_map(|node| node.entries.iter().map(|&entry| (node.hash, entry)))
}

#[cfg(test)]
mod tests;
