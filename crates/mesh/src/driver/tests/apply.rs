//! Tests of `Mesh::apply` on one node and on a cluster of three voters, and of its
//! spec change on a cluster.

use std::sync::atomic::{AtomicUsize, Ordering};

use spec::channel::Channel;
use spec::data_type::DataType;
use spec::definition::Kind;
use spec::region::Problem;
use spec::subject::Subject;

use super::send::stop;
use super::*;
use crate::change::HOMES_MAX;
use crate::common::name;
use crate::error::{Miss, Missed};

impl Cluster {
    /// Node `node` proposes the spec change of `definitions` on `base` at its next
    /// tick, with each node as a holder.
    pub(super) fn apply(
        &self,
        node: u8,
        base: Pointer,
        definitions: &BTreeMap<Name, Definition>,
    ) {
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

    /// Node `node` calls `Mesh::apply` of `definitions` on `base` at its next tick.
    pub(super) fn change(
        &self,
        node: u8,
        base: Pointer,
        definitions: &BTreeMap<Name, Definition>,
    ) {
        let change = (base, definitions.clone());
        self.board.lock().unwrap().changes.insert(node, change);
    }

    /// Sets a one-way delay of `delay` on each link between two voters.
    fn delay(&mut self, delay: Span) {
        let config = link::Config {
            delay,
            ..link::Config::default()
        };
        for (a, b) in [(1, 2), (1, 3), (2, 3)] {
            let (a, b) = (self.node(a).clone(), self.node(b).clone());
            self.sim.link(&a, &b, config);
            self.sim.link(&b, &a, config);
        }
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
pub(super) fn pointer(
    version: u64,
    definitions: &BTreeMap<Name, Definition>,
) -> Pointer {
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
pub(super) fn base() -> Pointer {
    Pointer {
        version: 0,
        root: tree::empty(),
    }
}

#[test]
fn apply_gives_each_problem_of_the_spec_and_proposes_nothing() {
    let entries = solo_stored(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home_to(1)).await;
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
        cluster.script(|_| home_to(9));
        cluster.run(seconds(5));
        let board = cluster.board();
        // The two calls end on two nodes, in either order.
        let mut applied = board.applied.clone();
        applied.sort_by_key(|(_, _, result)| result.is_err());
        let [(_, _, Ok(moved)), (loser, _, Err(stale))] = applied.as_slice() else {
            panic!("run {run}: the calls gave {applied:?}");
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
    cluster.script(|_| home_to(9));
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

// The second call finds the pointer that it makes, so the pointer moves once.
#[test]
fn a_lone_voter_gives_two_calls_of_one_change_from_one_base_one_pointer() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let definitions = create_subjects(&["plant.a"], 1);
        let moved = pointer(1, &definitions);
        assert_eq!(
            mesh.apply(base(), definitions.clone(), BTreeMap::new())
                .await,
            Ok(moved)
        );
        assert_eq!(
            mesh.apply(base(), definitions, BTreeMap::new()).await,
            Ok(moved)
        );
        assert_eq!(mesh.pointer(), moved);
    });
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

// No node runs at the addresses of voters 2 and 3, so each dial fails.
#[test]
fn apply_in_a_region_of_three_voters_with_no_peer_gives_quorum_and_proposes_nothing() {
    let entries = solo_stored(|node, tasks| async move {
        let (mesh, store) = open_kept(&node, &tasks, &IDS).await;
        let definitions = create_subjects(&["plant.a"], 1);
        let root = spec::region::tree(&mut Chunks::default(), &definitions).root;
        let missed = |id| Missed {
            node: key(id),
            name: Some(name(&format!("plant.node{id}"))),
            cause: Miss::Dial(transport::Error::Unreachable {
                peer: Some(public(id)),
                attempts: Box::default(),
            }),
        };
        let quorum = Error::Quorum {
            held: 1,
            voters: 3,
            missed: vec![missed(2), missed(3)],
        };
        assert_eq!(
            mesh.apply(base(), definitions, BTreeMap::new()).await,
            Err(quorum.clone())
        );
        assert!(store.get(root).await.unwrap().is_some());
        let unreachable = |id| format!("no address reached peer {}", public(id));
        assert_eq!(
            quorum.to_string(),
            format!(
                "1 of 3 voters hold the chunks of the spec change, not a majority; \
                 plant.node2: its dial failed: {}; plant.node3: its dial failed: {}",
                unreachable(2),
                unreachable(3)
            )
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
    cluster.script(|_| home_to(9));
    cluster.run(seconds(5));
    let board = cluster.board();
    let quorum = Error::Quorum {
        held: 1,
        voters: 2,
        missed: Vec::new(),
    };
    assert_eq!(
        quorum.to_string(),
        "1 of 2 voters hold the chunks of the spec change, not a majority, at the \
         apply of the change"
    );
    assert_eq!(board.applied, [(1, base(), Err(quorum))]);
    // Node 3 is in no configuration, so it gets each entry as a learner.
    for id in IDS {
        assert_eq!(board.homes[&id].last(), Some(&Some(key(9))), "node {id}");
        assert_eq!(board.pointers[&id], base(), "node {id}");
    }
}

/// Asserts that each of `ids` uses the spec of `definitions` at `pointer`.
pub(super) fn assert_used(
    board: &Board,
    ids: &[u8],
    pointer: Pointer,
    definitions: &BTreeMap<Name, Definition>,
) {
    for id in ids {
        let seen = &board.specs[id];
        assert_eq!(seen.pointer, Some(pointer), "node {id}");
        assert_eq!(&seen.definitions, definitions, "node {id}");
    }
}

// Voter 3 is down, so the holders are 1 and 2, and voter 2 uses the spec from the
// chunks that voter 1 put on it.
#[test]
fn an_apply_with_one_voter_down_commits_and_the_other_voter_uses_the_spec() {
    let mut cluster = Cluster::new(0);
    cluster.start_voter(1);
    cluster.start_voter(2);
    cluster.run(seconds(5));
    let definitions = create_subjects(&["plant.a"], 1);
    cluster.change(1, base(), &definitions);
    cluster.run(seconds(5));
    let board = cluster.board();
    let moved = pointer(1, &definitions);
    assert_eq!(board.applied, [(1, moved, Ok(moved))]);
    assert_used(&board, &[1, 2], moved, &definitions);
}

#[test]
fn an_apply_in_a_region_of_two_voters_commits_and_each_uses_the_spec() {
    let mut cluster = Cluster::new(0);
    cluster.board.lock().unwrap().learners = [3].into();
    cluster.start();
    cluster.run(seconds(5));
    let definitions = create_subjects(&["plant.a"], 1);
    cluster.change(2, base(), &definitions);
    cluster.run(seconds(5));
    let board = cluster.board();
    let moved = pointer(1, &definitions);
    assert_eq!(board.applied, [(2, moved, Ok(moved))]);
    assert_used(&board, &[1, 2], moved, &definitions);
}

// Voter 2 takes the put and never answers, and voter 3 is down, so the apply ends at
// the deadline of the `Stored` of its first chunk on voter 2.
#[test]
fn an_apply_whose_one_live_peer_never_answers_gives_quorum_at_the_deadline() {
    let mut cluster = Cluster::new(0);
    cluster.board.lock().unwrap().silent = [2].into();
    cluster.start_voter(1);
    cluster.start_voter(2);
    cluster.run(seconds(5));
    cluster.change(1, base(), &create_subjects(&["plant.a"], 1));
    cluster.run(peer::DEADLINE);
    assert_eq!(cluster.board.lock().unwrap().applied, []);
    cluster.run(seconds(1));
    let quorum = Error::Quorum {
        held: 1,
        voters: 3,
        missed: vec![late(2), late(3)],
    };
    assert_eq!(cluster.board().applied, [(1, base(), Err(quorum))]);
}

// Each chunk takes about 1 s over links of 300 ms, so a put of the 41 chunks on a voter
// takes about 41 s, and each `Stored` comes within the deadline.
#[test]
fn an_apply_over_links_of_300_ms_commits_as_each_chunk_ends_within_the_deadline() {
    let mut cluster = Cluster::new(0);
    cluster.delay(Span::from_nanos(300 * Span::MILLISECOND.nanos()));
    cluster.start();
    cluster.run(seconds(10));
    let definitions = create_largest(40);
    cluster.change(1, base(), &definitions);
    cluster.run(seconds(120));
    let board = cluster.board();
    let moved = pointer(1, &definitions);
    assert_eq!(board.applied, [(1, moved, Ok(moved))]);
    assert_used(&board, &IDS, moved, &definitions);
}

// Voter 3 is down. Voter 2 stores chunks for 15 s over links of 300 ms, about one a
// second, and then its links are cut. With seed 1, its last `Stored` was sent before
// the cut and comes within 300 ms of it, so its put misses between 10 s and 10.5 s
// after the cut. A deadline of the whole call would miss at 10 s after its start.
#[test]
fn a_voter_that_goes_silent_in_its_put_misses_the_deadline_after_its_last_stored() {
    let mut cluster = Cluster::new(1);
    cluster.delay(Span::from_nanos(300 * Span::MILLISECOND.nanos()));
    cluster.start_voter(1);
    cluster.start_voter(2);
    cluster.run(seconds(10));
    cluster.change(1, base(), &create_largest(40));
    cluster.run(seconds(15));
    cluster.link(1, 2, 1.0);
    cluster.link(2, 1, 1.0);
    cluster.run(peer::DEADLINE);
    assert_eq!(cluster.board().applied, []);
    cluster.run(Span::from_nanos(500 * Span::MILLISECOND.nanos()));
    let quorum = Error::Quorum {
        held: 1,
        voters: 3,
        missed: vec![late(2), late(3)],
    };
    assert_eq!(
        quorum.to_string(),
        "1 of 3 voters hold the chunks of the spec change, not a majority; \
         plant.node2: a step of its put passed the deadline of 10s; plant.node3: a \
         step of its put passed the deadline of 10s"
    );
    assert_eq!(cluster.board().applied, [(1, base(), Err(quorum))]);
}

// Voter 3 is down, and voter 2 restarts after each span of `restarts` in turn, in a
// put of about 41 s, over 16 seeds. The put dials voter 2 again after each restart,
// as the session before each stored a chunk, or lost to the dial of voter 2 at its
// start. So the apply commits.
fn restart_in_put(restarts: &[i64]) {
    const DELAY: Span = Span::from_nanos(300 * Span::MILLISECOND.nanos());
    for seed in 0..16 {
        let mut cluster = Cluster::new(seed);
        cluster.delay(DELAY);
        cluster.start_voter(1);
        cluster.start_voter(2);
        cluster.run(seconds(10));
        let definitions = create_largest(40);
        cluster.change(1, base(), &definitions);
        for &run in restarts {
            cluster.run(seconds(run));
            let node = cluster.node(2).clone();
            cluster.sim.crash(&node, Crash::Power);
            cluster.start_voter(2);
        }
        cluster.run(seconds(120));
        let moved = pointer(1, &definitions);
        let board = cluster.board();
        assert_eq!(board.applied, [(1, moved, Ok(moved))], "seed {seed}");
        assert_used(&board, &[1, 2], moved, &definitions);
        assert!(
            !board.blobs.contains_key(&1),
            "seed {seed}: {:?}",
            board.blobs
        );
    }
}

#[test]
fn an_apply_counts_a_voter_that_restarts_once_in_its_put() {
    restart_in_put(&[10]);
}

#[test]
fn an_apply_counts_a_voter_that_restarts_twice_in_its_put() {
    restart_in_put(&[10, 15]);
}

// Voter 3 is down, and voter 2 restarts at 10 s and again 1 s later, before the
// redialed session stores a chunk, which takes about 1 s over links of 300 ms. A
// session that ends with no chunk stored ends the put after one redial.
#[test]
fn an_apply_misses_a_voter_whose_redialed_session_ends_with_no_chunk_stored() {
    let mut cluster = Cluster::new(0);
    cluster.delay(Span::from_nanos(300 * Span::MILLISECOND.nanos()));
    cluster.start_voter(1);
    cluster.start_voter(2);
    cluster.run(seconds(10));
    cluster.change(1, base(), &create_largest(40));
    for run in [10, 1] {
        cluster.run(seconds(run));
        let node = cluster.node(2).clone();
        cluster.sim.crash(&node, Crash::Power);
        cluster.start_voter(2);
    }
    cluster.run(seconds(120));
    let broken = blob::peer::Error::Stream(transport::Error::Broken {
        reason: "reset by peer".to_owned(),
    });
    let quorum = Error::Quorum {
        held: 1,
        voters: 3,
        missed: vec![
            Missed {
                node: key(2),
                name: Some(name("plant.node2")),
                cause: Miss::Put(broken),
            },
            late(3),
        ],
    };
    assert_eq!(
        quorum.to_string(),
        "1 of 3 voters hold the chunks of the spec change, not a majority; \
         plant.node2: its put failed: the blob stream failed: the connection broke: \
         reset by peer; plant.node3: a step of its put passed the deadline of 10s"
    );
    assert_eq!(cluster.board().applied, [(1, base(), Err(quorum))]);
}

// Voter 3 is down, and voter 2 closes the session of each blob stream with code 0, as
// one that lost to a dial of the peer does. The put dials once more, and once more for
// the code 0, so voter 2 serves three blob streams and misses.
#[test]
fn an_apply_redials_a_voter_once_for_a_session_that_ends_with_code_0() {
    let mut cluster = Cluster::new(0);
    cluster.start_voter(1);
    cluster.start_voter(2);
    cluster.run(seconds(5));
    cluster.board.lock().unwrap().closes = [(2, [0, 0, 0].into())].into();
    cluster.change(1, base(), &create_largest(1));
    cluster.run(seconds(15));
    let closed =
        blob::peer::Error::Stream(transport::Error::PeerClosed { code: Code(0) });
    let quorum = Error::Quorum {
        held: 1,
        voters: 3,
        missed: vec![
            Missed {
                node: key(2),
                name: Some(name("plant.node2")),
                cause: Miss::Put(closed),
            },
            late(3),
        ],
    };
    let board = cluster.board();
    assert_eq!(board.applied, [(1, base(), Err(quorum))]);
    assert_eq!(board.blobs, [(2, 3)].into());
}

// Voter 3 is down, and voter 2 closes the sessions of its first two blob streams with
// `codes`, in order, then serves. Gives whether the apply committed, the blob streams
// that each node served, and those served to a clean end.
fn closes(codes: [u32; 2]) -> (bool, BTreeMap<u8, usize>, BTreeMap<u8, usize>) {
    let mut cluster = Cluster::new(0);
    cluster.start_voter(1);
    cluster.start_voter(2);
    cluster.run(seconds(5));
    cluster.board.lock().unwrap().closes = [(2, codes.into())].into();
    let definitions = create_largest(1);
    cluster.change(1, base(), &definitions);
    cluster.run(seconds(30));
    let moved = pointer(1, &definitions);
    let board = cluster.board();
    (
        board.applied == [(1, moved, Ok(moved))],
        board.blobs,
        board.clean,
    )
}

// The session that ends with code 0 earns its own redial, so the plain redial is left
// for the other session, whichever ends first. The put ends the stream that it does
// not lose.
#[test]
fn an_apply_redials_a_voter_for_a_code_0_end_before_or_after_another_end() {
    let committed = (true, [(2, 3)].into(), [(2, 1)].into());
    assert_eq!(closes([7, 0]), committed, "code 7, then code 0");
    assert_eq!(closes([0, 7]), committed, "code 0, then code 7");
}

// Voter 3 is down, and voter 2 restarts 1.5 s into the apply, after the last `Stored`
// of its put and before the end of its stream, so the end fails with an error. Each
// chunk is durable on voter 2, so it is a holder, and the put opens no second stream
// to it.
#[test]
fn an_apply_counts_a_voter_whose_stream_end_fails_with_an_error_after_each_stored() {
    let mut cluster = Cluster::new(0);
    cluster.delay(Span::from_nanos(300 * Span::MILLISECOND.nanos()));
    cluster.start_voter(1);
    cluster.start_voter(2);
    cluster.run(seconds(10));
    let definitions = create_largest(1);
    cluster.change(1, base(), &definitions);
    cluster.run(Span::from_nanos(1500 * Span::MILLISECOND.nanos()));
    let node = cluster.node(2).clone();
    cluster.sim.crash(&node, Crash::Power);
    cluster.start_voter(2);
    cluster.run(seconds(30));
    let moved = pointer(1, &definitions);
    let board = cluster.board();
    assert_eq!(board.applied, [(1, moved, Ok(moved))]);
    assert_eq!(board.blobs, [(2, 1)].into());
}

// Voter 3 is down, and the links of voter 2 break 1.5 s into the apply, after the
// last `Stored` of its put and before the end of its stream, so the end passes its
// deadline. Each chunk is durable on voter 2, so it is a holder, and the put opens no
// second stream to it.
#[test]
fn an_apply_counts_a_voter_whose_stream_end_passes_the_deadline_after_each_stored() {
    let mut cluster = Cluster::new(0);
    cluster.delay(Span::from_nanos(300 * Span::MILLISECOND.nanos()));
    cluster.start_voter(1);
    cluster.start_voter(2);
    cluster.run(seconds(10));
    let definitions = create_largest(1);
    cluster.change(1, base(), &definitions);
    cluster.run(Span::from_nanos(1500 * Span::MILLISECOND.nanos()));
    cluster.link(1, 2, 1.0);
    cluster.link(2, 1, 1.0);
    cluster.run(seconds(12));
    let board = cluster.board();
    assert_eq!(board.applied, []);
    assert_eq!(board.blobs, [(2, 1)].into());
    cluster.delay(Span::from_nanos(300 * Span::MILLISECOND.nanos()));
    cluster.run(seconds(60));
    let moved = pointer(1, &definitions);
    assert_eq!(cluster.board().applied, [(1, moved, Ok(moved))]);
}

// Voter 3 holds each blob stream with no answer, and the links of voters 1 and 2
// break 1.5 s into the apply, after the last `Stored` of the put on voter 2. The end
// of that stream never comes, so the put ends `Ok` at its deadline, and the apply
// commits with voter 3 before the links come back.
#[test]
fn an_apply_counts_a_voter_whose_stream_never_ends_at_the_deadline_of_the_end() {
    let mut cluster = Cluster::new(0);
    cluster.delay(Span::from_nanos(300 * Span::MILLISECOND.nanos()));
    cluster.start();
    cluster.run(seconds(10));
    let (led, _) = cluster.take();
    assert!(!led.contains(&2), "voter 2 led: {led:?}");
    cluster.board.lock().unwrap().silent = [3].into();
    let definitions = create_largest(1);
    cluster.change(1, base(), &definitions);
    cluster.run(Span::from_nanos(1500 * Span::MILLISECOND.nanos()));
    cluster.link(1, 2, 1.0);
    cluster.link(2, 1, 1.0);
    cluster.run(seconds(15));
    let moved = pointer(1, &definitions);
    assert_eq!(cluster.board().applied, [(1, moved, Ok(moved))]);
}

// `count` subjects of 512 keys, each of which fills one chunk of 16 417 bytes.
pub(super) fn create_largest(count: usize) -> BTreeMap<Name, Definition> {
    let labels: Vec<String> = (0..count).map(|at| format!("plant.s{at}")).collect();
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    create_subjects(&labels, 512)
}

// Voter `id`, a step of whose put passed the deadline.
fn late(id: u8) -> Missed {
    Missed {
        node: key(id),
        name: Some(name(&format!("plant.node{id}"))),
        cause: Miss::Deadline,
    }
}

// The follower `p` sends no `raft` message to the follower `q`, which restarts, and
// the leader holds each blob stream with no answer. So the apply of `p` commits only
// when its put reaches the new process of `q`.
#[test]
fn an_apply_of_a_follower_counts_a_follower_that_restarted() {
    let mut cluster = Cluster::new(4);
    cluster.script(home_to);
    cluster.start();
    cluster.run(seconds(5));
    let leader = cluster.take().0[0];
    let mut followers = IDS.into_iter().filter(|&id| id != leader);
    let (p, q) = (followers.next().unwrap(), followers.next().unwrap());
    let node = cluster.node(q).clone();
    cluster.sim.crash(&node, Crash::Power);
    cluster.start_voter(q);
    cluster.run(seconds(5));
    cluster.board.lock().unwrap().silent = [leader].into();
    let definitions = create_subjects(&["plant.a"], 1);
    cluster.change(p, base(), &definitions);
    cluster.run(seconds(5));
    let moved = pointer(1, &definitions);
    assert_eq!(cluster.board().applied, [(p, moved, Ok(moved))]);
}

// The cards of voters 2 and 3 have addresses where no node answers, so the puts
// wait in their dials. The apply runs in a task of its own, so only the stop wakes
// it, and the stop falls between two wakes of the dials, which come each second.
#[test]
fn an_apply_whose_puts_wait_gives_stopped_as_soon_as_the_group_stops() {
    let entries = solo_stored(|node, tasks| async move {
        let mesh = Mesh::start(dialed_at(&node, &tasks, 1, PORT, &IDS, &IDS).await);
        let mesh = mesh.await.unwrap();
        let clock = node.clock();
        let given = Rc::new(RefCell::new(None));
        let (proposer, slot) = (mesh.clone(), Rc::clone(&given));
        tasks.spawn(async move {
            let definitions = create_subjects(&["plant.a"], 1);
            let applied = proposer.apply(base(), definitions, BTreeMap::new()).await;
            *slot.borrow_mut() = Some(applied);
        });
        clock
            .sleep(Span::from_nanos(1_123 * Span::MILLISECOND.nanos()))
            .await;
        assert_eq!(*given.borrow(), None);
        let stopped = stop(&node, &mesh);
        clock.sleep(Span::MILLISECOND).await;
        assert_eq!(*given.borrow(), Some(Err(Error::Stopped(stopped))));
    });
    assert_eq!(specs(&entries), []);
}

// The puts on voters 2 and 3 wait in their dials until the deadline, so the apply
// waits in the group before its `Quorum`. A waker left in the group only takes memory
// until the next commit, change of leader, or stop. tests/memory
// `dropped_calls_of_the_spec_hold_no_heap` shows it through `spec`.
#[test]
fn an_apply_that_ends_frees_its_waker_in_the_group() {
    solo(|node, tasks| async move {
        let mesh = Mesh::start(dialed_at(&node, &tasks, 1, PORT, &IDS, &IDS).await);
        let mesh = mesh.await.unwrap();
        let definitions = create_subjects(&["plant.a"], 1);
        let applied = mesh.apply(base(), definitions, BTreeMap::new()).await;
        let quorum = Error::Quorum {
            held: 1,
            voters: 3,
            missed: vec![late(2), late(3)],
        };
        assert_eq!(applied, Err(quorum));
        assert!(mesh.group.borrow().calls.is_empty());
    });
}

// The store of voter 2 fails the put of the chunks, so the proposer is the one holder
// of the two voters.
#[test]
fn an_apply_whose_one_peer_fails_its_put_gives_quorum() {
    let mut cluster = Cluster::new(0);
    cluster.board.lock().unwrap().learners = [3].into();
    cluster.start();
    cluster.run(seconds(5));
    cluster
        .node(2)
        .fail_file(Path::new(BLOB), Operation::SyncDir);
    cluster.change(1, base(), &create_subjects(&["plant.a"], 1));
    cluster.run(seconds(1));
    let reset = transport::Error::Reset {
        code: transport::Code(0),
    };
    let quorum = Error::Quorum {
        held: 1,
        voters: 2,
        missed: vec![Missed {
            node: key(2),
            name: Some(name("plant.node2")),
            cause: Miss::Put(blob::peer::Error::Stream(reset)),
        }],
    };
    assert_eq!(cluster.board().applied, [(1, base(), Err(quorum))]);
}

// The put on voter 2 reads the chunk file of the proposer, which fails.
#[test]
fn an_apply_whose_store_fails_a_read_for_a_peer_names_the_error_of_the_store() {
    let mut cluster = Cluster::new(0);
    cluster.board.lock().unwrap().learners = [3].into();
    cluster.start();
    cluster.run(seconds(5));
    let definitions = create_subjects(&["plant.a"], 1);
    let root = spec::region::tree(&mut Chunks::default(), &definitions).root;
    let path = Path::new(BLOB).join(root.to_string());
    cluster.node(1).fail_file(&path, Operation::ReadAt);
    cluster.change(1, base(), &definitions);
    cluster.run(seconds(5));
    let shown = path.display().to_string();
    let cause = files::Error::Io {
        path,
        operation: Operation::ReadAt,
        code: 5,
    };
    let quorum = Error::Quorum {
        held: 1,
        voters: 2,
        missed: vec![Missed {
            node: key(2),
            name: Some(name("plant.node2")),
            cause: Miss::Put(blob::peer::Error::Store(blob::Error::Files(cause))),
        }],
    };
    assert_eq!(cluster.board().applied, [(1, base(), Err(quorum.clone()))]);
    assert_eq!(
        quorum.to_string(),
        format!(
            "1 of 2 voters hold the chunks of the spec change, not a majority; \
             plant.node2: its put failed: read_at of {shown} failed with OS error 5"
        )
    );
}

// Voter 1 leaves 127 voters for voter 128. With no chunk to put, each put ends `Ok`
// at once, in the order of the keys. A change holds the 65 holders. It calls `spread`,
// as `apply` cannot reach a joint configuration until #336.
#[test]
fn spread_in_a_joint_configuration_counts_a_majority_of_each_half() {
    solo(|node, tasks| async move {
        let members: Vec<u8> = (1..=128).collect();
        let mesh = open(&node, &tasks, 1, &members, &[1]).await.unwrap();
        let keys = |ids: std::ops::RangeInclusive<u8>| ids.map(key).collect();
        let voters = Voters {
            incoming: keys(2..=128),
            outgoing: keys(1..=127),
        };
        let holders = mesh.spread(&voters, &[]).await.unwrap();
        assert_eq!(holders, keys(1..=65));
        let change = Change::Spec {
            base: base(),
            root: base().root,
            chunks: BTreeSet::new(),
            holders,
            homes: BTreeMap::new(),
        };
        let mut bytes = Vec::new();
        change.encode(&mut bytes);
        assert_eq!(Change::decode(&bytes), Ok(change));
    });
}

// It calls `spread`, as `check_proposer` gives `NoVote` to a node that is not a voter,
// so `apply` cannot reach this case.
#[test]
fn spread_on_a_node_that_is_not_a_voter_counts_only_the_voters() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2, 3, 4], &[1]).await.unwrap();
        let voters = Voters {
            incoming: [2, 3, 4].map(key).into(),
            outgoing: BTreeSet::new(),
        };
        assert_eq!(mesh.spread(&voters, &[]).await, Ok([2, 3].map(key).into()));
    });
}

#[test]
fn spread_names_a_voter_that_this_node_has_no_record_of() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        let voters = Voters {
            incoming: [1, 9].map(key).into(),
            outgoing: BTreeSet::new(),
        };
        let quorum = Error::Quorum {
            held: 1,
            voters: 2,
            missed: vec![Missed {
                node: key(9),
                name: None,
                cause: Miss::Unknown,
            }],
        };
        let spread = mesh.spread(&voters, &[Digest([1; 32])]).await;
        assert_eq!(spread, Err(quorum.clone()));
        assert_eq!(
            quorum.to_string(),
            format!(
                "1 of 2 voters hold the chunks of the spec change, not a majority; {}: \
                 this node has no record of it",
                key(9)
            )
        );
    });
}

// Each chunk takes a round trip of 200 ms, so the put of all 1024 chunks on a voter
// takes more than 200 s, and each `Stored` comes within the deadline.
#[test]
fn an_apply_of_the_most_chunks_commits_over_links_of_100_ms() {
    let mut cluster = Cluster::new(0);
    cluster.delay(Span::from_nanos(100 * Span::MILLISECOND.nanos()));
    cluster.start();
    cluster.run(seconds(10));
    let definitions = create_large(1950);
    cluster.change(1, base(), &definitions);
    cluster.run(seconds(300));
    let board = cluster.board();
    let moved = pointer(1, &definitions);
    assert_eq!(board.applied, [(1, moved, Ok(moved))]);
    assert_used(&board, &IDS, moved, &definitions);
}

// Each of 40 subjects of 512 keys fills one chunk of 16 417 bytes, the size at which
// the chunker cuts.
#[test]
fn an_apply_of_the_largest_chunks_commits_over_links_of_100_ms() {
    let mut cluster = Cluster::new(0);
    cluster.delay(Span::from_nanos(100 * Span::MILLISECOND.nanos()));
    cluster.start();
    cluster.run(seconds(10));
    let definitions = create_largest(40);
    let mut chunks = Chunks::default();
    let update = spec::region::tree(&mut chunks, &definitions);
    let largest = update.chunks.iter().map(|&d| chunks.get(d).unwrap().len());
    assert_eq!(largest.max(), Some(16_417));
    assert_eq!(update.chunks.len(), 41);
    cluster.change(1, base(), &definitions);
    cluster.run(seconds(60));
    let board = cluster.board();
    let moved = pointer(1, &definitions);
    assert_eq!(board.applied, [(1, moved, Ok(moved))]);
    assert_used(&board, &IDS, moved, &definitions);
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

#[test]
fn the_base_tree_comes_from_the_store_when_it_is_the_tree_in_use() {
    let first = create_subjects(&["plant.a"], 1);
    let moved = pointer(1, &first);
    solo(move |node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        assert_eq!(mesh.apply(base(), first, BTreeMap::new()).await, Ok(moved));
        assert_eq!(mesh.spec().await.unwrap().pointer, Some(moved));
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
    });
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

// `INDEX` has a home, so the change leaves the pointer, and a later apply on it
// applies.
#[test]
fn an_apply_of_the_tree_of_its_base_with_each_index_homed_gives_the_base() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        let a = create_indexes(1);
        let moved = pointer(1, &a);
        let first = create_homes(1, "plant.node1");
        assert_eq!(mesh.apply(base(), a.clone(), first).await, Ok(moved));
        let other = create_homes(1, "plant.node2");
        assert_eq!(mesh.apply(moved, a, other).await, Ok(moved));
        assert_eq!(mesh.pointer(), moved);
        assert_eq!(mesh.spec().await.unwrap().pointer, Some(moved));
        assert_eq!(mesh.watch(INDEX).next().await, Ok(Some(key(1))));
        let b = create_indexes(2);
        let next = pointer(2, &b);
        assert_eq!(mesh.apply(moved, b, BTreeMap::new()).await, Ok(next));
    });
}

// The second call gives `INDEX` a home, so the pointer moves to the root of `moved`
// at the next version, and the third call, which leaves the pointer, finds it.
#[test]
fn an_apply_of_the_tree_of_a_replaced_base_gives_stale() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        let a = create_indexes(1);
        let moved = pointer(1, &a);
        let applied = mesh.apply(base(), a.clone(), BTreeMap::new()).await;
        assert_eq!(applied, Ok(moved));
        let homes = create_homes(1, "plant.node1");
        let next = pointer(2, &a);
        assert_eq!(mesh.apply(moved, a.clone(), homes).await, Ok(next));
        let stale = Error::Stale {
            base: moved,
            pointer: next,
        };
        assert_eq!(mesh.apply(moved, a, BTreeMap::new()).await, Err(stale));
    });
}

#[test]
fn an_apply_at_the_root_of_its_base_that_gives_a_home_moves_the_pointer() {
    solo(|node, tasks| async move {
        let mesh = open_founded(&node, &tasks, 1).await;
        let definitions = create_indexes(2);
        let founded = pointer(0, &definitions);
        let moved = pointer(1, &definitions);
        let homes = create_homes(2, "plant.node2");
        assert_eq!(mesh.apply(founded, definitions, homes).await, Ok(moved));
        assert_eq!(mesh.watch(INDEX).next().await, Ok(Some(key(1))));
        assert_eq!(mesh.watch(SECOND).next().await, Ok(Some(key(2))));
    });
}

#[test]
fn apply_gives_ok_when_a_listed_index_keeps_the_home_it_had() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home_to(1)).await;
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
        cluster.script(|_| home_to(1));
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
pub(super) struct Counted(pub(super) AtomicUsize);

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
        lead(&mesh, &node.clock(), home_to(1)).await;
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
        lead(&mesh, &node.clock(), home_to(1)).await;
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
        lead(&mesh, &node.clock(), home_to(1)).await;
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
    cluster.script(|_| home_to(9));
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

// The call is still reading the base tree when `SECOND` gets its home.
#[test]
fn an_index_that_gets_a_home_while_the_call_reads_the_base_tree_needs_no_known_node() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home_to(1)).await;
        let a = create_subjects(&["plant.a"], 1);
        let moved = pointer(1, &a);
        assert_eq!(
            mesh.apply(base(), a.clone(), BTreeMap::new()).await,
            Ok(moved)
        );
        let mut b = create_indexes(2);
        b.extend(a);
        let homes = BTreeMap::from([(
            Kind::Channel.key("plant.i1").unwrap(),
            name("plant.node9"),
        )]);
        let next = pointer(2, &b);
        let mut call = pin!(mesh.apply(moved, b, homes));
        assert!(now(call.as_mut()).await.is_pending());
        assert_eq!(mesh.set_home(SECOND, key(2)).await, Ok(()));
        assert_eq!(mesh.watch(SECOND).next().await, Ok(Some(key(2))));
        assert_eq!(call.await, Ok(next));
        assert_eq!(mesh.watch(SECOND).next().await, Ok(Some(key(2))));
    });
}
