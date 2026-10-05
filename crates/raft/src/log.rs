use crate::{Error, Position};

/// One entry of the replicated log. A leader appends an entry with no data when its
/// term starts; the caller applies nothing for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Where the entry is in the log.
    pub at: Position,
    /// What the caller proposed.
    pub data: Vec<u8>,
}

// The log in memory. Entry `i` has index `i + 1`, and terms never decrease.
#[derive(Debug)]
pub(crate) struct Log {
    entries: Vec<Entry>,
}

impl Log {
    pub(crate) fn new(entries: Vec<Entry>, applied: u64) -> Result<Self, Error> {
        let mut before = Position::default();
        for entry in &entries {
            if entry.at.index != before.index + 1 || entry.at.term < before.term {
                return Err(Error::EntryOutOfOrder {
                    at: entry.at,
                    before,
                });
            }
            before = entry.at;
        }
        if applied > before.index {
            return Err(Error::AppliedPastLog {
                applied,
                last: before.index,
            });
        }
        Ok(Self { entries })
    }

    pub(crate) fn last(&self) -> Position {
        self.entries
            .last()
            .map_or_else(Position::default, |entry| entry.at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Term;

    fn entry(term: u64, index: u64) -> Entry {
        Entry {
            at: Position {
                term: Term(term),
                index,
            },
            data: vec![u8::try_from(index).unwrap()],
        }
    }

    #[test]
    fn an_empty_log_ends_at_the_zero_position() {
        let log = Log::new(vec![], 0).unwrap();
        assert_eq!(log.last(), Position::default());
    }

    #[test]
    fn ends_at_its_last_entry() {
        let entries = vec![entry(1, 1), entry(1, 2), entry(3, 3)];
        let log = Log::new(entries, 3).unwrap();
        assert_eq!(
            log.last(),
            Position {
                term: Term(3),
                index: 3
            }
        );
    }

    fn out_of_order(at: &Entry, before: Position) -> Error {
        Error::EntryOutOfOrder { at: at.at, before }
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
}
