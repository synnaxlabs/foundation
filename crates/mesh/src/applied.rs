//! The terms of the entries that a node applied, for a proposal that waits for its
//! outcome.

use std::collections::{BTreeMap, VecDeque};

use raft::{Position, Term};

use crate::region::Refused;

/// What became of the entry that a leader put at one position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// This node applied that entry, with what the apply gave: a refused change
    /// changed nothing.
    Applied(Result<(), Refused>),
    /// The log has, or will have, a different entry at that index.
    Replaced,
    /// This node did not apply far enough to know.
    Pending,
}

/// The applied index at the start of one try of a proposal. The leader puts the entry
/// of the try above it, because a leader holds each entry that a node applied.
#[derive(Debug)]
pub(crate) struct Floor(u64);

/// The term of each entry that this node applied, as far as an open try can ask.
///
/// It holds one pair for each term that has an applied entry above the lowest open
/// floor, and one pair for the term of the last applied entry. So it holds at most one
/// pair while no try is open, and it grows only by the terms that the node applies
/// while a try is open. It holds the refusal of each refused entry above the lowest
/// open floor, so it holds none while no try is open.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    // The index of the last applied entry, or 0.
    index: u64,
    // Each term with the index of its first applied entry, in the order of the log.
    terms: VecDeque<(Term, u64)>,
    // The count of open tries at each floor.
    floors: BTreeMap<u64, usize>,
    // The refusal of each refused entry, by index.
    refusals: BTreeMap<u64, Refused>,
}

impl Applied {
    /// Records that this node applied the entry at `at`, with what the apply gave.
    /// Call it for each applied entry, in the order of the log.
    pub(crate) fn push(&mut self, at: Position, applied: Result<(), Refused>) {
        self.index = at.index;
        if self.terms.back().is_none_or(|&(term, _)| term != at.term) {
            self.terms.push_back((at.term, at.index));
        }
        if let Err(refused) = applied {
            self.refusals.insert(at.index, refused);
        }
        self.trim();
    }

    /// Starts a try. Until [`Applied::close`] gets the floor, this keeps the term of
    /// each applied entry above it.
    pub(crate) fn open(&mut self) -> Floor {
        let count = self.floors.entry(self.index).or_default();
        *count = count.saturating_add(1);
        Floor(self.index)
    }

    /// Ends the try of `floor`. Call it one time for each floor.
    ///
    /// # Panics
    ///
    /// When another `Applied` opened `floor`.
    pub(crate) fn close(&mut self, Floor(floor): &Floor) {
        let count = self
            .floors
            .get_mut(floor)
            .expect("invariant: a floor closes on the `Applied` that opened it");
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.floors.remove(floor);
        }
        self.trim();
    }

    /// What became of the entry that a leader put at `at` for the try of `floor`.
    ///
    /// - [`Outcome::Applied`] when this node applied an entry of the term of `at` at
    ///   the index of `at`, with the refusal of that entry, if any.
    /// - [`Outcome::Replaced`] when it applied an entry of a different term at that
    ///   index, or an entry of a higher term below it, or when `at` is not above
    ///   `floor`.
    /// - [`Outcome::Pending`] in each other case.
    pub(crate) fn outcome(&self, floor: &Floor, at: Position) -> Outcome {
        if at.index <= floor.0 {
            return Outcome::Replaced;
        }
        let mut terms = self.terms.iter().rev();
        if at.index > self.index {
            return match terms.next() {
                Some(&(last, _)) if last > at.term => Outcome::Replaced,
                _ => Outcome::Pending,
            };
        }
        match terms.find(|&&(_, first)| first <= at.index) {
            Some(&(term, _)) if term == at.term => Outcome::Applied(
                self.refusals.get(&at.index).cloned().map_or(Ok(()), Err),
            ),
            _ => Outcome::Replaced,
        }
    }

    // Removes each pair whose entries are all at or below the lowest open floor, but
    // not the last pair: the next entry can have its term. Removes each refusal at or
    // below that floor.
    fn trim(&mut self) {
        let low = self.floors.keys().next().copied().unwrap_or(self.index);
        while self
            .terms
            .get(1)
            .is_some_and(|&(_, first)| first <= low.saturating_add(1))
        {
            self.terms.pop_front();
        }
        self.refusals = self.refusals.split_off(&low.saturating_add(1));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;
    use types::digest::Digest;

    use super::*;
    use crate::pointer::Pointer;

    fn at(index: u64, term: u64) -> Position {
        Position {
            term: Term(term),
            index,
        }
    }

    /// A refusal that differs for each `index`.
    fn refusal(index: u64) -> Refused {
        let pointer = |version| Pointer {
            version,
            root: Digest([0; 32]),
        };
        Refused::Stale {
            base: pointer(index),
            pointer: pointer(0),
        }
    }

    /// An `Applied` after the entries of `terms`, from index 1, with a floor that a
    /// try opened after the first `before` of them.
    fn create_applied(terms: &[u64], before: usize) -> (Applied, Floor) {
        let mut applied = Applied::default();
        let mut entries = (1..).zip(terms).map(|(index, &term)| at(index, term));
        for entry in entries.by_ref().take(before) {
            applied.push(entry, Ok(()));
        }
        let floor = applied.open();
        entries.for_each(|entry| applied.push(entry, Ok(())));
        (applied, floor)
    }

    #[test]
    fn an_entry_above_the_applied_index_is_pending() {
        let (applied, floor) = create_applied(&[1, 1], 1);
        assert_eq!(applied.outcome(&floor, at(3, 1)), Outcome::Pending);
        assert_eq!(applied.outcome(&floor, at(3, 2)), Outcome::Pending);
    }

    #[test]
    fn an_entry_that_the_node_applied_after_the_floor_is_applied() {
        let (applied, floor) = create_applied(&[1, 1, 2, 2], 1);
        let applied_ok = Outcome::Applied(Ok(()));
        assert_eq!(applied.outcome(&floor, at(2, 1)), applied_ok);
        assert_eq!(applied.outcome(&floor, at(3, 2)), applied_ok);
        assert_eq!(applied.outcome(&floor, at(4, 2)), applied_ok);
    }

    #[test]
    fn a_refused_entry_that_the_node_applied_after_the_floor_gives_its_refusal() {
        let (mut applied, floor) = create_applied(&[1], 1);
        applied.push(at(2, 1), Err(refusal(2)));
        applied.push(at(3, 1), Ok(()));
        let refused = Outcome::Applied(Err(refusal(2)));
        assert_eq!(applied.outcome(&floor, at(2, 1)), refused);
        assert_eq!(applied.outcome(&floor, at(3, 1)), Outcome::Applied(Ok(())));
        assert_eq!(applied.outcome(&floor, at(2, 2)), Outcome::Replaced);
    }

    #[test]
    fn an_index_with_an_entry_of_a_different_term_is_replaced() {
        let (applied, floor) = create_applied(&[1, 1, 2, 2], 1);
        assert_eq!(applied.outcome(&floor, at(2, 2)), Outcome::Replaced);
        assert_eq!(applied.outcome(&floor, at(3, 1)), Outcome::Replaced);
        assert_eq!(applied.outcome(&floor, at(4, 3)), Outcome::Replaced);
    }

    #[test]
    fn an_entry_above_an_applied_entry_of_a_higher_term_is_replaced() {
        let (applied, floor) = create_applied(&[1, 3], 1);
        assert_eq!(applied.outcome(&floor, at(5, 2)), Outcome::Replaced);
        assert_eq!(applied.outcome(&floor, at(5, 3)), Outcome::Pending);
    }

    #[test]
    fn an_entry_that_is_not_above_the_floor_is_replaced() {
        let (applied, floor) = create_applied(&[1, 1], 2);
        assert_eq!(applied.outcome(&floor, at(2, 1)), Outcome::Replaced);
        assert_eq!(applied.outcome(&floor, at(0, 0)), Outcome::Replaced);
    }

    #[test]
    fn an_applied_with_no_entry_knows_no_outcome() {
        let (applied, floor) = create_applied(&[], 0);
        assert_eq!(applied.outcome(&floor, at(1, 0)), Outcome::Pending);
    }

    // The pairs that stay only take memory, which no call shows, so the tests of the
    // bound read `terms`.
    #[test]
    fn it_holds_one_pair_while_no_try_is_open() {
        let (mut applied, floor) = create_applied(&[1, 2, 3, 3, 4], 0);
        assert_eq!(
            applied.terms,
            [(Term(1), 1), (Term(2), 2), (Term(3), 3), (Term(4), 5)]
        );
        applied.close(&floor);
        assert_eq!(applied.terms, [(Term(4), 5)]);
        applied.push(at(6, 5), Ok(()));
        assert_eq!(applied.terms, [(Term(5), 6)]);
    }

    #[test]
    fn it_holds_the_pairs_above_the_lowest_floor_of_two() {
        let (mut applied, low) = create_applied(&[1, 2, 3], 2);
        let high = applied.open();
        applied.push(at(4, 4), Ok(()));
        assert_eq!(applied.terms, [(Term(3), 3), (Term(4), 4)]);
        applied.close(&high);
        assert_eq!(applied.terms, [(Term(3), 3), (Term(4), 4)]);
        applied.close(&low);
        assert_eq!(applied.terms, [(Term(4), 4)]);
    }

    #[test]
    #[should_panic(
        expected = "invariant: a floor closes on the `Applied` that opened it"
    )]
    fn a_floor_of_a_different_applied_does_not_close() {
        let (_, floor) = create_applied(&[1], 1);
        Applied::default().close(&floor);
    }

    #[derive(Clone, Debug)]
    enum Step {
        /// The node applies the next entry, `up` terms above the last one, and
        /// refuses it when `refused`.
        Push {
            up: u64,
            refused: bool,
        },
        Open,
        /// The open floor at this place closes, counted around the list.
        Close(usize),
    }

    fn step() -> impl Strategy<Value = Step> {
        prop_oneof![
            3 => any::<bool>().prop_map(|refused| Step::Push { up: 0, refused }),
            1 => (1..3_u64, any::<bool>())
                .prop_map(|(up, refused)| Step::Push { up, refused }),
            2 => Just(Step::Open),
            2 => any::<usize>().prop_map(Step::Close),
        ]
    }

    /// The outcome by the term of each applied entry, from index 1, and whether the
    /// node refused it.
    fn expected(history: &[(u64, bool)], floor: u64, at: Position) -> Outcome {
        let applied = usize::try_from(at.index)
            .ok()
            .and_then(|index| index.checked_sub(1))
            .and_then(|place| history.get(place));
        match applied {
            _ if at.index <= floor => Outcome::Replaced,
            Some(&(term, false)) if term == at.term.0 => Outcome::Applied(Ok(())),
            Some(&(term, true)) if term == at.term.0 => {
                Outcome::Applied(Err(refusal(at.index)))
            }
            Some(_) => Outcome::Replaced,
            None if history.last().is_some_and(|&(last, _)| last > at.term.0) => {
                Outcome::Replaced
            }
            None => Outcome::Pending,
        }
    }

    proptest! {
        // After each step, each open floor gets the outcome of each position near the
        // log that the full history gives, and the pairs and refusals are only those
        // of the doc.
        #[test]
        fn each_open_floor_gets_the_outcome_of_the_full_history(
            steps in prop::collection::vec(step(), 0..32),
        ) {
            let mut applied = Applied::default();
            let mut history: Vec<(u64, bool)> = Vec::new();
            let mut open: Vec<Floor> = Vec::new();
            for step in steps {
                let last = u64::try_from(history.len()).unwrap();
                match step {
                    Step::Push { up, refused } => {
                        let term = history.last().map_or(1, |&(term, _)| term);
                        let term = term.checked_add(up).unwrap();
                        history.push((term, refused));
                        let index = last.checked_add(1).unwrap();
                        let result = if refused { Err(refusal(index)) } else { Ok(()) };
                        applied.push(at(index, term), result);
                    }
                    Step::Open => open.push(applied.open()),
                    Step::Close(place) => {
                        if let Some(place) = place.checked_rem(open.len()) {
                            applied.close(&open.swap_remove(place));
                        }
                    }
                }
                let last = u64::try_from(history.len()).unwrap();
                let top = history.last().map_or(1, |&(term, _)| term);
                for floor in &open {
                    for index in 0..=last.saturating_add(2) {
                        for term in 0..=top.saturating_add(1) {
                            let position = at(index, term);
                            prop_assert_eq!(
                                applied.outcome(floor, position),
                                expected(&history, floor.0, position),
                                "floor {} at {:?}", floor.0, position
                            );
                        }
                    }
                }
                let low = open.iter().map(|floor| floor.0).min().unwrap_or(last);
                let low = usize::try_from(low).unwrap();
                let above = history.iter().skip(low);
                let terms = above.clone().map(|&(term, _)| term).collect::<BTreeSet<_>>();
                let pairs = terms.len().max(usize::from(last > 0));
                prop_assert_eq!(applied.terms.len(), pairs);
                let refusals = above.filter(|&&(_, refused)| refused).count();
                prop_assert_eq!(applied.refusals.len(), refusals);
            }
        }
    }
}
