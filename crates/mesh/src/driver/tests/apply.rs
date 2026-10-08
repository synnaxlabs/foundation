//! Tests of `Mesh::apply` on one node, and of its spec change on a cluster of three
//! voters, where each voter holds the chunks.

use std::sync::atomic::{AtomicUsize, Ordering};

use spec::channel::Channel;
use spec::data_type::DataType;
use spec::definition::Kind;
use spec::region::Problem;
use spec::subject::Subject;

use super::send::stop;
use super::*;
use crate::change::HOMES_MAX;

impl Cluster {
    /// Node `node` proposes the spec change of `definitions` on `base` at its next
    /// tick, with each node as a holder.
    fn apply(&self, node: u8, base: Pointer, definitions: &BTreeMap<Name, Definition>) {
        self.apply_held(node, base, definitions, IDS.into(), BTreeMap::new());
    }

    /// As [`Cluster::apply`], with `holders` and `homes`.
    fn apply_held(
        &self,
        node: u8,
        base: Pointer,
        definitions: &BTreeMap<Name, Definition>,
        holders: BTreeSet<u8>,
        homes: BTreeMap<channel::Key, node::Key>,
    ) {
        let update = spec::region::tree(&mut Chunks::default(), definitions);
        let spec = Proposal {
            base,
            root: update.root,
            chunks: update.chunks.into_iter().collect(),
            holders: holders.into_iter().map(key).collect(),
            homes,
        };
        self.board.lock().unwrap().applies.insert(node, spec);
    }

    /// Node `node` proposes `voters` at its next tick.
    fn configure(&self, node: u8, voters: BTreeSet<u8>) {
        let mut board = self.board.lock().unwrap();
        board.configurations.insert(node, voters);
    }
}

/// A subject of `keys` keys at the tree key of each of `labels`.
pub(super) fn create_subjects(
    labels: &[&str],
    keys: u16,
) -> BTreeMap<Name, Definition> {
    let key = |at: u16| {
        let mut bytes = [1; 32];
        bytes[..2].copy_from_slice(&at.to_le_bytes());
        PublicKey::new(bytes).unwrap()
    };
    let subject = Subject::new((0..keys).map(key).collect()).unwrap();
    labels
        .iter()
        .map(|label| {
            (
                Kind::Subject.key(label).unwrap(),
                Definition::Subject(subject.clone()),
            )
        })
        .collect()
}

/// The pointer after `version` changes, at the tree of `definitions`.
fn pointer(version: u64, definitions: &BTreeMap<Name, Definition>) -> Pointer {
    let update = spec::region::tree(&mut Chunks::default(), definitions);
    Pointer {
        version,
        root: update.root,
    }
}

/// `count` subjects of 64 keys, whose tree has 1024 chunks at 1950 and 1025 at
/// 1951.
pub(super) fn create_large(count: usize) -> BTreeMap<Name, Definition> {
    let labels: Vec<String> = (0..count).map(|at| format!("plant.s{at}")).collect();
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    create_subjects(&labels, 64)
}

/// The mesh of node 1 in a region of `voters`, and its chunk store.
async fn open_kept(
    node: &sim::node::Node,
    tasks: &Tasks,
    voters: &[u8],
) -> (Mesh, Rc<blob::Store>) {
    let config = config(node, tasks, 1, voters, voters).await;
    let store = Rc::clone(&config.store);
    (Mesh::start(config).await.unwrap(), store)
}

/// The pointer of a region with no founding definitions.
fn base() -> Pointer {
    Pointer {
        version: 0,
        root: tree::empty(),
    }
}

#[test]
fn apply_gives_each_problem_of_the_spec_and_proposes_nothing() {
    let entries = solo_stored(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home(1)).await;
        let mut definitions = create_subjects(&["other.a"], 1);
        let misplaced = name("plant.b");
        let subject = definitions.values().next().unwrap().clone();
        definitions.insert(misplaced.clone(), subject);
        let applied = now(pin!(mesh.apply(base(), definitions, BTreeMap::new()))).await;
        let problems = vec![
            Problem::Ungoverned {
                name: name("other.a.@subject"),
                region: "plant".parse().unwrap(),
            },
            Problem::Misplaced {
                name: misplaced,
                kind: Kind::Subject,
            },
        ];
        let error = Error::Problems(problems);
        assert_eq!(applied, Poll::Ready(Err(error.clone())));
        assert_eq!(
            error.to_string(),
            "the spec has problems: the region `plant` does not govern \
             `other.a.@subject`; `plant.b` is not a tree key of a definition of kind \
             `subject`"
        );
        assert_eq!(mesh.pointer(), base());
    });
    let spec = |entry: &Entry| match &entry.data {
        Data::Bytes(bytes) => matches!(Change::decode(bytes), Ok(Change::Spec { .. })),
        Data::Empty | Data::Voters(_) => false,
    };
    assert_eq!(entries.iter().filter(|entry| spec(entry)).count(), 0);
    assert_eq!(entries.len(), 2);
}

#[test]
fn apply_on_a_node_that_is_not_a_voter_gives_problems_and_large_before_no_vote() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[2]).await.unwrap();
        let valid = create_subjects(&["plant.a"], 1);
        let applied = now(pin!(mesh.apply(base(), valid, BTreeMap::new()))).await;
        assert_eq!(applied, Poll::Ready(Err(Error::NoVote)));
        let ungoverned = create_subjects(&["other"], 1);
        let applied = now(pin!(mesh.apply(base(), ungoverned, BTreeMap::new()))).await;
        let problem = Problem::Ungoverned {
            name: name("other.@subject"),
            region: "plant".parse().unwrap(),
        };
        assert_eq!(applied, Poll::Ready(Err(Error::Problems(vec![problem]))));
        let applied = now(pin!(mesh.apply(
            base(),
            create_large(1951),
            BTreeMap::new()
        )))
        .await;
        assert!(matches!(applied, Poll::Ready(Err(Error::Large { .. }))));
    });
}

#[test]
fn apply_on_a_stopped_group_gives_problems_and_large_before_the_stop() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
        let stopped = stop(&node, &mesh);
        node.clock().sleep(Span::MILLISECOND).await;
        let ungoverned = create_subjects(&["other"], 1);
        let applied = now(pin!(mesh.apply(base(), ungoverned, BTreeMap::new()))).await;
        assert!(matches!(applied, Poll::Ready(Err(Error::Problems(_)))));
        let applied = now(pin!(mesh.apply(
            base(),
            create_large(1951),
            BTreeMap::new()
        )))
        .await;
        assert!(matches!(applied, Poll::Ready(Err(Error::Large { .. }))));
        let valid = create_subjects(&["plant.a"], 1);
        let applied = now(pin!(mesh.apply(base(), valid, BTreeMap::new()))).await;
        assert_eq!(applied, Poll::Ready(Err(Error::Stopped(stopped))));
    });
}

#[test]
fn apply_on_a_stopped_group_of_a_node_that_is_not_a_voter_gives_stopped() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[2]).await.unwrap();
        let stopped = stop(&node, &mesh);
        node.clock().sleep(Span::MILLISECOND).await;
        let valid = create_subjects(&["plant.a"], 1);
        let applied = now(pin!(mesh.apply(base(), valid, BTreeMap::new()))).await;
        assert_eq!(applied, Poll::Ready(Err(Error::Stopped(stopped))));
    });
}

// A floor only takes memory, which no call shows, so this test reads the group.
#[test]
fn an_apply_opens_no_floor_while_it_puts_its_chunks() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let before = mesh.group.borrow().applied.clone();
        let definitions = create_subjects(&["plant.a"], 1);
        let moved = pointer(1, &definitions);
        let mut call = pin!(mesh.apply(base(), definitions, BTreeMap::new()));
        assert_eq!(now(call.as_mut()).await, Poll::Pending);
        assert_eq!(mesh.group.borrow().applied, before);
        assert_eq!(call.await, Ok(moved));
    });
}

#[test]
fn apply_refuses_a_tree_of_more_chunks_than_one_change_lists() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let applied = now(pin!(mesh.apply(
            base(),
            create_large(1951),
            BTreeMap::new()
        )))
        .await;
        let error = Error::Large {
            chunks: 1025,
            most: CHUNKS_MAX,
        };
        assert_eq!(applied, Poll::Ready(Err(error.clone())));
        assert_eq!(
            error.to_string(),
            "the change lists 1025 chunks, more than the 1024 that one change can list"
        );
    });
}

#[test]
fn a_lone_voter_applies_a_tree_of_the_most_chunks() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let definitions = create_large(1950);
        let update = spec::region::tree(&mut Chunks::default(), &definitions);
        assert_eq!(update.chunks.len(), CHUNKS_MAX);
        let moved = pointer(1, &definitions);
        assert_eq!(
            mesh.apply(base(), definitions, BTreeMap::new()).await,
            Ok(moved)
        );
        assert_eq!(mesh.pointer(), moved);
    });
}

// Each order: the first call applies, the second on the same base is stale, and the
// second on the new pointer applies.
#[test]
fn an_apply_on_a_stale_base_gives_the_pointer_through_a_follower_and_the_leader() {
    for run in 0..2 {
        let (mut cluster, leader, follower, _) = Cluster::led(run);
        let (first, second) = if run == 0 {
            (leader, follower)
        } else {
            (follower, leader)
        };
        let (a, b) = (
            create_subjects(&["plant.a"], 1),
            create_subjects(&["plant.b"], 2),
        );
        let (moved, next) = (pointer(1, &a), pointer(2, &b));
        cluster.apply(first, base(), &a);
        cluster.run(seconds(5));
        cluster.apply(second, base(), &b);
        cluster.run(seconds(5));
        cluster.apply(second, moved, &b);
        cluster.run(seconds(5));
        let stale = Error::Stale {
            base: base(),
            pointer: moved,
        };
        assert_eq!(
            stale.to_string(),
            format!(
                "the spec changed: the pointer is version 1, root {}, not the base \
                 version 0, root {}",
                moved.root,
                base().root
            ),
            "run {run}"
        );
        let applied = [
            (first, moved, Ok(moved)),
            (second, moved, Err(stale)),
            (second, next, Ok(next)),
        ];
        assert_eq!(cluster.board().applied, applied, "run {run}");
    }
}

#[test]
fn of_two_applies_from_one_base_one_gives_the_pointer_and_the_other_stale() {
    for run in 0..4 {
        let (mut cluster, leader, follower, _) = Cluster::led(run);
        let (a, b) = (
            create_subjects(&["plant.a"], 1),
            create_subjects(&["plant.b"], 1),
        );
        cluster.apply(leader, base(), &a);
        cluster.apply(follower, base(), &b);
        cluster.run(seconds(5));
        cluster.script(|_| home(9));
        cluster.run(seconds(5));
        let board = cluster.board();
        let [(_, _, Ok(moved)), (loser, _, Err(stale))] = board.applied.as_slice()
        else {
            panic!("run {run}: the calls gave {:?}", board.applied);
        };
        assert!(
            [pointer(1, &a), pointer(1, &b)].contains(moved),
            "run {run}"
        );
        let stale_error = Error::Stale {
            base: base(),
            pointer: *moved,
        };
        assert_eq!(*stale, stale_error, "run {run}");
        assert!([leader, follower].contains(loser), "run {run}");
        let pointers: BTreeMap<_, _> = IDS.map(|id| (id, *moved)).into();
        assert_eq!(board.pointers, pointers, "run {run}");
    }
}

// The leader puts the entry of the call in its log and loses its lead before a
// quorum has it. The next leader replaces the entry, so the call proposes again.
#[test]
fn a_leader_that_loses_its_lead_applies_through_the_next_leader() {
    let (mut cluster, old, ..) = Cluster::led(2);
    cluster.link_each(old, 1.0);
    let definitions = create_subjects(&["plant.a"], 1);
    let moved = pointer(1, &definitions);
    cluster.apply(old, base(), &definitions);
    cluster.run(seconds(5));
    assert_eq!(cluster.board().applied, []);
    cluster.link_each(old, 0.0);
    cluster.run(seconds(5));
    assert_eq!(cluster.board().applied, [(old, moved, Ok(moved))]);
}

// The leader commits the entry of the follower, and the cut drops the answer, so the
// call proposes again and its second entry is refused as stale.
#[test]
fn a_call_whose_answer_is_cut_off_gives_the_pointer_of_its_own_change() {
    for run in 0..4 {
        let (mut cluster, _, follower, _) = Cluster::led(run);
        let config = link::Config {
            delay: Span::from_nanos(6 * TICK.nanos()),
            ..link::Config::default()
        };
        for other in IDS.into_iter().filter(|&id| id != follower) {
            let (a, b) = (cluster.node(follower).clone(), cluster.node(other).clone());
            cluster.sim.link(&a, &b, config);
            cluster.sim.link(&b, &a, config);
        }
        cluster.run(seconds(5));
        let definitions = create_subjects(&["plant.a"], 1);
        let moved = pointer(1, &definitions);
        cluster.apply(follower, base(), &definitions);
        cluster.run(Span::from_nanos(3 * TICK.nanos()));
        cluster.link_each(follower, 1.0);
        cluster.run(seconds(5));
        assert_eq!(cluster.board().applied, [], "run {run}");
        cluster.link_each(follower, 0.0);
        cluster.run(seconds(10));
        let applied = [(follower, moved, Ok(moved))];
        assert_eq!(cluster.board().applied, applied, "run {run}");
    }
}

// The leader and a follower make the same change from one base. The second entry
// finds the pointer that its call makes, so the pointer moves once.
#[test]
fn two_calls_of_one_change_from_one_base_give_one_pointer() {
    let (mut cluster, leader, follower, _) = Cluster::led(0);
    let definitions = create_subjects(&["plant.a"], 1);
    let moved = pointer(1, &definitions);
    cluster.apply(leader, base(), &definitions);
    cluster.apply(follower, base(), &definitions);
    cluster.run(seconds(5));
    // A home after the changes gives the pointer of each node to the board.
    cluster.script(|_| home(9));
    cluster.run(seconds(5));
    let board = cluster.board();
    let mut applied = board.applied.clone();
    applied.sort_by_key(|&(id, ..)| id);
    let mut expected = [(leader, moved, Ok(moved)), (follower, moved, Ok(moved))];
    expected.sort_by_key(|&(id, ..)| id);
    assert_eq!(applied, expected);
    for id in IDS {
        assert_eq!(board.pointers[&id], moved, "node {id}");
    }
}

#[test]
fn an_apply_that_finds_a_later_pointer_of_its_own_root_is_stale() {
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
        let second = mesh.apply(first, b, BTreeMap::new()).await.unwrap();
        let third = mesh
            .apply(second, a.clone(), BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(third, pointer(3, &a));
        let stale = Error::Stale {
            base: base(),
            pointer: third,
        };
        assert_eq!(mesh.apply(base(), a, BTreeMap::new()).await, Err(stale));
    });
}

// A base past the pointer is stale, also at the last version and the pointer's root.
#[test]
fn an_apply_on_a_base_at_the_last_version_gives_stale() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let last = Pointer {
            version: u64::MAX,
            root: tree::empty(),
        };
        let stale = Error::Stale {
            base: last,
            pointer: base(),
        };
        for definitions in [create_subjects(&["plant.a"], 1), BTreeMap::new()] {
            assert_eq!(
                mesh.apply(last, definitions, BTreeMap::new()).await,
                Err(stale.clone())
            );
        }
        assert_eq!(mesh.pointer(), base());
    });
}

#[test]
fn a_lone_voter_puts_each_chunk_of_its_change_in_its_store_and_lists_it() {
    let definitions = create_large(200);
    let mut tree = Chunks::default();
    let update = spec::region::tree(&mut tree, &definitions);
    let (moved, applied) = (pointer(1, &definitions), definitions.clone());
    let digests = update.chunks.clone();
    let entries = solo_stored(move |node, tasks| async move {
        let (mesh, store) = open_kept(&node, &tasks, &[1]).await;
        assert_eq!(
            mesh.apply(base(), applied, BTreeMap::new()).await,
            Ok(moved)
        );
        for digest in digests {
            let chunk = store.get(digest).await.unwrap().unwrap();
            assert_eq!(Some(&*chunk), tree.get(digest), "{digest}");
        }
    });
    let [change] = specs(&entries).try_into().unwrap();
    let Change::Spec {
        chunks, holders, ..
    } = change
    else {
        unreachable!()
    };
    assert_eq!(chunks, update.chunks.into_iter().collect());
    assert_eq!(holders, [key(1)].into());
}

#[test]
fn a_change_lists_only_the_chunks_that_the_tree_of_its_base_lacks() {
    let first = create_large(200);
    let mut second = first.clone();
    second.extend(create_subjects(&["plant.added"], 1));
    let mut tree = Chunks::default();
    let old = spec::region::tree(&mut tree, &first).root;
    let new = spec::region::tree(&mut tree, &second);
    let lacked = tree::diff(&tree, old, new.root).unwrap().chunks;
    assert!(
        lacked.len() < new.chunks.len() / 10,
        "{} chunks",
        lacked.len()
    );
    let entries = solo_stored(move |node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let moved = mesh.apply(base(), first, BTreeMap::new()).await.unwrap();
        mesh.apply(moved, second, BTreeMap::new()).await.unwrap();
    });
    let [_, change] = specs(&entries).try_into().unwrap();
    let Change::Spec { chunks, .. } = change else {
        unreachable!()
    };
    assert_eq!(chunks, lacked.into_iter().collect());
}

/// A store that lost a chunk which the base shares with the new tree gets it again,
/// though the change lists only the chunks that the base lacks: `diff` never reads a
/// shared chunk.
#[test]
fn a_change_puts_a_lost_chunk_that_its_base_shares_with_the_new_tree() {
    let first = create_large(200);
    let mut second = first.clone();
    second.extend(create_subjects(&["plant.added"], 1));
    let mut tree = Chunks::default();
    let old = spec::region::tree(&mut tree, &first).root;
    let new = spec::region::tree(&mut tree, &second);
    let lacked: BTreeSet<Digest> = tree::diff(&tree, old, new.root)
        .unwrap()
        .chunks
        .into_iter()
        .collect();
    let shared = *new.chunks.iter().find(|at| !lacked.contains(at)).unwrap();
    let listed = lacked.clone();
    let entries = solo_stored(move |node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let moved = mesh.apply(base(), first, BTreeMap::new()).await.unwrap();
        drop(mesh);
        node.clock().sleep(Span::MILLISECOND).await;
        let path = Path::new(BLOB).join(shared.to_string());
        node.files().remove(&path).await.unwrap();
        let (mesh, store) = open_kept(&node, &tasks, &[1]).await;
        assert!(store.get(shared).await.unwrap().is_none());
        mesh.apply(moved, second, BTreeMap::new()).await.unwrap();
        let held = store.get(shared).await.unwrap();
        assert_eq!(held.as_deref(), tree.get(shared));
    });
    let [_, Change::Spec { chunks, .. }] =
        <[Change; 2]>::try_from(specs(&entries)).unwrap()
    else {
        unreachable!()
    };
    assert_eq!(chunks, listed);
}

/// A change to the tree of its base lists no chunk, and puts again the root that the
/// store lost.
#[test]
fn a_change_to_the_tree_of_its_base_puts_a_lost_root() {
    let definitions = create_subjects(&["plant.a", "plant.b"], 1);
    let mut tree = Chunks::default();
    let root = spec::region::tree(&mut tree, &definitions).root;
    let entries = solo_stored(move |node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let moved = mesh
            .apply(base(), definitions.clone(), BTreeMap::new())
            .await
            .unwrap();
        drop(mesh);
        node.clock().sleep(Span::MILLISECOND).await;
        let path = Path::new(BLOB).join(root.to_string());
        node.files().remove(&path).await.unwrap();
        let (mesh, store) = open_kept(&node, &tasks, &[1]).await;
        assert!(store.get(root).await.unwrap().is_none());
        mesh.apply(moved, definitions, BTreeMap::new())
            .await
            .unwrap();
        let held = store.get(root).await.unwrap();
        assert_eq!(held.as_deref(), tree.get(root));
    });
    let [_, Change::Spec { chunks, .. }] =
        <[Change; 2]>::try_from(specs(&entries)).unwrap()
    else {
        unreachable!()
    };
    assert_eq!(chunks, BTreeSet::new());
}

// The store lacks the root of the first base, and holds a chunk that is not a node of
// a tree as the root of the second. Each change is stale, and lists the whole tree.
#[test]
fn a_change_on_a_base_whose_tree_the_store_cannot_give_lists_each_chunk() {
    let definitions = create_subjects(&["plant.a", "plant.b"], 1);
    let update = spec::region::tree(&mut Chunks::default(), &definitions);
    let corrupt = Digest::of(b"x");
    let bases = [Digest([7; 32]), corrupt].map(|root| Pointer { version: 0, root });
    let entries = solo_stored(move |node, tasks| async move {
        let (mesh, store) = open_kept(&node, &tasks, &[1]).await;
        let chunk = create_pool().copy(b"x").unwrap();
        store.put(corrupt, &chunk).await.unwrap();
        for at in bases {
            let stale = Error::Stale {
                base: at,
                pointer: base(),
            };
            assert_eq!(
                mesh.apply(at, definitions.clone(), BTreeMap::new()).await,
                Err(stale)
            );
        }
    });
    let all: BTreeSet<Digest> = update.chunks.into_iter().collect();
    for change in specs(&entries) {
        let Change::Spec { chunks, .. } = change else {
            unreachable!()
        };
        assert_eq!(chunks, all);
    }
    assert_eq!(specs(&entries).len(), 2);
}

#[test]
fn apply_in_a_region_of_three_voters_gives_quorum_and_puts_and_proposes_nothing() {
    let entries = solo_stored(|node, tasks| async move {
        let (mesh, store) = open_kept(&node, &tasks, &IDS).await;
        let definitions = create_subjects(&["plant.a"], 1);
        let root = spec::region::tree(&mut Chunks::default(), &definitions).root;
        let quorum = Error::Quorum { held: 1, voters: 3 };
        assert_eq!(
            mesh.apply(base(), definitions, BTreeMap::new()).await,
            Err(quorum.clone())
        );
        assert!(store.get(root).await.unwrap().is_none());
        assert_eq!(
            quorum.to_string(),
            "1 of 3 voters hold the chunks of the spec change, not a majority"
        );
        assert_eq!(mesh.pointer(), base());
    });
    assert_eq!(specs(&entries), []);
}

// Voter 1 makes 2 a voter, then proposes a change that only it holds. Each voter
// refuses it at the apply, because the holders are 1 of the 2 voters. The home after it
// shows that each applied past it.
#[test]
fn each_member_refuses_a_change_whose_holders_lack_a_majority_of_its_voters() {
    let mut cluster = Cluster::new(0);
    cluster.board.lock().unwrap().learners = [2, 3].into();
    cluster.start();
    cluster.run(seconds(5));
    cluster.configure(1, [1, 2].into());
    cluster.run(seconds(5));
    let definitions = create_subjects(&["plant.a"], 1);
    cluster.apply_held(1, base(), &definitions, [1].into(), BTreeMap::new());
    cluster.run(seconds(5));
    cluster.script(|_| home(9));
    cluster.run(seconds(5));
    let board = cluster.board();
    let quorum = Error::Quorum { held: 1, voters: 2 };
    assert_eq!(board.applied, [(1, base(), Err(quorum))]);
    // Node 3 is in no configuration, so it gets no entry.
    for id in [1, 2] {
        assert_eq!(board.homes[&id].last(), Some(&Some(key(9))), "node {id}");
        assert_eq!(board.pointers[&id], base(), "node {id}");
    }
}

#[test]
fn a_failed_put_gives_the_error_of_the_store_and_proposes_nothing() {
    let entries = solo_stored(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        node.fail_file(Path::new(BLOB), Operation::SyncDir);
        let definitions = create_subjects(&["plant.a"], 1);
        let cause = files::Error::Io {
            path: BLOB.into(),
            operation: Operation::SyncDir,
            code: 5,
        };
        let failed = Error::Blob(blob::Error::Files(cause));
        assert_eq!(
            mesh.apply(base(), definitions, BTreeMap::new()).await,
            Err(failed.clone())
        );
        assert_eq!(
            failed.to_string(),
            "the chunk store failed: sync_dir of blob failed with OS error 5"
        );
        assert_eq!(mesh.pointer(), base());
    });
    assert_eq!(specs(&entries), []);
}

#[test]
fn a_change_counts_only_the_chunks_it_lists_against_the_most() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let moved = mesh
            .apply(base(), create_large(1950), BTreeMap::new())
            .await
            .unwrap();
        let larger = create_large(1951);
        let expected = pointer(2, &larger);
        assert_eq!(
            mesh.apply(moved, larger, BTreeMap::new()).await,
            Ok(expected)
        );
    });
}

#[test]
fn a_pool_with_no_block_for_a_chunk_gives_pool_and_proposes_nothing() {
    let definitions = create_subjects(&["plant.a"], 1);
    let mut tree = Chunks::default();
    let update = spec::region::tree(&mut tree, &definitions);
    let [root] = update.chunks[..] else {
        unreachable!("{} chunks", update.chunks.len())
    };
    let requested = tree.get(root).unwrap().len();
    let entries = solo_stored(move |node, tasks| async move {
        let pool = small_pool();
        let config = Config {
            pool: Rc::clone(&pool),
            ..config(&node, &tasks, 1, &[1], &[1]).await
        };
        let mesh = Mesh::start(config).await.unwrap();
        let _held = fill(&pool);
        let applied = mesh.apply(base(), definitions, BTreeMap::new()).await;
        assert_eq!(applied, Err(exhausted(requested)));
        assert_eq!(mesh.pointer(), base());
    });
    assert_eq!(specs(&entries), []);
}

#[test]
fn a_failed_read_of_the_base_tree_gives_the_error_of_the_store() {
    let first = create_subjects(&["plant.a"], 1);
    let moved = pointer(1, &first);
    let entries = solo_stored(move |node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        assert_eq!(mesh.apply(base(), first, BTreeMap::new()).await, Ok(moved));
        let path = Path::new(BLOB).join(moved.root.to_string());
        node.fail_file(&path, Operation::Open);
        let second = create_subjects(&["plant.b"], 1);
        let cause = files::Error::Io {
            path,
            operation: Operation::Open,
            code: 5,
        };
        let failed = Error::Blob(blob::Error::Files(cause));
        assert_eq!(
            mesh.apply(moved, second, BTreeMap::new()).await,
            Err(failed)
        );
        assert_eq!(mesh.pointer(), moved);
    });
    assert_eq!(specs(&entries).len(), 1);
}

/// The spec changes of `entries`, in order.
fn specs(entries: &[Entry]) -> Vec<Change> {
    let change = |entry: &Entry| match &entry.data {
        Data::Bytes(bytes) => Change::decode(bytes).ok(),
        Data::Empty | Data::Voters(_) => None,
    };
    let spec = |change: &Change| matches!(change, Change::Spec { .. });
    entries.iter().filter_map(change).filter(spec).collect()
}

fn name(text: &str) -> Name {
    text.parse().unwrap()
}

/// An index channel of key `7 + at` at the tree key `plant.i<at>`, for each `at` below
/// `count`.
fn create_indexes(count: u128) -> BTreeMap<Name, Definition> {
    let index = |at: u128| {
        let channel = Channel {
            key: channel::Key::from_u128(at.checked_add(7).unwrap()),
            kind: spec::channel::Kind::Index {
                error: None,
                control: None,
            },
        };
        let key = Kind::Channel.key(&format!("plant.i{at}")).unwrap();
        (key, Definition::Channel(channel))
    };
    (0..count).map(index).collect()
}

/// The home `node` for the index at `plant.i<at>`, for each `at` below `count`.
fn create_homes(count: u128, node: &str) -> BTreeMap<Name, Name> {
    let home = |at: u128| {
        (
            Kind::Channel.key(&format!("plant.i{at}")).unwrap(),
            name(node),
        )
    };
    (0..count).map(home).collect()
}

/// The mesh of node `id` in a region of members 1 and 2 and voter 1. Its founding spec
/// holds the index `plant.i0` with its home at node 1, and `plant.i1` with no home.
async fn open_founded(node: &sim::node::Node, tasks: &Tasks, id: u8) -> Mesh {
    let mut config = config(node, tasks, id, &[1, 2], &[1]).await;
    config.founding.definitions = create_indexes(2);
    config.founding.homes = BTreeMap::from([(INDEX, key(1))]);
    Mesh::start(config).await.unwrap()
}

#[test]
fn each_member_holds_the_founding_homes_at_each_open() {
    for id in [1, 2] {
        solo(move |node, tasks| async move {
            let mesh = open_founded(&node, &tasks, id).await;
            assert_eq!(mesh.watch(INDEX).next().await, Ok(Some(key(1))));
            assert_eq!(mesh.watch(SECOND).next().await, Ok(None));
            drop(mesh);
            node.clock().sleep(Span::MILLISECOND).await;
            let mesh = open_founded(&node, &tasks, id).await;
            assert_eq!(mesh.watch(INDEX).next().await, Ok(Some(key(1))));
            assert_eq!(mesh.watch(SECOND).next().await, Ok(None));
        });
    }
}

#[test]
fn a_spec_change_keeps_a_founding_home_and_gives_a_founding_index_with_none_its_home() {
    solo(|node, tasks| async move {
        let mesh = open_founded(&node, &tasks, 1).await;
        let founded = pointer(0, &create_indexes(2));
        let definitions = create_indexes(3);
        let moved = pointer(1, &definitions);
        let homes = create_homes(3, "plant.node2");
        assert_eq!(mesh.apply(founded, definitions, homes).await, Ok(moved));
        assert_eq!(mesh.watch(INDEX).next().await, Ok(Some(key(1))));
        assert_eq!(mesh.watch(SECOND).next().await, Ok(Some(key(2))));
    });
}

#[test]
fn apply_gives_each_listed_index_the_member_of_its_name_as_its_home() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        let definitions = create_indexes(2);
        let homes = [(Kind::Channel.key("plant.i1").unwrap(), name("plant.node2"))];
        let moved = pointer(1, &definitions);
        let applied = mesh.apply(base(), definitions, homes.into()).await;
        assert_eq!(applied, Ok(moved));
        assert_eq!(mesh.watch(INDEX).next().await, Ok(None));
        assert_eq!(mesh.watch(SECOND).next().await, Ok(Some(key(2))));
    });
}

// A listed home is a proposal: the equal change of the first call gives each index a
// home, so the second call gives `Ok`, and the homes of the first hold.
#[test]
fn an_equal_change_with_other_homes_gives_ok_and_the_first_homes_hold() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        let definitions = create_indexes(1);
        let moved = pointer(1, &definitions);
        let first = create_homes(1, "plant.node1");
        let applied = mesh.apply(base(), definitions.clone(), first).await;
        assert_eq!(applied, Ok(moved));
        let other = create_homes(1, "plant.node2");
        assert_eq!(mesh.apply(base(), definitions, other).await, Ok(moved));
        assert_eq!(mesh.watch(INDEX).next().await, Ok(Some(key(1))));
    });
}

// The first call homes only `INDEX`, so `SECOND`, which the second lists, has none.
#[test]
fn an_equal_change_with_fewer_homes_gives_stale() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        let definitions = create_indexes(2);
        let moved = pointer(1, &definitions);
        let first = create_homes(1, "plant.node1");
        let applied = mesh.apply(base(), definitions.clone(), first).await;
        assert_eq!(applied, Ok(moved));
        let both = create_homes(2, "plant.node1");
        let stale = Error::Stale {
            base: base(),
            pointer: moved,
        };
        assert_eq!(mesh.apply(base(), definitions, both).await, Err(stale));
        assert_eq!(mesh.watch(INDEX).next().await, Ok(Some(key(1))));
        assert_eq!(mesh.watch(SECOND).next().await, Ok(None));
    });
}

// As above, but a home of `SECOND` applies in the batch of the second call's entry,
// after it. The call reads the homes when it settles, so it gives `Ok`.
#[test]
fn an_equal_change_gives_ok_when_a_later_home_applies_in_its_batch() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        let definitions = create_indexes(2);
        let moved = pointer(1, &definitions);
        let first = create_homes(1, "plant.node1");
        let applied = mesh.apply(base(), definitions.clone(), first).await;
        assert_eq!(applied, Ok(moved));
        let both = create_homes(2, "plant.node1");
        // No public call shows a queued proposal, so this reads `proposals`, whose
        // order is the order of the log.
        assert!(mesh.group.borrow().proposals.is_empty());
        let mut call = pin!(mesh.apply(base(), definitions, both));
        while mesh.group.borrow().proposals.is_empty() {
            assert!(now(call.as_mut()).await.is_pending());
            if mesh.group.borrow().proposals.is_empty() {
                node.clock().sleep(Span::MILLISECOND).await;
            }
        }
        let mut set = pin!(mesh.set_home(SECOND, key(2)));
        assert!(now(set.as_mut()).await.is_pending());
        assert_eq!(mesh.group.borrow().proposals.len(), 2);
        assert_eq!(call.await, Ok(moved));
        assert_eq!(set.await, Ok(()));
        assert_eq!(mesh.watch(SECOND).next().await, Ok(Some(key(2))));
    });
}

// `INDEX` keeps its home from the first call, so both equal calls give `Ok`.
#[test]
fn two_equal_calls_with_a_kept_home_give_ok() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        let a = create_indexes(1);
        let moved = pointer(1, &a);
        let first = create_homes(1, "plant.node1");
        assert_eq!(mesh.apply(base(), a, first).await, Ok(moved));
        let b = create_indexes(2);
        let next = pointer(2, &b);
        let both = create_homes(2, "plant.node2");
        let one = mesh.apply(moved, b.clone(), both.clone()).await;
        let two = mesh.apply(moved, b, both).await;
        assert_eq!((one, two), (Ok(next), Ok(next)));
        assert_eq!(mesh.watch(INDEX).next().await, Ok(Some(key(1))));
        assert_eq!(mesh.watch(SECOND).next().await, Ok(Some(key(2))));
    });
}

#[test]
fn apply_gives_ok_when_a_listed_index_keeps_the_home_it_had() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home(1)).await;
        let definitions = create_indexes(1);
        let moved = pointer(1, &definitions);
        let homes = create_homes(1, "plant.node2");
        assert_eq!(mesh.apply(base(), definitions, homes).await, Ok(moved));
        assert_eq!(mesh.watch(INDEX).next().await, Ok(Some(key(1))));
    });
}

// `INDEX` has the home node 1 before the call, which lists node 2. A cut drops the
// answer to the follower, which then finds the pointer of its own entry.
#[test]
fn a_lost_answer_gives_the_result_of_a_call_with_a_home_kept() {
    for cut in [false, true] {
        let (mut cluster, _, follower, _) = Cluster::led(0);
        let config = link::Config {
            delay: Span::from_nanos(6 * TICK.nanos()),
            ..link::Config::default()
        };
        for other in IDS.into_iter().filter(|&id| id != follower) {
            let (a, b) = (cluster.node(follower).clone(), cluster.node(other).clone());
            cluster.sim.link(&a, &b, config);
            cluster.sim.link(&b, &a, config);
        }
        cluster.script(|_| home(1));
        cluster.run(seconds(5));
        let definitions = create_indexes(1);
        let moved = pointer(1, &definitions);
        let homes = BTreeMap::from([(INDEX, key(2))]);
        cluster.apply_held(follower, base(), &definitions, IDS.into(), homes);
        cluster.run(Span::from_nanos(3 * TICK.nanos()));
        if cut {
            cluster.link_each(follower, 1.0);
            cluster.run(seconds(5));
            cluster.link_each(follower, 0.0);
        }
        cluster.run(seconds(10));
        let applied = [(follower, moved, Ok(moved))];
        assert_eq!(cluster.board().applied, applied, "cut {cut}");
    }
}

/// A waker that counts its wakes.
struct Counted(AtomicUsize);

impl Wake for Counted {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

// Each wake of a watch takes its waker, so a pending watch counts one wake at most.
#[test]
fn a_spec_change_wakes_the_watches_only_when_it_gives_a_home() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home(1)).await;
        let counted = Arc::new(Counted(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counted));
        let mut cx = Context::from_waker(&waker);
        let mut watch = mesh.watch(INDEX);
        assert_eq!(watch.next().await, Ok(Some(key(1))));
        let mut next = Box::pin(watch.next());
        assert!(next.as_mut().poll(&mut cx).is_pending());
        let a = create_indexes(1);
        let moved = mesh.apply(base(), a, BTreeMap::new()).await.unwrap();
        assert_eq!(counted.0.load(Ordering::Relaxed), 0);
        let homes = [(Kind::Channel.key("plant.i1").unwrap(), name("plant.node2"))];
        let b = create_indexes(2);
        mesh.apply(moved, b, homes.into()).await.unwrap();
        assert_eq!(counted.0.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn apply_refuses_a_home_of_a_name_that_is_not_an_index_and_proposes_nothing() {
    let entries = solo_stored(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home(1)).await;
        let mut definitions = create_indexes(1);
        definitions.extend(create_subjects(&["plant.a"], 1));
        let quality = spec::channel::Data::new(
            channel::Key::from_u128(7),
            None,
            DataType::Quality,
            None,
        );
        let data = Channel {
            key: channel::Key::from_u128(1),
            kind: spec::channel::Kind::Data(quality.unwrap()),
        };
        let data_name = Kind::Channel.key("plant.d").unwrap();
        definitions.insert(data_name.clone(), Definition::Channel(data));
        let subject = name("plant.a.@subject");
        let absent = Kind::Channel.key("plant.i1").unwrap();
        for index in [subject, absent, data_name] {
            let homes = [(index.clone(), name("plant.node1"))].into();
            let applied = mesh.apply(base(), definitions.clone(), homes).await;
            assert_eq!(applied, Err(Error::NotIndex(index.clone())));
        }
        assert_eq!(
            Error::NotIndex(name("plant.a.@subject")).to_string(),
            "a home names plant.a.@subject, which the spec does not hold as an index \
             channel"
        );
    });
    assert_eq!(specs(&entries), []);
}

#[test]
fn apply_refuses_a_home_on_a_node_that_is_not_a_member_and_proposes_nothing() {
    let entries = solo_stored(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home(1)).await;
        let homes = [(Kind::Channel.key("plant.i1").unwrap(), name("plant.node2"))];
        let applied = mesh.apply(base(), create_indexes(2), homes.into()).await;
        let error = Error::UnknownNode(name("plant.node2"));
        assert_eq!(applied, Err(error.clone()));
        assert_eq!(
            error.to_string(),
            "a home names the node plant.node2, which is not a member of the region"
        );
    });
    assert_eq!(specs(&entries), []);
}

#[test]
fn apply_refuses_more_homes_than_one_change_gives_before_no_vote() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[2]).await.unwrap();
        let most = u128::try_from(HOMES_MAX).unwrap();
        let homes = create_homes(most + 1, "plant.node1");
        let applied = mesh.apply(base(), create_indexes(most + 1), homes).await;
        let error = Error::Homes {
            homes: 513,
            most: 512,
        };
        assert_eq!(applied, Err(error.clone()));
        assert_eq!(
            error.to_string(),
            "the change gives 513 homes, more than the 512 that one change can give"
        );
        let homes = create_homes(most, "plant.node3");
        let applied = mesh.apply(base(), create_indexes(most), homes).await;
        assert_eq!(applied, Err(Error::NoVote));
    });
}

#[test]
fn apply_counts_against_the_most_only_the_listed_indexes_with_no_home() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let most = u128::try_from(HOMES_MAX).unwrap();
        let first = create_indexes(most);
        let moved = pointer(1, &first);
        let homes = create_homes(most, "plant.node1");
        assert_eq!(mesh.apply(base(), first, homes).await, Ok(moved));
        let definitions = create_indexes(most + 1);
        let next = pointer(2, &definitions);
        let homes = create_homes(most + 1, "plant.node1");
        assert_eq!(mesh.apply(moved, definitions, homes).await, Ok(next));
        let last = channel::Key::from_u128(most + 7);
        assert_eq!(mesh.watch(last).next().await, Ok(Some(key(1))));
    });
}

// The listed node of `plant.i0` is no member, but the index has a home, so the apply
// reads no node of it.
#[test]
fn a_change_gives_only_the_listed_homes_of_the_indexes_with_no_home() {
    let entries = solo_stored(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        let first = create_indexes(1);
        let moved = pointer(1, &first);
        let homes = create_homes(1, "plant.node1");
        assert_eq!(mesh.apply(base(), first, homes).await, Ok(moved));
        let definitions = create_indexes(2);
        let next = pointer(2, &definitions);
        let mut homes = create_homes(2, "plant.node2");
        homes.insert(Kind::Channel.key("plant.i0").unwrap(), name("plant.node9"));
        assert_eq!(mesh.apply(moved, definitions, homes).await, Ok(next));
        assert_eq!(mesh.watch(INDEX).next().await, Ok(Some(key(1))));
        assert_eq!(mesh.watch(SECOND).next().await, Ok(Some(key(2))));
    });
    let homes: Vec<_> = specs(&entries)
        .into_iter()
        .map(|change| match change {
            Change::Spec { homes, .. } => homes,
            _ => unreachable!(),
        })
        .collect();
    let given = [
        BTreeMap::from([(INDEX, key(1))]),
        BTreeMap::from([(SECOND, key(2))]),
    ];
    assert_eq!(homes, given);
}

#[test]
fn apply_gives_homes_before_a_failed_read_of_the_base_tree() {
    let first = create_subjects(&["plant.a"], 1);
    let moved = pointer(1, &first);
    solo_stored(move |node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        assert_eq!(mesh.apply(base(), first, BTreeMap::new()).await, Ok(moved));
        let path = Path::new(BLOB).join(moved.root.to_string());
        node.fail_file(&path, Operation::Open);
        let over = u128::try_from(HOMES_MAX).unwrap() + 1;
        let homes = create_homes(over, "plant.node1");
        let applied = mesh.apply(moved, create_indexes(over), homes).await;
        let error = Error::Homes {
            homes: 513,
            most: 512,
        };
        assert_eq!(applied, Err(error));
    });
}

#[test]
fn apply_gives_unknown_node_before_quorum_and_puts_nothing() {
    let entries = solo_stored(|node, tasks| async move {
        let (mesh, store) = open_kept(&node, &tasks, &IDS).await;
        let definitions = create_indexes(1);
        let root = spec::region::tree(&mut Chunks::default(), &definitions).root;
        let homes = create_homes(1, "plant.node9");
        let applied = mesh.apply(base(), definitions, homes).await;
        assert_eq!(applied, Err(Error::UnknownNode(name("plant.node9"))));
        assert!(store.get(root).await.unwrap().is_none());
    });
    assert_eq!(specs(&entries), []);
}

#[test]
fn apply_gives_problems_before_a_home_that_is_not_an_index() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let ungoverned = create_subjects(&["other"], 1);
        let homes = create_homes(1, "plant.node1");
        let applied = mesh.apply(base(), ungoverned, homes).await;
        let problem = Problem::Ungoverned {
            name: name("other.@subject"),
            region: "plant".parse().unwrap(),
        };
        assert_eq!(applied, Err(Error::Problems(vec![problem])));
    });
}

// The second change is stale, so its homes wait for the third, which keeps the home
// of `INDEX` that the first gave and gives one to the index that it adds. A board
// takes a state only when the home of `INDEX` moves, so a set of it ends the run.
#[test]
fn each_member_reads_the_homes_of_a_spec_change_and_a_later_change_keeps_them() {
    let mut cluster = Cluster::new(0);
    cluster.start();
    cluster.run(seconds(5));
    let (a, b) = (create_indexes(1), create_indexes(2));
    let (moved, next) = (pointer(1, &a), pointer(2, &b));
    let first = BTreeMap::from([(INDEX, key(2))]);
    let both = BTreeMap::from([(INDEX, key(3)), (SECOND, key(3))]);
    cluster.apply_held(1, base(), &a, IDS.into(), first);
    cluster.run(seconds(5));
    cluster.apply_held(3, base(), &b, IDS.into(), both.clone());
    cluster.run(seconds(5));
    cluster.apply_held(3, moved, &b, IDS.into(), both);
    cluster.run(seconds(5));
    cluster.script(|_| home(9));
    cluster.run(seconds(5));
    let board = cluster.board();
    let stale = Error::Stale {
        base: base(),
        pointer: moved,
    };
    let applied = [
        (1, moved, Ok(moved)),
        (3, moved, Err(stale)),
        (3, next, Ok(next)),
    ];
    assert_eq!(board.applied, applied);
    for id in IDS {
        let homes = [None, Some(key(2)), Some(key(9))];
        assert_eq!(board.homes[&id], homes, "node {id}");
        assert_eq!(board.seconds[&id], Some(key(3)), "node {id}");
        assert_eq!(board.pointers[&id], next, "node {id}");
    }
}
