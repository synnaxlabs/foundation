//! A writer's frame, checked against the count of each group and split into one index
//! frame per present group (INDEX FRAMES).

use std::ops::Range;

use types::frame::key_set::KeySet;
use types::frame::{self, Draft, Form};
use types::sample::{Scalar, Type};

use crate::index::Refusal;

/// The buffers of [`Split`], kept from one frame to the next, so that a split makes no
/// heap allocation once they are large enough.
#[derive(Debug, Default)]
pub(crate) struct Scratch {
    /// Each series that fits its group's count, sorted by group and then entry once
    /// all are checked.
    series: Vec<Series>,
    /// Each present group, in group order.
    parts: Vec<Part>,
    /// The place in `parts` of each group, by group number. The place of an absent
    /// group is left from an earlier frame.
    places: Vec<usize>,
    /// The encoded series of a raw frame, back to back.
    bytes: Vec<u8>,
    /// The index stamps of each present group, back to back.
    stamps: Vec<[u8; 8]>,
    /// The entries and lengths of one index frame, for [`Draft::new`].
    lens: Vec<(usize, usize)>,
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
    /// Its index stamps in [`Scratch::stamps`].
    stamps: Range<usize>,
    /// The refusal of its first series that does not fit `count`.
    check: Result<(), Refusal>,
    made: bool,
}

impl Scratch {
    /// Checks each series of `draft`, a writer's frame of `set`, against the count of
    /// its group with `codec`, and encodes it when it is raw. Time is linear in the
    /// series bytes, plus O(n log n) for n present series.
    ///
    /// # Panics
    ///
    /// If `draft` is not of `set`, or holds a series whose type `codec` does not take.
    pub(crate) fn split<'a>(
        &'a mut self,
        set: &'a KeySet,
        mut draft: Draft,
    ) -> Split<'a> {
        assert_eq!(
            draft.key_set(),
            set.key(),
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
            draft: (draft.form() == Form::Encoded).then_some(draft),
        }
    }

    /// Records each present group of `draft` with its count.
    fn collect(&mut self, set: &KeySet, draft: &mut Draft) {
        self.series.clear();
        self.parts.clear();
        self.bytes.clear();
        self.stamps.clear();
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
                    stamps: 0..0,
                    check: Ok(()),
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
    /// `bytes`, and puts the stamps of each index in `stamps`.
    fn check(&mut self, set: &KeySet, draft: &mut Draft) {
        let form = draft.form();
        for (entry, bytes) in draft.iter_mut() {
            let group = set.entries()[entry].group;
            let part = &mut self.parts[self.places[to_usize(group)]];
            if part.check.is_err() {
                continue;
            }
            let scalar = scalar(set.entries()[entry].data_type);
            let index = set.index(entry) == entry;
            let count = to_usize(part.count);
            let start = self.bytes.len();
            let checked = match form {
                Form::Raw => {
                    encode(&mut self.bytes, scalar, count, bytes).inspect(|_| {
                        if index {
                            let (stamps, _) = bytes.as_chunks::<8>();
                            self.stamps.extend_from_slice(stamps);
                        }
                    })
                }
                Form::Encoded if index => decode(&mut self.stamps, count, bytes),
                Form::Encoded => {
                    codec::validate(scalar, count, bytes).map(|_| bytes.len())
                }
            };
            match checked {
                Ok(len) => {
                    self.series.push(Series {
                        group,
                        entry,
                        start,
                        len,
                    });
                    if index {
                        part.stamps = self.stamps.len() - count..self.stamps.len();
                    }
                }
                Err(error) => {
                    let channel = set.entries()[entry].key;
                    part.check = Err(Refusal::Codec { channel, error });
                }
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
    /// The writer's frame when it is encoded, as its series are the encoded bytes.
    /// `None` for a raw frame, or once the frame of its one group was given out.
    draft: Option<Draft>,
}

impl Split<'_> {
    /// Each present group, in group order, with the stamps of its index series, or
    /// [`Refusal::Codec`] for its first series that does not fit the group's count.
    pub(crate) fn groups(
        &self,
    ) -> impl Iterator<Item = (u32, Result<&[[u8; 8]], Refusal>)> {
        self.scratch.parts.iter().map(|part| {
            let stamps = part.check.clone();
            (
                part.group,
                stamps.map(|()| &self.scratch.stamps[part.stamps.clone()]),
            )
        })
    }

    /// The index frame of `group`: the writer's key set with only `group` present, its
    /// count, and its series encoded. The index frame of an encoded frame with one
    /// group is that frame, with no copy.
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
        let part = &scratch.parts[at];
        if let Err(error) = &part.check {
            panic!("group {group} failed its check: {error}");
        }
        assert!(
            !part.made,
            "the index frame of group {group} was made already"
        );
        if scratch.parts.len() == 1
            && let Some(draft) = self.draft.take()
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
        for ((_, out), series) in index.iter_mut().zip(series) {
            let bytes = match &mut self.draft {
                Some(draft) => draft
                    .series(series.entry)
                    .expect("invariant: a checked series is in the frame"),
                None => &mut scratch.bytes[series.start..series.start + series.len],
            };
            out.copy_from_slice(bytes);
        }
        index.set_count(group, part.count);
        scratch.parts[at].made = true;
        Ok(index)
    }
}

/// The scalar of a series of `data_type`.
///
/// # Panics
///
/// If `codec` does not take `data_type`.
fn scalar(data_type: Type) -> Scalar {
    match data_type {
        Type::Scalar(scalar) => scalar,
        other => panic!("codec does not take a series of {other:?} yet"),
    }
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
    bytes.resize(start + codec::max_len(scalar, values.len()), 0);
    let written =
        codec::Encoder::new(scalar).encode(count, values, &mut bytes[start..]);
    bytes.truncate(start + written.as_ref().copied().unwrap_or(0));
    written
}

/// Decodes `encoded`, `count` stamps, onto the end of `stamps`, and returns the length
/// of `encoded`.
fn decode(
    stamps: &mut Vec<[u8; 8]>,
    count: usize,
    encoded: &[u8],
) -> Result<usize, codec::Error> {
    let start = stamps.len();
    stamps.resize(start + count, [0; 8]);
    let out = stamps[start..].as_flattened_mut();
    let decoded = codec::decode(Scalar::Stamp, count, encoded, out);
    if decoded.is_err() {
        stamps.truncate(start);
    }
    decoded.map(|()| encoded.len())
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).expect("invariant: a usize holds a u32")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use proptest::prelude::*;
    use types::channel::{self, Slot};
    use types::frame::key_set::Group;
    use types::frame::{Form, Frame, Path, Range};

    use super::*;
    use crate::common::{interner, key, pool};

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
        let mut out = vec![0; codec::max_len(scalar, values.len())];
        let count = values.len() / scalar.width();
        let len = codec::Encoder::new(scalar)
            .encode(count, values, &mut out)
            .expect("values that fit the count");
        out.truncate(len);
        out
    }

    fn decoded(set: &KeySet, entry: usize, count: u32, bytes: &[u8]) -> Vec<u8> {
        let scalar = scalar_of(set, entry);
        let count = usize::try_from(count).expect("a small count");
        let mut out = vec![0; count * scalar.width()];
        codec::decode(scalar, count, bytes, &mut out).expect("an encoded series");
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
        interner().intern(&[zero, one])
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
        let set = interner().intern(&[
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
                let pool = pool(1 << 16);
                let mut scratch = Scratch::default();

                let split = scratch.split(&set, draft(&pool, &set, form, &write));

                let groups: Vec<(u32, Vec<[u8; 8]>)> = split
                    .groups()
                    .map(|(group, stamps)| {
                        (group, stamps.expect("a valid group").to_vec())
                    })
                    .collect();
                let expected = vec![
                    (0, stamps(&write[&0].series[&0])),
                    (1, stamps(&write[&1].series[&2])),
                ];
                assert_eq!(groups, expected, "{form:?}");
            }
        }

        #[test]
        fn gives_only_the_present_groups() {
            let set = two_groups();
            let write = BTreeMap::from([(1, both()[&1].clone())]);
            let pool = pool(1 << 16);
            let mut scratch = Scratch::default();

            let split = scratch.split(&set, draft(&pool, &set, Form::Raw, &write));

            let groups: Vec<u32> = split.groups().map(|(group, _)| group).collect();
            assert_eq!(groups, [1]);
        }

        mod when_a_series_does_not_fit_its_count {
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
                    let pool = pool(1 << 16);
                    let mut scratch = Scratch::default();

                    let split = scratch.split(&set, draft(&pool, &set, form, &write));

                    let groups: Vec<_> = split.groups().collect();
                    assert_eq!(expected.to_string(), message);
                    let channel = key(Slot::new(2));
                    let refusal = Refusal::Codec {
                        channel,
                        error: expected,
                    };
                    assert_eq!(groups[0], (0, Err(refusal)), "{form:?}");
                    let stamps = stamps(&write[&1].series[&2]);
                    assert_eq!(groups[1], (1, Ok(&stamps[..])), "{form:?}");
                }
            }

            #[test]
            fn refuses_an_encoded_index_of_too_few_stamps_with_the_decode_error() {
                let set = two_groups();
                let write = both();
                let pool = pool(1 << 16);
                let mut draft = draft(&pool, &set, Form::Encoded, &write);
                draft.set_count(1, 3);
                let mut scratch = Scratch::default();

                let split = scratch.split(&set, draft);

                let groups: Vec<_> = split.groups().collect();
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
                let refusal = Refusal::Codec {
                    channel,
                    error: expected,
                };
                assert_eq!(groups[1], (1, Err(refusal)));
                let stamps = stamps(&write[&0].series[&0]);
                assert_eq!(groups[0], (0, Ok(&stamps[..])));
            }

            #[test]
            fn refuses_a_series_whose_count_was_never_set() {
                let set = two_groups();
                let write = both();
                let pool = pool(1 << 16);
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

                let split = scratch.split(&set, draft);

                let groups: Vec<_> = split.groups().collect();
                let expected = codec::Error::Length {
                    expected: 0,
                    actual: 16,
                };
                let channel = key(Slot::new(3));
                let refusal = Refusal::Codec {
                    channel,
                    error: expected,
                };
                assert_eq!(groups, [(1, Err(refusal))]);
            }
        }
    }

    mod frame {
        use super::*;

        #[test]
        fn makes_the_index_frame_of_each_group() {
            let set = two_groups();
            let write = both();
            for form in [Form::Raw, Form::Encoded] {
                let pool = pool(1 << 16);
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
                let pool = pool(1 << 16);
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
            let pool = pool(1 << 16);
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
            let pool = pool(1 << 16);
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
            let pool = pool(1 << 16);
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
                let mut interner = interner();
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
                let pool = pool(1 << 16);

                Scratch::default().split(&set, draft(&pool, &other, Form::Raw, &write));
            }

            #[test]
            #[should_panic(expected = "group 1 is absent from the frame")]
            fn on_an_absent_group() {
                let set = two_groups();
                let write = BTreeMap::from([(0, both()[&0].clone())]);
                let pool = pool(1 << 16);
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
                let pool = pool(1 << 16);
                let mut scratch = Scratch::default();
                let mut split =
                    scratch.split(&set, draft(&pool, &set, Form::Raw, &write));

                drop(split.frame(&pool, 0));
            }

            #[test]
            #[should_panic(expected = "the index frame of group 1 was made already")]
            fn on_a_group_made_twice() {
                let set = two_groups();
                let pool = pool(1 << 16);
                let mut scratch = Scratch::default();
                let mut split =
                    scratch.split(&set, draft(&pool, &set, Form::Raw, &both()));
                let _made = split.frame(&pool, 1).expect("room");

                drop(split.frame(&pool, 1));
            }

            #[test]
            #[should_panic(expected = "codec does not take a series of String yet")]
            fn on_a_series_of_a_type_codec_does_not_take() {
                let set = interner().intern(&[Group {
                    index: key(Slot::new(1)),
                    data: &[(key(Slot::new(2)), Type::String)],
                }]);
                let samples = Samples {
                    count: 1,
                    series: BTreeMap::from([
                        (0, 5_u64.to_le_bytes().to_vec()),
                        (1, b"text".to_vec()),
                    ]),
                };
                let write = BTreeMap::from([(0, samples)]);
                let pool = pool(1 << 16);

                Scratch::default().split(&set, draft(&pool, &set, Form::Raw, &write));
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
                (scratch.stamps.as_ptr().addr(), scratch.stamps.capacity()),
                (scratch.lens.as_ptr().addr(), scratch.lens.capacity()),
            ]
        }

        #[test]
        fn keeps_its_buffers_from_a_larger_write() {
            let (large, many) = many_series();
            let small = two_groups();
            for form in [Form::Raw, Form::Encoded] {
                let pool = pool(1 << 16);
                let mut scratch = Scratch::default();
                let mut split =
                    scratch.split(&large, draft(&pool, &large, form, &many));
                for group in [0, 1] {
                    drop(split.frame(&pool, group).expect("room"));
                }
                let kept = buffers(&scratch);

                let mut split =
                    scratch.split(&small, draft(&pool, &small, form, &both()));
                for group in [0, 1] {
                    drop(split.frame(&pool, group).expect("room"));
                }

                assert_eq!(buffers(&scratch), kept, "{form:?}");
            }
        }
    }

    const SCALARS: [Scalar; 14] = [
        Scalar::Bool,
        Scalar::I8,
        Scalar::I16,
        Scalar::I32,
        Scalar::I64,
        Scalar::U8,
        Scalar::U16,
        Scalar::U32,
        Scalar::U64,
        Scalar::F32,
        Scalar::F64,
        Scalar::Stamp,
        Scalar::Span,
        Scalar::Uuid,
    ];

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
                let set = interner().intern(&shapes);
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
            let pool = pool(1 << 20);
            let mut scratch = Scratch::default();
            let mut split = scratch.split(&set, draft(&pool, &set, form, &write));

            let groups: Vec<(u32, Vec<[u8; 8]>)> = split
                .groups()
                .map(|(group, stamps)| (group, stamps.expect("a valid group").to_vec()))
                .collect();
            let expected: Vec<(u32, Vec<[u8; 8]>)> = write
                .iter()
                .map(|(&group, samples)| {
                    let index = set.groups()[usize::try_from(group).expect("small")];
                    (group, stamps(&samples.series[&index]))
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
            let pool = pool(1 << 20);
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
