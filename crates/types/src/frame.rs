//! Frames and the key sets they point at.
//!
//! A frame is one pool block: a header, a range for each present index group, a
//! descriptor for each present series, and the series bytes. It holds offsets, never
//! pointers.

pub mod key_set;

use std::{fmt, iter, mem};

use key_set::KeySet;

/// Bytes of the header.
const HEAD: usize = 16;
/// Bytes of one range: group and count (each `u32`), then seq (`u64`).
const RANGE: usize = 16;
/// Bytes of one descriptor: entry and the end of its series in the series bytes (each
/// `u32`).
const DESCRIPTOR: usize = 8;
/// Each series starts at a multiple of this.
const SERIES_ALIGN: usize = 8;

/// Offsets of the header's fields: the key set key and the counts of ranges and
/// descriptors (each `u32`), then form and path (each `u8`), then zeros.
mod at {
    pub(super) const KEY_SET: usize = 0;
    pub(super) const RANGES: usize = 4;
    pub(super) const SERIES: usize = 8;
    pub(super) const FORM: usize = 12;
    pub(super) const PATH: usize = 13;

    /// Offsets of a range's fields after its group, which is at 0.
    pub(super) mod range {
        pub(in crate::frame) const COUNT: usize = 4;
        pub(in crate::frame) const SEQ: usize = 8;
    }
}

/// One of an index's two write paths, each with its own seq. Backfill is late data
/// that live readers never see.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Path {
    /// The newest data.
    Live,
    /// Late data. It ends before the newest live sample.
    Backfill,
}

impl Path {
    const fn byte(self) -> u8 {
        match self {
            Self::Live => 0,
            Self::Backfill => 1,
        }
    }

    fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::Live,
            1 => Self::Backfill,
            other => unreachable!("invariant: only a draft writes a path, not {other}"),
        }
    }
}

/// What a writer says a write holds. The home applies a write labeled with a path to
/// that path, and checks a resend against both paths first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Label {
    /// Data the writer has not sent before, for this path.
    Path(Path),
    /// A frame that the writer sends again after a reconnect, with its original
    /// boundaries. Each of its indexes lands on one path or on none.
    Resend,
}

/// How a frame's series hold their samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Form {
    /// As a connector writes them: fixed-width values back to back, or for strings,
    /// bytes, and lists, the `u32` end of each sample, in elements, then zeros up to a
    /// multiple of the element width or 8, whichever is less, then the elements.
    Raw,
    /// As `codec` encodes them, in tagged vectors.
    Encoded,
}

impl Form {
    const fn byte(self) -> u8 {
        match self {
            Self::Raw => 0,
            Self::Encoded => 1,
        }
    }

    fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::Raw,
            1 => Self::Encoded,
            other => unreachable!("invariant: only a draft writes a form, not {other}"),
        }
    }
}

/// The samples of one index group in a frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Range {
    /// The seq of the first sample. The home sets it.
    pub seq: u64,
    /// How many samples each series of the group holds.
    pub count: u32,
}

impl Range {
    /// The range of `group` in the frame `bytes`, or `None` when it is absent.
    fn find(bytes: &[u8], group: u32) -> Option<Self> {
        let (ranges, ..) = split(bytes);
        let range = &ranges[search(ranges, group)?];
        Some(Self {
            seq: u64::from_le_bytes(get(range, at::range::SEQ)),
            count: u32::from_le_bytes(get(range, at::range::COUNT)),
        })
    }
}

/// Why [`Draft::new`] refused a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// An entry is past the end of the key set.
    OutOfRange {
        /// The entry.
        entry: usize,
        /// The key set's entry count.
        entries: usize,
    },
    /// An entry is not above the entry before it.
    Unordered {
        /// The entry.
        entry: usize,
        /// The entry before it.
        last: usize,
    },
    /// An entry is present but the index of its group is absent.
    IndexAbsent {
        /// The entry.
        entry: usize,
        /// The index's entry.
        index: usize,
    },
    /// The pool has no block for the frame.
    Pool(block::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange { entry, entries } => write!(
                f,
                "entry {entry} is out of range for a key set of {entries} entries"
            ),
            Self::Unordered { entry, last } => {
                write!(
                    f,
                    "entry {entry} follows entry {last}: entries must increase"
                )
            }
            Self::IndexAbsent { entry, index } => write!(
                f,
                "entry {entry} is present but its index, entry {index}, is absent"
            ),
            Self::Pool(error) => write!(f, "no block for the frame: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Pool(error) => Some(error),
            _ => None,
        }
    }
}

impl From<block::Error> for Error {
    fn from(error: block::Error) -> Self {
        Self::Pool(error)
    }
}

/// A frame being written, in a block that it alone holds.
#[derive(Debug)]
pub struct Draft(block::Unique);

impl Draft {
    /// Takes a block from `pool` for a frame of `set`, and writes its header, ranges,
    /// and descriptors. `series` holds each present entry and the byte length of its
    /// series, in increasing entry order. A group is present when its index is. Each
    /// range starts at zero, and series bytes are not cleared.
    ///
    /// # Errors
    ///
    /// [`Error::Pool`] when the pool has no block that large, and the other variants
    /// when `series` breaks a rule above.
    pub fn new(
        pool: &block::Pool,
        set: &KeySet,
        form: Form,
        series: &[(usize, usize)],
    ) -> Result<Self, Error> {
        let (groups, len) = measure(set, series)?;
        let start = body_start(groups, series.len());
        let mut block = pool.alloc(start.saturating_add(len))?;
        let head = &mut block[..HEAD];
        head.fill(0);
        put(head, at::KEY_SET, &set.key().get().to_le_bytes());
        put(head, at::RANGES, &to_u32(groups).to_le_bytes());
        put(head, at::SERIES, &to_u32(series.len()).to_le_bytes());
        head[at::FORM] = form.byte();
        let (ranges, descriptors, body) = split_mut(&mut block);
        let indexes = series
            .iter()
            .map(|&(entry, _)| entry)
            .filter(|&entry| set.index(entry) == entry);
        for (range, index) in ranges.iter_mut().zip(indexes) {
            put(range, 0, &set.entries()[index].group.to_le_bytes());
            range[at::range::COUNT..].fill(0);
        }
        for &(entry, _) in series {
            if search(ranges, set.entries()[entry].group).is_none() {
                let index = set.index(entry);
                return Err(Error::IndexAbsent { entry, index });
            }
        }
        let mut end = 0_usize;
        for (descriptor, &(entry, len)) in descriptors.iter_mut().zip(series) {
            let start = end.next_multiple_of(SERIES_ALIGN);
            body[end..start].fill(0);
            end = start + len;
            put(descriptor, 0, &to_u32(entry).to_le_bytes());
            put(descriptor, 4, &to_u32(end).to_le_bytes());
        }
        Ok(Self(block))
    }

    /// The bytes of `entry`'s series, to fill, or `None` when it is absent. Time is
    /// logarithmic in the number of present series.
    pub fn series(&mut self, entry: usize) -> Option<&mut [u8]> {
        let (_, descriptors, body) = split_mut(&mut self.0);
        let n = search(descriptors, u32::try_from(entry).ok()?)?;
        let (start, end) = bounds(descriptors, n);
        Some(&mut body[start..end])
    }

    /// Each present entry and its series bytes, to fill, in entry order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (usize, &mut [u8])> {
        let (_, descriptors, mut body) = split_mut(&mut self.0);
        let mut offset = 0_usize;
        spans(ends(descriptors)).map(move |(entry, start, end)| {
            let (_, rest) = mem::take(&mut body).split_at_mut(start - offset);
            let (series, rest) = rest.split_at_mut(end - start);
            (body, offset) = (rest, end);
            (entry, series)
        })
    }

    /// Each present entry and its series bytes, in entry order.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &[u8])> {
        let (_, descriptors, body) = split(&self.0);
        series(body, ends(descriptors))
    }

    /// The key set the draft's entries number into.
    #[must_use]
    pub fn key_set(&self) -> key_set::Key {
        key_set::Key::new(u32::from_le_bytes(get(&self.0, at::KEY_SET)))
    }

    /// How the draft's series hold their samples.
    #[must_use]
    pub fn form(&self) -> Form {
        Form::from_byte(self.0[at::FORM])
    }

    /// The samples of group `group`, or `None` when its index is absent. Time is
    /// logarithmic in the number of present groups.
    #[must_use]
    pub fn range(&self, group: u32) -> Option<Range> {
        Range::find(&self.0, group)
    }

    /// Sets how many samples each series of group `group` holds.
    ///
    /// # Panics
    ///
    /// If `group` is absent from the frame.
    pub fn set_count(&mut self, group: u32, count: u32) {
        put(
            self.record_mut(group),
            at::range::COUNT,
            &count.to_le_bytes(),
        );
    }

    /// Sets the seq of the first sample of group `group`, on the path the frame
    /// freezes on.
    ///
    /// # Panics
    ///
    /// If `group` is absent from the frame.
    pub fn set_seq(&mut self, group: u32, seq: u64) {
        put(self.record_mut(group), at::range::SEQ, &seq.to_le_bytes());
    }

    fn record_mut(&mut self, group: u32) -> &mut [u8; RANGE] {
        let (ranges, ..) = split_mut(&mut self.0);
        let Some(n) = search(ranges, group) else {
            panic!("group {group} is absent from the frame");
        };
        &mut ranges[n]
    }

    /// The finished frame on `path`, the path whose seq its ranges count on.
    #[must_use]
    pub fn freeze(mut self, path: Path) -> Frame {
        self.0[at::PATH] = path.byte();
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
        key_set::Key::new(u32::from_le_bytes(get(&self.0, at::KEY_SET)))
    }

    /// The path whose seq the frame's ranges count on.
    #[must_use]
    pub fn path(&self) -> Path {
        Path::from_byte(self.0[at::PATH])
    }

    /// How the frame's series hold their samples.
    #[must_use]
    pub fn form(&self) -> Form {
        Form::from_byte(self.0[at::FORM])
    }

    /// The samples of group `group`, or `None` when its index is absent. Time is
    /// logarithmic in the number of present groups.
    #[must_use]
    pub fn range(&self, group: u32) -> Option<Range> {
        Range::find(&self.0, group)
    }

    /// The series bytes of `entry`, or `None` when it is absent. Time is logarithmic
    /// in the number of present series.
    #[must_use]
    pub fn series(&self, entry: usize) -> Option<&[u8]> {
        let (_, descriptors, body) = split(&self.0);
        let n = search(descriptors, u32::try_from(entry).ok()?)?;
        let (start, end) = bounds(descriptors, n);
        Some(&body[start..end])
    }

    /// Each present entry and its series bytes, in entry order.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &[u8])> {
        let (_, _, body) = split(&self.0);
        series(body, self.ends())
    }

    /// The credit that sending the frame to a reader spends: the bytes a block of the
    /// frame's length takes from its pool. It depends only on that length.
    #[must_use]
    pub fn charge(&self) -> u64 {
        to_u64(block::footprint(self.0.len()))
    }

    /// The series bytes of every present entry, as one view that shares the frame's
    /// block, from the first series to the end. [`Frame::ends`] gives where each
    /// series ends in this view. Copies nothing. Until it drops, the
    /// view keeps the whole block in use: [`Frame::charge`] bytes of the pool, not its
    /// length.
    #[must_use]
    pub fn body(&self) -> block::Block {
        let (ranges, series) = counts(&self.0);
        self.0.clone().skip(body_start(ranges, series))
    }

    /// Each present entry and the end of its series, as `(entry, end)`, in the order
    /// of [`Frame::iter`]. `end` counts from the start of [`Frame::body`], and
    /// [`series`] reads each series back from the body and these ends.
    pub fn ends(&self) -> impl Iterator<Item = (usize, usize)> {
        let (_, descriptors, _) = split(&self.0);
        ends(descriptors)
    }
}

/// Each series in `body`, a frame's [`Frame::body`], with its tag. `ends` gives a tag
/// and the end of each series, in order, as [`Frame::ends`] gives them. Copies nothing.
///
/// # Panics
///
/// The iterator panics where [`check`] refuses `body` and `ends`. The body and ends of
/// one frame never panic. Run [`check`] once on a body and ends from another node
/// before the first read.
pub fn series<T>(
    body: &[u8],
    ends: impl IntoIterator<Item = (T, usize)>,
) -> impl Iterator<Item = (T, &[u8])> {
    let (mut spans, mut last) = (spans(ends), 0);
    iter::from_fn(move || {
        let Some((tag, start, end)) = spans.next() else {
            last_fits(last, body.len()).unwrap_or_else(|error| panic!("{error}"));
            return None;
        };
        last = end;
        match cut(body, start, end) {
            Ok(series) => Some((tag, series)),
            Err(error) => panic!("{error}"),
        }
    })
}

/// Checks that `ends` fit `body`, so that [`series`] reads them without a panic.
///
/// # Errors
///
/// Returns the first [`BadEnd`] in the order of `ends`.
pub fn check<T>(
    body: &[u8],
    ends: impl IntoIterator<Item = (T, usize)>,
) -> Result<(), BadEnd> {
    let mut last = 0;
    for (_, start, end) in spans(ends) {
        cut(body, start, end)?;
        last = end;
    }
    last_fits(last, body.len())
}

/// Why [`check`] refused the ends of a body of series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadEnd {
    /// An end is past the body.
    Past {
        /// The end.
        end: usize,
        /// The bytes of the body.
        len: usize,
    },
    /// An end is before the start of its series, the end before it rounded up to 8.
    Before {
        /// The end.
        end: usize,
        /// The start of its series.
        start: usize,
    },
    /// The last end is not the end of the body.
    Short {
        /// The last end, or 0 when there is none.
        last: usize,
        /// The bytes of the body.
        len: usize,
    },
}

impl fmt::Display for BadEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Past { end, len } => {
                write!(f, "the end {end} is past the body of {len} bytes")
            }
            Self::Before { end, start } => {
                write!(
                    f,
                    "the end {end} is before {start}, the start of its series"
                )
            }
            Self::Short { last, len } => write!(
                f,
                "the last end {last} is not the end of the body of {len} bytes"
            ),
        }
    }
}

impl std::error::Error for BadEnd {}

/// The entry and the end of each descriptor's series.
fn ends(descriptors: &[[u8; DESCRIPTOR]]) -> impl Iterator<Item = (usize, usize)> + '_ {
    descriptors
        .iter()
        .map(|descriptor| (to_usize(lead(descriptor)), end_of(*descriptor)))
}

/// The tag, start, and end of each series in a frame's series bytes, from the tag and
/// end of each.
fn spans<T>(
    ends: impl IntoIterator<Item = (T, usize)>,
) -> impl Iterator<Item = (T, usize, usize)> {
    let mut last = 0_usize;
    ends.into_iter().map(move |(tag, end)| {
        let start = last.next_multiple_of(SERIES_ALIGN);
        last = end;
        (tag, start, end)
    })
}

/// Checks that `last`, the last end, is the end of `len` bytes of series.
fn last_fits(last: usize, len: usize) -> Result<(), BadEnd> {
    if last == len {
        Ok(())
    } else {
        Err(BadEnd::Short { last, len })
    }
}

/// The series from `start` to `end` in `body`, or why it does not fit.
fn cut(body: &[u8], start: usize, end: usize) -> Result<&[u8], BadEnd> {
    body.get(start..end).ok_or_else(|| {
        let len = body.len();
        if end > len {
            BadEnd::Past { end, len }
        } else {
            BadEnd::Before { end, start }
        }
    })
}

/// The present groups of a frame of `series`, and the bytes of its series with the
/// padding between them. The bytes saturate at `usize::MAX`, which no pool holds.
fn measure(set: &KeySet, series: &[(usize, usize)]) -> Result<(usize, usize), Error> {
    let entries = set.entries().len();
    let (mut groups, mut bytes, mut last) = (0, 0_usize, None);
    for &(entry, len) in series {
        if entry >= entries {
            return Err(Error::OutOfRange { entry, entries });
        }
        if let Some(last) = last
            && entry <= last
        {
            return Err(Error::Unordered { entry, last });
        }
        last = Some(entry);
        groups += usize::from(set.index(entry) == entry);
        bytes = bytes
            .checked_next_multiple_of(SERIES_ALIGN)
            .map_or(usize::MAX, |start| start.saturating_add(len));
    }
    Ok((groups, bytes))
}

/// A frame's ranges, its descriptors, and its series bytes.
fn split(bytes: &[u8]) -> (&[[u8; RANGE]], &[[u8; DESCRIPTOR]], &[u8]) {
    let (ranges, series) = counts(bytes);
    let (head, body) = bytes.split_at(body_start(ranges, series));
    let (ranges, descriptors) = head[HEAD..].split_at(RANGE * ranges);
    (ranges.as_chunks().0, descriptors.as_chunks().0, body)
}

fn split_mut(
    bytes: &mut [u8],
) -> (&mut [[u8; RANGE]], &mut [[u8; DESCRIPTOR]], &mut [u8]) {
    let (ranges, series) = counts(bytes);
    let (head, body) = bytes.split_at_mut(body_start(ranges, series));
    let (ranges, descriptors) = head[HEAD..].split_at_mut(RANGE * ranges);
    (
        ranges.as_chunks_mut().0,
        descriptors.as_chunks_mut().0,
        body,
    )
}

/// Where the series bytes start in a frame of `ranges` ranges and `series`
/// descriptors.
const fn body_start(ranges: usize, series: usize) -> usize {
    HEAD + RANGE * ranges + DESCRIPTOR * series
}

/// The counts of ranges and descriptors in a frame's header.
fn counts(bytes: &[u8]) -> (usize, usize) {
    let count = |at| to_usize(u32::from_le_bytes(get(bytes, at)));
    (count(at::RANGES), count(at::SERIES))
}

/// The position of the range or descriptor that leads with `key`, or `None`.
fn search<const N: usize>(records: &[[u8; N]], key: u32) -> Option<usize> {
    records.binary_search_by_key(&key, lead).ok()
}

/// The group of a range or the entry of a descriptor.
fn lead<const N: usize>(record: &[u8; N]) -> u32 {
    u32::from_le_bytes(get(record, 0))
}

/// Where a descriptor's series ends in the series bytes.
fn end_of(descriptor: [u8; DESCRIPTOR]) -> usize {
    to_usize(u32::from_le_bytes(get(&descriptor, 4)))
}

/// Where descriptor `n`'s series starts and ends in the series bytes.
fn bounds(descriptors: &[[u8; DESCRIPTOR]], n: usize) -> (usize, usize) {
    let start = match n {
        0 => 0,
        n => end_of(descriptors[n - 1]).next_multiple_of(SERIES_ALIGN),
    };
    (start, end_of(descriptors[n]))
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

/// A byte count as a `u64`.
fn to_u64(n: usize) -> u64 {
    u64::try_from(n).expect("invariant: a u64 holds a usize")
}

/// An entry, count, or offset of a frame as a `u32`. An entry fits because a key set's
/// slots are distinct `u32`s, and the rest because a pool block is at most 2 GiB.
fn to_u32(n: usize) -> u32 {
    u32::try_from(n).expect("invariant: a frame's numbers fit a u32")
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;
    use crate::channel;
    use crate::sample::{Scalar, Type};
    use key_set::{Group, Interner};
    use proptest::collection::vec;
    use proptest::prelude::*;

    const F64: Type = Type::Scalar(Scalar::F64);
    const U8: Type = Type::Scalar(Scalar::U8);

    fn key(n: u32) -> channel::Key {
        channel::Key::from_u128(u128::from(n))
    }

    /// An interner where key `n` has slot `n`, for each `n` below 1000.
    fn interner() -> Interner {
        let mut interner = Interner::new();
        for n in 0..1000 {
            interner.slots().assign(key(n));
        }
        interner
    }

    fn pool(budget: usize) -> block::Pool {
        let config = block::Config { budget };
        let memory = block::Heap::new(config.reservation());
        block::Pool::new(config, memory)
    }

    /// One group: index key 1 (entry 0), then keys 2 and 3 (entries 1 and 2).
    fn one_group(interner: &mut Interner) -> std::sync::Arc<KeySet> {
        interner.intern(&[Group {
            index: key(1),
            data: &[(key(2), F64), (key(3), U8)],
        }])
    }

    /// Two groups: index key 1 with key 2, and index key 3 with key 4.
    fn two_groups() -> std::sync::Arc<KeySet> {
        interner().intern(&[
            Group {
                index: key(1),
                data: &[(key(2), F64)],
            },
            Group {
                index: key(3),
                data: &[(key(4), F64)],
            },
        ])
    }

    /// The error of a draft of `series` over [`one_group`].
    fn refusal(series: &[(usize, usize)]) -> Error {
        let set = one_group(&mut interner());
        let result = Draft::new(&pool(1 << 16), &set, Form::Raw, series);
        result.unwrap_err()
    }

    #[test]
    fn lays_out_a_frame_byte_for_byte() {
        let mut interner = interner();
        for n in 10..13 {
            interner.intern(&[Group {
                index: key(n),
                data: &[],
            }]);
        }
        let set = one_group(&mut interner);
        let pool = pool(1 << 16);
        let mut dirty = pool.alloc(58).unwrap();
        dirty.fill(0xff);
        drop(dirty);
        let series = [(0, 3), (2, 2)];
        let mut draft = Draft::new(&pool, &set, Form::Encoded, &series).unwrap();
        draft.series(0).unwrap().copy_from_slice(&[0xaa; 3]);
        draft.series(2).unwrap().copy_from_slice(&[1, 2]);
        draft.set_count(0, 2);
        draft.set_seq(0, 7);
        let frame = draft.freeze(Path::Backfill);
        let mut expected = Vec::new();
        for n in [3_u32, 1, 2] {
            expected.extend(n.to_le_bytes());
        }
        expected.extend([1, 1, 0, 0]);
        for n in [0_u32, 2] {
            expected.extend(n.to_le_bytes());
        }
        expected.extend(7_u64.to_le_bytes());
        for n in [0_u32, 3, 2, 10] {
            expected.extend(n.to_le_bytes());
        }
        expected.extend([0xaa, 0xaa, 0xaa, 0, 0, 0, 0, 0, 1, 2]);
        assert_eq!(&*frame.0, expected.as_slice());
    }

    #[test]
    fn writes_the_path_at_freeze() {
        let set = two_groups();
        let pool = pool(1 << 16);
        for (path, byte) in [(Path::Live, 0), (Path::Backfill, 1)] {
            let frame = Draft::new(&pool, &set, Form::Raw, &[])
                .unwrap()
                .freeze(path);
            assert_eq!(frame.0[13], byte, "{path:?}");
            assert_eq!(frame.path(), path);
        }
    }

    #[test]
    #[should_panic(expected = "invariant: only a draft writes a path, not 2")]
    fn panics_on_an_unknown_path_byte() {
        let mut block = pool(1 << 16).alloc(HEAD).unwrap();
        block.fill(2);
        let path = Frame(block.freeze()).path();
        unreachable!("read {path:?} from byte 2");
    }

    #[test]
    fn reads_the_header_of_a_draft() {
        let mut interner = interner();
        // A key wider than one byte, and unlike the counts in the header.
        for n in 0..256 {
            interner.intern(&[Group {
                index: key(1000 + n),
                data: &[],
            }]);
        }
        let set = one_group(&mut interner);
        assert_eq!(set.key().get(), 256);
        let pool = pool(1 << 16);
        let draft = Draft::new(&pool, &set, Form::Encoded, &[(0, 8)]).unwrap();
        assert_eq!(draft.key_set(), set.key());
        assert_eq!(draft.form(), Form::Encoded);
        assert_eq!(draft.freeze(Path::Live).key_set(), set.key());
    }

    #[test]
    fn reads_the_header_and_absent_parts() {
        let set = two_groups();
        let pool = pool(1 << 16);
        let draft = Draft::new(&pool, &set, Form::Raw, &[(0, 8)]).unwrap();
        let frame = draft.freeze(Path::Backfill);
        assert_eq!(frame.key_set(), set.key());
        assert_eq!(frame.path(), Path::Backfill);
        assert_eq!(frame.form(), Form::Raw);
        assert_eq!(frame.range(0), Some(Range::default()));
        assert_eq!(frame.series(0).map(<[u8]>::len), Some(8));
        assert_eq!(frame.range(1), None);
        assert_eq!(frame.series(1), None);
        assert_eq!(frame.range(u32::MAX), None, "a group past the key set");
        assert_eq!(frame.series(4), None, "an entry past the key set");
        assert_eq!(frame.series(usize::MAX), None, "an entry past u32");
        assert_eq!(
            frame.iter().map(|(entry, _)| entry).collect::<Vec<_>>(),
            [0]
        );
    }

    #[test]
    fn charges_the_bytes_its_block_takes() {
        let set = two_groups();
        // Fixed values: nodes of two versions must agree on a charge.
        let cases: [(&[(usize, usize)], u64); 7] = [
            (&[], 128),
            (&[(0, 0)], 128),
            (&[(0, 0), (2, 0)], 128),
            (&[(0, 24)], 128),
            (&[(0, 25)], 192),
            (&[(0, 89)], 256),
            (&[(0, 8), (1, 8), (2, 4000)], 4160),
        ];
        for (series, charge) in cases {
            let pool = pool(1 << 16);
            let before = pool.committed();
            let draft = Draft::new(&pool, &set, Form::Raw, series).unwrap();
            let taken = to_u64(pool.committed() - before);
            let frame = draft.freeze(Path::Live);
            assert_eq!(frame.charge(), charge, "{series:?}");
            assert_eq!(frame.charge(), taken, "{series:?}");
        }
    }

    #[test]
    fn views_the_series_bytes() {
        let set = two_groups();
        let pool = pool(1 << 16);
        let series = [(0, 3), (1, 0), (2, 9)];
        let mut draft = Draft::new(&pool, &set, Form::Raw, &series).unwrap();
        draft.series(0).unwrap().fill(1);
        draft.series(2).unwrap().fill(2);
        let frame = draft.freeze(Path::Live);
        let expected = [[1, 1, 1, 0, 0, 0, 0, 0].as_slice(), &[2; 9]].concat();
        assert_eq!(&*frame.body(), expected.as_slice());
        let empty = Draft::new(&pool, &set, Form::Raw, &[]).unwrap();
        assert!(empty.freeze(Path::Live).body().is_empty());
    }

    #[test]
    fn gives_each_end_and_reads_the_series_back_from_the_view() {
        let set = two_groups();
        let pool = pool(1 << 16);
        let lens = [(0, 3), (1, 0), (2, 9)];
        let mut draft = Draft::new(&pool, &set, Form::Raw, &lens).unwrap();
        draft.series(0).unwrap().fill(1);
        draft.series(2).unwrap().fill(2);
        let frame = draft.freeze(Path::Live);
        let ends: Vec<_> = frame.ends().collect();
        assert_eq!(ends, [(0, 3), (1, 8), (2, 17)]);
        let body = frame.body();
        assert_eq!(check(&body, ends.iter().copied()), Ok(()));
        let read: Vec<_> = series(&body, ends).collect();
        assert_eq!(read, [(0, [1; 3].as_slice()), (1, &[]), (2, &[2; 9])]);
        let empty = Draft::new(&pool, &set, Form::Raw, &[]).unwrap();
        let empty = empty.freeze(Path::Live);
        assert_eq!(empty.ends().count(), 0);
        assert_eq!(check(&empty.body(), empty.ends()), Ok(()));
        assert_eq!(series(&empty.body(), empty.ends()).count(), 0);
    }

    /// Tags `ends` with their positions.
    fn tagged(ends: &[usize]) -> impl Iterator<Item = (usize, usize)> + '_ {
        ends.iter().copied().enumerate()
    }

    #[test]
    fn refuses_an_end_past_the_body() {
        let error = check(&[0; 4], tagged(&[5])).unwrap_err();
        assert_eq!(error, BadEnd::Past { end: 5, len: 4 });
        assert_eq!(error.to_string(), "the end 5 is past the body of 4 bytes");
    }

    #[test]
    fn refuses_an_end_before_the_start_of_its_series() {
        let error = check(&[0; 16], tagged(&[3, 2])).unwrap_err();
        assert_eq!(error, BadEnd::Before { end: 2, start: 8 });
        assert_eq!(
            error.to_string(),
            "the end 2 is before 8, the start of its series"
        );
    }

    #[test]
    fn refuses_an_end_inside_the_padding() {
        let error = check(&[0; 4], tagged(&[3, 4])).unwrap_err();
        assert_eq!(error, BadEnd::Before { end: 4, start: 8 });
    }

    #[test]
    fn refuses_ends_that_stop_before_the_body() {
        let error = check(&[1; 17], tagged(&[3, 8])).unwrap_err();
        assert_eq!(error, BadEnd::Short { last: 8, len: 17 });
        assert_eq!(
            error.to_string(),
            "the last end 8 is not the end of the body of 17 bytes"
        );
        let error = check(&[1; 16], tagged(&[])).unwrap_err();
        assert_eq!(error, BadEnd::Short { last: 0, len: 16 });
    }

    #[test]
    fn refuses_the_first_bad_end() {
        let error = check(&[0; 4], tagged(&[9, 2])).unwrap_err();
        assert_eq!(error, BadEnd::Past { end: 9, len: 4 });
    }

    #[test]
    #[should_panic(expected = "the end 5 is past the body of 4 bytes")]
    fn panics_on_an_end_past_the_body() {
        series(&[0; 4], tagged(&[5])).for_each(drop);
    }

    #[test]
    #[should_panic(expected = "the end 2 is before 8, the start of its series")]
    fn panics_on_an_end_before_the_start_of_its_series() {
        series(&[0; 16], tagged(&[3, 2])).for_each(drop);
    }

    #[test]
    #[should_panic(expected = "the last end 8 is not the end of the body of 17 bytes")]
    fn panics_when_the_ends_stop_before_the_body() {
        series(&[1; 17], tagged(&[3, 8])).for_each(drop);
    }

    #[test]
    fn gives_the_block_back_when_the_frame_and_view_drop() {
        let set = two_groups();
        let pool = pool(1 << 16);
        let series = [(0, 3), (1, 0), (2, 9)];
        let frame = Draft::new(&pool, &set, Form::Raw, &series)
            .unwrap()
            .freeze(Path::Live);
        let committed = pool.committed();
        let len = frame.0.len();
        let body = frame.body();
        drop(frame);
        drop(body);
        let again = pool.alloc(len).unwrap();
        assert_eq!(
            pool.committed(),
            committed,
            "the pool reuses the frame's block"
        );
        drop(again);
    }

    #[test]
    fn finds_ranges_and_series_among_many() {
        let groups: Vec<Group<'_>> = (0..130)
            .map(|n| Group {
                index: key(n),
                data: &[],
            })
            .collect();
        let set = interner().intern(&groups);
        let pool = pool(1 << 16);
        let series = [(0, 8), (64, 8), (129, 8)];
        let mut draft = Draft::new(&pool, &set, Form::Raw, &series).unwrap();
        for (seq, &(entry, _)) in (1..).zip(&series) {
            draft
                .series(entry)
                .unwrap()
                .fill(u8::try_from(seq).unwrap());
            let group = u32::try_from(entry).unwrap();
            draft.set_count(group, 1);
            draft.set_seq(group, seq);
        }
        let frame = draft.freeze(Path::Live);
        assert_eq!(frame.range(64), Some(Range { seq: 2, count: 1 }));
        assert_eq!(frame.range(129), Some(Range { seq: 3, count: 1 }));
        assert_eq!(frame.range(128), None);
        assert_eq!(frame.series(129), Some([3; 8].as_slice()));
        assert_eq!(frame.series(63), None);
    }

    #[test]
    fn returns_the_pool_error() {
        let pool = pool(512);
        let set = one_group(&mut interner());
        let result = Draft::new(&pool, &set, Form::Raw, &[(0, 1000)]);
        let error = result.unwrap_err();
        let cause = block::Error::TooLarge {
            requested: 1040,
            largest: pool.largest(),
        };
        assert_eq!(error, Error::Pool(cause.clone()));
        assert_eq!(
            error.to_string(),
            format!("no block for the frame: {cause}")
        );
        assert_eq!(
            error.source().map(ToString::to_string),
            Some(cause.to_string())
        );
    }

    #[test]
    fn returns_the_pool_error_past_u32_max_bytes() {
        let set = interner().intern(&[Group {
            index: key(1),
            data: &[],
        }]);
        let pool = pool(1 << 16);
        let len = usize::try_from(u32::MAX).unwrap() + 1;
        let result = Draft::new(&pool, &set, Form::Raw, &[(0, len - 40)]);
        let expected = block::Error::TooLarge {
            requested: len,
            largest: pool.largest(),
        };
        assert_eq!(result.unwrap_err(), Error::Pool(expected));
    }

    #[test]
    fn returns_the_pool_error_when_lengths_overflow() {
        let error = refusal(&[(0, usize::MAX), (2, usize::MAX)]);
        let expected = block::Error::TooLarge {
            requested: usize::MAX,
            largest: pool(1 << 16).largest(),
        };
        assert_eq!(error, Error::Pool(expected));
    }

    #[test]
    fn refuses_an_entry_out_of_range() {
        let error = refusal(&[(0, 1), (3, 1)]);
        assert_eq!(
            error,
            Error::OutOfRange {
                entry: 3,
                entries: 3
            }
        );
        assert_eq!(
            error.to_string(),
            "entry 3 is out of range for a key set of 3 entries"
        );
        assert!(error.source().is_none());
    }

    #[test]
    fn refuses_entries_out_of_order() {
        let error = refusal(&[(2, 1), (0, 1)]);
        assert_eq!(error, Error::Unordered { entry: 0, last: 2 });
        assert_eq!(
            error.to_string(),
            "entry 0 follows entry 2: entries must increase"
        );
    }

    #[test]
    fn refuses_an_entry_twice() {
        let error = refusal(&[(0, 1), (0, 1)]);
        assert_eq!(error, Error::Unordered { entry: 0, last: 0 });
    }

    #[test]
    fn refuses_data_without_its_index() {
        let error = refusal(&[(2, 1)]);
        assert_eq!(error, Error::IndexAbsent { entry: 2, index: 0 });
        assert_eq!(
            error.to_string(),
            "entry 2 is present but its index, entry 0, is absent"
        );
    }

    #[test]
    fn refuses_data_whose_index_is_another_groups() {
        let error =
            Draft::new(&pool(1 << 16), &two_groups(), Form::Raw, &[(0, 1), (3, 1)])
                .unwrap_err();
        assert_eq!(error, Error::IndexAbsent { entry: 3, index: 2 });
    }

    #[test]
    fn sets_count_and_seq_apart() {
        let pool = pool(1 << 16);
        let series = [(0, 8), (2, 8)];
        let mut draft = Draft::new(&pool, &two_groups(), Form::Raw, &series).unwrap();
        assert_eq!(draft.range(0), Some(Range::default()));
        draft.set_count(0, 5);
        assert_eq!(draft.range(0), Some(Range { seq: 0, count: 5 }));
        draft.set_seq(0, 9);
        assert_eq!(draft.range(0), Some(Range { seq: 9, count: 5 }));
        draft.set_seq(1, 4);
        draft.set_count(1, 3);
        draft.set_count(0, 6);
        assert_eq!(draft.range(2), None);
        let frame = draft.freeze(Path::Live);
        assert_eq!(frame.range(0), Some(Range { seq: 9, count: 6 }));
        assert_eq!(frame.range(1), Some(Range { seq: 4, count: 3 }));
    }

    #[test]
    #[should_panic(expected = "group 1 is absent from the frame")]
    fn refuses_a_count_for_an_absent_group() {
        let pool = pool(1 << 16);
        let series = [(0, 1)];
        let mut draft = Draft::new(&pool, &two_groups(), Form::Raw, &series).unwrap();
        draft.set_count(1, 1);
    }

    #[test]
    #[should_panic(expected = "group 1 is absent from the frame")]
    fn refuses_a_seq_for_an_absent_group() {
        let pool = pool(1 << 16);
        let series = [(0, 1)];
        let mut draft = Draft::new(&pool, &two_groups(), Form::Raw, &series).unwrap();
        draft.set_seq(1, 1);
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
        /// Each group's seq and count, and whether the seq is set first.
        ranges: Vec<(u64, u32, bool)>,
        path: Path,
        form: Form,
        /// Whether the draft is filled through `iter_mut` or by entry.
        in_order: bool,
    }

    /// Up to 4 groups of up to 40 data channels, with data slots on both sides of the
    /// index slot.
    fn cases() -> impl Strategy<Value = Case> {
        (1_usize..5)
            .prop_flat_map(|n| {
                (
                    vec(0_usize..41, n),
                    vec(any::<bool>(), n),
                    vec(any::<bool>(), n * 41),
                    vec(0_usize..40, n * 41),
                    vec(any::<(u64, u32, bool)>(), n),
                    prop_oneof![Just(Path::Live), Just(Path::Backfill)],
                    prop_oneof![Just(Form::Raw), Just(Form::Encoded)],
                    any::<bool>(),
                )
            })
            .prop_map(
                |(data, groups, present, lens, ranges, path, form, in_order)| Case {
                    data,
                    groups,
                    present,
                    lens,
                    ranges,
                    path,
                    form,
                    in_order,
                },
            )
    }

    fn pattern(entry: usize, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| u8::try_from((entry * 31 + i) % 251).unwrap())
            .collect()
    }

    /// The key set of `case` and the series of its frame.
    fn shape(case: &Case) -> (std::sync::Arc<KeySet>, Vec<(usize, usize)>) {
        let data: Vec<Vec<(channel::Key, Type)>> = (0..case.data.len())
            .map(|g| {
                let base = u32::try_from(g * 100).unwrap();
                let count = u32::try_from(case.data[g]).unwrap();
                (0..count).map(|j| (key(base + 2 * j + 1), F64)).collect()
            })
            .collect();
        let groups: Vec<Group<'_>> = data
            .iter()
            .enumerate()
            .map(|(g, data)| Group {
                index: key(u32::try_from(g * 100 + 40).unwrap()),
                data,
            })
            .collect();
        let set = interner().intern(&groups);
        let series = (0..set.entries().len())
            .filter(|&entry| {
                let group = usize::try_from(set.entries()[entry].group).unwrap();
                let index = set.index(entry) == entry;
                case.groups[group] && (index || case.present[entry])
            })
            .map(|entry| (entry, case.lens[entry]))
            .collect();
        (set, series)
    }

    /// Fills each series of `draft` with its pattern, through `iter_mut` or by entry.
    fn fill(
        draft: &mut Draft,
        series: &[(usize, usize)],
        entries: usize,
        in_order: bool,
    ) -> Result<(), TestCaseError> {
        if in_order {
            let mut filled = Vec::new();
            for (entry, bytes) in draft.iter_mut() {
                bytes.copy_from_slice(&pattern(entry, bytes.len()));
                filled.push((entry, bytes.len()));
            }
            prop_assert_eq!(filled, series);
            return Ok(());
        }
        for entry in 0..=entries {
            let len = series
                .iter()
                .find(|&&(e, _)| e == entry)
                .map(|&(_, len)| len);
            match (draft.series(entry), len) {
                (Some(bytes), Some(len)) => bytes.copy_from_slice(&pattern(entry, len)),
                (None, None) => {}
                (bytes, len) => {
                    prop_assert!(false, "entry {entry}: {bytes:?}, {len:?}");
                }
            }
        }
        Ok(())
    }

    /// Writes a frame of `case`, then checks that every read gives back what it wrote.
    fn round_trip(case: &Case) -> Result<(), TestCaseError> {
        let (set, series) = shape(case);
        let entries = set.entries().len();
        let pool = pool(1 << 20);
        let before = pool.committed();
        let mut draft = Draft::new(&pool, &set, case.form, &series)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        let taken = to_u64(pool.committed() - before);
        prop_assert_eq!(draft.key_set(), set.key());
        prop_assert_eq!(draft.form(), case.form);
        fill(&mut draft, &series, entries, case.in_order)?;
        let mut ranges = Vec::new();
        for ((group, &(seq, count, seq_first)), &present) in
            (0_u32..).zip(&case.ranges).zip(&case.groups)
        {
            let range = present.then_some(Range { seq, count });
            if present && seq_first {
                draft.set_seq(group, seq);
                draft.set_count(group, count);
            } else if present {
                draft.set_count(group, count);
                draft.set_seq(group, seq);
            }
            prop_assert_eq!(draft.range(group), range);
            ranges.push(range);
        }
        ranges.push(None);
        let written: Vec<(usize, Vec<u8>)> = series
            .iter()
            .map(|&(entry, len)| (entry, pattern(entry, len)))
            .collect();
        let mut drafted = Vec::new();
        for (entry, bytes) in draft.iter() {
            let group = set.entries()[entry].group;
            prop_assert_eq!(draft.range(group), ranges[to_usize(group)]);
            drafted.push((entry, bytes.to_vec()));
        }
        prop_assert_eq!(&drafted, &written);
        let frame = draft.freeze(case.path);

        prop_assert_eq!(frame.key_set(), set.key());
        prop_assert_eq!(frame.path(), case.path);
        prop_assert_eq!(frame.form(), case.form);
        prop_assert_eq!(frame.charge(), taken);
        for (group, range) in (0_u32..).zip(ranges) {
            prop_assert_eq!(frame.range(group), range);
        }
        for entry in 0..=entries {
            let expected = written.iter().find(|(e, _)| *e == entry);
            let read = frame.series(entry);
            prop_assert_eq!(read, expected.map(|(_, bytes)| bytes.as_slice()));
        }
        let read: Vec<(usize, Vec<u8>)> = frame
            .iter()
            .map(|(entry, bytes)| (entry, bytes.to_vec()))
            .collect();
        let mut body = Vec::new();
        for (_, bytes) in &written {
            body.resize(body.len().next_multiple_of(SERIES_ALIGN), 0);
            body.extend(bytes);
        }
        prop_assert_eq!(&*frame.body(), body.as_slice());
        let view = frame.body();
        prop_assert_eq!(check(&view, frame.ends()), Ok(()));
        let from_ends: Vec<(usize, Vec<u8>)> = super::series(&view, frame.ends())
            .map(|(entry, bytes)| (entry, bytes.to_vec()))
            .collect();
        prop_assert_eq!(&from_ends, &written);
        prop_assert_eq!(read, written);
        Ok(())
    }

    proptest! {
        #[test]
        fn reads_back_what_a_draft_wrote(case in cases()) {
            round_trip(&case)?;
        }
    }
}
