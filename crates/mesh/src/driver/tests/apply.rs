//! Tests of `Mesh::apply` on one node, and of its spec change on a cluster of three
//! voters, where each voter holds the chunks.

use spec::definition::Kind;
use spec::region::Problem;
use spec::subject::Subject;

use super::send::stop;
use super::*;

impl Cluster {
    /// Node `node` proposes the spec change of `definitions` on `base` at its next
    /// tick, with each node as a holder.
    fn apply(&self, node: u8, base: Pointer, definitions: &BTreeMap<Name, Definition>) {
        self.apply_held(node, base, definitions, IDS.into());
    }

    /// As [`Cluster::apply`], with `holders`.
    fn apply_held(
        &self,
        node: u8,
        base: Pointer,
        definitions: &BTreeMap<Name, Definition>,
        holders: BTreeSet<u8>,
    ) {
        let update = spec::region::tree(&mut Chunks::default(), definitions);
        let spec = Proposal {
            base,
            root: update.root,
            chunks: update.chunks.into_iter().collect(),
            holders: holders.into_iter().map(key).collect(),
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
        let applied = now(pin!(mesh.apply(base(), definitions))).await;
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
        let applied = now(pin!(mesh.apply(base(), valid))).await;
        assert_eq!(applied, Poll::Ready(Err(Error::NoVote)));
        let ungoverned = create_subjects(&["other"], 1);
        let applied = now(pin!(mesh.apply(base(), ungoverned))).await;
        let problem = Problem::Ungoverned {
            name: name("other.@subject"),
            region: "plant".parse().unwrap(),
        };
        assert_eq!(applied, Poll::Ready(Err(Error::Problems(vec![problem]))));
        let applied = now(pin!(mesh.apply(base(), create_large(1951)))).await;
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
        let applied = now(pin!(mesh.apply(base(), ungoverned))).await;
        assert!(matches!(applied, Poll::Ready(Err(Error::Problems(_)))));
        let applied = now(pin!(mesh.apply(base(), create_large(1951)))).await;
        assert!(matches!(applied, Poll::Ready(Err(Error::Large { .. }))));
        let valid = create_subjects(&["plant.a"], 1);
        let applied = now(pin!(mesh.apply(base(), valid))).await;
        assert_eq!(applied, Poll::Ready(Err(Error::Stopped(stopped))));
    });
}

#[test]
fn apply_refuses_a_tree_of_more_chunks_than_one_change_lists() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let applied = now(pin!(mesh.apply(base(), create_large(1951)))).await;
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
        assert_eq!(mesh.apply(base(), definitions).await, Ok(moved));
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

#[test]
fn an_apply_that_finds_a_later_pointer_of_its_own_root_is_stale() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let (a, b) = (
            create_subjects(&["plant.a"], 1),
            create_subjects(&["plant.b"], 1),
        );
        let first = mesh.apply(base(), a.clone()).await.unwrap();
        let second = mesh.apply(first, b).await.unwrap();
        let third = mesh.apply(second, a.clone()).await.unwrap();
        assert_eq!(third, pointer(3, &a));
        let stale = Error::Stale {
            base: base(),
            pointer: third,
        };
        assert_eq!(mesh.apply(base(), a).await, Err(stale));
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
            assert_eq!(mesh.apply(last, definitions).await, Err(stale.clone()));
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
        assert_eq!(mesh.apply(base(), applied).await, Ok(moved));
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
        let moved = mesh.apply(base(), first).await.unwrap();
        mesh.apply(moved, second).await.unwrap();
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
        let moved = mesh.apply(base(), first).await.unwrap();
        drop(mesh);
        node.clock().sleep(Span::MILLISECOND).await;
        let path = Path::new(BLOB).join(shared.to_string());
        node.files().remove(&path).await.unwrap();
        let (mesh, store) = open_kept(&node, &tasks, &[1]).await;
        assert!(store.get(shared).await.unwrap().is_none());
        mesh.apply(moved, second).await.unwrap();
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
        let moved = mesh.apply(base(), definitions.clone()).await.unwrap();
        drop(mesh);
        node.clock().sleep(Span::MILLISECOND).await;
        let path = Path::new(BLOB).join(root.to_string());
        node.files().remove(&path).await.unwrap();
        let (mesh, store) = open_kept(&node, &tasks, &[1]).await;
        assert!(store.get(root).await.unwrap().is_none());
        mesh.apply(moved, definitions).await.unwrap();
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
            assert_eq!(mesh.apply(at, definitions.clone()).await, Err(stale));
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
        assert_eq!(mesh.apply(base(), definitions).await, Err(quorum.clone()));
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
    cluster.apply_held(1, base(), &definitions, [1].into());
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
        assert_eq!(mesh.apply(base(), definitions).await, Err(failed.clone()));
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
        let moved = mesh.apply(base(), create_large(1950)).await.unwrap();
        let larger = create_large(1951);
        let expected = pointer(2, &larger);
        assert_eq!(mesh.apply(moved, larger).await, Ok(expected));
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
        let applied = mesh.apply(base(), definitions).await;
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
        assert_eq!(mesh.apply(base(), first).await, Ok(moved));
        let path = Path::new(BLOB).join(moved.root.to_string());
        node.fail_file(&path, Operation::Open);
        let second = create_subjects(&["plant.b"], 1);
        let cause = files::Error::Io {
            path,
            operation: Operation::Open,
            code: 5,
        };
        let failed = Error::Blob(blob::Error::Files(cause));
        assert_eq!(mesh.apply(moved, second).await, Err(failed));
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
