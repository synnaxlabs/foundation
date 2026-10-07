use std::collections::BTreeSet;

use types::node;

use crate::{Claim, Error, Link, Position, Proof, Signature, Term, Voters};

/// One entry of the replicated log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Where the entry is in the log.
    pub at: Position,
    /// What the entry carries.
    pub data: Data,
}

impl Entry {
    // Each claim a change carries, with its signature; nothing for other data.
    pub(crate) fn claims(
        &self,
    ) -> impl Iterator<Item = (Claim<'_>, Option<Signature>)> {
        let change = match &self.data {
            Data::Voters(change) => Some(change),
            Data::Empty | Data::Bytes(_) => None,
        };
        change.into_iter().flat_map(|change| change.claims(self.at))
    }

    // Gives each `None` of a change the signature `sign` makes for its claim.
    pub(crate) fn sign(&mut self, sign: &mut impl FnMut(&Claim<'_>) -> Signature) {
        match &mut self.data {
            Data::Voters(change) => change.sign(self.at, sign),
            Data::Empty | Data::Bytes(_) => {}
        }
    }
}

/// What a log entry carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Data {
    /// A leader's first entry of its term. The caller applies nothing.
    Empty,
    /// What the caller proposed.
    Bytes(Vec<u8>),
    /// A voter configuration, with the proof that a leader of its term wrote it. A
    /// node uses it from the time it writes the entry, committed or not. The caller
    /// applies nothing.
    Voters(Change),
}

/// A voter configuration that a leader wrote. A node that missed it checks it with
/// `votes` and `signature`, then counts a later proof against it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// The configuration.
    pub voters: Voters,
    /// The votes that elected the leader that wrote the entry, in the entry's term.
    pub votes: Proof,
    /// The leader's signature of its [`Claim::Change`]. A `Raft` gives its own with
    /// `None`, for the caller to sign.
    pub signature: Option<Signature>,
}

impl Change {
    // Each vote in the term of `at`, in rising key order, then the leader's change.
    pub(crate) fn claims(
        &self,
        at: Position,
    ) -> impl Iterator<Item = (Claim<'_>, Option<Signature>)> {
        let change = Claim::Change {
            leader: self.votes.candidate,
            at,
            voters: &self.voters,
        };
        self.votes
            .claims(at.term)
            .chain(std::iter::once((change, self.signature)))
    }

    // Gives each `None` the signature `sign` makes for its claim: a vote in the
    // term of `at`, then the leader's signature of the change at `at`.
    pub(crate) fn sign(
        &mut self,
        at: Position,
        sign: &mut impl FnMut(&Claim<'_>) -> Signature,
    ) {
        self.votes.sign(at.term, sign);
        if self.signature.is_none() {
            self.signature = Some(sign(&Claim::Change {
                leader: self.votes.candidate,
                at,
                voters: &self.voters,
            }));
        }
    }
}

// The log in memory. Entry `i` has index `i + 1`, and terms start above zero and
// never decrease. Three indexes trail the end: `committed` is what a quorum holds,
// `applied` is the last entry given to the caller to apply, and `stable` is the
// last entry given to the caller to write. `voters` is the index of the last
// configuration entry, or 0 for `base`, the configuration the node started with.
#[derive(Debug)]
pub(crate) struct Log {
    entries: Vec<Entry>,
    committed: u64,
    applied: u64,
    stable: u64,
    // The indexes of the configuration entries, rising.
    configs: Vec<u64>,
    base: Voters,
}

impl Log {
    pub(crate) fn new(
        base: Voters,
        entries: Vec<Entry>,
        applied: u64,
    ) -> Result<Self, Error> {
        base.check()?;
        let run = check(Position::default(), entries)?;
        let last = run.last().map_or(0, |at| at.index);
        let Run { entries, .. } = run;
        if applied > last {
            return Err(Error::AppliedPastLog { applied, last });
        }
        Ok(Self {
            configs: configs_in(&entries),
            base,
            entries,
            committed: applied,
            applied,
            stable: last,
        })
    }

    pub(crate) fn last(&self) -> Position {
        self.entries
            .last()
            .map_or_else(Position::default, |entry| entry.at)
    }

    // `index` as an index that the log holds.
    pub(crate) fn held(&self, index: u64) -> Result<Held, Error> {
        let last = self.last().index;
        if index > last {
            return Err(Error::IndexPastLog { index, last });
        }
        Ok(Held(index))
    }

    pub(crate) fn committed(&self) -> u64 {
        self.committed
    }

    // The configuration in force and where it is: the last configuration entry, or
    // `base` at the zero position.
    pub(crate) fn voters(&self) -> (Position, &Voters) {
        match self.configs.last() {
            Some(&index) => {
                let (at, change) = self.config(index);
                (at, &change.voters)
            }
            None => (Position::default(), &self.base),
        }
    }

    // The configuration entry at `index`, one of `configs`.
    fn config(&self, index: u64) -> (Position, &Change) {
        let entry =
            &self.entries[usize::try_from(index - 1).expect("invariant: index fits")];
        match &entry.data {
            Data::Voters(change) => (entry.at, change),
            Data::Empty | Data::Bytes(_) => {
                unreachable!("invariant: `configs` names configuration entries")
            }
        }
    }

    // Whether the configuration in force is committed.
    pub(crate) fn settled(&self) -> bool {
        self.voters().0.index <= self.committed
    }

    // The last committed configuration: the last configuration entry at or below
    // `committed`, or the configuration before the entries.
    pub(crate) fn committed_voters(&self) -> Voters {
        self.voters_through(self.committed)
    }

    // The configuration before `index`: the one before the entries with no
    // configuration entry before `index`.
    pub(crate) fn voters_before(&self, index: u64) -> Voters {
        self.voters_through(index.saturating_sub(1))
    }

    // The last committed configuration entry with a term below `term`, or the
    // configuration before the entries: what elected a leader of `term`.
    pub(crate) fn committed_voters_below(&self, term: Term) -> Voters {
        self.voters_through(self.committed.min(self.first_of(term) - 1))
    }

    // The configuration entries with a term below `term`, oldest first, as the chain
    // of a message of `term`.
    pub(crate) fn links(&self, term: Term) -> Vec<Link> {
        self.configs
            .iter()
            .map(|&index| self.config(index))
            .take_while(|(at, _)| at.term < term)
            .map(|(at, change)| Link {
                at,
                change: change.clone(),
            })
            .collect()
    }

    // The last configuration entry at or below `index`, or the configuration before
    // the entries.
    fn voters_through(&self, index: u64) -> Voters {
        let through = self.configs.partition_point(|&config| config <= index);
        match through.checked_sub(1) {
            Some(last) => self.config(self.configs[last]).1.voters.clone(),
            None => self.before_entries(),
        }
    }

    // The configuration before the entries. A node that starts with no voters takes
    // it from its first configuration entry: from index 1, the joint entry whose
    // outgoing set is the founding configuration. A leave stands for its joint phase.
    fn before_entries(&self) -> Voters {
        if !self.base.incoming.is_empty() {
            return self.base.clone();
        }
        let first = self
            .configs
            .first()
            .map(|&index| &self.config(index).1.voters);
        first.map_or_else(Voters::default, Voters::replaced)
    }

    // Every node in the configuration in force and, while that configuration is
    // uncommitted, in the one before it.
    pub(crate) fn nodes(&self) -> BTreeSet<node::Key> {
        let (at, voters) = self.voters();
        let before = (!self.settled()).then(|| self.voters_before(at.index));
        voters
            .nodes()
            .chain(before.iter().flat_map(Voters::nodes))
            .collect()
    }

    // The position at `index`: the zero position for 0, `None` past the end.
    pub(crate) fn at(&self, index: u64) -> Option<Position> {
        if index == 0 {
            return Some(Position::default());
        }
        let at = usize::try_from(index - 1).ok()?;
        self.entries.get(at).map(|entry| entry.at)
    }

    // The index of the first entry of `term` or of a later term: one past the end
    // with none.
    pub(crate) fn first_of(&self, term: Term) -> u64 {
        let before = self.entries.partition_point(|entry| entry.at.term < term);
        u64::try_from(before).map_or(u64::MAX, |before| before.saturating_add(1))
    }

    // Up to `max` entries from index `from` through index `end`, cloned for a
    // message. Empty when `from` is past `end` or past the log.
    pub(crate) fn slice(&self, from: u64, end: u64, max: usize) -> Vec<Entry> {
        let offset = |index: u64| usize::try_from(index).unwrap_or(usize::MAX);
        let end = offset(end).min(self.entries.len());
        self.entries
            .get(offset(from.saturating_sub(1))..end)
            .unwrap_or_default()
            .iter()
            .take(max)
            .cloned()
            .collect()
    }

    // Appends one entry at the end as the leader.
    pub(crate) fn push(&mut self, term: Term, data: Data) -> Position {
        let at = Position {
            term,
            index: self.last().index + 1,
        };
        if matches!(data, Data::Voters(_)) {
            self.configs.push(at.index);
        }
        self.entries.push(Entry { at, data });
        at
    }

    // Appends the leader's entries as a follower. Entries already in the log stay;
    // the first entry that differs replaces it and all after it. Returns the index of
    // the last entry the leader sent, or the commit index when the run's `prev` is
    // below it, or an error with the follower's hint for the next `prev`: its last
    // index, or the index before a `prev` it does not have.
    pub(crate) fn append(&mut self, run: Run) -> Result<u64, u64> {
        let Run { prev, entries } = run;
        if prev.index < self.committed {
            return Ok(self.committed);
        }
        if self.at(prev.index) != Some(prev) {
            return Err(prev.index.saturating_sub(1).min(self.last().index));
        }
        let last = entries.last().map_or(prev.index, |entry| entry.at.index);
        let Some(first) = entries
            .iter()
            .position(|entry| self.at(entry.at.index) != Some(entry.at))
        else {
            return Ok(last);
        };
        let from = entries[first].at.index;
        let kept = usize::try_from(from - 1).unwrap_or(usize::MAX);
        self.entries.truncate(kept);
        self.stable = self.stable.min(from - 1);
        self.configs
            .truncate(self.configs.partition_point(|&config| config < from));
        self.entries.extend(entries.into_iter().skip(first));
        self.configs.extend(configs_in(&self.entries[kept..]));
        Ok(last)
    }

    // Raises the commit index to `index`; a lower `index` changes nothing.
    //
    // Panics when `index` is past the end: a leader names only entries the
    // follower holds, and a leader commits only its own entries.
    pub(crate) fn commit_to(&mut self, index: u64) {
        let last = self.last().index;
        assert!(
            index <= last,
            "invariant: commit index {index} is past the last log index {last}"
        );
        self.committed = self.committed.max(index);
    }

    // The entries no `Ready` has given to write yet.
    pub(crate) fn take_unstable(&mut self) -> Vec<Entry> {
        let entries = self.slice(self.stable + 1, u64::MAX, usize::MAX);
        self.stable = self.last().index;
        entries
    }

    // The committed entries no `Ready` has given to apply yet.
    pub(crate) fn take_committed(&mut self) -> Vec<Entry> {
        let entries = self.slice(self.applied + 1, self.committed, usize::MAX);
        self.applied = self.committed;
        entries
    }
}

fn voters_in(entry: &Entry) -> Option<&Voters> {
    match &entry.data {
        Data::Voters(change) => Some(&change.voters),
        Data::Empty | Data::Bytes(_) => None,
    }
}

// The indexes of the configuration entries among `entries`, rising.
fn configs_in(entries: &[Entry]) -> Vec<u64> {
    entries
        .iter()
        .filter(|entry| voters_in(entry).is_some())
        .map(|entry| entry.at.index)
        .collect()
}

// Entries that follow `prev`: indexes in sequence, terms non-decreasing and not
// zero, and each configuration with a voter. Only `check` makes one.
#[derive(Debug)]
pub(crate) struct Run {
    prev: Position,
    entries: Vec<Entry>,
}

impl Run {
    // The position of the last entry. `None`, not `prev`, with no entries: the run
    // writes no entry at `prev`.
    pub(crate) fn last(&self) -> Option<Position> {
        self.entries.last().map(|entry| entry.at)
    }
}

// An index at most the last index of the log that made it. Only `Log::held` makes
// one, and a caller uses it before the log changes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Held(u64);

impl Held {
    pub(crate) fn index(self) -> u64 {
        self.0
    }
}

// Checks that `entries` follow `prev`.
pub(crate) fn check(prev: Position, entries: Vec<Entry>) -> Result<Run, Error> {
    let mut before = prev;
    for entry in &entries {
        let term = entry.at.term;
        if Some(entry.at.index) != before.index.checked_add(1)
            || term < before.term
            || term == Term(0)
        {
            return Err(Error::EntryOutOfOrder {
                at: entry.at,
                before,
            });
        }
        if voters_in(entry).is_some_and(|voters| voters.incoming.is_empty()) {
            return Err(Error::NoVoters);
        }
        before = entry.at;
    }
    Ok(Run { prev, entries })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn entry(term: u64, index: u64) -> Entry {
        Entry {
            at: Position {
                term: Term(term),
                index,
            },
            data: Data::Bytes(vec![u8::try_from(index).unwrap()]),
        }
    }

    fn position(term: u64, index: u64) -> Position {
        entry(term, index).at
    }

    fn log(terms: &[u64]) -> Log {
        let entries = terms
            .iter()
            .zip(1..)
            .map(|(&term, index)| entry(term, index))
            .collect();
        Log::new(Voters::default(), entries, 0).unwrap()
    }

    fn terms(log: &Log) -> Vec<u64> {
        log.entries.iter().map(|entry| entry.at.term.0).collect()
    }

    fn run(prev: Position, entries: Vec<Entry>) -> Run {
        check(prev, entries).unwrap()
    }

    fn out_of_order(at: &Entry, before: Position) -> Error {
        Error::EntryOutOfOrder { at: at.at, before }
    }

    #[test]
    fn an_empty_log_ends_at_the_zero_position() {
        let mut log = Log::new(Voters::default(), vec![], 0).unwrap();
        assert_eq!(log.last(), Position::default());
        assert_eq!(log.at(0), Some(Position::default()));
        assert_eq!(log.at(1), None);
        assert_eq!(log.take_unstable(), vec![]);
        assert_eq!(log.take_committed(), vec![]);
    }

    #[test]
    fn ends_at_its_last_entry() {
        let log = log(&[1, 1, 3]);
        assert_eq!(log.last(), position(3, 3));
        assert_eq!(log.at(2), Some(position(1, 2)));
        assert_eq!(log.at(4), None);
    }

    #[test]
    fn rejects_entries_that_do_not_start_at_one() {
        let err = Log::new(Voters::default(), vec![entry(1, 2)], 0).unwrap_err();
        assert_eq!(err, out_of_order(&entry(1, 2), Position::default()));
        assert_eq!(
            err.to_string(),
            "log entry at index 2 in term 1 does not follow index 0 in term 0"
        );
    }

    #[test]
    fn rejects_a_gap_and_a_term_that_goes_back() {
        let err =
            Log::new(Voters::default(), vec![entry(1, 1), entry(1, 3)], 0).unwrap_err();
        assert_eq!(err, out_of_order(&entry(1, 3), entry(1, 1).at));
        let err =
            Log::new(Voters::default(), vec![entry(2, 1), entry(1, 2)], 0).unwrap_err();
        assert_eq!(err, out_of_order(&entry(1, 2), entry(2, 1).at));
    }

    #[test]
    fn rejects_entries_that_do_not_follow_prev() {
        let err = check(position(1, 3), vec![entry(2, 2)]).unwrap_err();
        assert_eq!(err, out_of_order(&entry(2, 2), position(1, 3)));
    }

    #[test]
    fn rejects_an_entry_in_term_zero() {
        let err = Log::new(Voters::default(), vec![entry(0, 1)], 0).unwrap_err();
        assert_eq!(err, out_of_order(&entry(0, 1), Position::default()));
        assert_eq!(
            err.to_string(),
            "log entry at index 1 in term 0 does not follow index 0 in term 0"
        );
    }

    #[test]
    fn rejects_an_applied_index_past_the_log() {
        let err = Log::new(Voters::default(), vec![entry(1, 1)], 2).unwrap_err();
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
    fn the_entries_from_disk_are_stable_and_committed_up_to_applied() {
        let entries = vec![entry(1, 1), entry(1, 2), entry(3, 3)];
        let mut log = Log::new(Voters::default(), entries, 2).unwrap();
        assert_eq!(log.committed(), 2);
        assert_eq!(log.take_unstable(), vec![]);
        assert_eq!(log.take_committed(), vec![]);
        log.commit_to(3);
        assert_eq!(log.take_committed(), vec![entry(3, 3)]);
    }

    fn voters(id: u128) -> Voters {
        Voters {
            incoming: [node::Key::from_u128(id)].into_iter().collect(),
            ..Voters::default()
        }
    }

    // `voters` as a change that node 1 wrote with its own vote alone.
    fn change(voters: Voters) -> Data {
        Data::Voters(Change {
            voters,
            votes: Proof {
                grant: crate::Grant::Vote,
                candidate: node::Key::from_u128(1),
                voters: [(node::Key::from_u128(1), None)].into(),
            },
            signature: None,
        })
    }

    fn config(term: u64, index: u64, id: u128) -> Entry {
        Entry {
            at: position(term, index),
            data: change(voters(id)),
        }
    }

    #[test]
    fn holds_the_last_configuration_it_wrote() {
        let mut log = log(&[1]);
        assert_eq!(log.voters(), (Position::default(), &Voters::default()));
        log.push(Term(1), change(voters(1)));
        log.push(Term(1), change(voters(2)));
        log.push(Term(1), Data::Bytes(vec![9]));
        assert_eq!(log.voters(), (position(1, 3), &voters(2)));
        let log = Log::new(voters(9), vec![entry(1, 1), config(1, 2, 3)], 0).unwrap();
        assert_eq!(log.voters(), (position(1, 2), &voters(3)));
        let log = Log::new(voters(9), vec![entry(1, 1)], 0).unwrap();
        assert_eq!(log.voters(), (Position::default(), &voters(9)));
    }

    #[test]
    fn the_committed_configuration_is_the_last_one_at_or_below_committed() {
        let entries = [
            entry(1, 1),
            config(1, 2, 1),
            config(1, 3, 2),
            config(1, 4, 3),
        ];
        for (applied, id) in [(0, 9), (1, 9), (2, 1), (3, 2), (4, 3)] {
            let log = Log::new(voters(9), entries.to_vec(), applied).unwrap();
            assert_eq!(log.committed_voters(), voters(id), "committed {applied}");
        }
    }

    #[test]
    fn gives_the_last_configuration_before_an_index() {
        let entries = vec![config(1, 1, 1), entry(1, 2), config(1, 3, 2), entry(1, 4)];
        let log = Log::new(voters(9), entries, 0).unwrap();
        let before: Vec<Voters> = (0..=6).map(|i| log.voters_before(i)).collect();
        assert_eq!(before, [9, 9, 1, 1, 2, 2, 2].map(voters));
    }

    // The empty set a node with no voters starts with is no configuration.
    #[test]
    fn with_no_voters_the_first_configuration_shows_the_one_before_the_entries() {
        let set =
            |ids: &[u128]| ids.iter().map(|&id| node::Key::from_u128(id)).collect();
        let joint = Voters {
            incoming: set(&[1, 2]),
            outgoing: set(&[1]),
        };
        let leave = joint.leave();
        let cases = [
            (Data::Empty, Voters::default()),
            (change(joint), voters(1)),
            (change(leave.clone()), leave),
        ];
        for (data, before) in cases {
            let at = position(1, 2);
            let entries = vec![entry(1, 1), Entry { at, data }, entry(1, 3)];
            let log = Log::new(Voters::default(), entries, 1).unwrap();
            assert_eq!(log.committed_voters(), before);
            assert_eq!(log.voters_before(2), before);
        }
    }

    #[test]
    fn with_no_voters_a_truncated_configuration_shows_nothing() {
        let config = config(1, 2, 1);
        let mut log =
            Log::new(Voters::default(), vec![entry(1, 1), config], 0).unwrap();
        assert_eq!(log.committed_voters(), voters(1));
        assert_eq!(log.append(run(position(1, 1), vec![entry(2, 2)])), Ok(2));
        assert_eq!(log.committed_voters(), Voters::default());
    }

    #[test]
    fn the_nodes_are_the_voters_in_force_and_until_committed_the_ones_before() {
        let keys = |ids: &[u128]| -> BTreeSet<node::Key> {
            ids.iter().map(|&id| node::Key::from_u128(id)).collect()
        };
        let log = Log::new(voters(1), vec![], 0).unwrap();
        assert_eq!(log.nodes(), keys(&[1]));
        let entries = vec![config(1, 1, 2), entry(1, 2), config(1, 3, 3)];
        let log = Log::new(voters(1), entries, 0).unwrap();
        assert_eq!(log.nodes(), keys(&[2, 3]));
        let log = Log::new(voters(1), vec![config(1, 1, 2)], 0).unwrap();
        assert_eq!(log.nodes(), keys(&[1, 2]));
        let log = Log::new(voters(1), vec![config(1, 1, 2)], 1).unwrap();
        assert_eq!(log.nodes(), keys(&[2]));
    }

    #[test]
    fn an_append_puts_in_force_the_last_configuration_it_leaves_in_the_log() {
        let mut log = Log::new(
            Voters::default(),
            vec![entry(1, 1), config(1, 2, 1), config(1, 3, 2)],
            0,
        )
        .unwrap();
        assert_eq!(log.append(run(position(1, 3), vec![entry(2, 4)])), Ok(4));
        assert_eq!(log.voters(), (position(1, 3), &voters(2)));
        assert_eq!(log.append(run(position(1, 2), vec![entry(3, 3)])), Ok(3));
        assert_eq!(log.voters(), (position(1, 2), &voters(1)));
        assert_eq!(
            log.append(run(position(3, 3), vec![config(3, 4, 4)])),
            Ok(4)
        );
        assert_eq!(log.voters(), (position(3, 4), &voters(4)));
        assert_eq!(log.append(run(position(1, 1), vec![entry(4, 2)])), Ok(2));
        assert_eq!(log.voters(), (Position::default(), &Voters::default()));
    }

    #[test]
    fn gives_a_pushed_entry_once_to_write() {
        let mut log = log(&[1]);
        assert_eq!(log.push(Term(2), Data::Bytes(vec![9])), position(2, 2));
        let pushed = Entry {
            at: position(2, 2),
            data: Data::Bytes(vec![9]),
        };
        assert_eq!(log.take_unstable(), vec![pushed]);
        assert_eq!(log.take_unstable(), vec![]);
    }

    #[test]
    fn commits_in_order_and_never_backwards() {
        let mut log = log(&[1, 1, 2]);
        log.commit_to(3);
        assert_eq!(log.committed(), 3);
        log.commit_to(1);
        assert_eq!(log.committed(), 3);
        assert_eq!(
            log.take_committed(),
            vec![entry(1, 1), entry(1, 2), entry(2, 3)]
        );
        assert_eq!(log.take_committed(), vec![]);
    }

    #[test]
    #[should_panic(expected = "invariant: commit index 4 is past the last log index 3")]
    fn refuses_to_commit_past_the_end() {
        log(&[1, 1, 2]).commit_to(4);
    }

    #[test]
    fn appends_after_a_matching_prev() {
        let mut log = log(&[1, 2]);
        log.take_unstable();
        let result = log.append(run(position(2, 2), vec![entry(3, 3)]));
        assert_eq!(result, Ok(3));
        assert_eq!(terms(&log), [1, 2, 3]);
        assert_eq!(log.take_unstable(), vec![entry(3, 3)]);
    }

    #[test]
    fn replaces_the_first_entry_that_differs_and_all_after_it() {
        let mut log = log(&[1, 2, 2, 2]);
        log.take_unstable();
        let result = log.append(run(position(1, 1), vec![entry(3, 2), entry(4, 3)]));
        assert_eq!(result, Ok(3));
        assert_eq!(terms(&log), [1, 3, 4]);
        assert_eq!(log.take_unstable(), vec![entry(3, 2), entry(4, 3)]);
    }

    #[test]
    fn keeps_entries_it_already_has() {
        let mut log = log(&[1, 2]);
        log.take_unstable();
        let result = log.append(run(Position::default(), vec![entry(1, 1)]));
        assert_eq!(result, Ok(1));
        assert_eq!(terms(&log), [1, 2]);
        assert_eq!(log.take_unstable(), vec![]);
    }

    #[test]
    fn a_probe_with_no_entries_matches_or_not() {
        let mut log = log(&[1, 2]);
        assert_eq!(log.append(run(position(2, 2), vec![])), Ok(2));
        assert_eq!(log.append(run(position(1, 2), vec![])), Err(1));
    }

    #[test]
    fn hints_its_last_index_for_a_prev_it_does_not_have() {
        let mut log = log(&[1, 2]);
        assert_eq!(log.append(run(position(3, 5), vec![entry(3, 6)])), Err(2));
        assert_eq!(terms(&log), [1, 2]);
    }

    #[test]
    fn hints_the_index_before_a_prev_that_differs() {
        let mut log = log(&[1, 2, 2]);
        assert_eq!(log.append(run(position(1, 3), vec![])), Err(2));
    }

    #[test]
    fn answers_an_append_below_its_commit_index_with_that_index() {
        let mut log = log(&[1, 1, 2]);
        log.commit_to(2);
        assert_eq!(log.append(run(position(1, 1), vec![entry(2, 2)])), Ok(2));
        assert_eq!(log.append(run(Position::default(), vec![])), Ok(2));
        assert_eq!(terms(&log), [1, 1, 2]);
    }

    #[test]
    fn gives_the_first_index_of_a_term_or_of_a_later_one() {
        let log = log(&[1, 1, 3, 3]);
        let first: Vec<u64> = (0..=4).map(|term| log.first_of(Term(term))).collect();
        assert_eq!(first, vec![1, 1, 3, 3, 5]);
    }

    #[test]
    fn gives_entries_from_an_index_through_an_end_up_to_a_limit() {
        let log = log(&[1, 1, 2, 2]);
        assert_eq!(log.slice(2, 4, 2), vec![entry(1, 2), entry(2, 3)]);
        assert_eq!(log.slice(4, u64::MAX, 10), vec![entry(2, 4)]);
        assert_eq!(log.slice(5, u64::MAX, 10), vec![]);
        assert_eq!(log.slice(2, 3, 10), vec![entry(1, 2), entry(2, 3)]);
        assert_eq!(log.slice(3, 2, 10), vec![]);
        assert_eq!(log.slice(4, 2, 10), vec![]);
    }

    // Entries from index `from`: each `(rise, config)` raises the term by `rise`
    // and is a configuration entry when `config`.
    fn entries_from(from: u64, term: u64, kinds: &[(u64, bool)]) -> (Vec<Entry>, u64) {
        let mut term = term;
        let entries = kinds
            .iter()
            .zip(from..)
            .map(|(&(rise, config), index)| {
                term += rise;
                let data = if config {
                    change(voters(u128::from(index)))
                } else {
                    Data::Bytes(vec![])
                };
                Entry {
                    at: position(term, index),
                    data,
                }
            })
            .collect();
        (entries, term)
    }

    // The links of `entries` below `term`, read with no index.
    fn links_in(entries: &[Entry], term: Term) -> Vec<Link> {
        entries
            .iter()
            .filter(|entry| entry.at.term < term)
            .filter_map(|entry| match &entry.data {
                Data::Voters(change) => Some(Link {
                    at: entry.at,
                    change: change.clone(),
                }),
                Data::Empty | Data::Bytes(_) => None,
            })
            .collect()
    }

    fn kinds() -> impl Strategy<Value = Vec<(u64, bool)>> {
        prop::collection::vec((0..3u64, any::<bool>()), 0..12)
    }

    proptest! {
        // The index of configuration entries matches the entries after a start, an
        // append that keeps, truncates, or extends the log, and a push.
        #[test]
        fn the_configuration_index_follows_the_entries(
            start in kinds(),
            cut in 0..12usize,
            appended in kinds(),
            pushed in any::<bool>(),
            below in 0..6u64,
        ) {
            let (mut entries, _) = entries_from(1, 1, &start);
            let mut log = Log::new(voters(9), entries.clone(), 0).unwrap();
            let cut = cut.min(entries.len());
            let prev = entries.get(cut.wrapping_sub(1)).map_or_else(
                Position::default,
                |entry| entry.at,
            );
            let base = prev.term.0.max(1);
            let (new, _) = entries_from(prev.index + 1, base, &appended);
            log.append(run(prev, new.clone())).unwrap();
            let same = new.iter().zip(&entries[cut..]);
            let same = same.take_while(|(a, b)| a.at == b.at).count();
            // A run that the log holds in full changes nothing past it.
            if same < new.len() {
                entries.truncate(cut + same);
                entries.extend(new.into_iter().skip(same));
            }
            if pushed {
                let at = log.push(log.last().term.max(Term(1)), change(voters(0)));
                entries.push(Entry { at, data: change(voters(0)) });
            }
            let fresh = Log::new(voters(9), entries.clone(), 0).unwrap();
            prop_assert_eq!(log.links(Term(below)), links_in(&entries, Term(below)));
            prop_assert_eq!(log.voters(), fresh.voters());
            for index in 0..=u64::try_from(entries.len()).unwrap() {
                prop_assert_eq!(log.voters_through(index), fresh.voters_through(index));
            }
        }
    }
}
