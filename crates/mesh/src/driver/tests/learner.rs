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

// A dial gives the open session to its peer, so the test restarts a follower, which
// then holds only the sessions made after its restart. The old session of node 4 to
// it ends at the first ping, each third of `IDLE`, so the run after is longer.
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
        cluster.run(seconds(30));
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

// No voter answers node 4, so each dial waits until it times out.
#[test]
fn the_drop_of_the_mesh_ends_the_wait_for_the_dial_of_a_learner() {
    for seed in 0..8 {
        solo_at(seed, move |node, tasks| async move {
            let config = dialed_at(&node, &tasks, 4, 0, &MEMBERS, &IDS).await;
            let transport = Rc::clone(&config.transport);
            let mesh = Mesh::open(config).await.unwrap();
            let clock = node.clock();
            clock.sleep(seconds(5)).await;
            let ended = mesh.ended();
            let dropped = clock.now();
            drop(mesh);
            ended.await;
            let waited = clock.now() - dropped;
            assert!(waited <= TICK, "seed {seed}: it ended after {waited}");
            assert_eq!(Rc::strong_count(&transport), 1, "seed {seed}");
        });
    }
}

// No voter answers node 4, so each dial would wait until it times out.
#[test]
fn a_learner_stops_its_wait_when_it_names_the_next_voter() {
    solo(|node, tasks| async move {
        let config = dialed_at(&node, &tasks, 4, 0, &MEMBERS, &IDS).await;
        let transport = Rc::clone(&config.transport);
        let _mesh = Mesh::open(config).await.unwrap();
        let clock = node.clock();
        clock.sleep(seconds(3)).await;
        let one = Rc::strong_count(&transport);
        clock.sleep(seconds(30)).await;
        assert_eq!(Rc::strong_count(&transport), one);
    });
}

// Each voter restarts, so node 4 holds a session to the new leader that the leader
// lost, and a dial gives that session until a ping of `transport` ends it.
#[test]
fn a_learner_reaches_a_restarted_leader_after_its_old_session_ends() {
    for seed in 0..8 {
        let mut cluster = cluster(seed, true);
        cluster.run(seconds(10));
        assert!(last(&cluster.board().homes, 4).is_some(), "run {seed}");
        for id in 1..=3 {
            let node = cluster.node(id).clone();
            cluster.sim.crash(&node, Crash::Process);
        }
        {
            let mut board = cluster.board.lock().unwrap();
            board.learners = [4].into();
            board.hidden = Some(4);
        }
        cluster.script(|_| home_to(9));
        for id in 1..=3 {
            cluster.start_voter(id);
        }
        // A third of `IDLE`, two election timeouts for each voter, and an election.
        cluster.run(seconds(30));
        let homes = cluster.board().homes;
        assert_eq!(last(&homes, 4), Some(key(9)), "run {seed}");
    }
}

// No voter answers node 4, so its dial would wait until it times out.
#[test]
fn a_stop_of_the_group_ends_the_wait_for_the_dial_of_a_learner() {
    solo(|node, tasks| async move {
        let config = dialed_at(&node, &tasks, 4, 0, &MEMBERS, &IDS).await;
        let transport = Rc::clone(&config.transport);
        let mesh = Mesh::open(config).await.unwrap();
        let clock = node.clock();
        clock.sleep(seconds(3)).await;
        let cause = fail_sync(&node);
        let heartbeat = proven(1, 4, Body::Heartbeat { commit: 0 });
        assert_eq!(mesh.receive(public(1), heartbeat), Ok(()));
        let stopped = clock.now();
        let mut watch = mesh.watch(INDEX);
        assert_eq!(watch.next().await, Ok(None));
        assert_eq!(watch.next().await, Err(cause));
        mesh.ended().await;
        let waited = clock.now() - stopped;
        assert!(waited <= TICK, "it ended after {waited}");
        drop(mesh);
        assert_eq!(Rc::strong_count(&transport), 1);
    });
}
