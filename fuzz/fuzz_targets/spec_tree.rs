//! `get`, `apply`, and `diff` of `spec::tree` never panic on chunks from a peer. On a
//! tree that a `diff` from the empty tree reads whole, that `diff` lists the entries
//! in name order, `get` agrees with it on each name, and `apply` gives a tree whose
//! entries are those entries with the changes. `spec::region::definitions` gives the
//! error of that `diff`, the first entry in name order that is not a definition, or
//! the decoded entries exactly when `spec::region::tree` of them has the same root.
//!
//! Input: a count of chunks, each a count of pieces, each a length, bytes, and a
//! link. A length from `KEY` to `VALUE - 1` adds the entry key `p.a`, `p.b`, or
//! `p.c` instead of bytes. A length of `VALUE` or more adds an index channel
//! definition with its value length, so that a short input builds a tree that
//! `definitions` reads. Then changes to the end, each a length, a name, and a length
//! and value, where a value length of 255 deletes. A link `k` above zero adds the
//! digest of chunk `k - 1`, modulo the chunks built so far, where chunk 0 is the
//! empty tree. A count past the end reads as zero; bytes past the end stop at the
//! end.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use std::collections::{BTreeMap, BTreeSet};

use libfuzzer_sys::fuzz_target;
use spec::channel::{Channel, Kind};
use spec::definition::Definition;
use spec::region;
use spec::tree::{self, Change, Chunks, Diff, Error};
use types::channel::Key;
use types::digest::Digest;
use types::name::Name;

const DELETE: u8 = 255;
const KEY: u8 = 250;
const VALUE: u8 = 253;

fuzz_target!(|input: &[u8]| {
    let mut input = input;
    let mut chunks = Chunks::default();
    let mut roots = vec![tree::empty()];
    for _ in 0..byte(&mut input) {
        let mut chunk = Vec::new();
        for _ in 0..byte(&mut input) {
            match byte(&mut input) {
                len @ VALUE.. => {
                    let at = u128::from(len - VALUE);
                    let definition = Definition::Channel(Channel {
                        key: Key::from_u128(at + 1),
                        kind: Kind::Index {
                            error: None,
                            control: None,
                        },
                    });
                    let value = definition.encode();
                    chunk.push(u8::try_from(value.len()).expect("a short value"));
                    chunk.extend(value);
                }
                len @ KEY.. => chunk.extend([3, b'p', b'.', b'a' + (len - KEY)]),
                len => chunk.extend_from_slice(bytes(&mut input, len)),
            }
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
    let names: BTreeSet<Name> =
        changes.iter().map(|change| name(change).clone()).collect();
    for (at, &root) in roots.iter().enumerate() {
        check(
            &mut chunks,
            root,
            roots[at.saturating_sub(1)],
            &names,
            &changes,
        );
    }
});

/// Runs each operation on the tree at `root`, and `diff` against `previous` too.
fn check(
    chunks: &mut Chunks,
    root: Digest,
    previous: Digest,
    names: &BTreeSet<Name>,
    changes: &[Change],
) {
    let whole =
        tree::diff(chunks, tree::empty(), root).map(|diff| entries(root, &diff));
    let got: Vec<_> = names
        .iter()
        .map(|name| tree::get(chunks, root, name).map(|value| value.map(Vec::from)))
        .collect();
    let between = tree::diff(chunks, root, previous).map(drop);
    let errors = got.iter().filter_map(|got| got.as_ref().err());
    for error in errors
        .chain(whole.as_ref().err())
        .chain(between.as_ref().err())
    {
        named(chunks, error);
    }
    read(chunks, root, &whole);
    let applied = tree::apply(chunks, root, changes.iter().cloned());
    if let Err(error) = &applied {
        named(chunks, error);
    }
    let Ok(whole) = whole else {
        return;
    };
    let mut expected: BTreeMap<Name, Vec<u8>> = whole.into_iter().collect();
    for (name, got) in names.iter().zip(got) {
        assert_eq!(
            got,
            Ok(expected.get(name).cloned()),
            "get disagrees with diff"
        );
    }
    for change in changes {
        match change {
            Change::Set(name, value) => expected.insert(name.clone(), value.clone()),
            Change::Delete(name) => expected.remove(name),
        };
    }
    let update = applied.expect("a change reads a tree that a diff reads whole");
    let found = tree::diff(chunks, tree::empty(), update.root)
        .map(|diff| entries(update.root, &diff));
    let expected: Vec<_> = expected.into_iter().collect();
    assert_eq!(
        found,
        Ok(expected),
        "the changed tree is not the entries with the change"
    );
    let made = tree::diff(chunks, root, update.root).map(|diff| diff.chunks);
    assert_eq!(
        made,
        Ok(update.chunks),
        "the update does not list the chunks it made"
    );
}

/// `region::definitions` of the tree at `root`, where `whole` is its `diff` from the
/// empty tree.
fn read(chunks: &Chunks, root: Digest, whole: &Result<Vec<(Name, Vec<u8>)>, Error>) {
    let expected = whole
        .clone()
        .map_err(region::Error::Tree)
        .and_then(|entries| {
            let definitions = entries
                .into_iter()
                .map(|(key, bytes)| match Definition::decode(&bytes) {
                    Ok(definition) => Ok((key, definition)),
                    Err(error) => Err(region::Error::Definition { key, error }),
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            let rebuilt = region::tree(&mut Chunks::default(), &definitions).root;
            if rebuilt == root {
                Ok(definitions)
            } else {
                Err(region::Error::Tree(Error::Corrupt(root)))
            }
        });
    assert_eq!(
        region::definitions(chunks, root),
        expected,
        "definitions is not the decode of the tree"
    );
}

/// The chunk an error names is absent for `Missing` and present for `Corrupt`. The
/// empty tree has no chunk, so a parent that names it gives `Corrupt`.
fn named(chunks: &Chunks, error: &Error) {
    match *error {
        Error::Missing(digest) => {
            let absent = chunks.get(digest).is_none() && digest != tree::empty();
            assert!(absent, "{error}, but the chunk is here");
        }
        Error::Corrupt(digest) => {
            let present = chunks.get(digest).is_some() || digest == tree::empty();
            assert!(present, "{error}, but the chunk is not here");
        }
    }
}

fn name(change: &Change) -> &Name {
    let (Change::Set(name, _) | Change::Delete(name)) = change;
    name
}

/// The entries of a diff from the empty tree to `root`, which are in name order. The
/// chunks of the diff are in digest order and hold `root`.
fn entries(root: Digest, diff: &Diff<'_>) -> Vec<(Name, Vec<u8>)> {
    assert!(
        diff.chunks.is_sorted(),
        "the chunks of a diff are out of digest order"
    );
    let has_root = root == tree::empty() || diff.chunks.contains(&root);
    assert!(has_root, "a diff from the empty tree leaves out the root");
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
