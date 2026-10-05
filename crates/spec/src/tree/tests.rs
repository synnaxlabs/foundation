use proptest::prelude::*;
use proptest::sample::Index;

use std::collections::BTreeSet;

use super::apply::{Cursor, apply_at};
use super::diff::children;
use super::*;

// A small scale gives trees of several levels from a few hundred entries.
const SMALL: u32 = 256;

type Model = BTreeMap<u16, Vec<u8>>;
// A change by id: a value sets the entry, and `None` deletes it.
type Step = (u16, Option<Vec<u8>>);
// An entry that differs: its name, its old value, and its new value.
type Pair = (Name, Option<Vec<u8>>, Option<Vec<u8>>);

// Some names are longer than a chunk at the small scale.
fn name(id: u16) -> Name {
    let long = if id.is_multiple_of(41) { 230 } else { 0 };
    let name = format!("site_{}.pt_{id}{}", id % 7, "x".repeat(long));
    name.parse().unwrap()
}

fn step(chunks: &mut Chunks, scale: u32, root: Digest, steps: &[Step]) -> Update {
    let changes = steps.iter().map(|(id, value)| match value {
        Some(value) => Change::Set(name(*id), value.clone()),
        None => Change::Delete(name(*id)),
    });
    apply_at(chunks, scale, root, changes).unwrap()
}

fn build(chunks: &mut Chunks, scale: u32, model: &Model) -> Digest {
    let changes: Vec<_> = model.iter().map(|(id, v)| (*id, Some(v.clone()))).collect();
    step(chunks, scale, empty(), &changes).root
}

fn digests(update: &Update) -> BTreeSet<Digest> {
    update.chunks.iter().copied().collect()
}

// Each chunk of the tree at `root`, without the chunk of the empty tree.
fn reachable(chunks: &Chunks, root: Digest) -> BTreeSet<Digest> {
    let mut found = BTreeSet::new();
    let mut level = vec![chunks.node(root).unwrap()];
    while let Some(first) = level.first() {
        found.extend(level.iter().map(|node| node.digest));
        if first.level == 0 {
            break;
        }
        level = children(chunks, &level).unwrap();
    }
    found.remove(&empty());
    found
}

fn height(chunks: &Chunks, root: Digest) -> usize {
    usize::from(chunks.node(root).unwrap().level) + 1
}

fn changed(old: &Model, new: &Model) -> Vec<Pair> {
    let ids = old.keys().chain(new.keys()).copied();
    let ids: BTreeSet<u16> = ids.filter(|id| old.get(id) != new.get(id)).collect();
    let pair = |id| (name(id), old.get(&id).cloned(), new.get(&id).cloned());
    let mut pairs: Vec<Pair> = ids.into_iter().map(pair).collect();
    pairs.sort();
    pairs
}

fn pairs(diff: &Diff<'_>) -> Vec<Pair> {
    let pair = |changed: &Changed<'_>| {
        let old = changed.old.map(<[u8]>::to_vec);
        (changed.name.clone(), old, changed.new.map(<[u8]>::to_vec))
    };
    diff.changes.iter().map(pair).collect()
}

// Some values are longer than a chunk at the small scale.
fn value() -> impl Strategy<Value = Vec<u8>> {
    let size = prop_oneof![9 => 0..48_usize, 1 => 200..1200_usize];
    size.prop_flat_map(|size| prop::collection::vec(any::<u8>(), size))
}

fn a_step() -> impl Strategy<Value = Step> {
    (0..2000_u16, prop::option::weighted(0.7, value()))
}

fn model(size: usize) -> impl Strategy<Value = Model> {
    prop::collection::btree_map(0..2000_u16, value(), 0..size)
}

#[test]
fn an_empty_tree_has_no_entries_and_no_chunks() {
    let mut chunks = Chunks::default();
    assert_eq!(get(&chunks, empty(), &name(1)), Ok(None));
    assert_eq!(diff(&chunks, empty(), empty()), Ok(Diff::default()));
    let update = step(&mut chunks, SMALL, empty(), &[]);
    assert_eq!((update.root, update.chunks.len()), (empty(), 0));
    let update = step(&mut chunks, SMALL, empty(), &[(1, None)]);
    assert_eq!((update.root, update.chunks.len()), (empty(), 0));
}

#[test]
fn a_tree_with_each_entry_deleted_is_the_empty_tree() {
    let mut chunks = Chunks::default();
    let model: Model = (0..500).map(|id| (id, vec![1; 20])).collect();
    let root = build(&mut chunks, SMALL, &model);
    assert!(height(&chunks, root) > 2);
    let deletes: Vec<_> = model.keys().map(|id| (*id, None)).collect();
    let update = step(&mut chunks, SMALL, root, &deletes);
    assert_eq!((update.root, update.chunks.len()), (empty(), 0));
    let diff = diff(&chunks, root, empty()).unwrap();
    assert_eq!(pairs(&diff), changed(&model, &Model::new()));
    assert_eq!(diff.chunks, vec![]);
}

#[test]
fn a_tree_that_shrinks_to_one_leaf_has_that_leaf_as_its_root() {
    let mut chunks = Chunks::default();
    let model: Model = (0..500).map(|id| (id, vec![1; 20])).collect();
    let root = build(&mut chunks, SMALL, &model);
    let deletes: Vec<_> = (2..500).chain([0]).map(|id| (id, None)).collect();
    let update = step(&mut chunks, SMALL, root, &deletes);
    let one = build(&mut chunks, SMALL, &Model::from([(1, vec![1; 20])]));
    assert_eq!(update.root, one);
    assert_eq!(digests(&update), BTreeSet::from([one]));
}

#[test]
fn a_root_that_gets_a_sibling_stays_in_the_tree() {
    let mut chunks = Chunks::default();
    let last = [(1437, Some(vec![0; 29]))];
    let root = step(&mut chunks, 64, empty(), &last).root;
    let first = [(581, Some(vec![])), (1246, Some(vec![0; 29]))];
    let update = step(&mut chunks, 64, root, &first);
    assert!(height(&chunks, update.root) > 1);
    assert_eq!(
        get(&chunks, update.root, &name(1437)),
        Ok(Some(&[0; 29][..]))
    );
    assert!(reachable(&chunks, update.root).contains(&root));
}

#[test]
fn get_finds_only_the_names_in_the_tree() {
    let mut chunks = Chunks::default();
    let model: Model = (100..600_u16)
        .map(|id| (id * 2, id.to_le_bytes().to_vec()))
        .collect();
    let root = build(&mut chunks, SMALL, &model);
    for id in 0..1400 {
        let found = get(&chunks, root, &name(id)).unwrap();
        assert_eq!(found, model.get(&id).map(Vec::as_slice), "{id}");
    }
}

#[test]
fn the_last_change_to_a_name_wins() {
    let mut chunks = Chunks::default();
    let changes = [(1, Some(vec![1])), (1, None), (1, Some(vec![3]))];
    let root = step(&mut chunks, SMALL, empty(), &changes).root;
    assert_eq!(get(&chunks, root, &name(1)), Ok(Some(&[3][..])));
    let root = step(&mut chunks, SMALL, root, &[(1, Some(vec![4])), (1, None)]).root;
    assert_eq!(root, empty());
}

#[test]
fn a_change_that_changes_nothing_makes_no_chunks() {
    let mut chunks = Chunks::default();
    let model: Model = (0..500).map(|id| (id, vec![1; 20])).collect();
    let root = build(&mut chunks, SMALL, &model);
    let same = [
        (7, Some(vec![1; 20])),
        (300, Some(vec![1; 20])),
        (900, None),
    ];
    let update = step(&mut chunks, SMALL, root, &same);
    assert_eq!((update.root, update.chunks.len()), (root, 0));
}

#[test]
fn a_missing_chunk_is_named() {
    let mut all = Chunks::default();
    let model: Model = (0..500).map(|id| (id, vec![1; 20])).collect();
    let root = build(&mut all, SMALL, &model);
    let first = Cursor::seek(&all, root, 0, b"").unwrap().node.digest;
    let mut chunks = all.clone();
    chunks.0.remove(&first);
    let key = model.keys().copied().min_by_key(|id| name(*id)).unwrap();

    assert_eq!(get(&chunks, root, &name(key)), Err(Error::Missing(first)));
    let change = [Change::Delete(name(key)), Change::Set(name(1999), vec![2])];
    assert_eq!(apply(&mut chunks, root, change), Err(Error::Missing(first)));
    assert_eq!(chunks.0.len(), all.0.len() - 1);
    assert_eq!(diff(&chunks, empty(), root), Err(Error::Missing(first)));
    let mut none = Chunks::default();
    assert_eq!(get(&none, root, &name(key)), Err(Error::Missing(root)));
    assert_eq!(apply(&mut none, root, []), Err(Error::Missing(root)));
    assert_eq!(diff(&none, root, empty()), Err(Error::Missing(root)));
    assert_eq!(
        Error::Missing(Digest([0x1f; 32])).to_string(),
        format!("chunk {} is not here", "1f".repeat(32)),
    );
}

#[test]
fn a_change_reads_only_the_chunks_near_it() {
    let mut all = Chunks::default();
    let root = plant(&mut all).root;
    let (first, value) = definition(0);
    let mut near = Chunks::default();
    for level in 0..2 {
        let key = first.as_str().as_bytes();
        let mut cursor = Cursor::seek(&all, root, level, key).unwrap();
        for _ in 0..3 {
            let path = cursor.path.iter().map(|(node, _)| node);
            for node in path.chain([&cursor.node]) {
                near.insert(all.0[&node.digest].clone());
            }
            cursor.advance().unwrap();
        }
    }
    assert!(near.0.len() < 10);

    let change = [Change::Set(first.clone(), vec![1])];
    let update = apply(&mut near, root, change.clone()).unwrap();
    assert_eq!(update, apply(&mut all, root, change).unwrap());
    assert_eq!(get(&near, root, &first), Ok(Some(value.as_slice())));
}

#[test]
fn a_missing_chunk_after_the_change_is_named() {
    let mut all = Chunks::default();
    let root = plant(&mut all).root;
    let (first, _) = definition(0);
    let key = first.as_str().as_bytes();
    for level in 0..2 {
        let mut cursor = Cursor::seek(&all, root, level, key).unwrap();
        cursor.advance().unwrap();
        let next = cursor.node.digest;
        let mut chunks = all.clone();
        chunks.0.remove(&next);
        let change = [Change::Set(first.clone(), vec![1])];
        assert_eq!(apply(&mut chunks, root, change), Err(Error::Missing(next)));
        assert_eq!(diff(&chunks, empty(), root), Err(Error::Missing(next)));
    }
}

// Builds a chunk from parts, as a peer with a fault can.
fn raw(chunks: &mut Chunks, level: u8, entries: &[(&str, &[u8])]) -> Digest {
    let mut bytes = vec![level];
    for (key, payload) in entries {
        chunk::write(&mut bytes, level, key.as_bytes(), payload);
    }
    chunks.insert(bytes)
}

#[test]
fn a_child_at_the_wrong_level_is_named() {
    let mut chunks = Chunks::default();
    let leaf = raw(&mut chunks, 0, &[("a", b"v")]);
    let inner = raw(&mut chunks, 1, &[("a", &leaf.0)]);
    let root = raw(&mut chunks, 2, &[("a", &inner.0), ("b", &leaf.0)]);
    assert_eq!(diff(&chunks, empty(), root), Err(Error::Corrupt(leaf)));
    let b = "b".parse().unwrap();
    assert_eq!(get(&chunks, root, &b), Err(Error::Corrupt(leaf)));

    let top = raw(&mut chunks, 255, &[("a", &leaf.0)]);
    let change = [Change::Set("zz".parse().unwrap(), vec![1])];
    assert_eq!(apply(&mut chunks, top, change), Err(Error::Corrupt(leaf)));
}

#[test]
fn a_child_with_another_last_key_is_named() {
    let mut chunks = Chunks::default();
    let mut child = raw(&mut chunks, 0, &[("b", b"v")]);
    // Without the check, two entries share each child, and a diff reads 2^20 leaves.
    for level in 1..=20 {
        child = raw(&mut chunks, level, &[("a", &child.0), ("b", &child.0)]);
    }
    let found = diff(&chunks, empty(), child);
    assert!(matches!(found, Err(Error::Corrupt(_))), "{found:?}");
    let shared = raw(&mut chunks, 0, &[("b", b"v")]);
    let root = raw(&mut chunks, 1, &[("a", &shared.0), ("b", &shared.0)]);
    assert_eq!(diff(&chunks, empty(), root), Err(Error::Corrupt(shared)));
}

#[test]
fn a_value_longer_than_four_scales_is_alone_in_its_leaf() {
    let mut chunks = Chunks::default();
    let mut model: Model = (1..200).map(|id| (id, vec![1; 8])).collect();
    model.insert(0, vec![7; 1100]);
    let root = build(&mut chunks, SMALL, &model);
    let first = model.keys().copied().min_by_key(|id| name(*id)).unwrap();
    let key = name(first);
    let leaf = Cursor::seek(&chunks, root, 0, key.as_str().as_bytes()).unwrap();
    assert_eq!(leaf.node.entries.len(), 1);
    assert_eq!(leaf.node.entries[0].key, key.as_str().as_bytes());
}

// A chunk at the small scale is shorter than the longest name, so this covers the
// rule that a chunk above the leaves holds two entries.
#[test]
fn a_name_longer_than_a_chunk_fits_in_the_tree() {
    let long: Name = "a".repeat(255).parse().unwrap();
    let mut chunks = Chunks::default();
    let model: Model = (0..500).map(|id| (id, vec![1; 20])).collect();
    let root = build(&mut chunks, SMALL, &model);
    let change = [Change::Set(long.clone(), vec![1])];
    let update = apply_at(&mut chunks, SMALL, root, change.clone()).unwrap();
    assert_eq!(get(&chunks, update.root, &long), Ok(Some(&[1][..])));
    assert_eq!(get(&chunks, update.root, &name(1)), Ok(Some(&[1; 20][..])));

    let steps: Vec<Step> = model.into_iter().map(|(id, v)| (id, Some(v))).collect();
    let mut whole = Chunks::default();
    let first = apply_at(&mut whole, SMALL, empty(), change).unwrap().root;
    assert_eq!(update.root, step(&mut whole, SMALL, first, &steps).root);
}

#[test]
fn bytes_that_are_not_a_chunk_are_named() {
    let mut chunks = Chunks::default();
    let root = chunks.insert(vec![0, 9]);
    assert_eq!(get(&chunks, root, &name(1)), Err(Error::Corrupt(root)));
    assert_eq!(apply(&mut chunks, root, []), Err(Error::Corrupt(root)));
    assert_eq!(diff(&chunks, empty(), root), Err(Error::Corrupt(root)));
    // A leaf whose key is not a name.
    let root = chunks.insert(vec![0, 1, b'!', 0]);
    assert_eq!(diff(&chunks, empty(), root), Err(Error::Corrupt(root)));
    assert_eq!(
        Error::Corrupt(Digest([0x1f; 32])).to_string(),
        format!("chunk {} is not a chunk of a spec tree", "1f".repeat(32)),
    );
}

// A spec of 50,000 definitions with names and value sizes like real ones.
fn plant(chunks: &mut Chunks) -> Update {
    apply(chunks, empty(), (0..50_000).map(set)).unwrap()
}

fn set(id: u32) -> Change {
    let (name, value) = definition(id);
    Change::Set(name, value)
}

fn definition(id: u32) -> (Name, Vec<u8>) {
    let name = format!("plant_{}.line_{}.sensor_{id}", id % 5, id % 61);
    let size = 60 + usize::try_from(id.wrapping_mul(2_654_435_761) % 120).unwrap();
    (name.parse().unwrap(), vec![0xA5; size])
}

#[test]
fn chunk_sizes_stay_near_the_scale() {
    let mut chunks = Chunks::default();
    let update = plant(&mut chunks);
    assert_eq!(height(&chunks, update.root), 3);
    let made = update
        .chunks
        .iter()
        .map(|digest| chunks.get(*digest).unwrap());
    let leaves = made.filter(|bytes| bytes[0] == 0);
    let mut sizes: Vec<usize> = leaves.map(<[u8]>::len).collect();
    sizes.sort_unstable();
    let at = |percent: usize| sizes[(sizes.len() - 1) * percent / 100];
    let mean = sizes.iter().sum::<usize>() / sizes.len();
    assert!((3400..4200).contains(&mean), "mean {mean}");
    assert!(at(5) > 1500, "p5 {}", at(5));
    assert!(at(95) < 6000, "p95 {}", at(95));
    assert!(at(100) < 2 * 4096, "max {}", at(100));
}

#[test]
fn one_change_to_a_large_tree_rewrites_about_one_chunk_for_each_level() {
    let mut chunks = Chunks::default();
    let root = plant(&mut chunks).root;
    let mut counts = BTreeMap::new();
    for id in (0..75_000).step_by(37) {
        let (name, value) = definition(id);
        let change = match id % 3 {
            0 => Change::Delete(name),
            1 => Change::Set(name, vec![9; 33]),
            _ => Change::Set(name, value),
        };
        let update = apply(&mut chunks.clone(), root, [change]).unwrap();
        *counts.entry(update.chunks.len()).or_insert(0_usize) += 1;
    }
    let changes: usize = counts.range(1..).map(|(_, changes)| changes).sum();
    assert!(counts[&3] * 10 > changes * 8, "{counts:?}");
    assert!(counts.keys().all(|made| *made <= 16), "{counts:?}");
}

proptest! {
    #[test]
    fn the_same_entries_give_the_same_root_in_any_order(
        batches in prop::collection::vec(prop::collection::vec(a_step(), 1..40), 0..24),
    ) {
        let mut chunks = Chunks::default();
        let mut model = Model::new();
        let mut root = empty();
        for batch in &batches {
            let before = model.clone();
            for (id, value) in batch {
                match value {
                    Some(value) => model.insert(*id, value.clone()),
                    None => model.remove(id),
                };
            }
            let update = step(&mut chunks, SMALL, root, batch);
            let old = reachable(&chunks, root);
            let new = reachable(&chunks, update.root);
            let made: BTreeSet<Digest> = new.difference(&old).copied().collect();
            prop_assert_eq!(digests(&update), made);
            let diff = diff(&chunks, root, update.root).unwrap();
            prop_assert_eq!(&diff.chunks, &update.chunks);
            prop_assert_eq!(pairs(&diff), changed(&before, &model));
            root = update.root;
        }
        prop_assert_eq!(root, build(&mut Chunks::default(), SMALL, &model));
        for (id, value) in &model {
            prop_assert_eq!(get(&chunks, root, &name(*id)), Ok(Some(value.as_slice())));
        }
    }

    #[test]
    fn one_change_rewrites_a_few_chunks_for_each_level(
        model in model(1500),
        id in 0..2000_u16,
        value in prop::option::of(value()),
    ) {
        let mut chunks = Chunks::default();
        let root = build(&mut chunks, SMALL, &model);
        let update = step(&mut chunks, SMALL, root, &[(id, value)]);
        let bound = BOUND * height(&chunks, root).max(height(&chunks, update.root));
        prop_assert!(update.chunks.len() <= bound, "{} chunks", update.chunks.len());
    }

    #[test]
    fn a_diff_is_the_set_difference_of_the_entries(
        old in model(400),
        new in model(400),
    ) {
        let mut chunks = Chunks::default();
        let old_root = build(&mut chunks, SMALL, &old);
        let new_root = build(&mut chunks, SMALL, &new);
        let diff = diff(&chunks, old_root, new_root).unwrap();
        prop_assert_eq!(pairs(&diff), changed(&old, &new));
        let old_chunks = reachable(&chunks, old_root);
        let new_chunks = reachable(&chunks, new_root);
        let made: Vec<Digest> = new_chunks.difference(&old_chunks).copied().collect();
        prop_assert_eq!(diff.chunks, made);
    }
}

// The chunks that one change can make for each level, at the small scale.
const BOUND: usize = 16;

// A change to the chunk format or the boundary rule changes these digests, and with
// them the root digest of every stored spec.
#[test]
fn the_roots_of_known_trees_do_not_change() {
    let mut chunks = Chunks::default();
    let plant = plant(&mut chunks).root;
    let roots = [empty(), plant].map(|root| root.to_string());
    let known = [
        "2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213",
        "32c5b07b5b6059284caec8ed6b05c67c3b4a590d6600b055dd347debac1f31d6",
    ];
    assert_eq!(roots, known);
}

// A chunk of any level whose entries name earlier chunks, or hold loose bytes.
type Loose = (u8, Vec<(u8, Result<Index, Vec<u8>>)>);

fn loose() -> impl Strategy<Value = Vec<Loose>> {
    let payload = prop_oneof![
        3 => any::<Index>().prop_map(Ok),
        1 => prop::collection::vec(any::<u8>(), 0..40).prop_map(Err),
    ];
    let entries = prop::collection::vec((b'a'..b'f', payload), 0..4);
    prop::collection::vec((0..4_u8, entries), 1..12)
}

proptest! {
    #[test]
    fn chunks_from_a_peer_with_a_fault_give_a_result(
        loose in loose(),
        bytes in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let mut chunks = Chunks::default();
        let mut made = vec![chunks.insert(bytes)];
        for (level, entries) in &loose {
            let mut bytes = vec![*level];
            for (key, payload) in entries {
                let payload = match payload {
                    Ok(index) => made[index.index(made.len())].0.to_vec(),
                    Err(bytes) => bytes.clone(),
                };
                bytes.push(1);
                bytes.push(*key);
                if *level == 0 {
                    bytes.push(u8::try_from(payload.len()).unwrap());
                }
                bytes.extend(payload);
            }
            made.push(chunks.insert(bytes));
        }
        let name: Name = "c".parse().unwrap();
        for &root in &made {
            let change = [Change::Set(name.clone(), vec![1; 300])];
            let results = (
                get(&chunks, root, &name).is_ok(),
                diff(&chunks, empty(), root).is_ok(),
                diff(&chunks, root, made[0]).is_ok(),
                apply_at(&mut chunks.clone(), SMALL, root, change).is_ok(),
            );
            // A tree that a diff can read whole is a tree that a change can read.
            prop_assert!(!results.1 || results.3, "{results:?}");
        }
    }
}
