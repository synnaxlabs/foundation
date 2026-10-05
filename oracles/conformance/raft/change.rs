//! Membership change scenarios ported from the tests of etcd/raft (Copyright 2015
//! The etcd Authors, Apache License 2.0, see `LICENSE`). This file is modified from
//! the etcd source: `README.md` lists each source and the changes. etcd applies a
//! configuration when the caller applies its entry; here a node uses it from the
//! time it writes the entry, and the leader writes the leave on its own.

use std::collections::BTreeSet;

use raft::{Body, Data, Entry, Error, Hard, Message, Role, Term, Voters};
use types::node;

use crate::common::{ELECTION, at_term, key, start};
use crate::replication::{accept_all, elect, leader, noop, position};

fn set(ids: &[u8]) -> BTreeSet<node::Key> {
    ids.iter().copied().map(key).collect()
}

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

fn reply(from: u8, term: u64, last: u64) -> Message {
    Message {
        from: key(from),
        to: key(1),
        term: Term(term),
        body: Body::AppendReply { last },
    }
}

/// `TestStepConfig`: a change adds one entry, and the change is pending.
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

/// `TestStepIgnoreConfig`: a second change while the first is uncommitted. etcd
/// turns it into an empty entry; here it is an error and the log does not change.
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

/// `TestNewLeaderPendingConfig`: etcd blocks a change until the new leader commits
/// an entry of its term. Here only an uncommitted configuration entry blocks one:
/// a new leader with a plain entry in its log takes a change at once.
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

/// `TestAddNode`: a group of one adds a node. etcd applies the change directly;
/// here node 2 acknowledges the joint entry and the leave.
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

/// `TestAddNodeCheckQuorum`: a node a change adds counts as active until the next
/// quorum check, so the leader does not step down at once.
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

/// `TestRemoveNode`: removing a node leaves the voters without it. etcd panics on
/// removing the last voter; here the proposal is `Error::NoVoters`.
#[test]
fn remove_node() {
    let (mut raft, mut disk) = leader(2);
    disk.store(raft.ready());
    raft.propose_voters(set(&[1])).unwrap();
    disk.store(raft.ready());
    raft.step(reply(2, 1, 2)).unwrap();
    assert_eq!(raft.voters(), &voters(&[1], &[]));
    assert_eq!(disk.store(raft.ready()).len(), 1);
    assert_eq!(disk.committed(), 3);
    assert_eq!(raft.propose_voters(set(&[])), Err(Error::NoVoters));
}

/// `TestCommitAfterRemoveNode`: a proposal made while a removal is pending commits
/// once the removal does. etcd commits it after the caller applies the change;
/// here the leave is in force at once, so both commit in one step.
#[test]
fn commit_after_remove_node() {
    let (mut raft, mut disk) = leader(2);
    disk.store(raft.ready());
    let joint = raft.propose_voters(set(&[1])).unwrap();
    disk.store(raft.ready());
    assert_eq!(disk.committed(), 0);
    raft.propose(b"hello".to_vec()).unwrap();
    raft.step(reply(2, 1, joint.index)).unwrap();
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

/// `TestPromotable`: a node campaigns only when it is one of its own voters.
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
