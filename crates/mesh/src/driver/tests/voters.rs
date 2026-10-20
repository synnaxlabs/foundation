//! Tests of the put of the tree in force on the new voters of a change of voters, on a
//! cluster of three nodes.
//!
//! A lone test reads the voters through the private `Mesh::group`: `Mesh` gives no
//! read of its voters, and a change that shows them waits out the deadline of a put.

use std::fmt::Write as _;

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

use super::apply::{
    assert_used, base, create_indexes, create_largest, create_subjects, late, pointer,
};
use super::unapplied::changes;
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

/// The level, the message, and each other field as ` <name>=<value>` of each event of
/// `mesh` that a node logs, in order. Clones share them.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl Subscriber for Log {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target().starts_with("mesh::")
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut line = Line::default();
        event.record(&mut line);
        let level = event.metadata().level();
        let Line { message, fields } = line;
        self.0
            .lock()
            .unwrap()
            .push(format!("{level} {message}{fields}"));
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

#[derive(Default)]
struct Line {
    message: String,
    fields: String,
}

impl Visit for Line {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            write!(self.fields, " {}={value:?}", field.name()).unwrap();
        }
    }
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

/// The one-way delay of each link of `silent_after_the_joint_entry`.
const LINK: Span = Span::from_nanos(300 * Span::MILLISECOND.nanos());

/// Over links of `LINK`, the first put of the 26 chunks on each learner, in two calls,
/// takes about 17 s. 5 s into it, voter 1 applies a second change, whose one holder is
/// voter 1. At 12 s the learners stop answering new blob streams, so the put of the
/// tree of the second change, after the joint entry, gets no `Stored`, and the call
/// tries again `RETRY` after each try. Gives the cluster with the log of voter 1, at
/// 12 s, and the pointer and the definitions of the second change.
fn silent_after_the_joint_entry(
    log: &Log,
) -> (Cluster, Pointer, BTreeMap<Name, Definition>) {
    let mut cluster = Cluster::new(0);
    cluster.board.lock().unwrap().learners = [2, 3].into();
    cluster.logs.insert(1, tracing::Dispatch::new(log.clone()));
    cluster.delay(LINK);
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
    let mut board = cluster.board.lock().unwrap();
    assert_eq!(board.applied, [(1, moved, Ok(moved))]);
    board.silent = [2, 3].into();
    drop(board);
    (cluster, moved, next)
}

/// The line of the warning of a put after the joint entry on which no learner answers.
fn warning() -> String {
    let quorum = Error::Quorum {
        held: 1,
        voters: 3,
        missed: vec![late(2), late(3)],
    };
    format!(
        "WARN the new voters are in force, and the put of the spec tree in force on \
         them runs again error={quorum}"
    )
}

// Voter 1 warns at the first failed put, and not again before the learners answer at
// 50 s. Then the call ends.
#[test]
fn a_pointer_that_moves_before_the_joint_entry_is_put_until_a_majority_holds_it() {
    let log = Log::default();
    let (mut cluster, moved, next) = silent_after_the_joint_entry(&log);
    cluster.run(seconds(38));
    {
        let mut board = cluster.board.lock().unwrap();
        assert_eq!(board.configured, []);
        board.silent = BTreeSet::new();
    }
    assert_eq!(log.lines(), [warning()]);
    cluster.run(seconds(20));
    let board = cluster.board();
    assert_eq!(outcomes(&board), [(1, Ok(()))]);
    // The first put, and the second put after each deadline, on each learner.
    for id in [2, 3] {
        assert!(board.blobs[&id] >= 3, "node {id}: {}", board.blobs[&id]);
    }
    assert_used(&board, &IDS, moved, &next);
}

// A try ends each 11 s: the deadline of the put, then `RETRY`. So the first warning
// comes about 16 s into the silence, the second at the first failed try at least a
// minute after it, and the third after the run.
#[test]
fn a_put_after_the_joint_entry_that_fails_for_a_minute_warns_again() {
    let log = Log::default();
    let (mut cluster, _, _) = silent_after_the_joint_entry(&log);
    cluster.run(seconds(100));
    assert_eq!(cluster.board.lock().unwrap().configured, []);
    assert_eq!(log.lines(), [warning(), warning()]);
}

// A second change, with voter 1 as its one holder, gives `INDEX` a home and keeps the
// tree. It applies before the joint entry, so the call puts the tree once on each
// learner.
#[test]
fn a_change_that_moves_only_homes_before_the_joint_entry_puts_the_tree_once() {
    let definitions = create_indexes(1);
    let mut cluster = lone(&definitions);
    drop(cluster.board());
    let first = pointer(1, &definitions);
    let homes = BTreeMap::from([(INDEX, key(1))]);
    cluster.configure(1, IDS.into());
    cluster.apply_held(1, first, &definitions, [1].into(), homes);
    cluster.run(seconds(10));
    let board = cluster.board();
    let moved = pointer(2, &definitions);
    assert_eq!(moved.root, first.root);
    assert_eq!(board.applied, [(1, moved, Ok(moved))]);
    assert_eq!(outcomes(&board), [(1, Ok(()))]);
    assert_eq!(board.blobs, [(2, 1), (3, 1)].into());
}

// Once the set is in force, voter 2 follows voter 1.
#[test]
fn a_proposal_by_a_follower_gives_not_leader_and_puts_nothing() {
    let definitions = create_subjects(&["plant.a"], 1);
    let mut cluster = lone(&definitions);
    drop(cluster.board());
    cluster.configure(1, IDS.into());
    cluster.run(seconds(10));
    assert_eq!(cluster.board().blobs, [(2, 1), (3, 1)].into());
    cluster.configure(2, [1, 2].into());
    cluster.run(seconds(10));
    let board = cluster.board();
    let leader = Some(key(1));
    let not_leader = Error::Raft(raft::Error::NotLeader { leader });
    assert_eq!(outcomes(&board), [(2, Err(not_leader))]);
    assert_eq!(board.blobs, BTreeMap::new());
}

// A `Spec` entry with the lone voter as holder moves the pointer to a tree that the
// store of the lone voter lacks: the state machine never reads chunks.
#[test]
fn a_proposer_whose_store_lacks_the_tree_in_force_gives_tree_missing() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home_to(1)).await;
        let root = unheld_root();
        settle_unheld(&mesh, root).await;
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

/// The root of a tree of one chunk that no store of a lone test holds.
fn unheld_root() -> Digest {
    let definitions = create_subjects(&["plant.a"], 1);
    spec::region::tree(&mut Chunks::default(), &definitions).root
}

/// Applies a change on `mesh`, the lone voter and the one holder, to a tree of the
/// one chunk `root`, with no put of it in the store of `mesh`, and gives the pointer
/// that it moved to.
async fn settle_unheld(mesh: &Mesh, root: Digest) -> Pointer {
    let holders = [key(1)].into();
    let start = mesh.pointer();
    let moved = mesh
        .settle_spec(start, root, [root].into(), holders, BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(mesh.pointer(), moved);
    moved
}

// A change with voter 1 as the one holder then applies: the set did not change.
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
    let next = create_subjects(&["plant.b"], 1);
    cluster.change(1, pointer(1, &definitions), &next);
    cluster.run(seconds(5));
    let board = cluster.board();
    let moved = pointer(2, &next);
    assert_eq!(board.applied, [(1, moved, Ok(moved))]);
    assert_used(&board, &[1], moved, &next);
}

// The founding tree is empty, so the call puts nothing before the joint entry.
#[test]
fn a_change_of_voters_gives_the_position_of_its_joint_entry() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        let led = lead(&mesh, &node.clock(), home_to(1)).await;
        let proposed = mesh.propose_voters([key(1)].into()).await;
        assert_eq!(proposed, Ok(after(led, 1)));
    });
}

// Node 2 never acks, so the joint entry of the first call stays uncommitted.
#[test]
fn a_change_while_a_change_of_voters_is_pending_gives_change_pending() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        let led = lead(&mesh, &node.clock(), home_to(1)).await;
        let first = mesh.propose_voters([key(1), key(2)].into());
        assert!(pending_for(first, &node.clock(), seconds(1)).await);
        let second = now(pin!(mesh.propose_voters([key(1)].into()))).await;
        let pending = raft::Error::ChangePending { at: after(led, 1) };
        assert_eq!(second, Poll::Ready(Err(Error::Raft(pending))));
    });
}

// The write holds the joint entry, then an append of a new leader replaces it.
#[test]
fn a_joint_entry_that_a_new_leader_replaces_gives_not_leader() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
        let first = elect(&mesh).await;
        let mut proposed = pin!(mesh.propose_voters([key(1), key(2)].into()));
        assert!(pending_for(proposed.as_mut(), &node.clock(), seconds(1)).await);
        let joint = Voters {
            incoming: [key(1), key(2)].into(),
            outgoing: IDS.iter().copied().map(key).collect(),
        };
        assert_eq!(mesh.group.borrow().raft.voters(), &joint);
        let bad = at(2);
        let append = Body::Append {
            prev: first,
            entries: vec![Entry {
                at: bad,
                data: changes(&[home_to(2)]).remove(0),
            }],
            commit: 2,
        };
        assert_eq!(mesh.receive(public(2), proven(2, 1, append)), Ok(()));
        let leader = Some(key(2));
        let replaced = Error::Raft(raft::Error::NotLeader { leader });
        assert_eq!(proposed.await, Err(replaced));
        assert_eq!(mesh.group.borrow().raft.voters().outgoing, BTreeSet::new());
    });
}

// The store of the proposer fails the read of the one chunk of the tree in force.
#[test]
fn a_proposer_whose_store_fails_a_read_gives_blob_and_proposes_nothing() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home_to(1)).await;
        let definitions = create_subjects(&["plant.a"], 1);
        let moved = mesh
            .apply(base(), definitions, BTreeMap::new())
            .await
            .unwrap();
        let path = Path::new(BLOB).join(moved.root.to_string());
        node.fail_file(&path, Operation::Open);
        let proposed = mesh.propose_voters([key(1), key(2)].into());
        let ended = within(&node.clock(), seconds(1), proposed).await;
        let cause = files::Error::Io {
            path,
            operation: Operation::Open,
            code: 5,
        };
        let failed = Error::Blob(blob::Error::Files(cause));
        assert_eq!(ended, Some(Err(failed)));
        assert_eq!(mesh.group.borrow().raft.voters().outgoing, BTreeSet::new());
    });
}

// The pointer moved to a tree whose one chunk arrives in the store after the joint
// entry. The call tries again `RETRY` after its last try, not at once.
#[test]
fn a_moved_tree_that_arrives_after_the_joint_entry_is_put_at_the_next_try() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        let led = lead(&mesh, &node.clock(), home_to(1)).await;
        let definitions = create_subjects(&["plant.a"], 1);
        let mut chunks = Chunks::default();
        let update = spec::region::tree(&mut chunks, &definitions);
        let mut voters = pin!(moved_unheld(&mesh, update.root));
        let third = Span::from_nanos(300 * Span::MILLISECOND.nanos());
        assert!(pending_for(voters.as_mut(), &node.clock(), third).await);
        put(&mesh.store, &mesh.pool, &chunks, &update.chunks)
            .await
            .unwrap();
        assert!(pending_for(voters.as_mut(), &node.clock(), third).await);
        let ended = within(&node.clock(), seconds(2), voters).await;
        assert_eq!(ended, Some(Ok(after(led, 2))));
    });
}

// The pointer moved to a tree whose one chunk is not a tree chunk. It arrives in the
// store after the joint entry, and the next try ends the call.
#[test]
fn a_moved_tree_with_a_corrupt_chunk_gives_tree_corrupt_after_the_joint_entry() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home_to(1)).await;
        let bytes = [9];
        let root = Digest::of(&bytes);
        let mut voters = pin!(moved_unheld(&mesh, root));
        assert!(pending_for(voters.as_mut(), &node.clock(), seconds(3)).await);
        let block = mesh.pool.copy(&bytes).unwrap();
        mesh.store.put(root, &block).await.unwrap();
        let ended = within(&node.clock(), seconds(5), voters).await;
        let corrupt = Error::Tree(tree::Error::Corrupt(root));
        assert_eq!(ended, Some(Err(corrupt.clone())));
        let text = format!(
            "this node cannot list the spec tree in force: chunk {root} is not a chunk \
             of a spec tree"
        );
        assert_eq!(corrupt.to_string(), text);
    });
}

/// Proposes the lone voter of `mesh` as the set, while a `Spec` entry before the
/// joint entry moves the pointer to a tree of the one chunk `root`, which the store
/// of `mesh` lacks. Gives the call, pending at its put after the joint entry.
async fn moved_unheld(mesh: &Mesh, root: Digest) -> Result<Position, Error> {
    let mut spec = pin!(settle_unheld(mesh, root));
    assert!(now(spec.as_mut()).await.is_pending());
    let mut voters = pin!(mesh.propose_voters([key(1)].into()));
    assert!(now(voters.as_mut()).await.is_pending());
    spec.await;
    voters.await
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
// of the lone voter lacks, so the call tries again after the joint entry, and warns
// once. Then the group stops.
#[test]
fn a_stop_while_the_moved_tree_is_missing_gives_stopped() {
    let log = Log::default();
    solo_logged(&log, |node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home_to(1)).await;
        let mut voters = pin!(moved_unheld(&mesh, unheld_root()));
        assert!(pending_for(voters.as_mut(), &node.clock(), seconds(3)).await);
        let stopped = Error::Stopped(fail_sync(&node));
        assert_eq!(mesh.propose(home_to(2)).await, Err(stopped.clone()));
        let ended = within(&node.clock(), seconds(30), voters).await;
        assert_eq!(ended, Some(Err(stopped)));
    });
    let missing = Error::Tree(tree::Error::Missing(unheld_root()));
    let warning = format!(
        "WARN the new voters are in force, and the put of the spec tree in force on \
         them runs again error={missing}"
    );
    assert_eq!(log.lines(), [warning]);
}

/// As [`solo`], with `log` as the log of the shard of `body`.
fn solo_logged<F: Future<Output = ()> + 'static>(
    log: &Log,
    body: impl FnOnce(sim::node::Node, Tasks) -> F + Send + 'static,
) {
    let mut sim = Sim::new(crate::common::simulation(0));
    let node = sim.node(sim::node::Config::default());
    let shards = node.shards().logged(tracing::Dispatch::new(log.clone()));
    let config = env::shards::Config {
        name: "solo".into(),
        core: None,
    };
    drop(shards.start(config, |tasks| body(node, tasks)).unwrap());
    sim.run().unwrap();
}
