use types::node;

use crate::Error;

/// The nodes whose votes count. `incoming` is the voter set. In a joint phase,
/// `outgoing` is the set it replaces, and an election, a commit, and a leader's
/// quorum check each need a majority of both sets. Otherwise `outgoing` is empty.
/// [`Raft::new`](crate::Raft::new) sorts each list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Voters {
    /// The voters, or in a joint phase the new voters.
    pub incoming: Vec<node::Key>,
    /// The voters a joint phase replaces. Empty outside a joint phase.
    pub outgoing: Vec<node::Key>,
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
    // Sorts each list. A node in both lists is the normal joint overlap.
    pub(crate) fn normalize(&mut self) -> Result<(), Error> {
        for list in [&mut self.incoming, &mut self.outgoing] {
            list.sort_unstable();
            let mut pairs = list.iter().zip(list.iter().skip(1));
            if let Some((_, &twice)) = pairs.find(|(first, second)| first == second) {
                return Err(Error::DuplicateVoter(twice));
            }
        }
        Ok(())
    }

    // Every node in either list. A node in both comes twice.
    pub(crate) fn peers(&self) -> impl Iterator<Item = node::Key> + '_ {
        self.incoming.iter().chain(&self.outgoing).copied()
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
    pub(crate) fn reached(&self, pred: impl Fn(node::Key) -> bool) -> bool {
        self.tally(|key| Some(pred(key))) == Tally::Won
    }
}

fn majority_committed(set: &[node::Key], matched: &impl Fn(node::Key) -> u64) -> u64 {
    if set.is_empty() {
        return u64::MAX;
    }
    let mut held: Vec<u64> = set.iter().map(|&key| matched(key)).collect();
    held.sort_unstable();
    held[set.len() - (set.len() / 2 + 1)]
}

fn majority_tally(
    set: &[node::Key],
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

// The cases come from etcd's quorum tables, unchanged. `oracles/conformance/raft/
// README.md` explains their format.
#[cfg(test)]
mod tests {
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
        let mut voters = Voters::default();
        let mut values = Vec::new();
        for arg in line.replace(", ", ",").split_whitespace().skip(1) {
            let (name, value) = arg.split_once('=').expect("name=value");
            let keys = |value: &str| {
                parse_list(value)
                    .iter()
                    .map(|id| key(id.parse().expect("an id")))
                    .collect()
            };
            match name {
                "cfg" => voters.incoming = keys(value),
                "cfgj" if value == "zero" => {}
                "cfgj" => voters.outgoing = keys(value),
                "idx" | "votes" => values = parse_list(value),
                other => panic!("unknown argument {other}"),
            }
        }
        let mut given = BTreeMap::new();
        let mut next = values.into_iter().peekable();
        for id in voters.peers() {
            if let (Entry::Vacant(slot), Some(value)) = (given.entry(id), next.peek()) {
                slot.insert(value.clone());
                next.next();
            }
        }
        voters.normalize().unwrap();
        Case {
            voters,
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
        assert!(!cases.is_empty());
        cases
    }

    fn check_committed(table: &str) {
        for (case, expected) in cases(table) {
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
            assert!(
                expected.ends_with(&shown),
                "{:?}: got {shown}, expected {expected}",
                case.voters
            );
        }
    }

    fn check_vote(table: &str) {
        for (case, expected) in cases(table) {
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
        check_committed(table!("majority_commit.txt"));
    }

    #[test]
    fn joint_commit() {
        check_committed(table!("joint_commit.txt"));
    }

    #[test]
    fn majority_vote() {
        check_vote(table!("majority_vote.txt"));
    }

    #[test]
    fn joint_vote() {
        check_vote(table!("joint_vote.txt"));
    }

    #[test]
    fn normalize_sorts_each_list_and_finds_a_duplicate() {
        let mut voters = Voters {
            incoming: vec![key(3), key(1)],
            outgoing: vec![key(2), key(3), key(2)],
        };
        assert_eq!(voters.normalize(), Err(Error::DuplicateVoter(key(2))));
        assert_eq!(voters.incoming, [key(1), key(3)]);
        voters.outgoing.dedup();
        voters.normalize().unwrap();
        assert_eq!(voters.outgoing, [key(2), key(3)]);
    }

    #[test]
    fn reached_needs_a_majority_of_each_set() {
        let voters = Voters {
            incoming: vec![key(1), key(2), key(3)],
            outgoing: vec![key(1), key(4), key(5)],
        };
        assert!(!voters.reached(|node| node <= key(3)));
        assert!(voters.reached(|node| node <= key(4)));
    }
}
