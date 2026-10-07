use std::collections::BTreeMap;

use types::node;

use crate::{Entry, Position, Term, Voters};

/// One message between two nodes of a voter group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// The sender.
    pub from: node::Key,
    /// The receiver.
    pub to: node::Key,
    /// The sender's term. A PreVote carries the term the sender would campaign in, and
    /// a reply that grants a PreVote or a vote carries the term of the request.
    pub term: Term,
    /// What the message says.
    pub body: Body,
    /// The proof of `term` the message carries, if any. A [`Body::Vote`] carries the
    /// sender's pre-votes. A leader's [`Body::Heartbeat`] or [`Body::Append`] carries
    /// its votes until the receiver answers an append, and again after the receiver
    /// is silent through a quorum check. An answer to a message of a lower term
    /// carries the proof of the sender's term.
    pub proof: Option<Proof>,
}

/// What a voter granted a candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grant {
    /// A pre-vote: the voter would vote for the candidate. It binds nothing.
    PreVote,
    /// A vote in the candidate's term.
    Vote,
}

/// A voter's signature of its [`Claim`]. A `Raft` carries it and never reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signature(pub [u8; 64]);

/// What a signature attests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim<'a> {
    /// `voter` grants `grant` to `candidate` in `term`.
    Grant {
        /// The voter that signs.
        voter: node::Key,
        /// What it grants.
        grant: Grant,
        /// The term of the grant.
        term: Term,
        /// The node it grants it to.
        candidate: node::Key,
    },
    /// `leader` wrote the configuration entry `voters` at `at`.
    Change {
        /// The leader that signs.
        leader: node::Key,
        /// Where the entry is in the log.
        at: Position,
        /// The configuration.
        voters: &'a Voters,
    },
}

impl Claim<'_> {
    /// The node whose signature attests the claim: the voter of a grant, or the
    /// leader of a change.
    #[must_use]
    pub fn signer(&self) -> node::Key {
        match *self {
            Self::Grant { voter, .. } => voter,
            Self::Change { leader, .. } => leader,
        }
    }
}

/// A voter's answer to a [`Body::PreVote`] or a [`Body::Vote`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The voter does not grant it.
    Refused,
    /// The voter grants it, with its signature. A `Raft` gives its own grant with
    /// `None`, for the caller to sign.
    Granted(Option<Signature>),
}

/// A quorum of signed pre-votes or votes for one candidate in one term: the term of
/// the message or hard state that holds it. A `Raft` counts the keys against its
/// own configuration and carries the signatures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proof {
    /// What the voters granted.
    pub grant: Grant,
    /// The node they granted it to.
    pub candidate: node::Key,
    /// The voters, the candidate included, each with its signature. A `Raft` gives
    /// its own entry with `None`, for the caller to sign.
    pub voters: BTreeMap<node::Key, Option<Signature>>,
}

impl Proof {
    // Each entry's grant in `term`, in rising key order, with its signature.
    pub(crate) fn claims(
        &self,
        term: Term,
    ) -> impl Iterator<Item = (Claim<'_>, Option<Signature>)> {
        self.voters.iter().map(move |(&voter, &signature)| {
            let claim = Claim::Grant {
                voter,
                grant: self.grant,
                term,
                candidate: self.candidate,
            };
            (claim, signature)
        })
    }

    // Gives each entry with no signature the signature `sign` makes for its claim.
    pub(crate) fn sign(
        &mut self,
        term: Term,
        sign: &mut impl FnMut(&Claim<'_>) -> Signature,
    ) {
        for (&voter, signature) in &mut self.voters {
            if signature.is_none() {
                *signature = Some(sign(&Claim::Grant {
                    voter,
                    grant: self.grant,
                    term,
                    candidate: self.candidate,
                }));
            }
        }
    }
}

impl Message {
    /// Each claim the message carries, with its signature: the entries of its proof
    /// in rising key order, then, for each change an append carries, its votes in
    /// the entry's term and the leader's change, then the sender's grant when the
    /// body grants. The caller checks each signature against its signer's key before
    /// `step`, and refuses a `None`: `step` keeps each signature as it came.
    pub fn claims(&self) -> impl Iterator<Item = (Claim<'_>, Option<Signature>)> + '_ {
        let proof = self.proof.iter().flat_map(|proof| proof.claims(self.term));
        let changes = self.body.entries().iter().flat_map(Entry::claims);
        let granted = self
            .body
            .granted()
            .map(|(grant, signature)| (self.claim(grant), signature));
        proof.chain(changes).chain(granted)
    }

    // Gives each grant with no signature, and each `None` of a change it carries,
    // the signature `sign` makes for its claim.
    pub(crate) fn sign(&mut self, sign: &mut impl FnMut(&Claim<'_>) -> Signature) {
        if let Some(proof) = &mut self.proof {
            proof.sign(self.term, sign);
        }
        if let Body::Append { entries, .. } = &mut self.body {
            for entry in entries {
                entry.sign(sign);
            }
        }
        if let Some((grant, None)) = self.body.granted() {
            let answer = Answer::Granted(Some(sign(&self.claim(grant))));
            self.body = match grant {
                Grant::PreVote => Body::PreVoteReply { answer },
                Grant::Vote => Body::VoteReply { answer },
            };
        }
    }

    // The sender's claim of a grant to the receiver.
    fn claim(&self, grant: Grant) -> Claim<'static> {
        Claim::Grant {
            voter: self.from,
            grant,
            term: self.term,
            candidate: self.to,
        }
    }
}

/// What a [`Message`] says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// Asks whether the receiver would vote for the sender. It changes no term.
    PreVote {
        /// The sender's last log position.
        last: Position,
    },
    /// Answers a [`Body::PreVote`].
    PreVoteReply {
        /// Whether the sender would vote for the receiver.
        answer: Answer,
    },
    /// Asks for the receiver's vote in the message's term.
    Vote {
        /// The sender's last log position.
        last: Position,
    },
    /// Answers a [`Body::Vote`].
    VoteReply {
        /// Whether the receiver has the sender's vote.
        answer: Answer,
    },
    /// A leader states that it leads the message's term.
    Heartbeat {
        /// The leader's commit index, no higher than what the receiver holds.
        commit: u64,
    },
    /// Answers a [`Body::Heartbeat`]. A leader counts it as contact for CheckQuorum.
    HeartbeatReply,
    /// Entries after `prev`, from the leader. An empty list probes the receiver's
    /// log or carries a new commit index.
    Append {
        /// The position the entries follow. The receiver takes them only when it
        /// holds it.
        prev: Position,
        /// The entries, in order from `prev.index + 1`.
        entries: Vec<Entry>,
        /// The leader's commit index.
        commit: u64,
    },
    /// Answers a [`Body::Append`] whose entries the receiver took.
    AppendReply {
        /// The index of the last entry the leader sent, which the receiver holds.
        last: u64,
    },
    /// Answers a [`Body::Append`] whose `prev` the receiver did not hold.
    AppendReject {
        /// The receiver's hint for the next `prev`: the last index its log may
        /// share with the leader's.
        hint: u64,
    },
}

impl Body {
    /// Whether only a leader sends this body.
    pub(crate) fn leads(&self) -> bool {
        matches!(self, Self::Heartbeat { .. } | Self::Append { .. })
    }

    /// What this body grants, with its signature: `None` unless it grants.
    // The entries an append carries; none for another body.
    pub(crate) fn entries(&self) -> &[Entry] {
        match self {
            Self::Append { entries, .. } => entries,
            Self::PreVote { .. }
            | Self::PreVoteReply { .. }
            | Self::Vote { .. }
            | Self::VoteReply { .. }
            | Self::Heartbeat { .. }
            | Self::HeartbeatReply
            | Self::AppendReply { .. }
            | Self::AppendReject { .. } => &[],
        }
    }

    pub(crate) fn granted(&self) -> Option<(Grant, Option<Signature>)> {
        match *self {
            Self::PreVoteReply {
                answer: Answer::Granted(signature),
            } => Some((Grant::PreVote, signature)),
            Self::VoteReply {
                answer: Answer::Granted(signature),
            } => Some((Grant::Vote, signature)),
            Self::PreVoteReply {
                answer: Answer::Refused,
            }
            | Self::VoteReply {
                answer: Answer::Refused,
            }
            | Self::PreVote { .. }
            | Self::Vote { .. }
            | Self::Heartbeat { .. }
            | Self::HeartbeatReply
            | Self::Append { .. }
            | Self::AppendReply { .. }
            | Self::AppendReject { .. } => None,
        }
    }

    /// Whether this body answers a request.
    pub(crate) fn answers(&self) -> bool {
        match self {
            Self::PreVote { .. }
            | Self::Vote { .. }
            | Self::Heartbeat { .. }
            | Self::Append { .. } => false,
            Self::PreVoteReply { .. }
            | Self::VoteReply { .. }
            | Self::HeartbeatReply
            | Self::AppendReply { .. }
            | Self::AppendReject { .. } => true,
        }
    }
}
