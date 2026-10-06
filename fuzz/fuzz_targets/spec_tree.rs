//! `get`, `apply`, and `diff` of `spec::tree` never panic on chunks from a peer, and
//! a change to a tree that a diff reads whole gives the entries of the tree with the
//! change.
//!
//! Input, read from the front, with a zero for each byte past the end: a count, that
//! many chunks, then changes to the end. A chunk is a count and that many pieces. A
//! piece is a length, that many bytes, and a link: a link `k` above zero adds the
//! digest of chunk `k - 1`, where chunk 0 is the empty tree. A change is a length, a
//! name, and the length of a value, then the value; a value length of 255 deletes.

#![no_main]

use std::collections::BTreeMap;

use libfuzzer_sys::fuzz_target;
use spec::tree::{self, Change, Chunks, Diff};
use types::digest::Digest;
use types::name::Name;

const DELETE: u8 = 255;

fuzz_target!(|bytes: &[u8]| {
    let mut input = bytes;
    let mut chunks = Chunks::default();
    let mut roots = vec![tree::empty()];
    for _ in 0..take(&mut input) {
        let mut chunk = Vec::new();
        for _ in 0..take(&mut input) {
            let len = take(&mut input);
            chunk.extend_from_slice(split(&mut input, len));
            if let Some(link) = take(&mut input).checked_sub(1) {
                chunk.extend(roots[usize::from(link) % roots.len()].0);
            }
        }
        roots.push(chunks.insert(chunk));
    }
    let mut changes = Vec::new();
    while !input.is_empty() {
        let len = take(&mut input);
        let name = str::from_utf8(split(&mut input, len)).ok();
        let name = name.and_then(|name| name.parse::<Name>().ok());
        let change = match take(&mut input) {
            DELETE => name.map(Change::Delete),
            len => {
                let value = split(&mut input, len).to_vec();
                name.map(|name| Change::Set(name, value))
            }
        };
        changes.extend(change);
    }
    for (at, &root) in roots.iter().enumerate() {
        check(&mut chunks, root, roots[at.saturating_sub(1)], &changes);
    }
});

/// Runs each operation on the tree at `root`. The chunks that `apply` adds have
/// digests that no input chunk names, so they do not change a later root.
fn check(chunks: &mut Chunks, root: Digest, other: Digest, changes: &[Change]) {
    for change in changes {
        let (Change::Set(name, _) | Change::Delete(name)) = change;
        let _ = tree::get(chunks, root, name);
    }
    let _ = tree::diff(chunks, root, other);
    let whole = tree::diff(chunks, tree::empty(), root).map(|diff| entries(&diff));
    let applied = tree::apply(chunks, root, changes.iter().cloned());
    let Ok(whole) = whole else {
        return;
    };
    let update = applied.expect("a change reads a tree that a diff reads whole");
    let mut expected: BTreeMap<Name, Vec<u8>> = whole.into_iter().collect();
    for change in changes {
        match change {
            Change::Set(name, value) => expected.insert(name.clone(), value.clone()),
            Change::Delete(name) => expected.remove(name),
        };
    }
    let found =
        tree::diff(chunks, tree::empty(), update.root).map(|diff| entries(&diff));
    assert_eq!(
        found,
        Ok(expected.into_iter().collect()),
        "the change was lost"
    );
}

/// The entries of a diff from the empty tree, which are in name order.
fn entries(diff: &Diff<'_>) -> Vec<(Name, Vec<u8>)> {
    let entries: Vec<_> = diff
        .changes
        .iter()
        .map(|changed| {
            let value = changed
                .new
                .expect("a diff from the empty tree adds each entry");
            (changed.name.clone(), value.to_vec())
        })
        .collect();
    let sorted = entries.is_sorted_by(|(before, _), (after, _)| before < after);
    assert!(sorted, "a diff is out of name order");
    entries
}

fn take(input: &mut &[u8]) -> u8 {
    let Some((&byte, rest)) = input.split_first() else {
        return 0;
    };
    *input = rest;
    byte
}

fn split<'a>(input: &mut &'a [u8], len: u8) -> &'a [u8] {
    let (head, rest) = input.split_at(usize::from(len).min(input.len()));
    *input = rest;
    head
}
