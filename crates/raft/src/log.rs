use crate::{Error, Position, Term};

/// One entry of the replicated log. A leader appends an entry with no data when its
/// term starts; the caller applies nothing for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Where the entry is in the log.
    pub at: Position,
    /// What the caller proposed.
    pub data: Vec<u8>,
}

// The log in memory. Entry `i` has index `i + 1`, and terms start above zero and
// never decrease. Three indexes trail the end: `committed` is what a quorum holds,
// `applied` is the last entry given to the caller to apply, and `stable` is the
// last entry given to the caller to write.
#[derive(Debug)]
pub(crate) struct Log {
    entries: Vec<Entry>,
    committed: u64,
    applied: u64,
    stable: u64,
}

impl Log {
    pub(crate) fn new(entries: Vec<Entry>, applied: u64) -> Result<Self, Error> {
        let before = check(&entries, Position::default())?;
        if applied > before.index {
            return Err(Error::AppliedPastLog {
                applied,
                last: before.index,
            });
        }
        Ok(Self {
            entries,
            committed: applied,
            applied,
            stable: before.index,
        })
    }

    pub(crate) fn last(&self) -> Position {
        self.entries
            .last()
            .map_or_else(Position::default, |entry| entry.at)
    }

    pub(crate) fn committed(&self) -> u64 {
        self.committed
    }

    // The position at `index`: the zero position for 0, `None` past the end.
    pub(crate) fn at(&self, index: u64) -> Option<Position> {
        if index == 0 {
            return Some(Position::default());
        }
        let at = usize::try_from(index - 1).ok()?;
        self.entries.get(at).map(|entry| entry.at)
    }

    // Up to `max` entries from `index`, cloned for a message.
    pub(crate) fn slice(&self, index: u64, max: usize) -> Vec<Entry> {
        let from = usize::try_from(index.saturating_sub(1)).unwrap_or(usize::MAX);
        self.entries
            .get(from..)
            .unwrap_or_default()
            .iter()
            .take(max)
            .cloned()
            .collect()
    }

    // Appends one entry at the end as the leader.
    pub(crate) fn push(&mut self, term: Term, data: Vec<u8>) -> Position {
        let at = Position {
            term,
            index: self.last().index + 1,
        };
        self.entries.push(Entry { at, data });
        at
    }

    // Appends the leader's entries after `prev` as a follower. The caller checked
    // that they follow `prev`. Entries already in the log stay; the first entry that
    // differs replaces it and all after it. Returns the index of the last entry the
    // leader sent, or the commit index when `prev` is below it, or an error with the
    // follower's hint for the next `prev`: its last index, or the index before a
    // `prev` it does not have.
    pub(crate) fn append(
        &mut self,
        prev: Position,
        entries: Vec<Entry>,
    ) -> Result<u64, u64> {
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
        self.entries
            .truncate(usize::try_from(from - 1).unwrap_or(usize::MAX));
        self.stable = self.stable.min(from - 1);
        self.entries.extend(entries.into_iter().skip(first));
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
        let entries = self.slice(self.stable + 1, usize::MAX);
        self.stable = self.last().index;
        entries
    }

    // The committed entries no `Ready` has given to apply yet.
    pub(crate) fn take_committed(&mut self) -> Vec<Entry> {
        let count =
            usize::try_from(self.committed - self.applied).unwrap_or(usize::MAX);
        let entries = self.slice(self.applied + 1, count);
        self.applied = self.committed;
        entries
    }
}

// Checks that `entries` follow `before`: indexes in sequence, terms non-decreasing
// and not zero. Returns the last position, or `before` with no entries.
pub(crate) fn check(
    entries: &[Entry],
    mut before: Position,
) -> Result<Position, Error> {
    for entry in entries {
        let term = entry.at.term;
        if entry.at.index != before.index + 1 || term < before.term || term == Term(0) {
            return Err(Error::EntryOutOfOrder {
                at: entry.at,
                before,
            });
        }
        before = entry.at;
    }
    Ok(before)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(term: u64, index: u64) -> Entry {
        Entry {
            at: Position {
                term: Term(term),
                index,
            },
            data: vec![u8::try_from(index).unwrap()],
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
        Log::new(entries, 0).unwrap()
    }

    fn terms(log: &Log) -> Vec<u64> {
        log.entries.iter().map(|entry| entry.at.term.0).collect()
    }

    fn out_of_order(at: &Entry, before: Position) -> Error {
        Error::EntryOutOfOrder { at: at.at, before }
    }

    #[test]
    fn an_empty_log_ends_at_the_zero_position() {
        let mut log = Log::new(vec![], 0).unwrap();
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
        let err = Log::new(vec![entry(1, 2)], 0).unwrap_err();
        assert_eq!(err, out_of_order(&entry(1, 2), Position::default()));
        assert_eq!(
            err.to_string(),
            "log entry at index 2 in term 1 does not follow index 0 in term 0"
        );
    }

    #[test]
    fn rejects_a_gap_and_a_term_that_goes_back() {
        let err = Log::new(vec![entry(1, 1), entry(1, 3)], 0).unwrap_err();
        assert_eq!(err, out_of_order(&entry(1, 3), entry(1, 1).at));
        let err = Log::new(vec![entry(2, 1), entry(1, 2)], 0).unwrap_err();
        assert_eq!(err, out_of_order(&entry(1, 2), entry(2, 1).at));
    }

    #[test]
    fn rejects_an_entry_in_term_zero() {
        let err = Log::new(vec![entry(0, 1)], 0).unwrap_err();
        assert_eq!(err, out_of_order(&entry(0, 1), Position::default()));
        assert_eq!(
            err.to_string(),
            "log entry at index 1 in term 0 does not follow index 0 in term 0"
        );
    }

    #[test]
    fn rejects_an_applied_index_past_the_log() {
        let err = Log::new(vec![entry(1, 1)], 2).unwrap_err();
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
        let mut log = Log::new(entries, 2).unwrap();
        assert_eq!(log.committed(), 2);
        assert_eq!(log.take_unstable(), vec![]);
        assert_eq!(log.take_committed(), vec![]);
        log.commit_to(3);
        assert_eq!(log.take_committed(), vec![entry(3, 3)]);
    }

    #[test]
    fn gives_a_pushed_entry_once_to_write() {
        let mut log = log(&[1]);
        assert_eq!(log.push(Term(2), vec![9]), position(2, 2));
        let pushed = Entry {
            at: position(2, 2),
            data: vec![9],
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
        let result = log.append(position(2, 2), vec![entry(3, 3)]);
        assert_eq!(result, Ok(3));
        assert_eq!(terms(&log), [1, 2, 3]);
        assert_eq!(log.take_unstable(), vec![entry(3, 3)]);
    }

    #[test]
    fn replaces_the_first_entry_that_differs_and_all_after_it() {
        let mut log = log(&[1, 2, 2, 2]);
        log.take_unstable();
        let result = log.append(position(1, 1), vec![entry(3, 2), entry(4, 3)]);
        assert_eq!(result, Ok(3));
        assert_eq!(terms(&log), [1, 3, 4]);
        assert_eq!(log.take_unstable(), vec![entry(3, 2), entry(4, 3)]);
    }

    #[test]
    fn keeps_entries_it_already_has() {
        let mut log = log(&[1, 2]);
        log.take_unstable();
        let result = log.append(Position::default(), vec![entry(1, 1)]);
        assert_eq!(result, Ok(1));
        assert_eq!(terms(&log), [1, 2]);
        assert_eq!(log.take_unstable(), vec![]);
    }

    #[test]
    fn a_probe_with_no_entries_matches_or_not() {
        let mut log = log(&[1, 2]);
        assert_eq!(log.append(position(2, 2), vec![]), Ok(2));
        assert_eq!(log.append(position(1, 2), vec![]), Err(1));
    }

    #[test]
    fn hints_its_last_index_for_a_prev_it_does_not_have() {
        let mut log = log(&[1, 2]);
        assert_eq!(log.append(position(3, 5), vec![entry(3, 6)]), Err(2));
        assert_eq!(terms(&log), [1, 2]);
    }

    #[test]
    fn hints_the_index_before_a_prev_that_differs() {
        let mut log = log(&[1, 2, 2]);
        assert_eq!(log.append(position(1, 3), vec![]), Err(2));
    }

    #[test]
    fn answers_an_append_below_its_commit_index_with_that_index() {
        let mut log = log(&[1, 1, 2]);
        log.commit_to(2);
        assert_eq!(log.append(position(1, 1), vec![entry(2, 2)]), Ok(2));
        assert_eq!(log.append(Position::default(), vec![]), Ok(2));
        assert_eq!(terms(&log), [1, 1, 2]);
    }

    #[test]
    fn gives_entries_from_an_index_up_to_a_limit() {
        let log = log(&[1, 1, 2, 2]);
        assert_eq!(log.slice(2, 2), vec![entry(1, 2), entry(2, 3)]);
        assert_eq!(log.slice(4, 10), vec![entry(2, 4)]);
        assert_eq!(log.slice(5, 10), vec![]);
    }
}
