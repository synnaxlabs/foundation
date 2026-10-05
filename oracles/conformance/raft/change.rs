//! Membership change scenarios ported from the tests of etcd/raft (Copyright 2015
//! The etcd Authors, Apache License 2.0, see `LICENSE`). This file is modified from
//! the etcd source: `README.md` lists each source and the changes.

use raft::{Body, Data, Entry, Error, Hard, Role, Voters};

use crate::common::{
    ELECTION, accept_all, at_term, elect, leader, noop, position, reply, set, start,
};

fn voters(incoming: &[u8], outgoing: &[u8]) -> Voters {
    Voters {
        incoming: set(incoming),
        outgoing: set(outgoing),
    }
}

fn config(term: u64, index: u64, voters: Voters) -> Entry {
    Entry {
        at: position(term, index),
        data: Data::Voters(voters),
    }
}

#[test]
fn step_config() {
    let (mut raft, mut disk) = leader(2);
    disk.store(raft.ready());
    let index = disk.last();
    let at = raft.propose_voters(set(&[1, 2])).unwrap();
    assert_eq!(at, position(1, index + 1));
    disk.store(raft.ready());
    assert_eq!(disk.last(), index + 1);
    assert_eq!(
        disk.entries.last(),
        Some(&config(1, index + 1, voters(&[1, 2], &[1, 2])))
    );
    assert_eq!(
        raft.propose_voters(set(&[1, 2])),
        Err(Error::ChangePending { at })
    );
}

/// etcd turns a second change into an empty entry; here it is an error and the log
/// does not change.
#[test]
fn step_ignore_config() {
    let (mut raft, mut disk) = leader(2);
    disk.store(raft.ready());
    let at = raft.propose_voters(set(&[1, 2])).unwrap();
    disk.store(raft.ready());
    let index = disk.last();
    let error = raft.propose_voters(set(&[1, 2])).unwrap_err();
    assert_eq!(error, Error::ChangePending { at });
    assert_eq!(
        error.to_string(),
        "a configuration change at index 2 in term 1 is pending"
    );
    disk.store(raft.ready());
    assert_eq!(disk.last(), index);
}

/// etcd blocks a change until the new leader commits an entry of its term. Here only
/// an uncommitted configuration entry blocks one.
#[test]
fn new_leader_pending_config() {
    for (entries, pending) in [
        (vec![noop(1, 1)], None),
        (
            vec![noop(1, 1), config(1, 2, voters(&[1, 2], &[]))],
            Some(position(1, 2)),
        ),
    ] {
        let (mut raft, mut disk) = start(1, &[1, 2], ELECTION, at_term(1), entries, 0);
        elect(&mut raft, &mut disk, &[2]);
        let result = raft.propose_voters(set(&[1, 2]));
        assert_eq!(result.err(), pending.map(|at| Error::ChangePending { at }));
    }
}

/// etcd applies the change directly; here node 2 acknowledges the joint entry and
/// the leave.
#[test]
fn add_node() {
    let (mut raft, mut disk) = leader(1);
    disk.store(raft.ready());
    raft.propose_voters(set(&[1, 2])).unwrap();
    assert_eq!(raft.voters(), &voters(&[1, 2], &[1]));
    accept_all(&mut raft, &mut disk);
    assert_eq!(raft.voters(), &voters(&[1, 2], &[]));
    assert_eq!(
        disk.committed,
        [
            noop(1, 1),
            config(1, 2, voters(&[1, 2], &[1])),
            config(1, 3, voters(&[1, 2], &[]))
        ]
    );
}

#[test]
fn add_node_check_quorum() {
    let (mut raft, mut disk) = leader(1);
    disk.store(raft.ready());
    for _ in 0..ELECTION - 1 {
        raft.tick(0);
    }
    raft.propose_voters(set(&[1, 2])).unwrap();
    raft.tick(0);
    assert_eq!(raft.role(), Role::Leader);
    for _ in 0..ELECTION {
        raft.tick(0);
    }
    assert_eq!(raft.role(), Role::Follower);
}

/// etcd panics on removing the last voter; here the proposal is `Error::NoVoters`.
#[test]
fn remove_node() {
    let (mut raft, mut disk) = leader(2);
    disk.store(raft.ready());
    raft.propose_voters(set(&[1])).unwrap();
    disk.store(raft.ready());
    raft.step(reply(2, 1, Body::AppendReply { last: 2 }))
        .unwrap();
    assert_eq!(raft.voters(), &voters(&[1], &[]));
    assert_eq!(disk.store(raft.ready()).len(), 1);
    assert_eq!(disk.committed(), 3);
    assert_eq!(raft.propose_voters(set(&[])), Err(Error::NoVoters));
}

/// etcd commits the proposal after the caller applies the change; here the leave is
/// in force at once, so both commit in one step.
#[test]
fn commit_after_remove_node() {
    let (mut raft, mut disk) = leader(2);
    disk.store(raft.ready());
    let joint = raft.propose_voters(set(&[1])).unwrap();
    disk.store(raft.ready());
    assert_eq!(disk.committed(), 0);
    raft.propose(b"hello".to_vec()).unwrap();
    raft.step(reply(2, 1, Body::AppendReply { last: joint.index }))
        .unwrap();
    disk.store(raft.ready());
    assert_eq!(
        disk.committed,
        [
            noop(1, 1),
            config(1, 2, voters(&[1], &[1, 2])),
            Entry {
                at: position(1, 3),
                data: Data::Bytes(b"hello".to_vec()),
            },
            config(1, 4, voters(&[1], &[])),
        ]
    );
    raft.propose(b"world".to_vec()).unwrap();
    disk.store(raft.ready());
    assert_eq!(disk.committed(), 5);
}

#[test]
fn promotable() {
    for (voters, promotable) in [
        (&[1][..], true),
        (&[1, 2, 3], true),
        (&[], false),
        (&[2, 3], false),
    ] {
        let (mut raft, _) = start(1, voters, 5, Hard::default(), vec![], 0);
        for _ in 0..10 {
            raft.tick(0);
        }
        assert_eq!(raft.role() != Role::Follower, promotable, "{voters:?}");
    }
}
