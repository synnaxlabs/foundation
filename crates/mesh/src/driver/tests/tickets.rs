//! Tests of `Mesh::ticket`: on a cluster of three voters, on one node, and on node 1
//! with node 2 as a raw peer that plays its leader.

use super::home::run;
use super::*;
use crate::ticket::{Record, Ticket, Voter};

impl Cluster {
    /// Node `node` makes a ticket with `options` at its next tick.
    fn make(&self, node: u8, options: Options) {
        self.board.lock().unwrap().tickets.insert(node, options);
    }
}

/// The voter of a ticket for voter `id` of a cluster.
fn voter_of(id: u8) -> Voter {
    let card = create_voter(id).card;
    Voter {
        key: key(id),
        public_key: public(id),
        addresses: card.card().addresses.clone(),
    }
}

// The tests read the record on each member from its state: an admit through
// `join::ask` would use a single-use ticket at the first member, so no public call
// shows the record on each member.
#[test]
fn a_call_on_each_voter_records_its_ticket_on_each_member() {
    for run in 0..4 {
        let (mut cluster, leader, follower, _) = Cluster::led(run);
        let edge = common::options("plant.edge", false);
        cluster.make(follower, edge.clone());
        cluster.make(leader, common::options("plant", true));
        cluster.run(seconds(5));
        cluster.script(|_| home_to(9));
        cluster.run(seconds(5));
        let mut board = cluster.board();
        board.made.sort_by_key(|&(id, _)| id != follower);
        let made: Vec<_> = board
            .made
            .into_iter()
            .map(|(id, made)| (id, made.unwrap()))
            .collect();
        let [(made_by, ref edge_ticket), (_, ref plant_ticket)] = made[..] else {
            panic!("run {run}: the calls gave {made:?}");
        };
        assert_eq!(made_by, follower, "run {run}");
        assert_ne!(edge_ticket.public_key(), plant_ticket.public_key());
        let records = [
            (edge_ticket, edge),
            (plant_ticket, common::options("plant", true)),
        ];
        for (ticket, options) in records {
            assert_eq!(ticket.region(), &"plant".parse().unwrap());
            assert_eq!(ticket.voters(), IDS.map(voter_of), "run {run}");
            let record = Record::new(ticket.public_key(), options);
            for id in IDS {
                let state = &board.states[&id];
                let held = state.ticket(ticket.public_key());
                assert_eq!(held, Some(&record), "run {run}: node {id}");
            }
        }
    }
}

#[test]
fn a_ticket_lists_only_the_voters_in_key_order() {
    let mut cluster = Cluster::of(0, &[1, 2, 3, 4]);
    cluster.board.lock().unwrap().learners = [2].into();
    cluster.script(home_to);
    cluster.start();
    cluster.run(seconds(5));
    cluster.make(4, common::options("plant", false));
    cluster.run(seconds(5));
    let (id, made) = cluster.board().made.pop().unwrap();
    assert_eq!(id, 4);
    assert_eq!(made.unwrap().voters(), [1, 3, 4].map(voter_of));
}

#[test]
fn a_call_on_a_member_that_is_not_a_voter_gives_no_vote() {
    let mut cluster = Cluster::new(5);
    cluster.board.lock().unwrap().learners = [3].into();
    cluster.script(home_to);
    cluster.start();
    cluster.run(seconds(5));
    let board = cluster.board();
    let &[at] = board.at.as_slice() else {
        panic!("the group took a proposal at each of {:?}", board.at);
    };
    cluster.make(3, common::options("plant", false));
    cluster.run(seconds(5));
    cluster.script(|_| home_to(9));
    cluster.run(seconds(5));
    let board = cluster.board();
    let made: Vec<_> = board
        .made
        .into_iter()
        .map(|(id, made)| (id, made.err()))
        .collect();
    assert_eq!(made, [(3, Some(Error::NoVote))]);
    assert_eq!(board.at, [after(at, 1)]);
}

#[test]
fn a_prefix_outside_the_region_gives_outside_and_proposes_nothing() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        let at = lead(&mesh, &node.clock(), home_to(1)).await;
        for prefix in ["plantx", "other.plant", "plan"] {
            let made = mesh.ticket(common::options(prefix, false)).await;
            let outside = Error::Outside {
                prefix: prefix.parse().unwrap(),
                region: "plant".parse().unwrap(),
            };
            assert_eq!(made.err(), Some(outside), "{prefix}");
        }
        assert_eq!(mesh.propose(home_to(1)).await, Ok(after(at, 1)));
        let outside = Error::Outside {
            prefix: "plantx".parse().unwrap(),
            region: "plant".parse().unwrap(),
        };
        assert_eq!(
            outside.to_string(),
            "the ticket's prefix plantx is not under the region's prefix plant"
        );
    });
}

#[test]
fn a_call_on_a_node_that_is_not_a_voter_gives_no_vote_at_once() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1, 2], &[2]).await.unwrap();
        let made = now(pin!(mesh.ticket(common::options("plant", false)))).await;
        assert_eq!(made.map(Result::err), Poll::Ready(Some(Error::NoVote)));
    });
}

// The entropy of the node gives each key pair, so two calls give two tickets. The test
// reads the record from the state, as the first test does.
#[test]
fn a_lone_voter_records_each_ticket_with_its_own_key_pair() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &[1], &[1]).await.unwrap();
        lead(&mesh, &node.clock(), home_to(1)).await;
        let options = common::options("plant.edge", false);
        let first = mesh.ticket(options.clone()).await.unwrap();
        let second = mesh.ticket(options.clone()).await.unwrap();
        assert_ne!(first.public_key(), second.public_key());
        let card = mesh.member(key(1)).unwrap().card.card().clone();
        let voter = Voter {
            key: key(1),
            public_key: card.public_key,
            addresses: card.addresses,
        };
        let group = mesh.group.borrow();
        for ticket in [first, second] {
            let record = Record::new(ticket.public_key(), options.clone());
            assert_eq!(group.state.ticket(ticket.public_key()), Some(&record));
            assert_eq!(ticket.voters(), std::slice::from_ref(&voter));
        }
    });
}

// Node 2 records the ticket of the first try and gives no answer, so node 1 proposes
// again, and the entry of the second try is refused as recorded.
#[test]
fn an_entry_of_an_earlier_try_counts_as_the_record() {
    let options = common::options("plant.edge", false);
    let asked = options.clone();
    let call = |_, mesh: Mesh| async move { mesh.ticket(asked).await };
    let (made, changes) = run(call, |mut leader| async move {
        let (first, unanswered) = leader.proposal().await;
        drop(unanswered);
        let (second, mut asked) = leader.proposal().await;
        leader.answer(&mut asked, at(2)).await.unwrap();
        let each = [(&first, at(1)), (&second, at(2))];
        let each = each.map(|(change, at)| (encoded(change), at));
        leader.append_each(Position::default(), each.into()).await;
        leader.rest(seconds(2)).await;
        [first, second]
    });
    let made = made.unwrap();
    let recorded = Change::Ticket {
        public_key: made.public_key(),
        options,
    };
    assert_eq!(changes, [recorded.clone(), recorded]);
}

// Node 2 sends the entry of the ticket and, after it, `appended`, and commits only the
// ticket. Gives the voters of the ticket, which read back from its text, and the voters
// of the newest configuration in the log of node 1.
fn ticket_after(appended: Vec<Voters>) -> (Vec<node::Key>, Vec<node::Key>) {
    let call = |_, mesh: Mesh| async move {
        let made = mesh.ticket(common::options("plant.edge", false)).await;
        let newest: Vec<_> = mesh.group.borrow().raft.voters().nodes().collect();
        (made, newest)
    };
    let ((made, newest), ()) = run(call, |mut leader| async move {
        let (change, mut asked) = leader.proposal().await;
        leader.answer(&mut asked, at(1)).await.unwrap();
        let ticket = Entry {
            at: at(1),
            data: Data::Bytes(encoded(&change)),
        };
        let configurations = appended.into_iter().zip(2..);
        let appended = configurations
            .map(|(voters, index)| common::voters_entry(2, at(index), voters));
        leader
            .append_entries(iter::once(ticket).chain(appended).collect(), 1)
            .await;
        leader.rest(seconds(2)).await;
    });
    let text = made.unwrap().text();
    let read: Ticket = text.parse().unwrap();
    let listed = read.voters().iter().map(|voter| voter.key).collect();
    (listed, newest)
}

// Node 4 is no member that node 1 applied, so a ticket that lists the newest
// configuration has no card for it.
#[test]
fn a_ticket_lists_the_applied_voters_when_the_log_replaces_one() {
    let joint = Voters {
        incoming: [1, 2, 4].map(key).into(),
        outgoing: IDS.map(key).into(),
    };
    let last = Voters {
        incoming: [1, 2, 4].map(key).into(),
        outgoing: [].into(),
    };
    let (listed, newest) = ticket_after(vec![joint, last]);
    assert_eq!(newest, [1, 2, 4].map(key));
    assert_eq!(listed, IDS.map(key));
}

#[test]
fn a_ticket_lists_the_applied_voters_when_the_log_adds_one() {
    let joint = Voters {
        incoming: [1, 2, 3, 4].map(key).into(),
        outgoing: IDS.map(key).into(),
    };
    let (listed, newest) = ticket_after(vec![joint]);
    assert_eq!(newest, [1, 2, 3, 4].map(key));
    assert_eq!(listed, IDS.map(key));
}

// Node 1 applies a joint configuration that removes node 3, and then the ticket.
#[test]
fn a_ticket_lists_each_voter_of_both_halves_of_an_applied_joint_configuration() {
    let call = |_, mesh: Mesh| async move {
        mesh.ticket(common::options("plant.edge", false)).await
    };
    let (made, ()) = run(call, |mut leader| async move {
        let (change, mut asked) = leader.proposal().await;
        leader.answer(&mut asked, at(2)).await.unwrap();
        let joint = Voters {
            incoming: [1, 2].map(key).into(),
            outgoing: IDS.map(key).into(),
        };
        let ticket = Entry {
            at: at(2),
            data: Data::Bytes(encoded(&change)),
        };
        let entries = vec![common::voters_entry(2, at(1), joint), ticket];
        leader.append_entries(entries, 2).await;
        leader.rest(seconds(2)).await;
    });
    let listed: Vec<_> = made
        .unwrap()
        .voters()
        .iter()
        .map(|voter| voter.key)
        .collect();
    assert_eq!(listed, IDS.map(key));
}
