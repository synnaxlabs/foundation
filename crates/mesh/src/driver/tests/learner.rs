//! Tests of how a leader reaches a member that is not a voter, on a cluster of voters
//! 1 to 3 and the learner 4.

use super::*;

const MEMBERS: [u8; 4] = [1, 2, 3, 4];

/// A cluster of [`MEMBERS`] in which each node proposes itself as the home, with the
/// learner 4, whose card has no address when `hidden`.
fn cluster(seed: u64, hidden: bool) -> Cluster {
    let cluster = Cluster::of(seed, &MEMBERS);
    {
        let mut board = cluster.board.lock().unwrap();
        board.learners = [4].into();
        board.hidden = hidden.then_some(4);
    }
    cluster.script(home_to);
    cluster.start();
    cluster
}

// The home that node `id` last applied.
fn last(homes: &Homes, id: u8) -> Option<node::Key> {
    homes.get(&id)?.last().copied().flatten()
}

// No leader can dial node 4, so node 4 dials the voters in turn until it reaches
// the one that leads.
#[test]
fn a_learner_that_no_leader_can_dial_applies_the_changes_of_each_leader() {
    for seed in 0..4 {
        let mut cluster = cluster(seed, true);
        cluster.run(seconds(10));
        let board = cluster.board();
        let &[leader] = board.led.as_slice() else {
            panic!(
                "run {seed}: the group took a proposal from each of {:?}",
                board.led
            );
        };
        assert_eq!(last(&board.homes, 4), Some(key(leader)), "run {seed}");
        let node = cluster.node(leader).clone();
        cluster.sim.crash(&node, Crash::Process);
        cluster.script(|_| home_to(9));
        cluster.run(seconds(20));
        let homes = cluster.board().homes;
        assert_eq!(homes.get(&leader), None, "run {seed}");
        assert_eq!(last(&homes, 4), Some(key(9)), "run {seed}");
    }
}

// A dial of the learner reuses an open session, so the test restarts a follower,
// which then holds only the sessions dialed after its restart.
// A dial gives the open session to the peer, and the session of node 4 to the
// follower that crashes ends only at `IDLE`, so the run after the restart is longer.
#[test]
fn a_learner_that_the_leader_reaches_dials_no_voter() {
    for seed in 0..4 {
        let mut cluster = cluster(seed, false);
        cluster.run(seconds(10));
        let board = cluster.board();
        let &[leader] = board.led.as_slice() else {
            panic!(
                "run {seed}: the group took a proposal from each of {:?}",
                board.led
            );
        };
        assert_eq!(last(&board.homes, 4), Some(key(leader)), "run {seed}");
        let follower = if leader == 1 { 2 } else { 1 };
        let node = cluster.node(follower).clone();
        cluster.sim.crash(&node, Crash::Process);
        cluster.run(seconds(2));
        drop(cluster.board());
        cluster.board.lock().unwrap().learners = [4].into();
        cluster.script(|_| home_to(9));
        cluster.start_voter(follower);
        cluster.run(seconds(70));
        let board = cluster.board();
        assert_eq!(last(&board.homes, 4), Some(key(9)), "run {seed}");
        let homes = last(&board.homes, follower);
        assert_eq!(homes, Some(key(9)), "run {seed}");
        let peers = board.opened.iter().filter(|(to, _)| *to == follower);
        let id = |&(_, peer): &(u8, PublicKey)| {
            MEMBERS.into_iter().find(|&id| public(id) == peer)
        };
        let peers: Vec<_> = peers.map(id).collect();
        assert_eq!(peers, [Some(leader)], "run {seed}");
    }
}

// With no leader, node 4 names the voters in key order, one each two election
// timeouts, and starts again after the last. Each look falls half a tick after a
// tick.
#[test]
fn a_learner_that_no_leader_reaches_names_each_voter_in_turn() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 4, &MEMBERS, &IDS).await.unwrap();
        let clock = node.clock();
        clock.sleep(Span::from_nanos(TICK.nanos() / 2)).await;
        let mut turns = Vec::new();
        let mut turn = None;
        let quiet = 2 * ELECTION_TICKS;
        for tick in 0..=4 * quiet {
            let now = mesh.group.borrow().turn;
            if now != turn {
                turns.push((tick, now));
                turn = now;
            }
            clock.sleep(TICK).await;
        }
        let at = |turn, id| (turn * quiet, Some(key(id)));
        assert_eq!(turns, [at(1, 1), at(2, 2), at(3, 3), at(4, 1)]);
    });
}
