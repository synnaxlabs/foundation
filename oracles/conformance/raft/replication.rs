//! Replication scenarios ported from the tests of etcd/raft (Copyright 2015 The
//! etcd Authors, Apache License 2.0, see `LICENSE`). This file is modified from the
//! etcd source: `README.md` lists each source and the changes.

use raft::{
    Body, Data, Entry, Error, Hard, Message, Position, Raft, Ready, Role, Term,
};

use crate::common::{
    Disk, ELECTION, Network, accept, accept_all, at_term, count, elect, key, leader,
    noop, position, reply, start,
};

/// A log with one entry per term in `terms`, from index 1, with no data.
fn log(terms: &[u64]) -> Vec<Entry> {
    terms
        .iter()
        .zip(1..)
        .map(|(&term, index)| noop(term, index))
        .collect()
}

fn entry(term: u64, index: u64, data: &[u8]) -> Entry {
    Entry {
        at: position(term, index),
        data: Data::Bytes(data.to_vec()),
    }
}

/// etcd's `commitNoopEntry`: the followers accept the leader's first entry.
fn commit_noop(raft: &mut Raft, disk: &mut Disk) {
    for message in disk.store(raft.ready()) {
        let Body::Append { entries, .. } = &message.body else {
            panic!("not a message to append noop entry");
        };
        assert!(entries.len() == 1 && entries[0].data == Data::Empty);
        raft.step(accept(&message)).unwrap();
    }
    disk.store(raft.ready());
}

fn append(term: u64, prev: Position, entries: Vec<Entry>, commit: u64) -> Message {
    reply(
        2,
        term,
        Body::Append {
            prev,
            entries,
            commit,
        },
    )
}

#[test]
fn leader_start_replication() {
    let (mut raft, mut disk) = leader(3);
    commit_noop(&mut raft, &mut disk);
    let li = disk.last();
    let at = raft.propose(b"some data".to_vec()).unwrap();
    assert_eq!(at, position(1, li + 1));
    let ready = raft.ready();
    assert_eq!(ready.entries, [entry(1, li + 1, b"some data")]);
    assert!(ready.committed.is_empty());
    let messages = disk.store(ready);
    assert_eq!(disk.committed(), li);
    let expected: Vec<Message> = [2, 3]
        .map(|to| Message {
            from: key(1),
            to: key(to),
            term: Term(1),
            body: Body::Append {
                prev: position(1, li),
                entries: vec![entry(1, li + 1, b"some data")],
                commit: li,
            },
            proof: None,
        })
        .to_vec();
    assert_eq!(messages, expected);
}

#[test]
fn leader_commit_entry() {
    let (mut raft, mut disk) = leader(3);
    commit_noop(&mut raft, &mut disk);
    let li = disk.last();
    raft.propose(b"some data".to_vec()).unwrap();
    for message in disk.store(raft.ready()) {
        raft.step(accept(&message)).unwrap();
    }
    let ready = raft.ready();
    assert_eq!(ready.committed, [entry(1, li + 1, b"some data")]);
    let messages = disk.store(ready);
    assert_eq!(disk.committed(), li + 1);
    assert_eq!(messages.len(), 2);
    for (i, message) in messages.iter().enumerate() {
        assert_eq!(message.to, key(u8::try_from(i).unwrap() + 2));
        let Body::Append { commit, .. } = &message.body else {
            panic!("{:?}", message.body);
        };
        assert_eq!(*commit, li + 1);
    }
}

#[test]
fn leader_acknowledge_commit() {
    let cases: [(u8, &[u8], bool); 9] = [
        (1, &[], true),
        (3, &[], false),
        (3, &[2], true),
        (3, &[2, 3], true),
        (5, &[], false),
        (5, &[2], false),
        (5, &[2, 3], true),
        (5, &[2, 3, 4], true),
        (5, &[2, 3, 4, 5], true),
    ];
    for (i, (size, acceptors, acked)) in cases.into_iter().enumerate() {
        let (mut raft, mut disk) = leader(size);
        commit_noop(&mut raft, &mut disk);
        let li = disk.last();
        raft.propose(b"some data".to_vec()).unwrap();
        for message in disk.store(raft.ready()) {
            if acceptors.iter().any(|&id| key(id) == message.to) {
                raft.step(accept(&message)).unwrap();
            }
        }
        disk.store(raft.ready());
        assert_eq!(disk.committed() > li, acked, "#{i}");
    }
}

/// etcd's leader sends both entries in one `Append`, because its test skips the
/// first replication. Ours probes with the first entry, so the followers accept
/// until the leader is quiet.
#[test]
fn leader_commit_preceding_entries() {
    let cases: [&[u64]; 4] = [&[], &[2], &[1, 2], &[1]];
    for (i, terms) in cases.into_iter().enumerate() {
        let (mut raft, mut disk) =
            start(1, &[1, 2, 3], ELECTION, at_term(2), log(terms), 0);
        elect(&mut raft, &mut disk, &[2]);
        raft.propose(b"some data".to_vec()).unwrap();
        accept_all(&mut raft, &mut disk);
        let li = count(terms.len());
        let mut expected = log(terms);
        expected.push(noop(3, li + 1));
        expected.push(entry(3, li + 2, b"some data"));
        assert_eq!(disk.committed, expected, "#{i}");
    }
}

#[test]
fn follower_commit_entry() {
    let cases = [
        (vec![entry(1, 1, b"some data")], 1),
        (
            vec![entry(1, 1, b"some data"), entry(1, 2, b"some data2")],
            2,
        ),
        (
            vec![entry(1, 1, b"some data2"), entry(1, 2, b"some data")],
            2,
        ),
        (
            vec![entry(1, 1, b"some data"), entry(1, 2, b"some data2")],
            1,
        ),
    ];
    for (i, (entries, commit)) in cases.into_iter().enumerate() {
        let (mut raft, mut disk) =
            start(1, &[1, 2, 3], ELECTION, Hard::default(), vec![], 0);
        raft.step(append(1, Position::default(), entries.clone(), commit))
            .unwrap();
        disk.store(raft.ready());
        assert_eq!(disk.committed(), commit, "#{i}");
        let committed = usize::try_from(commit).unwrap();
        assert_eq!(disk.committed, entries[..committed], "#{i}");
    }
}

/// A rejection is its own body with the hint, where etcd has a `Reject` flag and a
/// `RejectHint` field.
#[test]
fn follower_check_msg_app() {
    let cases = [
        (position(0, 0), Body::AppendReply { last: 1 }),
        (position(1, 1), Body::AppendReply { last: 1 }),
        (position(2, 2), Body::AppendReply { last: 2 }),
        (position(1, 2), Body::AppendReject { hint: 1 }),
        (position(3, 3), Body::AppendReject { hint: 2 }),
    ];
    for (i, (prev, body)) in cases.into_iter().enumerate() {
        let (mut raft, mut disk) =
            start(1, &[1, 2, 3], ELECTION, at_term(2), log(&[1, 2]), 1);
        raft.step(append(2, prev, vec![], 0)).unwrap();
        let expected = Message {
            from: key(1),
            to: key(2),
            term: Term(2),
            body,
            proof: None,
        };
        assert_eq!(disk.store(raft.ready()), [expected], "#{i}");
    }
}

/// etcd sends each append at term 2. Ours refuses an entry above the message's
/// term with `Error::TermBehindLog`, then takes the same append at the term of its
/// last entry.
#[test]
fn follower_append_entries() {
    // The term of the last entry, `prev`, the entries, the terms on disk, whether
    // the entries are unstable, and the `last` of the answer.
    type Case = (u64, Position, Vec<Entry>, &'static [u64], bool, u64);
    let cases: [Case; 4] = [
        (3, position(2, 2), log3(&[3]), &[1, 2, 3], true, 3),
        (4, position(1, 1), log2(&[3, 4]), &[1, 3, 4], true, 3),
        (2, position(0, 0), log(&[1]), &[1, 2], false, 1),
        (3, position(0, 0), log(&[3]), &[3], true, 1),
    ];
    for (i, case) in cases.into_iter().enumerate() {
        let (term, prev, entries, terms, unstable, last) = case;
        let unstable = if unstable { entries.clone() } else { vec![] };
        let (mut raft, mut disk) =
            start(1, &[1, 2, 3], ELECTION, at_term(2), log(&[1, 2]), 0);
        if term > 2 {
            let refused = Error::TermBehindLog {
                term: Term(2),
                last: entries.last().unwrap().at,
            };
            let etcd = append(2, prev, entries.clone(), 0);
            assert_eq!(raft.step(etcd), Err(refused), "#{i}");
            assert_eq!(raft.ready(), Ready::default(), "#{i}");
        }
        raft.step(append(term, prev, entries, 0)).unwrap();
        let ready = raft.ready();
        assert_eq!(ready.entries, unstable, "#{i}");
        let answer = Message {
            from: key(1),
            to: key(2),
            term: Term(term),
            body: Body::AppendReply { last },
            proof: None,
        };
        assert_eq!(disk.store(ready), [answer], "#{i}");
        assert_eq!(disk.hard.term, Term(term), "#{i}");
        assert_eq!(disk.entries, log(terms), "#{i}");
    }
}

fn log2(terms: &[u64]) -> Vec<Entry> {
    log_from(2, terms)
}

fn log3(terms: &[u64]) -> Vec<Entry> {
    log_from(3, terms)
}

fn log_from(index: u64, terms: &[u64]) -> Vec<Entry> {
    terms
        .iter()
        .zip(index..)
        .map(|(&term, index)| noop(term, index))
        .collect()
}

/// etcd's node 3 answers only the vote. With PreVote always on, it also answers the
/// pre-vote, and both answers are late when node 2 already gave its vote.
#[test]
fn leader_sync_follower_log() {
    let entries = log(&[1, 1, 1, 4, 4, 5, 5, 6, 6, 6]);
    let term = 8;
    let cases: [&[u64]; 6] = [
        &[1, 1, 1, 4, 4, 5, 5, 6, 6],
        &[1, 1, 1, 4, 4],
        &[1, 1, 1, 4, 4, 5, 5, 6, 6, 6, 6],
        &[1, 1, 1, 4, 4, 5, 5, 6, 6, 6, 7, 7],
        &[1, 1, 1, 4, 4, 4, 4],
        &[1, 1, 1, 2, 2, 2, 3, 3, 3, 3, 3],
    ];
    for (i, terms) in cases.into_iter().enumerate() {
        let voters = [1, 2, 3];
        let last = count(entries.len());
        let lead = start(1, &voters, ELECTION, at_term(term), entries.clone(), last);
        let follower = start(2, &voters, ELECTION, at_term(term - 1), log(terms), 0);
        let mut network = Network::new([lead, follower]);
        network.campaign(&[1]);
        let from_3 = |body| Message {
            from: key(3),
            to: key(1),
            term: Term(term + 1),
            body,
            proof: None,
        };
        network.send(from_3(Body::PreVoteReply { granted: true }));
        network.send(from_3(Body::VoteReply { granted: true }));
        network.propose(1, b"");
        network.check(1, Role::Leader, term + 1);
        assert_eq!(network.disk(1).entries, network.disk(2).entries, "#{i}");
        assert_eq!(network.disk(1).committed, network.disk(2).committed, "#{i}");
    }
}

/// etcd calls the handler directly, so the term of the message is not read; here
/// every message carries the follower's term.
#[test]
fn handle_msg_app() {
    let cases = [
        (position(3, 2), vec![], 3, 2, 0, true),
        (position(3, 3), vec![], 3, 2, 0, true),
        (position(1, 1), vec![], 1, 2, 1, false),
        (position(0, 0), log(&[2]), 1, 1, 1, false),
        (position(2, 2), log3(&[2, 2]), 3, 4, 3, false),
        (position(2, 2), log3(&[2]), 4, 3, 3, false),
        (position(1, 1), log2(&[2]), 4, 2, 2, false),
        (position(1, 1), vec![], 3, 2, 1, false),
        (position(1, 1), log2(&[2]), 3, 2, 2, false),
        (position(2, 2), vec![], 3, 2, 2, false),
        (position(2, 2), vec![], 4, 2, 2, false),
    ];
    for (i, (prev, entries, commit, last, committed, rejected)) in
        cases.into_iter().enumerate()
    {
        let (mut raft, mut disk) =
            start(1, &[1, 2], ELECTION, at_term(2), log(&[1, 2]), 0);
        raft.step(append(2, prev, entries, commit)).unwrap();
        let messages = disk.store(raft.ready());
        assert_eq!((disk.last(), disk.committed()), (last, committed), "#{i}");
        let [message] = &messages[..] else {
            panic!("#{i}: {messages:?}");
        };
        let got = matches!(message.body, Body::AppendReject { .. });
        assert_eq!(got, rejected, "#{i}");
    }
}

/// etcd's follower is at term 2 with an entry of term 3. Ours refuses a term behind
/// its log, so the follower and the heartbeat are at term 3.
#[test]
fn handle_heartbeat() {
    let commit = 2;
    for (i, (sent, expected)) in [(commit + 1, commit + 1), (commit - 1, commit)]
        .into_iter()
        .enumerate()
    {
        let (mut raft, mut disk) =
            start(1, &[1, 2], 5, at_term(3), log(&[1, 2, 3]), commit);
        raft.step(reply(2, 3, Body::Heartbeat { commit: sent }))
            .unwrap();
        let messages = disk.store(raft.ready());
        assert_eq!(disk.committed(), expected, "#{i}");
        let [message] = &messages[..] else {
            panic!("#{i}: {messages:?}");
        };
        assert_eq!(message.body, Body::HeartbeatReply, "#{i}");
    }
}

#[test]
fn handle_heartbeat_resp() {
    let (mut raft, mut disk) = start(1, &[1, 2], 5, at_term(3), log(&[1, 2, 3]), 3);
    elect(&mut raft, &mut disk, &[2]);
    disk.store(raft.ready());
    let mut messages = vec![];
    for _ in 0..2 {
        raft.step(reply(2, 4, Body::HeartbeatReply)).unwrap();
        messages = disk.store(raft.ready());
        let [message] = &messages[..] else {
            panic!("{messages:?}");
        };
        assert!(matches!(message.body, Body::Append { .. }), "{message:?}");
    }
    raft.step(accept(&messages[0])).unwrap();
    disk.store(raft.ready());
    raft.step(reply(2, 4, Body::HeartbeatReply)).unwrap();
    assert_eq!(disk.store(raft.ready()), []);
}

#[test]
fn msg_app_resp_wait_reset() {
    let (mut raft, mut disk) = leader(3);
    disk.store(raft.ready());
    raft.step(reply(2, 1, Body::AppendReply { last: 1 }))
        .unwrap();
    disk.store(raft.ready());
    assert_eq!(disk.committed(), 1);
    raft.propose(vec![]).unwrap();
    for to in [2, 3] {
        let messages = disk.store(raft.ready());
        let [message] = &messages[..] else {
            panic!("{messages:?}");
        };
        assert_eq!(message.to, key(to));
        let Body::Append { entries, .. } = &message.body else {
            panic!("{:?}", message.body);
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].at.index, 2);
        raft.step(reply(3, 1, Body::AppendReply { last: 1 }))
            .unwrap();
    }
}

#[test]
fn leader_only_commits_log_from_current_term() {
    for (i, (index, committed)) in [(1, 0), (2, 0), (3, 3)].into_iter().enumerate() {
        let (mut raft, mut disk) =
            start(1, &[1, 2], ELECTION, at_term(2), log(&[1, 2]), 0);
        elect(&mut raft, &mut disk, &[2]);
        raft.propose(vec![]).unwrap();
        disk.store(raft.ready());
        raft.step(reply(2, 3, Body::AppendReply { last: index }))
            .unwrap();
        disk.store(raft.ready());
        assert_eq!(disk.committed(), committed, "#{i}");
    }
}

/// etcd runs this network without CheckQuorum. Here the followers tick to the end of
/// their lease, short of their own timeout, before node 2 campaigns.
#[test]
fn log_replication() {
    for (i, (second_leader, committed)) in
        [(false, 2), (true, 4)].into_iter().enumerate()
    {
        let mut network = Network::of(3, &[1, 2, 3], &Hard::default());
        network.campaign(&[1]);
        network.propose(1, b"somedata");
        let mut proposed = vec![b"somedata".to_vec()];
        if second_leader {
            for id in [2, 3] {
                network.tick(id, 1, ELECTION);
            }
            network.campaign(&[2]);
            network.propose(2, b"somedata");
            proposed.push(b"somedata".to_vec());
        }
        for id in [1, 2, 3] {
            let disk = network.disk(id);
            assert_eq!(disk.committed(), committed, "#{i}.{id}");
            let data: Vec<&Vec<u8>> = disk
                .committed
                .iter()
                .filter_map(|entry| match &entry.data {
                    Data::Bytes(data) => Some(data),
                    Data::Empty | Data::Voters(_) => None,
                })
                .collect();
            assert_eq!(data, proposed.iter().collect::<Vec<_>>(), "#{i}.{id}");
        }
    }
}
