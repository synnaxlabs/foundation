//! Tests of the spec that a node uses: `Mesh::spec` on one node, across a power cut,
//! and on a cluster of three voters.

use spec::definition::Kind;
use spec::region::Problem;

use super::apply::{base, create_large, create_subjects, pointer};
use super::*;
use crate::used::Cause;

/// The spec in use when the spec of `pointer`, with `definitions`, took effect.
fn in_use(pointer: Pointer, definitions: &BTreeMap<Name, Definition>) -> Spec {
    Spec {
        pointer: Some(pointer),
        definitions: Rc::new(definitions.clone()),
        behind: None,
    }
}

/// What a read of a node of a cluster gives once the spec of `pointer`, with
/// `definitions`, took effect.
fn seen(pointer: Pointer, definitions: &BTreeMap<Name, Definition>) -> Seen {
    let mut chunks = Chunks::default();
    spec::region::tree(&mut chunks, definitions);
    Seen {
        pointer: Some(pointer),
        definitions: definitions.clone(),
        behind: None,
        newest: None,
        chunks: format!("{chunks:?}"),
    }
}

/// A subject at `@admin.@subject`, which the region `plant` does not govern.
fn create_admin() -> BTreeMap<Name, Definition> {
    let subjects = create_subjects(&["plant.a"], 1);
    let subject = subjects.into_values().next().unwrap();
    [("@admin.@subject".parse().unwrap(), subject)].into()
}

/// The problem of the spec of [`create_admin`].
fn admin() -> Cause {
    Cause::Problems(vec![Problem::Ungoverned {
        name: "@admin.@subject".parse().unwrap(),
        region: "plant".parse().unwrap(),
    }])
}

/// Commits the spec change of `definitions` on `base` from node 1 through the
/// propose path, which runs no check, after a put of each chunk of its tree.
async fn commit(
    mesh: &Mesh,
    base: Pointer,
    definitions: &BTreeMap<Name, Definition>,
) -> Pointer {
    let mut chunks = Chunks::default();
    let update = spec::region::tree(&mut chunks, definitions);
    put(&mesh.store, &mesh.pool, &chunks, &update.chunks)
        .await
        .unwrap();
    let listed = update.chunks.into_iter().collect();
    let holders = [key(1)].into();
    mesh.settle_spec(base, update.root, listed, holders, BTreeMap::new())
        .await
        .unwrap()
}

/// Waits until the pointer of `mesh` is `pointer`, as after the replay of its log.
async fn reach(mesh: &Mesh, clock: &Clock, pointer: Pointer) {
    for _ in 0..100 {
        if mesh.pointer() == pointer {
            return;
        }
        clock.sleep(TICK).await;
    }
    panic!("the pointer is {:?}, not {pointer:?}", mesh.pointer());
}

/// The path of the file that names `pointer`.
fn file(pointer: Pointer) -> PathBuf {
    let name = format!("{}-{}", pointer.version, pointer.root);
    Path::new(used::SPEC).join(name)
}

/// The names in the directory of the file of the spec in use on `node`.
fn listed(sim: &mut Sim, node: &sim::node::Node) -> Vec<PathBuf> {
    let at = |node: sim::node::Node, _| async move {
        node.files().list(Path::new(used::SPEC)).await.unwrap()
    };
    sim.run_on(node, at).unwrap()
}

/// The name of the file that names `pointer`.
fn name(pointer: Pointer) -> PathBuf {
    file(pointer).file_name().unwrap().into()
}

#[test]
fn a_lone_voter_uses_the_spec_of_each_change_that_it_applies() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        assert_eq!(mesh.spec().await, Ok(in_use(base(), &BTreeMap::new())));
        let a = create_subjects(&["plant.a"], 1);
        let b = create_subjects(&["plant.b"], 1);
        let first = mesh
            .apply(base(), a.clone(), BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(mesh.spec().await, Ok(in_use(first, &a)));
        let second = mesh.apply(first, b.clone(), BTreeMap::new()).await.unwrap();
        assert_eq!(mesh.spec().await, Ok(in_use(second, &b)));
    });
}

#[test]
fn a_region_opened_with_one_founding_definition_reads_it_from_its_spec() {
    solo(|node, tasks| async move {
        let founding = create_subjects(&["plant.app"], 1);
        let mut config = config(&node, &tasks, 1, &[1], &[1]).await;
        config.founding.definitions = founding.clone();
        let mesh = Mesh::start(config).await.unwrap();
        let spec = in_use(pointer(0, &founding), &founding);
        assert_eq!(mesh.spec().await, Ok(spec));
    });
}

// The replay of the log after an open applies each change again. A read of a
// replayed pointer would create the file of its spec, which fails here.
#[test]
fn a_replayed_pointer_at_or_below_the_one_in_use_leaves_the_spec_in_use() {
    let (a, b) = (
        create_subjects(&["plant.a"], 1),
        create_subjects(&["plant.b"], 1),
    );
    let (first, second) = (pointer(1, &a), pointer(2, &b));
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let next = b.clone();
    sim.run_on(&node, move |node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        assert_eq!(mesh.apply(base(), a, BTreeMap::new()).await, Ok(first));
        assert_eq!(
            mesh.apply(first, next.clone(), BTreeMap::new()).await,
            Ok(second)
        );
        assert_eq!(mesh.spec().await, Ok(in_use(second, &next)));
    })
    .unwrap();
    sim.crash(&node, Crash::Power);
    sim.run_on(&node, move |node, tasks| async move {
        for _ in 0..5 {
            node.fail_file(&file(second), Operation::Open);
        }
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        reach(&mesh, &node.clock(), second).await;
        node.clock().sleep(seconds(3)).await;
        assert_eq!(mesh.spec().await, Ok(in_use(second, &b)));
    })
    .unwrap();
}

#[test]
fn a_founding_spec_with_problems_gives_no_spec_in_use_until_a_valid_change() {
    solo(|node, tasks| async move {
        let founding = create_admin();
        let mut config = config(&node, &tasks, 1, &[1], &[1]).await;
        config.founding.definitions = founding.clone();
        let mesh = Mesh::start(config).await.unwrap();
        let founded = pointer(0, &founding);
        let none = Spec {
            pointer: None,
            definitions: Rc::default(),
            behind: Some(Behind {
                pointer: founded,
                cause: admin(),
            }),
        };
        assert_eq!(mesh.spec().await, Ok(none));
        let a = create_subjects(&["plant.a"], 1);
        let moved = mesh
            .apply(founded, a.clone(), BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(mesh.spec().await, Ok(in_use(moved, &a)));
    });
}

// The commit of a subject at `@admin` moves the pointer, and the node keeps the last
// spec, also after a power cut. The group does not stop: a later change takes effect.
#[test]
fn a_committed_spec_with_a_subject_at_admin_keeps_the_last_spec_across_a_power_cut() {
    let a = create_subjects(&["plant.a"], 1);
    let (first, second) = (pointer(1, &a), pointer(2, &create_admin()));
    let kept = move || Spec {
        behind: Some(Behind {
            pointer: second,
            cause: admin(),
        }),
        ..in_use(first, &a)
    };
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let before = kept.clone();
    sim.run_on(&node, move |node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let a = create_subjects(&["plant.a"], 1);
        assert_eq!(mesh.apply(base(), a, BTreeMap::new()).await, Ok(first));
        assert_eq!(commit(&mesh, first, &create_admin()).await, second);
        assert_eq!(mesh.spec().await, Ok(before()));
        assert!(
            !mesh
                .spec()
                .await
                .unwrap()
                .definitions
                .contains_key(&"@admin.@subject".parse::<Name>().unwrap())
        );
    })
    .unwrap();
    sim.crash(&node, Crash::Power);
    sim.run_on(&node, move |node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        reach(&mesh, &node.clock(), second).await;
        assert_eq!(mesh.spec().await, Ok(kept()));
        let b = create_subjects(&["plant.b"], 1);
        let third = mesh
            .apply(second, b.clone(), BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(mesh.spec().await, Ok(in_use(third, &b)));
    })
    .unwrap();
}

// A subject at a key with no label never reaches the spec in use.
#[test]
fn a_committed_spec_with_a_misplaced_subject_keeps_the_last_spec() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let a = create_subjects(&["plant.a"], 1);
        let first = mesh
            .apply(base(), a.clone(), BTreeMap::new())
            .await
            .unwrap();
        let name: Name = "plant.@x.@subject".parse().unwrap();
        let subject = a.values().next().unwrap().clone();
        let second = commit(&mesh, first, &[(name.clone(), subject)].into()).await;
        let problem = Problem::Misplaced {
            name,
            kind: Kind::Subject,
        };
        let behind = Spec {
            behind: Some(Behind {
                pointer: second,
                cause: Cause::Problems(vec![problem]),
            }),
            ..in_use(first, &a)
        };
        assert_eq!(mesh.spec().await, Ok(behind));
    });
}

// The first read gets the root from the store, then misses one chunk. A retry gets
// only that chunk, so the loss of the root from the store after the first read
// changes nothing.
#[test]
fn a_retry_gets_only_the_missed_chunk_and_keeps_the_chunks_it_read() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        assert_eq!(mesh.spec().await, Ok(in_use(base(), &BTreeMap::new())));
        let definitions = create_large(200);
        let mut chunks = Chunks::default();
        let update = spec::region::tree(&mut chunks, &definitions);
        let lacked = *update.chunks.iter().find(|at| **at != update.root).unwrap();
        let held = update.chunks.iter().copied().filter(|at| *at != lacked);
        let held: Vec<Digest> = held.collect();
        put(&mesh.store, &mesh.pool, &chunks, &held).await.unwrap();
        let holders = [key(1)].into();
        let settled = mesh.settle_spec(
            base(),
            update.root,
            BTreeSet::new(),
            holders,
            BTreeMap::new(),
        );
        let moved = settled.await.unwrap();
        let missing = spec::region::Error::Tree(tree::Error::Missing(lacked));
        let behind = Spec {
            behind: Some(Behind {
                pointer: moved,
                cause: Cause::Read(missing),
            }),
            ..in_use(base(), &BTreeMap::new())
        };
        assert_eq!(mesh.spec().await, Ok(behind.clone()));
        node.clock().sleep(seconds(3)).await;
        assert_eq!(mesh.spec().await, Ok(behind));
        put(&mesh.store, &mesh.pool, &chunks, &[lacked])
            .await
            .unwrap();
        let root = Path::new(BLOB).join(update.root.to_string());
        node.files().remove(&root).await.unwrap();
        node.clock().sleep(seconds(2)).await;
        assert_eq!(mesh.spec().await, Ok(in_use(moved, &definitions)));
    });
}

#[test]
fn a_failed_get_of_the_store_leaves_the_node_behind_until_a_retry() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let a = create_subjects(&["plant.a"], 1);
        let mut chunks = Chunks::default();
        let update = spec::region::tree(&mut chunks, &a);
        put(&mesh.store, &mesh.pool, &chunks, &update.chunks)
            .await
            .unwrap();
        let path = Path::new(BLOB).join(update.root.to_string());
        node.fail_file(&path, Operation::Open);
        let holders = [key(1)].into();
        let settled = mesh.settle_spec(
            base(),
            update.root,
            BTreeSet::new(),
            holders,
            BTreeMap::new(),
        );
        let moved = settled.await.unwrap();
        let cause = files::Error::Io {
            path,
            operation: Operation::Open,
            code: 5,
        };
        let behind = Spec {
            behind: Some(Behind {
                pointer: moved,
                cause: Cause::Blob(blob::Error::Files(cause)),
            }),
            ..in_use(base(), &BTreeMap::new())
        };
        assert_eq!(mesh.spec().await, Ok(behind));
        node.clock().sleep(seconds(2)).await;
        assert_eq!(mesh.spec().await, Ok(in_use(moved, &a)));
    });
}

// The change lists the root of the tree in use, whose get fails.
#[test]
fn a_read_gets_no_listed_chunk_that_the_tree_in_use_holds() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let (a, b) = (
            create_subjects(&["plant.a"], 1),
            create_subjects(&["plant.b"], 1),
        );
        let first = mesh.apply(base(), a, BTreeMap::new()).await.unwrap();
        let mut chunks = Chunks::default();
        let update = spec::region::tree(&mut chunks, &b);
        put(&mesh.store, &mesh.pool, &chunks, &update.chunks)
            .await
            .unwrap();
        node.fail_file(
            &Path::new(BLOB).join(first.root.to_string()),
            Operation::Open,
        );
        let holders = [key(1)].into();
        let listed = [first.root].into();
        let settled =
            mesh.settle_spec(first, update.root, listed, holders, BTreeMap::new());
        let moved = settled.await.unwrap();
        assert_eq!(mesh.spec().await, Ok(in_use(moved, &b)));
    });
}

// The read misses one of two chunks. The retry gets it, and the next read waits on a
// get of the other, so the cause of the first read stays.
#[test]
fn a_retry_keeps_the_cause_of_the_read_before_it() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let mut chunks = Chunks::default();
        let update = spec::region::tree(&mut chunks, &create_large(200));
        let mut lacked = update.chunks.iter().filter(|at| **at != update.root);
        let lacked = [*lacked.next().unwrap(), *lacked.next().unwrap()];
        let held = update.chunks.iter().filter(|at| !lacked.contains(at));
        let held: Vec<Digest> = held.copied().collect();
        put(&mesh.store, &mesh.pool, &chunks, &held).await.unwrap();
        let holders = [key(1)].into();
        let settled = mesh.settle_spec(
            base(),
            update.root,
            BTreeSet::new(),
            holders,
            BTreeMap::new(),
        );
        settled.await.unwrap();
        let first = mesh.spec().await.unwrap();
        let Some(Behind {
            cause: Cause::Read(spec::region::Error::Tree(tree::Error::Missing(missed))),
            ..
        }) = first.behind
        else {
            panic!("{first:?}");
        };
        let other = *lacked.iter().find(|at| **at != missed).unwrap();
        put(&mesh.store, &mesh.pool, &chunks, &[missed])
            .await
            .unwrap();
        let block = mesh.pool.copy(chunks.get(other).unwrap()).unwrap();
        let mut stuck = pin!(mesh.store.put(other, &block));
        assert!(now(stuck.as_mut()).await.is_pending());
        node.clock().sleep(seconds(2)).await;
        assert_eq!(now(pin!(mesh.spec())).await, Poll::Ready(Ok(first)));
    });
}

// The read of v1 misses one chunk, and the retry get of it waits on a put that does
// not end. v2 commits with each chunk in the store.
#[test]
fn a_call_at_a_later_pointer_waits_for_no_retry_of_an_older_one() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let mut chunks = Chunks::default();
        let update = spec::region::tree(&mut chunks, &create_large(200));
        let lacked = *update.chunks.iter().find(|at| **at != update.root).unwrap();
        let held = update.chunks.iter().copied().filter(|at| *at != lacked);
        let held: Vec<Digest> = held.collect();
        put(&mesh.store, &mesh.pool, &chunks, &held).await.unwrap();
        let holders = [key(1)].into();
        let settled = mesh.settle_spec(
            base(),
            update.root,
            BTreeSet::new(),
            holders,
            BTreeMap::new(),
        );
        let second = settled.await.unwrap();
        let missing = spec::region::Error::Tree(tree::Error::Missing(lacked));
        let behind = Some(Behind {
            pointer: second,
            cause: Cause::Read(missing),
        });
        assert_eq!(mesh.spec().await.unwrap().behind, behind);
        let block = mesh.pool.copy(chunks.get(lacked).unwrap()).unwrap();
        let mut stuck = pin!(mesh.store.put(lacked, &block));
        assert!(now(stuck.as_mut()).await.is_pending());
        node.clock().sleep(seconds(2)).await;
        let b = create_subjects(&["plant.b"], 1);
        let third = commit(&mesh, second, &b).await;
        let mut call = pin!(mesh.spec());
        node.clock().sleep(seconds(3)).await;
        let used = Poll::Ready(Ok(in_use(third, &b)));
        assert_eq!(now(call.as_mut()).await, used);
    });
}

// The retry get of the missed chunk waits on a put that does not end, and the group
// stops. The test, the mesh, and `keep` hold the store.
#[test]
fn keep_ends_when_the_group_stops_during_a_retry() {
    solo(|node, tasks| async move {
        let config = config(&node, &tasks, 1, &[1], &[1]).await;
        let store = Rc::clone(&config.store);
        let mesh = Mesh::start(config).await.unwrap();
        let mut chunks = Chunks::default();
        let update = spec::region::tree(&mut chunks, &create_large(200));
        let lacked = *update.chunks.iter().find(|at| **at != update.root).unwrap();
        let held = update.chunks.iter().copied().filter(|at| *at != lacked);
        let held: Vec<Digest> = held.collect();
        put(&mesh.store, &mesh.pool, &chunks, &held).await.unwrap();
        let holders = [key(1)].into();
        let settled = mesh.settle_spec(
            base(),
            update.root,
            BTreeSet::new(),
            holders,
            BTreeMap::new(),
        );
        let second = settled.await.unwrap();
        assert_eq!(mesh.spec().await.unwrap().behind.unwrap().pointer, second);
        let block = mesh.pool.copy(chunks.get(lacked).unwrap()).unwrap();
        let mut stuck = pin!(mesh.store.put(lacked, &block));
        assert!(now(stuck.as_mut()).await.is_pending());
        node.clock().sleep(seconds(2)).await;
        assert_eq!(Rc::strong_count(&store), 3, "keep runs");
        let stopped = super::fail_sync(&node);
        let mut b_chunks = Chunks::default();
        let b = spec::region::tree(&mut b_chunks, &create_subjects(&["plant.b"], 1));
        put(&mesh.store, &mesh.pool, &b_chunks, &b.chunks)
            .await
            .unwrap();
        let holders = [key(1)].into();
        let settled =
            mesh.settle_spec(second, b.root, BTreeSet::new(), holders, BTreeMap::new());
        assert_eq!(settled.await, Err(Error::Stopped(stopped)));
        node.clock().sleep(seconds(3)).await;
        assert_eq!(Rc::strong_count(&store), 2, "keep runs after the stop");
    });
}

// The first read of v1 waits on a put of its root, and the group stops. The test, the
// mesh, and `keep` hold the store.
#[test]
fn a_first_read_ends_when_the_group_stops() {
    solo(|node, tasks| async move {
        let config = config(&node, &tasks, 1, &[1], &[1]).await;
        let store = Rc::clone(&config.store);
        let mesh = Mesh::start(config).await.unwrap();
        let mut chunks = Chunks::default();
        let update = spec::region::tree(&mut chunks, &create_subjects(&["plant.b"], 1));
        let held = update.chunks.iter().filter(|at| **at != update.root);
        let held: Vec<Digest> = held.copied().collect();
        put(&store, &mesh.pool, &chunks, &held).await.unwrap();
        let block = mesh.pool.copy(chunks.get(update.root).unwrap()).unwrap();
        let mut stuck = pin!(store.put(update.root, &block));
        assert!(now(stuck.as_mut()).await.is_pending());
        let holders = [key(1)].into();
        let settled = mesh.settle_spec(
            base(),
            update.root,
            BTreeSet::new(),
            holders,
            BTreeMap::new(),
        );
        let first = settled.await.unwrap();
        node.clock().sleep(seconds(2)).await;
        assert_eq!(Rc::strong_count(&store), 3, "keep runs");
        let stopped = super::fail_sync(&node);
        let c = create_subjects(&["plant.c"], 1);
        let settled = mesh.settle_spec(
            first,
            pointer(2, &c).root,
            BTreeSet::new(),
            [key(1)].into(),
            BTreeMap::new(),
        );
        assert_eq!(settled.await, Err(Error::Stopped(stopped)));
        node.clock().sleep(seconds(3)).await;
        assert_eq!(Rc::strong_count(&store), 2, "keep runs after the stop");
    });
}

// As in `a_call_at_a_later_pointer_waits_for_no_retry_of_an_older_one`, but the retry
// get finds the missed chunk, and the retry read that follows waits on a put of the
// other.
#[test]
fn a_call_at_a_later_pointer_waits_for_no_retry_read_of_an_older_one() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let mut chunks = Chunks::default();
        let update = spec::region::tree(&mut chunks, &create_large(200));
        let mut lacked = update.chunks.iter().filter(|at| **at != update.root);
        let lacked = [*lacked.next().unwrap(), *lacked.next().unwrap()];
        let held = update.chunks.iter().filter(|at| !lacked.contains(at));
        let held: Vec<Digest> = held.copied().collect();
        put(&mesh.store, &mesh.pool, &chunks, &held).await.unwrap();
        let holders = [key(1)].into();
        let settled = mesh.settle_spec(
            base(),
            update.root,
            BTreeSet::new(),
            holders,
            BTreeMap::new(),
        );
        let second = settled.await.unwrap();
        let first = mesh.spec().await.unwrap();
        let Some(Behind {
            cause: Cause::Read(spec::region::Error::Tree(tree::Error::Missing(missed))),
            ..
        }) = first.behind
        else {
            panic!("{first:?}");
        };
        let other = *lacked.iter().find(|at| **at != missed).unwrap();
        put(&mesh.store, &mesh.pool, &chunks, &[missed])
            .await
            .unwrap();
        let block = mesh.pool.copy(chunks.get(other).unwrap()).unwrap();
        let mut stuck = pin!(mesh.store.put(other, &block));
        assert!(now(stuck.as_mut()).await.is_pending());
        node.clock().sleep(seconds(2)).await;
        let b = create_subjects(&["plant.b"], 1);
        let third = commit(&mesh, second, &b).await;
        let mut call = pin!(mesh.spec());
        node.clock().sleep(seconds(3)).await;
        let used = Poll::Ready(Ok(in_use(third, &b)));
        assert_eq!(now(call.as_mut()).await, used);
    });
}

// v1 has a problem. The first read of v2 waits on a put of its root, and v3 commits
// while it waits.
#[test]
fn a_first_read_goes_on_when_a_newer_pointer_commits() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let first = commit(&mesh, base(), &create_admin()).await;
        assert_eq!(mesh.spec().await.unwrap().behind.unwrap().pointer, first);
        let b = create_subjects(&["plant.b"], 1);
        let mut b_chunks = Chunks::default();
        let b_tree = spec::region::tree(&mut b_chunks, &b);
        let held = b_tree.chunks.iter().filter(|at| **at != b_tree.root);
        let held: Vec<Digest> = held.copied().collect();
        put(&mesh.store, &mesh.pool, &b_chunks, &held)
            .await
            .unwrap();
        let block = mesh.pool.copy(b_chunks.get(b_tree.root).unwrap()).unwrap();
        let mut b_root = pin!(mesh.store.put(b_tree.root, &block));
        assert!(now(b_root.as_mut()).await.is_pending());
        let holders = [key(1)].into();
        let settled = mesh.settle_spec(
            first,
            b_tree.root,
            BTreeSet::new(),
            holders,
            BTreeMap::new(),
        );
        let second = settled.await.unwrap();
        let mut call = pin!(mesh.spec());
        assert!(now(call.as_mut()).await.is_pending());
        let mut c_chunks = Chunks::default();
        let c_tree =
            spec::region::tree(&mut c_chunks, &create_subjects(&["plant.c"], 1));
        let block = mesh.pool.copy(c_chunks.get(c_tree.root).unwrap()).unwrap();
        let mut c_root = pin!(mesh.store.put(c_tree.root, &block));
        assert!(now(c_root.as_mut()).await.is_pending());
        let holders = [key(1)].into();
        let settled = mesh.settle_spec(
            second,
            c_tree.root,
            BTreeSet::new(),
            holders,
            BTreeMap::new(),
        );
        settled.await.unwrap();
        node.clock().sleep(seconds(1)).await;
        b_root.await.unwrap();
        node.clock().sleep(seconds(1)).await;
        let used = Poll::Ready(Ok(in_use(second, &b)));
        assert_eq!(now(call.as_mut()).await, used);
    });
}

// The first read misses one chunk. The next get of it fails, and the one after finds
// no chunk.
#[test]
fn each_get_of_the_missed_chunk_gives_the_cause_of_the_spec_behind() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let definitions = create_large(200);
        let mut chunks = Chunks::default();
        let update = spec::region::tree(&mut chunks, &definitions);
        let lacked = *update.chunks.iter().find(|at| **at != update.root).unwrap();
        let held = update.chunks.iter().copied().filter(|at| *at != lacked);
        let held: Vec<Digest> = held.collect();
        put(&mesh.store, &mesh.pool, &chunks, &held).await.unwrap();
        let holders = [key(1)].into();
        let settled = mesh.settle_spec(
            base(),
            update.root,
            BTreeSet::new(),
            holders,
            BTreeMap::new(),
        );
        let moved = settled.await.unwrap();
        let behind = |cause| Spec {
            behind: Some(Behind {
                pointer: moved,
                cause,
            }),
            ..in_use(base(), &BTreeMap::new())
        };
        let missing =
            Cause::Read(spec::region::Error::Tree(tree::Error::Missing(lacked)));
        assert_eq!(mesh.spec().await, Ok(behind(missing.clone())));
        put(&mesh.store, &mesh.pool, &chunks, &[lacked])
            .await
            .unwrap();
        let path = Path::new(BLOB).join(lacked.to_string());
        node.files().remove(&path).await.unwrap();
        node.fail_file(&path, Operation::Open);
        node.clock().sleep(Span::from_nanos(1_500_000_000)).await;
        let cause = files::Error::Io {
            path,
            operation: Operation::Open,
            code: 5,
        };
        let failed = Cause::Blob(blob::Error::Files(cause));
        assert_eq!(mesh.spec().await, Ok(behind(failed)));
        node.clock().sleep(seconds(1)).await;
        assert_eq!(mesh.spec().await, Ok(behind(missing)));
    });
}

// No public call shows the chunks that the task holds for the newest pointer.
#[test]
fn a_newer_pointer_drops_the_chunks_got_for_the_one_it_replaces() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let mut at = base();
        for count in [200, 300] {
            let mut chunks = Chunks::default();
            let update = spec::region::tree(&mut chunks, &create_large(count));
            let lacked = *update.chunks.iter().find(|at| **at != update.root).unwrap();
            let held = update.chunks.iter().copied().filter(|at| *at != lacked);
            let held: Vec<Digest> = held.collect();
            put(&mesh.store, &mesh.pool, &chunks, &held).await.unwrap();
            let holders = [key(1)].into();
            let settled = mesh.settle_spec(
                at,
                update.root,
                BTreeSet::new(),
                holders,
                BTreeMap::new(),
            );
            at = settled.await.unwrap();
            assert_eq!(mesh.spec().await.unwrap().behind.unwrap().pointer, at);
            let group = mesh.group.borrow();
            let got = &group.used.newest.as_ref().unwrap().got;
            assert!(!got.is_empty(), "{count}");
            for chunk in got {
                let digest = Digest::of(chunk);
                assert!(chunks.get(digest).is_some(), "{count}: {digest}");
            }
        }
    });
}

/// Creates an empty file for each of `names` in the directory of the spec in use.
async fn create_files(node: &sim::node::Node, names: &[PathBuf]) {
    let files = node.files();
    files.create_dir(Path::new(used::SPEC)).await.unwrap();
    for name in names {
        let path = Path::new(used::SPEC).join(name);
        drop(
            files
                .open(&path, files::Mode::Create { len: 0 })
                .await
                .unwrap(),
        );
    }
}

// The log syncs the directory of the node first, so no fault reaches only the sync of
// the spec in use there.
#[test]
fn an_open_gives_the_error_of_each_failed_file_call_on_the_spec_in_use() {
    let older = Pointer {
        version: 1,
        root: Digest([1; 32]),
    };
    let newer = Pointer {
        version: 2,
        root: Digest([2; 32]),
    };
    let calls = [
        (PathBuf::from(used::SPEC), Operation::CreateDir),
        (PathBuf::from(used::SPEC), Operation::List),
        (PathBuf::from(used::SPEC), Operation::SyncDir),
        (file(older), Operation::Remove),
    ];
    for (path, operation) in calls {
        solo(move |node, tasks| async move {
            create_files(&node, &[name(older), name(newer)]).await;
            node.fail_file(&path, operation);
            let cause = files::Error::Io {
                path,
                operation,
                code: 5,
            };
            let opened = open(&node, &tasks, 1, &[1], &[1]).await.err();
            assert_eq!(opened, Some(Error::Files(cause)));
        });
    }
}

// `Files::list` gives names in text order, where `10-` comes before `9-`.
#[test]
fn an_open_uses_the_file_of_the_highest_version() {
    let nine = Pointer {
        version: 9,
        root: Digest([9; 32]),
    };
    let ten = Pointer {
        version: 10,
        root: Digest([10; 32]),
    };
    solo(move |node, tasks| async move {
        create_files(&node, &[name(nine), name(ten)]).await;
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let spec = mesh.spec().await.unwrap();
        assert_eq!(spec.behind.map(|behind| behind.pointer), Some(ten));
        let names = node.files().list(Path::new(used::SPEC)).await.unwrap();
        assert_eq!(names, [name(ten)]);
    });
}

#[test]
fn a_failed_sync_of_the_new_file_leaves_the_node_behind_until_a_retry() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let (a, b) = (
            create_subjects(&["plant.a"], 1),
            create_subjects(&["plant.b"], 1),
        );
        let first = mesh
            .apply(base(), a.clone(), BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(mesh.spec().await, Ok(in_use(first, &a)));
        node.fail_file(Path::new(used::SPEC), Operation::SyncDir);
        let second = mesh.apply(first, b.clone(), BTreeMap::new()).await.unwrap();
        let cause = files::Error::Io {
            path: used::SPEC.into(),
            operation: Operation::SyncDir,
            code: 5,
        };
        let behind = Spec {
            behind: Some(Behind {
                pointer: second,
                cause: Cause::Files(cause),
            }),
            ..in_use(first, &a)
        };
        assert_eq!(mesh.spec().await, Ok(behind));
        node.clock().sleep(seconds(2)).await;
        assert_eq!(mesh.spec().await, Ok(in_use(second, &b)));
    });
}

// A power cut loses a random part of the directory changes that no sync made durable,
// so the file of the new pointer may stay. The file of the old pointer always does.
#[test]
fn a_power_cut_before_the_sync_of_the_new_file_keeps_the_old_file() {
    let (a, b) = (
        create_subjects(&["plant.a"], 1),
        create_subjects(&["plant.b"], 1),
    );
    let (first, second) = (pointer(1, &a), pointer(2, &b));
    let mut olds = 0;
    for seed in 0..8 {
        let mut sim = Sim::new(sim::Config {
            seed,
            ..sim::Config::default()
        });
        let node = sim.node(sim::node::Config::default());
        let (applied, next) = (a.clone(), b.clone());
        sim.run_on(&node, move |node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            assert_eq!(
                mesh.apply(base(), applied, BTreeMap::new()).await,
                Ok(first)
            );
            assert_eq!(mesh.spec().await.unwrap().pointer, Some(first));
            node.fail_file(Path::new(used::SPEC), Operation::SyncDir);
            assert_eq!(mesh.apply(first, next, BTreeMap::new()).await, Ok(second));
            let spec = mesh.spec().await.unwrap();
            assert_eq!(spec.pointer, Some(first), "seed {seed}");
        })
        .unwrap();
        sim.crash(&node, Crash::Power);
        let names = listed(&mut sim, &node);
        let old = names == [name(first)];
        assert!(
            old || names == [name(first), name(second)],
            "seed {seed}: {names:?}"
        );
        olds += usize::from(old);
        let (used, definitions) = if old { (first, &a) } else { (second, &b) };
        let definitions = definitions.clone();
        sim.run_on(&node, move |node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            let spec = in_use(used, &definitions);
            assert_eq!(mesh.spec().await, Ok(spec), "seed {seed}");
        })
        .unwrap();
    }
    assert!(olds > 0, "no power cut lost the file of the new pointer");
}

#[test]
fn a_power_cut_before_the_removal_of_the_old_file_uses_the_new_spec() {
    let (a, b) = (
        create_subjects(&["plant.a"], 1),
        create_subjects(&["plant.b"], 1),
    );
    let (first, second) = (pointer(1, &a), pointer(2, &b));
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let next = b.clone();
    sim.run_on(&node, move |node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        assert_eq!(
            mesh.apply(base(), a.clone(), BTreeMap::new()).await,
            Ok(first)
        );
        assert_eq!(mesh.spec().await, Ok(in_use(first, &a)));
        node.fail_file(&file(first), Operation::Remove);
        assert_eq!(
            mesh.apply(first, next.clone(), BTreeMap::new()).await,
            Ok(second)
        );
        assert_eq!(mesh.spec().await, Ok(in_use(second, &next)));
    })
    .unwrap();
    sim.crash(&node, Crash::Power);
    assert_eq!(listed(&mut sim, &node), [name(first), name(second)]);
    sim.run_on(&node, move |node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        assert_eq!(mesh.spec().await, Ok(in_use(second, &b)));
    })
    .unwrap();
    assert_eq!(listed(&mut sim, &node), [name(second)]);
}

// The removal after the sync removes each file but the new one, so a file that a
// failed removal left goes at the next change.
#[test]
fn a_failed_removal_of_the_old_file_leaves_it_until_the_next_change() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let changes =
            ["plant.a", "plant.b", "plant.c"].map(|label| create_subjects(&[label], 1));
        let first = mesh
            .apply(base(), changes[0].clone(), BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(mesh.spec().await.unwrap().pointer, Some(first));
        node.fail_file(&file(first), Operation::Remove);
        let second = mesh
            .apply(first, changes[1].clone(), BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(mesh.spec().await, Ok(in_use(second, &changes[1])));
        let names = node.files().list(Path::new(used::SPEC)).await.unwrap();
        assert_eq!(names, [name(first), name(second)]);
        let third = mesh
            .apply(second, changes[2].clone(), BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(mesh.spec().await, Ok(in_use(third, &changes[2])));
        let names = node.files().list(Path::new(used::SPEC)).await.unwrap();
        assert_eq!(names, [name(third)]);
    });
}

#[test]
fn an_open_refuses_a_file_that_does_not_name_a_pointer() {
    solo(|node, tasks| async move {
        let files = node.files();
        let stray = Path::new(used::SPEC).join("1-00");
        files.create_dir(Path::new(used::SPEC)).await.unwrap();
        drop(
            files
                .open(&stray, files::Mode::Create { len: 0 })
                .await
                .unwrap(),
        );
        let opened = open(&node, &tasks, 1, &[1], &[1]).await.err();
        assert_eq!(
            opened,
            Some(Error::Stray {
                path: stray.clone()
            })
        );
        assert_eq!(
            Error::Stray { path: stray }.to_string(),
            "spec/1-00 is in the directory of the spec in use, but it does not name a \
             pointer"
        );
    });
}

// Each member moves the pointer to the spec with the problem and keeps the last spec
// it used, then uses the next valid spec.
#[test]
fn each_member_keeps_the_last_spec_when_a_committed_spec_has_a_problem() {
    let (mut cluster, _, follower, _) = Cluster::led(1);
    let (a, admin_spec, b) = (
        create_subjects(&["plant.a"], 1),
        create_admin(),
        create_subjects(&["plant.b"], 1),
    );
    let (first, second, third) =
        (pointer(1, &a), pointer(2, &admin_spec), pointer(3, &b));
    let behind = Seen {
        behind: Some(Behind {
            pointer: second,
            cause: admin(),
        }),
        newest: Some(second),
        ..seen(first, &a)
    };
    let steps = [
        (base(), &a, first, seen(first, &a)),
        (first, &admin_spec, second, behind),
        (second, &b, third, seen(third, &b)),
    ];
    for (at, definitions, next, expected) in steps {
        let puts = IDS.map(|id| (id, definitions.clone())).into();
        cluster.board.lock().unwrap().puts = puts;
        cluster.run(seconds(1));
        cluster.apply(follower, at, definitions);
        cluster.run(seconds(5));
        let board = cluster.board();
        assert_eq!(board.applied, [(follower, next, Ok(next))]);
        for id in IDS {
            assert_eq!(board.specs[&id], expected, "node {id}, pointer {next:?}");
        }
    }
}

// The member holds the spec in use and the newest pointer, never a pointer that a newer
// one replaced.
#[test]
fn a_member_that_lacks_the_chunks_of_three_changes_uses_the_newest_once_it_gets_them() {
    let (mut cluster, leader, ..) = Cluster::led(0);
    let changes =
        ["plant.a", "plant.b", "plant.c"].map(|label| create_subjects(&[label], 1));
    let mut at = base();
    for (version, definitions) in (1..).zip(&changes) {
        let puts = [1, 2].map(|id| (id, definitions.clone())).into();
        cluster.board.lock().unwrap().puts = puts;
        cluster.run(seconds(1));
        cluster.apply(leader, at, definitions);
        cluster.run(seconds(5));
        let next = pointer(version, definitions);
        let board = cluster.board();
        assert_eq!(board.applied, [(leader, next, Ok(next))]);
        for id in [1, 2] {
            assert_eq!(board.specs[&id], seen(next, definitions), "node {id}");
        }
        let missing = spec::region::Error::Tree(tree::Error::Missing(next.root));
        let behind = Seen {
            behind: Some(Behind {
                pointer: next,
                cause: Cause::Read(missing),
            }),
            newest: Some(next),
            ..seen(base(), &BTreeMap::new())
        };
        assert_eq!(board.specs[&3], behind, "pointer {next:?}");
        at = next;
    }
    cluster.board.lock().unwrap().puts = [(3, changes[2].clone())].into();
    cluster.run(seconds(5));
    assert_eq!(cluster.board().specs[&3], seen(at, &changes[2]));
}

/// Builds the tree of `definitions`, puts each chunk but the root in the store, and
/// gives the chunks and the root.
async fn create_rootless(
    mesh: &Mesh,
    definitions: &BTreeMap<Name, Definition>,
) -> (Chunks, Digest) {
    let mut chunks = Chunks::default();
    let update = spec::region::tree(&mut chunks, definitions);
    let root = update.root;
    let others: Vec<_> = update.chunks.into_iter().filter(|d| *d != root).collect();
    put(&mesh.store, &mesh.pool, &chunks, &others)
        .await
        .unwrap();
    (chunks, root)
}

// The read of v2 waits on a get of its root. A call starts at v3, and v4 replaces v3
// before its read begins, so the call waits for the read of v4.
#[test]
fn a_call_at_a_pointer_replaced_before_its_read_began_ends_with_the_later_pointer() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let clock = node.clock();
        let first = mesh
            .apply(base(), create_subjects(&["plant.a"], 1), BTreeMap::new())
            .await
            .unwrap();
        let x = create_subjects(&["plant.x"], 1);
        let (x_chunks, x_root) = create_rootless(&mesh, &x).await;
        let d = create_subjects(&["plant.d"], 1);
        let (d_chunks, d_root) = create_rootless(&mesh, &d).await;
        let x_block = mesh.pool.copy(x_chunks.get(x_root).unwrap()).unwrap();
        let d_block = mesh.pool.copy(d_chunks.get(d_root).unwrap()).unwrap();
        let mut x_put = pin!(mesh.store.put(x_root, &x_block));
        assert!(now(x_put.as_mut()).await.is_pending());
        let mut d_put = pin!(mesh.store.put(d_root, &d_block));
        assert!(now(d_put.as_mut()).await.is_pending());
        let holders: BTreeSet<node::Key> = [key(1)].into();
        let second = mesh
            .settle_spec(
                first,
                x_root,
                BTreeSet::new(),
                holders.clone(),
                BTreeMap::new(),
            )
            .await
            .unwrap();
        clock.sleep(TICK).await;
        let third = commit(&mesh, second, &create_subjects(&["plant.c"], 1)).await;
        let mut call = pin!(mesh.spec());
        assert_eq!(now(call.as_mut()).await, Poll::Pending);
        let fourth = mesh
            .settle_spec(third, d_root, BTreeSet::new(), holders, BTreeMap::new())
            .await
            .unwrap();
        x_put.await.unwrap();
        clock.sleep(seconds(3)).await;
        assert_eq!(now(call.as_mut()).await, Poll::Pending);
        d_put.await.unwrap();
        clock.sleep(seconds(3)).await;
        assert_eq!(
            now(call.as_mut()).await,
            Poll::Ready(Ok(in_use(fourth, &d)))
        );
    });
}

// The read of v2 waits on a get of its root while v3 commits, then fails. The read of
// v3 waits on a get of its root, which never ends, so `behind` names v2.
#[test]
fn a_call_ends_when_the_read_of_its_pointer_ends_after_a_later_pointer_commits() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let clock = node.clock();
        let a = create_subjects(&["plant.a"], 1);
        let first = mesh
            .apply(base(), a.clone(), BTreeMap::new())
            .await
            .unwrap();
        let (admin_chunks, admin_root) = create_rootless(&mesh, &create_admin()).await;
        let b = create_subjects(&["plant.b"], 1);
        let (b_chunks, b_root) = create_rootless(&mesh, &b).await;
        let admin_block = mesh
            .pool
            .copy(admin_chunks.get(admin_root).unwrap())
            .unwrap();
        let b_block = mesh.pool.copy(b_chunks.get(b_root).unwrap()).unwrap();
        let mut admin_put = pin!(mesh.store.put(admin_root, &admin_block));
        assert!(now(admin_put.as_mut()).await.is_pending());
        let mut b_put = pin!(mesh.store.put(b_root, &b_block));
        assert!(now(b_put.as_mut()).await.is_pending());
        let holders: BTreeSet<node::Key> = [key(1)].into();
        let second = mesh
            .settle_spec(
                first,
                admin_root,
                BTreeSet::new(),
                holders.clone(),
                BTreeMap::new(),
            )
            .await
            .unwrap();
        clock.sleep(TICK).await;
        let mut call = pin!(mesh.spec());
        assert_eq!(now(call.as_mut()).await, Poll::Pending);
        let third = mesh
            .settle_spec(second, b_root, BTreeSet::new(), holders, BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(third.version, 3);
        admin_put.await.unwrap();
        clock.sleep(seconds(3)).await;
        let behind = Spec {
            behind: Some(Behind {
                pointer: second,
                cause: admin(),
            }),
            ..in_use(first, &a)
        };
        assert_eq!(now(call.as_mut()).await, Poll::Ready(Ok(behind)));
    });
}

// The sync of the file of v2 fails, and the process dies before the retry. The next
// open uses v2 from a file that no sync made durable, and removes the file of v1.
// After a power cut, the open after that must still use v2, the spec it used last.
#[test]
fn an_open_uses_the_spec_it_used_last_after_a_power_cut() {
    let (a, b) = (
        create_subjects(&["plant.a"], 1),
        create_subjects(&["plant.b"], 1),
    );
    let (first, second) = (pointer(1, &a), pointer(2, &b));
    let mut found = Vec::new();
    for seed in 0..8 {
        let mut sim = Sim::new(sim::Config {
            seed,
            ..sim::Config::default()
        });
        let node = sim.node(sim::node::Config::default());
        let (applied, next) = (a.clone(), b.clone());
        sim.run_on(&node, move |node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            assert_eq!(
                mesh.apply(base(), applied, BTreeMap::new()).await,
                Ok(first)
            );
            assert_eq!(mesh.spec().await.unwrap().pointer, Some(first));
            node.fail_file(Path::new(used::SPEC), Operation::SyncDir);
            assert_eq!(mesh.apply(first, next, BTreeMap::new()).await, Ok(second));
            assert_eq!(mesh.spec().await.unwrap().pointer, Some(first));
        })
        .unwrap();
        sim.crash(&node, Crash::Process);
        let used = b.clone();
        sim.run_on(&node, move |node, tasks| async move {
            let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
            assert_eq!(mesh.spec().await, Ok(in_use(second, &used)), "seed {seed}");
        })
        .unwrap();
        sim.crash(&node, Crash::Power);
        let pointer = sim
            .run_on(&node, move |node, tasks| async move {
                let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
                mesh.spec().await.unwrap().pointer
            })
            .unwrap();
        if pointer != Some(second) {
            found.push((seed, pointer.map(|pointer| pointer.version)));
        }
    }
    assert_eq!(found, [], "(seed, version at the open) that is not v2");
}
