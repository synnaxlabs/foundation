use std::collections::BTreeSet;

use types::node;

use crate::Error;

/// The nodes whose votes count. `incoming` is the voter set. In a joint phase,
/// `outgoing` is the set it replaces, and an election, a commit, and a leader's
/// quorum check each need a majority of both sets. Otherwise `outgoing` is empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Voters {
    /// The voters, or in a joint phase the new voters.
    pub incoming: BTreeSet<node::Key>,
    /// The voters a joint phase replaces. Empty outside a joint phase.
    pub outgoing: BTreeSet<node::Key>,
}

// How a campaign stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tally {
    Won,
    Lost,
    // Answers are still missing.
    Open,
}

impl Voters {
    // Both sets empty is a node that only follows. An empty `incoming` with an
    // `outgoing` would run on the set a joint phase replaces.
    pub(crate) fn check(&self) -> Result<(), Error> {
        if self.incoming.is_empty() && !self.outgoing.is_empty() {
            return Err(Error::EmptyIncoming);
        }
        Ok(())
    }

    // Every node in either set, once.
    pub(crate) fn peers(&self) -> impl Iterator<Item = node::Key> + '_ {
        self.incoming.union(&self.outgoing).copied()
    }

    // The highest index that a majority of each set holds, where `matched` is the
    // index a voter holds. `u64::MAX` with no voter at all.
    pub(crate) fn committed(&self, matched: impl Fn(node::Key) -> u64) -> u64 {
        majority_committed(&self.incoming, &matched)
            .min(majority_committed(&self.outgoing, &matched))
    }

    // How a campaign stands, where `vote` is a voter's answer so far. Won needs a
    // majority of each set; lost when either set can no longer reach one.
    pub(crate) fn tally(&self, vote: impl Fn(node::Key) -> Option<bool>) -> Tally {
        match (
            majority_tally(&self.incoming, &vote),
            majority_tally(&self.outgoing, &vote),
        ) {
            (incoming, outgoing) if incoming == outgoing => incoming,
            (Tally::Lost, _) | (_, Tally::Lost) => Tally::Lost,
            (Tally::Won | Tally::Open, Tally::Won | Tally::Open) => Tally::Open,
        }
    }

    // Whether a majority of each set satisfies `pred`.
    pub(crate) fn quorum(&self, pred: impl Fn(node::Key) -> bool) -> bool {
        self.tally(|key| Some(pred(key))) == Tally::Won
    }
}

fn majority_committed(
    set: &BTreeSet<node::Key>,
    matched: &impl Fn(node::Key) -> u64,
) -> u64 {
    if set.is_empty() {
        return u64::MAX;
    }
    let mut held: Vec<u64> = set.iter().map(|&key| matched(key)).collect();
    held.sort_unstable();
    held[set.len() - (set.len() / 2 + 1)]
}

fn majority_tally(
    set: &BTreeSet<node::Key>,
    vote: &impl Fn(node::Key) -> Option<bool>,
) -> Tally {
    if set.is_empty() {
        return Tally::Won;
    }
    let (mut yes, mut open) = (0, 0);
    for &key in set {
        match vote(key) {
            Some(true) => yes += 1,
            Some(false) => {}
            None => open += 1,
        }
    }
    let quorum = set.len() / 2 + 1;
    if yes >= quorum {
        Tally::Won
    } else if yes + open >= quorum {
        Tally::Open
    } else {
        Tally::Lost
    }
}

// etcd's quorum tables run the math against their cases.
#[cfg(test)]
#[path = "../../../oracles/conformance/raft/quorum.rs"]
mod quorum;

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: u64) -> node::Key {
        node::Key::from_u128(u128::from(id))
    }

    #[test]
    fn rejects_an_empty_incoming_set_with_an_outgoing_set() {
        let voters = Voters {
            incoming: BTreeSet::new(),
            outgoing: BTreeSet::from([key(1)]),
        };
        assert_eq!(voters.check(), Err(Error::EmptyIncoming));
        assert_eq!(Voters::default().check(), Ok(()));
    }

    #[test]
    fn peers_names_a_node_in_both_sets_once() {
        let voters = Voters {
            incoming: BTreeSet::from([key(2), key(1)]),
            outgoing: BTreeSet::from([key(2), key(3)]),
        };
        let peers: Vec<node::Key> = voters.peers().collect();
        assert_eq!(peers, [key(1), key(2), key(3)]);
    }

    #[test]
    fn quorum_needs_a_majority_of_each_set() {
        let voters = Voters {
            incoming: BTreeSet::from([key(1), key(2), key(3)]),
            outgoing: BTreeSet::from([key(1), key(4), key(5)]),
        };
        assert!(!voters.quorum(|node| node <= key(3)));
        assert!(voters.quorum(|node| node <= key(4)));
    }
}
