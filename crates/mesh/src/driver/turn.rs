//! When a member that is not a voter dials a voter, and which voter. A leader that
//! cannot dial the member reaches it on a session that the member dialed.

use std::ops::{Bound, RangeBounds};

use raft::Voters;
use types::node;

use super::ELECTION_TICKS;

/// The ticks after which a member that is not a voter, with no message from the
/// leader, dials a voter. A leader sends it a heartbeat at each quorum check, which
/// comes each `ELECTION_TICKS`.
const QUIET_TICKS: u32 = 2 * ELECTION_TICKS;

/// The dials of a member that is not a voter.
#[derive(Debug)]
pub(crate) struct Turn {
    own: node::Key,
    // The ticks since the leader last reached this node.
    quiet: u32,
    // The first voter that the next dial can name: the leader that reached this node
    // last, included, or the voter that this node dialed last, excluded.
    from: Bound<node::Key>,
}

impl Turn {
    /// The dials of the member `own`.
    pub(crate) fn new(own: node::Key) -> Self {
        Self {
            own,
            quiet: 0,
            from: Bound::Unbounded,
        }
    }

    /// Notes a message from `from` that `raft` took while `leader` leads.
    pub(crate) fn heard(&mut self, leader: Option<node::Key>, from: node::Key) {
        if leader == Some(from) {
            self.quiet = 0;
            self.from = Bound::Included(from);
        }
    }

    /// Counts a tick. Gives the voter to dial when the member is not in `voters`
    /// and no leader reached it for `QUIET_TICKS`, in key order: first the leader
    /// that reached it last, then each voter after the one that it dialed, and
    /// after the last voter the first.
    pub(crate) fn tick(&mut self, voters: &Voters) -> Option<node::Key> {
        if voters.contains(self.own) {
            return None;
        }
        self.quiet = self.quiet.saturating_add(1);
        if self.quiet < QUIET_TICKS {
            return None;
        }
        self.quiet = 0;
        let from = (self.from, Bound::Unbounded);
        let next = voters.nodes().find(|key| from.contains(key));
        let next = next.or_else(|| voters.nodes().next())?;
        self.from = Bound::Excluded(next);
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::common::key;

    fn voters(ids: &[u8]) -> Voters {
        Voters {
            incoming: ids.iter().copied().map(key).collect(),
            outgoing: BTreeSet::new(),
        }
    }

    // The tick of each dial in `ticks`, with the voter that it names.
    fn dials(turn: &mut Turn, voters: &Voters, ticks: u32) -> Vec<(u32, u8)> {
        let named = (1..=ticks).filter_map(|tick| Some((tick, turn.tick(voters)?)));
        let id = |to| (1..=u8::MAX).find(|&id| key(id) == to).unwrap();
        named.map(|(tick, to)| (tick, id(to))).collect()
    }

    #[test]
    fn with_no_leader_it_names_each_voter_in_turn_each_two_election_timeouts() {
        let quiet = 2 * ELECTION_TICKS;
        let mut turn = Turn::new(key(4));
        let named = dials(&mut turn, &voters(&[1, 2, 3]), 4 * quiet);
        let at = |turn, id| (turn * quiet, id);
        assert_eq!(named, [at(1, 1), at(2, 2), at(3, 3), at(4, 1)]);
    }

    #[test]
    fn it_names_the_leader_that_reached_it_last_first() {
        let quiet = 2 * ELECTION_TICKS;
        let mut turn = Turn::new(key(4));
        let voters = voters(&[1, 2, 3]);
        turn.heard(Some(key(2)), key(2));
        let named = dials(&mut turn, &voters, 3 * quiet);
        assert_eq!(named, [(quiet, 2), (2 * quiet, 3), (3 * quiet, 1)]);
    }

    #[test]
    fn a_message_from_the_leader_restarts_the_wait() {
        let quiet = 2 * ELECTION_TICKS;
        let mut turn = Turn::new(key(4));
        let voters = voters(&[1, 2, 3]);
        assert_eq!(dials(&mut turn, &voters, quiet - 1), []);
        turn.heard(Some(key(3)), key(3));
        assert_eq!(dials(&mut turn, &voters, quiet), [(quiet, 3)]);
    }

    #[test]
    fn a_message_from_a_member_that_does_not_lead_counts_for_nothing() {
        let quiet = 2 * ELECTION_TICKS;
        let mut turn = Turn::new(key(4));
        let voters = voters(&[1, 2, 3]);
        assert_eq!(dials(&mut turn, &voters, quiet - 1), []);
        turn.heard(Some(key(1)), key(3));
        turn.heard(None, key(3));
        assert_eq!(dials(&mut turn, &voters, 1), [(1, 1)]);
    }

    #[test]
    fn a_voter_and_a_member_with_no_voters_name_none() {
        let ticks = 4 * ELECTION_TICKS;
        assert_eq!(dials(&mut Turn::new(key(4)), &voters(&[]), ticks), []);
        assert_eq!(dials(&mut Turn::new(key(4)), &voters(&[1, 4]), ticks), []);
    }
}
