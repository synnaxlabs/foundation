use std::collections::BTreeMap;
use std::ops::RangeBounds;

use types::node;

use crate::log;
use crate::log::Log;
use crate::progress::Progress;
use crate::voters::Tally;
use crate::{
    Body, Config, Data, Entry, Error, Hard, Message, Position, Start, Term, Voters,
};

/// What a node is doing in its term.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// Follows a leader, or waits for one.
    Follower,
    /// Asks whether it could win an election, before it disturbs a term.
    PreCandidate,
    /// Asks for votes in its term.
    Candidate,
    /// Leads its term.
    Leader,
}

// Entries in one `Append`.
const BATCH: usize = 64;

// One voter: the leader's view of its log, its answer to the current campaign, and
// whether it answered this leader since the last quorum check.
#[derive(Debug)]
struct Peer {
    progress: Progress,
    vote: Option<bool>,
    active: bool,
}

impl Peer {
    fn new(last: u64) -> Self {
        Self {
            progress: Progress::new(last),
            vote: None,
            active: false,
        }
    }
}

/// What the caller must do after an input, in this order: write `hard` and `entries`
/// to disk, send `messages`, then apply `committed`.
#[must_use = "a dropped Ready loses its messages and its hard state"]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ready {
    /// The hard state, if it changed since the last `Ready`.
    pub hard: Option<Hard>,
    /// Entries to write, in place of any entry at or after the first one's index.
    pub entries: Vec<Entry>,
    /// Entries that a quorum holds, in order, each given once.
    pub committed: Vec<Entry>,
    /// Messages to send after the write, in the order the node made them.
    pub messages: Vec<Message>,
    /// The voters, when a configuration entry committed since the last `Ready`.
    /// Store it with `applied`: it is `Start.voters` after a restart.
    pub voters: Option<Voters>,
}

/// One node's state machine. PreVote and CheckQuorum are always on.
///
/// After each call to [`tick`](Self::tick), [`step`](Self::step), or
/// [`campaign`](Self::campaign), take [`ready`](Self::ready) and do what it says.
#[derive(Debug)]
pub struct Raft {
    key: node::Key,
    voters: Voters,
    // A configuration entry committed since the last `Ready`.
    reconfigured: bool,
    peers: BTreeMap<node::Key, Peer>,
    election_ticks: u64,
    heartbeat_ticks: u64,
    term: Term,
    vote: Option<node::Key>,
    // The hard state that the last `Ready` gave.
    given: Hard,
    log: Log,
    role: Role,
    leader: Option<node::Key>,
    election_elapsed: u64,
    heartbeat_elapsed: u64,
    // The randomized election timeout. `None` until the first tick after a reset.
    timeout: Option<u64>,
    outbox: Vec<Message>,
}

impl Raft {
    /// Builds a follower.
    ///
    /// # Errors
    ///
    /// - [`Error::Ticks`] when `heartbeat_ticks` is 0 or `election_ticks` is not
    ///   greater than `heartbeat_ticks`.
    /// - [`Error::EmptyIncoming`] when `voters.incoming` is empty but `outgoing` is
    ///   not.
    /// - [`Error::EntryOutOfOrder`] when `entries` do not run from index 1 with
    ///   terms that never decrease.
    /// - [`Error::AppliedPastLog`] when `applied` is past the last entry.
    /// - [`Error::TermBehindLog`] when `hard.term` is lower than the last entry's term.
    pub fn new(config: Config, start: Start) -> Result<Self, Error> {
        let Config {
            key,
            election_ticks,
            heartbeat_ticks,
        } = config;
        let Start {
            hard,
            voters,
            entries,
            applied,
        } = start;
        if heartbeat_ticks == 0 || election_ticks <= heartbeat_ticks {
            return Err(Error::Ticks {
                election: election_ticks,
                heartbeat: heartbeat_ticks,
            });
        }
        voters.check()?;
        let log = Log::new(entries, applied)?;
        let last = log.last();
        let peers = voters
            .peers()
            .map(|key| (key, Peer::new(last.index)))
            .collect();
        if hard.term < last.term {
            return Err(Error::TermBehindLog {
                term: hard.term,
                last,
            });
        }
        Ok(Self {
            key,
            voters,
            reconfigured: false,
            peers,
            outbox: Vec::new(),
            election_ticks: u64::from(election_ticks),
            heartbeat_ticks: u64::from(heartbeat_ticks),
            term: hard.term,
            vote: hard.vote,
            given: hard,
            log,
            role: Role::Follower,
            leader: None,
            election_elapsed: 0,
            heartbeat_elapsed: 0,
            timeout: None,
        })
    }

    /// This node.
    #[must_use]
    pub fn key(&self) -> node::Key {
        self.key
    }

    /// The node's current term.
    #[must_use]
    pub fn term(&self) -> Term {
        self.term
    }

    /// What the node is doing in its term.
    #[must_use]
    pub fn role(&self) -> Role {
        self.role
    }

    /// The leader of the current term, when the node knows it. A leader returns
    /// itself.
    #[must_use]
    pub fn leader(&self) -> Option<node::Key> {
        self.leader
    }

    /// The nodes whose votes count.
    #[must_use]
    pub fn voters(&self) -> &Voters {
        &self.voters
    }

    /// The term and vote that must be on disk before a message of this term leaves.
    /// [`Ready::hard`] says when to write it.
    #[must_use]
    pub fn hard(&self) -> Hard {
        Hard {
            term: self.term,
            vote: self.vote,
        }
    }

    /// Takes what the caller must do since the last call.
    pub fn ready(&mut self) -> Ready {
        let hard = self.hard();
        let changed = hard != self.given;
        self.given = hard;
        Ready {
            hard: changed.then_some(hard),
            entries: self.log.take_unstable(),
            committed: self.log.take_committed(),
            messages: std::mem::take(&mut self.outbox),
            voters: std::mem::take(&mut self.reconfigured).then(|| self.voters.clone()),
        }
    }

    /// Appends `data` to the log as the leader and starts to replicate it. Returns
    /// the entry's position; it is committed once a quorum holds it.
    ///
    /// # Errors
    ///
    /// [`Error::NotLeader`] when this node does not lead. `leader` names the node
    /// that does, when this node knows it.
    pub fn propose(&mut self, data: Vec<u8>) -> Result<Position, Error> {
        self.propose_entry(Data::Bytes(data))
    }

    fn propose_entry(&mut self, data: Data) -> Result<Position, Error> {
        if self.role != Role::Leader {
            return Err(Error::NotLeader {
                leader: self.leader,
            });
        }
        let at = self.log.push(self.term, data);
        self.commit();
        self.replicate();
        Ok(at)
    }

    /// Moves the node's time forward by one tick. `random` is a fresh, uniformly
    /// random value. The node uses it to choose its next election timeout.
    ///
    /// A follower or candidate that reaches its election timeout starts an election. A
    /// leader sends heartbeats, and steps down when it has not heard from a quorum for
    /// `election_ticks`.
    pub fn tick(&mut self, random: u64) {
        self.election_elapsed += 1;
        if self.role == Role::Leader {
            self.tick_leader();
            return;
        }
        let jitter = random % self.election_ticks;
        let timeout = *self.timeout.get_or_insert(self.election_ticks + jitter);
        if self.promotable() && self.election_elapsed >= timeout {
            self.election_elapsed = 0;
            self.pre_campaign();
        }
    }

    /// Starts an election now, without a wait for the election timeout. A leader and
    /// a node that is not in its own voter list do nothing.
    pub fn campaign(&mut self) {
        if self.role != Role::Leader && self.promotable() {
            self.pre_campaign();
        }
    }

    /// Handles one message from another node.
    ///
    /// # Errors
    ///
    /// - [`Error::Misrouted`] when the message is for another node.
    /// - [`Error::Loopback`] when the message names this node as its sender.
    /// - [`Error::SecondLeader`] when this node leads the message's term and the
    ///   message is a heartbeat or an append.
    ///
    /// The node's state does not change on an error.
    pub fn step(&mut self, message: Message) -> Result<(), Error> {
        let Message {
            from,
            to,
            term,
            body,
        } = message;
        if to != self.key {
            return Err(Error::Misrouted { to });
        }
        if from == self.key {
            return Err(Error::Loopback);
        }
        if !self.meet(from, term, &body) {
            return Ok(());
        }
        match body {
            Body::PreVote { last } => {
                let granted = (term > self.term || self.free_for(from))
                    && last >= self.log.last();
                let reply = if granted { term } else { self.term };
                self.send(from, reply, Body::PreVoteReply { granted });
            }
            Body::Vote { last } => {
                let granted = self.free_for(from) && last >= self.log.last();
                if granted {
                    self.election_elapsed = 0;
                    self.vote = Some(from);
                }
                self.send(from, self.term, Body::VoteReply { granted });
            }
            Body::PreVoteReply { granted } => {
                if self.role == Role::PreCandidate {
                    self.poll(from, granted);
                }
            }
            Body::VoteReply { granted } => {
                if self.role == Role::Candidate {
                    self.poll(from, granted);
                }
            }
            Body::Heartbeat { commit } => self.heartbeat(from, commit)?,
            Body::HeartbeatReply => {
                let last = self.log.last().index;
                let behind = self.heard_from(from).is_some_and(|peer| {
                    peer.progress.heard();
                    peer.progress.matched() < last
                });
                // A follower that lacks entries gets an append even when every
                // entry is in flight: a lost append is found this way.
                if behind {
                    self.send_appends(from..=from);
                }
            }
            Body::Append {
                prev,
                entries,
                commit,
            } => {
                log::check(&entries, prev)?;
                self.follow(from)?;
                self.append(from, prev, entries, commit);
            }
            Body::AppendReply { last } => self.accepted(from, last),
            Body::AppendReject { hint } => {
                if let Some(peer) = self.heard_from(from) {
                    peer.progress.rejected(hint);
                    self.catch_up(from);
                }
            }
        }
        Ok(())
    }

    // Commits what the leader's heartbeat says and answers it.
    fn heartbeat(&mut self, leader: node::Key, commit: u64) -> Result<(), Error> {
        self.follow(leader)?;
        self.commit_to(commit);
        self.send(leader, self.term, Body::HeartbeatReply);
        Ok(())
    }

    // Appends the leader's entries as a follower and answers.
    fn append(
        &mut self,
        leader: node::Key,
        prev: Position,
        entries: Vec<Entry>,
        commit: u64,
    ) {
        let reply = match self.log.append(prev, entries) {
            Ok(last) => {
                self.commit_to(commit.min(last));
                Body::AppendReply { last }
            }
            Err(hint) => Body::AppendReject { hint },
        };
        self.send(leader, self.term, reply);
    }

    // Records that a follower holds the leader's log up to `last`, as the leader.
    fn accepted(&mut self, from: node::Key, last: u64) {
        let accepted = self
            .heard_from(from)
            .is_some_and(|peer| peer.progress.accepted(last));
        if accepted {
            if self.commit() {
                self.replicate();
            } else {
                self.catch_up(from);
            }
        }
    }

    // Notes that a voter answered this leader. `None` when this node does not lead
    // or `from` is not a voter.
    fn heard_from(&mut self, from: node::Key) -> Option<&mut Peer> {
        if self.role != Role::Leader {
            return None;
        }
        let peer = self.peers.get_mut(&from)?;
        peer.active = true;
        Some(peer)
    }

    // Commits the highest index that a quorum holds, when an entry of the leader's
    // own term is there. Returns whether the commit index moved.
    fn commit(&mut self) -> bool {
        let last = self.log.last().index;
        let index = self.voters.committed(|key| {
            if key == self.key {
                last
            } else {
                self.peer(key).progress.matched()
            }
        });
        let current = self.log.at(index).is_some_and(|at| at.term == self.term);
        if index > self.log.committed() && current {
            self.commit_to(index);
            return true;
        }
        false
    }

    // Raises the commit index to `index` and puts the last configuration it commits
    // in force. A lower `index` changes nothing.
    fn commit_to(&mut self, index: u64) {
        let from = self.log.committed();
        self.log.commit_to(index);
        let to = self.log.committed();
        if let Some(voters) = self.log.last_voters(from, to).cloned() {
            self.set_voters(voters);
        }
    }

    // The one writer of `voters` and `peers` after `new`. A peer that stays keeps
    // its progress; a new one starts at the end of the log.
    fn set_voters(&mut self, voters: Voters) {
        assert!(
            voters.check().is_ok(),
            "invariant: a committed configuration is valid"
        );
        let last = self.log.last().index;
        let mut old = std::mem::take(&mut self.peers);
        self.peers = voters
            .peers()
            .map(|key| (key, old.remove(&key).unwrap_or_else(|| Peer::new(last))))
            .collect();
        self.voters = voters;
        self.reconfigured = true;
    }

    // Sends each follower the entries it lacks and the commit index.
    fn replicate(&mut self) {
        self.send_appends(..);
    }

    // Sends one follower the entries it lacks, when it lacks any.
    fn catch_up(&mut self, to: node::Key) {
        let last = self.log.last().index;
        if self
            .peers
            .get(&to)
            .is_some_and(|peer| peer.progress.behind(last))
        {
            self.send_appends(to..=to);
        }
    }

    // Sends each follower in `range` the entries from its `next` and the commit
    // index, unless the leader waits for its reply.
    fn send_appends<R: RangeBounds<node::Key>>(&mut self, range: R) {
        let commit = self.log.committed();
        for (&to, peer) in self.peers.range_mut(range) {
            if to == self.key || peer.progress.paused() {
                continue;
            }
            let next = peer.progress.next();
            let prev = self
                .log
                .at(next - 1)
                .expect("invariant: a follower's next entry follows the leader's log");
            let entries = self.log.slice(next, BATCH);
            let last = entries.last().map_or(prev.index, |entry| entry.at.index);
            peer.progress.sent(last);
            self.outbox.push(Message {
                from: self.key,
                to,
                term: self.term,
                body: Body::Append {
                    prev,
                    entries,
                    commit,
                },
            });
        }
    }

    // Compares the message's term with the node's term, and steps down for a higher
    // one. Returns whether the message still needs its normal handling.
    fn meet(&mut self, from: node::Key, term: Term, body: &Body) -> bool {
        if term > self.term {
            match body {
                // A voter that heard from a leader within the election timeout does
                // not help to replace it.
                Body::PreVote { .. } | Body::Vote { .. } if self.leased() => {
                    return false;
                }
                // A PreVote, or its grant, carries a term that no node is in yet.
                Body::PreVote { .. } | Body::PreVoteReply { granted: true } => {}
                Body::Heartbeat { .. } | Body::Append { .. } => {
                    self.become_follower(term, Some(from));
                }
                Body::Vote { .. }
                | Body::PreVoteReply { granted: false }
                | Body::VoteReply { .. }
                | Body::HeartbeatReply
                | Body::AppendReply { .. }
                | Body::AppendReject { .. } => self.become_follower(term, None),
            }
        } else if term < self.term {
            match body {
                // The reply carries the higher term, so a stale leader steps down and
                // a node that is ahead of its group can be elected.
                Body::Heartbeat { .. } | Body::Append { .. } => {
                    self.send(from, self.term, Body::HeartbeatReply);
                }
                Body::PreVote { .. } => {
                    self.send(from, self.term, Body::PreVoteReply { granted: false });
                }
                Body::Vote { .. }
                | Body::PreVoteReply { .. }
                | Body::VoteReply { .. }
                | Body::HeartbeatReply
                | Body::AppendReply { .. }
                | Body::AppendReject { .. } => {}
            }
            return false;
        }
        true
    }

    fn tick_leader(&mut self) {
        self.heartbeat_elapsed += 1;
        if self.election_elapsed >= self.election_ticks {
            self.election_elapsed = 0;
            let heard = self
                .voters
                .quorum(|key| key == self.key || self.peer(key).active);
            for peer in self.peers.values_mut() {
                peer.active = false;
            }
            if !heard {
                self.become_follower(self.term, None);
                return;
            }
        }
        if self.heartbeat_elapsed >= self.heartbeat_ticks {
            self.heartbeat_elapsed = 0;
            // A follower commits what the heartbeat says, so it names only entries
            // the follower is known to hold.
            for (&to, peer) in &self.peers {
                if to != self.key {
                    let commit = self.log.committed().min(peer.progress.matched());
                    self.outbox.push(Message {
                        from: self.key,
                        to,
                        term: self.term,
                        body: Body::Heartbeat { commit },
                    });
                }
            }
        }
    }

    // Handles a heartbeat or an append from the leader of the node's own term.
    fn follow(&mut self, leader: node::Key) -> Result<(), Error> {
        match self.role {
            Role::Leader => {
                return Err(Error::SecondLeader {
                    term: self.term,
                    from: leader,
                });
            }
            Role::Follower => {
                self.election_elapsed = 0;
                self.leader = Some(leader);
            }
            Role::PreCandidate | Role::Candidate => {
                self.become_follower(self.term, Some(leader));
            }
        }
        Ok(())
    }

    fn pre_campaign(&mut self) {
        // No term is left to campaign in.
        let Some(next) = self.term.next() else {
            return;
        };
        for peer in self.peers.values_mut() {
            peer.vote = None;
        }
        self.leader = None;
        self.role = Role::PreCandidate;
        self.broadcast(
            next,
            &Body::PreVote {
                last: self.log.last(),
            },
        );
        self.poll(self.key, true);
    }

    fn become_candidate(&mut self) {
        let next = self
            .term
            .next()
            .expect("invariant: a pre-candidate's term has a next term");
        self.reset(next);
        self.vote = Some(self.key);
        self.role = Role::Candidate;
        self.broadcast(
            self.term,
            &Body::Vote {
                last: self.log.last(),
            },
        );
        self.poll(self.key, true);
    }

    fn become_leader(&mut self) {
        self.reset(self.term);
        self.leader = Some(self.key);
        self.role = Role::Leader;
        let last = self.log.last().index;
        for peer in self.peers.values_mut() {
            peer.progress = Progress::new(last);
        }
        // An entry of the leader's own term lets it commit the ones before it.
        self.log.push(self.term, Data::Empty);
        self.commit();
        self.replicate();
    }

    fn become_follower(&mut self, term: Term, leader: Option<node::Key>) {
        self.reset(term);
        self.leader = leader;
        self.role = Role::Follower;
    }

    fn reset(&mut self, term: Term) {
        if self.term != term {
            self.term = term;
            self.vote = None;
        }
        self.leader = None;
        self.election_elapsed = 0;
        self.heartbeat_elapsed = 0;
        self.timeout = None;
        for peer in self.peers.values_mut() {
            peer.vote = None;
            peer.active = false;
        }
    }

    // Records one answer to the current campaign and acts when the answers decide it.
    // The first answer of a voter counts.
    fn poll(&mut self, from: node::Key, granted: bool) {
        let Some(peer) = self.peers.get_mut(&from) else {
            return;
        };
        peer.vote.get_or_insert(granted);
        match self.voters.tally(|key| self.peer(key).vote) {
            Tally::Won => match self.role {
                Role::PreCandidate => self.become_candidate(),
                Role::Candidate => self.become_leader(),
                Role::Follower | Role::Leader => {
                    unreachable!("invariant: only a campaign counts votes")
                }
            },
            Tally::Lost => self.become_follower(self.term, None),
            Tally::Open => {}
        }
    }

    // Reports whether this node may give its vote in the current term to `candidate`.
    fn free_for(&self, candidate: node::Key) -> bool {
        self.vote == Some(candidate) || (self.vote.is_none() && self.leader.is_none())
    }

    fn leased(&self) -> bool {
        self.leader.is_some() && self.election_elapsed < self.election_ticks
    }

    fn promotable(&self) -> bool {
        self.peers.contains_key(&self.key)
    }

    fn peer(&self, key: node::Key) -> &Peer {
        self.peers
            .get(&key)
            .expect("invariant: every voter has a peer")
    }

    fn broadcast(&mut self, term: Term, body: &Body) {
        for &to in self.peers.keys() {
            if to != self.key {
                self.outbox.push(Message {
                    from: self.key,
                    to,
                    term,
                    body: body.clone(),
                });
            }
        }
    }

    fn send(&mut self, to: node::Key, term: Term, body: Body) {
        self.outbox.push(Message {
            from: self.key,
            to,
            term,
            body,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Position;

    fn key(id: u8) -> node::Key {
        node::Key::from_u128(u128::from(id))
    }

    const CONFIG: Config = Config {
        key: node::Key::from_u128(1),
        election_ticks: 10,
        heartbeat_ticks: 1,
    };

    fn start(voters: &[u8], hard: Hard) -> Start {
        Start {
            hard,
            voters: Voters {
                incoming: voters.iter().copied().map(key).collect(),
                ..Voters::default()
            },
            entries: Vec::new(),
            applied: 0,
        }
    }

    fn entries(positions: &[(u64, u64)]) -> Vec<Entry> {
        let entry = |&(term, index)| Entry {
            at: Position {
                term: Term(term),
                index,
            },
            data: Data::Empty,
        };
        positions.iter().map(entry).collect()
    }

    fn raft(voters: &[u8], hard: Hard) -> Raft {
        Raft::new(CONFIG, start(voters, hard)).unwrap()
    }

    fn message(from: u8, term: u64, body: Body) -> Message {
        Message {
            from: key(from),
            to: key(1),
            term: Term(term),
            body,
        }
    }

    fn sent(raft: &mut Raft) -> Vec<Message> {
        raft.ready().messages
    }

    mod ready {
        use super::*;

        #[test]
        fn is_empty_after_a_start() {
            let mut raft = raft(&[1, 2, 3], at_term(2));
            assert_eq!(raft.ready(), Ready::default());
        }

        #[test]
        fn gives_the_hard_state_once_when_it_changes() {
            let mut raft = raft(&[1], Hard::default());
            raft.campaign();
            let hard = Hard {
                term: Term(1),
                vote: Some(key(1)),
            };
            assert_eq!((raft.role(), raft.ready().hard), (Role::Leader, Some(hard)));
            assert_eq!(raft.ready().hard, None);
            raft.tick(0);
            assert_eq!(raft.ready().hard, None);
        }

        #[test]
        fn gives_the_hard_state_when_only_the_vote_changes() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            assert_eq!(raft.ready().hard, None);
            let vote = Body::Vote {
                last: Position::default(),
            };
            raft.step(message(2, 1, vote.clone())).unwrap();
            let hard = Hard {
                term: Term(1),
                vote: Some(key(2)),
            };
            assert_eq!(raft.ready().hard, Some(hard));
            assert_eq!(raft.ready().hard, None);
        }

        #[test]
        fn gives_each_message_once() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            raft.campaign();
            let ready = raft.ready();
            assert_eq!(ready.hard, None);
            assert_eq!(ready.messages.len(), 2);
            assert_eq!(raft.ready(), Ready::default());
        }

        #[test]
        fn campaigns_with_the_last_position_of_the_log_from_disk() {
            let start = Start {
                entries: entries(&[(1, 1), (2, 2), (2, 3)]),
                ..start(&[1, 2], at_term(5))
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            raft.campaign();
            let last = Position {
                term: Term(2),
                index: 3,
            };
            let prevote = Message {
                from: key(1),
                to: key(2),
                term: Term(6),
                body: Body::PreVote { last },
            };
            assert_eq!(raft.ready().messages, [prevote]);
        }
    }

    mod new {
        use super::*;

        #[test]
        fn rejects_zero_heartbeat_ticks() {
            let config = Config {
                heartbeat_ticks: 0,
                ..CONFIG
            };
            let err = Raft::new(config, Start::default()).unwrap_err();
            assert_eq!(
                err,
                Error::Ticks {
                    election: 10,
                    heartbeat: 0
                }
            );
            assert_eq!(
                err.to_string(),
                "election_ticks (10) must be greater than heartbeat_ticks (0), and \
                 heartbeat_ticks must be at least 1"
            );
        }

        #[test]
        fn rejects_election_ticks_equal_to_heartbeat_ticks() {
            let config = Config {
                election_ticks: 3,
                heartbeat_ticks: 3,
                ..CONFIG
            };
            assert_eq!(
                Raft::new(config, Start::default()).unwrap_err(),
                Error::Ticks {
                    election: 3,
                    heartbeat: 3
                }
            );
        }

        #[test]
        fn rejects_a_log_that_is_out_of_order() {
            let start = Start {
                entries: entries(&[(1, 1), (1, 3)]),
                ..start(&[1], at_term(1))
            };
            let err = Raft::new(CONFIG, start).unwrap_err();
            let position = |term, index| Position {
                term: Term(term),
                index,
            };
            let out_of_order = Error::EntryOutOfOrder {
                at: position(1, 3),
                before: position(1, 1),
            };
            assert_eq!(err, out_of_order);
            assert_eq!(
                err.to_string(),
                "log entry at index 3 in term 1 does not follow index 1 in term 1"
            );
        }

        #[test]
        fn rejects_an_applied_index_past_the_log() {
            let start = Start {
                entries: entries(&[(1, 1)]),
                applied: 2,
                ..start(&[1], at_term(1))
            };
            let err = Raft::new(CONFIG, start).unwrap_err();
            assert_eq!(
                err,
                Error::AppliedPastLog {
                    applied: 2,
                    last: 1
                }
            );
            assert_eq!(
                err.to_string(),
                "applied index 2 is past the last log index 1"
            );
        }

        #[test]
        fn rejects_a_term_behind_the_log() {
            let hard = Hard {
                term: Term(2),
                vote: None,
            };
            let last = Position {
                term: Term(3),
                index: 7,
            };
            let start = Start {
                entries: entries(&[
                    (1, 1),
                    (3, 2),
                    (3, 3),
                    (3, 4),
                    (3, 5),
                    (3, 6),
                    (3, 7),
                ]),
                ..start(&[1], hard)
            };
            let err = Raft::new(CONFIG, start).unwrap_err();
            assert_eq!(
                err,
                Error::TermBehindLog {
                    term: Term(2),
                    last
                }
            );
            assert_eq!(
                err.to_string(),
                "stored term 2 is lower than term 3 of the last log entry"
            );
        }
    }

    mod tick {
        use super::*;

        fn ticks_to_campaign(random: u64) -> u32 {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            for ticks in 1..=20 {
                raft.tick(random);
                if raft.role() != Role::Follower {
                    return ticks;
                }
            }
            panic!("no campaign in 20 ticks");
        }

        #[test]
        fn campaigns_between_one_and_two_election_timeouts() {
            assert_eq!(ticks_to_campaign(0), 10);
            assert_eq!(ticks_to_campaign(9), 19);
            assert_eq!(ticks_to_campaign(10), 10);
            assert_eq!(ticks_to_campaign(u64::MAX), 15);
        }

        #[test]
        fn keeps_the_timeout_it_drew_first() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            raft.tick(5);
            for _ in 0..13 {
                raft.tick(0);
            }
            assert_eq!(raft.role(), Role::Follower);
            raft.tick(0);
            assert_eq!(raft.role(), Role::PreCandidate);
        }

        fn at_term_one() -> Raft {
            raft(&[1, 2, 3], at_term(1))
        }

        #[test]
        fn draws_a_new_timeout_after_a_new_term() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            raft.tick(5);
            raft.step(message(2, 1, Body::Heartbeat { commit: 0 }))
                .unwrap();
            tick_times(&mut raft, 9);
            assert_eq!(raft.role(), Role::Follower);
            raft.tick(0);
            assert_eq!(raft.role(), Role::PreCandidate);
        }

        #[test]
        fn waits_a_full_timeout_after_a_heartbeat() {
            let mut raft = at_term_one();
            tick_times(&mut raft, 9);
            raft.step(message(2, 1, Body::Heartbeat { commit: 0 }))
                .unwrap();
            tick_times(&mut raft, 9);
            assert_eq!(raft.role(), Role::Follower);
            raft.tick(0);
            assert_eq!(raft.role(), Role::PreCandidate);
        }

        #[test]
        fn waits_a_full_timeout_after_it_grants_a_vote() {
            let mut raft = at_term_one();
            tick_times(&mut raft, 9);
            let vote = Body::Vote {
                last: Position::default(),
            };
            raft.step(message(2, 1, vote.clone())).unwrap();
            tick_times(&mut raft, 9);
            assert_eq!(raft.role(), Role::Follower);
            raft.tick(0);
            assert_eq!(raft.role(), Role::PreCandidate);
        }

        #[test]
        fn sends_a_prevote_for_the_next_term_to_each_other_voter() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            for _ in 0..10 {
                raft.tick(0);
            }
            let prevote = |to| Message {
                from: key(1),
                to: key(to),
                term: Term(1),
                body: Body::PreVote {
                    last: Position::default(),
                },
            };
            assert_eq!(sent(&mut raft), [prevote(2), prevote(3)]);
            assert_eq!(raft.hard(), Hard::default());
        }

        #[test]
        fn makes_a_leader_send_a_heartbeat_every_heartbeat_ticks() {
            let mut raft = raft(&[1, 2], Hard::default());
            raft.campaign();
            for body in [
                Body::PreVoteReply { granted: true },
                Body::VoteReply { granted: true },
            ] {
                raft.step(message(2, 1, body)).unwrap();
            }
            assert_eq!(raft.role(), Role::Leader);
            sent(&mut raft);
            raft.tick(0);
            let heartbeat = Message {
                from: key(1),
                to: key(2),
                term: Term(1),
                body: Body::Heartbeat { commit: 0 },
            };
            assert_eq!(sent(&mut raft), [heartbeat]);
        }
    }

    mod campaign {
        use super::*;

        #[test]
        fn does_nothing_on_a_leader() {
            let mut raft = raft(&[1], Hard::default());
            raft.campaign();
            assert_eq!((raft.role(), raft.term()), (Role::Leader, Term(1)));
            raft.campaign();
            assert_eq!((raft.role(), raft.term()), (Role::Leader, Term(1)));
            assert_eq!(raft.leader(), Some(key(1)));
        }
    }

    mod step {
        use super::*;

        #[test]
        fn rejects_a_message_for_another_node() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            let message = Message {
                to: key(3),
                ..message(2, 1, Body::Heartbeat { commit: 0 })
            };
            let err = raft.step(message).unwrap_err();
            assert_eq!(err, Error::Misrouted { to: key(3) });
            assert_eq!(
                err.to_string(),
                "a message for node 00000000000000000000000000000003 is not for this \
                 node"
            );
            assert_eq!((raft.term(), raft.leader()), (Term(0), None));
        }

        #[test]
        fn rejects_a_message_from_itself() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            raft.campaign();
            let rejected = Body::PreVoteReply { granted: false };
            let err = raft.step(message(1, 1, rejected.clone())).unwrap_err();
            assert_eq!(err, Error::Loopback);
            assert_eq!(err.to_string(), "a message names this node as its sender");
            assert_eq!(raft.role(), Role::PreCandidate);
        }

        #[test]
        fn rejects_a_heartbeat_for_a_term_it_leads() {
            let mut raft = raft(&[1], Hard::default());
            raft.campaign();
            let err = raft
                .step(message(2, 1, Body::Heartbeat { commit: 0 }))
                .unwrap_err();
            assert_eq!(
                err,
                Error::SecondLeader {
                    term: Term(1),
                    from: key(2)
                }
            );
            assert_eq!(
                err.to_string(),
                "node 00000000000000000000000000000002 also claims to lead term 1"
            );
            assert_eq!((raft.role(), raft.leader()), (Role::Leader, Some(key(1))));
            assert_eq!(sent(&mut raft), []);
        }

        #[test]
        fn does_not_campaign_past_the_last_term() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            raft.step(message(9, u64::MAX, Body::Heartbeat { commit: 0 }))
                .unwrap();
            sent(&mut raft);
            for _ in 0..20 {
                raft.tick(0);
            }
            raft.campaign();
            assert_eq!((raft.role(), raft.term()), (Role::Follower, Term(u64::MAX)));
            assert_eq!(sent(&mut raft), []);
        }

        #[test]
        fn does_not_vote_twice_in_a_term_after_a_restart() {
            let mut first = raft(&[1, 2, 3], Hard::default());
            let vote = Body::Vote {
                last: Position::default(),
            };
            first.step(message(2, 1, vote.clone())).unwrap();
            let granted = Body::VoteReply { granted: true };
            assert_eq!(sent(&mut first)[0].body, granted);

            let mut restarted = raft(&[1, 2, 3], first.hard());
            restarted.step(message(3, 1, vote.clone())).unwrap();
            let rejected = Body::VoteReply { granted: false };
            assert_eq!(sent(&mut restarted)[0].body, rejected);
            restarted.step(message(2, 1, vote.clone())).unwrap();
            assert_eq!(sent(&mut restarted)[0].body, granted);
        }

        #[test]
        fn counts_only_the_first_answer_of_a_voter() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            raft.campaign();
            let rejected = Body::PreVoteReply { granted: false };
            raft.step(message(2, 0, rejected.clone())).unwrap();
            let granted = Body::PreVoteReply { granted: true };
            raft.step(message(2, 1, granted.clone())).unwrap();
            assert_eq!(raft.role(), Role::PreCandidate);
            raft.step(message(3, 1, granted.clone())).unwrap();
            assert_eq!(raft.role(), Role::Candidate);
        }

        #[test]
        fn ignores_an_answer_from_a_node_that_is_not_a_voter() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            raft.campaign();
            let granted = Body::PreVoteReply { granted: true };
            raft.step(message(9, 1, granted.clone())).unwrap();
            assert_eq!(raft.role(), Role::PreCandidate);
        }

        #[test]
        fn becomes_follower_when_a_quorum_rejects() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            raft.campaign();
            let rejected = Body::PreVoteReply { granted: false };
            raft.step(message(2, 0, rejected.clone())).unwrap();
            assert_eq!(raft.role(), Role::PreCandidate);
            raft.step(message(3, 0, rejected.clone())).unwrap();
            assert_eq!((raft.role(), raft.term()), (Role::Follower, Term(0)));
        }

        #[test]
        fn answers_a_heartbeat_from_a_lower_term_with_its_own_term() {
            let hard = Hard {
                term: Term(5),
                vote: None,
            };
            let mut raft = raft(&[1, 2, 3], hard);
            raft.step(message(2, 4, Body::Heartbeat { commit: 0 }))
                .unwrap();
            let reply = Message {
                from: key(1),
                to: key(2),
                term: Term(5),
                body: Body::HeartbeatReply,
            };
            assert_eq!(sent(&mut raft), [reply]);
            assert_eq!(raft.leader(), None);
        }
    }

    // Makes `raft` win the election it campaigns for, with grants from `from`.
    fn elect(raft: &mut Raft, from: &[u8]) {
        raft.campaign();
        let term = raft.term().0 + 1;
        for granted in [
            Body::PreVoteReply { granted: true },
            Body::VoteReply { granted: true },
        ] {
            for &from in from {
                raft.step(message(from, term, granted.clone())).unwrap();
            }
        }
        assert_eq!((raft.role(), raft.term()), (Role::Leader, Term(term)));
        sent(raft);
    }

    fn at_term(term: u64) -> Hard {
        Hard {
            term: Term(term),
            vote: None,
        }
    }

    fn tick_times(raft: &mut Raft, times: u32) {
        for _ in 0..times {
            raft.tick(0);
        }
    }

    mod candidate {
        use super::*;

        fn candidate() -> Raft {
            let mut raft = raft(&[1, 2, 3, 4, 5], at_term(1));
            raft.campaign();
            let granted = Body::PreVoteReply { granted: true };
            raft.step(message(2, 2, granted.clone())).unwrap();
            raft.step(message(3, 2, granted.clone())).unwrap();
            assert_eq!((raft.role(), raft.term()), (Role::Candidate, Term(2)));
            sent(&mut raft);
            raft
        }

        #[test]
        fn does_not_count_a_prevote_grant_as_a_vote() {
            let mut raft = candidate();
            let granted = Body::PreVoteReply { granted: true };
            raft.step(message(4, 2, granted.clone())).unwrap();
            raft.step(message(5, 2, granted.clone())).unwrap();
            assert_eq!(raft.role(), Role::Candidate);
        }

        #[test]
        fn does_not_count_a_vote_from_a_lower_term() {
            let mut raft = candidate();
            let granted = Body::VoteReply { granted: true };
            raft.step(message(4, 1, granted.clone())).unwrap();
            raft.step(message(5, 1, granted.clone())).unwrap();
            assert_eq!(raft.role(), Role::Candidate);
        }

        #[test]
        fn follows_the_leader_of_its_term() {
            let mut raft = candidate();
            raft.step(message(2, 2, Body::Heartbeat { commit: 0 }))
                .unwrap();
            assert_eq!((raft.role(), raft.term()), (Role::Follower, Term(2)));
            assert_eq!(raft.leader(), Some(key(2)));
            let reply = Message {
                from: key(1),
                to: key(2),
                term: Term(2),
                body: Body::HeartbeatReply,
            };
            assert_eq!(sent(&mut raft), [reply]);
        }

        #[test]
        fn steps_down_on_a_vote_reply_from_a_higher_term() {
            let mut raft = candidate();
            let rejected = Body::VoteReply { granted: false };
            raft.step(message(4, 7, rejected.clone())).unwrap();
            assert_eq!((raft.role(), raft.hard()), (Role::Follower, at_term(7)));
        }
    }

    mod pre_candidate {
        use super::*;

        #[test]
        fn does_not_count_a_vote_grant_as_a_prevote() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            raft.campaign();
            let granted = Body::VoteReply { granted: true };
            raft.step(message(2, 1, granted.clone())).unwrap();
            assert_eq!((raft.role(), raft.term()), (Role::PreCandidate, Term(1)));
        }

        #[test]
        fn sends_prevotes_again_only_after_a_full_timeout() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            tick_times(&mut raft, 10);
            assert_eq!(sent(&mut raft).len(), 2);
            tick_times(&mut raft, 9);
            assert_eq!(sent(&mut raft), []);
            raft.tick(0);
            assert_eq!(sent(&mut raft).len(), 2);
            assert_eq!((raft.role(), raft.term()), (Role::PreCandidate, Term(0)));
        }
    }

    mod follower {
        use super::*;

        #[test]
        fn waits_a_full_timeout_after_a_new_term() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            tick_times(&mut raft, 9);
            raft.step(message(2, 1, Body::HeartbeatReply)).unwrap();
            tick_times(&mut raft, 9);
            assert_eq!(raft.role(), Role::Follower);
            raft.tick(0);
            assert_eq!(raft.role(), Role::PreCandidate);
        }

        #[test]
        fn rejects_a_vote_in_a_term_whose_leader_it_knows() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            raft.step(message(2, 1, Body::Heartbeat { commit: 0 }))
                .unwrap();
            sent(&mut raft);
            let vote = Body::Vote {
                last: Position::default(),
            };
            raft.step(message(3, 1, vote.clone())).unwrap();
            let rejected = Body::VoteReply { granted: false };
            assert_eq!(sent(&mut raft)[0].body, rejected);
            assert_eq!(raft.hard(), at_term(1));
        }

        #[test]
        fn takes_the_term_of_a_vote_it_rejects() {
            let start = Start {
                entries: entries(&[(1, 1), (1, 2), (1, 3), (1, 4), (1, 5)]),
                ..start(&[1, 2, 3], at_term(1))
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            let vote = Body::Vote {
                last: Position::default(),
            };
            raft.step(message(2, 3, vote.clone())).unwrap();
            let reply = Message {
                from: key(1),
                to: key(2),
                term: Term(3),
                body: Body::VoteReply { granted: false },
            };
            assert_eq!(sent(&mut raft), [reply]);
            assert_eq!(raft.hard(), at_term(3));
        }

        #[test]
        fn rejects_a_prevote_from_a_lower_term_with_its_own_term() {
            let mut raft = raft(&[1, 2, 3], at_term(5));
            let prevote = Body::PreVote {
                last: Position::default(),
            };
            raft.step(message(2, 3, prevote)).unwrap();
            let reply = Message {
                from: key(1),
                to: key(2),
                term: Term(5),
                body: Body::PreVoteReply { granted: false },
            };
            assert_eq!(sent(&mut raft), [reply]);
        }

        #[test]
        fn follows_a_leader_that_is_not_in_its_voter_list() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            raft.step(message(9, 1, Body::Heartbeat { commit: 0 }))
                .unwrap();
            assert_eq!((raft.term(), raft.leader()), (Term(1), Some(key(9))));
        }

        #[test]
        fn does_not_campaign_when_it_is_not_in_its_voter_list() {
            let mut raft = raft(&[2, 3], Hard::default());
            raft.campaign();
            assert_eq!(raft.role(), Role::Follower);
            assert_eq!(sent(&mut raft), []);
        }
    }

    mod leader {
        use super::*;

        #[test]
        fn steps_down_when_it_hears_from_less_than_a_quorum() {
            let mut raft = raft(&[1, 2, 3, 4, 5], Hard::default());
            elect(&mut raft, &[2, 3]);
            raft.step(message(2, 1, Body::HeartbeatReply)).unwrap();
            tick_times(&mut raft, 9);
            assert_eq!(raft.role(), Role::Leader);
            raft.tick(0);
            assert_eq!((raft.role(), raft.leader()), (Role::Follower, None));
        }

        #[test]
        fn keeps_its_vote_when_it_steps_down() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            tick_times(&mut raft, 10);
            assert_eq!(raft.role(), Role::Follower);
            sent(&mut raft);
            let vote = Body::Vote {
                last: Position::default(),
            };
            raft.step(message(3, 1, vote.clone())).unwrap();
            let rejected = Body::VoteReply { granted: false };
            assert_eq!(sent(&mut raft)[0].body, rejected);
            assert_eq!(raft.hard().vote, Some(key(1)));
        }

        #[test]
        fn does_not_count_contact_from_a_lower_term() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            elect(&mut raft, &[2]);
            raft.step(message(2, 1, Body::HeartbeatReply)).unwrap();
            tick_times(&mut raft, 10);
            assert_eq!(raft.role(), Role::Follower);
        }

        #[test]
        fn does_not_count_contact_from_an_earlier_leadership() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            raft.step(message(2, 1, Body::HeartbeatReply)).unwrap();
            raft.step(message(3, 5, Body::HeartbeatReply)).unwrap();
            assert_eq!((raft.role(), raft.term()), (Role::Follower, Term(5)));
            elect(&mut raft, &[2]);
            tick_times(&mut raft, 10);
            assert_eq!(raft.role(), Role::Follower);
        }

        #[test]
        fn starts_its_heartbeat_period_when_elected() {
            let config = Config {
                heartbeat_ticks: 3,
                ..CONFIG
            };
            let mut raft = Raft::new(config, start(&[1, 2], Hard::default())).unwrap();
            elect(&mut raft, &[2]);
            tick_times(&mut raft, 2);
            assert_eq!(sent(&mut raft), []);
            raft.step(message(2, 5, Body::HeartbeatReply)).unwrap();
            elect(&mut raft, &[2]);
            tick_times(&mut raft, 2);
            assert_eq!(sent(&mut raft), []);
            raft.tick(0);
            assert_eq!(sent(&mut raft).len(), 1);
        }
    }

    mod replication {
        use super::*;

        fn position(term: u64, index: u64) -> Position {
            Position {
                term: Term(term),
                index,
            }
        }

        fn append(prev: Position, entries: Vec<Entry>, commit: u64) -> Body {
            Body::Append {
                prev,
                entries,
                commit,
            }
        }

        fn accepted(last: u64) -> Body {
            Body::AppendReply { last }
        }

        // A leader of 1, 2, and 3 at term 2 over the log `positions`, after its
        // first messages.
        fn leader_over(positions: &[(u64, u64)]) -> Raft {
            let start = Start {
                entries: entries(positions),
                ..start(&[1, 2, 3], at_term(1))
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            elect(&mut raft, &[2]);
            raft
        }

        #[test]
        fn a_follower_refuses_a_proposal_and_names_the_leader_it_knows() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            let error = raft.propose(vec![1]).unwrap_err();
            assert_eq!(error, Error::NotLeader { leader: None });
            assert_eq!(
                error.to_string(),
                "this node does not lead, and knows no leader"
            );
            raft.step(message(2, 1, Body::Heartbeat { commit: 0 }))
                .unwrap();
            let error = raft.propose(vec![1]).unwrap_err();
            assert_eq!(
                error,
                Error::NotLeader {
                    leader: Some(key(2))
                }
            );
            assert_eq!(
                error.to_string(),
                "this node does not lead; node 00000000000000000000000000000002 does"
            );
        }

        #[test]
        fn a_new_leader_writes_an_empty_entry_of_its_term_and_probes() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            raft.campaign();
            let term = Term(2);
            let granted = Body::PreVoteReply { granted: true };
            raft.step(message(2, 2, granted)).unwrap();
            sent(&mut raft);
            raft.step(message(2, 2, Body::VoteReply { granted: true }))
                .unwrap();
            let ready = raft.ready();
            let empty = Entry {
                at: position(2, 1),
                data: Data::Empty,
            };
            assert_eq!(ready.entries, std::slice::from_ref(&empty));
            assert_eq!(ready.committed, []);
            let probe = |to| Message {
                from: key(1),
                to: key(to),
                term,
                body: append(Position::default(), vec![empty.clone()], 0),
            };
            assert_eq!(ready.messages, [probe(2), probe(3)]);
        }

        #[test]
        fn a_proposal_takes_the_next_index_and_goes_out_in_the_ready() {
            let mut raft = leader_over(&[]);
            raft.step(message(2, 2, accepted(1))).unwrap();
            sent(&mut raft);
            let at = raft.propose(vec![7]).unwrap();
            assert_eq!(at, position(2, 2));
            let ready = raft.ready();
            let entry = Entry {
                at,
                data: Data::Bytes(vec![7]),
            };
            assert_eq!(ready.entries, std::slice::from_ref(&entry));
            let to_2 = Message {
                from: key(1),
                to: key(2),
                term: Term(2),
                body: append(position(2, 1), vec![entry], 1),
            };
            assert_eq!(ready.messages, [to_2]);
        }

        #[test]
        fn a_leader_alone_commits_what_it_proposes() {
            let mut raft = raft(&[1], Hard::default());
            raft.campaign();
            sent(&mut raft);
            let at = raft.propose(vec![7]).unwrap();
            let ready = raft.ready();
            let entry = Entry {
                at,
                data: Data::Bytes(vec![7]),
            };
            assert_eq!(ready.committed, [entry]);
            assert_eq!(ready.messages, []);
        }

        #[test]
        fn commits_only_with_a_quorum_and_only_from_its_own_term() {
            let mut raft = leader_over(&[(1, 1)]);
            raft.step(message(2, 2, accepted(1))).unwrap();
            assert_eq!(raft.ready().committed, []);
            raft.step(message(3, 2, accepted(2))).unwrap();
            let committed = raft.ready().committed;
            let at: Vec<Position> = committed.iter().map(|entry| entry.at).collect();
            assert_eq!(at, [position(1, 1), position(2, 2)]);
        }

        #[test]
        fn sends_from_the_hint_after_a_rejection() {
            let mut raft = leader_over(&[(1, 1), (1, 2)]);
            raft.step(message(2, 2, Body::AppendReject { hint: 0 }))
                .unwrap();
            let [message] = &sent(&mut raft)[..] else {
                panic!();
            };
            let expected =
                append(Position::default(), entries(&[(1, 1), (1, 2), (2, 3)]), 0);
            assert_eq!((message.to, &message.body), (key(2), &expected));
        }

        #[test]
        fn resends_after_a_heartbeat_reply_when_an_append_was_lost() {
            let mut raft = leader_over(&[]);
            raft.step(message(2, 2, accepted(1))).unwrap();
            sent(&mut raft);
            raft.propose(vec![7]).unwrap();
            // The append with entry 2 is lost.
            sent(&mut raft);
            raft.step(message(2, 2, Body::HeartbeatReply)).unwrap();
            let [sent_message] = &sent(&mut raft)[..] else {
                panic!("no append after the heartbeat reply");
            };
            let Body::Append { prev, entries, .. } = &sent_message.body else {
                panic!("{:?}", sent_message.body);
            };
            assert_eq!(
                (sent_message.to, *prev, entries.len()),
                (key(2), position(2, 2), 0)
            );
            raft.step(message(2, 2, Body::AppendReject { hint: 1 }))
                .unwrap();
            let [sent_message] = &sent(&mut raft)[..] else {
                panic!("no append after the rejection");
            };
            let Body::Append { prev, entries, .. } = &sent_message.body else {
                panic!("{:?}", sent_message.body);
            };
            assert_eq!((*prev, entries.len()), (position(2, 1), 1));
        }

        #[test]
        fn a_heartbeat_names_only_entries_the_follower_holds() {
            let mut raft = leader_over(&[]);
            raft.step(message(2, 2, accepted(1))).unwrap();
            sent(&mut raft);
            raft.tick(0);
            let commits: Vec<(node::Key, u64)> = sent(&mut raft)
                .iter()
                .map(|message| {
                    let Body::Heartbeat { commit } = message.body else {
                        panic!("{:?}", message.body);
                    };
                    (message.to, commit)
                })
                .collect();
            assert_eq!(commits, [(key(2), 1), (key(3), 0)]);
        }

        #[test]
        fn answers_an_append_from_a_lower_term_with_its_own_term() {
            let mut raft = raft(&[1, 2, 3], at_term(5));
            let body = append(Position::default(), entries(&[(4, 1)]), 1);
            raft.step(message(2, 4, body)).unwrap();
            let reply = Message {
                from: key(1),
                to: key(2),
                term: Term(5),
                body: Body::HeartbeatReply,
            };
            assert_eq!(sent(&mut raft), [reply]);
            assert_eq!(raft.ready().entries, []);
        }

        #[test]
        fn a_follower_takes_the_leader_of_an_append() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            let body = append(Position::default(), entries(&[(1, 1)]), 1);
            raft.step(message(2, 1, body)).unwrap();
            assert_eq!(raft.leader(), Some(key(2)));
            let ready = raft.ready();
            assert_eq!(ready.entries, entries(&[(1, 1)]));
            assert_eq!(ready.committed, entries(&[(1, 1)]));
            let reply = Message {
                from: key(1),
                to: key(2),
                term: Term(1),
                body: accepted(1),
            };
            assert_eq!(ready.messages, [reply]);
        }
    }

    mod peers {
        use super::*;

        fn committed(raft: &mut Raft) -> Vec<u64> {
            raft.ready()
                .committed
                .iter()
                .map(|entry| entry.at.index)
                .collect()
        }

        #[test]
        fn finds_each_voter_when_the_start_list_is_unsorted() {
            let mut raft = raft(&[5, 1, 3], Hard::default());
            elect(&mut raft, &[5, 3]);
            raft.step(message(3, 1, Body::HeartbeatReply)).unwrap();
            raft.step(message(5, 1, Body::HeartbeatReply)).unwrap();
            tick_times(&mut raft, 10);
            assert_eq!(raft.role(), Role::Leader);
            let to: std::collections::BTreeSet<node::Key> =
                sent(&mut raft).iter().map(|m| m.to).collect();
            assert_eq!(to.into_iter().collect::<Vec<_>>(), [key(3), key(5)]);
        }

        #[test]
        fn a_non_voter_does_not_count_for_a_commit_or_the_quorum_check() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            raft.propose(b"x".to_vec()).unwrap();
            sent(&mut raft);
            raft.step(message(9, 1, Body::AppendReply { last: 2 }))
                .unwrap();
            assert_eq!(committed(&mut raft), []);
            raft.step(message(9, 1, Body::HeartbeatReply)).unwrap();
            tick_times(&mut raft, 10);
            assert_eq!(raft.role(), Role::Follower);
        }

        #[test]
        fn a_non_voter_answer_does_not_move_a_campaign() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            raft.campaign();
            let grant = Body::PreVoteReply { granted: true };
            raft.step(message(9, 1, grant.clone())).unwrap();
            assert_eq!(raft.role(), Role::PreCandidate);
            raft.step(message(2, 1, grant)).unwrap();
            assert_eq!(raft.role(), Role::Candidate);
            let grant = Body::VoteReply { granted: true };
            raft.step(message(9, 1, grant.clone())).unwrap();
            assert_eq!(raft.role(), Role::Candidate);
            raft.step(message(3, 1, grant)).unwrap();
            assert_eq!(raft.role(), Role::Leader);
        }

        #[test]
        fn a_heartbeat_names_the_commit_each_follower_holds() {
            let mut raft = raft(&[1, 2, 3, 4, 5], at_term(1));
            elect(&mut raft, &[2, 3]);
            raft.step(message(3, 2, Body::AppendReply { last: 1 }))
                .unwrap();
            raft.step(message(5, 2, Body::AppendReply { last: 1 }))
                .unwrap();
            assert_eq!(committed(&mut raft), [1]);
            raft.tick(0);
            let heartbeats: Vec<(node::Key, u64)> = sent(&mut raft)
                .iter()
                .filter_map(|m| match m.body {
                    Body::Heartbeat { commit } => Some((m.to, commit)),
                    Body::PreVote { .. }
                    | Body::PreVoteReply { .. }
                    | Body::Vote { .. }
                    | Body::VoteReply { .. }
                    | Body::HeartbeatReply
                    | Body::Append { .. }
                    | Body::AppendReply { .. }
                    | Body::AppendReject { .. } => None,
                })
                .collect();
            assert_eq!(
                heartbeats,
                [(key(2), 0), (key(3), 1), (key(4), 0), (key(5), 1)]
            );
        }
    }

    mod config {
        use super::*;

        fn voters(ids: &[u8]) -> Voters {
            Voters {
                incoming: ids.iter().copied().map(key).collect(),
                ..Voters::default()
            }
        }

        fn config(term: u64, index: u64, voters: Voters) -> Entry {
            Entry {
                at: Position {
                    term: Term(term),
                    index,
                },
                data: Data::Voters(voters),
            }
        }

        // Node 1 follows 2 at term 1 and holds `entries`, none committed.
        fn follower_with(entries: Vec<Entry>) -> Raft {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            let body = Body::Append {
                prev: Position::default(),
                entries,
                commit: 0,
            };
            raft.step(message(2, 1, body)).unwrap();
            sent(&mut raft);
            raft
        }

        fn heartbeat(raft: &mut Raft, commit: u64) {
            raft.step(message(2, 1, Body::Heartbeat { commit }))
                .unwrap();
        }

        #[test]
        fn a_leader_puts_a_committed_configuration_in_force() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            let new = voters(&[1, 2, 3, 4]);
            let at = raft.propose_entry(Data::Voters(new.clone())).unwrap();
            assert_eq!(at.index, 2);
            assert_eq!(raft.ready().voters, None);
            raft.step(message(2, 1, Body::AppendReply { last: 2 }))
                .unwrap();
            let ready = raft.ready();
            assert_eq!(ready.voters, Some(new.clone()));
            assert_eq!(ready.committed.last(), Some(&config(1, 2, new.clone())));
            assert_eq!(raft.voters(), &new);
            // The new peer gets one probe and then waits for its reply.
            let to: Vec<node::Key> = ready.messages.iter().map(|m| m.to).collect();
            assert_eq!(to, [key(2), key(4)]);
            assert_eq!(raft.ready().voters, None);
            raft.propose(vec![7]).unwrap();
            let to: Vec<node::Key> = sent(&mut raft).iter().map(|m| m.to).collect();
            assert_eq!(to, [key(2)]);
        }

        #[test]
        fn a_follower_puts_a_committed_configuration_in_force() {
            let new = voters(&[2, 3]);
            let mut raft = follower_with(vec![
                entries(&[(1, 1)]).remove(0),
                config(1, 2, new.clone()),
            ]);
            assert_eq!(raft.voters(), &voters(&[1, 2, 3]));
            heartbeat(&mut raft, 2);
            assert_eq!(raft.ready().voters, Some(new.clone()));
            assert_eq!(raft.voters(), &new);
            tick_times(&mut raft, 40);
            assert_eq!(raft.role(), Role::Follower);
        }

        #[test]
        fn only_the_last_configuration_a_commit_covers_counts() {
            let last = voters(&[1, 2, 3, 4, 5]);
            let mut raft = follower_with(vec![
                entries(&[(1, 1)]).remove(0),
                config(1, 2, voters(&[1, 2])),
                config(1, 3, last.clone()),
            ]);
            heartbeat(&mut raft, 3);
            assert_eq!(raft.ready().voters, Some(last.clone()));
            assert_eq!(raft.voters(), &last);
        }

        #[test]
        fn a_restart_waits_for_the_entries_after_applied_to_commit() {
            let new = voters(&[1, 2, 3, 4]);
            let start = Start {
                entries: vec![entries(&[(1, 1)]).remove(0), config(1, 2, new.clone())],
                applied: 1,
                ..start(&[1, 2, 3], at_term(1))
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            assert_eq!(raft.voters(), &voters(&[1, 2, 3]));
            assert_eq!(raft.ready().voters, None);
            heartbeat(&mut raft, 2);
            assert_eq!(raft.ready().voters, Some(new.clone()));
            assert_eq!(raft.voters(), &new);
        }

        #[test]
        #[should_panic(expected = "invariant: a committed configuration is valid")]
        fn a_committed_configuration_must_be_valid() {
            let bad = Voters {
                outgoing: [key(1), key(2)].into_iter().collect(),
                ..Voters::default()
            };
            let mut raft = follower_with(vec![config(1, 1, bad)]);
            heartbeat(&mut raft, 1);
        }
    }

    mod joint {
        use super::*;

        fn joint(incoming: &[u8], outgoing: &[u8]) -> Raft {
            let start = Start {
                voters: Voters {
                    incoming: incoming.iter().copied().map(key).collect(),
                    outgoing: outgoing.iter().copied().map(key).collect(),
                },
                ..start(&[], Hard::default())
            };
            Raft::new(CONFIG, start).unwrap()
        }

        #[test]
        fn rejects_an_empty_incoming_set_with_an_outgoing_set() {
            let start = Start {
                voters: Voters {
                    outgoing: [key(1), key(2)].into_iter().collect(),
                    ..Voters::default()
                },
                ..start(&[], Hard::default())
            };
            let error = Raft::new(CONFIG, start).unwrap_err();
            assert_eq!(error, Error::EmptyIncoming);
            assert_eq!(
                error.to_string(),
                "the incoming voter set is empty while the outgoing set is not"
            );
        }

        #[test]
        fn reports_the_voters_it_started_with() {
            let raft = joint(&[3, 1], &[2, 1]);
            let voters = Voters {
                incoming: [key(1), key(3)].into_iter().collect(),
                outgoing: [key(1), key(2)].into_iter().collect(),
            };
            assert_eq!(raft.voters(), &voters);
        }

        #[test]
        fn a_node_only_in_the_outgoing_list_campaigns() {
            let mut raft = joint(&[2, 3], &[1, 2, 3]);
            raft.campaign();
            assert_eq!(
                (raft.role(), sent(&mut raft).len()),
                (Role::PreCandidate, 2)
            );
        }

        #[test]
        fn an_election_needs_a_majority_of_each_set() {
            let mut raft = joint(&[1, 2, 3], &[4, 5, 6]);
            raft.campaign();
            sent(&mut raft);
            let granted = Body::PreVoteReply { granted: true };
            raft.step(message(2, 1, granted.clone())).unwrap();
            raft.step(message(3, 1, granted.clone())).unwrap();
            assert_eq!(raft.role(), Role::PreCandidate);
            raft.step(message(4, 1, granted.clone())).unwrap();
            assert_eq!(raft.role(), Role::PreCandidate);
            raft.step(message(5, 1, granted)).unwrap();
            assert_eq!(raft.role(), Role::Candidate);
        }

        #[test]
        fn an_election_is_lost_when_either_set_rejects() {
            let mut raft = joint(&[1, 2, 3], &[4, 5, 6]);
            raft.campaign();
            sent(&mut raft);
            raft.step(message(2, 1, Body::PreVoteReply { granted: true }))
                .unwrap();
            raft.step(message(4, 0, Body::PreVoteReply { granted: false }))
                .unwrap();
            assert_eq!(raft.role(), Role::PreCandidate);
            raft.step(message(5, 0, Body::PreVoteReply { granted: false }))
                .unwrap();
            assert_eq!((raft.role(), raft.term()), (Role::Follower, Term(0)));
        }

        #[test]
        fn a_commit_needs_a_majority_of_each_set() {
            let mut raft = joint(&[1, 2, 3], &[1, 4, 5]);
            elect(&mut raft, &[2, 4]);
            raft.propose(vec![7]).unwrap();
            sent(&mut raft);
            let accepted = Body::AppendReply { last: 2 };
            raft.step(message(2, 1, accepted.clone())).unwrap();
            raft.step(message(3, 1, accepted.clone())).unwrap();
            assert_eq!(raft.ready().committed, []);
            raft.step(message(4, 1, accepted)).unwrap();
            let committed: Vec<u64> = raft
                .ready()
                .committed
                .iter()
                .map(|entry| entry.at.index)
                .collect();
            assert_eq!(committed, [1, 2]);
        }

        #[test]
        fn a_leader_steps_down_without_a_majority_of_each_set() {
            let mut raft = joint(&[1, 2, 3], &[1, 4, 5]);
            elect(&mut raft, &[2, 4]);
            raft.step(message(2, 1, Body::HeartbeatReply)).unwrap();
            raft.step(message(4, 1, Body::HeartbeatReply)).unwrap();
            tick_times(&mut raft, 10);
            assert_eq!(raft.role(), Role::Leader);
            raft.step(message(2, 1, Body::HeartbeatReply)).unwrap();
            raft.step(message(3, 1, Body::HeartbeatReply)).unwrap();
            tick_times(&mut raft, 10);
            assert_eq!(raft.role(), Role::Follower);
        }
    }
}
