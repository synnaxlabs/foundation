//! The frame that a remote reader builds from the frames of its index (HUB WIRE).

use std::ops::Range;

use super::key_set::{self, KeySet};
use super::{Frame, Mask, View, charge, ends, padded, parts, to_u32, to_usize};
use crate::channel::Slot;
use crate::hash;

/// The frame that a remote reader builds from each frame of its index: one series for
/// each place, in place order, at the ends of [`ends`] (HUB WIRE). A place is one
/// listing of a slot. A slot after its first listing, or one that a frame's key set
/// lacks, holds no series. Keeps what it learns of each key set it lays, until it is
/// dropped, so only the first frame of a key set allocates: about 12 bytes for each
/// place in that key set. A node builds key sets only from the spec, which bounds
/// them.
#[derive(Debug)]
pub struct Places {
    slots: Box<[Slot]>,
    held: hash::Map<key_set::Key, Held>,
    /// The bounds of each entry of the last [`Held`] laid, by its position there.
    bounds: Vec<Option<Range<usize>>>,
    placed: Vec<Placed>,
}

/// One series of the frame that a [`Places`] reader builds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    /// Its place: its listing in [`Places::new`].
    pub place: usize,
    /// Its bytes in the home's frame, as [`Frame::body`] holds them.
    pub bounds: Range<usize>,
    /// Its end in the reader's frame, as [`Frame::ends`] gives it there. The series
    /// starts at `end - bounds.len()`, and the bytes between the end before it and
    /// that start are zeros.
    pub end: usize,
}

/// The places in one key set.
#[derive(Debug)]
struct Held {
    mask: Mask,
    /// Each entry that a place names, sorted, with the first place that names it.
    entries: Box<[(u32, u32)]>,
    /// The position in `entries` of each, in place order.
    order: Box<[u32]>,
    /// The places name each entry of the key set in entry order, so the reader's frame
    /// is the home's frame.
    whole: bool,
}

impl Places {
    /// The places of `slots`, one for each listing, in order.
    #[must_use]
    pub fn new(slots: Box<[Slot]>) -> Self {
        Self {
            slots,
            held: hash::Map::default(),
            bounds: Vec::new(),
            placed: Vec::new(),
        }
    }

    /// The series of `frame`, of key set `set`, at the places, in place order. Time
    /// is O(m log(n/m)) for m places in `set` and n series in `frame`.
    ///
    /// # Panics
    ///
    /// If `set` is not the key set of `frame`.
    pub fn lay(&mut self, frame: &Frame, set: &KeySet) -> &[Placed] {
        let held = self
            .held
            .entry(set.key())
            .or_insert_with(|| Held::new(&self.slots, set));
        lay(held, frame, &mut self.bounds, &mut self.placed);
        &self.placed
    }

    /// The [`Frame::charge`] of the frame that the reader builds from `frame`, of key
    /// set `set`. O(1) when the places name each entry of `set` in entry order, as
    /// the reader's frame is then `frame`; else as for [`Places::lay`], with no
    /// sort by place.
    ///
    /// # Panics
    ///
    /// If `set` is not the key set of `frame`.
    pub fn charge(&mut self, frame: &Frame, set: &KeySet) -> u64 {
        let held = self
            .held
            .entry(set.key())
            .or_insert_with(|| Held::new(&self.slots, set));
        if held.whole {
            assert!(
                frame.key_set() == set.key(),
                "the frame is of key set {} and the places of key set {}",
                frame.key_set().get(),
                set.key().get()
            );
            let (_, descriptors, body) = parts(&frame.0);
            return charge(descriptors.len(), body.len());
        }
        // The body holds each series padded, but the last in place order unpadded.
        let (mut series, mut padding) = (0, 0);
        let mut last: Option<(u32, usize)> = None;
        each(held, frame, |at, bounds| {
            let place = held.entries[at].1;
            series += 1;
            padding += padded(bounds.len());
            if last.is_none_or(|(last, _)| last < place) {
                last = Some((place, bounds.len()));
            }
        });
        let body = last.map_or(0, |(_, len)| padding - padded(len) + len);
        charge(series, body)
    }
}

/// Calls `f` with the position in `held.entries` and the bounds of each series of
/// `frame` that a place names, in entry order.
fn each(held: &Held, frame: &Frame, mut f: impl FnMut(usize, Range<usize>)) {
    let mut at = 0;
    for (entry, range) in View::new(frame, &held.mask).bounds() {
        let entry = to_u32(entry);
        // The mask adds the index of each group, which a place need not name.
        while held.entries.get(at).is_some_and(|&(held, _)| held < entry) {
            at += 1;
        }
        if held.entries.get(at).is_some_and(|&(held, _)| held == entry) {
            f(at, range);
            at += 1;
        }
    }
}

/// Fills `placed` with the series of `frame` at the places of `held`.
fn lay(
    held: &Held,
    frame: &Frame,
    bounds: &mut Vec<Option<Range<usize>>>,
    placed: &mut Vec<Placed>,
) {
    bounds.clear();
    bounds.resize(held.entries.len(), None);
    each(held, frame, |at, range| bounds[at] = Some(range));
    placed.clear();
    let present = held.order.iter().filter_map(|&at| {
        let at = to_usize(at);
        let range = bounds[at].clone()?;
        let len = range.len();
        Some(((to_usize(held.entries[at].1), range), len))
    });
    placed.extend(ends(present).map(|((place, bounds), end)| Placed {
        place,
        bounds,
        end,
    }));
}

impl Held {
    fn new(slots: &[Slot], set: &KeySet) -> Self {
        let mut entries: Vec<(u32, u32)> = slots
            .iter()
            .enumerate()
            .filter_map(|(place, &slot)| Some((to_u32(set.find(slot)?), to_u32(place))))
            .collect();
        // Each entry keeps its first place.
        entries.sort_unstable();
        entries.dedup_by_key(|&mut (entry, _)| entry);
        let mut order: Vec<u32> = (0..to_u32(entries.len())).collect();
        order.sort_unstable_by_key(|&at| entries[to_usize(at)].1);
        let whole = entries.len() == set.entries().len()
            && order.iter().enumerate().all(|(n, &at)| to_usize(at) == n);
        Self {
            mask: Mask::of_entries(
                set,
                entries.iter().map(|&(entry, _)| to_usize(entry)),
            ),
            entries: entries.into(),
            order: order.into(),
            whole,
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::collection::vec;
    use proptest::prelude::*;

    use super::*;
    use crate::frame::key_set::Group;
    use crate::frame::tests::{Case, cases, frame_of, interner, key, pool};
    use crate::frame::{Draft, Form, Path, split};
    use crate::sample::{Scalar, Type};

    const F64: Type = Type::Scalar(Scalar::F64);

    /// A case and its places, each an entry of the case's key set by position modulo
    /// the entries, or a slot of no entry at the last position.
    fn placed() -> impl Strategy<Value = (Case, Vec<usize>)> {
        (cases(), vec(0_usize..200, 0..60))
    }

    fn slots(set: &KeySet, picks: &[usize]) -> Box<[Slot]> {
        let entries = set.entries();
        picks
            .iter()
            .map(|&pick| match entries.get(pick % (entries.len() + 1)) {
                Some(entry) => entry.slot,
                None => Slot::new(u32::MAX),
            })
            .collect()
    }

    /// The series of the reader's frame, as the rule states it: for each place in
    /// order, the series of its slot's entry, when the place is the slot's first
    /// listing and the frame holds that series.
    fn model<'a>(
        set: &KeySet,
        frame: &'a Frame,
        slots: &[Slot],
    ) -> Vec<(usize, &'a [u8])> {
        let mut seen = Vec::new();
        let mut series = Vec::new();
        for (place, &slot) in slots.iter().enumerate() {
            if seen.contains(&slot) {
                continue;
            }
            seen.push(slot);
            if let Some(bytes) = set.find(slot).and_then(|entry| frame.series(entry)) {
                series.push((place, bytes));
            }
        }
        series
    }

    /// The series bytes of the reader's frame that `placed` lays out from `frame`.
    fn build(frame: &Frame, placed: &[Placed]) -> Vec<u8> {
        let home = frame.body();
        let mut body = vec![0xff; placed.last().map_or(0, |placed| placed.end)];
        let mut last = 0;
        for placed in placed {
            let start = placed.end - placed.bounds.len();
            body[last..start].fill(0);
            body[start..placed.end].copy_from_slice(&home[placed.bounds.clone()]);
            last = placed.end;
        }
        body
    }

    proptest! {
        #[test]
        fn lays_the_series_of_each_first_place_in_place_order((case, picks) in placed()) {
            let (set, frame) = frame_of(&case);
            let slots = slots(&set, &picks);
            let expected = model(&set, &frame, &slots);
            let mut places = Places::new(slots);
            for _ in 0..2 {
                let placed = places.lay(&frame, &set).to_vec();
                let body = build(&frame, &placed);
                let laid = placed.iter().map(|placed| (placed.place, placed.end));
                let read: Vec<(usize, &[u8])> = split(&body, laid).collect();
                prop_assert_eq!(&read, &expected);
                let lens = expected.iter().map(|&(place, bytes)| (place, bytes.len()));
                let ends: Vec<(usize, usize)> = ends(lens).collect();
                let laid: Vec<(usize, usize)> =
                    placed.iter().map(|placed| (placed.place, placed.end)).collect();
                prop_assert_eq!(laid, ends);
                let charge = charge(placed.len(), body.len());
                prop_assert_eq!(places.charge(&frame, &set), charge);
            }
        }

        #[test]
        fn charges_places_of_each_entry_in_entry_order_as_the_frame(case in cases()) {
            let (set, frame) = frame_of(&case);
            let slots: Box<[Slot]> = set.entries().iter().map(|entry| entry.slot).collect();
            let mut places = Places::new(slots);
            let charge = charge(frame.ends().count(), frame.body().len());
            prop_assert_eq!(places.charge(&frame, &set), charge);
            prop_assert!(places.placed.is_empty(), "the charge laid the frame");
            if set.groups().len() == 1 {
                prop_assert_eq!(charge, frame.charge());
            }
        }
    }

    /// Two key sets of one interner: key 1 indexes key 2 in the first; key 1 indexes
    /// key 2 and key 3 indexes key 4 in the second.
    fn two_sets() -> (std::sync::Arc<KeySet>, std::sync::Arc<KeySet>) {
        let mut interner = interner();
        let one = interner.intern(&[Group {
            index: key(1),
            data: &[(key(2), F64)],
        }]);
        let two = interner.intern(&[
            Group {
                index: key(1),
                data: &[(key(2), F64)],
            },
            Group {
                index: key(3),
                data: &[(key(4), F64)],
            },
        ]);
        (one, two)
    }

    /// A frame of `set` with `series`, each byte its entry plus one.
    fn filled(set: &KeySet, series: &[(usize, usize)]) -> Frame {
        let mut draft = Draft::new(&pool(1 << 16), set, Form::Encoded, series).unwrap();
        for (entry, bytes) in draft.iter_mut() {
            bytes.fill(u8::try_from(entry + 1).unwrap());
        }
        draft.freeze(Path::Live)
    }

    #[test]
    fn keeps_the_places_of_each_key_set_apart() {
        let (one, two) = two_sets();
        let slot = |n: u32| two.entries()[usize::try_from(n).unwrap()].slot;
        // Places: key 4, key 2, key 1, key 4 again, then key 3.
        let mut places =
            Places::new([slot(3), slot(1), slot(0), slot(3), slot(2)].into());
        let a = filled(&one, &[(0, 3), (1, 10)]);
        let b = filled(&two, &[(0, 3), (2, 5), (3, 1)]);
        for _ in 0..2 {
            assert_eq!(
                places.lay(&a, &one),
                [
                    Placed {
                        place: 1,
                        bounds: 8..18,
                        end: 10
                    },
                    Placed {
                        place: 2,
                        bounds: 0..3,
                        end: 19
                    },
                ]
            );
            assert_eq!(
                places.lay(&b, &two),
                [
                    Placed {
                        place: 0,
                        bounds: 16..17,
                        end: 1
                    },
                    Placed {
                        place: 2,
                        bounds: 0..3,
                        end: 11
                    },
                    Placed {
                        place: 4,
                        bounds: 8..13,
                        end: 21
                    },
                ]
            );
        }
        assert_eq!(places.charge(&a, &one), charge(2, 19));
        assert_eq!(places.charge(&b, &two), charge(3, 21));
    }

    #[test]
    fn lays_nothing_of_a_frame_without_a_placed_series() {
        let (one, two) = two_sets();
        let mut places = Places::new([two.entries()[3].slot].into());
        assert_eq!(places.lay(&filled(&one, &[(0, 3), (1, 10)]), &one), []);
        assert_eq!(places.lay(&filled(&two, &[(0, 3), (1, 10)]), &two), []);
        assert_eq!(places.charge(&filled(&two, &[(0, 3)]), &two), charge(0, 0));
    }

    #[test]
    #[should_panic(expected = "the frame is of key set 0 and the mask of key set 1")]
    fn panics_on_a_frame_of_another_key_set_in_lay() {
        let (one, two) = two_sets();
        let mut places = Places::new([one.entries()[1].slot].into());
        places.lay(&filled(&one, &[(0, 3)]), &two);
    }

    #[test]
    #[should_panic(expected = "the frame is of key set 0 and the places of key set 1")]
    fn panics_on_a_frame_of_another_key_set_in_a_whole_charge() {
        let (one, two) = two_sets();
        let slots = two.entries().iter().map(|entry| entry.slot).collect();
        Places::new(slots).charge(&filled(&one, &[(0, 3)]), &two);
    }
}
