//! The quorum math against the tables of etcd/raft (Copyright 2015 The etcd
//! Authors, Apache License 2.0, see `LICENSE`). The tables in `quorum/` are
//! unchanged; `README.md` explains their format.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use types::node;

use crate::voters::{Tally, Voters};

macro_rules! table {
    ($name:literal) => {
        include_str!(concat!("quorum/", $name))
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
