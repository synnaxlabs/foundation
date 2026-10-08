//! The frame that a remote reader builds from the frames of its index (HUB WIRE).

use std::ops::Range;

use super::key_set::{self, KeySet};
use super::view::gallop;
use super::{Frame, Mask, View, charge, ends, padded, parts, to_u32, to_usize};
use crate::channel::Slot;
use crate::hash;

/// The frame that a remote reader builds from each frame of its index: one series for
/// each place, in place order, at the ends of [`ends`] (HUB WIRE). A place is one
/// listing of a slot. A slot after its first listing, or one that a frame's key set
/// lacks, holds no series. Keeps what it learns of each key set it lays until it is
/// dropped, about 16 bytes for each place in that key set, and two buffers that grow
/// to the most entries of a dense frame laid (24 bytes each) and the most series given
/// (32 bytes each). Only these allocate. A node builds key sets only from the spec,
/// which bounds them.
#[derive(Debug)]
pub struct Places {
    slots: Box<[Slot]>,
    held: hash::Map<key_set::Key, Held>,
    /// The bounds of each entry of a dense [`Held`] as [`lay`] places it, by its
    /// position. Each is `None` between calls.
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
    /// starts at `end - bounds.len()`. The bytes from the end before it to that start
    /// are padding, which the caller writes (FRAME LAYOUT).
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
    /// The places name each entry of the key set, so the reader's frame holds each
    /// series of a frame.
    every: bool,
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
    /// is O(j log(n/j)) for the lesser j and the greater n of the m places in `set`
    /// and the series in `frame`, plus O(k log k) for the k series it gives, or O(m)
    /// when it gives at least one series for each 8 entries that places name.
    ///
    /// # Panics
    ///
    /// If `set` is not the key set of `frame`.
    pub fn lay(&mut self, frame: &Frame, set: &KeySet) -> &[Placed] {
        let held = held(&mut self.held, &self.slots, frame, set);
        lay(held, frame, &mut self.bounds, &mut self.placed);
        &self.placed
    }

    /// The [`Frame::charge`] of the frame that the reader builds from `frame`, of key
    /// set `set`. O(1) when the places name each entry of `set`, in any order, as that
    /// frame then holds each series of `frame`; else as for [`Places::lay`].
    ///
    /// # Panics
    ///
    /// If `set` is not the key set of `frame`.
    pub fn charge(&mut self, frame: &Frame, set: &KeySet) -> u64 {
        let held = held(&mut self.held, &self.slots, frame, set);
        // Each block payload is a multiple of 8 bytes, so the padding of the last
        // series, which differs with place order, does not change the footprint.
        if held.every {
            let (_, descriptors, body) = parts(&frame.0);
            return charge(descriptors.len(), body.len());
        }
        let (mut series, mut body) = (0, 0);
        each(held, frame, |_, bounds| {
            series += 1;
            body += padded(bounds.len());
        });
        charge(series, body)
    }
}

/// What `slots` learn of `set`, the key set of `frame`, from `held`, or new.
fn held<'a>(
    held: &'a mut hash::Map<key_set::Key, Held>,
    slots: &[Slot],
    frame: &Frame,
    set: &KeySet,
) -> &'a Held {
    assert!(
        frame.key_set() == set.key(),
        "the frame is of key set {} and the places of key set {}",
        frame.key_set().get(),
        set.key().get()
    );
    held.entry(set.key())
        .or_insert_with(|| Held::new(slots, set))
}

/// Calls `f` with the position in `held.entries` and the bounds of each series of
/// `frame` that a place names, in entry order.
fn each(held: &Held, frame: &Frame, mut f: impl FnMut(usize, Range<usize>)) {
    let mut at = 0;
    for (entry, range) in View::new(frame, &held.mask).bounds() {
        let entry = to_u32(entry);
        at += gallop(&held.entries[at..], |&(held, _)| held < entry);
        // The mask adds the index of each group, which a place need not name.
        if held.entries.get(at).is_some_and(|&(held, _)| held == entry) {
            f(at, range);
        }
    }
}

/// A frame that gives fewer than one series for each `SPARSE` entries that places name
/// is sparse: [`lay`] sorts its series by place, as a walk of each place costs more.
const SPARSE: usize = 8;

/// Fills `placed` with the series of `frame` at the places of `held`.
fn lay(
    held: &Held,
    frame: &Frame,
    bounds: &mut Vec<Option<Range<usize>>>,
    placed: &mut Vec<Placed>,
) {
    placed.clear();
    let dense_from = held.entries.len().div_ceil(SPARSE);
    let mut dense = false;
    // `place` holds the position in `held.entries` until the series is placed.
    each(held, frame, |at, range| {
        if dense {
            bounds[at] = Some(range);
            return;
        }
        placed.push(Placed {
            place: at,
            bounds: range,
            end: 0,
        });
        if placed.len() >= dense_from {
            dense = true;
            bounds.resize(held.entries.len(), None);
            for placed in placed.drain(..) {
                bounds[placed.place] = Some(placed.bounds);
            }
        }
    });
    if dense {
        let present = held.order.iter().filter_map(|&at| {
            let at = to_usize(at);
            let range = bounds[at].take()?;
            let len = range.len();
            Some(((to_usize(held.entries[at].1), range), len))
        });
        placed.extend(ends(present).map(|((place, bounds), end)| Placed {
            place,
            bounds,
            end,
        }));
    } else {
        sort(held, placed);
    }
}

/// Places the series of a sparse frame in `placed`, which holds them in entry order.
// Inlined into `lay`, it slows a lay of few places.
#[inline(never)]
fn sort(held: &Held, placed: &mut [Placed]) {
    for placed in placed.iter_mut() {
        placed.place = to_usize(held.entries[placed.place].1);
    }
    placed.sort_unstable_by_key(|placed| placed.place);
    let lens = placed.iter_mut().map(|placed| {
        let len = placed.bounds.len();
        (placed, len)
    });
    for (placed, end) in ends(lens) {
        placed.end = end;
    }
}

/// A place as a `u32`.
fn place_u32(place: usize) -> u32 {
    u32::try_from(place).expect("invariant: an open's keys fit a u32")
}

impl Held {
    fn new(slots: &[Slot], set: &KeySet) -> Self {
        let mut entries: Vec<(u32, u32)> = slots
            .iter()
            .enumerate()
            .filter_map(|(place, &slot)| {
                Some((to_u32(set.find(slot)?), place_u32(place)))
            })
            .collect();
        // Each entry keeps its first place.
        entries.sort_unstable();
        entries.dedup_by_key(|&mut (entry, _)| entry);
        let mut order: Vec<u32> = (0..to_u32(entries.len())).collect();
        order.sort_unstable_by_key(|&at| entries[to_usize(at)].1);
        let every = entries.len() == set.entries().len();
        Self {
            mask: Mask::of_entries(
                set,
                entries.iter().map(|&(entry, _)| to_usize(entry)),
            ),
            entries: entries.into(),
            order: order.into(),
            every,
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
    use crate::frame::{Draft, Form, Path, SERIES_ALIGN, split};
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
        fn charges_places_of_each_entry_in_any_order_as_the_frame_it_lays(
            case in cases(),
            turn: usize,
            reversed: bool,
        ) {
            let (set, frame) = frame_of(&case);
            let mut slots: Vec<Slot> = set.entries().iter().map(|entry| entry.slot).collect();
            let turn = turn % slots.len();
            slots.rotate_left(turn);
            if reversed {
                slots.reverse();
            }
            let mut places = Places::new(slots.into());
            let placed = places.lay(&frame, &set);
            let laid = charge(placed.len(), placed.last().map_or(0, |placed| placed.end));
            prop_assert_eq!(places.charge(&frame, &set), laid);
            prop_assert_eq!(laid, charge(frame.ends().count(), frame.body().len()));
            if set.groups().len() == 1 {
                prop_assert_eq!(laid, frame.charge());
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

    /// `charge` pads the last series, which a frame does not, so each block class
    /// must end at a series start.
    #[test]
    fn ends_each_block_class_at_a_series_start() {
        let mut len = 0;
        while block::footprint(len) != usize::MAX {
            let class = block::footprint(len);
            let (mut low, mut high) = (len, 2 * len + 1024);
            while high - low > 1 {
                let mid = low + (high - low) / 2;
                if block::footprint(mid) == class {
                    low = mid;
                } else {
                    high = mid;
                }
            }
            assert_eq!(low % SERIES_ALIGN, 0, "the class of {len} ends at {low}");
            len = high;
        }
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
    fn charges_a_padded_last_series_as_the_unpadded_one() {
        // Each size class of a block ends at a quarter step of a power of two.
        for shift in 3..31 {
            for quarter in 0..4 {
                let end = (1_usize << shift) + quarter * (1 << shift) / 4;
                for len in end.saturating_sub(64)..end + 64 {
                    assert_eq!(charge(3, padded(len)), charge(3, len), "{len} bytes");
                }
            }
        }
    }

    /// A frame that gives 2 series is dense for places of up to `2 * SPARSE` entries,
    /// whatever series it holds outside them (`tests/alloc` pins the cut). Each side
    /// lays the series in place order, which is not entry order here.
    #[test]
    fn lays_frames_on_each_side_of_the_sparse_bound() {
        let data: Vec<(crate::channel::Key, Type)> = (2..61)
            .map(|n| (key(n), Type::Scalar(Scalar::F64)))
            .collect();
        let set = interner().intern(&[Group {
            index: key(1),
            data: &data,
        }]);
        // The index series is outside the places, so the frame gives 2 of its 3.
        let frame = filled(&set, &[(0, 3), (52, 10), (57, 5)]);
        for entries in [15, 16, 17] {
            let slots: Box<[Slot]> = set.entries()[60 - entries..]
                .iter()
                .rev()
                .map(|e| e.slot)
                .collect();
            let expected = model(&set, &frame, &slots);
            let mut reader = Places::new(slots);
            let placed = reader.lay(&frame, &set).to_vec();
            let laid: Vec<(usize, usize)> = placed
                .iter()
                .map(|placed| (placed.place, placed.end))
                .collect();
            assert_eq!(laid, [(2, 5), (7, 18)], "{entries} entries");
            let body = build(&frame, &placed);
            let read: Vec<(usize, &[u8])> = split(&body, laid).collect();
            assert_eq!(read, expected, "{entries} entries");
        }
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
    #[should_panic(expected = "the frame is of key set 0 and the places of key set 1")]
    fn panics_on_a_frame_of_another_key_set_in_lay() {
        let (one, two) = two_sets();
        let mut places = Places::new([one.entries()[1].slot].into());
        places.lay(&filled(&one, &[(0, 3)]), &two);
    }

    #[test]
    #[should_panic(expected = "the frame is of key set 0 and the places of key set 1")]
    fn panics_on_a_frame_of_another_key_set_in_a_charge_of_every_entry() {
        let (one, two) = two_sets();
        let slots = two.entries().iter().map(|entry| entry.slot).collect();
        Places::new(slots).charge(&filled(&one, &[(0, 3)]), &two);
    }
}
