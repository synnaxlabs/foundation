//! The test network and the node disks that the scenarios share, ported from the
//! tests of etcd/raft (Copyright 2015 The etcd Authors, Apache License 2.0, see
//! `LICENSE`). This file is modified from the etcd source.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use raft::{
    Body, Config, Data, Entry, Hard, Message, Position, Raft, Ready, Role, Start, Term,
    Voters,
};
use types::node;

pub(crate) const ELECTION: u32 = 10;

pub(crate) fn key(id: u8) -> node::Key {
    node::Key::from_u128(u128::from(id))
}

pub(crate) fn at_term(term: u64) -> Hard {
    Hard {
        term: Term(term),
        vote: None,
        leader: None,
        proof: None,
    }
}

/// Node `id` with a log of `last.index` entries, all in `last.term`, and its disk.
pub(crate) fn build(
    id: u8,
    voters: &[u8],
    election: u32,
    hard: Hard,
    last: Position,
) -> (Raft, Disk) {
    let entries = (1..=last.index)
        .map(|index| Entry {
            at: Position {
                term: last.term,
                index,
            },
            data: Data::Empty,
        })
        .collect();
    start(id, voters, election, hard, entries, 0)
}

/// What one node has stored: its log and the entries it applied, in order.
#[derive(Debug, Default)]
pub(crate) struct Disk {
    pub(crate) hard: Hard,
    pub(crate) entries: Vec<Entry>,
    pub(crate) committed: Vec<Entry>,
}

impl Disk {
    /// Does what a `Ready` asks for and returns its messages.
    pub(crate) fn store(&mut self, ready: Ready) -> Vec<Message> {
        let Ready {
            hard,
            entries,
            committed,
            messages,
        } = ready;
        if let Some(hard) = hard {
            self.hard = hard;
        }
        if let Some(first) = entries.first() {
            let keep = usize::try_from(first.at.index - 1).unwrap();
            assert!(
                keep <= self.entries.len(),
                "a write past the end of the log"
            );
            self.entries.truncate(keep);
            self.entries.extend(entries);
        }
        self.committed.extend(committed);
        messages
    }

    pub(crate) fn last(&self) -> u64 {
        count(self.entries.len())
    }

    pub(crate) fn committed(&self) -> u64 {
        count(self.committed.len())
    }
}

pub(crate) fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap()
}

/// Node `id` with the stored state `hard`, the log `entries`, and `applied` of them
/// applied, with the disk that holds the same.
pub(crate) fn start(
    id: u8,
    voters: &[u8],
    election: u32,
    hard: Hard,
    entries: Vec<Entry>,
    applied: u64,
) -> (Raft, Disk) {
    let config = Config {
        key: key(id),
        election_ticks: election,
        heartbeat_ticks: 1,
    };
    let start = Start {
        hard: hard.clone(),
        voters: Voters {
            incoming: voters.iter().copied().map(key).collect(),
            ..Voters::default()
        },
        entries: entries.clone(),
        applied,
    };
    let disk = Disk {
        hard,
        committed: entries[..usize::try_from(applied).unwrap()].to_vec(),
        entries,
    };
    (Raft::new(config, start).unwrap(), disk)
}

/// The etcd test network: it delivers messages in order until none remain. A voter
/// with no peer never answers.
pub(crate) struct Network {
    peers: BTreeMap<node::Key, Raft>,
    disks: BTreeMap<node::Key, Disk>,
    cuts: BTreeSet<(node::Key, node::Key)>,
}

impl Network {
    pub(crate) fn new(peers: impl IntoIterator<Item = (Raft, Disk)>) -> Self {
        let (peers, disks) = peers
            .into_iter()
            .map(|(peer, disk)| {
                let key = peer.key();
                ((key, peer), (key, disk))
            })
            .unzip();
        Self {
            peers,
            disks,
            cuts: BTreeSet::new(),
        }
    }

    /// `size` voters, all with the stored state `hard`. Only `present` have a peer.
    pub(crate) fn of(size: u8, present: &[u8], hard: &Hard) -> Self {
        let voters: Vec<u8> = (1..=size).collect();
        Self::new(
            present.iter().map(|&id| {
                build(id, &voters, ELECTION, hard.clone(), Position::default())
            }),
        )
    }

    pub(crate) fn peer(&self, id: u8) -> &Raft {
        &self.peers[&key(id)]
    }

    /// Makes each node campaign, then delivers until quiet.
    pub(crate) fn campaign(&mut self, ids: &[u8]) {
        let mut queue = VecDeque::new();
        for &id in ids {
            self.peers.get_mut(&key(id)).unwrap().campaign();
            queue.extend(self.take(key(id)));
        }
        self.run(queue);
    }

    /// Delivers one message, then delivers until quiet.
    pub(crate) fn send(&mut self, message: Message) {
        self.run(VecDeque::from([message]));
    }

    /// Ticks one node. Its messages wait until the node next handles a message.
    pub(crate) fn tick(&mut self, id: u8, random: u64, times: u32) {
        let peer = self.peers.get_mut(&key(id)).unwrap();
        for _ in 0..times {
            peer.tick(random);
        }
    }

    /// Delivers what a node has waiting, then delivers until quiet.
    pub(crate) fn flush(&mut self, id: u8) {
        let queue = self.take(key(id)).into();
        self.run(queue);
    }

    pub(crate) fn cut(&mut self, a: u8, b: u8) {
        self.cuts.insert((key(a), key(b)));
        self.cuts.insert((key(b), key(a)));
    }

    pub(crate) fn isolate(&mut self, id: u8) {
        for other in 1..=u8::MAX {
            if other != id {
                self.cut(id, other);
            }
        }
    }

    pub(crate) fn recover(&mut self) {
        self.cuts.clear();
    }

    fn take(&mut self, id: node::Key) -> Vec<Message> {
        let ready = self.peers.get_mut(&id).unwrap().ready();
        let mut messages = self.disks.get_mut(&id).unwrap().store(ready);
        messages.retain(|message| !self.cuts.contains(&(message.from, message.to)));
        messages
    }

    fn run(&mut self, mut queue: VecDeque<Message>) {
        while let Some(message) = queue.pop_front() {
            let Some(peer) = self.peers.get_mut(&message.to) else {
                continue;
            };
            let to = message.to;
            peer.step(message).unwrap();
            queue.extend(self.take(to));
        }
    }

    #[track_caller]
    pub(crate) fn check(&self, id: u8, role: Role, term: u64) {
        let peer = self.peer(id);
        assert_eq!((peer.role(), peer.term()), (role, Term(term)), "node {id}");
    }

    /// Proposes `data` to a node, then delivers until quiet.
    pub(crate) fn propose(&mut self, id: u8, data: &[u8]) {
        let peer = self.peers.get_mut(&key(id)).unwrap();
        peer.propose(data.to_vec()).unwrap();
        self.flush(id);
    }

    pub(crate) fn disk(&self, id: u8) -> &Disk {
        &self.disks[&key(id)]
    }
}

pub(crate) fn heartbeat(from: u8, to: u8, term: Term) -> Message {
    Message {
        from: key(from),
        to: key(to),
        term,
        body: Body::Heartbeat { commit: 0 },
        proof: None,
    }
}

/// A leader's first entry of its term; etcd's noop entry.
pub(crate) fn noop(term: u64, index: u64) -> Entry {
    Entry {
        at: position(term, index),
        data: Data::Empty,
    }
}

/// Elects node 1 with the votes of `others`. The leader's first `Ready` is left for
/// the caller. etcd's tests call `becomeLeader` and send nothing.
pub(crate) fn elect(raft: &mut Raft, disk: &mut Disk, others: &[u8]) {
    let term = Term(raft.term().0 + 1);
    raft.campaign();
    disk.store(raft.ready());
    for granted in [
        Body::PreVoteReply { granted: true },
        Body::VoteReply { granted: true },
    ] {
        let vote = granted == Body::VoteReply { granted: true };
        for &from in others {
            raft.step(Message {
                from: key(from),
                to: key(1),
                term,
                body: granted.clone(),
                proof: None,
            })
            .unwrap();
        }
        if !vote {
            disk.store(raft.ready());
        }
    }
    assert_eq!(raft.role(), Role::Leader);
}

/// Node 1 at term 1 as the leader of `size` voters, with its first `Ready` left.
pub(crate) fn leader(size: u8) -> (Raft, Disk) {
    let voters: Vec<u8> = (1..=size).collect();
    let others: Vec<u8> = (2..=size / 2 + 1).collect();
    let (mut raft, mut disk) = start(1, &voters, ELECTION, Hard::default(), vec![], 0);
    elect(&mut raft, &mut disk, &others);
    (raft, disk)
}

/// etcd's `acceptAndReply`: the reply of a follower that took an `Append`.
pub(crate) fn accept(message: &Message) -> Message {
    let Body::Append { prev, entries, .. } = &message.body else {
        panic!("type should be Append");
    };
    Message {
        from: message.to,
        to: message.from,
        term: message.term,
        body: Body::AppendReply {
            last: prev.index + count(entries.len()),
        },
        proof: None,
    }
}

/// Every follower accepts each `Append` until the leader sends none.
pub(crate) fn accept_all(raft: &mut Raft, disk: &mut Disk) {
    loop {
        let messages = disk.store(raft.ready());
        let appends: Vec<&Message> = messages
            .iter()
            .filter(|message| matches!(message.body, Body::Append { .. }))
            .collect();
        if appends.is_empty() {
            return;
        }
        for message in appends {
            raft.step(accept(message)).unwrap();
        }
    }
}

pub(crate) fn reply(from: u8, term: u64, body: Body) -> Message {
    Message {
        from: key(from),
        to: key(1),
        term: Term(term),
        body,
        proof: None,
    }
}

pub(crate) fn position(term: u64, index: u64) -> Position {
    Position {
        term: Term(term),
        index,
    }
}

pub(crate) fn set(ids: &[u8]) -> BTreeSet<node::Key> {
    ids.iter().copied().map(key).collect()
}
