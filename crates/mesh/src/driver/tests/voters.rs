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

// A `Spec` entry with the lone voter as holder moves the pointer to a tree that the
// store of the lone voter lacks: the state machine never reads chunks.
#[test]
fn a_proposer_whose_store_lacks_the_tree_in_force_gives_tree_missing() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home_to(1)).await;
        let root = settle_unheld(&mesh).await.root;
        let proposed = now(pin!(mesh.propose_voters([key(1), key(2)].into()))).await;
        let missing = Error::Tree(tree::Error::Missing(root));
        assert_eq!(proposed, Poll::Ready(Err(missing.clone())));
        assert_eq!(
            missing.to_string(),
            format!(
                "this node cannot list the spec tree in force: chunk {root} is not here"
            )
        );
        assert_eq!(mesh.group.borrow().raft.voters().outgoing, BTreeSet::new());
    });
}

/// Applies a change on `mesh`, the lone voter and the one holder, with no put of its
/// chunks in the store of `mesh`, and gives the pointer that it moved to.
async fn settle_unheld(mesh: &Mesh) -> Pointer {
    let definitions = create_subjects(&["plant.a"], 1);
    let mut chunks = Chunks::default();
    let update = spec::region::tree(&mut chunks, &definitions);
    let listed = update.chunks.iter().copied().collect();
    let holders = [key(1)].into();
    let start = mesh.pointer();
    let moved = mesh
        .settle_spec(start, update.root, listed, holders, BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(mesh.pointer(), moved);
    moved
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

// Voter 1 of {1, 2} leads nothing while voter 2 is absent.
#[test]
fn a_proposal_by_a_voter_that_does_not_lead_gives_not_leader_and_puts_nothing() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1, 2]).await.unwrap();
        node.clock().sleep(seconds(5)).await;
        let proposed = now(pin!(mesh.propose_voters([key(1)].into()))).await;
        let not_leader = raft::Error::NotLeader { leader: None };
        assert_eq!(proposed, Poll::Ready(Err(Error::Raft(not_leader))));
    });
}

// A `Spec` entry before the joint entry moves the pointer to a tree that the store
// of the lone voter lacks, so the call tries again after the joint entry. Then the
// group stops.
#[test]
fn a_stop_while_the_moved_tree_is_missing_gives_stopped() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home_to(1)).await;
        let mut spec = pin!(settle_unheld(&mesh));
        assert!(now(spec.as_mut()).await.is_pending());
        let mut voters = pin!(mesh.propose_voters([key(1)].into()));
        assert!(now(voters.as_mut()).await.is_pending());
        spec.await;
        assert!(pending_for(voters.as_mut(), &node.clock(), seconds(3)).await);
        let stopped = Error::Stopped(fail_sync(&node));
        assert_eq!(mesh.propose(home_to(2)).await, Err(stopped.clone()));
        let ended = within(&node.clock(), seconds(30), voters).await;
        assert_eq!(ended, Some(Err(stopped)));
    });
}
