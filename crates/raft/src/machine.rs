use std::collections::{BTreeMap, BTreeSet, btree_map};
use std::ops::RangeBounds;

use types::node;

use crate::log;
use crate::log::{Held, Log, Run};
use crate::progress::Progress;
use crate::voters::Tally;
use crate::{
    Answer, Body, Change, Claim, Config, Data, Entry, Error, Grant, Hard, Link,
    Message, Position, Proof, Signature, Start, Term, Voters,
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

// The leader's view of one other node: its log progress, whether the next quorum
// check counts it, and whether it answered an append since it was last silent
// through a check. Only an append reply counts: a stale heartbeat gets the same
// `HeartbeatReply`.
#[derive(Debug)]
struct Peer {
    progress: Progress,
    active: bool,
    answered: bool,
}

impl Peer {
    fn new(last: u64) -> Self {
        Self {
            progress: Progress::new(last),
            active: false,
            answered: false,
        }
    }
}

// A message body that `check` passed.
enum Checked {
    PreVote { last: Position },
    PreVoteReply { answer: Answer },
    Vote { last: Position },
    VoteReply { answer: Answer },
    Heartbeat { commit: Held },
    HeartbeatReply,
    Append { run: Run, commit: u64 },
    AppendReply { last: Held },
    AppendReject { hint: Held },
}

// What `prove` decided: the links of the chain it read, and whether the proof and
// those links let this node take the message.
struct Verdict<'a> {
    read: &'a [Link],
    proven: bool,
}

/// What the caller must do after an input, in this order: [`sign`](Self::sign) this
/// node's grants, write `hard` and `entries` to disk and sync them, send `messages`,
/// then apply `committed`. Write `hard` and `entries` in any order: a crash between
/// the two is safe. `raft` is safe only when the disk keeps what it synced.
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

impl Ready {
    /// Gives each grant with no signature the signature that `sign` makes for its
    /// claim: this node's entry in the hard proof and in each message's proof, each
    /// grant it sends, and the votes and the signature of each change it wrote, in
    /// `entries`, in `committed`, in each chain, and in each append. Another node's
    /// grant or change keeps the signature it came with, so it has one when the
    /// caller checked its message (see [`Raft::claims`]).
    pub fn sign(&mut self, mut sign: impl FnMut(&Claim<'_>) -> Signature) {
        if let Some(hard) = &mut self.hard
            && let Some(proof) = &mut hard.proof
        {
            proof.sign(hard.term, &mut sign);
        }
        for entry in self.entries.iter_mut().chain(&mut self.committed) {
            entry.sign(&mut sign);
        }
        for message in &mut self.messages {
            message.sign(&mut sign);
        }
    }
}

/// One node's state machine. PreVote and CheckQuorum are always on.
///
/// After each call to [`tick`](Self::tick), [`step`](Self::step), or
/// [`campaign`](Self::campaign), take [`ready`](Self::ready) and do what it says.
#[derive(Debug)]
pub struct Raft {
    key: node::Key,
    voters: Voters,
    // Where the log holds `voters`: the zero position for `Start.voters`.
    in_force: Position,
    // The configuration before `voters`.
    before: Voters,
    // The other voters in force, plus the nodes that the configuration in force
    // removed, until a release, a quorum check, or the next configuration drops them.
    // Never this node.
    peers: BTreeMap<node::Key, Peer>,
    // The first answer of each other voter to the current campaign.
    answers: BTreeMap<node::Key, Answer>,
    // The leader's proof: the voters that elected it, itself included, with the
    // votes that arrive after the win. `None` in every other role.
    votes: Option<Proof>,
    election_ticks: u64,
    heartbeat_ticks: u64,
    term: Term,
    vote: Option<node::Key>,
    // The proof of `term`: see `Hard::proof`.
    proof: Option<Proof>,
    // The hard state that the last `Ready` gave.
    given: Hard,
    log: Log,
    role: Role,
    leader: Option<node::Key>,
    // The leader of `term`, once this node hears one or becomes one. It stays until
    // the term ends, through a step-down and a campaign; `leader` does not.
    led: Option<node::Key>,
    election_elapsed: u64,
    heartbeat_elapsed: u64,
    // The randomized election timeout. `None` until the first tick after a reset.
    timeout: Option<u64>,
    outbox: Vec<Message>,
}

impl Raft {
    /// Builds a follower. When the last entry has a higher term than `hard`, the
    /// node starts at that term with no vote: a crash came between the two writes of
    /// one [`Ready`], and the first [`ready`](Self::ready) gives the new `hard`.
    ///
    /// # Errors
    ///
    /// - [`Error::Ticks`] when `heartbeat_ticks` is 0 or `election_ticks` is not
    ///   greater than `heartbeat_ticks`.
    /// - [`Error::EmptyIncoming`] when `voters.incoming` is empty but `outgoing` is
    ///   not.
    /// - [`Error::NoVoters`] when a configuration in `entries` has an empty
    ///   `incoming` set.
    /// - [`Error::EntryOutOfOrder`] when `entries` do not run from index 1 with
    ///   terms that never decrease.
    /// - [`Error::AppliedPastLog`] when `applied` is past the last entry.
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
        let log = Log::new(voters, entries, applied)?;
        let last = log.last();
        let (in_force, voters) = log.voters();
        let voters = voters.clone();
        let before = log.voters_before(in_force.index);
        let peers = others(&log, key)
            .into_iter()
            .map(|key| (key, Peer::new(last.index)))
            .collect();
        let (term, vote, led, proof) = if hard.term < last.term {
            (last.term, None, None, None)
        } else {
            (hard.term, hard.vote, hard.leader, hard.proof.clone())
        };
        Ok(Self {
            key,
            voters,
            in_force,
            before,
            peers,
            answers: BTreeMap::new(),
            votes: None,
            outbox: Vec::new(),
            election_ticks: u64::from(election_ticks),
            heartbeat_ticks: u64::from(heartbeat_ticks),
            term,
            vote,
            proof,
            given: hard,
            log,
            role: Role::Follower,
            leader: None,
            led,
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

    /// The leader this node follows now, or itself when it leads. `None` after a
    /// step-down; [`Hard::leader`] keeps the leader of the term.
    #[must_use]
    pub fn leader(&self) -> Option<node::Key> {
        self.leader
    }

    /// The nodes whose votes count.
    #[must_use]
    pub fn voters(&self) -> &Voters {
        &self.voters
    }

    /// The state that must be on disk before a message of this term leaves.
    /// [`Ready::hard`] says when to write it.
    #[must_use]
    pub fn hard(&self) -> Hard {
        Hard {
            term: self.term,
            vote: self.vote,
            leader: self.led,
            proof: self.proof.clone(),
        }
    }

    /// Takes what the caller must do since the last call.
    pub fn ready(&mut self) -> Ready {
        let given = &self.given;
        let changed = (self.term, self.vote, self.led)
            != (given.term, given.vote, given.leader)
            || self.proof != given.proof;
        let hard = changed.then(|| self.hard());
        if let Some(hard) = &hard {
            self.given = hard.clone();
        }
        let mut messages = std::mem::take(&mut self.outbox);
        self.chain(&mut messages);
        Ready {
            hard,
            entries: self.log.take_unstable(),
            committed: self.log.take_committed(),
            messages,
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
        self.leading()?;
        Ok(self.propose_entry(Data::Bytes(data)))
    }

    /// Proposes `voters` as the voter set, as the leader. The group first enters a
    /// joint phase, where the old set and the new set each need a majority. When the
    /// joint entry commits, the leader proposes the entry that leaves the joint phase
    /// on its own. Returns the position of the joint entry. A node outside the new
    /// set gets the leave and its commit, then votes and follows but never
    /// campaigns; until the leave commits, it still campaigns, since it may yet have
    /// to lead the commit. A leader outside the new set steps down when the leave
    /// commits.
    ///
    /// # Errors
    ///
    /// - [`Error::NotLeader`] when this node does not lead.
    /// - [`Error::NoVoters`] when `voters` is empty.
    /// - [`Error::ChangePending`] when the last configuration entry in the log is
    ///   not committed yet. The leave follows a joint entry at once, so this covers
    ///   a joint phase that has not left.
    pub fn propose_voters(
        &mut self,
        voters: BTreeSet<node::Key>,
    ) -> Result<Position, Error> {
        self.leading()?;
        if voters.is_empty() {
            return Err(Error::NoVoters);
        }
        if !self.log.settled() {
            let (at, _) = self.log.voters();
            return Err(Error::ChangePending { at });
        }
        let joint = self.voters.enter(voters);
        Ok(self.propose_entry(Data::Voters(self.change(joint))))
    }

    // A configuration change with this leader's votes as its proof.
    fn change(&self, voters: Voters) -> Change {
        let proof = self.votes.clone();
        Change {
            voters,
            votes: proof.expect("invariant: only a leader writes a configuration"),
            signature: None,
        }
    }

    fn leading(&self) -> Result<(), Error> {
        if self.role == Role::Leader {
            return Ok(());
        }
        Err(Error::NotLeader {
            leader: self.leader,
        })
    }

    // Appends `data` as the leader and replicates it.
    fn propose_entry(&mut self, data: Data) -> Position {
        let at = self.log.push(self.term, data);
        self.sync_voters();
        if !self.advance() {
            self.replicate();
        }
        at
    }

    /// Moves the node's time forward by one tick. `random` is a fresh, uniformly
    /// random value. The node uses it to choose its next election timeout.
    ///
    /// A follower or candidate that reaches its election timeout starts an election,
    /// unless [`Raft::campaign`] would do nothing. A leader sends heartbeats, and
    /// steps down when it has not heard from a quorum for `election_ticks`.
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

    /// Starts an election now, without a wait for the election timeout. A leader, a
    /// node that is not a voter of its configuration in force (nor, while that one is
    /// not committed, of the one before it), and a node in the last term
    /// (`u64::MAX`) do nothing.
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
    /// - [`Error::Unproven`] when the message claims a higher term, or a leader of
    ///   this node's term that it did not prove, with no proof that a quorum of this
    ///   node's voters granted it, and no chain of configuration entries that leads
    ///   from this node's committed configuration to one the proof is a quorum of.
    /// - [`Error::EntryOutOfOrder`] when an append's entries do not follow its `prev`.
    /// - [`Error::NoVoters`] when an append carries a configuration with an empty
    ///   `incoming` set.
    /// - [`Error::TermBehindLog`] when an append carries an entry of a later term
    ///   than the message.
    /// - [`Error::IndexPastLog`] when a heartbeat, an append reply, or an append
    ///   reject names an index past this node's log.
    ///
    /// A message for a lower term is stale: it is answered or dropped with no check.
    /// A reply is dropped with no check when its sender is not in [`Raft::voters`],
    /// unless the configuration in force removed the sender and this node still sends
    /// to it. The node's state does not change on an error.
    pub fn step(&mut self, message: Message) -> Result<(), Error> {
        if !self.reads(&message)? {
            self.answer_stale(message.from, &message.body);
            return Ok(());
        }
        let Message {
            from,
            term,
            body,
            mut proof,
            chain,
            ..
        } = message;
        let body = self.check(from, term, body, proof.as_ref(), &chain)?;
        if self.meet(from, term, &body, &mut proof) {
            self.handle(from, term, body, proof);
        }
        Ok(())
    }

    /// Each claim of `message`, from another node to this one, that
    /// [`step`](Self::step) reads, with its signature: the grants of its proof in
    /// rising key order; then the votes and the change of each link of its chain
    /// that `step` reads; then the votes and the change of each configuration an
    /// append carries; then the sender's grant. A message for another node or from
    /// this node, a message for a lower term, and a reply from a node that is not a
    /// peer give no claim: `step` refuses the first two by their header and reads
    /// none of the others. The list is the one `step` reads only when `step` gets
    /// the same message, with no call to this node between the two. The caller
    /// checks each signature against its signer's key before `step`, and refuses a
    /// `None`: `step` keeps each signature as it came.
    pub fn claims<'a>(
        &'a self,
        message: &'a Message,
    ) -> impl Iterator<Item = (Claim<'a>, Option<Signature>)> + 'a {
        let Message {
            from,
            term,
            body,
            proof,
            chain,
            ..
        } = message;
        let read = matches!(self.reads(message), Ok(true)).then(|| {
            let links = self.prove(*from, *term, body, proof.as_ref(), chain).read;
            message.claims(links)
        });
        read.into_iter().flatten()
    }

    // Whether `step` reads a message past its header. `step` refuses a message for
    // another node or from this node with the error. A message for a lower term is
    // stale, and a reply from a node that is not a peer is dropped.
    fn reads(&self, message: &Message) -> Result<bool, Error> {
        if message.to != self.key {
            return Err(Error::Misrouted { to: message.to });
        }
        if message.from == self.key {
            return Err(Error::Loopback);
        }
        let peer = !message.body.answers() || self.peers.contains_key(&message.from);
        Ok(message.term >= self.term && peer)
    }

    // Applies a message that `check` and `meet` passed: one of this term, a PreVote
    // for a later one, or a granted PreVoteReply for a later one. `proof` is what
    // the message carried, unless `meet` took it.
    fn handle(
        &mut self,
        from: node::Key,
        term: Term,
        body: Checked,
        proof: Option<Proof>,
    ) {
        match body {
            Checked::PreVote { last } => {
                let granted = (term > self.term || self.free_for(from))
                    && last >= self.log.last();
                if granted {
                    let answer = Answer::Granted(None);
                    self.send(from, term, Body::PreVoteReply { answer }, None);
                } else if term > self.term {
                    let answer = Answer::Refused;
                    self.send(from, self.term, Body::PreVoteReply { answer }, None);
                } else {
                    // The candidate is one term behind, so the refusal is an
                    // answer to a stale message: it carries this term's proof.
                    self.answer_stale(from, &Body::PreVote { last });
                }
            }
            Checked::Vote { last } => {
                let answer = if self.free_for(from) && last >= self.log.last() {
                    self.election_elapsed = 0;
                    self.vote = Some(from);
                    Answer::Granted(None)
                } else {
                    Answer::Refused
                };
                self.send(from, self.term, Body::VoteReply { answer }, None);
            }
            Checked::PreVoteReply { answer } => {
                self.pre_vote_reply(from, term, answer);
            }
            Checked::VoteReply { answer } => match (self.role, answer) {
                (Role::Candidate, _) => self.poll(from, answer),
                // A vote that arrives after the win joins the votes the leader
                // carries.
                (Role::Leader, Answer::Granted(signature))
                    if self.voters.contains(from) =>
                {
                    let votes = self.votes.as_mut();
                    votes
                        .expect("invariant: a leader has its votes")
                        .voters
                        .entry(from)
                        .or_insert(signature);
                }
                (Role::Leader | Role::Follower | Role::PreCandidate, _) => {}
            },
            Checked::Heartbeat { commit } => self.heartbeat(from, commit, proof),
            Checked::HeartbeatReply => {
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
            Checked::Append { run, commit } => {
                self.follow(from, proof);
                self.append(from, run, commit);
            }
            Checked::AppendReply { last } => self.accepted(from, last),
            Checked::AppendReject { hint } => {
                if let Some(peer) = self.heard_from(from) {
                    peer.answered = true;
                    peer.progress.rejected(hint.index());
                    self.catch_up(from);
                }
            }
        }
    }

    // Checks a message of this term or a later one against the node's state and its
    // log. An append and a vote can name an index past the last one, because this
    // node can be behind.
    fn check(
        &self,
        from: node::Key,
        term: Term,
        body: Body,
        proof: Option<&Proof>,
        chain: &[Link],
    ) -> Result<Checked, Error> {
        if body.leads() && term == self.term && self.led.is_some_and(|led| led != from)
        {
            return Err(Error::SecondLeader { term, from });
        }
        if !self.prove(from, term, &body, proof, chain).proven {
            return Err(Error::Unproven { term, from });
        }
        Ok(match body {
            Body::PreVote { last } => Checked::PreVote { last },
            Body::PreVoteReply { answer } => Checked::PreVoteReply { answer },
            Body::Vote { last } => Checked::Vote { last },
            Body::VoteReply { answer } => Checked::VoteReply { answer },
            Body::Heartbeat { commit } => Checked::Heartbeat {
                commit: self.log.held(commit)?,
            },
            Body::HeartbeatReply => Checked::HeartbeatReply,
            Body::Append {
                prev,
                entries,
                commit,
            } => {
                let run = log::check(prev, entries)?;
                if let Some(last) = run.last().filter(|last| last.term > term) {
                    return Err(Error::TermBehindLog { term, last });
                }
                Checked::Append { run, commit }
            }
            Body::AppendReply { last } => Checked::AppendReply {
                last: self.log.held(last)?,
            },
            Body::AppendReject { hint } => Checked::AppendReject {
                hint: self.log.held(hint)?,
            },
        })
    }

    // Commits what the leader's heartbeat says and answers it.
    fn heartbeat(&mut self, leader: node::Key, commit: Held, proof: Option<Proof>) {
        self.follow(leader, proof);
        self.log.commit_to(commit.index());
        self.release_removed();
        self.send(leader, self.term, Body::HeartbeatReply, None);
    }

    // The last index a leader sends a removed node: the leave, or the leader's first
    // entry when that is later. An older leader can leave entries past the leave on
    // the node, such as a configuration that makes it a voter again. All are of a
    // lower term, so that entry replaces them.
    fn removed_end(&self) -> u64 {
        let (leave, _) = self.log.voters();
        leave.index.max(self.log.first_of(self.term))
    }

    // Releases the nodes a committed change removed: they leave the peers. A leader
    // keeps one until it holds all it gets, and sends it the commit as it goes.
    fn release_removed(&mut self) {
        if !self.log.settled() {
            return;
        }
        let end = self.removed_end();
        let leader = self.role == Role::Leader;
        let removed: Vec<node::Key> = self
            .peers
            .iter()
            .filter(|&(&key, peer)| {
                !self.voters.contains(key)
                    && (!leader || peer.progress.matched() >= end)
            })
            .map(|(&key, _)| key)
            .collect();
        for key in removed {
            if leader {
                self.send_heartbeat(key);
            }
            self.peers.remove(&key);
        }
    }

    // A follower commits what the heartbeat says, so it names only entries the
    // follower is known to hold.
    fn send_heartbeat(&mut self, to: node::Key) {
        let peer = &self.peers[&to];
        let commit = self.log.committed().min(peer.progress.matched());
        let proof = (!peer.answered).then(|| self.votes.clone()).flatten();
        self.send(to, self.term, Body::Heartbeat { commit }, proof);
    }

    // Appends the leader's entries as a follower and answers.
    fn append(&mut self, leader: node::Key, run: Run, commit: u64) {
        let reply = match self.log.append(run) {
            Ok(last) => {
                self.sync_voters();
                self.log.commit_to(commit.min(last));
                self.release_removed();
                Body::AppendReply { last }
            }
            Err(hint) => Body::AppendReject { hint },
        };
        self.send(leader, self.term, reply, None);
    }

    // Records that a follower holds the leader's log up to `last`, as the leader.
    fn accepted(&mut self, from: node::Key, last: Held) {
        let accepted = self.heard_from(from).is_some_and(|peer| {
            peer.answered = true;
            peer.progress.accepted(last.index())
        });
        if !accepted || self.advance() {
            return;
        }
        self.release_removed();
        self.catch_up(from);
    }

    // Notes that peer `from` answered this leader. `None` when this node does not
    // lead.
    fn heard_from(&mut self, from: node::Key) -> Option<&mut Peer> {
        if self.role != Role::Leader {
            return None;
        }
        let peer = self.peer_mut(from);
        peer.active = true;
        Some(peer)
    }

    // Commits the highest index that a quorum holds, when an entry of the leader's
    // own term is there, leaves a joint phase whose entry is committed, and sends
    // the followers the result. A group of one node commits the leave at once. A
    // leader outside the committed configuration steps down after the send. Returns
    // whether the commit index moved; when it did not, nothing is sent.
    fn advance(&mut self) -> bool {
        let mut moved = false;
        while self.commit_once() {
            moved = true;
            self.leave();
        }
        if !moved {
            return false;
        }
        self.release_removed();
        self.replicate();
        if self.log.settled() && !self.voters.incoming.contains(&self.key) {
            self.become_follower(self.term, None);
        }
        true
    }

    fn commit_once(&mut self) -> bool {
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
            self.log.commit_to(index);
            return true;
        }
        false
    }

    // Writes the entry that leaves a joint phase, once the joint configuration is
    // committed. `Start.voters` is committed by definition.
    fn leave(&mut self) {
        if !self.voters.joint() || !self.log.settled() {
            return;
        }
        let voters = self.voters.leave();
        let change = Data::Voters(self.change(voters));
        self.log.push(self.term, change);
        self.sync_voters();
    }

    // Puts the log's configuration in force. The peers become its other voters and
    // the nodes it removed: the other voters of the configuration before it. A new peer
    // starts at the end of the log.
    fn sync_voters(&mut self) {
        let (at, voters) = self.log.voters();
        // Equal voters at another position still change who was removed.
        if at == self.in_force {
            return;
        }
        self.in_force = at;
        self.before = self.log.voters_before(at.index);
        let last = self.log.last().index;
        let old = std::mem::replace(&mut self.voters, voters.clone());
        let keep = others(&self.log, self.key);
        self.peers.retain(|key, _| keep.contains(key));
        for key in keep {
            let peer = self.peers.entry(key).or_insert_with(|| Peer::new(last));
            // A node a change adds counts as heard until the next quorum check, even
            // when a removal left its peer, so the leader does not step down before
            // the node can answer.
            if self.voters.contains(key) && !old.contains(key) {
                peer.active = true;
            }
        }
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
    // index, unless the leader waits for its reply. A node that a change removed
    // gets entries only up to `removed_end`.
    fn send_appends<R: RangeBounds<node::Key>>(&mut self, range: R) {
        let commit = self.log.committed();
        let removed_end = self.removed_end();
        let (key, term, votes) = (self.key, self.term, &self.votes);
        for (&to, peer) in self.peers.range_mut(range) {
            if peer.progress.paused() {
                continue;
            }
            let next = peer.progress.next();
            let prev = self
                .log
                .at(next - 1)
                .expect("invariant: a follower's next entry follows the leader's log");
            let end = if self.voters.contains(to) {
                self.log.last().index
            } else {
                removed_end
            };
            let entries = self.log.slice(next, end, BATCH);
            let last = entries.last().map_or(prev.index, |entry| entry.at.index);
            peer.progress.sent(last);
            let proof = match votes {
                Some(votes) if !peer.answered => Some(votes.clone()),
                _ => None,
            };
            self.outbox.push(Message {
                from: key,
                to,
                term,
                body: Body::Append {
                    prev,
                    entries,
                    commit,
                },
                proof,
                chain: Vec::new(),
            });
        }
    }

    // Whether `proof` lets this node take the message, and the links of `chain` it
    // read to decide. A quorum of this node's voters must have granted the term. A
    // pre-vote and its grant claim no term. A leader's message needs its votes; a
    // vote request, its pre-votes; a reply, any proof of the term. In this node's own
    // term, only a leader's message needs a proof, and only while the node knows no
    // leader: `check` refused every other sender as a second one.
    fn prove<'a>(
        &self,
        from: node::Key,
        term: Term,
        body: &Body,
        proof: Option<&Proof>,
        chain: &'a [Link],
    ) -> Verdict<'a> {
        let unread = |proven| Verdict { read: &[], proven };
        if term == self.term && (!body.leads() || self.led.is_some()) {
            return unread(true);
        }
        let grant = match body {
            Body::PreVote { .. }
            | Body::PreVoteReply {
                answer: Answer::Granted(_),
            } => return unread(true),
            Body::Heartbeat { .. } | Body::Append { .. } => Some(Grant::Vote),
            Body::Vote { .. } => Some(Grant::PreVote),
            Body::PreVoteReply {
                answer: Answer::Refused,
            }
            | Body::VoteReply { .. }
            | Body::HeartbeatReply
            | Body::AppendReply { .. }
            | Body::AppendReject { .. } => None,
        };
        let Some(proof) = proof else {
            return unread(false);
        };
        let fits =
            grant.is_none_or(|grant| proof.grant == grant && proof.candidate == from);
        if !fits {
            return unread(false);
        }
        // A leader elected under the committed configuration can overwrite a
        // configuration entry that is not committed yet.
        let quorum =
            |voters: &Voters| voters.quorum(|key| proof.voters.contains_key(&key));
        if quorum(&self.voters) || quorum(&self.log.committed_voters()) {
            return unread(true);
        }
        self.walk(term, chain, quorum)
    }

    // Reads `chain` from its first link above the commit index until a link's
    // configuration is a `quorum`. Each link read must have a term below `term`,
    // rise from the position at the commit index or the last link read, hold
    // voters, and hold the votes of a quorum of the configuration that elected its
    // leader: the last link read of a lower term, else the last committed
    // configuration entry of a lower term.
    fn walk<'a>(
        &self,
        term: Term,
        chain: &'a [Link],
        quorum: impl Fn(&Voters) -> bool,
    ) -> Verdict<'a> {
        let committed = self.log.committed();
        // A linear skip: a hostile chain need not be sorted, and a binary search
        // over one lets a link that `claims` never lists decide the verdict.
        let above = chain.iter().position(|link| link.at.index > committed);
        let chain = &chain[above.unwrap_or(chain.len())..];
        let Some(first) = chain.first() else {
            return Verdict {
                read: chain,
                proven: false,
            };
        };
        let mut prev = self
            .log
            .at(committed)
            .expect("invariant: the commit index is in the log");
        // The links rise in term, so the last link read of a lower term is the last
        // link read when the term rises.
        let mut trusted = self.log.committed_voters_below(first.at.term);
        let mut last: Option<&Voters> = None;
        for (read, link) in chain.iter().enumerate() {
            let read = &chain[..=read];
            if let Some(last) = last.filter(|_| link.at.term > prev.term) {
                trusted = last.clone();
            }
            let rises = link.at.index > prev.index && link.at.term >= prev.term;
            let votes = &link.change.votes;
            let elected = rises
                && link.at.term < term
                && votes.grant == Grant::Vote
                && !link.change.voters.incoming.is_empty()
                && trusted.quorum(|key| votes.voters.contains_key(&key));
            if !elected {
                return Verdict {
                    read,
                    proven: false,
                };
            }
            if quorum(&link.change.voters) {
                return Verdict { read, proven: true };
            }
            last = Some(&link.change.voters);
            prev = link.at;
        }
        Verdict {
            read: chain,
            proven: false,
        }
    }

    // Steps down for a message of a higher term that `check` passed, except a PreVote
    // or its grant, and takes its proof. Returns false, so the message is dropped,
    // only for a PreVote or Vote of a higher term while this node has a lease.
    fn meet(
        &mut self,
        from: node::Key,
        term: Term,
        body: &Checked,
        proof: &mut Option<Proof>,
    ) -> bool {
        if term <= self.term {
            return true;
        }
        let leader = match body {
            // A voter that heard from a leader within the election timeout does
            // not help to replace it.
            Checked::PreVote { .. } | Checked::Vote { .. } if self.leased() => {
                return false;
            }
            // A PreVote, or its grant, carries a term that no node is in yet.
            Checked::PreVote { .. }
            | Checked::PreVoteReply {
                answer: Answer::Granted(_),
            } => {
                return true;
            }
            Checked::Heartbeat { .. } | Checked::Append { .. } => Some(from),
            Checked::Vote { .. }
            | Checked::PreVoteReply {
                answer: Answer::Refused,
            }
            | Checked::VoteReply { .. }
            | Checked::HeartbeatReply
            | Checked::AppendReply { .. }
            | Checked::AppendReject { .. } => None,
        };
        self.become_follower(term, leader);
        self.proof = proof.take();
        true
    }

    // Answers a message that `step` does not read: a request for a lower term gets
    // this term with its proof, so that its sender learns it, and a reply is
    // dropped. The answer carries this node's chain, which can fall short of the
    // sender's configuration when this node took the term through a chain it does
    // not hold: the leader's chain then moves it.
    fn answer_stale(&mut self, from: node::Key, body: &Body) {
        let reply = match body {
            // The reply carries the higher term, so a stale leader steps down and
            // a node that is ahead of its group can be elected.
            Body::Heartbeat { .. } | Body::Append { .. } => Body::HeartbeatReply,
            Body::PreVote { .. } => Body::PreVoteReply {
                answer: Answer::Refused,
            },
            Body::Vote { .. }
            | Body::PreVoteReply { .. }
            | Body::VoteReply { .. }
            | Body::HeartbeatReply
            | Body::AppendReply { .. }
            | Body::AppendReject { .. } => return,
        };
        // A refusal moves the sender only with a proof of this term.
        let Some(proof) = self.proof.clone() else {
            return;
        };
        self.send(from, self.term, reply, Some(proof));
    }

    fn tick_leader(&mut self) {
        self.heartbeat_elapsed += 1;
        if self.election_elapsed >= self.election_ticks {
            self.election_elapsed = 0;
            let heard = self
                .voters
                .quorum(|key| key == self.key || self.peer(key).active);
            // A removed node that answered nothing since the last check is released.
            let voters = &self.voters;
            self.peers
                .retain(|&key, peer| peer.active || voters.contains(key));
            // A peer silent through a check may have lost the leader with its hard
            // state, so it gets the votes again.
            for peer in self.peers.values_mut() {
                peer.answered = peer.answered && peer.active;
                peer.active = false;
            }
            if !heard {
                self.become_follower(self.term, None);
                return;
            }
        }
        if self.heartbeat_elapsed >= self.heartbeat_ticks {
            self.heartbeat_elapsed = 0;
            let peers: Vec<node::Key> = self.peers.keys().copied().collect();
            for to in peers {
                self.send_heartbeat(to);
            }
        }
    }

    // Handles a heartbeat or an append from the leader of the node's own term, with
    // the proof the message carried.
    fn follow(&mut self, leader: node::Key, proof: Option<Proof>) {
        self.led = Some(leader);
        // A node that its log alone put in the term proves it by its leader's votes.
        if self.proof.is_none() {
            self.proof = proof;
        }
        match self.role {
            Role::Leader => unreachable!(
                "invariant: `check` refuses a second leader of term {}",
                self.term
            ),
            Role::Follower => {
                self.election_elapsed = 0;
                self.leader = Some(leader);
            }
            Role::PreCandidate | Role::Candidate => {
                self.become_follower(self.term, Some(leader));
            }
        }
    }

    fn pre_campaign(&mut self) {
        // No term is left to campaign in.
        let Some(next) = self.term.next() else {
            return;
        };
        self.answers.clear();
        self.leader = None;
        self.role = Role::PreCandidate;
        self.broadcast(
            next,
            &Body::PreVote {
                last: self.log.last(),
            },
            None,
        );
        self.decide();
    }

    fn become_candidate(&mut self) {
        let next = self
            .term
            .next()
            .expect("invariant: a pre-candidate's term has a next term");
        // `granted` reads the answers that `reset` clears.
        let voters = self.granted();
        self.reset(next);
        self.proof = Some(Proof {
            grant: Grant::PreVote,
            candidate: self.key,
            voters,
        });
        self.vote = Some(self.key);
        self.role = Role::Candidate;
        let proof = self.proof.clone();
        self.broadcast(
            self.term,
            &Body::Vote {
                last: self.log.last(),
            },
            proof.as_ref(),
        );
        self.decide();
    }

    // A grant carries the term the pre-campaign asked for, a refusal the voter's own
    // term. A grant of this node's term from a voter came late to the campaign.
    fn pre_vote_reply(&mut self, from: node::Key, term: Term, answer: Answer) {
        let asked = match answer {
            Answer::Granted(_) => self.term.next(),
            Answer::Refused => Some(self.term),
        };
        if self.role == Role::PreCandidate && asked == Some(term) {
            self.poll(from, answer);
        } else if let Answer::Granted(signature) = answer
            && self.role == Role::Candidate
            && term == self.term
            && self.voters.contains(from)
        {
            self.join_pre_vote(from, signature);
        }
    }

    // Adds a pre-vote that arrived after the campaign to the candidate's proof and
    // asks the voters that have not answered again. A voter whose configuration the
    // first proof did not cover may take the larger one.
    fn join_pre_vote(&mut self, from: node::Key, signature: Option<Signature>) {
        let proof = self.proof.as_mut();
        let voters = &mut proof
            .expect("invariant: a candidate has its pre-votes")
            .voters;
        let btree_map::Entry::Vacant(entry) = voters.entry(from) else {
            return;
        };
        entry.insert(signature);
        let proof = self.proof.clone();
        let last = self.log.last();
        let unanswered: Vec<node::Key> = self
            .peers
            .keys()
            .filter(|&&peer| !self.answers.contains_key(&peer))
            .copied()
            .collect();
        for to in unanswered {
            self.send(to, self.term, Body::Vote { last }, proof.clone());
        }
    }

    // The voters that granted the current campaign with their signatures, this node
    // included with none.
    fn granted(&self) -> BTreeMap<node::Key, Option<Signature>> {
        let granted = |(&key, &answer)| match answer {
            Answer::Granted(signature) => Some((key, signature)),
            Answer::Refused => None,
        };
        self.answers
            .iter()
            .filter_map(granted)
            .chain([(self.key, None)])
            .collect()
    }

    fn become_leader(&mut self) {
        // `granted` reads the answers that `reset` clears.
        let voters = self.granted();
        self.reset(self.term);
        self.votes = Some(Proof {
            grant: Grant::Vote,
            candidate: self.key,
            voters,
        });
        self.leader = Some(self.key);
        self.led = Some(self.key);
        self.role = Role::Leader;
        let last = self.log.last().index;
        for peer in self.peers.values_mut() {
            peer.progress = Progress::new(last);
        }
        // An entry of the leader's own term lets it commit the ones before it.
        self.log.push(self.term, Data::Empty);
        // A leader always has a log entry for a joint configuration in force, so
        // `propose_voters` reads the log alone.
        self.leave();
        if !self.advance() {
            self.replicate();
        }
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
            self.led = None;
            self.proof = None;
        }
        self.leader = None;
        self.election_elapsed = 0;
        self.heartbeat_elapsed = 0;
        self.timeout = None;
        self.answers.clear();
        self.votes = None;
        for peer in self.peers.values_mut() {
            peer.active = false;
            peer.answered = false;
        }
    }

    // Records one answer to the current campaign and acts when the answers decide it.
    // An answer from a node that is not a voter has no effect.
    fn poll(&mut self, from: node::Key, answer: Answer) {
        if self.voters.contains(from) {
            self.answers.entry(from).or_insert(answer);
            self.decide();
        }
    }

    // Acts when the answers decide the current campaign. This node grants itself.
    fn decide(&mut self) {
        let answer = |key| {
            if key == self.key {
                Some(true)
            } else {
                let granted = |answer: &Answer| matches!(answer, Answer::Granted(_));
                self.answers.get(&key).map(granted)
            }
        };
        match self.voters.tally(answer) {
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

    // An uncommitted configuration may still be truncated, and a removed leader
    // must be able to win the election that commits its leave.
    fn promotable(&self) -> bool {
        self.voters.contains(self.key)
            || (!self.log.settled() && self.before.contains(self.key))
    }

    fn peer(&self, key: node::Key) -> &Peer {
        self.peers
            .get(&key)
            .expect("invariant: every other voter has a peer")
    }

    fn peer_mut(&mut self, key: node::Key) -> &mut Peer {
        self.peers
            .get_mut(&key)
            .expect("invariant: `step` drops a reply from a node that is not a peer")
    }

    // Sends `body` to each peer, with `proof`.
    fn broadcast(&mut self, term: Term, body: &Body, proof: Option<&Proof>) {
        for &to in self.peers.keys() {
            self.outbox.push(Message {
                from: self.key,
                to,
                term,
                body: body.clone(),
                proof: proof.cloned(),
                chain: Vec::new(),
            });
        }
    }

    // Sends `body` to `to`, with `proof`.
    fn send(&mut self, to: node::Key, term: Term, body: Body, proof: Option<Proof>) {
        self.outbox.push(Message {
            from: self.key,
            to,
            term,
            body,
            proof,
            chain: Vec::new(),
        });
    }

    // Gives each message with a proof the chain of this node's configuration
    // entries below its term.
    fn chain(&self, messages: &mut [Message]) {
        let mut chains: BTreeMap<Term, Vec<Link>> = BTreeMap::new();
        for message in messages
            .iter_mut()
            .filter(|message| message.proof.is_some())
        {
            message.chain = chains
                .entry(message.term)
                .or_insert_with(|| self.log.links(message.term))
                .clone();
        }
    }
}

// The nodes that `log` keeps a peer for on node `key`: every node but `key`.
fn others(log: &Log, key: node::Key) -> BTreeSet<node::Key> {
    let mut nodes = log.nodes();
    nodes.remove(&key);
    nodes
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

    const GRANTED: Answer = Answer::Granted(None);
    const REFUSED: Answer = Answer::Refused;

    // The signature of voter `id` for `grant`: each pair has its own.
    fn signature(grant: Grant, id: u8) -> Signature {
        let mut bytes = [id; 64];
        bytes[0] = match grant {
            Grant::PreVote => 0,
            Grant::Vote => 1,
        };
        Signature(bytes)
    }

    fn position(term: u64, index: u64) -> Position {
        Position {
            term: Term(term),
            index,
        }
    }

    // `voters` as a change that `leader` wrote with its own vote alone.
    fn change(leader: u8, voters: Voters) -> Data {
        Data::Voters(Change {
            voters,
            votes: Proof {
                grant: Grant::Vote,
                candidate: key(leader),
                voters: [(key(leader), None)].into(),
            },
            signature: None,
        })
    }

    // The configuration entry node 1 writes at `index` in `term` as the leader that
    // the voters `elected` elected, before `Ready::sign`: its own vote is unsigned.
    fn written(term: u64, index: u64, voters: Voters, elected: &[u8]) -> Entry {
        let proof = Proof {
            grant: Grant::Vote,
            candidate: key(1),
            voters: elected
                .iter()
                .map(|&id| (key(id), (id != 1).then(|| signature(Grant::Vote, id))))
                .collect(),
        };
        Entry {
            at: position(term, index),
            data: Data::Voters(Change {
                voters,
                votes: proof,
                signature: None,
            }),
        }
    }

    fn append(prev: Position, entries: Vec<Entry>, commit: u64) -> Body {
        Body::Append {
            prev,
            entries,
            commit,
        }
    }

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
            at: position(term, index),
            data: Data::Empty,
        };
        positions.iter().map(entry).collect()
    }

    fn raft(voters: &[u8], hard: Hard) -> Raft {
        Raft::new(CONFIG, start(voters, hard)).unwrap()
    }

    // The message `from` sends node 1 in `term`, with the proof a correct sender
    // carries: its votes on a leader's message, its pre-votes on a vote request, a
    // proof of the term on a reply, and none on a pre-vote or its grant. A grant
    // carries the sender's signature.
    fn message(from: u8, term: u64, mut body: Body) -> Message {
        let grant = if matches!(body, Body::VoteReply { .. }) {
            Grant::Vote
        } else {
            Grant::PreVote
        };
        if let Body::PreVoteReply { answer } | Body::VoteReply { answer } = &mut body
            && *answer == GRANTED
        {
            *answer = Answer::Granted(Some(signature(grant, from)));
        }
        let grant = match body {
            Body::PreVote { .. }
            | Body::PreVoteReply {
                answer: Answer::Granted(_),
            } => None,
            Body::Vote { .. } => Some(Grant::PreVote),
            Body::PreVoteReply { answer: REFUSED }
            | Body::VoteReply { .. }
            | Body::Heartbeat { .. }
            | Body::HeartbeatReply
            | Body::Append { .. }
            | Body::AppendReply { .. }
            | Body::AppendReject { .. } => Some(Grant::Vote),
        };
        Message {
            from: key(from),
            to: key(1),
            term: Term(term),
            body,
            proof: grant.map(|grant| proof(grant, from, &[from, 2, 3, 4, 5])),
            chain: Vec::new(),
        }
    }

    fn unproven(from: u8, term: u64, body: Body) -> Message {
        Message {
            proof: None,
            ..message(from, term, body)
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
                leader: Some(key(1)),
                proof: Some(proof(Grant::PreVote, 1, &[1])),
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
                leader: None,
                proof: None,
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
                proof: None,
                chain: Vec::new(),
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

        // A crash between the entry write and the hard write of one `Ready`.
        #[test]
        fn restarts_after_a_crash_between_the_entry_write_and_the_hard_write() {
            let hard = Hard {
                term: Term(2),
                vote: Some(key(2)),
                leader: None,
                proof: None,
            };
            let start = Start {
                entries: entries(&[(1, 1), (3, 2), (3, 3)]),
                ..start(&[1, 2, 3], hard)
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            let expected = Hard {
                term: Term(3),
                vote: None,
                leader: None,
                proof: None,
            };
            assert_eq!(
                (raft.term(), raft.role(), raft.hard()),
                (Term(3), Role::Follower, expected.clone())
            );
            let ready = raft.ready();
            assert_eq!(
                (ready.hard, ready.entries, ready.messages),
                (Some(expected), vec![], vec![])
            );
            assert_eq!(raft.ready(), Ready::default());
        }

        #[test]
        fn keeps_its_vote_and_gives_no_hard_when_the_log_ends_in_the_hard_term() {
            let hard = Hard {
                term: Term(1),
                vote: Some(key(2)),
                leader: None,
                proof: None,
            };
            let start = Start {
                entries: entries(&[(1, 1)]),
                ..start(&[1, 2, 3], hard.clone())
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            assert_eq!(raft.hard(), hard);
            assert_eq!(raft.ready(), Ready::default());
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
                proof: None,
                chain: Vec::new(),
            };
            assert_eq!(sent(&mut raft), [prevote(2), prevote(3)]);
            assert_eq!(raft.hard(), Hard::default());
        }

        #[test]
        fn makes_a_leader_send_a_heartbeat_every_heartbeat_ticks() {
            let mut raft = raft(&[1, 2], Hard::default());
            raft.campaign();
            for body in [
                Body::PreVoteReply { answer: GRANTED },
                Body::VoteReply { answer: GRANTED },
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
                proof: Some(proof(Grant::Vote, 1, &[1, 2])),
                chain: Vec::new(),
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
            let rejected = Body::PreVoteReply { answer: REFUSED };
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
        fn rejects_a_second_leader_in_the_term_of_the_leader_it_follows() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            raft.step(message(2, 1, Body::Heartbeat { commit: 0 }))
                .unwrap();
            sent(&mut raft);
            let entry = Entry {
                at: Position {
                    term: Term(1),
                    index: 1,
                },
                data: change(
                    2,
                    Voters {
                        incoming: [key(1)].into_iter().collect(),
                        ..Voters::default()
                    },
                ),
            };
            let append = Body::Append {
                prev: Position::default(),
                entries: vec![entry],
                commit: 0,
            };
            for body in [Body::Heartbeat { commit: 0 }, append] {
                let err = raft.step(message(3, 1, body)).unwrap_err();
                assert_eq!(
                    err,
                    Error::SecondLeader {
                        term: Term(1),
                        from: key(3)
                    }
                );
                assert_eq!(
                    err.to_string(),
                    "node 00000000000000000000000000000003 also claims to lead term 1"
                );
            }
            assert_eq!((raft.role(), raft.leader()), (Role::Follower, Some(key(2))));
            let ready = raft.ready();
            assert_eq!((ready.messages, ready.entries), (vec![], vec![]));
            assert_eq!(
                raft.step(message(2, 1, Body::Heartbeat { commit: 0 })),
                Ok(())
            );
        }

        #[test]
        fn refuses_an_unproven_leader_of_a_term_it_is_in() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            let heartbeat = Body::Heartbeat { commit: 0 };
            let err = raft.step(unproven(3, 1, heartbeat.clone())).unwrap_err();
            let expected = Error::Unproven {
                term: Term(1),
                from: key(3),
            };
            assert_eq!(err, expected);
            assert_eq!(
                err.to_string(),
                "node 00000000000000000000000000000003 claims term 1 with no proof \
                 this node accepts"
            );
            assert_eq!((raft.role(), raft.leader()), (Role::Follower, None));
            assert_eq!(raft.ready(), Ready::default());
            raft.step(message(3, 1, heartbeat)).unwrap();
            assert_eq!((raft.role(), raft.leader()), (Role::Follower, Some(key(3))));
        }

        #[test]
        fn refuses_an_unproven_leader_of_a_term_it_voted_in() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            let vote = Body::Vote {
                last: Position::default(),
            };
            raft.step(message(2, 1, vote)).unwrap();
            let heartbeat = Body::Heartbeat { commit: 0 };
            let err = raft.step(unproven(3, 1, heartbeat.clone())).unwrap_err();
            let expected = Error::Unproven {
                term: Term(1),
                from: key(3),
            };
            assert_eq!(err, expected);
            assert_eq!((raft.role(), raft.leader()), (Role::Follower, None));
            raft.step(message(3, 1, heartbeat)).unwrap();
            assert_eq!((raft.role(), raft.leader()), (Role::Follower, Some(key(3))));
        }

        #[test]
        fn rejects_a_second_leader_after_its_election_timeout_in_the_term() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            raft.step(message(2, 1, Body::Heartbeat { commit: 0 }))
                .unwrap();
            tick_times(&mut raft, 10);
            assert_eq!((raft.role(), raft.leader()), (Role::PreCandidate, None));
            sent(&mut raft);
            let err = raft
                .step(message(3, 1, Body::Heartbeat { commit: 0 }))
                .unwrap_err();
            let expected = Error::SecondLeader {
                term: Term(1),
                from: key(3),
            };
            assert_eq!(err, expected);
            raft.step(message(2, 1, Body::Heartbeat { commit: 0 }))
                .unwrap();
            assert_eq!((raft.role(), raft.leader()), (Role::Follower, Some(key(2))));
        }

        #[test]
        fn rejects_an_append_for_a_term_it_leads() {
            let mut raft = raft(&[1], Hard::default());
            raft.campaign();
            let body = append(position(1, 1), vec![], 0);
            let err = raft.step(message(2, 1, body)).unwrap_err();
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
            let granted = Body::VoteReply { answer: GRANTED };
            assert_eq!(sent(&mut first)[0].body, granted);

            let mut restarted = raft(&[1, 2, 3], first.hard());
            restarted.step(message(3, 1, vote.clone())).unwrap();
            let rejected = Body::VoteReply { answer: REFUSED };
            assert_eq!(sent(&mut restarted)[0].body, rejected);
            restarted.step(message(2, 1, vote.clone())).unwrap();
            assert_eq!(sent(&mut restarted)[0].body, granted);
        }

        #[test]
        fn counts_only_the_first_answer_of_a_voter() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            raft.campaign();
            let rejected = Body::PreVoteReply { answer: REFUSED };
            raft.step(message(2, 0, rejected.clone())).unwrap();
            let granted = Body::PreVoteReply { answer: GRANTED };
            raft.step(message(2, 1, granted.clone())).unwrap();
            assert_eq!(raft.role(), Role::PreCandidate);
            raft.step(message(3, 1, granted.clone())).unwrap();
            assert_eq!(raft.role(), Role::Candidate);
        }

        #[test]
        fn ignores_an_answer_from_a_node_that_is_not_a_voter() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            raft.campaign();
            let granted = Body::PreVoteReply { answer: GRANTED };
            raft.step(message(9, 1, granted.clone())).unwrap();
            assert_eq!(raft.role(), Role::PreCandidate);
        }

        #[test]
        fn becomes_follower_when_a_quorum_rejects() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            raft.campaign();
            let rejected = Body::PreVoteReply { answer: REFUSED };
            raft.step(message(2, 0, rejected.clone())).unwrap();
            assert_eq!(raft.role(), Role::PreCandidate);
            raft.step(message(3, 0, rejected.clone())).unwrap();
            assert_eq!((raft.role(), raft.term()), (Role::Follower, Term(0)));
        }

        // The candidate is not behind in term, so the refusal carries no proof.
        #[test]
        fn refuses_a_pre_vote_for_a_later_term_from_a_shorter_log_without_a_proof() {
            let start = Start {
                entries: entries(&[(1, 1)]),
                ..start(&[1, 2, 3], with_proof(1))
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            let last = Position::default();
            raft.step(message(2, 2, Body::PreVote { last })).unwrap();
            let refusal = Message {
                from: key(1),
                to: key(2),
                term: Term(1),
                body: Body::PreVoteReply { answer: REFUSED },
                proof: None,
                chain: Vec::new(),
            };
            assert_eq!(sent(&mut raft), [refusal]);
            assert_eq!((raft.term(), raft.role()), (Term(1), Role::Follower));
        }

        #[test]
        fn answers_a_heartbeat_from_a_lower_term_with_its_own_term() {
            let mut raft = raft(&[1, 2, 3], with_proof(5));
            raft.step(message(2, 4, Body::Heartbeat { commit: 0 }))
                .unwrap();
            let reply = Message {
                from: key(1),
                to: key(2),
                term: Term(5),
                body: Body::HeartbeatReply,
                proof: with_proof(5).proof,
                chain: Vec::new(),
            };
            assert_eq!(sent(&mut raft), [reply]);
            assert_eq!(raft.leader(), None);
        }

        #[test]
        fn drops_a_reply_from_a_node_that_is_not_a_peer() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            let hard = raft.hard();
            let bodies = [
                Body::PreVoteReply { answer: REFUSED },
                Body::VoteReply { answer: REFUSED },
                Body::HeartbeatReply,
                Body::AppendReply { last: 9 },
                Body::AppendReject { hint: 9 },
            ];
            for term in [1, 5] {
                for body in &bodies {
                    raft.step(message(9, term, body.clone())).unwrap();
                    let case = format!("{body:?} in term {term}");
                    let state = (raft.role(), raft.hard());
                    assert_eq!(state, (Role::Leader, hard.clone()), "{case}");
                    assert_eq!(sent(&mut raft), [], "{case}");
                }
            }
            raft.step(message(3, 5, Body::HeartbeatReply)).unwrap();
            assert_eq!((raft.role(), raft.term()), (Role::Follower, Term(5)));
        }

        #[test]
        fn a_campaign_drops_a_reply_from_a_node_that_is_not_a_peer() {
            for granted in [false, true] {
                let mut raft = raft(&[1, 2, 3], Hard::default());
                raft.campaign();
                if granted {
                    let reply = Body::PreVoteReply { answer: GRANTED };
                    raft.step(message(2, 1, reply)).unwrap();
                }
                let (role, hard) = (raft.role(), raft.hard());
                sent(&mut raft);
                let bodies = [
                    Body::PreVoteReply { answer: REFUSED },
                    Body::VoteReply { answer: REFUSED },
                ];
                for body in bodies {
                    raft.step(message(9, 5, body.clone())).unwrap();
                    let case = format!("{role:?} gets {body:?}");
                    let state = (raft.role(), raft.hard());
                    assert_eq!(state, (role, hard.clone()), "{case}");
                    assert_eq!(sent(&mut raft), [], "{case}");
                }
            }
        }
    }

    // Makes `raft` win the election it campaigns for, with grants from `from`.
    fn elect(raft: &mut Raft, from: &[u8]) {
        raft.campaign();
        let term = raft.term().0 + 1;
        for granted in [
            Body::PreVoteReply { answer: GRANTED },
            Body::VoteReply { answer: GRANTED },
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
            leader: None,
            proof: None,
        }
    }

    // The hard state of a node that a refusal from node 3 moved to `term`.
    fn with_proof(term: u64) -> Hard {
        Hard {
            proof: Some(proof(Grant::PreVote, 3, &[1, 3])),
            ..at_term(term)
        }
    }

    fn tick_times(raft: &mut Raft, times: u32) {
        for _ in 0..times {
            raft.tick(0);
        }
    }

    // The proof of `voters` for `candidate`, each signed but node 1 in its own proof.
    fn proof(grant: Grant, candidate: u8, voters: &[u8]) -> Proof {
        let entry = |id| {
            let signed = id != 1 || candidate != 1;
            (key(id), signed.then(|| signature(grant, id)))
        };
        Proof {
            grant,
            candidate: key(candidate),
            voters: voters.iter().copied().map(entry).collect(),
        }
    }

    mod proof {
        use super::*;

        fn heartbeat(to: u8, term: u64, proof: Option<Proof>) -> Message {
            Message {
                from: key(1),
                to: key(to),
                term: Term(term),
                body: Body::Heartbeat { commit: 0 },
                proof,
                chain: Vec::new(),
            }
        }

        #[test]
        fn a_candidate_carries_its_pre_votes() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            raft.campaign();
            sent(&mut raft);
            let granted = Body::PreVoteReply { answer: GRANTED };
            raft.step(message(2, 2, granted)).unwrap();
            let expected = proof(Grant::PreVote, 1, &[1, 2]);
            let vote = |to| Message {
                from: key(1),
                to: key(to),
                term: Term(2),
                body: Body::Vote {
                    last: Position::default(),
                },
                proof: Some(expected.clone()),
                chain: Vec::new(),
            };
            assert_eq!(sent(&mut raft), [vote(2), vote(3)]);
            let hard = Hard {
                term: Term(2),
                vote: Some(key(1)),
                leader: None,
                proof: Some(expected),
            };
            assert_eq!(raft.hard(), hard);
        }

        #[test]
        fn a_candidate_carries_the_signatures_of_its_pre_votes() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            raft.campaign();
            sent(&mut raft);
            raft.step(message(2, 2, Body::PreVoteReply { answer: GRANTED }))
                .unwrap();
            let signed = Some(signature(Grant::PreVote, 2));
            let voters = BTreeMap::from([(key(1), None), (key(2), signed)]);
            for vote in sent(&mut raft) {
                assert_eq!(vote.proof.unwrap().voters, voters);
            }
            assert_eq!(raft.hard().proof.unwrap().voters, voters);
        }

        #[test]
        fn a_leader_carries_the_signatures_of_its_votes() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            raft.tick(0);
            let signed = Some(signature(Grant::Vote, 2));
            let voters = BTreeMap::from([(key(1), None), (key(2), signed)]);
            for heartbeat in sent(&mut raft) {
                assert_eq!(heartbeat.proof.unwrap().voters, voters);
            }
        }

        #[test]
        fn a_late_pre_vote_joins_with_its_signature() {
            let mut raft = raft(&[1, 2, 3, 4], at_term(1));
            raft.campaign();
            for from in [2, 3] {
                raft.step(message(from, 2, Body::PreVoteReply { answer: GRANTED }))
                    .unwrap();
            }
            raft.step(message(4, 2, Body::PreVoteReply { answer: GRANTED }))
                .unwrap();
            let voters = raft.hard().proof.unwrap().voters;
            let signed = Some(signature(Grant::PreVote, 4));
            assert_eq!(voters.get(&key(4)), Some(&signed));
        }

        #[test]
        fn a_late_vote_joins_with_its_signature() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            raft.step(message(3, 1, Body::VoteReply { answer: GRANTED }))
                .unwrap();
            raft.tick(0);
            let voters = sent(&mut raft).remove(0).proof.unwrap().voters;
            let signed = Some(signature(Grant::Vote, 3));
            assert_eq!(voters.get(&key(3)), Some(&signed));
        }

        #[test]
        fn grants_with_no_signature() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            let last = Position::default();
            raft.step(message(2, 1, Body::PreVote { last })).unwrap();
            raft.step(message(2, 1, Body::Vote { last })).unwrap();
            let bodies: Vec<Body> =
                sent(&mut raft).into_iter().map(|m| m.body).collect();
            let expected = [
                Body::PreVoteReply { answer: GRANTED },
                Body::VoteReply { answer: GRANTED },
            ];
            assert_eq!(bodies, expected);
        }

        #[test]
        fn a_restart_keeps_the_signatures_of_its_proof() {
            let signed = Proof {
                grant: Grant::PreVote,
                candidate: key(1),
                voters: [1, 2]
                    .map(|id| (key(id), Some(signature(Grant::PreVote, id))))
                    .into(),
            };
            let hard = Hard {
                term: Term(2),
                vote: Some(key(1)),
                leader: Some(key(1)),
                proof: Some(signed.clone()),
            };
            let mut raft = raft(&[1, 2, 3], hard);
            raft.step(message(3, 1, Body::Heartbeat { commit: 0 }))
                .unwrap();
            assert_eq!(sent(&mut raft).remove(0).proof, Some(signed));
        }

        #[test]
        fn a_late_pre_vote_joins_the_candidates_proof_and_asks_again() {
            let mut raft = raft(&[1, 2, 3, 4], at_term(1));
            raft.campaign();
            sent(&mut raft);
            let granted = Body::PreVoteReply { answer: GRANTED };
            raft.step(message(2, 2, granted.clone())).unwrap();
            raft.step(message(3, 2, granted.clone())).unwrap();
            sent(&mut raft);
            raft.step(message(2, 2, Body::VoteReply { answer: GRANTED }))
                .unwrap();
            raft.step(message(4, 2, granted)).unwrap();
            let expected = proof(Grant::PreVote, 1, &[1, 2, 3, 4]);
            let vote = |to| Message {
                from: key(1),
                to: key(to),
                term: Term(2),
                body: Body::Vote {
                    last: Position::default(),
                },
                proof: Some(expected.clone()),
                chain: Vec::new(),
            };
            let ready = raft.ready();
            assert_eq!(ready.messages, [vote(3), vote(4)]);
            let hard = Hard {
                term: Term(2),
                vote: Some(key(1)),
                leader: None,
                proof: Some(expected),
            };
            assert_eq!(ready.hard, Some(hard));
            assert_eq!(raft.role(), Role::Candidate);
        }

        #[test]
        fn a_late_pre_vote_from_a_node_that_is_not_a_voter_does_not_join() {
            // Node 4 stays a peer while the leave that removes it is uncommitted.
            let leave = Voters {
                incoming: [1, 2, 3].into_iter().map(key).collect(),
                ..Voters::default()
            };
            let start = Start {
                entries: vec![Entry {
                    at: position(1, 1),
                    data: change(2, leave),
                }],
                ..start(&[1, 2, 3, 4], at_term(1))
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            raft.campaign();
            let granted = Body::PreVoteReply { answer: GRANTED };
            raft.step(message(2, 2, granted.clone())).unwrap();
            assert_eq!(raft.role(), Role::Candidate);
            sent(&mut raft);
            raft.step(message(4, 2, granted)).unwrap();
            assert_eq!(sent(&mut raft), []);
            let expected = Some(proof(Grant::PreVote, 1, &[1, 2]));
            assert_eq!(raft.hard().proof, expected);
        }

        #[test]
        fn a_leader_carries_its_votes_until_the_peer_answers() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            let votes = Some(proof(Grant::Vote, 1, &[1, 2]));
            raft.tick(0);
            let expected = [
                heartbeat(2, 1, votes.clone()),
                heartbeat(3, 1, votes.clone()),
            ];
            assert_eq!(sent(&mut raft), expected);
            // A heartbeat reply can answer a stale heartbeat, so it proves nothing.
            raft.step(message(2, 1, Body::HeartbeatReply)).unwrap();
            sent(&mut raft);
            raft.tick(0);
            let expected = [
                heartbeat(2, 1, votes.clone()),
                heartbeat(3, 1, votes.clone()),
            ];
            assert_eq!(sent(&mut raft), expected);
            raft.step(message(2, 1, Body::AppendReject { hint: 0 }))
                .unwrap();
            sent(&mut raft);
            raft.tick(0);
            let expected = [heartbeat(2, 1, None), heartbeat(3, 1, votes)];
            assert_eq!(sent(&mut raft), expected);
        }

        #[test]
        fn a_leader_elected_again_carries_its_new_votes() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            for from in [2, 3] {
                raft.step(message(from, 1, Body::AppendReject { hint: 0 }))
                    .unwrap();
            }
            raft.step(message(2, 2, Body::HeartbeatReply)).unwrap();
            assert_eq!(raft.role(), Role::Follower);
            elect(&mut raft, &[3]);
            raft.tick(0);
            let votes = Some(proof(Grant::Vote, 1, &[1, 3]));
            let expected = [heartbeat(2, 3, votes.clone()), heartbeat(3, 3, votes)];
            assert_eq!(sent(&mut raft), expected);
        }

        #[test]
        fn a_vote_from_outside_the_voters_does_not_join() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            raft.step(message(4, 1, Body::VoteReply { answer: GRANTED }))
                .unwrap();
            raft.tick(0);
            let votes = Some(proof(Grant::Vote, 1, &[1, 2]));
            let expected = [heartbeat(2, 1, votes.clone()), heartbeat(3, 1, votes)];
            assert_eq!(sent(&mut raft), expected);
        }

        #[test]
        fn a_late_vote_joins_the_votes() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            raft.step(message(3, 1, Body::VoteReply { answer: GRANTED }))
                .unwrap();
            raft.tick(0);
            let votes = Some(proof(Grant::Vote, 1, &[1, 2, 3]));
            let expected = [heartbeat(2, 1, votes.clone()), heartbeat(3, 1, votes)];
            assert_eq!(sent(&mut raft), expected);
            assert_eq!(raft.ready().hard, None);
        }

        #[test]
        fn a_refusal_carries_the_proof_of_its_term() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            let own = Some(proof(Grant::PreVote, 1, &[1, 2]));
            let prevote = Body::PreVote {
                last: Position::default(),
            };
            for (body, reply) in [
                (Body::Heartbeat { commit: 0 }, Body::HeartbeatReply),
                (prevote, Body::PreVoteReply { answer: REFUSED }),
            ] {
                raft.step(message(3, 0, body)).unwrap();
                let expected = Message {
                    from: key(1),
                    to: key(3),
                    term: Term(1),
                    body: reply,
                    proof: own.clone(),
                    chain: Vec::new(),
                };
                assert_eq!(sent(&mut raft), [expected]);
            }
        }

        #[test]
        fn a_refusal_in_the_same_term_carries_the_proof() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            let prevote = Body::PreVote {
                last: Position::default(),
            };
            raft.step(message(3, 1, prevote)).unwrap();
            let expected = Message {
                from: key(1),
                to: key(3),
                term: Term(1),
                body: Body::PreVoteReply { answer: REFUSED },
                proof: Some(proof(Grant::PreVote, 1, &[1, 2])),
                chain: Vec::new(),
            };
            assert_eq!(sent(&mut raft), [expected]);
        }

        #[test]
        fn writes_the_proof_and_the_leader_once_with_the_term() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            raft.campaign();
            raft.step(message(2, 2, Body::PreVoteReply { answer: GRANTED }))
                .unwrap();
            let candidate = Hard {
                term: Term(2),
                vote: Some(key(1)),
                leader: None,
                proof: Some(proof(Grant::PreVote, 1, &[1, 2])),
            };
            assert_eq!(raft.ready().hard, Some(candidate.clone()));
            raft.step(message(2, 2, Body::VoteReply { answer: GRANTED }))
                .unwrap();
            let leader = Hard {
                leader: Some(key(1)),
                ..candidate
            };
            assert_eq!(raft.ready().hard, Some(leader));
            raft.step(message(3, 2, Body::VoteReply { answer: GRANTED }))
                .unwrap();
            assert_eq!(raft.ready().hard, None);
        }

        #[test]
        fn takes_the_proof_of_the_message_that_moved_it() {
            let votes = proof(Grant::Vote, 2, &[2, 3]);
            let mut raft = raft(&[1, 2, 3], at_term(1));
            let message = Message {
                from: key(2),
                to: key(1),
                term: Term(2),
                body: Body::Heartbeat { commit: 0 },
                proof: Some(votes.clone()),
                chain: Vec::new(),
            };
            raft.step(message).unwrap();
            let expected = Hard {
                term: Term(2),
                vote: None,
                leader: Some(key(2)),
                proof: Some(votes),
            };
            assert_eq!(raft.ready().hard, Some(expected));
            let pre_votes = proof(Grant::PreVote, 3, &[2, 3]);
            let mut raft = self::raft(&[1, 2, 3], at_term(1));
            let message = Message {
                from: key(3),
                to: key(1),
                term: Term(2),
                body: Body::Vote {
                    last: Position::default(),
                },
                proof: Some(pre_votes.clone()),
                chain: Vec::new(),
            };
            raft.step(message).unwrap();
            let expected = Hard {
                term: Term(2),
                vote: Some(key(3)),
                leader: None,
                proof: Some(pre_votes),
            };
            assert_eq!(raft.ready().hard, Some(expected));
        }

        #[test]
        fn a_proof_does_not_outlive_its_term() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            let refusal = message(2, 2, Body::HeartbeatReply);
            let expected = Hard {
                proof: refusal.proof.clone(),
                ..at_term(2)
            };
            raft.step(refusal).unwrap();
            assert_eq!(raft.ready().hard, Some(expected));
        }

        #[test]
        fn keeps_the_leader_of_its_term_over_a_restart() {
            let hard = Hard {
                leader: Some(key(2)),
                ..at_term(1)
            };
            let mut raft = raft(&[1, 2, 3], hard.clone());
            assert_eq!((raft.leader(), raft.hard()), (None, hard));
            let err = raft
                .step(message(3, 1, Body::Heartbeat { commit: 0 }))
                .unwrap_err();
            let expected = Error::SecondLeader {
                term: Term(1),
                from: key(3),
            };
            assert_eq!(err, expected);
            raft.step(message(2, 1, Body::Heartbeat { commit: 0 }))
                .unwrap();
            assert_eq!(raft.leader(), Some(key(2)));
        }

        fn with(proof: Proof, message: Message) -> Message {
            Message {
                proof: Some(proof),
                ..message
            }
        }

        fn unchanged(raft: &mut Raft, term: u64, leader: Option<u8>) {
            let state = (raft.role(), raft.term(), raft.leader());
            assert_eq!(state, (Role::Follower, Term(term), leader.map(key)));
            assert_eq!(raft.ready(), Ready::default());
        }

        #[test]
        fn an_unproven_higher_term_changes_nothing() {
            let bodies = [
                Body::Vote {
                    last: Position::default(),
                },
                Body::VoteReply { answer: GRANTED },
                Body::PreVoteReply { answer: REFUSED },
                Body::Heartbeat { commit: 0 },
                Body::HeartbeatReply,
                append(Position::default(), vec![], 0),
                Body::AppendReply { last: 0 },
                Body::AppendReject { hint: 0 },
            ];
            for body in bodies {
                let mut raft = raft(&[1, 2, 3], at_term(1));
                sent(&mut raft);
                let err = raft.step(unproven(2, 2, body.clone())).unwrap_err();
                let expected = Error::Unproven {
                    term: Term(2),
                    from: key(2),
                };
                assert_eq!(err, expected, "{body:?}");
                unchanged(&mut raft, 1, None);
            }
        }

        #[test]
        fn a_short_proof_does_not_prove() {
            let mut raft = raft(&[1, 2, 3, 4, 5], at_term(1));
            sent(&mut raft);
            let heartbeat = || message(2, 2, Body::Heartbeat { commit: 0 });
            let short = proof(Grant::Vote, 2, &[2, 3]);
            let err = raft.step(with(short, heartbeat())).unwrap_err();
            let expected = Error::Unproven {
                term: Term(2),
                from: key(2),
            };
            assert_eq!(err, expected);
            unchanged(&mut raft, 1, None);
            let enough = proof(Grant::Vote, 2, &[2, 3, 4]);
            raft.step(with(enough, heartbeat())).unwrap();
            assert_eq!((raft.term(), raft.leader()), (Term(2), Some(key(2))));
        }

        #[test]
        fn a_proof_that_does_not_fit_its_message_does_not_prove() {
            let heartbeat = Body::Heartbeat { commit: 0 };
            let vote = Body::Vote {
                last: Position::default(),
            };
            let cases = [
                (heartbeat.clone(), proof(Grant::Vote, 3, &[2, 3])),
                (heartbeat, proof(Grant::PreVote, 2, &[2, 3])),
                (vote.clone(), proof(Grant::Vote, 2, &[2, 3])),
                (vote, proof(Grant::PreVote, 3, &[2, 3])),
            ];
            for (body, proof) in cases {
                let mut raft = raft(&[1, 2, 3], at_term(1));
                sent(&mut raft);
                let err = raft.step(with(proof, message(2, 2, body))).unwrap_err();
                let expected = Error::Unproven {
                    term: Term(2),
                    from: key(2),
                };
                assert_eq!(err, expected);
                unchanged(&mut raft, 1, None);
            }
        }

        #[test]
        fn a_proven_refusal_moves_a_leader_to_the_term_and_steps_it_down() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            let pre_votes = proof(Grant::PreVote, 3, &[2, 3]);
            let refusal = with(pre_votes.clone(), message(3, 2, Body::HeartbeatReply));
            raft.step(refusal).unwrap();
            assert_eq!((raft.role(), raft.leader()), (Role::Follower, None));
            let expected = Hard {
                term: Term(2),
                vote: None,
                leader: None,
                proof: Some(pre_votes),
            };
            assert_eq!(raft.ready().hard, Some(expected));
        }

        #[test]
        fn a_proof_needs_a_majority_of_each_set_while_joint() {
            let voters = Voters {
                incoming: [1, 2, 3].map(key).into_iter().collect(),
                outgoing: [1, 4, 5].map(key).into_iter().collect(),
            };
            let start = Start {
                voters,
                ..start(&[], at_term(1))
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            sent(&mut raft);
            let heartbeat = || message(2, 2, Body::Heartbeat { commit: 0 });
            let one_set = proof(Grant::Vote, 2, &[2, 3]);
            let err = raft.step(with(one_set, heartbeat())).unwrap_err();
            let expected = Error::Unproven {
                term: Term(2),
                from: key(2),
            };
            assert_eq!(err, expected);
            unchanged(&mut raft, 1, None);
            let both = proof(Grant::Vote, 2, &[2, 3, 4, 5]);
            raft.step(with(both, heartbeat())).unwrap();
            assert_eq!((raft.term(), raft.leader()), (Term(2), Some(key(2))));
        }

        #[test]
        fn a_node_in_a_term_by_its_log_alone_takes_its_leaders_proof() {
            let start = Start {
                entries: entries(&[(1, 1)]),
                ..start(&[1, 2, 3], Hard::default())
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            assert_eq!(raft.hard(), at_term(1));
            let heartbeat = message(2, 1, Body::Heartbeat { commit: 0 });
            let votes = heartbeat.proof.clone();
            raft.step(heartbeat).unwrap();
            let expected = Hard {
                leader: Some(key(2)),
                proof: votes.clone(),
                ..at_term(1)
            };
            assert_eq!(raft.ready().hard, Some(expected));
            raft.step(message(3, 0, Body::Heartbeat { commit: 0 }))
                .unwrap();
            let [refusal] = &sent(&mut raft)[..] else {
                panic!("one refusal");
            };
            assert_eq!((refusal.term, &refusal.proof), (Term(1), &votes));
        }

        #[test]
        fn a_quorum_of_the_committed_configuration_proves_through_a_pending_change() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            elect(&mut raft, &[2]);
            raft.propose_voters([key(1)].into()).unwrap();
            sent(&mut raft);
            let one_set = proof(Grant::PreVote, 3, &[2, 3]);
            raft.step(with(one_set, message(3, 3, Body::HeartbeatReply)))
                .unwrap();
            assert_eq!((raft.role(), raft.term()), (Role::Follower, Term(3)));
            let votes = proof(Grant::Vote, 3, &[2, 3]);
            let heartbeat = message(3, 3, Body::Heartbeat { commit: 0 });
            raft.step(with(votes, heartbeat)).unwrap();
            assert_eq!(raft.leader(), Some(key(3)));
        }

        #[test]
        fn carries_its_votes_again_to_a_peer_silent_through_a_quorum_check() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            elect(&mut raft, &[2]);
            raft.step(message(2, 2, Body::AppendReply { last: 1 }))
                .unwrap();
            sent(&mut raft);
            let votes = Some(proof(Grant::Vote, 1, &[1, 2]));
            let to_2 = |sent: Vec<Message>| {
                sent.into_iter()
                    .filter(|m| m.to == key(2))
                    .map(|m| m.proof)
                    .collect::<Vec<_>>()
            };
            // Node 2 was heard before the first check, so it keeps its answer.
            for _ in 0..CONFIG.election_ticks {
                raft.step(message(3, 2, Body::HeartbeatReply)).unwrap();
                raft.tick(0);
            }
            assert!(to_2(sent(&mut raft)).iter().all(Option::is_none));
            for _ in 0..CONFIG.election_ticks {
                raft.step(message(3, 2, Body::HeartbeatReply)).unwrap();
                raft.tick(0);
            }
            let proofs = to_2(sent(&mut raft));
            assert_eq!(proofs.last(), Some(&votes));
            assert!(proofs.contains(&None));
        }

        #[test]
        fn a_node_with_no_proof_of_its_term_answers_no_stale_message() {
            let start = Start {
                entries: entries(&[(2, 1)]),
                ..start(&[1, 2, 3], Hard::default())
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            sent(&mut raft);
            raft.step(message(2, 1, Body::Heartbeat { commit: 0 }))
                .unwrap();
            let pre_vote = Body::PreVote {
                last: Position::default(),
            };
            raft.step(message(3, 2, pre_vote)).unwrap();
            assert_eq!(sent(&mut raft), []);
            let heartbeat = message(2, 2, Body::Heartbeat { commit: 0 });
            let votes = heartbeat.proof.clone();
            raft.step(heartbeat).unwrap();
            sent(&mut raft);
            raft.step(message(3, 1, Body::Heartbeat { commit: 0 }))
                .unwrap();
            let [refusal] = &sent(&mut raft)[..] else {
                panic!("one refusal");
            };
            assert_eq!((refusal.term, &refusal.proof), (Term(2), &votes));
        }

        #[test]
        fn a_node_with_no_voters_takes_any_proof() {
            let mut raft = raft(&[], Hard::default());
            let alone = proof(Grant::Vote, 9, &[9]);
            let heartbeat = message(9, 1, Body::Heartbeat { commit: 0 });
            raft.step(with(alone, heartbeat)).unwrap();
            assert_eq!((raft.term(), raft.leader()), (Term(1), Some(key(9))));
        }

        #[test]
        fn a_second_leader_of_its_term_is_refused_with_its_proof() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            raft.step(message(2, 1, Body::Heartbeat { commit: 0 }))
                .unwrap();
            sent(&mut raft);
            let err = raft
                .step(message(3, 1, Body::Heartbeat { commit: 0 }))
                .unwrap_err();
            let expected = Error::SecondLeader {
                term: Term(1),
                from: key(3),
            };
            assert_eq!(err, expected);
            assert_eq!(raft.leader(), Some(key(2)));
            assert_eq!(raft.ready(), Ready::default());
        }
    }

    // A message with a proof carries the sender's configuration entries below its
    // term. A node whose configuration the proof is no quorum of reads them.
    mod chain {
        use super::*;

        const ALL: [u8; 4] = [1, 2, 3, 4];

        fn plain(ids: &[u8]) -> Voters {
            Voters {
                incoming: ids.iter().copied().map(key).collect(),
                ..Voters::default()
            }
        }

        // The change to `voters` that `leader` wrote at `at`, elected by `elected`.
        fn link(at: Position, leader: u8, elected: &[u8], voters: Voters) -> Link {
            Link {
                at,
                change: Change {
                    voters,
                    votes: proof(Grant::Vote, leader, elected),
                    signature: Some(signature(Grant::Vote, leader)),
                },
            }
        }

        // The chain of the shrink from `all` to 1, 2 and 3 that leader 3 wrote in
        // term 1, elected by `all`: the joint entry at index 1, the leave at 2.
        fn shrink(all: &[u8]) -> Vec<Link> {
            let joint = Voters {
                incoming: plain(&[1, 2, 3]).incoming,
                outgoing: plain(all).incoming,
            };
            vec![
                link(position(1, 1), 3, all, joint),
                link(position(1, 2), 3, all, plain(&[1, 2, 3])),
            ]
        }

        // The heartbeat of leader 2 in `term`, elected by 2 and 3, with `chain`.
        fn heartbeat(term: u64, chain: Vec<Link>) -> Message {
            Message {
                proof: Some(proof(Grant::Vote, 2, &[2, 3])),
                chain,
                ..message(2, term, Body::Heartbeat { commit: 0 })
            }
        }

        // Node 1, a voter of `all`, in term 1 with an empty log: it missed the shrink.
        fn behind(all: &[u8]) -> Raft {
            let mut raft = raft(all, at_term(1));
            sent(&mut raft);
            raft
        }

        // The leave to 1, 2 and 3 that node 1 wrote at index 1 of term 1, elected by
        // every node of `ALL`.
        fn leave() -> Entry {
            written(1, 1, plain(&[1, 2, 3]), &ALL)
        }

        fn leave_link() -> Link {
            let Entry {
                at,
                data: Data::Voters(change),
            } = leave()
            else {
                unreachable!("a leave is a change");
            };
            Link { at, change }
        }

        // Node 1 after the leave, committed.
        fn left() -> Raft {
            let start = Start {
                entries: vec![leave()],
                applied: 1,
                ..start(&ALL, at_term(1))
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            sent(&mut raft);
            raft
        }

        // `message` is unproven for `case`, and leaves the node as it was.
        fn refuses(raft: &mut Raft, message: Message, case: &str) {
            let state = |raft: &Raft| {
                (
                    raft.hard(),
                    raft.role(),
                    raft.leader(),
                    raft.voters().clone(),
                )
            };
            let before = state(raft);
            let expected = Error::Unproven {
                term: message.term,
                from: message.from,
            };
            assert_eq!(raft.step(message), Err(expected), "{case}");
            assert_eq!(state(raft), before, "{case}");
            assert_eq!(raft.ready(), Ready::default(), "{case}");
        }

        #[test]
        fn a_voter_down_through_a_shrink_takes_the_heartbeat_with_the_chain() {
            for all in [&[1, 2, 3, 4][..], &[1, 2, 3, 4, 5]] {
                let mut raft = behind(all);
                refuses(&mut raft, heartbeat(2, Vec::new()), "no chain");
                raft.step(heartbeat(2, shrink(all))).unwrap();
                let state = (raft.term(), raft.leader(), raft.voters());
                assert_eq!(state, (Term(2), Some(key(2)), &plain(all)), "{all:?}");
            }
        }

        #[test]
        fn a_voter_down_through_a_pending_leave_takes_the_append_with_the_chain() {
            let mut raft = behind(&ALL);
            let chain = shrink(&ALL);
            let appended = chain
                .iter()
                .map(|link| Entry {
                    at: link.at,
                    data: Data::Voters(link.change.clone()),
                })
                .chain(entries(&[(2, 3)]))
                .collect();
            let append = Message {
                body: append(Position::default(), appended, 1),
                ..heartbeat(2, chain)
            };
            raft.step(append).unwrap();
            let state = (raft.term(), raft.leader(), raft.voters());
            assert_eq!(state, (Term(2), Some(key(2)), &plain(&[1, 2, 3])));
        }

        #[test]
        fn a_voter_that_missed_the_leave_grants_a_vote_whose_chain_holds_it() {
            let mut raft = behind(&ALL);
            let last = Position::default();
            let vote = || Message {
                proof: Some(proof(Grant::PreVote, 2, &[2, 3])),
                ..message(2, 2, Body::Vote { last })
            };
            refuses(&mut raft, vote(), "no chain");
            let vote = Message {
                chain: shrink(&ALL),
                ..vote()
            };
            raft.step(vote).unwrap();
            let [reply] = &sent(&mut raft)[..] else {
                panic!("one reply");
            };
            let granted = Body::VoteReply { answer: GRANTED };
            assert_eq!(
                (reply.to, reply.term, &reply.body),
                (key(2), Term(2), &granted)
            );
            assert_eq!(raft.hard().vote, Some(key(2)));
        }

        #[test]
        fn a_link_that_fails_leaves_the_node_as_it_was() {
            let mut few = shrink(&ALL);
            few[0].change.votes = proof(Grant::Vote, 3, &[3, 4]);
            let mut pre_votes = shrink(&ALL);
            pre_votes[0].change.votes.grant = Grant::PreVote;
            let mut late = shrink(&ALL);
            late[1].at.term = Term(2);
            let mut flat = shrink(&ALL);
            flat[1].at.index = 1;
            let mut falling = shrink(&ALL);
            falling[1].at.term = Term(0);
            // Any proof is a quorum of no voters, so such a link proves nothing.
            let mut empty = shrink(&ALL);
            empty[1].change.voters = Voters::default();
            let cases = [
                ("a configuration with no voters", empty),
                (
                    "votes that are no quorum of the founding configuration",
                    few,
                ),
                ("pre-votes", pre_votes),
                ("a link in the message's term", late),
                ("an index that does not rise", flat),
                ("a term that falls", falling),
                (
                    "a chain that ends before the leave",
                    shrink(&ALL)[..1].to_vec(),
                ),
            ];
            for (case, chain) in cases {
                refuses(&mut behind(&ALL), heartbeat(2, chain), case);
            }
        }

        // The leader of a term is elected under the last configuration below it,
        // so a link trusts the last link of a lower term, else what the receiver
        // committed below its term.
        #[test]
        fn a_link_trusts_the_last_link_of_a_lower_term() {
            let all = [1, 2, 3, 4, 5];
            let leave = plain(&[1, 2, 3]);
            // 3, 4 and 5 are a quorum of the founding configuration, not of the joint
            // one: its incoming set holds 3 alone.
            let mut same_term = shrink(&all);
            same_term[1] = link(position(1, 2), 3, &[3, 4, 5], leave.clone());
            let mut raft = behind(&all);
            raft.step(heartbeat(2, same_term)).unwrap();
            assert_eq!(raft.leader(), Some(key(2)));
            let mut later_term = shrink(&all);
            later_term[1] = link(position(2, 2), 3, &[3, 4, 5], leave.clone());
            refuses(&mut behind(&all), heartbeat(3, later_term), "later term");
            let mut by_joint = shrink(&all);
            by_joint[1] = link(position(2, 2), 3, &[1, 2, 3, 4], leave);
            let mut raft = behind(&all);
            raft.step(heartbeat(3, by_joint)).unwrap();
            assert_eq!((raft.term(), raft.leader()), (Term(3), Some(key(2))));
        }

        // Node 1 committed the leave of term 1. A second change of term 1 was voted
        // under the configuration before the term, not under the leave.
        #[test]
        fn a_link_of_the_committed_term_trusts_the_configuration_below_it() {
            let heartbeat = |chain| Message {
                proof: Some(proof(Grant::Vote, 2, &[2, 5])),
                chain,
                ..message(2, 2, Body::Heartbeat { commit: 0 })
            };
            let next = plain(&[2, 5, 6]);
            // 1 and 2 are a quorum of the leave, not of the founding configuration.
            let by_leave = vec![link(position(1, 2), 1, &[1, 2], next.clone())];
            refuses(&mut left(), heartbeat(by_leave), "voted under the leave");
            let by_all = vec![link(position(1, 2), 1, &[1, 2, 3], next)];
            let mut raft = left();
            raft.step(heartbeat(by_all)).unwrap();
            assert_eq!((raft.term(), raft.leader()), (Term(2), Some(key(2))));
        }

        #[test]
        fn a_link_at_or_below_the_commit_index_is_skipped() {
            let chain = shrink(&ALL);
            let joint = Entry {
                at: chain[0].at,
                data: Data::Voters(chain[0].change.clone()),
            };
            let start = Start {
                entries: vec![joint],
                applied: 1,
                ..start(&ALL, at_term(1))
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            sent(&mut raft);
            let mut bad = chain;
            bad[0].change.votes = proof(Grant::PreVote, 9, &[]);
            raft.step(heartbeat(2, bad)).unwrap();
            assert_eq!((raft.term(), raft.leader()), (Term(2), Some(key(2))));
        }

        // Only the links before the first one above the commit index are skipped. A
        // later link at or below it is read in its turn, and fails to rise, unless
        // a link before it proved the term.
        #[test]
        fn an_unsorted_chain_is_read_from_its_first_link_above_the_commit_index() {
            let [joint, leave] = <[Link; 2]>::try_from(shrink(&ALL)).unwrap();
            let below = link(position(1, 0), 3, &ALL, plain(&[1, 2, 3]));
            let unsorted = vec![joint.clone(), below.clone(), leave.clone()];
            refuses(
                &mut behind(&ALL),
                heartbeat(2, unsorted),
                "below after joint",
            );
            let start = Start {
                entries: vec![Entry {
                    at: joint.at,
                    data: Data::Voters(joint.change),
                }],
                applied: 1,
                ..start(&ALL, at_term(1))
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            sent(&mut raft);
            raft.step(heartbeat(2, vec![leave, below])).unwrap();
            assert_eq!((raft.term(), raft.leader()), (Term(2), Some(key(2))));
        }

        // `claims` lists no link of such a chain, so `mesh` checks none.
        #[test]
        fn a_chain_with_every_link_at_or_below_the_commit_index_gives_no_link() {
            let mut raft = behind(&ALL);
            let below = link(position(1, 0), 3, &ALL, plain(&[1, 2, 3]));
            let message = heartbeat(2, vec![below]);
            let changes = raft
                .claims(&message)
                .filter(|(claim, _)| matches!(claim, Claim::Change { .. }))
                .count();
            assert_eq!(changes, 0);
            refuses(&mut raft, message, "every link below");
        }

        #[test]
        fn a_candidate_carries_its_chain_on_a_vote_and_none_on_a_pre_vote() {
            let mut raft = left();
            raft.campaign();
            let pre_votes = sent(&mut raft);
            assert_eq!(pre_votes.len(), 2);
            for pre_vote in &pre_votes {
                assert_eq!((&pre_vote.proof, &pre_vote.chain), (&None, &Vec::new()));
            }
            raft.step(message(2, 2, Body::PreVoteReply { answer: GRANTED }))
                .unwrap();
            let votes = sent(&mut raft);
            assert_eq!(votes.len(), 2);
            for vote in &votes {
                assert!(matches!(vote.body, Body::Vote { .. }), "{vote:?}");
                assert_eq!(vote.chain, [leave_link()]);
            }
        }

        // The chain holds only entries below the term: not the leader's own change.
        #[test]
        fn a_leader_carries_its_chain_until_the_peer_answers() {
            let mut raft = left();
            elect(&mut raft, &[2]);
            raft.step(message(2, 2, Body::AppendReply { last: 2 }))
                .unwrap();
            raft.propose_voters([key(1), key(2)].into()).unwrap();
            sent(&mut raft);
            raft.tick(0);
            raft.step(message(3, 2, Body::HeartbeatReply)).unwrap();
            let messages = sent(&mut raft);
            let appends = messages
                .iter()
                .filter(|m| m.to == key(3) && matches!(m.body, Body::Append { .. }))
                .count();
            assert_eq!(appends, 1, "{messages:#?}");
            for message in &messages {
                let answered = message.to == key(2);
                assert_eq!(message.proof.is_some(), !answered, "{message:?}");
                let chain = if answered { vec![] } else { vec![leave_link()] };
                assert_eq!(message.chain, chain, "{message:?}");
            }
        }

        #[test]
        fn an_answer_to_a_stale_message_carries_the_chain_of_its_term() {
            let start = Start {
                entries: vec![leave()],
                applied: 1,
                ..start(&ALL, with_proof(2))
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            sent(&mut raft);
            raft.step(message(3, 1, Body::Heartbeat { commit: 0 }))
                .unwrap();
            let [refusal] = &sent(&mut raft)[..] else {
                panic!("one refusal");
            };
            let expected = (Term(2), true, vec![leave_link()]);
            let got = (refusal.term, refusal.proof.is_some(), refusal.chain.clone());
            assert_eq!(got, expected);
        }

        #[test]
        fn a_grant_carries_no_chain() {
            let mut raft = left();
            let last = position(1, 1);
            raft.step(message(2, 2, Body::PreVote { last })).unwrap();
            raft.step(message(2, 2, Body::Vote { last })).unwrap();
            let replies = sent(&mut raft);
            assert_eq!(replies.len(), 2, "{replies:#?}");
            for reply in replies {
                assert!(
                    matches!(
                        reply.body,
                        Body::PreVoteReply { answer: GRANTED }
                            | Body::VoteReply { answer: GRANTED }
                    ),
                    "{reply:?}"
                );
                assert_eq!((reply.proof, reply.chain), (None, Vec::new()));
            }
        }
    }

    // `step` checks a message against the log before it changes any state.
    mod check {
        use super::*;

        // The state a refused message leaves as it was. `ready` drains what the node
        // made before, so the message must add nothing to it.
        fn state(raft: &mut Raft) -> (Hard, Role, Option<node::Key>, Ready) {
            (raft.hard(), raft.role(), raft.leader(), raft.ready())
        }

        #[test]
        fn refuses_a_heartbeat_that_commits_past_the_log() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            sent(&mut raft);
            let before = state(&mut raft);
            let err = raft
                .step(message(2, 1, Body::Heartbeat { commit: 1 }))
                .unwrap_err();
            assert_eq!(err, Error::IndexPastLog { index: 1, last: 0 });
            assert_eq!(err.to_string(), "index 1 is past the last log index 0");
            assert_eq!(state(&mut raft), before);
        }

        #[test]
        fn refuses_an_append_after_the_last_index() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            let before = state(&mut raft);
            let prev = position(1, u64::MAX);
            let body = append(prev, entries(&[(1, 0)]), 0);
            let err = raft.step(message(2, 1, body)).unwrap_err();
            let out_of_order = Error::EntryOutOfOrder {
                at: position(1, 0),
                before: prev,
            };
            assert_eq!(err, out_of_order);
            assert_eq!(state(&mut raft), before);
        }

        #[test]
        fn refuses_an_append_with_an_entry_above_its_term() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            let before = state(&mut raft);
            let body = append(Position::default(), entries(&[(1, 1), (3, 2)]), 0);
            let err = raft.step(message(2, 2, body)).unwrap_err();
            let last = position(3, 2);
            assert_eq!(
                err,
                Error::TermBehindLog {
                    term: Term(2),
                    last
                }
            );
            assert_eq!(err.to_string(), "term 2 is lower than term 3 of entry 2");
            assert_eq!(state(&mut raft), before);
        }

        // An append is checked for order before its term.
        #[test]
        fn refuses_an_append_out_of_order_before_one_above_its_term() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            let body = append(Position::default(), entries(&[(3, 2)]), 0);
            let err = raft.step(message(2, 2, body)).unwrap_err();
            let out_of_order = Error::EntryOutOfOrder {
                at: position(3, 2),
                before: Position::default(),
            };
            assert_eq!(err, out_of_order);
        }

        // A `prev` is never written, so its term can be above the message's.
        #[test]
        fn rejects_an_empty_append_with_a_prev_above_its_term() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            let body = append(position(3, 1), vec![], 0);
            raft.step(message(2, 2, body)).unwrap();
            let bodies: Vec<Body> =
                sent(&mut raft).into_iter().map(|m| m.body).collect();
            assert_eq!(bodies, [Body::AppendReject { hint: 0 }]);
        }

        #[test]
        fn takes_an_append_with_entries_of_its_term() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            let body = append(Position::default(), entries(&[(1, 1), (2, 2)]), 0);
            raft.step(message(2, 2, body)).unwrap();
            let terms: Vec<Term> =
                raft.ready().entries.iter().map(|e| e.at.term).collect();
            assert_eq!(terms, [Term(1), Term(2)]);
        }

        // The term, the vote, and the leader move only for a message that is
        // applied.
        #[test]
        fn keeps_its_state_when_it_refuses_an_append_of_a_higher_term() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            let before = state(&mut raft);
            let body = append(Position::default(), entries(&[(1, 2)]), 0);
            let err = raft.step(message(2, 5, body)).unwrap_err();
            let out_of_order = Error::EntryOutOfOrder {
                at: position(1, 2),
                before: Position::default(),
            };
            assert_eq!(err, out_of_order);
            assert_eq!(state(&mut raft), before);
        }

        #[test]
        fn a_leader_refuses_an_append_reply_for_the_last_index() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            elect(&mut raft, &[2]);
            let before = state(&mut raft);
            let reply = Body::AppendReply { last: u64::MAX };
            let err = raft.step(message(2, 2, reply)).unwrap_err();
            assert_eq!(
                err,
                Error::IndexPastLog {
                    index: u64::MAX,
                    last: 1
                }
            );
            assert_eq!(state(&mut raft), before);
        }

        #[test]
        fn a_leader_refuses_an_append_reply_past_its_log() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            elect(&mut raft, &[2]);
            let err = raft
                .step(message(2, 2, Body::AppendReply { last: 5 }))
                .unwrap_err();
            assert_eq!(err, Error::IndexPastLog { index: 5, last: 1 });
            assert_eq!(raft.ready(), Ready::default());
            assert_eq!(raft.propose(vec![7]), Ok(position(2, 2)));
        }

        #[test]
        fn a_leader_refuses_an_append_reject_past_its_log() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            elect(&mut raft, &[2]);
            let before = state(&mut raft);
            let reject = Body::AppendReject { hint: u64::MAX };
            let err = raft.step(message(2, 2, reject)).unwrap_err();
            assert_eq!(
                err,
                Error::IndexPastLog {
                    index: u64::MAX,
                    last: 1
                }
            );
            assert_eq!(state(&mut raft), before);
        }

        // A message for a lower term is stale: its content is not read.
        #[test]
        fn drops_a_stale_message_without_a_check() {
            let mut raft = raft(&[1, 2, 3], with_proof(3));
            sent(&mut raft);
            raft.step(message(2, 1, Body::AppendReply { last: u64::MAX }))
                .unwrap();
            raft.step(message(2, 1, Body::Heartbeat { commit: u64::MAX }))
                .unwrap();
            let reply = Message {
                from: key(1),
                to: key(2),
                term: Term(3),
                body: Body::HeartbeatReply,
                proof: with_proof(3).proof,
                chain: Vec::new(),
            };
            assert_eq!(sent(&mut raft), [reply]);
        }
    }

    mod candidate {
        use super::*;

        fn candidate() -> Raft {
            let mut raft = raft(&[1, 2, 3, 4, 5], at_term(1));
            raft.campaign();
            let granted = Body::PreVoteReply { answer: GRANTED };
            raft.step(message(2, 2, granted.clone())).unwrap();
            raft.step(message(3, 2, granted.clone())).unwrap();
            assert_eq!((raft.role(), raft.term()), (Role::Candidate, Term(2)));
            sent(&mut raft);
            raft
        }

        #[test]
        fn does_not_count_a_prevote_grant_as_a_vote() {
            let mut raft = candidate();
            let granted = Body::PreVoteReply { answer: GRANTED };
            raft.step(message(4, 2, granted.clone())).unwrap();
            raft.step(message(5, 2, granted.clone())).unwrap();
            assert_eq!(raft.role(), Role::Candidate);
        }

        #[test]
        fn does_not_count_a_vote_from_a_lower_term() {
            let mut raft = candidate();
            let granted = Body::VoteReply { answer: GRANTED };
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
                proof: None,
                chain: Vec::new(),
            };
            assert_eq!(sent(&mut raft), [reply]);
        }

        #[test]
        fn steps_down_on_a_vote_reply_from_a_higher_term() {
            let mut raft = candidate();
            let rejected = message(4, 7, Body::VoteReply { answer: REFUSED });
            let expected = Hard {
                proof: rejected.proof.clone(),
                ..at_term(7)
            };
            raft.step(rejected).unwrap();
            assert_eq!((raft.role(), raft.hard()), (Role::Follower, expected));
        }
    }

    mod pre_candidate {
        use super::*;

        #[test]
        fn does_not_count_a_vote_grant_as_a_prevote() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            raft.campaign();
            let granted = Body::VoteReply { answer: GRANTED };
            raft.step(message(2, 1, granted.clone())).unwrap();
            assert_eq!((raft.role(), raft.term()), (Role::PreCandidate, Term(1)));
        }

        // The answers stay as few as the voters, whatever nodes reply.
        #[test]
        fn keeps_no_answer_from_a_node_that_is_not_a_voter() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            raft.campaign();
            let granted = Body::PreVoteReply { answer: GRANTED };
            raft.step(message(9, 1, granted.clone())).unwrap();
            assert_eq!(raft.role(), Role::PreCandidate);
            assert!(raft.answers.is_empty());
            raft.step(message(2, 1, granted)).unwrap();
            assert_eq!(raft.role(), Role::Candidate);
        }

        // A grant of term 1 answers a pre-campaign from term 0. No node asks for 3.
        #[test]
        fn counts_a_grant_only_for_the_term_it_asks_for() {
            let mut raft = raft(&[1, 2, 3], at_term(1));
            raft.campaign();
            let granted = Body::PreVoteReply { answer: GRANTED };
            raft.step(message(2, 1, granted.clone())).unwrap();
            raft.step(message(2, 3, granted.clone())).unwrap();
            assert_eq!((raft.role(), raft.term()), (Role::PreCandidate, Term(1)));
            raft.step(message(3, 2, granted)).unwrap();
            assert_eq!((raft.role(), raft.term()), (Role::Candidate, Term(2)));
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
            let heartbeat = message(2, 1, Body::Heartbeat { commit: 0 });
            let votes = heartbeat.proof.clone();
            raft.step(heartbeat).unwrap();
            sent(&mut raft);
            let vote = Body::Vote {
                last: Position::default(),
            };
            raft.step(message(3, 1, vote.clone())).unwrap();
            let rejected = Body::VoteReply { answer: REFUSED };
            assert_eq!(sent(&mut raft)[0].body, rejected);
            let expected = Hard {
                leader: Some(key(2)),
                proof: votes,
                ..at_term(1)
            };
            assert_eq!(raft.hard(), expected);
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
            let vote = message(2, 3, vote);
            let expected = Hard {
                proof: vote.proof.clone(),
                ..at_term(3)
            };
            raft.step(vote).unwrap();
            let reply = Message {
                from: key(1),
                to: key(2),
                term: Term(3),
                body: Body::VoteReply { answer: REFUSED },
                proof: None,
                chain: Vec::new(),
            };
            assert_eq!(sent(&mut raft), [reply]);
            assert_eq!(raft.hard(), expected);
        }

        #[test]
        fn rejects_a_prevote_from_a_lower_term_with_its_own_term() {
            let mut raft = raft(&[1, 2, 3], with_proof(5));
            let prevote = Body::PreVote {
                last: Position::default(),
            };
            raft.step(message(2, 3, prevote)).unwrap();
            let reply = Message {
                from: key(1),
                to: key(2),
                term: Term(5),
                body: Body::PreVoteReply { answer: REFUSED },
                proof: with_proof(5).proof,
                chain: Vec::new(),
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
        fn rejects_a_second_leader_of_the_term_it_led_after_it_steps_down() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            tick_times(&mut raft, 10);
            assert_eq!((raft.role(), raft.leader()), (Role::Follower, None));
            sent(&mut raft);
            let alone = Voters {
                incoming: [key(1)].into_iter().collect(),
                ..Voters::default()
            };
            let append = Body::Append {
                prev: Position {
                    term: Term(1),
                    index: 1,
                },
                entries: vec![Entry {
                    at: Position {
                        term: Term(1),
                        index: 2,
                    },
                    data: change(3, alone),
                }],
                commit: 0,
            };
            let err = raft.step(message(3, 1, append)).unwrap_err();
            let expected = Error::SecondLeader {
                term: Term(1),
                from: key(3),
            };
            let voters: Vec<node::Key> =
                raft.voters().incoming.iter().copied().collect();
            raft.campaign();
            assert_eq!(
                (err, voters, raft.role(), raft.ready().committed),
                (
                    expected,
                    vec![key(1), key(2), key(3)],
                    Role::PreCandidate,
                    vec![]
                )
            );
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
            let rejected = Body::VoteReply { answer: REFUSED };
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
            let refusal = Message {
                proof: Some(proof(Grant::PreVote, 2, &[1, 2])),
                ..message(2, 5, Body::HeartbeatReply)
            };
            raft.step(refusal).unwrap();
            elect(&mut raft, &[2]);
            tick_times(&mut raft, 2);
            assert_eq!(sent(&mut raft), []);
            raft.tick(0);
            assert_eq!(sent(&mut raft).len(), 1);
        }
    }

    mod replication {
        use super::*;

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
            let granted = Body::PreVoteReply { answer: GRANTED };
            raft.step(message(2, 2, granted)).unwrap();
            sent(&mut raft);
            raft.step(message(2, 2, Body::VoteReply { answer: GRANTED }))
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
                proof: Some(proof(Grant::Vote, 1, &[1, 2])),
                chain: Vec::new(),
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
                proof: None,
                chain: Vec::new(),
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
            let mut raft = raft(&[1, 2, 3], with_proof(5));
            let body = append(Position::default(), entries(&[(4, 1)]), 1);
            raft.step(message(2, 4, body)).unwrap();
            let reply = Message {
                from: key(1),
                to: key(2),
                term: Term(5),
                body: Body::HeartbeatReply,
                proof: with_proof(5).proof,
                chain: Vec::new(),
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
                proof: None,
                chain: Vec::new(),
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
            let grant = Body::PreVoteReply { answer: GRANTED };
            raft.step(message(9, 1, grant.clone())).unwrap();
            assert_eq!(raft.role(), Role::PreCandidate);
            raft.step(message(2, 1, grant)).unwrap();
            assert_eq!(raft.role(), Role::Candidate);
            let grant = Body::VoteReply { answer: GRANTED };
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

        #[test]
        fn a_leader_keeps_its_lead_when_it_adds_voters_late_in_a_quorum_period() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            tick_times(&mut raft, 9);
            for from in [2, 3] {
                raft.step(message(from, 1, Body::HeartbeatReply)).unwrap();
            }
            let joint = Voters {
                incoming: [1, 4, 5].into_iter().map(key).collect(),
                outgoing: [1, 2, 3].into_iter().map(key).collect(),
            };
            let joint = Data::Voters(raft.change(joint));
            raft.propose_entry(joint);
            // Nodes 4 and 5 had one tick to answer a leader they did not know.
            tick_times(&mut raft, 1);
            assert_eq!(raft.role(), Role::Leader);
        }

        fn voters(ids: &[u8]) -> Voters {
            Voters {
                incoming: ids.iter().copied().map(key).collect(),
                ..Voters::default()
            }
        }

        fn config(term: u64, index: u64, voters: Voters) -> Entry {
            Entry {
                at: position(term, index),
                data: change(2, voters),
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

        #[test]
        fn a_leader_uses_a_configuration_from_the_time_it_writes_it() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            for from in [2, 3] {
                raft.step(message(from, 1, Body::AppendReply { last: 1 }))
                    .unwrap();
            }
            sent(&mut raft);
            let new = voters(&[1, 2, 3, 4]);
            let at = raft.propose_entry(Data::Voters(raft.change(new.clone())));
            assert_eq!(at.index, 2);
            assert_eq!(raft.voters(), &new);
            // The new peer gets one probe from the end of the log, then waits.
            let messages = sent(&mut raft);
            let to: Vec<node::Key> = messages.iter().map(|m| m.to).collect();
            assert_eq!(to, [key(2), key(3), key(4)]);
            let probe = Body::Append {
                prev: Position {
                    term: Term(1),
                    index: 2,
                },
                entries: Vec::new(),
                commit: 1,
            };
            assert_eq!(messages[2].body, probe);
            // Two of the four voters do not commit; three do.
            raft.step(message(2, 1, Body::AppendReply { last: 2 }))
                .unwrap();
            assert_eq!(raft.ready().committed, []);
            raft.step(message(3, 1, Body::AppendReply { last: 2 }))
                .unwrap();
            assert_eq!(raft.ready().committed, [written(1, 2, new, &[1, 2])]);
        }

        #[test]
        fn a_follower_uses_a_configuration_from_the_time_it_writes_it() {
            let new = voters(&[2, 3]);
            let mut raft = follower_with(vec![
                entries(&[(1, 1)]).remove(0),
                config(1, 2, new.clone()),
            ]);
            assert_eq!(raft.voters(), &new);
            assert_eq!(raft.ready().committed, []);
            // The entry may still be truncated, so the node campaigns until it commits.
            tick_times(&mut raft, 40);
            assert_eq!(raft.role(), Role::PreCandidate);
            raft.step(message(2, 1, Body::Heartbeat { commit: 2 }))
                .unwrap();
            assert_eq!(raft.ready().committed.last(), Some(&config(1, 2, new)));
            tick_times(&mut raft, 40);
            assert_eq!(raft.role(), Role::Follower);
        }

        #[test]
        fn the_last_configuration_in_the_log_counts() {
            let last = voters(&[1, 2, 3, 4, 5]);
            let raft = follower_with(vec![
                entries(&[(1, 1)]).remove(0),
                config(1, 2, voters(&[1, 2])),
                config(1, 3, last.clone()),
            ]);
            assert_eq!(raft.voters(), &last);
        }

        #[test]
        fn a_truncated_configuration_gives_way_to_the_one_before_it() {
            let second = voters(&[1, 2]);
            let mut raft = follower_with(vec![
                entries(&[(1, 1)]).remove(0),
                config(1, 2, second.clone()),
                config(1, 3, voters(&[1, 2, 3, 4, 5])),
            ]);
            let body = Body::Append {
                prev: Position {
                    term: Term(1),
                    index: 2,
                },
                entries: entries(&[(2, 3)]),
                commit: 0,
            };
            raft.step(message(2, 2, body)).unwrap();
            assert_eq!(raft.voters(), &second);
            let body = Body::Append {
                prev: Position {
                    term: Term(1),
                    index: 1,
                },
                entries: entries(&[(2, 2)]),
                commit: 0,
            };
            raft.step(message(2, 2, body)).unwrap();
            assert_eq!(raft.voters(), &voters(&[1, 2, 3]));
        }

        #[test]
        fn a_restart_uses_the_last_configuration_in_its_log() {
            let new = voters(&[1, 2, 3, 4]);
            let start = Start {
                entries: vec![entries(&[(1, 1)]).remove(0), config(1, 2, new.clone())],
                applied: 1,
                ..start(&[1, 2, 3], at_term(1))
            };
            let raft = Raft::new(CONFIG, start).unwrap();
            assert_eq!(raft.voters(), &new);
        }

        #[test]
        fn rejects_a_configuration_entry_with_no_voter() {
            let outgoing_only = Voters {
                outgoing: [key(1), key(2)].into_iter().collect(),
                ..Voters::default()
            };
            for bad in [Voters::default(), outgoing_only] {
                let mut raft = raft(&[1, 2, 3], Hard::default());
                let body = Body::Append {
                    prev: Position::default(),
                    entries: vec![config(1, 1, bad.clone())],
                    commit: 0,
                };
                let error = raft.step(message(2, 1, body)).unwrap_err();
                assert_eq!(error, Error::NoVoters);
                assert_eq!(
                    error.to_string(),
                    "a configuration has an empty incoming voter set"
                );
                let start = Start {
                    entries: vec![config(1, 1, bad)],
                    ..start(&[1, 2, 3], at_term(1))
                };
                assert_eq!(Raft::new(CONFIG, start).unwrap_err(), Error::NoVoters);
            }
        }
    }

    mod change {
        use super::*;

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
                data: change(2, voters),
            }
        }

        // Node 1 leads 2 and 3 at term 1, and both hold its first entry.
        fn leader() -> Raft {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            elect(&mut raft, &[2]);
            accept(&mut raft, &[2, 3], 1);
            sent(&mut raft);
            raft
        }

        fn accept(raft: &mut Raft, from: &[u8], last: u64) {
            for &from in from {
                raft.step(message(from, raft.term().0, Body::AppendReply { last }))
                    .unwrap();
            }
        }

        fn to(messages: &[Message]) -> Vec<node::Key> {
            messages.iter().map(|m| m.to).collect()
        }

        #[test]
        fn a_follower_refuses_a_change() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            assert_eq!(
                raft.propose_voters(set(&[1, 2])),
                Err(Error::NotLeader { leader: None })
            );
        }

        #[test]
        fn refuses_a_change_to_no_voter() {
            let mut raft = leader();
            let error = raft.propose_voters(set(&[])).unwrap_err();
            assert_eq!(error, Error::NoVoters);
            assert_eq!(raft.voters(), &voters(&[1, 2, 3], &[]));
        }

        #[test]
        fn a_change_enters_a_joint_phase_and_leaves_it_when_the_entry_commits() {
            let mut raft = leader();
            let at = raft.propose_voters(set(&[1, 2, 4])).unwrap();
            assert_eq!(at.index, 2);
            let joint = voters(&[1, 2, 4], &[1, 2, 3]);
            assert_eq!(raft.voters(), &joint);
            let ready = raft.ready();
            assert_eq!(ready.entries, [written(1, 2, joint.clone(), &[1, 2])]);
            assert_eq!(to(&ready.messages), [key(2), key(3), key(4)]);
            // Nodes 1 and 2 are a majority of each set.
            accept(&mut raft, &[2], 2);
            let new = voters(&[1, 2, 4], &[]);
            assert_eq!(raft.voters(), &new);
            let ready = raft.ready();
            assert_eq!(ready.committed, [written(1, 2, joint, &[1, 2])]);
            assert_eq!(ready.entries, [written(1, 3, new.clone(), &[1, 2])]);
            // Node 3 gets the leave; node 4 waits for the answer to its probe.
            assert_eq!(to(&ready.messages), [key(2), key(3)]);
            accept(&mut raft, &[2], 3);
            let ready = raft.ready();
            assert_eq!(ready.committed, [written(1, 3, new, &[1, 2])]);
            // Node 3 stays a peer until it holds the leave.
            assert_eq!(to(&ready.messages), [key(2), key(3)]);
            raft.tick(0);
            assert_eq!(to(&raft.ready().messages), [key(2), key(3), key(4)]);
            // Node 3 holds the leave: its commit reaches it, then it is released.
            accept(&mut raft, &[3], 3);
            let ready = raft.ready();
            assert_eq!(to(&ready.messages), [key(3)]);
            assert_eq!(ready.messages[0].body, Body::Heartbeat { commit: 3 });
            raft.tick(0);
            assert_eq!(to(&raft.ready().messages), [key(2), key(4)]);
            assert_eq!(raft.role(), Role::Leader);
        }

        #[test]
        fn refuses_a_second_change_until_the_first_leaves_and_commits() {
            let mut raft = leader();
            let joint = raft.propose_voters(set(&[1, 2, 4])).unwrap();
            let error = raft.propose_voters(set(&[1, 2])).unwrap_err();
            assert_eq!(error, Error::ChangePending { at: joint });
            assert_eq!(
                error.to_string(),
                "a configuration change at index 2 in term 1 is pending"
            );
            accept(&mut raft, &[2], 2);
            let leave = Position {
                term: Term(1),
                index: 3,
            };
            assert_eq!(
                raft.propose_voters(set(&[1, 2])),
                Err(Error::ChangePending { at: leave })
            );
            accept(&mut raft, &[2], 3);
            assert_eq!(raft.propose_voters(set(&[1, 2])).unwrap().index, 4);
        }

        #[test]
        fn a_new_leader_refuses_a_change_until_its_log_commits() {
            let start = Start {
                entries: vec![
                    entries(&[(1, 1)]).remove(0),
                    config(1, 2, voters(&[1, 2, 3], &[])),
                ],
                ..start(&[1, 2, 3], at_term(1))
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            elect(&mut raft, &[2]);
            let at = Position {
                term: Term(1),
                index: 2,
            };
            assert_eq!(
                raft.propose_voters(set(&[1, 2])),
                Err(Error::ChangePending { at })
            );
            accept(&mut raft, &[2], 3);
            assert_eq!(raft.propose_voters(set(&[1, 2])).unwrap().index, 4);
        }

        #[test]
        fn a_leader_elected_under_a_joint_start_leaves_the_joint_phase() {
            let start = Start {
                voters: voters(&[1, 2], &[1, 2, 3]),
                ..start(&[], Hard::default())
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            elect(&mut raft, &[2, 3]);
            let new = voters(&[1, 2], &[]);
            assert_eq!(raft.voters(), &new);
            let leave = Position {
                term: Term(1),
                index: 2,
            };
            assert_eq!(
                raft.propose_voters(set(&[1, 2, 3])),
                Err(Error::ChangePending { at: leave })
            );
            accept(&mut raft, &[2], 2);
            assert_eq!(raft.ready().committed[1], written(1, 2, new, &[1, 2]));
        }

        #[test]
        fn a_group_of_one_commits_the_joint_entry_and_the_leave_at_once() {
            let mut raft = raft(&[1], Hard::default());
            raft.campaign();
            assert_eq!(raft.role(), Role::Leader);
            sent(&mut raft);
            raft.propose_voters(set(&[1])).unwrap();
            let committed = raft.ready().committed;
            assert_eq!(
                committed,
                [
                    written(1, 2, voters(&[1], &[1]), &[1]),
                    written(1, 3, voters(&[1], &[]), &[1])
                ]
            );
        }

        #[test]
        fn a_leader_that_removes_itself_steps_down_after_the_leave_commits() {
            let mut raft = leader();
            raft.propose_voters(set(&[2, 3])).unwrap();
            accept(&mut raft, &[2, 3], 2);
            assert_eq!(raft.voters(), &voters(&[2, 3], &[]));
            assert_eq!(raft.role(), Role::Leader);
            sent(&mut raft);
            accept(&mut raft, &[2, 3], 3);
            assert_eq!((raft.role(), raft.leader()), (Role::Follower, None));
            // The commit of the leave goes out before the leader steps down.
            let messages = sent(&mut raft);
            assert_eq!(to(&messages), [key(2), key(3)]);
            let commit = Body::Append {
                prev: Position {
                    term: Term(1),
                    index: 3,
                },
                entries: Vec::new(),
                commit: 3,
            };
            assert_eq!(messages[0].body, commit);
            tick_times(&mut raft, 40);
            assert_eq!(raft.role(), Role::Follower);
        }

        // Node 3 holds the leave before it commits, so the commit releases it at
        // once: it gets the commit as a heartbeat, not as an append.
        #[test]
        fn a_removed_node_that_holds_the_leave_is_released_when_it_commits() {
            let mut raft = leader();
            raft.propose_voters(set(&[1, 2, 4])).unwrap();
            accept(&mut raft, &[2], 2);
            sent(&mut raft);
            accept(&mut raft, &[3], 3);
            assert_eq!(sent(&mut raft), []);
            accept(&mut raft, &[2], 3);
            let ready = raft.ready();
            assert_eq!(
                ready.committed,
                [written(1, 3, voters(&[1, 2, 4], &[]), &[1, 2])]
            );
            let to_3: Vec<&Body> = ready
                .messages
                .iter()
                .filter(|m| m.to == key(3))
                .map(|m| &m.body)
                .collect();
            assert_eq!(to_3, [&Body::Heartbeat { commit: 3 }]);
            raft.tick(0);
            assert_eq!(to(&raft.ready().messages), [key(2), key(4)]);
        }

        // Node 3 holds the joint entry and the leave that removes it, neither
        // committed. Whether it restarts with them or gets them in an append, its
        // peers are the voters in force and the voters before them.
        #[test]
        fn a_restart_with_an_uncommitted_leave_has_the_peers_an_append_gives() {
            let joint = voters(&[1, 2], &[1, 2, 3]);
            let new = voters(&[1, 2], &[]);
            let entries = vec![config(1, 1, joint), config(1, 2, new)];
            let fresh = start(&[1, 2, 3], Hard::default());
            let held = Start {
                hard: Hard {
                    term: Term(1),
                    vote: None,
                    leader: None,
                    proof: None,
                },
                entries: entries.clone(),
                ..fresh.clone()
            };
            let config = Config {
                key: key(3),
                ..CONFIG
            };
            let restarted = Raft::new(config, held).unwrap();
            let mut appended = Raft::new(config, fresh).unwrap();
            let append = Body::Append {
                prev: Position::default(),
                entries,
                commit: 0,
            };
            let append = Message {
                to: key(3),
                ..message(1, 1, append)
            };
            appended.step(append).unwrap();
            let peers = |raft: &Raft| raft.peers.keys().copied().collect::<Vec<_>>();
            assert_eq!(peers(&restarted), peers(&appended));
            assert_eq!(peers(&restarted), [key(1), key(2)]);
        }

        // The leave that removes node 1 is not committed, so node 1 may campaign. It
        // has no vote of its own to count: 2 and 3 elect it.
        #[test]
        fn a_removed_leader_campaigns_with_no_peer_of_its_own() {
            let mut raft = leader();
            assert!(!raft.peers.contains_key(&key(1)));
            raft.propose_voters(set(&[2, 3])).unwrap();
            accept(&mut raft, &[2, 3], 2);
            assert_eq!(raft.voters(), &voters(&[2, 3], &[]));
            tick_times(&mut raft, 20);
            assert_eq!(raft.role(), Role::Follower);
            elect(&mut raft, &[2, 3]);
            assert!(!raft.peers.contains_key(&key(1)));
        }

        // Node 1 is in neither `Start.voters` nor the change, which is not committed.
        #[test]
        fn a_node_outside_both_configurations_does_not_campaign() {
            let mut raft = raft(&[2, 3], Hard::default());
            let joint = voters(&[2, 3, 4], &[2, 3]);
            let change = append(Position::default(), vec![config(1, 1, joint)], 0);
            raft.step(message(2, 1, change)).unwrap();
            sent(&mut raft);
            raft.campaign();
            tick_times(&mut raft, 20);
            assert_eq!(raft.role(), Role::Follower);
            assert_eq!(sent(&mut raft), []);
        }

        // Node 1 holds the joint entry and the leave that removes it. While the
        // leave is not committed, it may campaign as a voter of the configuration
        // before the leave. Once the leave commits, it may not.
        #[test]
        fn a_voter_of_the_configuration_before_an_uncommitted_leave_campaigns() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            let joint = voters(&[2, 3], &[1, 2, 3]);
            let new = voters(&[2, 3], &[]);
            let entries = vec![config(1, 1, joint), config(1, 2, new)];
            raft.step(message(2, 1, append(Position::default(), entries, 1)))
                .unwrap();
            sent(&mut raft);
            raft.campaign();
            assert_eq!(raft.role(), Role::PreCandidate);
            assert_eq!(to(&sent(&mut raft)), [key(2), key(3)]);
            raft.step(message(2, 2, Body::Heartbeat { commit: 2 }))
                .unwrap();
            assert_eq!(raft.role(), Role::Follower);
            sent(&mut raft);
            raft.campaign();
            tick_times(&mut raft, 20);
            assert_eq!(raft.role(), Role::Follower);
            assert_eq!(sent(&mut raft), []);
        }

        // Node 1 is not in `Start.voters`. It joins, and its leave is not committed.
        #[test]
        fn a_node_that_joined_later_campaigns_before_its_leave_commits() {
            let mut raft = raft(&[2, 3], Hard::default());
            let entries = vec![
                config(1, 1, voters(&[1, 2, 3], &[2, 3])),
                config(1, 2, voters(&[1, 2, 3], &[])),
                config(1, 3, voters(&[2, 3], &[1, 2, 3])),
                config(1, 4, voters(&[2, 3], &[])),
            ];
            raft.step(message(2, 1, append(Position::default(), entries, 3)))
                .unwrap();
            sent(&mut raft);
            raft.campaign();
            assert_eq!(raft.role(), Role::PreCandidate);
            assert_eq!(to(&sent(&mut raft)), [key(2), key(3)]);
        }

        // Node 1 is only in `Start.voters`. Every configuration after it leaves 1 out.
        #[test]
        fn a_node_only_in_the_start_configuration_does_not_campaign() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            let entries = vec![
                config(1, 1, voters(&[2, 3], &[1, 2, 3])),
                config(1, 2, voters(&[2, 3], &[])),
                config(1, 3, voters(&[2, 3, 4], &[2, 3])),
            ];
            raft.step(message(2, 1, append(Position::default(), entries, 2)))
                .unwrap();
            sent(&mut raft);
            raft.campaign();
            tick_times(&mut raft, 20);
            assert_eq!(raft.role(), Role::Follower);
            assert_eq!(sent(&mut raft), []);
        }

        // Node 1 holds three uncommitted configuration entries: the joint entry and
        // the leave that remove it, and a joint entry that adds it back. A new leader
        // replaces the last. The leave is in force again, and node 1 is a voter of
        // the configuration before it, so it may campaign.
        #[test]
        fn a_truncated_entry_gives_the_configuration_before_it_back() {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            let changes = vec![
                config(1, 1, voters(&[2, 3], &[1, 2, 3])),
                config(1, 2, voters(&[2, 3], &[])),
                config(1, 3, voters(&[1, 2, 3], &[2, 3])),
            ];
            raft.step(message(2, 1, append(Position::default(), changes, 0)))
                .unwrap();
            sent(&mut raft);
            let replace = append(position(1, 2), entries(&[(2, 3)]), 0);
            raft.step(message(3, 2, replace)).unwrap();
            sent(&mut raft);
            assert_eq!(raft.voters(), &voters(&[2, 3], &[]));
            raft.campaign();
            assert_eq!(raft.role(), Role::PreCandidate);
            assert_eq!(to(&sent(&mut raft)), [key(2), key(3)]);
        }

        // Node 1 restarts with the leave of node 4 applied: the leave is committed
        // and node 4 was released before the restart. Its campaign asks the voters
        // alone, and once it leads it sends node 4 no entry.
        #[test]
        fn a_restart_with_a_committed_leave_has_released_the_removed_node() {
            let mut entries = entries(&[(1, 1)]);
            entries.push(config(1, 2, voters(&[1, 2, 3], &[1, 2, 3, 4])));
            entries.push(config(1, 3, voters(&[1, 2, 3], &[])));
            let held = Start {
                hard: Hard {
                    term: Term(1),
                    vote: None,
                    leader: None,
                    proof: None,
                },
                entries,
                applied: 3,
                ..start(&[1, 2, 3, 4], Hard::default())
            };
            let mut raft = Raft::new(CONFIG, held).unwrap();
            assert_eq!(raft.voters(), &voters(&[1, 2, 3], &[]));
            raft.campaign();
            assert_eq!(to(&sent(&mut raft)), [key(2), key(3)]);
            elect(&mut raft, &[3]);
            raft.step(message(4, 2, Body::AppendReject { hint: 2 }))
                .unwrap();
            assert_eq!(raft.role(), Role::Leader);
            assert_eq!(appended(&mut raft, 4), Vec::<u64>::new());
        }

        // The indexes of the entries in each `Append` to node `to`.
        fn appended(raft: &mut Raft, to: u8) -> Vec<u64> {
            sent(raft)
                .into_iter()
                .filter(|m| m.to == key(to))
                .filter_map(|m| {
                    let Body::Append { entries, .. } = m.body else {
                        return None;
                    };
                    Some(entries)
                })
                .flatten()
                .map(|entry| entry.at.index)
                .collect()
        }

        // Node 3 lags when the change removes it, and its answers to appends are
        // lost. Whether it answers each heartbeat or nothing, the leader sends it no
        // entry past the leave at index 3.
        #[test]
        fn a_leader_sends_a_removed_node_no_entry_past_the_leave() {
            for answers in [true, false] {
                let mut raft = leader();
                raft.propose_voters(set(&[1, 2])).unwrap();
                accept(&mut raft, &[2], 2);
                accept(&mut raft, &[2], 3);
                assert_eq!(raft.voters(), &voters(&[1, 2], &[]));
                let mut past = Vec::new();
                for round in 0..30_u8 {
                    let at = raft.propose(vec![round]).unwrap();
                    accept(&mut raft, &[2], at.index);
                    raft.tick(0);
                    if answers {
                        raft.step(message(3, 1, Body::HeartbeatReply)).unwrap();
                    }
                    past.extend(appended(&mut raft, 3).into_iter().filter(|&i| i > 3));
                }
                assert_eq!(raft.role(), Role::Leader);
                assert_eq!(past, Vec::<u64>::new(), "answers: {answers}");
            }
        }

        // Node 5's vote arrives after the change that removes it is in force. Node 5
        // is still a peer, but its vote does not join the votes that the leader
        // carries to node 4, which has not answered.
        #[test]
        fn a_late_vote_of_a_removed_node_does_not_join_the_votes() {
            let mut raft = raft(&[1, 2, 3, 4, 5], Hard::default());
            elect(&mut raft, &[2, 3]);
            accept(&mut raft, &[2, 3], 1);
            raft.propose_voters(set(&[1, 2, 3, 4])).unwrap();
            accept(&mut raft, &[2, 3], 2);
            accept(&mut raft, &[2, 3], 3);
            assert_eq!(raft.voters(), &voters(&[1, 2, 3, 4], &[]));
            raft.step(message(5, 1, Body::VoteReply { answer: GRANTED }))
                .unwrap();
            sent(&mut raft);
            raft.tick(0);
            let to_4 = sent(&mut raft).into_iter().find(|m| m.to == key(4));
            let voters = to_4.unwrap().proof.unwrap().voters;
            let expected = [key(1), key(2), key(3)];
            assert_eq!(voters.into_keys().collect::<Vec<_>>(), expected);
        }

        // Node 3 lags and answers each heartbeat, so it is still a peer when the
        // next change comes into force. That change releases it: node 3 gets
        // nothing more.
        #[test]
        fn the_next_change_releases_a_removed_node_that_is_still_a_peer() {
            let mut raft = leader();
            raft.propose_voters(set(&[1, 2])).unwrap();
            accept(&mut raft, &[2], 2);
            accept(&mut raft, &[2], 3);
            raft.step(message(3, 1, Body::HeartbeatReply)).unwrap();
            sent(&mut raft);
            raft.propose_voters(set(&[1, 2, 4])).unwrap();
            let mut to_3 = Vec::new();
            for _ in 0..30 {
                raft.tick(0);
                for from in [2, 3, 4] {
                    raft.step(message(from, 1, Body::HeartbeatReply)).unwrap();
                }
                to_3.extend(sent(&mut raft).into_iter().filter(|m| m.to == key(3)));
            }
            assert_eq!(raft.role(), Role::Leader);
            assert_eq!(to_3, []);
        }

        // One append brings node 1 the leave of node 3 and the next joint entry.
        // The joint entry is in force, so node 3 is not a peer, and the new leader
        // sends it nothing.
        #[test]
        fn two_changes_in_one_append_release_the_node_the_first_removed() {
            let mut raft = raft(&[1, 2, 3, 4], Hard::default());
            let body = Body::Append {
                prev: Position::default(),
                entries: vec![
                    entries(&[(1, 1)]).remove(0),
                    config(1, 2, voters(&[1, 2, 4], &[1, 2, 3, 4])),
                    config(1, 3, voters(&[1, 2, 4], &[])),
                    config(1, 4, voters(&[1, 2, 4, 5], &[1, 2, 4])),
                ],
                commit: 3,
            };
            raft.step(message(2, 1, body)).unwrap();
            sent(&mut raft);
            elect(&mut raft, &[2, 4]);
            raft.step(message(3, 2, Body::AppendReject { hint: 2 }))
                .unwrap();
            assert_eq!(raft.role(), Role::Leader);
            let to_3: Vec<Message> = sent(&mut raft)
                .into_iter()
                .filter(|m| m.to == key(3))
                .collect();
            assert_eq!(to_3, []);
        }

        // Node 1 holds the leave of node 3, not committed, when a second append
        // brings `later`. Node 1 then wins term 2, and nodes 3 and 4 reject its
        // probe. Gives the indexes of the entries that nodes 3 and 4 get.
        fn lead_after(later: Vec<Entry>) -> (Vec<u64>, Vec<u64>) {
            let mut raft = raft(&[1, 2, 3], Hard::default());
            let body = Body::Append {
                prev: Position::default(),
                entries: vec![
                    entries(&[(1, 1)]).remove(0),
                    config(1, 2, voters(&[1, 2], &[1, 2, 3])),
                    config(1, 3, voters(&[1, 2], &[])),
                ],
                commit: 2,
            };
            raft.step(message(2, 1, body)).unwrap();
            let last = later.last().unwrap().at.index;
            let body = Body::Append {
                prev: Position {
                    term: Term(1),
                    index: 3,
                },
                entries: later,
                commit: last - 1,
            };
            raft.step(message(2, 1, body)).unwrap();
            assert_eq!(raft.voters(), &voters(&[1, 2], &[]));
            sent(&mut raft);
            elect(&mut raft, &[2]);
            raft.propose(vec![7]).unwrap();
            sent(&mut raft);
            let mut reject = |from| {
                raft.step(message(from, 2, Body::AppendReject { hint: 2 }))
                    .unwrap();
                appended(&mut raft, from)
            };
            (reject(3), reject(4))
        }

        // The later change ends at the voters already in force, at another index.
        // It releases node 3, and node 4, which it removed, gets entries up to the
        // leader's first entry at index 8, not the proposal at index 9.
        #[test]
        fn a_later_change_to_the_same_voters_releases_a_removed_node() {
            let later = vec![
                config(1, 4, voters(&[1, 2, 4], &[1, 2])),
                config(1, 5, voters(&[1, 2, 4], &[])),
                config(1, 6, voters(&[1, 2], &[1, 2, 4])),
                config(1, 7, voters(&[1, 2], &[])),
            ];
            assert_eq!(lead_after(later), (vec![], vec![3, 4, 5, 6, 7, 8]));
        }

        #[test]
        fn a_later_change_that_changes_no_voter_releases_a_removed_node() {
            let later = vec![
                config(1, 4, voters(&[1, 2], &[1, 2])),
                config(1, 5, voters(&[1, 2], &[])),
            ];
            assert_eq!(lead_after(later), (vec![], vec![]));
        }

        // Node 3 missed its release, so it campaigns. The leader sends it nothing:
        // the caller tells it that it is out.
        #[test]
        fn a_leader_sends_nothing_to_a_removed_node_that_campaigns() {
            let mut raft = leader();
            raft.propose_voters(set(&[1, 2, 4])).unwrap();
            accept(&mut raft, &[2], 2);
            accept(&mut raft, &[2, 3], 3);
            sent(&mut raft);
            let last = Position {
                term: Term(1),
                index: 3,
            };
            raft.step(message(3, 2, Body::PreVote { last })).unwrap();
            assert_eq!(sent(&mut raft), []);
            raft.tick(0);
            assert_eq!(raft.role(), Role::Leader);
            assert_eq!(to(&sent(&mut raft)), [key(2), key(4)]);
        }

        // A follower that released node 4 drops its PreVote.
        #[test]
        fn a_follower_sends_nothing_to_a_removed_node_that_campaigns() {
            let mut raft = raft(&[1, 2, 3, 4], Hard::default());
            let mut entries = entries(&[(1, 1)]);
            entries.push(config(1, 2, voters(&[1, 2, 3], &[1, 2, 3, 4])));
            entries.push(config(1, 3, voters(&[1, 2, 3], &[])));
            let append = Body::Append {
                prev: Position::default(),
                entries,
                commit: 3,
            };
            raft.step(message(2, 1, append)).unwrap();
            sent(&mut raft);
            let last = Position {
                term: Term(1),
                index: 3,
            };
            raft.step(message(4, 2, Body::PreVote { last })).unwrap();
            assert_eq!(sent(&mut raft), []);
            assert_eq!(raft.role(), Role::Follower);
        }

        // Only the node a change adds counts as heard; the voters that stayed
        // silent still fail the quorum check.
        #[test]
        fn a_change_does_not_make_the_silent_voters_heard() {
            let mut raft = leader();
            // The first check passes on the accepted appends and clears the slate.
            tick_times(&mut raft, 19);
            assert_eq!(raft.role(), Role::Leader);
            raft.propose_voters(set(&[1, 2, 3, 4])).unwrap();
            raft.tick(0);
            assert_eq!(raft.role(), Role::Follower);
        }

        // A follower that takes a committed change releases the removed node, so
        // its campaign asks the voters alone, and a change that adds the node again
        // starts from a fresh peer that counts as heard.
        #[test]
        fn a_follower_releases_a_removed_node_when_the_change_commits() {
            let mut raft = raft(&[1, 2, 3, 4], Hard::default());
            let mut entries = entries(&[(1, 1)]);
            entries.push(config(1, 2, voters(&[1, 2, 3], &[1, 2, 3, 4])));
            entries.push(config(1, 3, voters(&[1, 2, 3], &[])));
            let append = Body::Append {
                prev: Position::default(),
                entries,
                commit: 3,
            };
            raft.step(message(2, 1, append)).unwrap();
            assert_eq!(raft.voters(), &voters(&[1, 2, 3], &[]));
            sent(&mut raft);
            raft.campaign();
            assert_eq!(to(&sent(&mut raft)), [key(2), key(3)]);
            elect(&mut raft, &[3]);
            raft.propose_voters(set(&[1, 2, 3, 4])).unwrap();
            raft.step(message(3, 2, Body::HeartbeatReply)).unwrap();
            tick_times(&mut raft, 10);
            assert_eq!(raft.role(), Role::Leader);
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

        // Node 1 leads at term 2 with the joint configuration as an uncommitted entry
        // at index 1, so the joint phase does not leave.
        fn joint_leader(incoming: &[u8], outgoing: &[u8]) -> Raft {
            let joint = Voters {
                incoming: incoming.iter().copied().map(key).collect(),
                outgoing: outgoing.iter().copied().map(key).collect(),
            };
            let start = Start {
                entries: vec![Entry {
                    at: position(1, 1),
                    data: change(2, joint.clone()),
                }],
                ..start(outgoing, at_term(1))
            };
            let mut raft = Raft::new(CONFIG, start).unwrap();
            elect(&mut raft, &[2, 4]);
            assert_eq!(raft.voters(), &joint);
            raft
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
            let granted = Body::PreVoteReply { answer: GRANTED };
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
            raft.step(message(2, 1, Body::PreVoteReply { answer: GRANTED }))
                .unwrap();
            raft.step(message(4, 0, Body::PreVoteReply { answer: REFUSED }))
                .unwrap();
            assert_eq!(raft.role(), Role::PreCandidate);
            raft.step(message(5, 0, Body::PreVoteReply { answer: REFUSED }))
                .unwrap();
            assert_eq!((raft.role(), raft.term()), (Role::Follower, Term(0)));
        }

        #[test]
        fn a_commit_needs_a_majority_of_each_set() {
            let mut raft = joint_leader(&[1, 2, 3], &[1, 4, 5]);
            raft.propose(vec![7]).unwrap();
            sent(&mut raft);
            let accepted = Body::AppendReply { last: 3 };
            raft.step(message(2, 2, accepted.clone())).unwrap();
            raft.step(message(3, 2, accepted.clone())).unwrap();
            assert_eq!(raft.ready().committed, []);
            raft.step(message(4, 2, accepted)).unwrap();
            let committed: Vec<u64> = raft
                .ready()
                .committed
                .iter()
                .map(|entry| entry.at.index)
                .collect();
            assert_eq!(committed, [1, 2, 3]);
        }

        #[test]
        fn a_leader_steps_down_without_a_majority_of_each_set() {
            let mut raft = joint_leader(&[1, 2, 3], &[1, 4, 5]);
            raft.step(message(2, 2, Body::HeartbeatReply)).unwrap();
            raft.step(message(4, 2, Body::HeartbeatReply)).unwrap();
            tick_times(&mut raft, 10);
            assert_eq!(raft.role(), Role::Leader);
            raft.step(message(2, 2, Body::HeartbeatReply)).unwrap();
            raft.step(message(3, 2, Body::HeartbeatReply)).unwrap();
            tick_times(&mut raft, 10);
            assert_eq!(raft.role(), Role::Follower);
        }
    }
}
