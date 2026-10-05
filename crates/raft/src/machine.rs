use types::node;

use crate::log::Log;
use crate::tracker::{BATCH, Progress};
use crate::{Body, Config, Entry, Error, Hard, Message, Position, Start, Term};

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
}

/// One node's state machine. PreVote and CheckQuorum are always on.
///
/// After each call to [`tick`](Self::tick), [`step`](Self::step), or
/// [`campaign`](Self::campaign), take [`ready`](Self::ready) and do what it says.
#[derive(Debug)]
pub struct Raft {
    key: node::Key,
    // Sorted. `votes` and `active` are parallel to it.
    voters: Vec<node::Key>,
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
    votes: Vec<Option<bool>>,
    // The voters a leader heard from since its last quorum check.
    active: Vec<bool>,
    // A leader's view of each voter's log, in voter order.
    progress: Vec<Progress>,
    outbox: Vec<Message>,
}

impl Raft {
    /// Builds a follower.
    ///
    /// # Errors
    ///
    /// - [`Error::Ticks`] when `heartbeat_ticks` is 0 or `election_ticks` is not
    ///   greater than `heartbeat_ticks`.
    /// - [`Error::DuplicateVoter`] when `voters` names a node twice.
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
            mut voters,
            entries,
            applied,
        } = start;
        if heartbeat_ticks == 0 || election_ticks <= heartbeat_ticks {
            return Err(Error::Ticks {
                election: election_ticks,
                heartbeat: heartbeat_ticks,
            });
        }
        voters.sort_unstable();
        let mut pairs = voters.iter().zip(voters.iter().skip(1));
        if let Some((&twice, _)) = pairs.find(|(a, b)| a == b) {
            return Err(Error::DuplicateVoter(twice));
        }
        let log = Log::new(entries, applied)?;
        let last = log.last();
        if hard.term < last.term {
            return Err(Error::TermBehindLog {
                term: hard.term,
                last,
            });
        }
        Ok(Self {
            key,
            votes: vec![None; voters.len()],
            active: vec![false; voters.len()],
            progress: vec![Progress::new(0); voters.len()],
            outbox: Vec::new(),
            voters,
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
            entries: self.log.unstable(),
            committed: self.log.take_committed(),
            messages: std::mem::take(&mut self.outbox),
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
                if self.role == Role::Leader
                    && let Ok(voter) = self.voters.binary_search(&from)
                {
                    self.active[voter] = true;
                    self.progress[voter].heard();
                    self.send_append(voter, false);
                }
            }
            Body::Append {
                prev,
                entries,
                commit,
            } => {
                self.follow(from)?;
                self.append(from, prev, entries, commit);
            }
            Body::AppendReply { index, rejected } => {
                if self.role == Role::Leader
                    && let Ok(voter) = self.voters.binary_search(&from)
                {
                    self.active[voter] = true;
                    self.track(voter, index, rejected);
                }
            }
        }
        Ok(())
    }

    // Commits what the leader's heartbeat says and answers it.
    fn heartbeat(&mut self, leader: node::Key, commit: u64) -> Result<(), Error> {
        self.follow(leader)?;
        self.log.commit_to(commit);
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
            Ok(index) => {
                self.log.commit_to(commit.min(index));
                Body::AppendReply {
                    index,
                    rejected: false,
                }
            }
            Err(index) => Body::AppendReply {
                index,
                rejected: true,
            },
        };
        self.send(leader, self.term, reply);
    }

    // Records a follower's answer to an append as the leader and sends what follows.
    fn track(&mut self, voter: usize, index: u64, rejected: bool) {
        if rejected {
            self.progress[voter].rejected(index);
            self.send_append(voter, false);
        } else if self.progress[voter].accepted(index) {
            if self.commit() {
                self.replicate();
            } else {
                self.send_append(voter, false);
            }
        }
    }

    // Commits the highest index that a quorum holds, when an entry of the leader's
    // own term is there. Returns whether the commit index moved.
    fn commit(&mut self) -> bool {
        let mut matched: Vec<u64> = self
            .progress
            .iter()
            .zip(&self.voters)
            .map(|(progress, &voter)| {
                if voter == self.key {
                    self.log.last().index
                } else {
                    progress.matched
                }
            })
            .collect();
        matched.sort_unstable();
        let index = matched[matched.len() - self.quorum()];
        let current = self.log.at(index).is_some_and(|at| at.term == self.term);
        if index > self.log.committed() && current {
            self.log.commit_to(index);
            return true;
        }
        false
    }

    // Sends each follower the entries it lacks and the commit index.
    fn replicate(&mut self) {
        for voter in 0..self.voters.len() {
            self.send_append(voter, true);
        }
    }

    // Sends one follower the entries from its `next`, unless the leader waits for
    // its reply. With `even_empty`, a follower that lacks nothing gets the commit
    // index alone.
    fn send_append(&mut self, voter: usize, even_empty: bool) {
        let to = self.voters[voter];
        let progress = &self.progress[voter];
        let behind = progress.next <= self.log.last().index;
        if to == self.key || progress.paused() || !(behind || even_empty) {
            return;
        }
        let next = progress.next;
        let prev = self
            .log
            .at(next - 1)
            .expect("invariant: a follower's next entry follows the leader's log");
        let entries = self.log.from(next, BATCH);
        let last = prev.index + u64::try_from(entries.len()).unwrap_or(u64::MAX);
        self.progress[voter].sent(last);
        let commit = self.log.committed();
        self.send(
            to,
            self.term,
            Body::Append {
                prev,
                entries,
                commit,
            },
        );
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
                | Body::AppendReply { .. } => self.become_follower(term, None),
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
                | Body::AppendReply { .. } => {}
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
                .iter()
                .zip(&self.active)
                .filter(|&(&voter, &active)| active || voter == self.key)
                .count();
            self.active.fill(false);
            if heard < self.quorum() {
                self.become_follower(self.term, None);
                return;
            }
        }
        if self.heartbeat_elapsed >= self.heartbeat_ticks {
            self.heartbeat_elapsed = 0;
            // A follower commits what the heartbeat says, so it names only entries
            // the follower is known to hold.
            for voter in 0..self.voters.len() {
                let to = self.voters[voter];
                if to != self.key {
                    let commit = self.log.committed().min(self.progress[voter].matched);
                    self.send(to, self.term, Body::Heartbeat { commit });
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
        self.votes.fill(None);
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
        self.progress.fill(Progress::new(last));
        // An entry of the leader's own term lets it commit the ones before it.
        self.log.push(self.term, Vec::new());
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
        self.votes.fill(None);
        self.active.fill(false);
    }

    // Records one answer to the current campaign and acts when the answers decide it.
    // The first answer of a voter counts.
    fn poll(&mut self, from: node::Key, granted: bool) {
        let Ok(voter) = self.voters.binary_search(&from) else {
            return;
        };
        self.votes[voter].get_or_insert(granted);
        let count = |answer| {
            self.votes
                .iter()
                .filter(|&&vote| vote == Some(answer))
                .count()
        };
        let (yes, no) = (count(true), count(false));
        if yes >= self.quorum() {
            match self.role {
                Role::PreCandidate => self.become_candidate(),
                Role::Candidate => self.become_leader(),
                Role::Follower | Role::Leader => {
                    unreachable!("invariant: only a campaign counts votes")
                }
            }
        } else if self.voters.len() - no < self.quorum() {
            self.become_follower(self.term, None);
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
        self.voters.binary_search(&self.key).is_ok()
    }

    fn quorum(&self) -> usize {
        self.voters.len() / 2 + 1
    }

    fn broadcast(&mut self, term: Term, body: &Body) {
        for voter in 0..self.voters.len() {
            let to = self.voters[voter];
            if to != self.key {
                self.send(to, term, body.clone());
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
            voters: voters.iter().copied().map(key).collect(),
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
            data: Vec::new(),
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
        fn rejects_a_duplicate_voter() {
            let start = start(&[2, 1, 2], Hard::default());
            let err = Raft::new(CONFIG, start).unwrap_err();
            assert_eq!(err, Error::DuplicateVoter(key(2)));
            assert_eq!(
                err.to_string(),
                "node 00000000000000000000000000000002 is in the voter list twice"
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
            let mut ticks = 0;
            while raft.role() == Role::Follower {
                raft.tick(random);
                ticks += 1;
            }
            ticks
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

        fn accepted(index: u64) -> Body {
            Body::AppendReply {
                index,
                rejected: false,
            }
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
                data: Vec::new(),
            };
            assert_eq!(ready.entries, [empty.clone()]);
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
            let entry = Entry { at, data: vec![7] };
            assert_eq!(ready.entries, [entry.clone()]);
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
            assert_eq!(ready.committed, [Entry { at, data: vec![7] }]);
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
            raft.step(message(
                2,
                2,
                Body::AppendReply {
                    index: 0,
                    rejected: true,
                },
            ))
            .unwrap();
            let [message] = &sent(&mut raft)[..] else {
                panic!();
            };
            let expected =
                append(Position::default(), entries(&[(1, 1), (1, 2), (2, 3)]), 0);
            assert_eq!((message.to, &message.body), (key(2), &expected));
        }

        #[test]
        fn a_heartbeat_names_only_entries_the_follower_holds() {
            let mut raft = leader_over(&[]);
            raft.step(message(2, 2, accepted(1))).unwrap();
            sent(&mut raft);
            raft.tick(0);
            let commits: Vec<(node::Key, u64)> = sent(&mut raft)
                .iter()
                .map(|message| match message.body {
                    Body::Heartbeat { commit } => (message.to, commit),
                    ref body => panic!("{body:?}"),
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
}
