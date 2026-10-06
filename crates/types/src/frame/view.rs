//! Views: a frame and the entries that one reader wants (M2).

use std::cmp::Ordering;
use std::iter;

use super::key_set::{self, KeySet};
use super::{
    Form, Frame, Path, Range, SERIES_ALIGN, body_start, bounds, charge_of, end_of,
    lead, next_end, parts, to_u32, to_usize,
};
use crate::channel;

/// The entries of one key set that a reader wants, and the index of each. Made once
/// per key set and reader, and used by every view of that pair.
#[derive(Debug)]
pub struct Mask {
    set: key_set::Key,
    held: Held,
}

/// The entries a mask holds. A list is the shorter side: at most half the key set.
#[derive(Debug)]
enum Held {
    /// Every entry of a key set that has at least one.
    Every,
    /// Only the listed entries.
    Only(List),
    /// Every entry but the listed ones, at least one.
    Except(List),
}

/// Sorted entries, and the sorted groups whose index is among them.
#[derive(Debug)]
struct List {
    entries: Box<[u32]>,
    groups: Box<[u32]>,
}

impl List {
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

    fn has_group(&self, group: u32) -> bool {
        self.groups.binary_search(&group).is_ok()
    }
}

impl Mask {
    /// The entries of `set` with a slot in `wanted`, and the index of each, so the
    /// series of a view always make a frame. A slot that `set` does not hold is
    /// skipped. Time is O(m log(k + m)) for m slots in `wanted` and k entries in
    /// `set`.
    #[must_use]
    pub fn new(set: &KeySet, wanted: impl IntoIterator<Item = channel::Slot>) -> Self {
        let mut entries: Vec<u32> = wanted
            .into_iter()
            .filter_map(|slot| set.find(slot))
            .flat_map(|entry| [entry, set.index(entry)].map(to_u32))
            .collect();
        entries.sort_unstable();
        entries.dedup();
        let len = set.entries().len();
        // The complement costs O(k), and k < 2 * entries.len() <= 4m.
        let held = if 2 * entries.len() <= len {
            Held::Only(List::new(set, entries))
        } else if entries.len() == len {
            Held::Every
        } else {
            let mut held = entries.into_iter().peekable();
            let others = (0..to_u32(len))
                .filter(|&entry| held.next_if_eq(&entry).is_none())
                .collect();
            Held::Except(List::new(set, others))
        };
        Self {
            set: set.key(),
            held,
        }
    }

    /// Whether the mask holds no entry: the reader wants nothing of the key set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(&self.held, Held::Only(list) if list.entries.is_empty())
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
        let held = match &self.mask.held {
            Held::Every => true,
            Held::Only(list) => list.has_group(group),
            Held::Except(list) => !list.has_group(group),
        };
        if held { self.frame.range(group) } else { None }
    }

    /// Each present entry that the mask holds and its series bytes, in entry order.
    /// Time is O(m log(n/m)) for the smaller m and larger n of the frame's series and
    /// the mask's entries when the mask holds at most half the key set, else linear in
    /// the frame's series.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &'a [u8])> + use<'a> {
        let list = match &self.mask.held {
            Held::Every => return Series::Every(self.frame.iter()),
            Held::Only(list) | Held::Except(list) => list,
        };
        let (_, descriptors, body) = parts(&self.frame.0);
        let series = move |n| {
            let (start, end) = bounds(descriptors, n);
            (to_usize(lead(&descriptors[n])), &body[start..end])
        };
        let mut listed = join(descriptors, &list.entries);
        if let Held::Only(_) = self.mask.held {
            return Series::Only(listed.map(series));
        }
        let mut next = Some(0);
        // The runs of series between those left out.
        let runs = iter::from_fn(move || {
            let first = next?;
            let end = listed.next();
            next = end.map(|end| end + 1);
            Some(first..end.unwrap_or(descriptors.len()))
        });
        Series::Except(runs.flatten().map(series))
    }

    /// The charge of a frame of only the view's series (CREDIT RULES): what a remote
    /// complete reader spends. Equal to [`Frame::charge`] when the mask holds every
    /// present series. Time is constant when the mask holds every entry, else
    /// O(m log(n/m)) for the smaller m and larger n of the frame's series and the
    /// mask's list, which has at most half the key set.
    #[must_use]
    pub fn charge(&self) -> u64 {
        charge_of(self.len())
    }

    /// The length of a frame of only the view's series.
    fn len(&self) -> usize {
        match &self.mask.held {
            Held::Every => self.frame.0.len(),
            Held::Only(list) => len_of_only(self.frame, list),
            Held::Except(list) => len_without(self.frame, list),
        }
    }
}

/// The length of a frame of only the series of `frame` that `list` holds.
fn len_of_only(frame: &Frame, list: &List) -> usize {
    let (ranges, descriptors, _) = parts(&frame.0);
    let groups = join(ranges, &list.groups).count();
    let (mut series, mut bytes) = (0, 0);
    for n in join(descriptors, &list.entries) {
        let (start, end) = bounds(descriptors, n);
        series += 1;
        bytes = next_end(bytes, end - start);
    }
    body_start(groups, series) + bytes
}

/// The length of `frame` without the series that `list` holds.
fn len_without(frame: &Frame, list: &List) -> usize {
    let (ranges, descriptors, _) = parts(&frame.0);
    let groups = join(ranges, &list.groups).count();
    let (mut series, mut cut, mut after) = (0, 0, 0);
    // The latest run of series left out: where it starts, and its part of `cut`.
    let (mut run, mut run_cut) = (0, 0);
    for n in join(descriptors, &list.entries) {
        let (start, end) = bounds(descriptors, n);
        let padded = end.next_multiple_of(SERIES_ALIGN) - start;
        if n != after {
            (run, run_cut) = (n, 0);
        }
        series += 1;
        cut += padded;
        run_cut += padded;
        after = n + 1;
    }
    // A frame that ends with series left out gives a view that ends at the last series
    // kept, without its padding.
    let (tail, tail_cut) = if after == descriptors.len() {
        (run, run_cut)
    } else {
        (descriptors.len(), 0)
    };
    let end = tail
        .checked_sub(1)
        .map_or(0, |last| end_of(descriptors[last]));
    body_start(ranges.len() - groups, descriptors.len() - series) + end
        - (cut - tail_cut)
}

/// The series of a view, as [`Held`] gives them.
enum Series<V, O, E> {
    Every(V),
    Only(O),
    Except(E),
}

impl<T, V, O, E> Iterator for Series<V, O, E>
where
    V: Iterator<Item = T>,
    O: Iterator<Item = T>,
    E: Iterator<Item = T>,
{
    type Item = T;

    fn next(&mut self) -> Option<T> {
        match self {
            Self::Every(series) => series.next(),
            Self::Only(series) => series.next(),
            Self::Except(series) => series.next(),
        }
    }

    fn fold<B, F: FnMut(B, T) -> B>(self, init: B, f: F) -> B {
        match self {
            Self::Every(series) => series.fold(init, f),
            Self::Only(series) => series.fold(init, f),
            Self::Except(series) => fold_out_of_line(series, init, f),
        }
    }
}

/// Folds `series` out of line. Inline, its state takes the caller's registers, and
/// the walk of a narrow view takes about 15% longer.
#[inline(never)]
fn fold_out_of_line<T, B>(
    series: impl Iterator<Item = T>,
    init: B,
    f: impl FnMut(B, T) -> B,
) -> B {
    series.fold(init, f)
}

/// The position of each record in `records` that leads with a key in `keys`, in order.
/// Both are sorted. Each step skips ahead on both by [`gallop`], so time is
/// O(m log(n/m)) for the shorter m and longer n.
fn join<'a, const N: usize>(
    records: &'a [[u8; N]],
    mut keys: &'a [u32],
) -> impl Iterator<Item = usize> + 'a {
    let mut at = 0;
    iter::from_fn(
        // Without it, the walk of a narrow view takes about 40% longer.
        #[inline(always)]
        move || {
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
        },
    )
}

/// The first position in `items` where `before` is false, as `slice::partition_point`
/// gives it, in time logarithmic in that position.
fn gallop<T>(items: &[T], mut before: impl FnMut(&T) -> bool) -> usize {
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
    use crate::sample::{Scalar, Type};

    /// The entries of `set` that `mask` holds.
    fn held(set: &KeySet, mask: &Mask) -> Vec<usize> {
        let is_listed = |entry: &usize| listed(mask).contains(&to_u32(*entry));
        let only = matches!(mask.held, Held::Only(_));
        (0..set.entries().len())
            .filter(|entry| is_listed(entry) == only)
            .collect()
    }

    /// The entries that `mask` lists.
    fn listed(mask: &Mask) -> &[u32] {
        match &mask.held {
            Held::Every => &[],
            Held::Only(list) | Held::Except(list) => &list.entries,
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
    fn lists_the_entries_that_a_mask_of_most_leaves_out() {
        let set = two_groups();
        let mask = Mask::new(&set, [1, 3, 4].map(Slot::new));
        assert!(matches!(mask.held, Held::Except(_)));
        assert_eq!(listed(&mask), [1]);
        assert_eq!(held(&set, &mask), [0, 2, 3]);
    }

    #[test]
    fn lists_the_entries_held_when_they_are_half() {
        let set = two_groups();
        let mask = Mask::new(&set, [Slot::new(2)]);
        assert!(matches!(mask.held, Held::Only(_)));
        assert_eq!(listed(&mask), [0, 1]);
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

    /// The charge of a frame of `len` bytes.
    fn charge(len: usize) -> u64 {
        u64::try_from(block::footprint(len)).unwrap()
    }

    /// One range, two descriptors, and 5 bytes, 3 of padding, and 1 byte of series.
    #[test]
    fn charges_a_frame_of_only_its_series() {
        let set = two_groups();
        let frame = full(&set);
        let mask = Mask::new(&set, [Slot::new(4)]);
        let view = View::new(&frame, &mask);
        assert_eq!(view.len(), 16 + 16 + 2 * 8 + 8 + 1);
        assert_eq!(view.charge(), charge(view.len()));
    }

    /// Two ranges, three descriptors, and 3 bytes and 5 of padding, 5 bytes and 3 of
    /// padding, and 1 byte.
    #[test]
    fn charges_a_view_that_leaves_out_a_middle_series() {
        let set = two_groups();
        let frame = full(&set);
        let mask = Mask::new(&set, [1, 3, 4].map(Slot::new));
        let view = View::new(&frame, &mask);
        assert_eq!(view.len(), 89);
        assert_eq!(view.charge(), charge(89));
    }

    /// Two ranges, three descriptors, and 3 bytes and 5 of padding, 10 bytes and 6 of
    /// padding, and 5 bytes.
    #[test]
    fn charges_a_view_that_leaves_out_the_last_series() {
        let set = two_groups();
        let frame = full(&set);
        let mask = Mask::new(&set, [1, 2, 3].map(Slot::new));
        assert_eq!(listed(&mask), [3]);
        let view = View::new(&frame, &mask);
        assert_eq!(view.len(), 101);
        assert_eq!(view.charge(), charge(101));
    }

    /// Three groups: index 1 with 2 and 3, index 4 with 5, and index 6 alone. The view
    /// leaves out the third group, and its frame fills a size class to the byte.
    #[test]
    fn charges_a_view_that_leaves_out_a_present_group() {
        const F64: Type = Type::Scalar(Scalar::F64);
        let set = interner().intern(&[
            Group {
                index: key(1),
                data: &[(key(2), F64), (key(3), F64)],
            },
            Group {
                index: key(4),
                data: &[(key(5), F64)],
            },
            Group {
                index: key(6),
                data: &[],
            },
        ]);
        let series = [(0, 8), (1, 8), (2, 8), (3, 8), (4, 136), (5, 8)];
        let frame = |series| {
            let draft = Draft::new(&pool(1 << 16), &set, Form::Raw, series).unwrap();
            draft.freeze(Path::Live)
        };
        let (whole, narrow) = (frame(&series), frame(&series[..5]));
        let mask = Mask::new(&set, [2, 3, 5].map(Slot::new));
        assert_eq!(listed(&mask), [5]);
        let view = View::new(&whole, &mask);
        assert_eq!(view.len(), 256);
        assert_eq!(narrow.0.len(), 256);
        assert_eq!(view.charge(), charge(256));
        assert_ne!(view.charge(), whole.charge());
    }

    #[test]
    fn charges_the_whole_frame_through_a_full_mask() {
        let set = two_groups();
        let frame = full(&set);
        let mask = Mask::new(&set, [1, 2, 3, 4].map(Slot::new));
        assert_eq!(View::new(&frame, &mask).charge(), frame.charge());
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
            prop_assert!(2 * listed(&mask).len() <= entries);
            let only = 2 * expected.len() <= entries;
            prop_assert_eq!(matches!(mask.held, Held::Only(_)), only);
            let every = !only && expected.len() == entries;
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
            for (group, &index) in (0..).zip(set.groups()) {
                let range = frame.range(group).filter(|_| held.contains(&index));
                prop_assert_eq!(view.range(group), range, "group {}", group);
            }
        }

        /// The oracle for the charge: a frame that `Draft` makes of the view's series.
        #[test]
        fn view_charges_the_frame_of_its_series((case, wanted) in masked()) {
            let (set, frame) = frame_of(&case);
            let mask = Mask::new(&set, slots(&set, &wanted));
            let view = View::new(&frame, &mask);
            let series: Vec<(usize, usize)> =
                view.iter().map(|(entry, bytes)| (entry, bytes.len())).collect();
            let pool = pool(1 << 20);
            let narrow = Draft::new(&pool, &set, frame.form(), &series)
                .map_err(|error| TestCaseError::fail(error.to_string()))?
                .freeze(frame.path());
            prop_assert_eq!(view.len(), narrow.0.len());
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
                .filter(|&n| held.contains(&lead(&records[n])))
                .collect();
            prop_assert_eq!(join(&records, &keys).collect::<Vec<_>>(), expected);
        }
    }
}
