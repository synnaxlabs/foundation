use std::cmp::Ordering;
use std::collections::BTreeSet;

use types::name::Name;

use super::chunk::{Entry, Node};
use types::digest::Digest;

use super::{Chunks, Error};

/// The result of [`diff`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff<'a> {
    /// Each entry that is in only one tree, or has a different value in the two
    /// trees, in name order.
    pub changes: Vec<Changed<'a>>,
    /// The digest of each chunk that is in the new tree and not in the old tree, in
    /// digest order.
    pub chunks: Vec<Digest>,
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
pub fn diff(chunks: &Chunks, old: Digest, new: Digest) -> Result<Diff<'_>, Error> {
    let mut diff = Diff::default();
    if old == new {
        return Ok(diff);
    }
    let mut olds = vec![chunks.node(old)?];
    let mut news = vec![chunks.node(new)?];
    loop {
        let old_level = olds.first().map_or(0, |node| node.level);
        let new_level = news.first().map_or(0, |node| node.level);
        let order = old_level.cmp(&new_level);
        if order == Ordering::Equal {
            let same: BTreeSet<Digest> = olds.iter().map(|node| node.digest).collect();
            let same: BTreeSet<Digest> = news
                .iter()
                .map(|node| node.digest)
                .filter(|digest| same.contains(digest))
                .collect();
            olds.retain(|node| !same.contains(&node.digest));
            news.retain(|node| !same.contains(&node.digest));
        }
        if order != Ordering::Greater {
            let made = news.iter().filter(|node| !node.entries.is_empty());
            diff.chunks.extend(made.map(|node| node.digest));
        }
        if old_level == 0 && new_level == 0 {
            break;
        }
        // The deeper side descends, until both reach the leaves together.
        if order != Ordering::Less {
            olds = children(chunks, &olds)?;
        }
        if order != Ordering::Greater {
            news = children(chunks, &news)?;
        }
    }
    diff.chunks.sort_unstable();
    let mut olds = entries(&olds).peekable();
    let mut news = entries(&news).peekable();
    loop {
        let order = match (olds.peek(), news.peek()) {
            (Some((_, old)), Some((_, new))) => old.key.cmp(new.key),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => break,
        };
        let (old, new) = match order {
            Ordering::Less => (olds.next(), None),
            Ordering::Equal => (olds.next(), news.next()),
            Ordering::Greater => (None, news.next()),
        };
        if let (Some((_, old)), Some((_, new))) = (old, new)
            && old.payload == new.payload
        {
            continue;
        }
        let Some((digest, entry)) = old.or(new) else {
            break;
        };
        let key = entry.key;
        let old = old.map(|(_, entry)| entry.payload);
        let new = new.map(|(_, entry)| entry.payload);
        let name = str::from_utf8(key).ok().and_then(|name| name.parse().ok());
        let name = name.ok_or(Error::Corrupt(digest))?;
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
        for index in 0..node.entries.len() {
            children.push(chunks.child(node, index)?);
        }
    }
    Ok(children)
}

fn entries<'a, 'b>(
    nodes: &'b [Node<'a>],
) -> impl Iterator<Item = (Digest, Entry<'a>)> + 'b {
    nodes
        .iter()
        .flat_map(|node| node.entries.iter().map(|&entry| (node.digest, entry)))
}
