//! Tests of `Mesh::apply`: on a cluster of three voters, and on one node.

use spec::definition::Kind;
use spec::region::Problem;
use spec::subject::Subject;

use super::*;

impl Cluster {
    /// Node `node` applies `definitions` on `base` at its next tick.
    fn apply(&self, node: u8, base: Pointer, definitions: BTreeMap<Name, Definition>) {
        let spec = (base, definitions);
        self.board.lock().unwrap().applies.insert(node, spec);
    }
}

/// A subject of `keys` keys at the tree key of each of `labels`.
fn create_subjects(labels: &[&str], keys: u16) -> BTreeMap<Name, Definition> {
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
fn create_large(count: usize) -> BTreeMap<Name, Definition> {
    let labels: Vec<String> = (0..count).map(|at| format!("plant.s{at}")).collect();
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    create_subjects(&labels, 64)
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
fn apply_on_a_node_that_is_not_a_voter_gives_problems_before_no_vote() {
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
            "the spec has 1025 chunks, more than the 1024 that one change lists"
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
        cluster.apply(first, base(), a);
        cluster.run(seconds(5));
        cluster.apply(second, base(), b.clone());
        cluster.run(seconds(5));
        cluster.apply(second, moved, b);
        cluster.run(seconds(5));
        let stale = Error::Stale {
            base: base(),
            pointer: moved,
        };
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
        cluster.apply(leader, base(), a.clone());
        cluster.apply(follower, base(), b.clone());
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
    cluster.apply(old, base(), definitions);
    cluster.run(seconds(5));
    assert_eq!(cluster.board().applied, []);
    cluster.link_each(old, 0.0);
    cluster.run(seconds(5));
    assert_eq!(cluster.board().applied, [(old, moved, Ok(moved))]);
}

fn name(text: &str) -> Name {
    text.parse().unwrap()
}
