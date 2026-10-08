//! A region of the spec: the record that a parent keeps for each child, and the check
//! of a region's definitions.

use std::fmt;

use types::name::Name;

mod check;

pub use check::{Problem, check};

/// The epoch and the first voters of a child region. Its tree key holds the prefix, so
/// the prefix is not part of the record. The current voters live in the region's own
/// Raft config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delegation {
    epoch: u64,
    voters: Vec<Name>,
}

impl Delegation {
    /// Makes a record. `voters` are node names; their order and repeats do not count.
    ///
    /// # Errors
    ///
    /// Returns [`NoVoters`] when `voters` is empty, because a region with no voter
    /// cannot commit.
    pub fn new(
        epoch: u64,
        voters: impl IntoIterator<Item = Name>,
    ) -> Result<Self, NoVoters> {
        let mut voters = voters.into_iter().collect::<Vec<_>>();
        voters.sort();
        voters.dedup();
        if voters.is_empty() {
            return Err(NoVoters);
        }
        Ok(Self { epoch, voters })
    }

    /// The epoch. A forced takeover raises it, and nodes fence on it.
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The voters the region started with, in name order with no repeat. The record
    /// does not follow later changes; the region's Raft config holds the current set.
    #[must_use]
    pub fn initial_voters(&self) -> &[Name] {
        &self.voters
    }
}

/// A region record with no voter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoVoters;

impl fmt::Display for NoVoters {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a region has no voter")
    }
}

impl std::error::Error for NoVoters {}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn refuses_a_record_with_no_voter() {
        assert_eq!(Delegation::new(1, []), Err(NoVoters));
        assert_eq!(NoVoters.to_string(), "a region has no voter");
    }

    proptest! {
        #[test]
        fn keeps_the_same_voters_in_any_order_or_repeat(
            texts in prop::collection::vec("[a-c]{1,2}", 1..6),
            shuffle in any::<prop::sample::Index>(),
        ) {
            let names = texts.iter().map(|t| t.parse::<Name>().unwrap());
            let made = Delegation::new(3, names.clone()).unwrap();
            let mut rotated = names.collect::<Vec<_>>();
            let by = shuffle.index(rotated.len());
            rotated.rotate_left(by);
            rotated.extend(rotated.clone());
            prop_assert_eq!(Delegation::new(3, rotated), Ok(made.clone()));
            prop_assert!(made.initial_voters().is_sorted_by(|a, b| a < b));
            let mut expected = texts.clone();
            expected.sort();
            expected.dedup();
            let voters = made
                .initial_voters()
                .iter()
                .map(Name::as_str)
                .collect::<Vec<_>>();
            prop_assert_eq!(voters, expected);
        }
    }
}
