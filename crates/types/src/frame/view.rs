//! Views: a frame and the entries that one reader wants (M2).

use std::cmp::Ordering;
use std::iter;
use std::sync::Arc;

use super::key_set::{self, KeySet};
use super::{
    Frame, SERIES_ALIGN, body_start, bounds, lead, split, to_u32, to_u64, to_usize,
};
use crate::channel;

/// The entries of one key set that a reader wants, and the index of each. Made once
/// per key set and reader, and shared by every view of that pair.
#[derive(Debug, PartialEq, Eq)]
pub struct Mask {
    set: key_set::Key,
    /// The entries held, sorted.
    entries: Box<[u32]>,
    /// The groups whose index is held, sorted.
    groups: Box<[u32]>,
    /// Whether the mask holds every entry of its key set.
    full: bool,
}

impl Mask {
    /// The entries of `set` whose slot `wanted` accepts, and the index of each, so the
    /// series of a view always make a frame. Time is linear in the entries of `set`.
    #[must_use]
    pub fn new(set: &KeySet, mut wanted: impl FnMut(channel::Slot) -> bool) -> Self {
        let mut entries = Vec::new();
        for (entry, item) in set.entries().iter().enumerate() {
            if wanted(item.slot) {
                entries.extend([entry, set.index(entry)].map(to_u32));
            }
        }
        entries.sort_unstable();
        entries.dedup();
        // Indexes in entry order are in group order (`KeySet::groups`).
        let groups = entries
            .iter()
            .map(|&entry| to_usize(entry))
            .filter(|&entry| set.index(entry) == entry)
            .map(|index| set.entries()[index].group)
            .collect();
        Self {
            set: set.key(),
            full: entries.len() == set.entries().len(),
            entries: entries.into(),
            groups,
        }
    }

    /// Whether the mask holds `entry`. An entry past the key set is not held. Time is
    /// logarithmic in the entries the mask holds.
    #[must_use]
    pub fn contains(&self, entry: usize) -> bool {
        u32::try_from(entry)
            .is_ok_and(|entry| self.entries.binary_search(&entry).is_ok())
    }

    /// Whether the mask holds no entry: the reader wants nothing of the key set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// What a reader gets: a frame and the entries it wants (M2). Cloning it adds a
/// reference to each.
#[derive(Clone, Debug)]
pub struct View {
    frame: Frame,
    mask: Arc<Mask>,
}

impl View {
    /// A view of `frame` through `mask`.
    ///
    /// # Panics
    ///
    /// If `mask` is of another key set than `frame`.
    #[must_use]
    pub fn new(frame: Frame, mask: Arc<Mask>) -> Self {
        assert!(
            frame.key_set() == mask.set,
            "the frame is of key set {} and the mask of key set {}",
            frame.key_set().get(),
            mask.set.get()
        );
        Self { frame, mask }
    }

    /// The whole frame, for its path and ranges.
    #[must_use]
    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    /// Each present entry that the mask holds and its series bytes, in entry order.
    /// Time is linear in the frame's series when the mask holds every entry, else
    /// O(m log(n/m)) for the smaller m and larger n of the frame's series and the
    /// mask's entries.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &[u8])> {
        let (_, descriptors, body) = split(&self.frame.0);
        let (every, entries) = if self.mask.full {
            (descriptors.len(), &[][..])
        } else {
            (0, &self.mask.entries[..])
        };
        (0..every).chain(join(descriptors, entries)).map(move |n| {
            let (start, end) = bounds(descriptors, n);
            (to_usize(lead(&descriptors[n])), &body[start..end])
        })
    }

    /// The charge of a frame of only the view's series (CREDIT RULES): what a remote
    /// complete reader spends. Equal to [`Frame::charge`] when the mask holds every
    /// present series. Time is constant when the mask holds every entry, else as for
    /// [`View::iter`].
    #[must_use]
    pub fn charge(&self) -> u64 {
        if self.mask.full {
            return self.frame.charge();
        }
        let (ranges, descriptors, _) = split(&self.frame.0);
        let groups = join(ranges, &self.mask.groups).count();
        let (mut series, mut bytes) = (0, 0_usize);
        for n in join(descriptors, &self.mask.entries) {
            let (start, end) = bounds(descriptors, n);
            series += 1;
            bytes = bytes.next_multiple_of(SERIES_ALIGN) + (end - start);
        }
        to_u64(block::footprint(body_start(groups, series) + bytes))
    }
}

/// The position of each record in `records` that leads with a key in `keys`, in order.
/// Both are sorted. Each step skips ahead on both by [`gallop`], so time is
/// O(m log(n/m)) for the shorter m and longer n.
fn join<'a, const N: usize>(
    records: &'a [[u8; N]],
    mut keys: &'a [u32],
) -> impl Iterator<Item = usize> + 'a {
    let mut at = 0;
    iter::from_fn(move || {
        loop {
            let key = lead(records.get(at)?);
            let &next = keys.first()?;
            match next.cmp(&key) {
                Ordering::Less => keys = &keys[gallop(keys, |&held| held < key)..],
                Ordering::Greater => {
                    at += gallop(&records[at..], |record| lead(record) < next);
                }
                Ordering::Equal => {
                    keys = &keys[1..];
                    at += 1;
                    return Some(at - 1);
                }
            }
        }
    })
}

/// The first position in `items` where `before` is false, as `slice::partition_point`
/// gives it, in time logarithmic in that position.
fn gallop<T>(items: &[T], mut before: impl FnMut(&T) -> bool) -> usize {
    let mut high = 1;
    while high <= items.len() && before(&items[high - 1]) {
        high *= 2;
    }
    let low = high / 2;
    low + items[low..high.min(items.len())].partition_point(before)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::collection::{btree_set, vec};
    use proptest::prelude::*;

    use super::*;
    use crate::frame::key_set::Group;
    use crate::frame::tests::{Case, cases, frame_of, interner, key, pool, two_groups};
    use crate::frame::{Draft, Form, Path};

    /// Wants the slots of the keys in `keys`, given that key `n` has slot `n`.
    fn keys(keys: &[u32]) -> impl Fn(channel::Slot) -> bool {
        let keys = keys.to_vec();
        move |slot| keys.contains(&slot.get())
    }

    /// The entries of `set` that `mask` holds, past the end by one.
    fn held(set: &KeySet, mask: &Mask) -> Vec<usize> {
        (0..=set.entries().len())
            .filter(|&entry| mask.contains(entry))
            .collect()
    }

    /// A frame of [`two_groups`] with all four series: index key 1 (entry 0) with 3
    /// bytes, key 2 (entry 1) with 10, index key 3 (entry 2) with 5, and key 4 (entry
    /// 3) with 1. Each byte is its entry.
    fn full(set: &KeySet) -> Frame {
        let series = [(0, 3), (1, 10), (2, 5), (3, 1)];
        let mut draft =
            Draft::new(&pool(1 << 16), set, Form::Encoded, &series).unwrap();
        for (entry, bytes) in draft.iter_mut() {
            bytes.fill(u8::try_from(entry).unwrap());
        }
        draft.freeze(Path::Live)
    }

    #[test]
    fn holds_each_wanted_entry_and_its_index() {
        let set = two_groups();
        let mask = Mask::new(&set, keys(&[4]));
        assert_eq!(held(&set, &mask), [2, 3]);
        assert!(!mask.is_empty());
        assert!(!mask.contains(usize::MAX));
    }

    #[test]
    fn holds_an_index_without_its_data() {
        let set = two_groups();
        assert_eq!(held(&set, &Mask::new(&set, keys(&[1]))), [0]);
    }

    #[test]
    fn wants_nothing_of_a_key_set_without_a_wanted_slot() {
        let set = two_groups();
        let mask = Mask::new(&set, keys(&[5, 6]));
        assert!(mask.is_empty());
        assert_eq!(held(&set, &mask), Vec::<usize>::new());
    }

    #[test]
    fn reads_only_the_held_series() {
        let set = two_groups();
        let view = View::new(full(&set), Arc::new(Mask::new(&set, keys(&[4]))));
        let read: Vec<(usize, Vec<u8>)> = view
            .iter()
            .map(|(entry, bytes)| (entry, bytes.to_vec()))
            .collect();
        assert_eq!(read, [(2, vec![2; 5]), (3, vec![3])]);
        assert_eq!(view.frame().key_set(), set.key());
    }

    /// One range, two descriptors, and 5 bytes, 3 of padding, and 1 byte of series.
    #[test]
    fn charges_a_frame_of_only_its_series() {
        let set = two_groups();
        let view = View::new(full(&set), Arc::new(Mask::new(&set, keys(&[4]))));
        let len = 16 + 16 + 2 * 8 + 8 + 1;
        assert_eq!(view.charge(), u64::try_from(block::footprint(len)).unwrap());
    }

    #[test]
    fn charges_the_whole_frame_through_a_full_mask() {
        let set = two_groups();
        let frame = full(&set);
        let view = View::new(frame.clone(), Arc::new(Mask::new(&set, |_| true)));
        assert_eq!(view.charge(), frame.charge());
    }

    #[test]
    #[should_panic(expected = "the frame is of key set 1 and the mask of key set 0")]
    fn refuses_a_mask_of_another_key_set() {
        let mut interner = interner();
        let first = interner.intern(&[Group {
            index: key(1),
            data: &[],
        }]);
        let second = interner.intern(&[Group {
            index: key(2),
            data: &[],
        }]);
        let draft = Draft::new(&pool(1 << 16), &second, Form::Raw, &[(0, 0)]).unwrap();
        let mask = Arc::new(Mask::new(&first, |_| true));
        drop(View::new(draft.freeze(Path::Live), mask));
    }

    /// A case and the chance that its mask wants each entry, from none to all. One
    /// case in four wants every entry.
    fn masked() -> impl Strategy<Value = (Case, Vec<bool>)> {
        (cases(), prop_oneof![1 => Just(1.0), 3 => 0.0..=1.0_f64])
            .prop_flat_map(|(case, p)| (Just(case), vec(prop::bool::weighted(p), 164)))
    }

    proptest! {
        #[test]
        fn mask_holds_the_wanted_entries_and_their_indexes((case, wanted) in masked()) {
            let (set, _) = frame_of(&case);
            let entries = set.entries();
            let wants = |entry: usize| wanted[entry];
            let mask = Mask::new(&set, |slot| wants(set.find(slot).unwrap()));
            let expected: Vec<usize> = (0..=entries.len())
                .filter(|&entry| {
                    entry < entries.len()
                        && (wants(entry)
                            || (0..entries.len())
                                .any(|data| set.index(data) == entry && wants(data)))
                })
                .collect();
            prop_assert_eq!(held(&set, &mask), expected.clone());
            prop_assert_eq!(mask.is_empty(), expected.is_empty());
        }

        #[test]
        fn view_reads_the_held_present_series((case, wanted) in masked()) {
            let (set, frame) = frame_of(&case);
            let mask = Arc::new(Mask::new(&set, |slot| wanted[set.find(slot).unwrap()]));
            let view = View::new(frame.clone(), Arc::clone(&mask));
            let read: Vec<(usize, &[u8])> = view.iter().collect();
            let expected: Vec<(usize, &[u8])> =
                frame.iter().filter(|&(entry, _)| mask.contains(entry)).collect();
            prop_assert_eq!(read, expected);
        }

        /// The oracle for the charge: a frame that `Draft` makes of the view's series.
        #[test]
        fn view_charges_the_frame_of_its_series((case, wanted) in masked()) {
            let (set, frame) = frame_of(&case);
            let mask = Arc::new(Mask::new(&set, |slot| wanted[set.find(slot).unwrap()]));
            let view = View::new(frame.clone(), mask);
            let series: Vec<(usize, usize)> =
                view.iter().map(|(entry, bytes)| (entry, bytes.len())).collect();
            let pool = pool(1 << 20);
            let narrow = Draft::new(&pool, &set, frame.form(), &series)
                .map_err(|error| TestCaseError::fail(error.to_string()))?
                .freeze(frame.path());
            prop_assert_eq!(view.charge(), narrow.charge());
            if wanted.iter().all(|&wanted| wanted) {
                prop_assert_eq!(view.charge(), frame.charge());
            }
        }

        /// Wide and narrow lists, so the skips cross many records at once.
        #[test]
        fn join_gives_each_record_whose_key_is_held(
            records in btree_set(0_u32..3000, 0..400),
            keys in btree_set(0_u32..3000, 0..400),
        ) {
            let records: Vec<[u8; 8]> = records
                .iter()
                .map(|&key| {
                    let mut record = [0; 8];
                    record[..4].copy_from_slice(&key.to_le_bytes());
                    record
                })
                .collect();
            let keys: Vec<u32> = keys.into_iter().collect();
            let held: BTreeSet<u32> = keys.iter().copied().collect();
            let expected: Vec<usize> = (0..records.len())
                .filter(|&n| held.contains(&super::super::lead(&records[n])))
                .collect();
            prop_assert_eq!(join(&records, &keys).collect::<Vec<_>>(), expected);
        }
    }
}
