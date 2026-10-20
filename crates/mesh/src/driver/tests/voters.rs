//! Tests of the put of the tree in force on the new voters of a change of voters, on a
//! cluster of three nodes.

use super::apply::{assert_used, base, create_largest, create_subjects, late, pointer};
use super::*;

/// A cluster in which voter 1, the one voter, applied the change of `definitions`,
/// which the learners 2 and 3 use.
fn lone(definitions: &BTreeMap<Name, Definition>) -> Cluster {
    let mut cluster = Cluster::new(0);
    cluster.board.lock().unwrap().learners = [2, 3].into();
    cluster.start();
    cluster.run(seconds(5));
    cluster.change(1, base(), definitions);
    cluster.run(seconds(5));
    assert_used(&cluster.board(), &IDS, pointer(1, definitions), definitions);
    cluster
}

/// What each call of `propose_voters` gave, in order, with no position.
fn outcomes(board: &Board) -> Vec<(u8, Result<(), Error>)> {
    board
        .configured
        .iter()
        .map(|(id, outcome)| (*id, outcome.as_ref().map(|_| ()).map_err(Clone::clone)))
        .collect()
}

// The learners got the tree through their spec task, so the put is one call on each.
// Voter 2 then applies a change: the set is in force.
#[test]
fn a_change_of_voters_puts_the_tree_in_force_on_each_new_voter() {
    let definitions = create_subjects(&["plant.a"], 1);
    let mut cluster = lone(&definitions);
    drop(cluster.board());
    cluster.configure(1, IDS.into());
    cluster.run(seconds(10));
    let board = cluster.board();
    assert_eq!(outcomes(&board), [(1, Ok(()))]);
    assert_eq!(board.blobs, [(2, 1), (3, 1)].into());
    let next = create_subjects(&["plant.b"], 1);
    cluster.change(2, pointer(1, &definitions), &next);
    cluster.run(seconds(5));
    let board = cluster.board();
    let moved = pointer(2, &next);
    assert_eq!(board.applied.last(), Some(&(2, moved, Ok(moved))));
    assert_used(&board, &IDS, moved, &next);
}

// The learners answer no blob stream, so the put ends at the deadline of the `Stored`
// of its first chunk on each. A change with voter 1 as the one holder then applies:
// the set did not change.
#[test]
fn a_change_of_voters_whose_puts_fail_gives_quorum_and_proposes_nothing() {
    let definitions = create_subjects(&["plant.a"], 1);
    let mut cluster = lone(&definitions);
    cluster.board.lock().unwrap().silent = [2, 3].into();
    cluster.configure(1, IDS.into());
    cluster.run(peer::DEADLINE);
    assert_eq!(cluster.board.lock().unwrap().configured, []);
    cluster.run(seconds(1));
    let quorum = Error::Quorum {
        held: 1,
        voters: 3,
        missed: vec![late(2), late(3)],
    };
    assert_eq!(outcomes(&cluster.board()), [(1, Err(quorum))]);
    let next = create_subjects(&["plant.b"], 1);
    cluster.change(1, pointer(1, &definitions), &next);
    cluster.run(seconds(5));
    let board = cluster.board();
    let moved = pointer(2, &next);
    assert_eq!(board.applied, [(1, moved, Ok(moved))]);
    assert_used(&board, &[1], moved, &next);
}

// Over links of 300 ms, the first put of the 26 chunks on each learner, in two calls,
// takes about 17 s. 5 s into it, voter 1 applies a second change, whose one holder
// is voter 1. At 12 s the learners stop answering new blob streams, so the put of the
// tree of the second change, after the joint entry, gets no `Stored`, and the call
// tries again each second. At 50 s they answer again, and the call ends.
#[test]
fn a_pointer_that_moves_before_the_joint_entry_is_put_until_a_majority_holds_it() {
    let mut cluster = Cluster::new(0);
    cluster.board.lock().unwrap().learners = [2, 3].into();
    cluster.delay(Span::from_nanos(300 * Span::MILLISECOND.nanos()));
    cluster.start();
    cluster.run(seconds(10));
    let definitions = create_largest(25);
    cluster.change(1, base(), &definitions);
    cluster.run(seconds(60));
    let first = pointer(1, &definitions);
    assert_used(&cluster.board(), &IDS, first, &definitions);
    cluster.configure(1, IDS.into());
    cluster.run(seconds(5));
    let next = create_subjects(&["plant.b"], 1);
    cluster.change(1, first, &next);
    cluster.run(seconds(7));
    let moved = pointer(2, &next);
    {
        let mut board = cluster.board.lock().unwrap();
        assert_eq!(board.applied, [(1, moved, Ok(moved))]);
        board.silent = [2, 3].into();
    }
    cluster.run(seconds(38));
    {
        let mut board = cluster.board.lock().unwrap();
        assert_eq!(board.configured, []);
        board.silent = BTreeSet::new();
    }
    cluster.run(seconds(20));
    let board = cluster.board();
    assert_eq!(outcomes(&board), [(1, Ok(()))]);
    // The first put, and the second put after each deadline, on each learner.
    for id in [2, 3] {
        assert!(board.blobs[&id] >= 3, "node {id}: {}", board.blobs[&id]);
    }
    assert_used(&board, &IDS, moved, &next);
}

// Voter 3 is down at the apply, and comes back while voters 1 and 2 answer no blob
// stream, so its store lacks the tree, from its root.
#[test]
fn a_proposer_whose_store_lacks_the_tree_in_force_gives_tree_missing() {
    let mut cluster = Cluster::new(0);
    cluster.start_voter(1);
    cluster.start_voter(2);
    cluster.run(seconds(5));
    let definitions = create_subjects(&["plant.a"], 1);
    cluster.change(1, base(), &definitions);
    cluster.run(seconds(5));
    cluster.board.lock().unwrap().silent = [1, 2].into();
    cluster.start_voter(3);
    cluster.run(seconds(5));
    cluster.configure(3, IDS.into());
    cluster.run(seconds(5));
    let root = pointer(1, &definitions).root;
    let missing = Error::Tree(tree::Error::Missing(root));
    assert_eq!(outcomes(&cluster.board()), [(3, Err(missing.clone()))]);
    assert_eq!(
        missing.to_string(),
        format!(
            "this node cannot list the spec tree in force: chunk {root} is not here"
        )
    );
}

#[test]
fn a_proposal_by_a_node_that_is_not_a_voter_gives_no_vote_and_puts_nothing() {
    let definitions = create_subjects(&["plant.a"], 1);
    let mut cluster = lone(&definitions);
    drop(cluster.board());
    cluster.configure(2, IDS.into());
    cluster.run(seconds(5));
    let board = cluster.board();
    assert_eq!(outcomes(&board), [(2, Err(Error::NoVote))]);
    assert_eq!(board.blobs, BTreeMap::new());
}
