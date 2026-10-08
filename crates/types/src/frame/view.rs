//! Views: a frame and the entries that one reader wants (M2).

use std::cmp::Ordering;

use super::key_set::{self, KeySet};
use super::{
    DESCRIPTOR, Form, Frame, Path, Range, bounds, descriptor_ends, lead, parts, spans,
    to_u32, to_usize,
};
use crate::channel;

/// The entries of one key set that a reader wants, and the index of each. Made once
/// per key set and reader, and used by every view of that pair.
#[derive(Debug)]
pub struct Mask {
    set: key_set::Key,
    held: Held,
}

#[derive(Debug)]
enum Held {
    /// Every entry of a key set that has at least one.
    Every,
    /// Some of the entries, or none.
    Listed(Subset),
}

/// Sorted entries, and the sorted groups whose index is among them.
#[derive(Debug)]
struct Subset {
    entries: Box<[u32]>,
    groups: Box<[u32]>,
}

impl Subset {
    fn new(set: &KeySet, entries: Vec<u32>) -> Self {
        // Indexes in entry order are in group order (`KeySet::groups`).
        let groups = entries
            .iter()
            .map(|&entry| to_usize(entry))
            .filter(|&entry| set.index(entry) == entry)
            .map(|index| set.entries()[index].group)
            .collect();
        Self {
            entries: entries.into(),
            groups,
        }
    }
}

impl Mask {
    /// The entries of `set` with a slot in `wanted`, and the index of each, so the
    /// series of a view always make a frame. A slot that `set` does not hold is
    /// skipped. Time is O(m log(k + m)) for m slots in `wanted` and k entries in
    /// `set`.
    #[must_use]
    pub fn new(set: &KeySet, wanted: impl IntoIterator<Item = channel::Slot>) -> Self {
        Self::of_entries(set, wanted.into_iter().filter_map(|slot| set.find(slot)))
    }

    /// The `entries` of `set`, and the index of each, as [`Mask::new`] gives for their
    /// slots, with no search of `set`.
    pub(super) fn of_entries(
        set: &KeySet,
        entries: impl Iterator<Item = usize>,
    ) -> Self {
        let mut entries: Vec<u32> = entries
            .flat_map(|entry| [entry, set.index(entry)].map(to_u32))
            .collect();
        entries.sort_unstable();
        entries.dedup();
        let held = if !entries.is_empty() && entries.len() == set.entries().len() {
            Held::Every
        } else {
            Held::Listed(Subset::new(set, entries))
        };
        Self {
            set: set.key(),
            held,
        }
    }

    /// Whether the mask holds no entry: the reader wants nothing of the key set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(&self.held, Held::Listed(held) if held.entries.is_empty())
    }
}

/// What a reader gets: a frame and the entries it wants (M2). It reads as the frame of
/// only those series. Making one copies nothing and takes no reference count.
#[derive(Clone, Copy, Debug)]
pub struct View<'a> {
    frame: &'a Frame,
    mask: &'a Mask,
}

impl<'a> View<'a> {
    /// A view of `frame` through `mask`.
    ///
    /// # Panics
    ///
    /// If `mask` is of another key set than `frame`.
    #[must_use]
    pub fn new(frame: &'a Frame, mask: &'a Mask) -> Self {
        assert!(
            frame.key_set() == mask.set,
            "the frame is of key set {} and the mask of key set {}",
            frame.key_set().get(),
            mask.set.get()
        );
        Self { frame, mask }
    }

    /// The key set of the frame and the mask.
    #[must_use]
    pub fn key_set(&self) -> key_set::Key {
        self.mask.set
    }

    /// The frame's path.
    #[must_use]
    pub fn path(&self) -> Path {
        self.frame.path()
    }

    /// The frame's form.
    #[must_use]
    pub fn form(&self) -> Form {
        self.frame.form()
    }

    /// The range of `group`, or `None` when the frame or the mask does not hold the
    /// group's index. Time is logarithmic in the frame's ranges and the mask's groups.
    #[must_use]
    pub fn range(&self, group: u32) -> Option<Range> {
        match &self.mask.held {
            Held::Listed(held) if held.groups.binary_search(&group).is_err() => None,
            _ => self.frame.range(group),
        }
    }

    /// Each present entry that the mask holds and its series bytes, in entry order.
    /// Time is linear in the frame's series when the mask holds every entry, else
    /// O(m log(n/m)) for the smaller m and larger n of the frame's series and the
    /// mask's entries.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &'a [u8])> + use<'a> {
        let Held::Listed(held) = &self.mask.held else {
            return Series::Every(self.frame.iter());
        };
        let (_, descriptors, body) = parts(&self.frame.0);
        Series::Listed(Join::new(descriptors, &held.entries).map(move |n| {
            let (entry, range) = at(descriptors, n);
            (entry, &body[range])
        }))
    }

    /// Each present entry that the mask holds and the bytes of its series in
    /// [`Frame::body`], in the order of [`View::iter`]. The bounds are in the frame's
    /// body, not in a frame of only these series: [`split`](super::split) cannot cut
    /// with them. Time is as for [`View::iter`].
    pub(crate) fn bounds(
        &self,
    ) -> impl Iterator<Item = (usize, std::ops::Range<usize>)> + use<'a> {
        let (_, descriptors, _) = parts(&self.frame.0);
        match &self.mask.held {
            Held::Every => Series::Every(
                spans(descriptor_ends(descriptors))
                    .map(|(entry, start, end)| (entry, start..end)),
            ),
            Held::Listed(held) => Series::Listed(
                Join::new(descriptors, &held.entries).map(move |n| at(descriptors, n)),
            ),
        }
    }
}

/// The entry of descriptor `n` and the bounds of its series in the series bytes.
fn at(descriptors: &[[u8; DESCRIPTOR]], n: usize) -> (usize, std::ops::Range<usize>) {
    let (start, end) = bounds(descriptors, n);
    (to_usize(lead(&descriptors[n])), start..end)
}

/// The series of a view: those of the whole frame, or those of the listed entries.
enum Series<E, L> {
    Every(E),
    Listed(L),
}

impl<T, E: Iterator<Item = T>, L: Iterator<Item = T>> Iterator for Series<E, L> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        match self {
            Self::Every(series) => series.next(),
            Self::Listed(series) => series.next(),
        }
    }

    fn fold<B, F: FnMut(B, T) -> B>(self, init: B, f: F) -> B {
        match self {
            Self::Every(series) => series.fold(init, f),
            Self::Listed(series) => series.fold(init, f),
        }
    }
}

/// The position of each record in `records` that leads with a key in `keys`, in order.
/// Both are sorted. Each step skips ahead on both by [`gallop`], so time is
/// O(m log(n/m)) for the shorter m and longer n.
struct Join<'a, const N: usize> {
    records: &'a [[u8; N]],
    keys: &'a [u32],
    at: usize,
}

impl<'a, const N: usize> Join<'a, N> {
    fn new(records: &'a [[u8; N]], keys: &'a [u32]) -> Self {
        Self {
            records,
            keys,
            at: 0,
        }
    }
}

impl<const N: usize> Iterator for Join<'_, N> {
    type Item = usize;

    #[expect(
        clippy::inline_always,
        reason = "as a call, it made a narrow walk 1.5 to 2.2 times slower"
    )]
    #[inline(always)]
    fn next(&mut self) -> Option<usize> {
        loop {
            let key = lead(self.records.get(self.at)?);
            let &next = self.keys.first()?;
            match next.cmp(&key) {
                Ordering::Less => {
                    self.keys = &self.keys[gallop(self.keys, |&held| held < key)..];
                }
                Ordering::Greater => {
                    let records = &self.records[self.at..];
                    self.at += gallop(records, |record| lead(record) < next);
                }
                Ordering::Equal => {
                    self.keys = &self.keys[1..];
                    self.at += 1;
                    return Some(self.at - 1);
                }
            }
        }
    }
}

/// The first position in `items` where `before` is false, as `slice::partition_point`
/// gives it, in time logarithmic in that position.
pub(super) fn gallop<T>(items: &[T], mut before: impl FnMut(&T) -> bool) -> usize {
    let mut high = 1;
    while items.get(high).is_some_and(&mut before) {
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
    use crate::channel::Slot;
    use crate::frame::Draft;
    use crate::frame::key_set::Group;
    use crate::frame::tests::{Case, cases, frame_of, interner, key, pool, two_groups};

    /// The entries of `set` that `mask` holds.
    fn held(set: &KeySet, mask: &Mask) -> Vec<usize> {
        match &mask.held {
            Held::Every => (0..set.entries().len()).collect(),
            Held::Listed(held) => held.entries.iter().map(|&n| to_usize(n)).collect(),
        }
    }

    /// A frame of [`two_groups`] with all four series: index key 1 (entry 0) with 3
    /// bytes, key 2 (entry 1) with 10, index key 3 (entry 2) with 5, and key 4 (entry
    /// 3) with 1. Each byte is its entry. Key `n` has slot `n`.
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
        let mask = Mask::new(&set, [Slot::new(4)]);
        assert_eq!(held(&set, &mask), [2, 3]);
        assert!(!mask.is_empty());
    }

    #[test]
    fn holds_an_index_without_its_data() {
        let set = two_groups();
        assert_eq!(held(&set, &Mask::new(&set, [Slot::new(1)])), [0]);
    }

    #[test]
    fn wants_nothing_of_a_key_set_without_a_wanted_slot() {
        let set = two_groups();
        let mask = Mask::new(&set, [5, 6].map(Slot::new));
        assert!(mask.is_empty());
        assert_eq!(held(&set, &mask), Vec::<usize>::new());
    }

    #[test]
    fn wants_nothing_of_an_empty_key_set() {
        let set = interner().intern(&[]);
        assert!(Mask::new(&set, [Slot::new(1)]).is_empty());
    }

    #[test]
    fn holds_every_entry_without_a_list() {
        let set = two_groups();
        let mask = Mask::new(&set, [4, 2, 3, 1, 4, 9].map(Slot::new));
        assert!(matches!(mask.held, Held::Every));
        assert!(!mask.is_empty());
    }

    #[test]
    fn reads_as_the_frame_of_only_the_held_series() {
        let set = two_groups();
        let frame = full(&set);
        let mask = Mask::new(&set, [Slot::new(4)]);
        let view = View::new(&frame, &mask);
        let read: Vec<(usize, &[u8])> = view.iter().collect();
        assert_eq!(read, [(2, &[2; 5][..]), (3, &[3])]);
        assert_eq!(view.key_set(), set.key());
        assert_eq!(view.path(), Path::Live);
        assert_eq!(view.form(), Form::Encoded);
        assert_eq!(view.range(0), None);
        assert_eq!(view.range(1), frame.range(1));
        assert_eq!(view.range(1), Some(Range::default()));
    }

    #[test]
    fn bounds_each_held_series_in_the_body_of_the_frame() {
        let set = two_groups();
        let frame = full(&set);
        let bounds = |slots: &[u32]| {
            let mask = Mask::new(&set, slots.iter().map(|&slot| Slot::new(slot)));
            View::new(&frame, &mask).bounds().collect::<Vec<_>>()
        };
        assert_eq!(bounds(&[4]), [(2, 24..29), (3, 32..33)]);
        assert_eq!(
            bounds(&[1, 2, 3, 4]),
            [(0, 0..3), (1, 8..18), (2, 24..29), (3, 32..33)]
        );
        assert_eq!(bounds(&[]), []);
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
        let frame = draft.freeze(Path::Live);
        let mask = Mask::new(&first, [Slot::new(1)]);
        let _view = View::new(&frame, &mask);
    }

    #[test]
    fn a_view_goes_to_another_thread() {
        fn sendable<T: Send>() {}
        sendable::<View<'static>>();
    }

    #[test]
    fn gallop_gives_the_partition_point_in_logarithmic_probes() {
        for len in 0..70_usize {
            let items: Vec<usize> = (0..len).collect();
            for point in 0..=len {
                let mut probes = 0_u32;
                let found = gallop(&items, |&item| {
                    probes += 1;
                    item < point
                });
                assert_eq!(found, point, "len {len}");
                let bound = 2 * (usize::BITS - point.leading_zeros()) + 2;
                assert!(probes <= bound, "len {len}, point {point}: {probes} probes");
            }
        }
    }

    /// A case and whether its mask wants each entry, from none to all. One case in four
    /// wants every entry.
    fn masked() -> impl Strategy<Value = (Case, Vec<bool>)> {
        (cases(), prop_oneof![1 => Just(1.0), 3 => 0.0..=1.0_f64])
            .prop_flat_map(|(case, p)| (Just(case), vec(prop::bool::weighted(p), 164)))
    }

    /// The slots of the entries of `set` that `wanted` marks, last first and each
    /// twice, then a slot of no entry.
    fn slots(set: &KeySet, wanted: &[bool]) -> Vec<Slot> {
        let marked: Vec<Slot> = set
            .entries()
            .iter()
            .zip(wanted)
            .filter(|&(_, &wanted)| wanted)
            .map(|(entry, _)| entry.slot)
            .collect();
        let last_first = marked.iter().rev().chain(&marked);
        last_first.copied().chain([Slot::new(u32::MAX)]).collect()
    }

    proptest! {
        #[test]
        fn mask_holds_the_wanted_entries_and_their_indexes((case, wanted) in masked()) {
            let (set, _) = frame_of(&case);
            let entries = set.entries().len();
            let mask = Mask::new(&set, slots(&set, &wanted));
            let indexes_wanted = |index: usize| {
                (0..entries).any(|data| set.index(data) == index && wanted[data])
            };
            let expected: Vec<usize> = (0..entries)
                .filter(|&entry| wanted[entry] || indexes_wanted(entry))
                .collect();
            prop_assert_eq!(held(&set, &mask), expected.clone());
            prop_assert_eq!(mask.is_empty(), expected.is_empty());
            let every = !expected.is_empty() && expected.len() == entries;
            prop_assert_eq!(matches!(mask.held, Held::Every), every);
        }

        #[test]
        fn view_reads_as_the_frame_of_the_held_series((case, wanted) in masked()) {
            let (set, frame) = frame_of(&case);
            let mask = Mask::new(&set, slots(&set, &wanted));
            let view = View::new(&frame, &mask);
            let held = held(&set, &mask);
            let read: Vec<(usize, &[u8])> = view.iter().collect();
            let expected: Vec<(usize, &[u8])> =
                frame.iter().filter(|(entry, _)| held.contains(entry)).collect();
            prop_assert_eq!(&read, &expected);
            let folded = view.iter().fold(Vec::new(), |mut folded, series| {
                folded.push(series);
                folded
            });
            prop_assert_eq!(folded, expected);
            let body = frame.body();
            let cut: Vec<(usize, &[u8])> = view
                .bounds()
                .map(|(entry, bounds)| (entry, &body[bounds]))
                .collect();
            prop_assert_eq!(cut, read);
            for (group, &index) in (0..).zip(set.groups()) {
                let range = frame.range(group).filter(|_| held.contains(&index));
                prop_assert_eq!(view.range(group), range, "group {}", group);
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
                .filter(|&n| held.contains(&lead(&records[n])))
                .collect();
            prop_assert_eq!(Join::new(&records, &keys).collect::<Vec<_>>(), expected);
        }
    }
}
