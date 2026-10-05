use std::collections::VecDeque;

// Entries in one `Append`.
pub(crate) const BATCH: usize = 64;
// `Append` messages a leader keeps in flight to one follower before it waits.
pub(crate) const INFLIGHT: usize = 8;

// What a leader knows about one follower's log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Progress {
    // The index of the next entry to send.
    pub(crate) next: u64,
    // The last index the follower is known to hold.
    pub(crate) matched: u64,
    state: State,
}

// A leader probes a follower whose log it does not know with one `Append` at a
// time, then replicates with several in flight once one matched.
#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Probe { waiting: bool },
    Replicate { inflight: VecDeque<u64> },
}

impl Progress {
    pub(crate) fn new(last: u64) -> Self {
        Self {
            next: last + 1,
            matched: 0,
            state: State::Probe { waiting: false },
        }
    }

    // Whether the leader waits for a reply before it sends more.
    pub(crate) fn paused(&self) -> bool {
        match &self.state {
            State::Probe { waiting } => *waiting,
            State::Replicate { inflight } => inflight.len() >= INFLIGHT,
        }
    }

    // Records an `Append` sent for entries up to `last`, or a probe when `last` is
    // the index before `next`.
    pub(crate) fn sent(&mut self, last: u64) {
        match &mut self.state {
            State::Probe { waiting } => *waiting = true,
            State::Replicate { inflight } => {
                if last >= self.next {
                    inflight.push_back(last);
                    self.next = last + 1;
                }
            }
        }
    }

    // Records that the follower holds every entry up to `index`. Returns whether
    // that is news.
    pub(crate) fn accepted(&mut self, index: u64) -> bool {
        let news = index > self.matched;
        if news {
            self.matched = index;
        }
        match &mut self.state {
            State::Probe { .. } => {
                self.state = State::Replicate {
                    inflight: VecDeque::new(),
                };
                self.next = self.matched + 1;
            }
            State::Replicate { inflight } => {
                while inflight.front().is_some_and(|&last| last <= index) {
                    inflight.pop_front();
                }
                self.next = self.next.max(index + 1);
            }
        }
        news
    }

    // Records that the follower did not have `prev`. `hint` is the follower's guess
    // at the last index the two logs share.
    pub(crate) fn rejected(&mut self, hint: u64) {
        self.next = (hint + 1).min(self.next.saturating_sub(1)).max(1);
        self.state = State::Probe { waiting: false };
    }

    // Lets the leader send again after a heartbeat reply: a probe answers, and a
    // full window frees one slot so that lost messages do not stop replication.
    pub(crate) fn heard(&mut self) {
        match &mut self.state {
            State::Probe { waiting } => *waiting = false,
            State::Replicate { inflight } => {
                if inflight.len() >= INFLIGHT {
                    inflight.pop_front();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_as_a_probe_after_the_leaders_last_entry() {
        let progress = Progress::new(5);
        assert_eq!((progress.next, progress.matched), (6, 0));
        assert!(!progress.paused());
    }

    #[test]
    fn a_probe_waits_for_its_reply() {
        let mut progress = Progress::new(5);
        progress.sent(5);
        assert!(progress.paused());
        progress.heard();
        assert!(!progress.paused());
    }

    #[test]
    fn an_accepted_probe_starts_replication_from_the_match() {
        let mut progress = Progress::new(5);
        progress.sent(5);
        assert!(progress.accepted(3));
        assert_eq!((progress.next, progress.matched), (4, 3));
        assert!(!progress.paused());
        assert!(!progress.accepted(3));
    }

    #[test]
    fn a_rejection_moves_next_back_to_the_hint() {
        let mut progress = Progress::new(10);
        progress.sent(10);
        progress.rejected(4);
        assert_eq!(progress.next, 5);
        assert!(!progress.paused());
        progress.rejected(9);
        assert_eq!(progress.next, 4);
        progress.rejected(0);
        assert_eq!(progress.next, 1);
        progress.rejected(0);
        assert_eq!(progress.next, 1);
    }

    #[test]
    fn replication_sends_ahead_until_the_window_is_full() {
        let mut progress = Progress::new(0);
        progress.sent(0);
        progress.accepted(0);
        for last in 1..=INFLIGHT as u64 {
            assert!(!progress.paused());
            progress.sent(last);
            assert_eq!(progress.next, last + 1);
        }
        assert!(progress.paused());
        progress.accepted(2);
        assert!(!progress.paused());
        assert_eq!((progress.next, progress.matched), (INFLIGHT as u64 + 1, 2));
    }

    #[test]
    fn a_heartbeat_reply_frees_one_slot_of_a_full_window() {
        let mut progress = Progress::new(0);
        progress.sent(0);
        progress.accepted(0);
        for last in 1..=INFLIGHT as u64 {
            progress.sent(last);
        }
        progress.heard();
        assert!(!progress.paused());
        progress.sent(INFLIGHT as u64 + 1);
        assert!(progress.paused());
    }

    #[test]
    fn a_probe_with_no_entries_does_not_move_next() {
        let mut progress = Progress::new(0);
        progress.sent(0);
        progress.accepted(0);
        progress.sent(0);
        assert_eq!(progress.next, 1);
        assert!(!progress.paused());
    }
}
