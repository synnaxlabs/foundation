//! Frames, the key sets they point at, and the views that readers get.
//!
//! A frame is one pool block: a header, a range for each present index group, a
//! descriptor for each present series, and the series bytes. It holds offsets, never
//! pointers.

pub mod key_set;
mod view;

use std::{fmt, iter, mem};

use key_set::KeySet;
pub use view::{Mask, View};

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
        let (ranges, ..) = parts(bytes);
        Some(Self::read(&ranges[search(ranges, group)?]))
    }

    /// Each present group and its range in the frame `bytes`, in group order.
    fn all(bytes: &[u8]) -> impl Iterator<Item = (u32, Self)> {
        let (ranges, ..) = parts(bytes);
        ranges
            .iter()
            .map(|record| (lead(record), Self::read(record)))
    }

    fn read(record: &[u8; RANGE]) -> Self {
        Self {
            seq: u64::from_le_bytes(get(record, at::range::SEQ)),
            count: u32::from_le_bytes(get(record, at::range::COUNT)),
        }
    }
}

/// Why [`Draft::new`], [`Layout::new`], or [`Layout::from_ends`] refused a frame.
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
    /// An end from another node does not fit its series ([`Layout::from_ends`]).
    End(BadEnd),
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
            Self::End(error) => write!(f, "the ends do not fit the series: {error}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Pool(error) => Some(error),
            Self::End(error) => Some(error),
            _ => None,
        }
    }
}

impl From<block::Error> for Error {
    fn from(error: block::Error) -> Self {
        Self::Pool(error)
    }
}

/// The shape of a frame of a key set, checked against each rule of [`Layout::new`],
/// before it takes a block.
#[derive(Clone, Copy, Debug)]
pub struct Layout<'a> {
    set: &'a KeySet,
    series: &'a [(usize, usize)],
    /// What the second value of each of `series` is.
    sizes: Sizes,
    groups: usize,
    body: usize,
}

/// How a [`Layout`] gives the size of each series.
#[derive(Clone, Copy, Debug)]
enum Sizes {
    Lens,
    Ends,
}

impl Sizes {
    /// Where a series of `size` ends when it follows series bytes that end at `last`.
    /// Saturates at `usize::MAX`.
    fn end(self, last: usize, size: usize) -> usize {
        match self {
            Self::Lens => next_end(last, size),
            Self::Ends => size,
        }
    }
}

impl<'a> Layout<'a> {
    /// Checks `series` for a frame of `set`: each present entry and the byte length
    /// of its series, in increasing entry order. Each data entry needs the index of
    /// its group in `series`. A group is present when its index is. Takes no block.
    /// Time is linear in `series` while the data of at most 16 groups alternate, and
    /// O(n log n) for n series at worst.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`], [`Error::Unordered`], or [`Error::IndexAbsent`] when
    /// `series` breaks a rule. Never [`Error::Pool`].
    pub fn new(set: &'a KeySet, series: &'a [(usize, usize)]) -> Result<Self, Error> {
        Self::checked(set, series, Sizes::Lens)
    }

    /// Checks `ends` for a frame of `set` whose series bytes come from another node:
    /// each present entry and the end of its series, as [`Frame::ends`] gives them.
    /// The rules of [`Layout::new`] apply, and no end may be below the start of its
    /// series, as [`check`] checks. [`Layout::body_len`] is the last end. Takes no
    /// block. Time is as for [`Layout::new`].
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`], [`Error::Unordered`], or [`Error::IndexAbsent`] as for
    /// [`Layout::new`]. [`Error::End`] with [`BadEnd::Before`] for the first end below
    /// the start of its series.
    pub fn from_ends(
        set: &'a KeySet,
        ends: &'a [(usize, usize)],
    ) -> Result<Self, Error> {
        Self::checked(set, ends, Sizes::Ends)
    }

    fn checked(
        set: &'a KeySet,
        series: &'a [(usize, usize)],
        sizes: Sizes,
    ) -> Result<Self, Error> {
        let entries = set.entries().len();
        let (mut groups, mut bytes, mut last) = (0, 0_usize, None);
        // An index usually comes just before its data. Other data search `series`, once
        // per group while `found` holds its index. An absent index waits for the order
        // checks, without which the search means nothing.
        let (mut last_index, mut found, mut absent) = (None, [usize::MAX; 16], None);
        for &(entry, size) in series {
            if entry >= entries {
                return Err(Error::OutOfRange { entry, entries });
            }
            if let Some(last) = last
                && entry <= last
            {
                return Err(Error::Unordered { entry, last });
            }
            last = Some(entry);
            let group = to_usize(set.entries()[entry].group);
            let index = set.groups()[group];
            if index == entry {
                groups += 1;
                last_index = Some(index);
            } else if last_index != Some(index) && absent.is_none() {
                let memo = &mut found[group % found.len()];
                if *memo != index {
                    if series
                        .binary_search_by_key(&index, |&(entry, _)| entry)
                        .is_err()
                    {
                        absent = Some(Error::IndexAbsent { entry, index });
                    }
                    *memo = index;
                }
            }
            let (start, end) = (padded(bytes), sizes.end(bytes, size));
            if end < start {
                return Err(Error::End(BadEnd::Before { end, start }));
            }
            bytes = end;
        }
        if let Some(error) = absent {
            return Err(error);
        }
        Ok(Self {
            set,
            series,
            sizes,
            groups,
            body: bytes,
        })
    }

    /// Bytes of the block that [`Layout::draft`] takes: the header, the ranges, the
    /// descriptors, and the series. Saturates at `usize::MAX`, which no pool holds.
    #[must_use]
    pub fn block_len(&self) -> usize {
        body_start(self.groups, self.series.len()).saturating_add(self.body)
    }

    /// Bytes of [`Frame::body`]: the series, with the padding between them.
    /// Saturates at `usize::MAX`.
    #[must_use]
    pub fn body_len(&self) -> usize {
        self.body
    }

    /// Takes a block of [`Layout::block_len`] bytes from `pool`, and writes the
    /// header, ranges, and descriptors. Each range starts at zero, and series bytes
    /// are not cleared.
    ///
    /// # Errors
    ///
    /// The pool's error when it cannot give the block.
    pub fn draft(self, pool: &block::Pool, form: Form) -> Result<Draft, block::Error> {
        let Self {
            set,
            series,
            sizes,
            groups,
            ..
        } = self;
        let mut block = pool.alloc(self.block_len())?;
        let head = &mut block[..HEAD];
        head.fill(0);
        put(head, at::KEY_SET, &set.key().get().to_le_bytes());
        put(head, at::RANGES, &to_u32(groups).to_le_bytes());
        put(head, at::SERIES, &to_u32(series.len()).to_le_bytes());
        head[at::FORM] = form.byte();
        let (ranges, descriptors, body) = parts_mut(&mut block);
        let indexes = series
            .iter()
            .map(|&(entry, _)| entry)
            .filter(|&entry| set.index(entry) == entry);
        for (range, index) in ranges.iter_mut().zip(indexes) {
            put(range, 0, &set.entries()[index].group.to_le_bytes());
            range[at::range::COUNT..].fill(0);
        }
        let mut end = 0_usize;
        for (descriptor, &(entry, size)) in descriptors.iter_mut().zip(series) {
            let start = padded(end);
            body[end..start].fill(0);
            end = sizes.end(end, size);
            put(descriptor, 0, &to_u32(entry).to_le_bytes());
            put(descriptor, 4, &to_u32(end).to_le_bytes());
        }
        Ok(Draft(block))
    }
}

/// A frame being written, in a block that it alone holds.
#[derive(Debug)]
pub struct Draft(block::Unique);

impl Draft {
    /// Takes a block from `pool` for a frame of `set` and `series`, and writes its
    /// header, ranges, and descriptors: [`Layout::new`], then [`Layout::draft`]. Each
    /// range starts at zero, and series bytes are not cleared.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`], [`Error::Unordered`], or [`Error::IndexAbsent`] when
    /// `series` breaks a rule of [`Layout::new`]. [`Error::Pool`] only when `series`
    /// keeps every rule and the pool cannot give a block for the frame.
    pub fn new(
        pool: &block::Pool,
        set: &KeySet,
        form: Form,
        series: &[(usize, usize)],
    ) -> Result<Self, Error> {
        Ok(Layout::new(set, series)?.draft(pool, form)?)
    }

    /// The series bytes, with the padding between series, as [`Frame::body`] gives
    /// them after [`Draft::freeze`]. Use it to fill a body received whole.
    pub fn body_mut(&mut self) -> &mut [u8] {
        let (_, _, body) = parts_mut(&mut self.0);
        body
    }

    /// The bytes of `entry`'s series, to fill, or `None` when it is absent. Time is
    /// logarithmic in the number of present series.
    pub fn series_mut(&mut self, entry: usize) -> Option<&mut [u8]> {
        let (_, descriptors, body) = parts_mut(&mut self.0);
        let n = search(descriptors, u32::try_from(entry).ok()?)?;
        let (start, end) = bounds(descriptors, n);
        Some(&mut body[start..end])
    }

    /// Each present entry and its series bytes, to fill, in entry order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (usize, &mut [u8])> {
        let (_, descriptors, mut body) = parts_mut(&mut self.0);
        let mut offset = 0_usize;
        spans(descriptor_ends(descriptors)).map(move |(entry, start, end)| {
            let (_, rest) = mem::take(&mut body).split_at_mut(start - offset);
            let (series, rest) = rest.split_at_mut(end - start);
            (body, offset) = (rest, end);
            (entry, series)
        })
    }

    /// Each present entry and its series bytes, in entry order.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &[u8])> {
        let (_, descriptors, body) = parts(&self.0);
        split(body, descriptor_ends(descriptors))
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

    /// Each present group and its range, in group order. Time is linear in the
    /// number of present groups.
    pub fn ranges(&self) -> impl Iterator<Item = (u32, Range)> {
        Range::all(&self.0)
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
        let (ranges, ..) = parts_mut(&mut self.0);
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

    /// Each present group and its range, in group order. Time is linear in the
    /// number of present groups.
    pub fn ranges(&self) -> impl Iterator<Item = (u32, Range)> {
        Range::all(&self.0)
    }

    /// The series bytes of `entry`, or `None` when it is absent. Time is logarithmic
    /// in the number of present series.
    #[must_use]
    pub fn series(&self, entry: usize) -> Option<&[u8]> {
        let (_, descriptors, body) = parts(&self.0);
        let n = search(descriptors, u32::try_from(entry).ok()?)?;
        let (start, end) = bounds(descriptors, n);
        Some(&body[start..end])
    }

    /// Each present entry and its series bytes, in entry order.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &[u8])> {
        let (_, _, body) = parts(&self.0);
        split(body, self.ends())
    }

    /// The credit that sending the frame to a reader spends: the bytes a block of the
    /// frame's length takes from its pool. It depends only on that length.
    #[must_use]
    pub fn charge(&self) -> u64 {
        charge_of(self.0.len())
    }

    /// The series bytes of every present entry, from the first series to the end, as
    /// a block that shares the frame's memory. [`Frame::ends`] gives where each series
    /// ends in it. Copies nothing. Until it drops, it keeps the frame's whole block in
    /// use: [`Frame::charge`] bytes of the pool, not its length.
    #[must_use]
    pub fn body(&self) -> block::Block {
        let (ranges, series) = counts(&self.0);
        self.0.clone().skip(body_start(ranges, series))
    }

    /// Each present entry and the end of its series, as `(entry, end)`, in the order
    /// of [`Frame::iter`]. `end` counts from the start of [`Frame::body`], and
    /// [`split`] cuts the body at these ends.
    pub fn ends(&self) -> impl Iterator<Item = (usize, usize)> {
        let (_, descriptors, _) = parts(&self.0);
        descriptor_ends(descriptors)
    }
}

/// The charge of a frame of one group whose series bytes ([`Frame::body`]) hold
/// `series` series in `body_len` bytes: what [`Frame::charge`] gives for that frame.
/// Gives `u64::MAX` for a frame that no pool holds.
#[must_use]
pub fn charge(series: usize, body_len: usize) -> u64 {
    charge_of(body_start(1, series).saturating_add(body_len))
}

/// The end of each series when series of the given lengths follow one another in a
/// frame's series bytes: the first starts at 0, and each other at the end before it,
/// rounded up to a multiple of 8. The last end is the length of the series bytes.
/// [`split`] cuts at these ends. Ends saturate at `usize::MAX`.
pub fn ends<T>(
    lens: impl IntoIterator<Item = (T, usize)>,
) -> impl Iterator<Item = (T, usize)> {
    let mut last = 0;
    lens.into_iter().map(move |(tag, len)| {
        last = next_end(last, len);
        (tag, last)
    })
}

/// Each series in `body`, a frame's [`Frame::body`], with its tag. `ends` gives a tag
/// and the end of each series, in order, as [`Frame::ends`] gives them. Copies nothing.
///
/// # Panics
///
/// The iterator panics at the first end that [`check`] refuses. When the body runs past
/// the last end, it panics only once it runs out. The body and ends of one frame never
/// panic. Run [`check`] once on a body and ends from another node before the first
/// [`split`].
pub fn split<T>(
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

/// Checks that `ends` fit `body`, so that [`split`] cuts at them without a panic.
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
fn descriptor_ends(
    descriptors: &[[u8; DESCRIPTOR]],
) -> impl Iterator<Item = (usize, usize)> + '_ {
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
        let start = padded(last);
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

/// Where a series of `len` bytes ends when it follows series bytes that end at
/// `last`. Saturates at `usize::MAX`, which no pool holds.
fn next_end(last: usize, len: usize) -> usize {
    padded(last).saturating_add(len)
}

/// The charge of a frame of `len` bytes (CREDIT RULES).
fn charge_of(len: usize) -> u64 {
    to_u64(block::footprint(len))
}

/// A frame's ranges, its descriptors, and its series bytes.
fn parts(bytes: &[u8]) -> (&[[u8; RANGE]], &[[u8; DESCRIPTOR]], &[u8]) {
    let (ranges, series) = counts(bytes);
    let (head, body) = bytes.split_at(body_start(ranges, series));
    let (ranges, descriptors) = head[HEAD..].split_at(RANGE * ranges);
    (ranges.as_chunks().0, descriptors.as_chunks().0, body)
}

fn parts_mut(
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
        n => padded(end_of(descriptors[n - 1])),
    };
    (start, end_of(descriptors[n]))
}

/// Where a series that ends at `end` stops with its padding, and the next one starts.
/// Saturates at `usize::MAX`.
const fn padded(end: usize) -> usize {
    match end.checked_next_multiple_of(SERIES_ALIGN) {
        Some(start) => start,
        None => usize::MAX,
    }
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

    pub(super) fn key(n: u32) -> channel::Key {
        channel::Key::from_u128(u128::from(n))
    }

    /// An interner where key `n` has slot `n`, for each `n` below 1000.
    pub(super) fn interner() -> Interner {
        let mut interner = Interner::new();
        for n in 0..1000 {
            interner.slots().assign(key(n));
        }
        interner
    }

    pub(super) fn pool(budget: usize) -> block::Pool {
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
    pub(super) fn two_groups() -> std::sync::Arc<KeySet> {
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
        draft.series_mut(0).unwrap().copy_from_slice(&[0xaa; 3]);
        draft.series_mut(2).unwrap().copy_from_slice(&[1, 2]);
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
        let mut draft = Draft::new(&pool, &set, Form::Raw, &[(0, 8)]).unwrap();
        assert_eq!(draft.series_mut(usize::MAX), None, "an entry past u32");
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
        draft.series_mut(0).unwrap().fill(1);
        draft.series_mut(2).unwrap().fill(2);
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
        draft.series_mut(0).unwrap().fill(1);
        draft.series_mut(2).unwrap().fill(2);
        let frame = draft.freeze(Path::Live);
        let ends: Vec<_> = frame.ends().collect();
        assert_eq!(ends, [(0, 3), (1, 8), (2, 17)]);
        let body = frame.body();
        assert_eq!(check(&body, ends.iter().copied()), Ok(()));
        let read: Vec<_> = split(&body, ends).collect();
        assert_eq!(read, [(0, [1; 3].as_slice()), (1, &[]), (2, &[2; 9])]);
        let empty = Draft::new(&pool, &set, Form::Raw, &[]).unwrap();
        let empty = empty.freeze(Path::Live);
        assert_eq!(empty.ends().count(), 0);
        assert_eq!(check(&empty.body(), empty.ends()), Ok(()));
        assert_eq!(split(&empty.body(), empty.ends()).count(), 0);
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
        split(&[0; 4], tagged(&[5])).for_each(drop);
    }

    #[test]
    #[should_panic(expected = "the end 2 is before 8, the start of its series")]
    fn panics_on_an_end_before_the_start_of_its_series() {
        split(&[0; 16], tagged(&[3, 2])).for_each(drop);
    }

    #[test]
    #[should_panic(expected = "the last end 8 is not the end of the body of 17 bytes")]
    fn panics_when_the_ends_stop_before_the_body() {
        split(&[1; 17], tagged(&[3, 8])).for_each(drop);
    }

    #[test]
    #[should_panic(expected = "the last end 0 is not the end of the body of 16 bytes")]
    fn panics_on_a_body_with_no_ends() {
        split(&[1; 16], tagged(&[])).for_each(drop);
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
                .series_mut(entry)
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
    fn finds_an_index_that_is_not_just_before_its_data() {
        // Key n has slot n, so both indexes sort before both data channels.
        let set = interner().intern(&[
            Group {
                index: key(1),
                data: &[(key(3), F64)],
            },
            Group {
                index: key(2),
                data: &[(key(4), F64)],
            },
        ]);
        let pool = pool(1 << 16);
        Draft::new(&pool, &set, Form::Raw, &[(0, 1), (1, 1), (2, 1), (3, 1)]).unwrap();
        let error = Draft::new(&pool, &set, Form::Raw, &[(1, 1), (2, 1)]).unwrap_err();
        assert_eq!(error, Error::IndexAbsent { entry: 2, index: 0 });
    }

    #[test]
    fn refuses_the_first_data_without_its_index_among_many_searched() {
        // Entries 0 to 2 are the indexes of groups 0 to 2, and 3 to 5 their data.
        let set = interner().intern(&[
            Group {
                index: key(1),
                data: &[(key(4), F64)],
            },
            Group {
                index: key(2),
                data: &[(key(5), F64)],
            },
            Group {
                index: key(3),
                data: &[(key(6), F64)],
            },
        ]);
        let pool = pool(1 << 16);
        let refuse = |series: &[(usize, usize)]| {
            Draft::new(&pool, &set, Form::Raw, series).unwrap_err()
        };
        assert_eq!(
            refuse(&[(1, 1), (3, 1), (5, 1)]),
            Error::IndexAbsent { entry: 3, index: 0 }
        );
        assert_eq!(
            refuse(&[(0, 1), (1, 1), (3, 1), (5, 1)]),
            Error::IndexAbsent { entry: 5, index: 2 }
        );
    }

    #[test]
    fn finds_the_index_of_data_among_hundreds_of_groups() {
        // Entries 0 to 256 are the indexes of groups 0 to 256, and 257 to 513 their
        // data. More groups than search memos share each memo.
        let data: Vec<_> = (1..=257).map(|n| [(key(n + 300), F64)]).collect();
        let groups: Vec<_> = (1..=257)
            .zip(&data)
            .map(|(n, data)| Group {
                index: key(n),
                data,
            })
            .collect();
        let set = interner().intern(&groups);
        let pool = pool(1 << 20);
        let all: Vec<_> = (0..514).map(|entry| (entry, 1)).collect();
        Draft::new(&pool, &set, Form::Raw, &all).unwrap();
        let mut series = all.clone();
        series.remove(256);
        let error = Draft::new(&pool, &set, Form::Raw, &series).unwrap_err();
        assert_eq!(
            error,
            Error::IndexAbsent {
                entry: 513,
                index: 256
            }
        );
    }

    #[test]
    fn finds_the_index_of_data_that_alternate_groups() {
        // Entries 0 and 1 are the indexes, and the data of groups 0 and 1 alternate.
        let set = interner().intern(&[
            Group {
                index: key(1),
                data: &[(key(3), F64), (key(5), F64)],
            },
            Group {
                index: key(2),
                data: &[(key(4), F64), (key(6), F64)],
            },
        ]);
        let pool = pool(1 << 16);
        let all: Vec<_> = (0..6).map(|entry| (entry, 1)).collect();
        Draft::new(&pool, &set, Form::Raw, &all).unwrap();
        let error = Draft::new(&pool, &set, Form::Raw, &all[1..]).unwrap_err();
        assert_eq!(error, Error::IndexAbsent { entry: 2, index: 0 });
        let error =
            Draft::new(&pool, &set, Form::Raw, &[(0, 1), (2, 1), (3, 1), (4, 1)])
                .unwrap_err();
        assert_eq!(error, Error::IndexAbsent { entry: 3, index: 1 });
    }

    #[test]
    fn refuses_data_whose_index_sorts_after_it() {
        let set = interner().intern(&[Group {
            index: key(3),
            data: &[(key(2), F64)],
        }]);
        let error = Draft::new(&pool(1 << 16), &set, Form::Raw, &[(0, 8)]).unwrap_err();
        assert_eq!(error, Error::IndexAbsent { entry: 0, index: 1 });
    }

    /// Takes each block of `pool`, a `pool(256)`, and gives them.
    fn exhaust(pool: &block::Pool) -> [block::Unique; 2] {
        let held = [pool.alloc(1).unwrap(), pool.alloc(1).unwrap()];
        assert_eq!(
            pool.alloc(1).unwrap_err(),
            block::Error::Exhausted {
                requested: 1,
                available: 0
            }
        );
        held
    }

    #[test]
    fn refuses_data_without_its_index_when_the_pool_is_full() {
        let set = one_group(&mut interner());
        let pool = pool(256);
        let _held = exhaust(&pool);
        let error = Draft::new(&pool, &set, Form::Raw, &[(2, 1)]).unwrap_err();
        assert_eq!(error, Error::IndexAbsent { entry: 2, index: 0 });
    }

    /// The block and body lengths of a layout of `series`.
    fn lengths(
        set: &KeySet,
        series: &[(usize, usize)],
    ) -> Result<(usize, usize), Error> {
        Layout::new(set, series).map(|layout| (layout.block_len(), layout.body_len()))
    }

    #[test]
    fn checks_a_layout_with_the_errors_of_a_draft_on_a_full_pool() {
        let set = one_group(&mut interner());
        let pool = pool(256);
        let _held = exhaust(&pool);
        for (series, expected) in [
            (
                &[(3, 1)][..],
                Error::OutOfRange {
                    entry: 3,
                    entries: 3,
                },
            ),
            (&[(1, 1), (0, 1)], Error::Unordered { entry: 0, last: 1 }),
            // The search for index 0 misses in unsorted series.
            (
                &[(1, 1), (2, 1), (0, 1)],
                Error::Unordered { entry: 0, last: 2 },
            ),
            (&[(2, 1)], Error::IndexAbsent { entry: 2, index: 0 }),
        ] {
            let draft = Draft::new(&pool, &set, Form::Raw, series);
            assert_eq!(draft.unwrap_err(), expected);
            assert_eq!(lengths(&set, series), Err(expected));
        }
    }

    #[test]
    fn drafts_a_layout_only_when_the_pool_has_its_block() {
        let set = one_group(&mut interner());
        let pool = pool(256);
        let held = exhaust(&pool);
        let layout = Layout::new(&set, &[(0, 3), (2, 2)]).unwrap();
        let error = layout.draft(&pool, Form::Raw).unwrap_err();
        assert_eq!(
            error,
            block::Error::Exhausted {
                requested: 58,
                available: 0
            }
        );
        drop(held);
        let draft = layout.draft(&pool, Form::Encoded).unwrap();
        assert_eq!(draft.0.len(), 58);
        assert_eq!(draft.form(), Form::Encoded);
        let series: Vec<(usize, usize)> = draft
            .iter()
            .map(|(entry, bytes)| (entry, bytes.len()))
            .collect();
        assert_eq!(series, [(0, 3), (2, 2)]);
    }

    #[test]
    fn lays_out_the_header_ranges_descriptors_and_padded_series() {
        let set = one_group(&mut interner());
        assert_eq!(lengths(&set, &[(0, 3), (2, 2)]), Ok((58, 10)));
        assert_eq!(lengths(&set, &[]), Ok((16, 0)));
    }

    #[test]
    fn saturates_the_lengths_of_a_layout() {
        let set = one_group(&mut interner());
        let max = usize::MAX;
        let sized = lengths(&set, &[(0, max), (2, max)]);
        assert_eq!(sized, Ok((max, max)));
        assert_eq!(lengths(&set, &[(0, max - 48)]), Ok((max - 8, max - 48)));
    }

    #[test]
    fn a_refused_draft_takes_no_budget() {
        let set = one_group(&mut interner());
        let pool = pool(1 << 16);
        let error = Draft::new(&pool, &set, Form::Raw, &[(2, 1)]).unwrap_err();
        assert_eq!(error, Error::IndexAbsent { entry: 2, index: 0 });
        assert_eq!(pool.committed(), 0);
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
    fn gives_only_the_present_ranges() {
        let pool = pool(1 << 16);
        let set = two_groups();
        let empty = Draft::new(&pool, &set, Form::Raw, &[]).unwrap();
        assert_eq!(empty.ranges().count(), 0);
        assert_eq!(empty.freeze(Path::Live).ranges().count(), 0);
        let mut draft = Draft::new(&pool, &set, Form::Raw, &[(2, 1)]).unwrap();
        draft.set_count(1, 1);
        draft.set_seq(1, 7);
        let ranges: Vec<_> = draft.ranges().collect();
        assert_eq!(ranges, [(1, Range { seq: 7, count: 1 })]);
        let frame = draft.freeze(Path::Live);
        assert_eq!(frame.ranges().collect::<Vec<_>>(), ranges);
    }

    #[test]
    fn gives_a_present_range_without_samples() {
        let pool = pool(1 << 16);
        let series = [(0, 8), (2, 0)];
        let mut draft = Draft::new(&pool, &two_groups(), Form::Raw, &series).unwrap();
        let ranges: Vec<_> = draft.ranges().collect();
        assert_eq!(ranges, [(0, Range::default()), (1, Range::default())]);
        draft.set_seq(1, 5);
        let ranges: Vec<_> = draft.ranges().collect();
        assert_eq!(
            ranges,
            [(0, Range::default()), (1, Range { seq: 5, count: 0 })]
        );
        let frame = draft.freeze(Path::Live);
        assert_eq!(frame.ranges().collect::<Vec<_>>(), ranges);
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
    pub(super) struct Case {
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
    pub(super) fn cases() -> impl Strategy<Value = Case> {
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

    /// The key set of `case` and a frame of it, with each series filled with its
    /// pattern.
    pub(super) fn frame_of(case: &Case) -> (std::sync::Arc<KeySet>, Frame) {
        let (set, series) = shape(case);
        let mut draft = Draft::new(&pool(1 << 20), &set, case.form, &series).unwrap();
        for (entry, bytes) in draft.iter_mut() {
            bytes.copy_from_slice(&pattern(entry, bytes.len()));
        }
        (set, draft.freeze(case.path))
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
            match (draft.series_mut(entry), len) {
                (Some(bytes), Some(len)) => bytes.copy_from_slice(&pattern(entry, len)),
                (None, None) => {}
                (bytes, len) => {
                    prop_assert!(false, "entry {entry}: {bytes:?}, {len:?}");
                }
            }
        }
        Ok(())
    }

    /// Sets the range of each present group of `case` and checks the reads. Returns
    /// each group's range, `None` when the group is absent.
    fn set_ranges(
        draft: &mut Draft,
        case: &Case,
    ) -> Result<Vec<Option<Range>>, TestCaseError> {
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
        let present: Vec<(u32, Range)> = (0_u32..)
            .zip(&ranges)
            .filter_map(|(group, range)| range.map(|range| (group, range)))
            .collect();
        prop_assert_eq!(draft.ranges().collect::<Vec<_>>(), present);
        Ok(ranges)
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
        let mut ranges = set_ranges(&mut draft, case)?;
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
        let present: Vec<(u32, Range)> = draft.ranges().collect();
        let frame = draft.freeze(case.path);

        prop_assert_eq!(frame.key_set(), set.key());
        prop_assert_eq!(frame.path(), case.path);
        prop_assert_eq!(frame.form(), case.form);
        prop_assert_eq!(frame.charge(), taken);
        prop_assert_eq!(frame.ranges().collect::<Vec<_>>(), present);
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
        let from_ends: Vec<(usize, Vec<u8>)> = super::split(&view, frame.ends())
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

        #[test]
        fn lays_out_the_block_and_body_of_a_draft(case in cases()) {
            let (set, series) = shape(&case);
            let sized = lengths(&set, &series);
            let draft = Draft::new(&pool(1 << 20), &set, case.form, &series).unwrap();
            let block = draft.0.len();
            let body = draft.freeze(case.path).body().len();
            prop_assert_eq!(sized, Ok((block, body)));
        }

        /// Key n has slot n, so shuffled keys give any slot order, and more groups
        /// than [`Layout::new`] remembers.
        #[test]
        fn refuses_the_first_entry_whose_index_is_absent(
            sizes in vec(0_usize..10, 1..20),
            keys in Just((0..1000).collect::<Vec<u32>>()).prop_shuffle(),
            kept in vec(any::<bool>(), 200),
        ) {
            let mut keys = keys.into_iter().map(key);
            let data: Vec<Vec<(channel::Key, Type)>> = sizes
                .iter()
                .map(|&size| keys.by_ref().take(size).map(|key| (key, F64)).collect())
                .collect();
            let groups: Vec<Group<'_>> = data
                .iter()
                .map(|data| Group {
                    index: keys.next().unwrap(),
                    data,
                })
                .collect();
            let set = interner().intern(&groups);
            let series: Vec<(usize, usize)> = (0..set.entries().len())
                .filter(|&entry| kept[entry])
                .map(|entry| (entry, 1))
                .collect();
            let present = |index| series.iter().any(|&(entry, _)| entry == index);
            let expected = series
                .iter()
                .map(|&(entry, _)| (entry, set.index(entry)))
                .find(|&(_, index)| !present(index))
                .map(|(entry, index)| Error::IndexAbsent { entry, index });
            let result = Draft::new(&pool(1 << 20), &set, Form::Raw, &series);
            prop_assert_eq!(result.err(), expected);
        }
    }

    #[test]
    fn charges_a_frame_of_one_group_by_its_series_and_body() {
        // The values of `charges_the_bytes_its_block_takes` for one group.
        for (series, body, charge) in
            [(1, 0, 128), (1, 24, 128), (1, 25, 192), (1, 89, 256)]
        {
            assert_eq!(super::charge(series, body), charge, "{series}, {body}");
        }
    }

    #[test]
    fn gives_the_ends_of_series_that_follow_one_another() {
        let ends: Vec<_> = super::ends([('a', 3), ('b', 0), ('c', 9)]).collect();
        assert_eq!(ends, [('a', 3), ('b', 8), ('c', 17)]);
        assert_eq!(super::ends::<u8>([]).count(), 0);
        let max = usize::MAX;
        let ends: Vec<_> = super::ends([(0, max), (1, 1)]).collect();
        assert_eq!(ends, [(0, max), (1, max)]);
    }

    #[test]
    fn checks_ends_from_another_node_with_the_rules_of_a_layout() {
        let set = one_group(&mut interner());
        for (ends, expected) in [
            (
                &[(0, 0), (3, 8)][..],
                Error::OutOfRange {
                    entry: 3,
                    entries: 3,
                },
            ),
            (&[(1, 0), (0, 8)], Error::Unordered { entry: 0, last: 1 }),
            (&[(2, 1)], Error::IndexAbsent { entry: 2, index: 0 }),
            (
                &[(0, 3), (2, 5)],
                Error::End(BadEnd::Before { end: 5, start: 8 }),
            ),
            (
                &[(0, 9), (1, 8)],
                Error::End(BadEnd::Before { end: 8, start: 16 }),
            ),
        ] {
            let error = Layout::from_ends(&set, ends).unwrap_err();
            assert_eq!(error, expected, "{ends:?}");
        }
    }

    #[test]
    fn refuses_an_end_before_its_series_with_its_message() {
        let set = one_group(&mut interner());
        let error = Layout::from_ends(&set, &[(0, 3), (2, 5)]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the ends do not fit the series: the end 5 is before 8, the start of its \
             series"
        );
        let source = error.source().unwrap().to_string();
        assert_eq!(source, "the end 5 is before 8, the start of its series");
    }

    #[test]
    fn lays_out_a_frame_from_its_ends() {
        let set = one_group(&mut interner());
        let layout = Layout::from_ends(&set, &[(0, 3), (2, 10)]).unwrap();
        assert_eq!((layout.block_len(), layout.body_len()), (58, 10));
        assert_eq!(super::charge(2, layout.body_len()), 128);
        let empty = Layout::from_ends(&set, &[(0, 0)]).unwrap();
        assert_eq!((empty.block_len(), empty.body_len()), (40, 0));
    }

    #[test]
    fn charges_ends_past_the_largest_block_as_the_most_credit_and_drafts_no_block() {
        let set = one_group(&mut interner());
        let ends = [(0, 8), (2, 1 << 32)];
        let layout = Layout::from_ends(&set, &ends).unwrap();
        assert_eq!(super::charge(ends.len(), layout.body_len()), u64::MAX);
        let pool = pool(1 << 16);
        let committed = pool.committed();
        let expected = block::Error::TooLarge {
            requested: 48 + (1 << 32),
            largest: pool.largest(),
        };
        let error = layout.draft(&pool, Form::Encoded).unwrap_err();
        assert_eq!(error, expected);
        assert_eq!(pool.committed(), committed, "the refusal takes no block");
    }

    #[derive(Clone, Debug)]
    struct Remote {
        /// Data channels on the index.
        data: usize,
        /// Whether the home's frame has each data channel.
        present: Vec<bool>,
        /// Whether the reader's key set holds each data channel.
        held: Vec<bool>,
        /// Each series length: the index's, then each data channel's.
        lens: Vec<usize>,
        /// The order in which the reader's node gives slots to keys 0 to 99.
        order: Vec<u32>,
        range: Range,
    }

    /// A home frame of one group, and a reader's key set over the index and some of
    /// its data, with slots in another order.
    fn remotes() -> impl Strategy<Value = Remote> {
        (
            0_usize..41,
            vec(any::<bool>(), 40),
            vec(any::<bool>(), 40),
            vec(0_usize..40, 41),
            Just((0..100).collect::<Vec<u32>>()).prop_shuffle(),
            any::<(u64, u32)>(),
        )
            .prop_map(|(data, present, held, lens, order, (seq, count))| {
                Remote {
                    data,
                    present,
                    held,
                    lens,
                    order,
                    range: Range { seq, count },
                }
            })
    }

    /// The home's key set and frame of `remote`, and the reader's key set. Data
    /// channel `j` is key `2j + 1`, and the index is key 40.
    fn home_and_reader(
        remote: &Remote,
    ) -> (std::sync::Arc<KeySet>, Frame, std::sync::Arc<KeySet>) {
        let data = |held: &dyn Fn(usize) -> bool| -> Vec<(channel::Key, Type)> {
            (0..remote.data)
                .filter(|&j| held(j))
                .map(|j| (key(u32::try_from(2 * j + 1).unwrap()), F64))
                .collect()
        };
        let all = data(&|_| true);
        let home = interner().intern(&[Group {
            index: key(40),
            data: &all,
        }]);
        let mut interner = Interner::new();
        for &n in &remote.order {
            interner.slots().assign(key(n));
        }
        let held = data(&|j| remote.held[j]);
        let reader = interner.intern(&[Group {
            index: key(40),
            data: &held,
        }]);
        let series: Vec<(usize, usize)> = home
            .entries()
            .iter()
            .enumerate()
            .filter_map(|(entry, channel)| {
                let n = usize::try_from(channel.key.as_u128()).unwrap();
                let data = n != 40;
                let at = if data { 1 + n / 2 } else { 0 };
                (!data || remote.present[n / 2]).then_some((entry, remote.lens[at]))
            })
            .collect();
        let mut draft =
            Draft::new(&pool(1 << 20), &home, Form::Encoded, &series).unwrap();
        for (entry, bytes) in draft.iter_mut() {
            bytes.copy_from_slice(&pattern(entry, bytes.len()));
        }
        draft.set_count(0, remote.range.count);
        draft.set_seq(0, remote.range.seq);
        (home, draft.freeze(Path::Live), reader)
    }

    /// The entry of `home` that holds the channel of `place`, an entry of `reader`.
    fn entry_of(home: &KeySet, reader: &KeySet, place: usize) -> usize {
        let key = reader.entries()[place].key;
        home.entries()
            .iter()
            .position(|entry| entry.key == key)
            .unwrap()
    }

    /// What the home sends the reader: the end of each place it has, in place order,
    /// and the series bytes in that order.
    fn sent(
        home: &KeySet,
        frame: &Frame,
        reader: &KeySet,
    ) -> (Vec<(usize, usize)>, Vec<u8>) {
        let series: Vec<(usize, &[u8])> = (0..reader.entries().len())
            .filter_map(|place| {
                let bytes = frame.series(entry_of(home, reader, place))?;
                Some((place, bytes))
            })
            .collect();
        let lens = series.iter().map(|&(place, bytes)| (place, bytes.len()));
        let ends = super::ends(lens).collect();
        let mut body = Vec::new();
        for (_, bytes) in series {
            body.resize(body.len().next_multiple_of(SERIES_ALIGN), 0);
            body.extend(bytes);
        }
        (ends, body)
    }

    proptest! {
        #[test]
        fn gives_the_ends_of_a_frame_from_the_lengths_of_its_series(case in cases()) {
            let (_, series) = shape(&case);
            let (_, frame) = frame_of(&case);
            let ends: Vec<_> = super::ends(series).collect();
            prop_assert_eq!(ends, frame.ends().collect::<Vec<_>>());
        }

        #[test]
        fn builds_the_readers_frame_from_what_the_home_sends(remote in remotes()) {
            let (home, frame, reader) = home_and_reader(&remote);
            let (ends, body) = sent(&home, &frame, &reader);
            let spent = super::charge(ends.len(), body.len());
            let layout = Layout::from_ends(&reader, &ends)
                .map_err(|error| TestCaseError::fail(error.to_string()))?;
            prop_assert_eq!(layout.body_len(), body.len());
            let pool = pool(1 << 20);
            let before = pool.committed();
            let mut draft = layout.draft(&pool, Form::Encoded).unwrap();
            let taken = to_u64(pool.committed() - before);
            draft.body_mut().copy_from_slice(&body);
            draft.set_count(0, remote.range.count);
            draft.set_seq(0, remote.range.seq);
            let received = draft.freeze(Path::Live);
            prop_assert_eq!(super::charge(ends.len(), layout.body_len()), spent);
            prop_assert_eq!(received.charge(), spent);
            prop_assert_eq!(taken, spent);
            prop_assert_eq!(received.ends().collect::<Vec<_>>(), ends);
            prop_assert_eq!(received.range(0), Some(remote.range));
            for place in 0..reader.entries().len() {
                let expected = frame.series(entry_of(&home, &reader, place));
                prop_assert_eq!(received.series(place), expected, "place {}", place);
            }
        }
    }
}
