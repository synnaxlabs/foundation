use std::collections::BTreeSet;

use types::name::Name;

use super::chunk::{Entry, Node};
use super::{Chunks, Error, Hash};

/// The result of [`diff`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff<'a> {
    /// Each entry that is in only one tree, or has a different value in the two
    /// trees, in name order.
    pub changes: Vec<Changed<'a>>,
    /// The hash of each chunk that is in the new tree and not in the old tree, in
    /// hash order.
    pub chunks: Vec<Hash>,
}

/// An entry that differs between two trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changed<'a> {
    /// The name of the entry.
    pub name: Name,
    /// The value in the old tree, or `None` if the old tree has no such entry.
    pub old: Option<&'a [u8]>,
    /// The value in the new tree, or `None` if the new tree has no such entry.
    pub new: Option<&'a [u8]>,
}

/// Compares the trees at `old` and `new`. It reads only the chunks that are in one
/// tree and not in the other.
///
/// # Errors
///
/// [`Error::Missing`] if one of those chunks is not in `chunks`. [`Error::Corrupt`]
/// if one of them does not fit in a tree, or has a key that is not a name.
pub fn diff(chunks: &Chunks, old: Hash, new: Hash) -> Result<Diff<'_>, Error> {
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
            let same: BTreeSet<Hash> = olds.iter().map(|node| node.hash).collect();
            let same: BTreeSet<Hash> = news
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
    diff.chunks.sort_unstable();
    let mut olds = entries(&olds).peekable();
    let mut news = entries(&news).peekable();
    loop {
        let (hash, key, old, new) = match (olds.peek().copied(), news.peek().copied()) {
            (Some((hash, old)), Some((_, new))) if old.key == new.key => {
                olds.next();
                news.next();
                if old.payload == new.payload {
                    continue;
                }
                (hash, old.key, Some(old.payload), Some(new.payload))
            }
            (Some((hash, old)), Some((_, new))) if old.key < new.key => {
                olds.next();
                (hash, old.key, Some(old.payload), None)
            }
            (Some((hash, old)), None) => {
                olds.next();
                (hash, old.key, Some(old.payload), None)
            }
            (_, Some((hash, new))) => {
                news.next();
                (hash, new.key, None, Some(new.payload))
            }
            (None, None) => break,
        };
        let name = str::from_utf8(key).ok().and_then(|name| name.parse().ok());
        let name = name.ok_or(Error::Corrupt(hash))?;
        diff.changes.push(Changed { name, old, new });
    }
    Ok(diff)
}

pub(super) fn children<'a>(
    chunks: &'a Chunks,
    nodes: &[Node<'a>],
) -> Result<Vec<Node<'a>>, Error> {
    let mut children = Vec::new();
    for node in nodes {
        for entry in &node.entries {
            children.push(chunks.child(node, entry)?);
        }
    }
    Ok(children)
}

fn entries<'a, 'b>(
    nodes: &'b [Node<'a>],
) -> impl Iterator<Item = (Hash, Entry<'a>)> + 'b {
    nodes
        .iter()
        .flat_map(|node| node.entries.iter().map(|&entry| (node.hash, entry)))
}
