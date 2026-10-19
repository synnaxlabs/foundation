use std::collections::BTreeMap;

use types::ed25519::PublicKey;
use types::node;

use crate::{Change, Entry, Position, Term, Voters};

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
    /// The configuration entries of the sender's log below `term`, oldest first,
    /// when the message carries a proof; else empty. A receiver that cannot count
    /// `proof` against its own configuration counts it against the last link it
    /// can trust, and keeps none of them. A message drops its oldest links until it
    /// [`fits`](crate::Config::fits).
    pub chain: Vec<Link>,
}

/// A configuration entry that a message carries as proof of the configuration
/// after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// Where the entry is in the sender's log.
    pub at: Position,
    /// The configuration, with the votes and the signature of the leader that
    /// wrote it.
    pub change: Change,
}

impl Link {
    /// Each claim the link carries, with its signature: its votes in the term of
    /// `at` in rising key order, then the leader's change at `at`, as
    /// [`Entry::claims`] gives them for the entry.
    pub fn claims(&self) -> impl Iterator<Item = (Claim<'_>, Option<Signature>)> {
        self.change.claims(self.at)
    }
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
    /// `leader` wrote the configuration entry `voters` at `at`, which binds
    /// `public_keys`.
    Change {
        /// The leader that signs.
        leader: node::Key,
        /// Where the entry is in the log.
        at: Position,
        /// The configuration.
        voters: &'a Voters,
        /// The keys that the change binds.
        public_keys: &'a BTreeMap<node::Key, PublicKey>,
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
    /// Each voter's grant in `term`, in rising key order, with its signature. The
    /// proof of a message grants in the term of the message, and the votes of a
    /// change in the term of its entry.
    pub fn claims(
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
    /// Whether `older`, a message that has not gone yet, can drop when this one goes
    /// after it: the sender left the term of `older`, or this one is a message of the
    /// same kind that brings all that `older` brings. False when they differ in sender
    /// or receiver.
    #[must_use]
    pub fn replaces(&self, older: &Message) -> bool {
        if older.from != self.from || older.to != self.to {
            return false;
        }
        // The term of a pre-vote and its reply can differ from the sender's own.
        let left = older.term < self.term
            && !matches!(self.body, Body::PreVote { .. } | Body::PreVoteReply { .. });
        let again = older.term <= self.term
            && match (&self.body, &older.body) {
                (Body::Heartbeat { .. }, Body::Heartbeat { .. })
                | (Body::PreVote { .. }, Body::PreVote { .. })
                | (Body::Vote { .. }, Body::Vote { .. })
                | (Body::HeartbeatReply, Body::HeartbeatReply) => true,
                // A grant of a later term carries another signature.
                (
                    Body::PreVoteReply { answer },
                    Body::PreVoteReply { answer: before },
                ) => std::mem::discriminant(answer) == std::mem::discriminant(before),
                _ => false,
            };
        left || again
    }

    // The claims of the proof in rising key order, then of each of `links`, then of
    // each change an append carries, then the sender's grant. `Raft::claims` picks
    // the links.
    pub(crate) fn claims<'a>(
        &'a self,
        links: &'a [Link],
    ) -> impl Iterator<Item = (Claim<'a>, Option<Signature>)> + 'a {
        let proof = self.proof.iter().flat_map(|proof| proof.claims(self.term));
        let links = links.iter().flat_map(|link| link.change.claims(link.at));
        let changes = self.body.entries().iter().flat_map(Entry::claims);
        let granted = self
            .body
            .granted()
            .map(|(grant, signature)| (self.claim(grant), signature));
        proof.chain(links).chain(changes).chain(granted)
    }

    // Gives each grant with no signature, and each `None` of a change it carries in
    // its chain or in an append, the signature `sign` makes for its claim.
    pub(crate) fn sign(&mut self, sign: &mut impl FnMut(&Claim<'_>) -> Signature) {
        if let Some(proof) = &mut self.proof {
            proof.sign(self.term, sign);
        }
        for link in &mut self.chain {
            link.change.sign(link.at, sign);
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

    /// What this body grants, with its signature: `None` unless it grants.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn message(from: u64, term: u64, body: Body) -> Message {
        Message {
            from: node::Key::from_u128(u128::from(from)),
            to: node::Key::from_u128(9),
            term: Term(term),
            body,
            proof: None,
            chain: Vec::new(),
        }
    }

    fn heartbeat(from: u64, term: u64) -> Message {
        message(from, term, Body::Heartbeat { commit: 0 })
    }

    fn append(from: u64, term: u64) -> Message {
        let body = Body::Append {
            prev: Position::default(),
            entries: Vec::new(),
            commit: 0,
        };
        message(from, term, body)
    }

    fn pre_vote(from: u64, term: u64) -> Message {
        let last = Position::default();
        message(from, term, Body::PreVote { last })
    }

    fn vote(from: u64, term: u64) -> Message {
        let last = Position::default();
        message(from, term, Body::Vote { last })
    }

    fn answer(from: u64, term: u64, body: fn(Answer) -> Body) -> Message {
        message(from, term, body(Answer::Granted(None)))
    }

    #[test]
    fn a_heartbeat_replaces_a_heartbeat_of_its_term_and_sender() {
        let older = Message {
            body: Body::Heartbeat { commit: 3 },
            ..heartbeat(1, 5)
        };
        assert!(heartbeat(1, 5).replaces(&older));
    }

    #[test]
    fn a_heartbeat_replaces_a_heartbeat_of_a_lower_term() {
        assert!(heartbeat(1, 5).replaces(&heartbeat(1, 4)));
        assert!(!heartbeat(1, 4).replaces(&heartbeat(1, 5)));
    }

    #[test]
    fn a_pre_vote_replaces_a_pre_vote_of_its_term_or_a_lower_one() {
        assert!(pre_vote(1, 5).replaces(&pre_vote(1, 5)));
        assert!(pre_vote(1, 5).replaces(&pre_vote(1, 4)));
        assert!(!pre_vote(1, 4).replaces(&pre_vote(1, 5)));
    }

    #[test]
    fn a_vote_replaces_a_vote_of_its_term_or_a_lower_one() {
        assert!(vote(1, 5).replaces(&vote(1, 5)));
        assert!(vote(1, 5).replaces(&vote(1, 4)));
        assert!(!vote(1, 4).replaces(&vote(1, 5)));
    }

    #[test]
    fn a_message_of_a_higher_term_replaces_each_message_of_a_lower_one() {
        let replies: [fn(Answer) -> Body; 2] = [
            |answer| Body::PreVoteReply { answer },
            |answer| Body::VoteReply { answer },
        ];
        let older = [
            heartbeat(1, 4),
            append(1, 4),
            pre_vote(1, 4),
            vote(1, 4),
            answer(1, 4, replies[0]),
            answer(1, 4, replies[1]),
            message(1, 4, Body::HeartbeatReply),
            message(1, 4, Body::AppendReply { last: 2 }),
            message(1, 4, Body::AppendReject { hint: 2 }),
        ];
        let newer = [
            heartbeat(1, 5),
            append(1, 5),
            vote(1, 5),
            answer(1, 5, replies[1]),
            message(1, 5, Body::HeartbeatReply),
            message(1, 5, Body::AppendReply { last: 1 }),
            message(1, 5, Body::AppendReject { hint: 1 }),
        ];
        for newer in &newer {
            for older in &older {
                assert!(newer.replaces(older), "{newer:?} keeps {older:?}");
            }
            assert!(!newer.replaces(&append(1, 5)), "{newer:?}");
        }
    }

    #[test]
    fn a_pre_vote_or_its_reply_of_a_higher_term_keeps_a_message_of_a_lower_one() {
        let reply = answer(1, 5, |answer| Body::PreVoteReply { answer });
        for older in [heartbeat(1, 4), append(1, 4), vote(1, 4)] {
            assert!(!pre_vote(1, 5).replaces(&older), "{older:?}");
            assert!(!reply.replaces(&older), "{older:?}");
        }
    }

    #[test]
    fn a_message_of_another_kind_at_its_term_keeps_older() {
        assert!(!heartbeat(1, 5).replaces(&append(1, 5)));
        assert!(!append(1, 5).replaces(&heartbeat(1, 5)));
        assert!(!vote(1, 5).replaces(&pre_vote(1, 5)));
        assert!(!append(1, 5).replaces(&append(1, 5)));
        for body in [
            Body::VoteReply {
                answer: Answer::Refused,
            },
            Body::AppendReply { last: 2 },
            Body::AppendReject { hint: 2 },
        ] {
            let reply = message(1, 5, body);
            assert!(!reply.replaces(&reply), "{reply:?}");
        }
    }

    #[test]
    fn a_heartbeat_reply_replaces_a_heartbeat_reply_of_its_term_or_a_lower_one() {
        let reply = |term| message(1, term, Body::HeartbeatReply);
        assert!(reply(5).replaces(&reply(5)));
        assert!(reply(5).replaces(&reply(4)));
        assert!(!reply(4).replaces(&reply(5)));
    }

    #[test]
    fn a_pre_vote_reply_replaces_one_of_its_answer_at_its_term_or_a_lower_one() {
        let refused = |term| {
            message(
                1,
                term,
                Body::PreVoteReply {
                    answer: Answer::Refused,
                },
            )
        };
        let granted = |term, byte| {
            let answer = Answer::Granted(Some(Signature([byte; 64])));
            message(1, term, Body::PreVoteReply { answer })
        };
        assert!(refused(5).replaces(&refused(5)));
        assert!(refused(5).replaces(&refused(4)));
        assert!(!refused(4).replaces(&refused(5)));
        assert!(granted(5, 1).replaces(&granted(5, 1)));
        assert!(granted(5, 1).replaces(&granted(4, 2)));
        assert!(!granted(4, 2).replaces(&granted(5, 1)));
    }

    #[test]
    fn a_refused_and_a_granted_pre_vote_reply_keep_each_other() {
        let refused = message(
            1,
            5,
            Body::PreVoteReply {
                answer: Answer::Refused,
            },
        );
        let granted = answer(1, 5, |answer| Body::PreVoteReply { answer });
        assert!(!refused.replaces(&granted));
        assert!(!granted.replaces(&refused));
        assert!(
            !refused.replaces(&answer(1, 4, |answer| Body::PreVoteReply { answer }))
        );
    }

    #[test]
    fn a_message_keeps_a_message_of_another_sender() {
        assert!(!heartbeat(1, 5).replaces(&heartbeat(2, 5)));
        assert!(!vote(1, 5).replaces(&heartbeat(2, 4)));
    }

    #[test]
    fn a_message_keeps_a_message_to_another_receiver() {
        let other = Message {
            to: node::Key::from_u128(8),
            ..heartbeat(1, 4)
        };
        assert!(!heartbeat(1, 5).replaces(&other));
        assert!(!vote(1, 5).replaces(&Message {
            to: other.to,
            ..vote(1, 5)
        }));
    }
}
