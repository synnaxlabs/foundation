//! A writer's frame, checked against the count of each group and split into one index
//! frame per present group (INDEX FRAMES).

use std::fmt;
use std::ops::Range;

use types::channel;
use types::frame::key_set::{self, KeySet};
use types::frame::{self, Draft, Form};
use types::sample::{Scalar, Type};

/// The buffers of [`Split`], kept from one frame to the next, so that a split makes no
/// heap allocation once they are large enough.
#[derive(Debug)]
pub(crate) struct Scratch {
    /// Each series that fits its group's count, and each encoded index before its
    /// check, sorted by group and then entry once all are checked.
    series: Vec<Series>,
    /// Each present group, in group order.
    parts: Vec<Part>,
    /// The place in `parts` of each group, by group number. The place of an absent
    /// group is left from an earlier frame.
    places: Vec<usize>,
    /// The encoded series of a raw frame, back to back.
    bytes: Vec<u8>,
    /// One vector of the stamps of an encoded index, decoded.
    vector: Box<[[u8; 8]; codec::VECTOR_LEN]>,
    /// The entries and lengths of one index frame, for [`Draft::new`].
    lens: Vec<(usize, usize)>,
}

impl Default for Scratch {
    fn default() -> Self {
        Self {
            series: Vec::new(),
            parts: Vec::new(),
            places: Vec::new(),
            bytes: Vec::new(),
            vector: Box::new([[0; 8]; codec::VECTOR_LEN]),
            lens: Vec::new(),
        }
    }
}

/// One present series of a writer's frame.
#[derive(Clone, Copy, Debug)]
struct Series {
    group: u32,
    entry: usize,
    /// Where its encoded bytes start in [`Scratch::bytes`]. Unused for an encoded
    /// frame, whose series stay in the frame.
    start: usize,
    /// How many encoded bytes it has.
    len: usize,
}

/// One present group of a writer's frame.
#[derive(Clone, Debug)]
struct Part {
    group: u32,
    count: u32,
    /// Its series in [`Scratch::series`].
    series: Range<usize>,
    /// The entry of its index series.
    index: usize,
    /// The error of its first series that does not fit `count`, by entry. An encoded
    /// index is checked last: by its [`Stamps`] as they decode, else by
    /// [`validate_index`].
    check: Result<(), Error>,
    /// Its encoded index is not checked yet: [`Scratch::check`] passed over it.
    pending: bool,
    made: bool,
}

impl Scratch {
    /// Checks each series of `draft`, a writer's frame of `set`, against the count of
    /// its group with `codec`, and encodes it when it is raw. Time is linear in the
    /// series bytes, plus O(n log n) for n present series.
    ///
    /// # Panics
    ///
    /// If `draft` is not of `set`, or holds a series whose type the home does not
    /// write.
    pub(crate) fn split<'a>(
        &'a mut self,
        set: &'a KeySet,
        mut draft: Draft,
    ) -> Split<'a> {
        assert!(
            draft.key_set() == set.key(),
            "the frame is of key set {}, not of key set {}",
            draft.key_set().get(),
            set.key().get()
        );
        self.collect(set, &mut draft);
        self.check(set, &mut draft);
        self.gather();
        Split {
            scratch: self,
            set,
            draft: Some(draft),
            next: 0,
        }
    }

    /// Records each present group of `draft` with its count.
    fn collect(&mut self, set: &KeySet, draft: &mut Draft) {
        self.series.clear();
        self.parts.clear();
        self.bytes.clear();
        self.places
            .resize(self.places.len().max(set.groups().len()), 0);
        for (entry, _) in draft.iter_mut() {
            if set.index(entry) == entry {
                let group = set.entries()[entry].group;
                self.places[to_usize(group)] = self.parts.len();
                self.parts.push(Part {
                    group,
                    count: 0,
                    series: 0..0,
                    index: entry,
                    check: Ok(()),
                    pending: false,
                    made: false,
                });
            }
        }
        for part in &mut self.parts {
            let range = draft
                .range(part.group)
                .expect("invariant: a present index has a range");
            part.count = range.count;
        }
    }

    /// Checks each series against the count of its group, until the first error in
    /// the group, and records each series that fits. Encodes a raw series into
    /// `bytes`. An encoded index waits for its [`Stamps`], unless a series after it
    /// fails.
    fn check(&mut self, set: &KeySet, draft: &mut Draft) {
        let form = draft.form();
        for (entry, bytes) in draft.iter_mut() {
            let group = set.entries()[entry].group;
            let part = &mut self.parts[self.places[to_usize(group)]];
            // Before the skip, so the panic does not depend on the series before it.
            let scalar = scalar(set.entries()[entry].data_type);
            if part.check.is_err() {
                continue;
            }
            let count = to_usize(part.count);
            let start = self.bytes.len();
            let checked = match form {
                Form::Raw => encode(&mut self.bytes, scalar, count, bytes),
                Form::Encoded if entry == part.index => {
                    part.pending = true;
                    Ok(bytes.len())
                }
                Form::Encoded => codec::validate(Type::Scalar(scalar), count, bytes)
                    .map(|_| bytes.len()),
            };
            match checked {
                Ok(len) => self.series.push(Series {
                    group,
                    entry,
                    start,
                    len,
                }),
                Err(error) => {
                    let channel = set.entries()[entry].key;
                    part.check = Err(Error { channel, error });
                }
            }
        }
        for part in &mut self.parts {
            if part.pending && part.check.is_err() {
                validate_index(part, set, draft);
            }
        }
    }

    /// Sorts the checked series by group and entry, and gives each part its series.
    fn gather(&mut self) {
        self.series
            .sort_unstable_by_key(|series| (series.group, series.entry));
        let mut next = 0;
        for part in &mut self.parts {
            let len = self.series[next..]
                .iter()
                .take_while(|series| series.group == part.group)
                .count();
            part.series = next..next + len;
            next += len;
        }
    }
}

/// A writer's frame, checked, to make into one index frame per present group.
#[derive(Debug)]
pub(crate) struct Split<'a> {
    scratch: &'a mut Scratch,
    set: &'a KeySet,
    /// The writer's frame. A raw frame is held for the stamps of its indexes, until
    /// [`Split::next`] has given every group. An encoded frame is held for its
    /// series, until it is given out as the frame of its one group.
    draft: Option<Draft>,
    /// The place in [`Scratch::parts`] of the group that [`Split::next`] gives next.
    next: usize,
}

/// The stamps of a group's index series, one vector at a time.
#[derive(Debug)]
pub(crate) struct Stamps<'a>(Source<'a>);

#[derive(Debug)]
enum Source<'a> {
    /// The stamps of a raw series that are not given yet.
    Raw(&'a [[u8; 8]]),
    /// An encoded series, decoded into `vector` one vector at a time.
    Encoded {
        decoder: codec::Decoder<'a>,
        vector: &'a mut [[u8; 8]; codec::VECTOR_LEN],
        set: &'a KeySet,
        /// The group, which records the end of the check.
        part: &'a mut Part,
    },
}

impl Stamps<'_> {
    /// The stamps of the next vector, at most [`codec::VECTOR_LEN`], or `None` after
    /// the last vector and after an error.
    ///
    /// # Errors
    ///
    /// The [`Error`] of an encoded index: of the vector it reads, as
    /// [`codec::validate`] gives it, or [`codec::Error::Trailing`] after the last
    /// vector. The group then fails its check.
    pub(crate) fn next(&mut self) -> Option<Result<&[[u8; 8]], Error>> {
        match &mut self.0 {
            Source::Raw(stamps) => {
                let len = stamps.len().min(codec::VECTOR_LEN);
                let (vector, rest) = stamps.split_at(len);
                *stamps = rest;
                (len > 0).then_some(Ok(vector))
            }
            Source::Encoded {
                decoder,
                vector,
                set,
                part,
            } => match decoder.next(vector.as_flattened_mut()) {
                Some(Ok(stamps)) => Some(Ok(stamps.as_chunks::<8>().0)),
                Some(Err(error)) => {
                    let channel = set.entries()[part.index].key;
                    let error = Error { channel, error };
                    part.check = Err(error.clone());
                    part.pending = false;
                    Some(Err(error))
                }
                None => {
                    part.pending = false;
                    None
                }
            },
        }
    }
}

impl Split<'_> {
    /// The next present group, in group order, with the stamps of its index series,
    /// or the [`Error`] of its first series that does not fit the group's count. The
    /// stamps of an encoded index give its error as they decode. After the last
    /// group, it gives the blocks of a raw frame back to the pool.
    ///
    /// # Panics
    ///
    /// If the index frame of the group was made already.
    pub(crate) fn next(&mut self) -> Option<(u32, Result<Stamps<'_>, Error>)> {
        let scratch = &mut *self.scratch;
        let Some(part) = scratch.parts.get_mut(self.next) else {
            self.draft.take_if(|draft| draft.form() == Form::Raw);
            return None;
        };
        self.next += 1;
        let group = part.group;
        if let Err(error) = &part.check {
            return Some((group, Err(error.clone())));
        }
        assert!(
            !part.made,
            "the index frame of group {group} was made before its stamps"
        );
        let Some(draft) = self.draft.as_mut() else {
            panic!("invariant: the frame is held until each group's stamps are given");
        };
        let form = draft.form();
        let index = series(draft, part.index);
        let stamps = match form {
            Form::Raw => Source::Raw(index.as_chunks::<8>().0),
            Form::Encoded => Source::Encoded {
                decoder: codec::Decoder::new(
                    Scalar::Stamp,
                    to_usize(part.count),
                    index,
                ),
                vector: &mut scratch.vector,
                set: self.set,
                part,
            },
        };
        Some((group, Ok(Stamps(stamps))))
    }

    /// The index frame of `group`: the writer's key set with only `group` present, its
    /// count, and its series encoded. The index frame of an encoded frame with one
    /// group is that frame, with no copy. It checks an encoded index whose stamps did
    /// not run to their end, in one pass over its vector headers.
    ///
    /// # Errors
    ///
    /// [`block::Error`] when `pool` has no block for it.
    ///
    /// # Panics
    ///
    /// If `group` is absent, failed its check, or was made already.
    pub(crate) fn frame(
        &mut self,
        pool: &block::Pool,
        group: u32,
    ) -> Result<Draft, block::Error> {
        let scratch = &mut *self.scratch;
        let at = scratch.places.get(to_usize(group)).copied().filter(|&at| {
            scratch
                .parts
                .get(at)
                .is_some_and(|part| part.group == group)
        });
        let Some(at) = at else {
            panic!("group {group} is absent from the frame");
        };
        let part = &mut scratch.parts[at];
        if part.pending {
            let Some(draft) = self.draft.as_mut() else {
                panic!("invariant: an encoded frame is held until it is made");
            };
            validate_index(part, self.set, draft);
        }
        let part = &scratch.parts[at];
        if let Err(error) = &part.check {
            panic!("group {group} failed its check: {error}");
        }
        assert!(
            !part.made,
            "the index frame of group {group} was made already"
        );
        if scratch.parts.len() == 1
            && let Some(draft) =
                self.draft.take_if(|draft| draft.form() == Form::Encoded)
        {
            scratch.parts[at].made = true;
            return Ok(draft);
        }
        let series = &scratch.series[part.series.clone()];
        scratch.lens.clear();
        scratch
            .lens
            .extend(series.iter().map(|series| (series.entry, series.len)));
        let mut index = match Draft::new(pool, self.set, Form::Encoded, &scratch.lens) {
            Ok(index) => index,
            Err(frame::Error::Pool(error)) => return Err(error),
            Err(error) => {
                panic!("invariant: a group's checked series make a frame: {error}")
            }
        };
        for ((_, out), &series) in index.iter_mut().zip(series) {
            out.copy_from_slice(checked(&mut self.draft, &scratch.bytes, series));
        }
        index.set_count(group, part.count);
        scratch.parts[at].made = true;
        Ok(index)
    }
}

/// The encoded bytes of `series`: in `draft` when the frame is encoded, else in
/// `bytes`.
fn checked<'s>(
    draft: &'s mut Option<Draft>,
    bytes: &'s [u8],
    series: Series,
) -> &'s [u8] {
    match draft {
        Some(draft) if draft.form() == Form::Encoded => {
            self::series(draft, series.entry)
        }
        _ => &bytes[series.start..series.start + series.len],
    }
}

/// Checks the encoded index of `part` in `draft`, and fails `part` with its error when
/// it has one.
#[cold]
#[inline(never)]
fn validate_index(part: &mut Part, set: &KeySet, draft: &mut Draft) {
    part.pending = false;
    let index = &set.entries()[part.index];
    let bytes = series(draft, part.index);
    let count = to_usize(part.count);
    if let Err(error) = codec::validate(index.data_type, count, bytes) {
        let channel = index.key;
        part.check = Err(Error { channel, error });
    }
}

/// The bytes of the checked series of `entry` in `draft`.
fn series(draft: &mut Draft, entry: usize) -> &[u8] {
    draft
        .series_mut(entry)
        .expect("invariant: a checked series is in the frame")
}

/// The scalar of a series of `data_type`, or `None` for a type the home does not
/// write.
const fn written(data_type: Type) -> Option<Scalar> {
    match data_type {
        Type::Scalar(scalar) => Some(scalar),
        Type::Array { .. }
        | Type::Matrix(_)
        | Type::List { .. }
        | Type::String
        | Type::Bytes => None,
    }
}

/// The first entry of `set` with a type the home does not write, if it has one.
pub(crate) fn unwritten(set: &KeySet) -> Option<&key_set::Entry> {
    set.entries()
        .iter()
        .find(|entry| written(entry.data_type).is_none())
}

/// The scalar of a series of `data_type`.
///
/// # Panics
///
/// If the home does not write a series of `data_type`: [`unwritten`] gives its entry.
fn scalar(data_type: Type) -> Scalar {
    written(data_type)
        .unwrap_or_else(|| panic!("home does not write a series of {data_type:?} yet"))
}

/// Encodes `values`, `count` samples of `scalar`, onto the end of `bytes`, and returns
/// the bytes written.
fn encode(
    bytes: &mut Vec<u8>,
    scalar: Scalar,
    count: usize,
    values: &[u8],
) -> Result<usize, codec::Error> {
    let start = bytes.len();
    let data_type = Type::Scalar(scalar);
    bytes.resize(start + codec::max_len(data_type, values.len()), 0);
    let written =
        codec::Encoder::new(data_type).encode(count, values, &mut bytes[start..]);
    bytes.truncate(start + written.as_ref().copied().unwrap_or(0));
    written
}

/// A series of `channel` that `codec` refuses at its group's count: a raw series of
/// the wrong length, or an encoded series whose headers are not valid at that count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Error {
    /// The series' channel.
    pub(crate) channel: channel::Key,
    /// Why `codec` refused it.
    pub(crate) error: codec::Error,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "channel {}: {}", self.channel, self.error)
    }
}

impl std::error::Error for Error {}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).expect("invariant: a usize holds a u32")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::iter;
    use std::sync::Arc;

    use proptest::prelude::*;
    use types::channel::{self, Slot};
    use types::frame::key_set::Group;
    use types::frame::{Form, Frame, Path, Range};

    use super::*;
    use crate::common::{SCALARS, create_interner, create_pool, key};

    /// The samples of one present group: its count and each present entry's values.
    #[derive(Clone, Debug)]
    struct Samples {
        count: u32,
        series: BTreeMap<usize, Vec<u8>>,
    }

    /// A writer's frame of `set` in `form`, with the count of each group of `write`.
    fn draft(
        pool: &block::Pool,
        set: &KeySet,
        form: Form,
        write: &BTreeMap<u32, Samples>,
    ) -> Draft {
        let mut bytes: BTreeMap<usize, Vec<u8>> = BTreeMap::new();
        for samples in write.values() {
            for (&entry, values) in &samples.series {
                let series = match form {
                    Form::Raw => values.clone(),
                    Form::Encoded => encoded(set, entry, values),
                };
                bytes.insert(entry, series);
            }
        }
        let lens: Vec<(usize, usize)> =
            bytes.iter().map(|(&entry, b)| (entry, b.len())).collect();
        let mut draft = Draft::new(pool, set, form, &lens).expect("a writer's frame");
        for (entry, series) in draft.iter_mut() {
            series.copy_from_slice(&bytes[&entry]);
        }
        for (&group, samples) in write {
            draft.set_count(group, samples.count);
        }
        draft
    }

    fn scalar_of(set: &KeySet, entry: usize) -> Scalar {
        match set.entries()[entry].data_type {
            Type::Scalar(scalar) => scalar,
            other => panic!("a test channel of type {other:?}"),
        }
    }

    /// `values` encoded as a series of `entry`, with as many samples as they hold.
    fn encoded(set: &KeySet, entry: usize, values: &[u8]) -> Vec<u8> {
        let scalar = scalar_of(set, entry);
        let mut out = vec![0; codec::max_len(Type::Scalar(scalar), values.len())];
        let count = values.len() / scalar.width();
        let len = codec::Encoder::new(Type::Scalar(scalar))
            .encode(count, values, &mut out)
            .expect("values that fit the count");
        out.truncate(len);
        out
    }

    fn decoded(set: &KeySet, entry: usize, count: u32, bytes: &[u8]) -> Vec<u8> {
        let scalar = scalar_of(set, entry);
        let count = usize::try_from(count).expect("a small count");
        let mut out = vec![0; count * scalar.width()];
        codec::decode(Type::Scalar(scalar), count, bytes, &mut out)
            .expect("an encoded series");
        out
    }

    /// Checks that `frame` is the index frame of `group` of `write`.
    fn assert_index_frame(
        set: &KeySet,
        write: &BTreeMap<u32, Samples>,
        group: u32,
        frame: &Frame,
    ) {
        let samples = &write[&group];
        assert_eq!(frame.key_set(), set.key());
        assert_eq!(frame.form(), Form::Encoded);
        for &other in write.keys() {
            let range = frame.range(other);
            if other == group {
                let count = samples.count;
                assert_eq!(range, Some(Range { seq: 0, count }), "group {group}");
            } else {
                assert_eq!(range, None, "group {other} in the frame of {group}");
            }
        }
        let series: BTreeMap<usize, Vec<u8>> = frame
            .iter()
            .map(|(entry, bytes)| (entry, decoded(set, entry, samples.count, bytes)))
            .collect();
        assert_eq!(series, samples.series, "the series of group {group}");
    }

    fn stamps(values: &[u8]) -> Vec<[u8; 8]> {
        values.as_chunks::<8>().0.to_vec()
    }

    /// The stamps of each vector of `stamps`, or the first error.
    fn vectors(mut stamps: Stamps<'_>) -> Result<Vec<Vec<[u8; 8]>>, Error> {
        iter::from_fn(|| stamps.next().map(|vector| vector.map(<[_]>::to_vec)))
            .collect()
    }

    /// A present group, with its stamps or its error.
    type Checked = (u32, Result<Vec<[u8; 8]>, Error>);

    /// Gives every group of `split` and every vector of its stamps.
    fn drain(split: &mut Split<'_>) {
        while let Some((_, stamps)) = split.next() {
            let mut stamps = stamps.expect("a valid group");
            while let Some(vector) = stamps.next() {
                vector.expect("a valid vector");
            }
        }
    }

    /// Each present group that `split` gives.
    fn groups(split: &mut Split<'_>) -> Vec<Checked> {
        let mut groups = Vec::new();
        while let Some((group, stamps)) = split.next() {
            let stamps = stamps.and_then(vectors).map(|vectors| vectors.concat());
            groups.push((group, stamps));
        }
        groups
    }

    /// A frame of `set`, a key set of one index and no data, whose index series is
    /// `encoded` with a count of `count`.
    fn encoded_index(
        pool: &block::Pool,
        set: &KeySet,
        count: u32,
        encoded: &[u8],
    ) -> Draft {
        let lens = [(0, encoded.len())];
        let mut draft = Draft::new(pool, set, Form::Encoded, &lens).expect("room");
        draft
            .series_mut(0)
            .expect("the index")
            .copy_from_slice(encoded);
        draft.set_count(0, count);
        draft
    }

    fn one_index() -> Arc<KeySet> {
        create_interner().intern(&[Group {
            index: key(Slot::new(1)),
            data: &[],
        }])
    }

    /// Groups 0 and 1, whose entries interleave: slot 1 (index 0), 2 (`I32`, group 0),
    /// 3 (index 1), 4 (`U16`, group 1), 5 (`F64`, group 0).
    fn two_groups() -> Arc<KeySet> {
        let zero = Group {
            index: key(Slot::new(1)),
            data: &[
                (key(Slot::new(2)), Type::Scalar(Scalar::I32)),
                (key(Slot::new(5)), Type::Scalar(Scalar::F64)),
            ],
        };
        let one = Group {
            index: key(Slot::new(3)),
            data: &[(key(Slot::new(4)), Type::Scalar(Scalar::U16))],
        };
        create_interner().intern(&[zero, one])
    }

    /// A write of both groups of [`two_groups`], with every entry present.
    fn both() -> BTreeMap<u32, Samples> {
        let zero = Samples {
            count: 3,
            series: BTreeMap::from([
                (0, [10_u64, 20, 30].map(u64::to_le_bytes).concat()),
                (1, [-1_i32, 0, 1].map(i32::to_le_bytes).concat()),
                (4, [0.5_f64, 1.5, 2.5].map(f64::to_le_bytes).concat()),
            ]),
        };
        let one = Samples {
            count: 2,
            series: BTreeMap::from([
                (2, [7_u64, 9].map(u64::to_le_bytes).concat()),
                (3, [300_u16, 301].map(u16::to_le_bytes).concat()),
            ]),
        };
        BTreeMap::from([(0, zero), (1, one)])
    }

    /// Two groups of 30 `U8` channels each, with interleaved slots, and a write of
    /// every entry.
    fn many_series() -> (Arc<KeySet>, BTreeMap<u32, Samples>) {
        let data: Vec<Vec<(channel::Key, Type)>> = (0..2)
            .map(|group| {
                (0..30)
                    .map(|n| {
                        let slot = Slot::new(3 + 2 * n + group);
                        (key(slot), Type::Scalar(Scalar::U8))
                    })
                    .collect()
            })
            .collect();
        let set = create_interner().intern(&[
            Group {
                index: key(Slot::new(1)),
                data: &data[0],
            },
            Group {
                index: key(Slot::new(2)),
                data: &data[1],
            },
        ]);
        let write = (0..2)
            .map(|group| {
                let count = 3 - group;
                let series = (0..set.entries().len())
                    .filter(|&entry| set.entries()[entry].group == group)
                    .map(|entry| {
                        let state = u64::try_from(entry + 1).expect("few");
                        let width = scalar_of(&set, entry).width();
                        (entry, values(state, count, width))
                    })
                    .collect();
                (group, Samples { count, series })
            })
            .collect();
        (set, write)
    }

    mod groups {
        use super::*;

        #[test]
        fn gives_the_stamps_of_each_group_in_group_order() {
            let set = two_groups();
            let write = both();
            for form in [Form::Raw, Form::Encoded] {
                let pool = create_pool(1 << 16);
                let mut scratch = Scratch::default();

                let mut split = scratch.split(&set, draft(&pool, &set, form, &write));

                let groups = groups(&mut split);
                let expected = vec![
                    (0, Ok(stamps(&write[&0].series[&0]))),
                    (1, Ok(stamps(&write[&1].series[&2]))),
                ];
                assert_eq!(groups, expected, "{form:?}");
            }
        }

        #[test]
        fn gives_only_the_present_groups() {
            let set = two_groups();
            let write = BTreeMap::from([(1, both()[&1].clone())]);
            let pool = create_pool(1 << 16);
            let mut scratch = Scratch::default();

            let mut split = scratch.split(&set, draft(&pool, &set, Form::Raw, &write));

            let groups: Vec<u32> = groups(&mut split)
                .into_iter()
                .map(|(group, _)| group)
                .collect();
            assert_eq!(groups, [1]);
        }

        mod when_codec_refuses_a_series {
            use super::*;

            #[test]
            fn refuses_a_data_series_of_too_few_samples_in_its_group_only() {
                let set = two_groups();
                let mut write = both();
                let short = [-1_i32, 0].map(i32::to_le_bytes).concat();
                write.get_mut(&0).expect("group 0").series.insert(1, short);
                let length = codec::Error::Length {
                    expected: 12,
                    actual: 8,
                };
                let truncated = codec::Error::Truncated {
                    vector: 0,
                    needed: 16,
                    available: 12,
                };
                for (form, expected, message) in [
                    (
                        Form::Raw,
                        length,
                        "the values hold 8 bytes, but the samples take 12",
                    ),
                    (
                        Form::Encoded,
                        truncated,
                        "vector 0 needs 16 bytes, but 12 are left",
                    ),
                ] {
                    let pool = create_pool(1 << 16);
                    let mut scratch = Scratch::default();

                    let mut split =
                        scratch.split(&set, draft(&pool, &set, form, &write));

                    let groups = groups(&mut split);
                    assert_eq!(expected.to_string(), message);
                    let channel = key(Slot::new(2));
                    let error = Error {
                        channel,
                        error: expected,
                    };
                    assert_eq!(groups[0], (0, Err(error)), "{form:?}");
                    let stamps = stamps(&write[&1].series[&2]);
                    assert_eq!(groups[1], (1, Ok(stamps)), "{form:?}");
                }
            }

            #[test]
            fn refuses_an_encoded_index_of_too_few_stamps_with_the_decode_error() {
                let set = two_groups();
                let write = both();
                let pool = create_pool(1 << 16);
                let mut draft = draft(&pool, &set, Form::Encoded, &write);
                draft.set_count(1, 3);
                let mut scratch = Scratch::default();

                let mut split = scratch.split(&set, draft);

                let groups = groups(&mut split);
                let expected = codec::Error::Truncated {
                    vector: 0,
                    needed: 32,
                    available: 24,
                };
                assert_eq!(
                    expected.to_string(),
                    "vector 0 needs 32 bytes, but 24 are left"
                );
                let channel = key(Slot::new(3));
                let error = Error {
                    channel,
                    error: expected,
                };
                assert_eq!(groups[1], (1, Err(error)));
                let stamps = stamps(&write[&0].series[&0]);
                assert_eq!(groups[0], (0, Ok(stamps)));
            }

            #[test]
            fn names_the_same_channel_in_either_form() {
                let set = two_groups();
                let write = both();
                let named = [Form::Raw, Form::Encoded].map(|form| {
                    let pool = create_pool(1 << 16);
                    let mut draft = draft(&pool, &set, form, &write);
                    draft.set_count(1, 3);
                    let mut scratch = Scratch::default();
                    let mut split = scratch.split(&set, draft);
                    let groups = groups(&mut split);
                    let (_, Err(error)) = &groups[1] else {
                        panic!("group 1 fits in {form:?}: {groups:?}");
                    };
                    error.channel
                });

                assert_eq!(named, [key(Slot::new(3)); 2]);
            }

            #[test]
            fn refuses_an_encoded_series_with_a_header_codec_does_not_take() {
                let set = two_groups();
                let pool = create_pool(1 << 16);
                let mut draft = draft(&pool, &set, Form::Encoded, &both());
                for (entry, series) in draft.iter_mut() {
                    if entry == 1 {
                        series[0] = 9;
                    }
                }
                let mut scratch = Scratch::default();

                let mut split = scratch.split(&set, draft);

                let groups = groups(&mut split);
                let expected = codec::Error::Tag { vector: 0, tag: 9 };
                assert_eq!(
                    expected.to_string(),
                    "vector 0 has tag 9, which this sample type does not use"
                );
                let error = Error {
                    channel: key(Slot::new(2)),
                    error: expected,
                };
                assert_eq!(groups[0], (0, Err(error)));
            }

            #[test]
            fn refuses_a_series_whose_count_was_never_set() {
                let set = two_groups();
                let write = both();
                let pool = create_pool(1 << 16);
                let lens: Vec<(usize, usize)> = write[&1]
                    .series
                    .iter()
                    .map(|(&entry, values)| (entry, values.len()))
                    .collect();
                let mut draft = Draft::new(&pool, &set, Form::Raw, &lens)
                    .expect("a writer's frame");
                for (entry, series) in draft.iter_mut() {
                    series.copy_from_slice(&write[&1].series[&entry]);
                }
                let mut scratch = Scratch::default();

                let mut split = scratch.split(&set, draft);

                let groups = groups(&mut split);
                let expected = codec::Error::Length {
                    expected: 0,
                    actual: 16,
                };
                let channel = key(Slot::new(3));
                let error = Error {
                    channel,
                    error: expected,
                };
                assert_eq!(groups, [(1, Err(error))]);
            }
        }
    }

    mod stamps {
        use super::*;

        #[test]
        fn gives_the_error_of_an_encoded_index_from_its_stamps() {
            let set = one_index();
            let pool = create_pool(1 << 16);
            let mut index = encoded(&set, 0, &5_u64.to_le_bytes());
            index[0] = 9;
            let mut scratch = Scratch::default();
            let mut split = scratch.split(&set, encoded_index(&pool, &set, 1, &index));

            let (_, stamps) = split.next().expect("group 0");
            let mut stamps = stamps.expect("an encoded index is checked as it decodes");

            let error = Error {
                channel: key(Slot::new(1)),
                error: codec::Error::Tag { vector: 0, tag: 9 },
            };
            assert_eq!(stamps.next(), Some(Err::<&[[u8; 8]], _>(error)));
            assert_eq!(stamps.next(), None);
        }

        #[test]
        fn gives_the_stamps_of_a_group_one_vector_at_a_time() {
            let set = one_index();
            let values: Vec<u8> = (10..2510_u64).flat_map(u64::to_le_bytes).collect();
            let samples = Samples {
                count: 2500,
                series: BTreeMap::from([(0, values.clone())]),
            };
            let write = BTreeMap::from([(0, samples)]);
            for form in [Form::Raw, Form::Encoded] {
                let pool = create_pool(1 << 16);
                let mut scratch = Scratch::default();
                let mut split = scratch.split(&set, draft(&pool, &set, form, &write));

                let (group, stamps) = split.next().expect("group 0");
                let vectors =
                    vectors(stamps.expect("a valid group")).expect("valid stamps");

                assert_eq!(group, 0);
                let lens: Vec<usize> = vectors.iter().map(Vec::len).collect();
                assert_eq!(lens, [1024, 1024, 452], "{form:?}");
                assert_eq!(vectors.concat(), super::stamps(&values), "{form:?}");
                assert!(split.next().is_none());
            }
        }

        #[test]
        fn gives_the_blocks_of_a_raw_frame_back_after_the_last_group() {
            let set = two_groups();
            let pool = create_pool(1 << 16);
            let mut scratch = Scratch::default();
            let mut split = scratch.split(&set, draft(&pool, &set, Form::Raw, &both()));
            let mut held = Vec::new();
            while let Ok(block) = pool.alloc(64) {
                held.push(block);
            }

            assert!(split.next().is_some());
            assert!(split.next().is_some());
            assert!(
                pool.alloc(64).is_err(),
                "the frame is held to the last group"
            );
            assert!(split.next().is_none());

            pool.alloc(64).expect("the blocks of the frame");
        }

        #[test]
        fn gives_an_index_of_many_stamps_one_vector_at_a_time() {
            let set = one_index();
            let vector = encoded(&set, 0, &[7_u64.to_le_bytes(); 1024].concat());
            let count = 1 << 22;
            let index = vector.repeat(count / 1024);
            let pool = create_pool(1 << 20);
            let draft = encoded_index(
                &pool,
                &set,
                u32::try_from(count).expect("small"),
                &index,
            );
            let mut scratch = Scratch::default();
            let mut split = scratch.split(&set, draft);

            let (_, stamps) = split.next().expect("group 0");
            let mut stamps = stamps.expect("a valid group");
            let mut read = 0;
            while let Some(vector) = stamps.next() {
                let vector = vector.expect("a valid vector");
                assert_eq!(vector, [7_u64.to_le_bytes(); 1024]);
                read += vector.len();
            }

            assert_eq!(read, count);
        }

        #[test]
        fn refuses_an_empty_index_that_claims_u32_max_stamps() {
            let set = one_index();
            let pool = create_pool(1 << 16);
            let draft = encoded_index(&pool, &set, u32::MAX, &[]);
            let mut scratch = Scratch::default();
            let mut split = scratch.split(&set, draft);

            let groups = groups(&mut split);

            let expected = codec::Error::Truncated {
                vector: 0,
                needed: 2,
                available: 0,
            };
            assert_eq!(
                expected.to_string(),
                "vector 0 needs 2 bytes, but 0 are left"
            );
            let error = Error {
                channel: key(Slot::new(1)),
                error: expected,
            };
            assert_eq!(groups, [(0, Err(error))]);
        }

        #[test]
        fn refuses_an_encoded_index_with_bytes_after_its_last_vector() {
            let set = one_index();
            let pool = create_pool(1 << 16);
            let mut index = encoded(&set, 0, &5_u64.to_le_bytes());
            index.push(0);
            let mut scratch = Scratch::default();
            let mut split = scratch.split(&set, encoded_index(&pool, &set, 1, &index));

            let groups = groups(&mut split);

            let expected = codec::Error::Trailing { extra: 1 };
            assert_eq!(expected.to_string(), "bytes after the last vector: 1");
            let error = Error {
                channel: key(Slot::new(1)),
                error: expected,
            };
            assert_eq!(groups, [(0, Err(error))]);
        }

        #[test]
        #[should_panic(
            expected = "the index frame of group 0 was made before its stamps"
        )]
        fn panics_after_the_frame_of_its_group() {
            let set = one_index();
            let pool = create_pool(1 << 16);
            let index = encoded(&set, 0, &5_u64.to_le_bytes());
            let mut scratch = Scratch::default();
            let mut split = scratch.split(&set, encoded_index(&pool, &set, 1, &index));
            let _frame = split.frame(&pool, 0).expect("room");

            let _stamps = split.next();
        }
    }

    mod frame {
        use super::*;

        #[test]
        fn makes_the_index_frame_of_each_group() {
            let set = two_groups();
            let write = both();
            for form in [Form::Raw, Form::Encoded] {
                let pool = create_pool(1 << 16);
                let mut scratch = Scratch::default();
                let mut split = scratch.split(&set, draft(&pool, &set, form, &write));

                for group in [0, 1] {
                    let frame = split.frame(&pool, group).expect("room");

                    assert_index_frame(&set, &write, group, &frame.freeze(Path::Live));
                }
            }
        }

        #[test]
        fn makes_the_index_frames_of_a_frame_of_many_series() {
            let (set, write) = many_series();
            for form in [Form::Raw, Form::Encoded] {
                let pool = create_pool(1 << 16);
                let mut scratch = Scratch::default();
                let mut split = scratch.split(&set, draft(&pool, &set, form, &write));

                for group in [0, 1] {
                    let frame = split.frame(&pool, group).expect("room");

                    assert_index_frame(&set, &write, group, &frame.freeze(Path::Live));
                }
            }
        }

        #[test]
        fn keeps_the_frame_of_one_encoded_group_with_no_copy() {
            let set = two_groups();
            let write = BTreeMap::from([(0, both()[&0].clone())]);
            let pool = create_pool(1 << 16);
            let mut scratch = Scratch::default();
            let mut split =
                scratch.split(&set, draft(&pool, &set, Form::Encoded, &write));
            let committed = pool.committed();

            let frame = split.frame(&pool, 0).expect("room");

            assert_eq!(pool.committed(), committed);
            assert_index_frame(&set, &write, 0, &frame.freeze(Path::Live));
        }

        #[test]
        fn returns_the_pool_error_when_the_pool_is_full() {
            let set = two_groups();
            let write = both();
            let pool = create_pool(1 << 16);
            let mut scratch = Scratch::default();
            let mut split = scratch.split(&set, draft(&pool, &set, Form::Raw, &write));
            let mut held = Vec::new();
            while let Ok(block) = pool.alloc(64) {
                held.push(block);
            }
            let available = (1 << 16) - pool.committed();

            let error = split.frame(&pool, 0).expect_err("a full pool");

            let exhausted = block::Error::Exhausted {
                requested: 128,
                available,
            };
            assert_eq!(error, exhausted);
            assert_eq!(
                error.to_string(),
                format!("pool is full: asked for 128 bytes, {available} bytes free")
            );
        }

        #[test]
        fn makes_the_frame_after_a_pool_error_once_the_pool_has_room() {
            let set = two_groups();
            let write = both();
            let pool = create_pool(1 << 16);
            let mut scratch = Scratch::default();
            let mut split = scratch.split(&set, draft(&pool, &set, Form::Raw, &write));
            let mut held = Vec::new();
            while let Ok(block) = pool.alloc(64) {
                held.push(block);
            }
            split.frame(&pool, 0).expect_err("a full pool");
            drop(held);

            let frame = split.frame(&pool, 0).expect("room after the pool frees");

            assert_index_frame(&set, &write, 0, &frame.freeze(Path::Live));
        }

        mod panics {
            use super::*;

            #[test]
            #[should_panic(expected = "the frame is of key set 1, not of key set 0")]
            fn on_a_frame_of_another_key_set() {
                let mut interner = create_interner();
                let set = interner.intern(&[Group {
                    index: key(Slot::new(1)),
                    data: &[],
                }]);
                let other = interner.intern(&[Group {
                    index: key(Slot::new(2)),
                    data: &[],
                }]);
                let write = BTreeMap::from([(
                    0,
                    Samples {
                        count: 1,
                        series: BTreeMap::from([(0, 5_u64.to_le_bytes().to_vec())]),
                    },
                )]);
                let pool = create_pool(1 << 16);

                Scratch::default().split(&set, draft(&pool, &other, Form::Raw, &write));
            }

            #[test]
            #[should_panic(expected = "group 1 is absent from the frame")]
            fn on_an_absent_group() {
                let set = two_groups();
                let write = BTreeMap::from([(0, both()[&0].clone())]);
                let pool = create_pool(1 << 16);
                let mut scratch = Scratch::default();
                let mut split =
                    scratch.split(&set, draft(&pool, &set, Form::Raw, &write));

                drop(split.frame(&pool, 1));
            }

            #[test]
            #[should_panic(expected = "group 0 failed its check: channel \
                                       02000000-0000-0000-0000-000000000002: the \
                                       values hold 8 bytes, but the samples take 12")]
            fn on_a_group_that_failed_its_check() {
                let set = two_groups();
                let mut write = both();
                write
                    .get_mut(&0)
                    .expect("group 0")
                    .series
                    .insert(1, vec![0; 8]);
                let pool = create_pool(1 << 16);
                let mut scratch = Scratch::default();
                let mut split =
                    scratch.split(&set, draft(&pool, &set, Form::Raw, &write));

                drop(split.frame(&pool, 0));
            }

            #[test]
            #[should_panic(expected = "group 0 failed its check")]
            fn on_a_group_whose_encoded_index_is_not_valid() {
                let set = one_index();
                let pool = create_pool(1 << 16);
                let mut index = encoded(&set, 0, &5_u64.to_le_bytes());
                index[0] = 9;
                let mut scratch = Scratch::default();
                let mut split =
                    scratch.split(&set, encoded_index(&pool, &set, 1, &index));

                drop(split.frame(&pool, 0));
            }

            #[test]
            #[should_panic(expected = "group 0 failed its check: channel \
                                       01000000-0000-0000-0000-000000000001: vector \
                                       0 has tag 9")]
            fn on_a_group_read_past_the_decode_error_of_its_index() {
                let set = one_index();
                let pool = create_pool(1 << 16);
                let mut index = encoded(&set, 0, &5_u64.to_le_bytes());
                index[0] = 9;
                let mut scratch = Scratch::default();
                let mut split =
                    scratch.split(&set, encoded_index(&pool, &set, 1, &index));
                let (_, stamps) = split.next().expect("group 0");
                let mut stamps =
                    stamps.expect("an encoded index is checked as it decodes");
                while stamps.next().is_some() {}

                drop(split.frame(&pool, 0));
            }

            /// Slot 1 (`I32`) is entry 0, before its index at slot 2. Both series are
            /// not valid, so the error of slot 1 is the first by entry.
            #[test]
            #[should_panic(expected = "group 0 failed its check: channel \
                                       01000000-0000-0000-0000-000000000001: vector \
                                       0 has tag 9")]
            fn on_a_group_with_the_error_of_its_first_series_by_entry() {
                let set = create_interner().intern(&[Group {
                    index: key(Slot::new(2)),
                    data: &[(key(Slot::new(1)), Type::Scalar(Scalar::I32))],
                }]);
                let samples = Samples {
                    count: 2,
                    series: BTreeMap::from([
                        (0, [1_i32, 2].map(i32::to_le_bytes).concat()),
                        (1, [10_u64, 20].map(u64::to_le_bytes).concat()),
                    ]),
                };
                let write = BTreeMap::from([(0, samples)]);
                let pool = create_pool(1 << 16);
                let mut draft = draft(&pool, &set, Form::Encoded, &write);
                for (entry, series) in draft.iter_mut() {
                    series[0] = if entry == 0 { 9 } else { 8 };
                }
                let mut scratch = Scratch::default();
                let mut split = scratch.split(&set, draft);

                drop(split.frame(&pool, 0));
            }

            #[test]
            #[should_panic(expected = "the index frame of group 1 was made already")]
            fn on_a_group_made_twice() {
                let set = two_groups();
                let pool = create_pool(1 << 16);
                let mut scratch = Scratch::default();
                let mut split =
                    scratch.split(&set, draft(&pool, &set, Form::Raw, &both()));
                let _made = split.frame(&pool, 1).expect("room");

                drop(split.frame(&pool, 1));
            }

            /// Splits a frame of one stamp at `count` and a `String` series after it.
            fn split_a_string_series(count: u32) {
                let set = create_interner().intern(&[Group {
                    index: key(Slot::new(1)),
                    data: &[(key(Slot::new(2)), Type::String)],
                }]);
                let samples = Samples {
                    count,
                    series: BTreeMap::from([
                        (0, 5_u64.to_le_bytes().to_vec()),
                        (1, b"text".to_vec()),
                    ]),
                };
                let write = BTreeMap::from([(0, samples)]);
                let pool = create_pool(1 << 16);

                Scratch::default().split(&set, draft(&pool, &set, Form::Raw, &write));
            }

            #[test]
            #[should_panic(expected = "home does not write a series of String yet")]
            fn on_a_series_of_a_type_the_home_does_not_write() {
                split_a_string_series(1);
            }

            #[test]
            #[should_panic(expected = "home does not write a series of String yet")]
            fn on_a_series_of_such_a_type_after_an_index_that_does_not_fit() {
                split_a_string_series(2);
            }
        }
    }

    mod unwritten {
        use types::sample::Matrix;

        use super::*;

        #[test]
        fn gives_none_for_a_key_set_of_scalars() {
            assert_eq!(unwritten(&two_groups()), None);
        }

        #[test]
        fn gives_none_for_a_series_of_each_scalar() {
            for scalar in SCALARS {
                let set = create_interner().intern(&[Group {
                    index: key(Slot::new(1)),
                    data: &[(key(Slot::new(2)), Type::Scalar(scalar))],
                }]);

                assert_eq!(unwritten(&set), None, "{scalar:?}");
            }
        }

        #[test]
        fn gives_the_first_entry_of_each_type_that_is_not_a_scalar() {
            let element = Scalar::F32;
            let types = [
                Type::Array { element, len: 3 },
                Type::Matrix(Matrix::new(element, 2, 3).expect("a matrix")),
                Type::List { element, max: 3 },
                Type::String,
                Type::Bytes,
            ];
            for data_type in types {
                let set = create_interner().intern(&[Group {
                    index: key(Slot::new(1)),
                    data: &[
                        (key(Slot::new(2)), Type::Scalar(Scalar::U8)),
                        (key(Slot::new(4)), data_type),
                        (key(Slot::new(5)), Type::Bytes),
                    ],
                }]);

                let entry = unwritten(&set).expect("an entry");

                assert_eq!((entry.slot, entry.data_type), (Slot::new(4), data_type));
            }
        }
    }

    mod scratch {
        use super::*;

        /// The address and capacity of each buffer of `scratch`.
        fn buffers(scratch: &Scratch) -> [(usize, usize); 6] {
            [
                (scratch.series.as_ptr().addr(), scratch.series.capacity()),
                (scratch.parts.as_ptr().addr(), scratch.parts.capacity()),
                (scratch.places.as_ptr().addr(), scratch.places.capacity()),
                (scratch.bytes.as_ptr().addr(), scratch.bytes.capacity()),
                (scratch.vector.as_ptr().addr(), scratch.vector.len()),
                (scratch.lens.as_ptr().addr(), scratch.lens.capacity()),
            ]
        }

        #[test]
        fn keeps_its_buffers_from_a_larger_write() {
            let (large, many) = many_series();
            let small = two_groups();
            for form in [Form::Raw, Form::Encoded] {
                let pool = create_pool(1 << 16);
                let mut scratch = Scratch::default();
                let mut split =
                    scratch.split(&large, draft(&pool, &large, form, &many));
                drain(&mut split);
                for group in [0, 1] {
                    drop(split.frame(&pool, group).expect("room"));
                }
                let kept = buffers(&scratch);

                let mut split =
                    scratch.split(&small, draft(&pool, &small, form, &both()));
                drain(&mut split);
                for group in [0, 1] {
                    drop(split.frame(&pool, group).expect("room"));
                }

                assert_eq!(buffers(&scratch), kept, "{form:?}");
            }
        }
    }

    /// The values of one series: `count` samples of `width` bytes from `state`, with
    /// runs so that more than one codec applies.
    fn values(mut state: u64, count: u32, width: usize) -> Vec<u8> {
        let len = usize::try_from(count).expect("a small count") * width;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let run = usize::try_from(state % 5).expect("small") * width;
            let byte = u8::try_from(state >> 56).expect("one byte");
            out.extend(std::iter::repeat_n(byte, run.max(1)));
        }
        out.truncate(len);
        out
    }

    /// A key set of 1 to 4 groups of scalar channels, with interleaved slots, and a
    /// write of some of its groups and entries.
    fn writes() -> impl Strategy<Value = (Arc<KeySet>, BTreeMap<u32, Samples>)> {
        let group = (
            prop::collection::vec(prop::sample::select(&SCALARS[..]), 0..4),
            any::<bool>(),
            0_u32..1100,
        );
        (prop::collection::vec(group, 1..5), any::<u64>())
            .prop_flat_map(|(groups, state)| {
                let channels: usize =
                    groups.iter().map(|(data, ..)| data.len() + 1).sum();
                let slots: Vec<u32> =
                    (1..=u32::try_from(channels).expect("few")).collect();
                (Just(groups), Just(slots).prop_shuffle(), Just(state))
            })
            .prop_map(|(groups, slots, state)| {
                let mut slots = slots.into_iter().map(Slot::new);
                let data: Vec<(Slot, Vec<(Slot, Type)>)> = groups
                    .iter()
                    .map(|(scalars, ..)| {
                        let index = slots.next().expect("a slot per channel");
                        let data = scalars
                            .iter()
                            .map(|&s| (slots.next().expect("a slot"), Type::Scalar(s)))
                            .collect();
                        (index, data)
                    })
                    .collect();
                let keys: Vec<Vec<(channel::Key, Type)>> = data
                    .iter()
                    .map(|(_, data)| data.iter().map(|&(s, t)| (key(s), t)).collect())
                    .collect();
                let shapes: Vec<Group<'_>> = data
                    .iter()
                    .zip(&keys)
                    .map(|((index, _), data)| Group {
                        index: key(*index),
                        data,
                    })
                    .collect();
                let set = create_interner().intern(&shapes);
                let mut write = BTreeMap::new();
                for (n, (index, data)) in data.iter().enumerate() {
                    let &(_, present, count) = &groups[n];
                    if !present {
                        continue;
                    }
                    let entry = set.find(*index).expect("an index entry");
                    let group = set.entries()[entry].group;
                    let mut series = BTreeMap::new();
                    for (k, &(slot, _)) in [(*index, Type::Scalar(Scalar::Stamp))]
                        .iter()
                        .chain(data)
                        .enumerate()
                    {
                        let entry = set.find(slot).expect("an entry");
                        if k > 0 && (state >> (k + n)) & 1 == 1 {
                            continue;
                        }
                        let width = scalar_of(&set, entry).width();
                        let state = state ^ u64::try_from(entry + 1).expect("few");
                        series.insert(entry, values(state, count, width));
                    }
                    write.insert(group, Samples { count, series });
                }
                (set, write)
            })
    }

    proptest! {
        #[test]
        fn makes_the_index_frame_of_any_write((set, write) in writes(), raw in any::<bool>()) {
            let form = if raw { Form::Raw } else { Form::Encoded };
            let pool = create_pool(1 << 20);
            let mut scratch = Scratch::default();
            let mut split = scratch.split(&set, draft(&pool, &set, form, &write));

            let groups = groups(&mut split);
            let expected: Vec<Checked> = write
                .iter()
                .map(|(&group, samples)| {
                    let index = set.groups()[usize::try_from(group).expect("small")];
                    (group, Ok(stamps(&samples.series[&index])))
                })
                .collect();
            prop_assert_eq!(groups, expected);
            for &group in write.keys() {
                let frame = split.frame(&pool, group).expect("room");
                assert_index_frame(&set, &write, group, &frame.freeze(Path::Live));
            }
        }

        #[test]
        fn reuses_its_buffers_across_writes(
            writes in prop::collection::vec((writes(), any::<bool>()), 1..4),
        ) {
            let pool = create_pool(1 << 20);
            let mut scratch = Scratch::default();

            for ((set, write), raw) in &writes {
                let form = if *raw { Form::Raw } else { Form::Encoded };
                let mut split = scratch.split(set, draft(&pool, set, form, write));

                for &group in write.keys() {
                    let frame = split.frame(&pool, group).expect("room");
                    assert_index_frame(set, write, group, &frame.freeze(Path::Live));
                }
            }
        }
    }
}
