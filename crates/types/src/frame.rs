//! Frames and the key sets they point at.
//!
//! A frame is one pool block: a header, the samples of each present index group, a
//! presence mask over its key set's entries, one descriptor per present series, and the
//! series bytes. It holds offsets, never pointers.

pub mod key_set;

use std::iter;

use key_set::KeySet;

/// Bytes of the header: key set key, entry count, group count, present group count (each
/// `u32`), then form and path (each `u8`), then zeros.
const HEAD: usize = 24;
/// Bytes of one mask word.
const WORD: usize = 8;
/// Bytes of one group's range: seq (`u64`), count (`u32`), then zeros.
const RANGE: usize = 16;
/// Bytes of one descriptor: offset and length (each `u32`).
const DESCRIPTOR: usize = 8;
/// Each series starts at a multiple of this.
const SERIES_ALIGN: usize = 8;

/// One of an index's two write paths, each with its own seq. Backfill is late data,
/// labeled by the writer, that live readers never see.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Path {
    /// The newest data.
    Live,
    /// Late data, labeled by the writer. It ends before the newest live sample.
    Backfill,
}

/// How a frame's series hold their samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Form {
    /// As a connector writes them: fixed-width values back to back, or for strings,
    /// bytes, and lists, `u32` end offsets and then the data.
    Raw,
    /// As `codec` encodes them, in tagged vectors.
    Encoded,
}

/// The samples of one index group in a frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Range {
    /// The seq of the first sample. The home sets it.
    pub seq: u64,
    /// How many samples each series of the group holds.
    pub count: u32,
}

/// A frame being written, in a block that it alone holds.
#[derive(Debug)]
pub struct Draft(block::Unique);

impl Draft {
    /// Takes a block from `pool` for a frame of `set` on `path`, and writes its header
    /// and descriptors. `series` holds each present entry and the byte length of its
    /// series, in entry order. A group is present when its index is. Each present
    /// group's range starts at zero, and series bytes are not cleared.
    ///
    /// # Errors
    ///
    /// The pool's error when it has no block that large.
    ///
    /// # Panics
    ///
    /// If an entry is out of range or out of order, a present entry's index is absent,
    /// or the frame would be longer than `u32::MAX` bytes.
    pub fn new(
        pool: &block::Pool,
        set: &KeySet,
        path: Path,
        form: Form,
        series: &[(usize, usize)],
    ) -> Result<Self, block::Error> {
        check(set, series);
        let group_of = |entry: usize| to_usize(set.entries()[entry].group);
        let indexes = series
            .iter()
            .map(|&(entry, _)| entry)
            .filter(|&entry| set.groups()[group_of(entry)] == entry);
        let layout = Layout {
            entries: set.entries().len(),
            groups: set.groups().len(),
            present: indexes.clone().count(),
        };
        let start = layout.descriptors() + DESCRIPTOR * series.len();
        let len = series
            .iter()
            .try_fold(start, |end, &(_, len)| {
                end.checked_next_multiple_of(SERIES_ALIGN)?.checked_add(len)
            })
            .filter(|&len| u32::try_from(len).is_ok())
            .expect("a frame is at most u32::MAX bytes");
        let mut block = pool.alloc(len)?;
        block[..start].fill(0);
        put(&mut block, 0, &set.key().get().to_le_bytes());
        for (at, n) in [
            (4, layout.entries),
            (8, layout.groups),
            (12, layout.present),
        ] {
            put(&mut block, at, &to_u32(n).to_le_bytes());
        }
        let form = match form {
            Form::Raw => 0,
            Form::Encoded => 1,
        };
        let path = match path {
            Path::Live => 0,
            Path::Backfill => 1,
        };
        put(&mut block, 16, &[form, path]);
        for index in indexes {
            set_bit(&mut block, HEAD, group_of(index));
        }
        let mut end = start;
        for (n, &(entry, len)) in series.iter().enumerate() {
            set_bit(&mut block, layout.mask(), entry);
            let offset = end.next_multiple_of(SERIES_ALIGN);
            block[end..offset].fill(0);
            let at = layout.descriptors() + DESCRIPTOR * n;
            put(&mut block, at, &to_u32(offset).to_le_bytes());
            put(&mut block, at + 4, &to_u32(len).to_le_bytes());
            end = offset + len;
        }
        Ok(Self(block))
    }

    /// The bytes of `entry`'s series, to fill. `None` when the entry is absent.
    ///
    /// # Panics
    ///
    /// If `entry` is out of range.
    pub fn series(&mut self, entry: usize) -> Option<&mut [u8]> {
        let (offset, len) = locate(&self.0, entry)?;
        Some(&mut self.0[offset..offset + len])
    }

    /// Sets the samples of group `group`.
    ///
    /// # Panics
    ///
    /// If `group` is out of range or absent.
    pub fn set_range(&mut self, group: u32, range: Range) {
        let Some(at) = range_at(&self.0, group) else {
            panic!("group {group} is absent from the frame");
        };
        put(&mut self.0, at, &range.seq.to_le_bytes());
        put(&mut self.0, at + 8, &range.count.to_le_bytes());
    }

    /// The finished frame.
    #[must_use]
    pub fn freeze(self) -> Frame {
        Frame(self.0.freeze())
    }
}

/// An immutable frame in one pool block. Cloning it adds one reference.
#[derive(Clone, Debug)]
pub struct Frame(block::Block);

impl Frame {
    /// The key set the frame's entries number into.
    #[must_use]
    pub fn key_set(&self) -> key_set::Key {
        key_set::Key::new(u32::from_le_bytes(get(&self.0, 0)))
    }

    /// The frame's write path.
    #[must_use]
    pub fn path(&self) -> Path {
        match self.0[17] {
            0 => Path::Live,
            1 => Path::Backfill,
            other => unreachable!("invariant: only a draft writes a path, not {other}"),
        }
    }

    /// How the frame's series hold their samples.
    #[must_use]
    pub fn form(&self) -> Form {
        match self.0[16] {
            0 => Form::Raw,
            1 => Form::Encoded,
            other => unreachable!("invariant: only a draft writes a form, not {other}"),
        }
    }

    /// Whether entry `entry` of the frame's key set has a series.
    ///
    /// # Panics
    ///
    /// If `entry` is out of range.
    #[must_use]
    pub fn present(&self, entry: usize) -> bool {
        let layout = Layout::read(&self.0);
        layout.check(entry);
        self.0[layout.mask() + entry / 8] >> (entry % 8) & 1 == 1
    }

    /// The samples of group `group`, or `None` when its index is absent.
    ///
    /// # Panics
    ///
    /// If `group` is out of range.
    #[must_use]
    pub fn range(&self, group: u32) -> Option<Range> {
        let at = range_at(&self.0, group)?;
        Some(Range {
            seq: u64::from_le_bytes(get(&self.0, at)),
            count: u32::from_le_bytes(get(&self.0, at + 8)),
        })
    }

    /// The series bytes of `entry`, or `None` when it is absent. Time is linear in
    /// `entry / 64`.
    ///
    /// # Panics
    ///
    /// If `entry` is out of range.
    #[must_use]
    pub fn series(&self, entry: usize) -> Option<&[u8]> {
        let (offset, len) = locate(&self.0, entry)?;
        Some(&self.0[offset..offset + len])
    }

    /// Each present entry and its series bytes, in entry order.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &[u8])> {
        let bytes: &[u8] = &self.0;
        let layout = Layout::read(bytes);
        let entries = (0..layout.entries.div_ceil(64)).flat_map(move |n| {
            let mut word = u64::from_le_bytes(get(bytes, layout.mask() + WORD * n));
            iter::from_fn(move || {
                let bit = word.trailing_zeros();
                word &= word.checked_sub(1)?;
                Some(n * 64 + to_usize(bit))
            })
        });
        entries.enumerate().map(move |(n, entry)| {
            let (offset, len) = descriptor(bytes, layout, n);
            (entry, &bytes[offset..offset + len])
        })
    }
}

/// Byte offsets of a frame's parts, from the counts in its header. The group mask
/// starts at [`HEAD`].
#[derive(Clone, Copy)]
struct Layout {
    entries: usize,
    groups: usize,
    /// Present groups.
    present: usize,
}

impl Layout {
    fn read(bytes: &[u8]) -> Self {
        let count = |at| to_usize(u32::from_le_bytes(get(bytes, at)));
        Self {
            entries: count(4),
            groups: count(8),
            present: count(12),
        }
    }

    const fn ranges(self) -> usize {
        HEAD + WORD * self.groups.div_ceil(64)
    }

    const fn mask(self) -> usize {
        self.ranges() + RANGE * self.present
    }

    const fn descriptors(self) -> usize {
        self.mask() + WORD * self.entries.div_ceil(64)
    }

    fn check(self, entry: usize) {
        assert!(
            entry < self.entries,
            "entry {entry} is out of range for a frame of {} entries",
            self.entries
        );
    }
}

/// Panics unless each entry of `series` is in range, in increasing order, and has its
/// index present.
fn check(set: &KeySet, series: &[(usize, usize)]) {
    let entries = set.entries();
    let mut last = None;
    for &(entry, _) in series {
        assert!(
            entry < entries.len(),
            "entry {entry} is out of range for a key set of {} entries",
            entries.len()
        );
        if let Some(last) = last {
            assert!(
                entry > last,
                "entry {entry} follows entry {last}: entries must increase"
            );
        }
        last = Some(entry);
    }
    for &(entry, _) in series {
        let index = set.groups()[to_usize(entries[entry].group)];
        let present = series.binary_search_by_key(&index, |&(e, _)| e).is_ok();
        assert!(
            present,
            "entry {entry} is present but its index, entry {index}, is absent"
        );
    }
}

/// The offset and length of `entry`'s series, or `None` when it is absent.
fn locate(bytes: &[u8], entry: usize) -> Option<(usize, usize)> {
    let layout = Layout::read(bytes);
    layout.check(entry);
    let n = rank(bytes, layout.mask(), entry)?;
    Some(descriptor(bytes, layout, n))
}

/// The offset of `group`'s range, or `None` when the group is absent.
fn range_at(bytes: &[u8], group: u32) -> Option<usize> {
    let layout = Layout::read(bytes);
    let group = to_usize(group);
    assert!(
        group < layout.groups,
        "group {group} is out of range for a frame of {} groups",
        layout.groups
    );
    Some(layout.ranges() + RANGE * rank(bytes, HEAD, group)?)
}

/// The offset and length in descriptor `n`.
fn descriptor(bytes: &[u8], layout: Layout, n: usize) -> (usize, usize) {
    let at = layout.descriptors() + DESCRIPTOR * n;
    let field = |at| to_usize(u32::from_le_bytes(get(bytes, at)));
    (field(at), field(at + 4))
}

/// How many bits before `bit` are set in the mask at `at`, or `None` when `bit` is
/// clear. Time is linear in `bit / 64`.
fn rank(bytes: &[u8], at: usize, bit: usize) -> Option<usize> {
    let word = |n| u64::from_le_bytes(get(bytes, at + WORD * n));
    let (n, shift) = (bit / 64, bit % 64);
    let last = word(n);
    if last >> shift & 1 == 0 {
        return None;
    }
    let before: u32 = (0..n).map(|n| word(n).count_ones()).sum();
    let below = (last & ((1 << shift) - 1)).count_ones();
    Some(to_usize(before) + to_usize(below))
}

fn set_bit(bytes: &mut [u8], at: usize, bit: usize) {
    bytes[at + bit / 8] |= 1 << (bit % 8);
}

fn get<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    *bytes[at..]
        .first_chunk()
        .expect("invariant: the layout keeps each field inside the frame")
}

fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
    bytes[at..at + value.len()].copy_from_slice(value);
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).expect("invariant: a usize holds a u32")
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).expect("invariant: a frame is at most u32::MAX bytes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Slot;
    use crate::sample::{Scalar, Type};
    use key_set::{Group, Interner};
    use proptest::collection::vec;
    use proptest::prelude::*;

    const F64: Type = Type::Scalar(Scalar::F64);
    const U8: Type = Type::Scalar(Scalar::U8);

    fn slot(n: u32) -> Slot {
        Slot::new(n)
    }

    fn pool(budget: usize) -> block::Pool {
        let config = block::Config { budget };
        let memory = block::Heap::new(config.reservation());
        block::Pool::new(config, memory)
    }

    /// One group: index slot 1 (entry 0), then slots 2 and 3 (entries 1 and 2).
    fn one_group() -> std::sync::Arc<KeySet> {
        Interner::new().intern(&[Group {
            index: slot(1),
            data: &[(slot(2), F64), (slot(3), U8)],
        }])
    }

    /// Two groups: index slot 1 with slot 2, and index slot 3 with slot 4.
    fn two_groups() -> std::sync::Arc<KeySet> {
        Interner::new().intern(&[
            Group {
                index: slot(1),
                data: &[(slot(2), F64)],
            },
            Group {
                index: slot(3),
                data: &[(slot(4), F64)],
            },
        ])
    }

    #[test]
    fn lays_out_a_frame_byte_for_byte() {
        let set = one_group();
        let pool = pool(1 << 16);
        let mut dirty = pool.alloc(82).unwrap();
        dirty.fill(0xff);
        drop(dirty);
        let series = [(0, 3), (2, 2)];
        let mut draft =
            Draft::new(&pool, &set, Path::Backfill, Form::Encoded, &series).unwrap();
        draft.series(0).unwrap().copy_from_slice(&[0xaa; 3]);
        draft.series(2).unwrap().copy_from_slice(&[1, 2]);
        draft.set_range(0, Range { seq: 7, count: 2 });
        let frame = draft.freeze();
        let mut expected = Vec::new();
        for n in [0_u32, 3, 1, 1] {
            expected.extend(n.to_le_bytes());
        }
        expected.extend([1, 1, 0, 0, 0, 0, 0, 0]);
        expected.extend(1_u64.to_le_bytes());
        expected.extend(7_u64.to_le_bytes());
        expected.extend(2_u32.to_le_bytes());
        expected.extend([0; 4]);
        expected.extend(0b101_u64.to_le_bytes());
        for n in [72_u32, 3, 80, 2] {
            expected.extend(n.to_le_bytes());
        }
        expected.extend([0xaa, 0xaa, 0xaa, 0, 0, 0, 0, 0, 1, 2]);
        assert_eq!(&*frame.0, expected.as_slice());
    }

    #[test]
    fn reads_the_header_and_absent_parts() {
        let set = two_groups();
        let pool = pool(1 << 16);
        let draft = Draft::new(&pool, &set, Path::Live, Form::Raw, &[(0, 8)]).unwrap();
        let frame = draft.freeze();
        assert_eq!(frame.key_set(), set.key());
        assert_eq!(frame.path(), Path::Live);
        assert_eq!(frame.form(), Form::Raw);
        assert_eq!(frame.range(0), Some(Range::default()));
        assert_eq!(frame.range(1), None);
        assert!(frame.present(0));
        assert!(!frame.present(1));
        assert_eq!(frame.series(1), None);
        assert_eq!(
            frame.iter().map(|(entry, _)| entry).collect::<Vec<_>>(),
            [0]
        );
    }

    #[test]
    fn reads_groups_past_the_first_mask_word() {
        let groups: Vec<Group<'_>> = (0..130)
            .map(|n| Group {
                index: slot(n),
                data: &[],
            })
            .collect();
        let set = Interner::new().intern(&groups);
        let pool = pool(1 << 16);
        let series = [(0, 8), (64, 8), (129, 8)];
        let mut draft =
            Draft::new(&pool, &set, Path::Live, Form::Raw, &series).unwrap();
        for (seq, &(entry, _)) in (1..).zip(&series) {
            draft
                .series(entry)
                .unwrap()
                .fill(u8::try_from(seq).unwrap());
            let group = u32::try_from(entry).unwrap();
            draft.set_range(group, Range { seq, count: 1 });
        }
        let frame = draft.freeze();
        assert_eq!(frame.range(64), Some(Range { seq: 2, count: 1 }));
        assert_eq!(frame.range(129), Some(Range { seq: 3, count: 1 }));
        assert_eq!(frame.range(128), None);
        assert_eq!(frame.series(129), Some([3; 8].as_slice()));
    }

    #[test]
    fn returns_the_pool_error() {
        let set = one_group();
        let pool = pool(512);
        let result = Draft::new(&pool, &set, Path::Live, Form::Raw, &[(0, 1000)]);
        let expected = block::Error::TooLarge {
            requested: 1064,
            largest: pool.largest(),
        };
        assert_eq!(result.unwrap_err(), expected);
    }

    #[test]
    #[should_panic(expected = "entry 3 is out of range for a key set of 3 entries")]
    fn refuses_an_entry_out_of_range() {
        drop(Draft::new(
            &pool(1 << 16),
            &one_group(),
            Path::Live,
            Form::Raw,
            &[(3, 1)],
        ));
    }

    #[test]
    #[should_panic(expected = "entry 0 follows entry 2: entries must increase")]
    fn refuses_entries_out_of_order() {
        let series = [(2, 1), (0, 1)];
        drop(Draft::new(
            &pool(1 << 16),
            &one_group(),
            Path::Live,
            Form::Raw,
            &series,
        ));
    }

    #[test]
    #[should_panic(expected = "entry 0 follows entry 0: entries must increase")]
    fn refuses_an_entry_twice() {
        let series = [(0, 1), (0, 1)];
        drop(Draft::new(
            &pool(1 << 16),
            &one_group(),
            Path::Live,
            Form::Raw,
            &series,
        ));
    }

    #[test]
    #[should_panic(expected = "entry 2 is present but its index, entry 0, is absent")]
    fn refuses_data_without_its_index() {
        drop(Draft::new(
            &pool(1 << 16),
            &one_group(),
            Path::Live,
            Form::Raw,
            &[(2, 1)],
        ));
    }

    #[test]
    #[should_panic(expected = "a frame is at most u32::MAX bytes")]
    fn refuses_a_frame_longer_than_u32_max() {
        let set = Interner::new().intern(&[Group {
            index: slot(1),
            data: &[],
        }]);
        let series = [(0, usize::try_from(u32::MAX).unwrap() - 64 + 1)];
        drop(Draft::new(
            &pool(1 << 16),
            &set,
            Path::Live,
            Form::Raw,
            &series,
        ));
    }

    #[test]
    #[should_panic(expected = "a frame is at most u32::MAX bytes")]
    fn refuses_series_whose_lengths_overflow() {
        let series = [(0, usize::MAX), (2, usize::MAX)];
        drop(Draft::new(
            &pool(1 << 16),
            &one_group(),
            Path::Live,
            Form::Raw,
            &series,
        ));
    }

    #[test]
    fn holds_a_frame_of_exactly_u32_max_bytes_in_its_layout() {
        let set = Interner::new().intern(&[Group {
            index: slot(1),
            data: &[],
        }]);
        let series = [(0, usize::try_from(u32::MAX).unwrap() - 64)];
        let result = Draft::new(&pool(1 << 16), &set, Path::Live, Form::Raw, &series);
        let expected = block::Error::TooLarge {
            requested: usize::try_from(u32::MAX).unwrap(),
            largest: pool(1 << 16).largest(),
        };
        assert_eq!(result.unwrap_err(), expected);
    }

    #[test]
    #[should_panic(expected = "group 1 is absent from the frame")]
    fn refuses_a_range_for_an_absent_group() {
        let pool = pool(1 << 16);
        let series = [(0, 1)];
        let mut draft =
            Draft::new(&pool, &two_groups(), Path::Live, Form::Raw, &series).unwrap();
        draft.set_range(1, Range::default());
    }

    #[test]
    #[should_panic(expected = "group 2 is out of range for a frame of 2 groups")]
    fn refuses_a_group_out_of_range() {
        let pool = pool(1 << 16);
        let draft =
            Draft::new(&pool, &two_groups(), Path::Live, Form::Raw, &[]).unwrap();
        assert!(draft.freeze().range(2).is_none());
    }

    #[test]
    #[should_panic(expected = "entry 4 is out of range for a frame of 4 entries")]
    fn refuses_a_series_out_of_range() {
        let pool = pool(1 << 16);
        let draft =
            Draft::new(&pool, &two_groups(), Path::Live, Form::Raw, &[]).unwrap();
        assert!(draft.freeze().series(4).is_none());
    }

    #[test]
    #[should_panic(expected = "entry 4 is out of range for a frame of 4 entries")]
    fn refuses_presence_out_of_range() {
        let pool = pool(1 << 16);
        let draft =
            Draft::new(&pool, &two_groups(), Path::Live, Form::Raw, &[]).unwrap();
        assert!(!draft.freeze().present(4));
    }

    #[test]
    #[should_panic(expected = "entry 3 is out of range for a frame of 3 entries")]
    fn refuses_a_draft_series_out_of_range() {
        let pool = pool(1 << 16);
        let mut draft =
            Draft::new(&pool, &one_group(), Path::Live, Form::Raw, &[]).unwrap();
        assert!(draft.series(3).is_none());
    }

    #[derive(Clone, Debug)]
    struct Case {
        /// Data channels per group.
        data: Vec<usize>,
        /// Whether each group is present.
        groups: Vec<bool>,
        /// Whether each data entry is present, by entry position.
        present: Vec<bool>,
        /// Each entry's series length, by entry position.
        lens: Vec<usize>,
        ranges: Vec<(u64, u32)>,
        path: Path,
        form: Form,
    }

    /// Up to 4 groups of up to 40 data channels, so a mask can span 3 words.
    fn cases() -> impl Strategy<Value = Case> {
        (1_usize..5)
            .prop_flat_map(|n| {
                (
                    vec(0_usize..41, n),
                    vec(any::<bool>(), n),
                    vec(any::<bool>(), n * 41),
                    vec(0_usize..40, n * 41),
                    vec(any::<(u64, u32)>(), n),
                    prop_oneof![Just(Path::Live), Just(Path::Backfill)],
                    prop_oneof![Just(Form::Raw), Just(Form::Encoded)],
                )
            })
            .prop_map(|(data, groups, present, lens, ranges, path, form)| Case {
                data,
                groups,
                present,
                lens,
                ranges,
                path,
                form,
            })
    }

    fn pattern(entry: usize, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| u8::try_from((entry * 31 + i) % 251).unwrap())
            .collect()
    }

    proptest! {
        #[test]
        fn reads_back_what_a_draft_wrote(case in cases()) {
            let data: Vec<Vec<(Slot, Type)>> = (0..case.data.len())
                .map(|g| {
                    let base = u32::try_from(g * 100).unwrap();
                    let count = u32::try_from(case.data[g]).unwrap();
                    (1..=count).map(|j| (slot(base + j), F64)).collect()
                })
                .collect();
            let groups: Vec<Group<'_>> = data
                .iter()
                .enumerate()
                .map(|(g, data)| Group {
                    index: slot(u32::try_from(g * 100).unwrap()),
                    data,
                })
                .collect();
            let set = Interner::new().intern(&groups);
            let group_of = |entry: usize| usize::try_from(set.entries()[entry].group).unwrap();
            let series: Vec<(usize, usize)> = (0..set.entries().len())
                .filter(|&entry| {
                    let group = group_of(entry);
                    let index = set.groups()[group] == entry;
                    case.groups[group] && (index || case.present[entry])
                })
                .map(|entry| (entry, case.lens[entry]))
                .collect();

            let pool = pool(1 << 20);
            let mut draft = Draft::new(&pool, &set, case.path, case.form, &series).unwrap();
            for entry in 0..set.entries().len() {
                let len = series.iter().find(|&&(e, _)| e == entry).map(|&(_, len)| len);
                match (draft.series(entry), len) {
                    (Some(bytes), Some(len)) => bytes.copy_from_slice(&pattern(entry, len)),
                    (None, None) => {}
                    (bytes, len) => prop_assert!(false, "entry {entry}: {bytes:?}, {len:?}"),
                }
            }
            let mut ranges = Vec::new();
            for (group, &(seq, count)) in case.ranges.iter().enumerate() {
                let range = Range { seq, count };
                let number = u32::try_from(group).unwrap();
                if case.groups[group] {
                    draft.set_range(number, range);
                    ranges.push(Some(range));
                } else {
                    ranges.push(None);
                }
            }
            let frame = draft.freeze();

            prop_assert_eq!(frame.key_set(), set.key());
            prop_assert_eq!(frame.path(), case.path);
            prop_assert_eq!(frame.form(), case.form);
            for (group, range) in ranges.iter().enumerate() {
                prop_assert_eq!(frame.range(u32::try_from(group).unwrap()), *range);
            }
            for entry in 0..set.entries().len() {
                let expected = series
                    .iter()
                    .find(|&&(e, _)| e == entry)
                    .map(|&(_, len)| pattern(entry, len));
                prop_assert_eq!(frame.present(entry), expected.is_some());
                prop_assert_eq!(frame.series(entry).map(<[u8]>::to_vec), expected);
            }
            let read: Vec<(usize, Vec<u8>)> =
                frame.iter().map(|(entry, bytes)| (entry, bytes.to_vec())).collect();
            let written: Vec<(usize, Vec<u8>)> =
                series.iter().map(|&(entry, len)| (entry, pattern(entry, len))).collect();
            prop_assert_eq!(read, written);
        }
    }
}
