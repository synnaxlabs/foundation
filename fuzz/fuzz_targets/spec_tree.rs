//! `get`, `apply`, and `diff` of `spec::tree` never panic on chunks from a peer. On a
//! tree that a `diff` from the empty tree reads whole, that `diff` lists the entries
//! in name order, `get` agrees with it on each name, and `apply` gives a tree whose
//! entries are those entries with the changes.
//!
//! Input: a count of chunks, each a count of pieces, each a length, bytes, and a
//! link; then changes to the end, each a length, a name, and a length and value, where
//! a value length of 255 deletes. A link `k` above zero adds the digest of chunk
//! `k - 1`, modulo the chunks built so far, where chunk 0 is the empty tree. A count
//! past the end reads as zero; bytes past the end stop at the end.

#![no_main]

use std::collections::BTreeMap;

use libfuzzer_sys::fuzz_target;
use spec::tree::{self, Change, Chunks, Diff};
use types::digest::Digest;
use types::name::Name;

const DELETE: u8 = 255;

fuzz_target!(|input: &[u8]| {
    let mut input = input;
    let mut chunks = Chunks::default();
    let mut roots = vec![tree::empty()];
    for _ in 0..byte(&mut input) {
        let mut chunk = Vec::new();
        for _ in 0..byte(&mut input) {
            let len = byte(&mut input);
            chunk.extend_from_slice(bytes(&mut input, len));
            if let Some(link) = byte(&mut input).checked_sub(1) {
                chunk.extend(roots[usize::from(link) % roots.len()].0);
            }
        }
        roots.push(chunks.insert(chunk));
    }
    let mut changes = Vec::new();
    while !input.is_empty() {
        let len = byte(&mut input);
        let name = str::from_utf8(bytes(&mut input, len)).ok();
        let name = name.and_then(|name| name.parse::<Name>().ok());
        let change = match byte(&mut input) {
            DELETE => name.map(Change::Delete),
            len => {
                let value = bytes(&mut input, len).to_vec();
                name.map(|name| Change::Set(name, value))
            }
        };
        changes.extend(change);
    }
    for (at, &root) in roots.iter().enumerate() {
        check(&mut chunks, root, roots[at.saturating_sub(1)], &changes);
    }
});

/// Runs each operation on the tree at `root`, and `diff` against `previous` too.
fn check(chunks: &mut Chunks, root: Digest, previous: Digest, changes: &[Change]) {
    let _ = tree::diff(chunks, root, previous);
    let whole = tree::diff(chunks, tree::empty(), root).map(|diff| entries(&diff));
    let got: Vec<_> = changes
        .iter()
        .map(|change| tree::get(chunks, root, name(change)).map(|v| v.map(Vec::from)))
        .collect();
    let applied = tree::apply(chunks, root, changes.iter().cloned());
    let Ok(whole) = whole else {
        return;
    };
    let mut expected: BTreeMap<Name, Vec<u8>> = whole.into_iter().collect();
    for (change, got) in changes.iter().zip(got) {
        let want = expected.get(name(change)).cloned();
        assert_eq!(got, Ok(want), "get disagrees with diff");
    }
    for change in changes {
        match change {
            Change::Set(name, value) => expected.insert(name.clone(), value.clone()),
            Change::Delete(name) => expected.remove(name),
        };
    }
    let update = applied.expect("a change reads a tree that a diff reads whole");
    let found =
        tree::diff(chunks, tree::empty(), update.root).map(|diff| entries(&diff));
    let expected: Vec<_> = expected.into_iter().collect();
    assert_eq!(
        found,
        Ok(expected),
        "the changed tree is not the entries with the change"
    );
}

fn name(change: &Change) -> &Name {
    let (Change::Set(name, _) | Change::Delete(name)) = change;
    name
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

fn byte(input: &mut &[u8]) -> u8 {
    let Some((&byte, rest)) = input.split_first() else {
        return 0;
    };
    *input = rest;
    byte
}

fn bytes<'a>(input: &mut &'a [u8], len: u8) -> &'a [u8] {
    let (head, rest) = input.split_at(usize::from(len).min(input.len()));
    *input = rest;
    head
}
