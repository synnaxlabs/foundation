use std::collections::VecDeque;

// What a leader knows about one follower's log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Progress {
    next: u64,
    matched: u64,
    state: State,
    // Whether the window was full at the last quorum check, with no append since.
    aged: bool,
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
            aged: false,
        }
    }

    // The index of the next entry to send.
    pub(crate) fn next(&self) -> u64 {
        self.next
    }

    // The last index the follower is known to hold.
    pub(crate) fn matched(&self) -> u64 {
        self.matched
    }

    // Whether the follower lacks entries of a log that ends at `last`.
    pub(crate) fn behind(&self, last: u64) -> bool {
        self.next <= last
    }

    // Whether the leader waits for a reply before it sends more, with `window`
    // appends in flight at most.
    pub(crate) fn paused(&self, window: usize) -> bool {
        match &self.state {
            State::Probe { waiting } => *waiting,
            State::Replicate { inflight } => inflight.len() >= window,
        }
    }

    // Records an `Append` sent for entries up to `last`, or with no entry when
    // `last` is the index before `next`. Each takes a slot of the window, so a
    // follower that answers nothing pauses also when its appends carry no entry.
    pub(crate) fn sent(&mut self, last: u64) {
        match &mut self.state {
            State::Probe { waiting } => {
                *waiting = true;
                self.aged = false;
            }
            State::Replicate { inflight } => {
                inflight.push_back(last);
                self.next = last + 1;
                self.aged = false;
            }
        }
    }

    // Records a quorum check, with `window` appends in flight at most, and whether
    // the follower answered since the check before. A window that was full at the
    // check before, with no append sent since, holds appends that the follower did not
    // answer through a whole check period in which it answered something else: they
    // count as lost. A probe then waits no more, and replication probes again from
    // the match. Returns whether the window freed.
    pub(crate) fn checked(&mut self, window: usize, heard: bool) -> bool {
        let paused = self.paused(window);
        let aged = std::mem::replace(&mut self.aged, paused);
        if !(aged && paused && heard) {
            return false;
        }
        self.aged = false;
        match &mut self.state {
            State::Probe { waiting } => *waiting = false,
            State::Replicate { .. } => {
                self.next = self.matched + 1;
                self.state = State::Probe { waiting: false };
            }
        }
        true
    }

    // Records that the follower holds every entry up to `index`, which is at most the
    // leader's last index. Returns whether that is news.
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
    // at the last index the two logs share. A hint below `matched` is stale.
    pub(crate) fn rejected(&mut self, hint: u64) {
        if hint < self.matched {
            return;
        }
        self.next = (hint + 1).min(self.next.saturating_sub(1)).max(1);
        self.state = State::Probe { waiting: false };
    }

    // Whether the leader probes for the follower's match, with at most one append
    // in flight.
    pub(crate) fn probing(&self) -> bool {
        matches!(self.state, State::Probe { .. })
    }

    // Lets the leader send again after a heartbeat reply: a probe answers, and a
    // window of `window` that is full frees one slot so that lost messages do not stop
    // replication.
    pub(crate) fn heard(&mut self, window: usize) {
        match &mut self.state {
            State::Probe { waiting } => *waiting = false,
            State::Replicate { inflight } => {
                if inflight.len() >= window {
                    inflight.pop_front();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFLIGHT: usize = 8;

    #[test]
    fn starts_as_a_probe_after_the_leaders_last_entry() {
        let progress = Progress::new(5);
        assert_eq!((progress.next, progress.matched), (6, 0));
        assert!(!progress.paused(INFLIGHT));
    }

    #[test]
    fn a_probe_waits_for_its_reply() {
        let mut progress = Progress::new(5);
        progress.sent(5);
        assert!(progress.paused(INFLIGHT));
        progress.heard(INFLIGHT);
        assert!(!progress.paused(INFLIGHT));
    }

    #[test]
    fn an_accepted_probe_starts_replication_from_the_match() {
        let mut progress = Progress::new(5);
        progress.sent(5);
        assert!(progress.accepted(3));
        assert_eq!((progress.next, progress.matched), (4, 3));
        assert!(!progress.paused(INFLIGHT));
        assert!(!progress.accepted(3));
    }

    #[test]
    fn a_rejection_moves_next_back_to_the_hint() {
        let mut progress = Progress::new(10);
        progress.sent(10);
        progress.rejected(4);
        assert_eq!(progress.next, 5);
        assert!(!progress.paused(INFLIGHT));
        progress.rejected(9);
        assert_eq!(progress.next, 4);
        progress.rejected(0);
        assert_eq!(progress.next, 1);
        progress.rejected(0);
        assert_eq!(progress.next, 1);
    }

    #[test]
    fn ignores_a_stale_rejection_below_the_match() {
        let mut progress = Progress::new(0);
        progress.sent(0);
        progress.accepted(3);
        progress.sent(5);
        progress.rejected(2);
        assert_eq!((progress.next, progress.matched), (6, 3));
        assert!(!progress.paused(INFLIGHT));
    }

    #[test]
    fn replication_sends_ahead_until_the_window_is_full() {
        let mut progress = Progress::new(0);
        progress.sent(0);
        progress.accepted(0);
        for last in 1..=INFLIGHT as u64 {
            assert!(!progress.paused(INFLIGHT));
            progress.sent(last);
            assert_eq!(progress.next, last + 1);
        }
        assert!(progress.paused(INFLIGHT));
        progress.accepted(2);
        assert!(!progress.paused(INFLIGHT));
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
        progress.heard(INFLIGHT);
        assert!(!progress.paused(INFLIGHT));
        progress.sent(INFLIGHT as u64 + 1);
        assert!(progress.paused(INFLIGHT));
    }

    #[test]
    fn a_window_full_through_a_check_period_frees_at_the_check() {
        let mut progress = Progress::new(0);
        progress.sent(0);
        progress.accepted(0);
        for last in 1..=INFLIGHT as u64 {
            progress.sent(last);
        }
        assert!(!progress.checked(1, true));
        assert!(progress.paused(1));
        assert!(progress.checked(1, true));
        assert!(!progress.paused(1));
        assert!(progress.probing());
        assert_eq!((progress.next(), progress.matched()), (1, 0));
    }

    #[test]
    fn an_append_after_a_check_starts_its_period_again() {
        let mut progress = Progress::new(0);
        progress.sent(0);
        progress.accepted(0);
        progress.sent(1);
        assert!(!progress.checked(1, true));
        progress.accepted(1);
        progress.sent(2);
        assert!(!progress.checked(1, true));
        assert!(progress.paused(1));
    }

    #[test]
    fn a_probe_waiting_through_a_check_period_frees_at_the_check() {
        let mut progress = Progress::new(5);
        progress.sent(5);
        assert!(!progress.checked(1, true));
        assert!(progress.checked(1, true));
        assert!(!progress.paused(1));
    }

    #[test]
    fn a_window_of_a_silent_follower_stays_full() {
        let mut progress = Progress::new(0);
        progress.sent(0);
        progress.accepted(0);
        progress.sent(1);
        assert!(!progress.checked(1, true));
        assert!(!progress.checked(1, false));
        assert!(!progress.checked(1, false));
        assert!(progress.paused(1));
        assert!(progress.checked(1, true));
    }

    #[test]
    fn a_window_with_room_at_the_check_frees_nothing() {
        let mut progress = Progress::new(0);
        progress.sent(0);
        progress.accepted(0);
        progress.sent(1);
        assert!(!progress.checked(INFLIGHT, true));
        assert!(!progress.checked(INFLIGHT, true));
        assert!(progress.paused(1));
    }

    #[test]
    fn a_follower_that_holds_more_than_it_was_sent_moves_next_past_it() {
        let mut progress = Progress::new(0);
        progress.sent(0);
        progress.accepted(0);
        progress.sent(2);
        assert!(progress.accepted(5));
        assert_eq!((progress.next, progress.matched), (6, 5));
    }

    #[test]
    fn appends_with_no_entry_fill_the_window() {
        let mut progress = Progress::new(0);
        progress.sent(0);
        progress.accepted(0);
        for _ in 0..INFLIGHT {
            assert!(!progress.paused());
            progress.sent(0);
        }
        assert!(progress.paused());
        assert_eq!(progress.next(), 1);
        progress.accepted(0);
        assert!(!progress.paused());
    }

    #[test]
    fn a_probe_with_no_entries_does_not_move_next() {
        let mut progress = Progress::new(0);
        progress.sent(0);
        progress.accepted(0);
        progress.sent(0);
        assert_eq!(progress.next, 1);
        assert!(!progress.paused(INFLIGHT));
    }
}
