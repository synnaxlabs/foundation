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

    // Whether this is a joint configuration.
    pub(crate) fn joint(&self) -> bool {
        !self.outgoing.is_empty()
    }

    // The joint configuration that moves from these voters to `incoming`.
    pub(crate) fn enter(&self, incoming: BTreeSet<node::Key>) -> Self {
        Self {
            incoming,
            outgoing: self.incoming.clone(),
        }
    }

    // The configuration that ends this joint phase: `incoming` alone.
    pub(crate) fn leave(&self) -> Self {
        Self {
            incoming: self.incoming.clone(),
            outgoing: BTreeSet::new(),
        }
    }

    // Whether `key` is in either set.
    pub(crate) fn contains(&self, key: node::Key) -> bool {
        self.incoming.contains(&key) || self.outgoing.contains(&key)
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

// The cases come from etcd's quorum tables, unchanged. `oracles/conformance/raft/
// README.md` explains their format.
#[cfg(test)]
mod quorum {
    use std::collections::BTreeMap;
    use std::collections::btree_map::Entry;

    use super::*;

    macro_rules! table {
        ($name:literal) => {
            include_str!(concat!("../../../oracles/conformance/raft/quorum/", $name))
        };
    }

    fn key(id: u64) -> node::Key {
        node::Key::from_u128(u128::from(id))
    }

    // One `cfg=(..) cfgj=(..) idx=(..)` line. The values go to the ids in order of
    // first appearance, as etcd's test harness does; `_` is an absent value.
    struct Case {
        voters: Voters,
        values: BTreeMap<node::Key, String>,
    }

    fn parse_list(arg: &str) -> Vec<String> {
        let inner = arg
            .strip_prefix('(')
            .and_then(|arg| arg.strip_suffix(')'))
            .unwrap_or_else(|| panic!("list expected, got {arg}"));
        inner
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_owned)
            .collect()
    }

    fn parse_case(line: &str) -> Case {
        let mut incoming = Vec::new();
        let mut outgoing = Vec::new();
        let mut values = Vec::new();
        for arg in line.replace(", ", ",").split_whitespace().skip(1) {
            let (name, value) = arg.split_once('=').expect("name=value");
            let keys = |value: &str| -> Vec<node::Key> {
                parse_list(value)
                    .iter()
                    .map(|id| key(id.parse().expect("an id")))
                    .collect()
            };
            match name {
                "cfg" => incoming = keys(value),
                "cfgj" if value == "zero" => {}
                "cfgj" => outgoing = keys(value),
                "idx" | "votes" => values = parse_list(value),
                other => panic!("unknown argument {other}"),
            }
        }
        let mut given = BTreeMap::new();
        let mut next = values.into_iter().peekable();
        for &id in incoming.iter().chain(&outgoing) {
            if let (Entry::Vacant(slot), Some(value)) = (given.entry(id), next.peek()) {
                slot.insert(value.clone());
                next.next();
            }
        }
        Case {
            voters: Voters {
                incoming: incoming.into_iter().collect(),
                outgoing: outgoing.into_iter().collect(),
            },
            values: given.into_iter().filter(|(_, v)| v != "_").collect(),
        }
    }

    // Each case and the last line of its result block.
    fn cases(table: &str) -> Vec<(Case, String)> {
        let mut cases = Vec::new();
        let mut lines = table.lines().peekable();
        while let Some(line) = lines.next() {
            if !(line.starts_with("committed") || line.starts_with("vote")) {
                continue;
            }
            assert_eq!(lines.next(), Some("----"), "after {line}");
            let mut last = String::new();
            while let Some(result) = lines.next_if(|result| !result.is_empty()) {
                last = result.to_owned();
            }
            cases.push((parse_case(line), last));
        }
        cases
    }

    fn check_committed(table: &str, count: usize) {
        let cases = cases(table);
        assert_eq!(cases.len(), count);
        for (case, expected) in cases {
            let matched = |key| {
                case.values
                    .get(&key)
                    .map_or(0, |value| value.parse().expect("an index"))
            };
            let committed = case.voters.committed(matched);
            let shown = if committed == u64::MAX {
                "∞".to_owned()
            } else {
                committed.to_string()
            };
            let expected = expected.trim_start_matches("<empty majority quorum>");
            assert_eq!(shown, expected, "{:?}", case.voters);
        }
    }

    fn check_vote(table: &str, count: usize) {
        let cases = cases(table);
        assert_eq!(cases.len(), count);
        for (case, expected) in cases {
            let vote = |key| case.values.get(&key).map(|value| value == "y");
            let tally = match case.voters.tally(vote) {
                Tally::Won => "VoteWon",
                Tally::Lost => "VoteLost",
                Tally::Open => "VotePending",
            };
            assert_eq!(tally, expected, "{:?}", case.voters);
        }
    }

    #[test]
    fn majority_commit() {
        check_committed(table!("majority_commit.txt"), 16);
    }

    #[test]
    fn joint_commit() {
        check_committed(table!("joint_commit.txt"), 50);
    }

    #[test]
    fn majority_vote() {
        check_vote(table!("majority_vote.txt"), 22);
    }

    #[test]
    fn joint_vote() {
        check_vote(table!("joint_vote.txt"), 39);
    }
}
