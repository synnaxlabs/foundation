//! The data entry of an index frame. Its body is a header that describes each series by
//! its channel and type, then the frame's series bytes.

use std::num::NonZeroU32;
use std::sync::Arc;

use block::Block;
use buffer::{Entry, Samples, Stored};
use types::channel;
use types::frame::key_set::{self, KeySet};
use types::frame::{self, Form, Frame, Layout, Path};
use types::hash;
use types::sample::{Scalar, Sides, Type};
use types::time::Stamp;

/// The buffer tag of a data entry.
const TAG: u8 = 0;

/// Bytes of the series count that starts a body.
const COUNT: usize = 4;
/// Bytes of one series descriptor: channel, kind, element, `n`, and end.
const DESCRIPTOR: usize = 26;

/// Offsets in a descriptor.
mod at {
    pub(super) const KIND: usize = 16;
    pub(super) const ELEMENT: usize = 17;
    pub(super) const N: usize = 18;
    pub(super) const END: usize = 22;
}

/// The buffer entry that stores `frame`, an encoded index frame of one group of `set`
/// whose newest stamp is `last`, at mesh time `stored_at`. Its parts are a header
/// block from `pool`, then [`Frame::body`]. Copies no series byte.
///
/// # Errors
///
/// [`block::Error`] when `pool` has no block for the header.
///
/// # Panics
///
/// If `frame` is not encoded, not of `set`, of more than one group, of no series, or
/// of no sample.
pub(crate) fn entry(
    pool: &block::Pool,
    frame: &Frame,
    set: &KeySet,
    last: Stamp,
    stored_at: Stamp,
) -> Result<Entry, block::Error> {
    let Some((entry, _)) = frame.ends().next() else {
        panic!("the frame has no series");
    };
    let index = &set.entries()[set.index(entry)];
    let Some(range) = frame.range(index.group) else {
        unreachable!("invariant: a frame holds the index of each series");
    };
    let Some(len) = NonZeroU32::new(range.count) else {
        panic!("the frame has no sample");
    };
    let parts = body(pool, frame, set)?;
    Ok(Entry {
        index: index.key,
        slot: index.slot,
        path: frame.path(),
        first: range.seq,
        samples: Some(Samples {
            len,
            last: Some(last),
        }),
        stored_at,
        tag: TAG,
        parts: parts.into(),
    })
}

/// The stored body of `frame`, an encoded index frame of `set`, as two parts: a header
/// block from `pool`, then [`Frame::body`].
///
/// # Errors
///
/// [`block::Error`] when `pool` has no block for the header.
///
/// # Panics
///
/// If `frame` is not encoded, not of `set`, or of more than one group.
fn body(
    pool: &block::Pool,
    frame: &Frame,
    set: &KeySet,
) -> Result<[Block; 2], block::Error> {
    assert_eq!(frame.form(), Form::Encoded, "the frame is not encoded");
    assert_eq!(
        frame.key_set(),
        set.key(),
        "the frame is not of the key set"
    );
    let entries = set.entries();
    let group = frame.ends().next().map(|(entry, _)| entries[entry].group);
    let count = frame.ends().count();
    let mut head = pool.alloc(COUNT + DESCRIPTOR * count)?;
    let (start, descriptors) = head.split_at_mut(COUNT);
    start.copy_from_slice(&to_u32(count).to_le_bytes());
    let (descriptors, _) = descriptors.as_chunks_mut::<DESCRIPTOR>();
    for (descriptor, (entry, end)) in descriptors.iter_mut().zip(frame.ends()) {
        let entry = &entries[entry];
        assert_eq!(
            Some(entry.group),
            group,
            "the frame has more than one group"
        );
        let (kind, element, n) = codes(entry.data_type);
        let channel = entry.key.as_u128().to_le_bytes();
        descriptor[..at::KIND].copy_from_slice(&channel);
        descriptor[at::KIND] = kind;
        descriptor[at::ELEMENT] = element;
        descriptor[at::N..at::END].copy_from_slice(&n.to_le_bytes());
        descriptor[at::END..].copy_from_slice(&to_u32(end).to_le_bytes());
    }
    Ok([head.freeze(), frame.body()])
}

/// One series of a stored body.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Series<'a> {
    /// The series' channel.
    pub(crate) channel: channel::Key,
    /// The layout of its samples when it was written.
    pub(crate) data_type: Type,
    /// Its bytes, as `codec` encodes them.
    pub(crate) bytes: &'a [u8],
}

/// Each series of `body`, the body of an entry that [`entry`] made, in entry order.
/// Copies nothing.
///
/// # Panics
///
/// If `body` is shorter than its header. The iterator panics on an unknown kind or
/// scalar, and on ends that do not fit the series bytes. Bytes from another node must
/// be checked before they reach `read`.
pub(crate) fn read(body: &[u8]) -> impl Iterator<Item = Series<'_>> {
    let Some(count) = body.first_chunk() else {
        panic!("the stored body of {} bytes has no count", body.len());
    };
    let header = COUNT + DESCRIPTOR * to_usize(u32::from_le_bytes(*count));
    let Some((head, series)) = body.split_at_checked(header) else {
        panic!(
            "the stored body of {} bytes is shorter than its header of {header} bytes",
            body.len()
        );
    };
    let (descriptors, _) = head[COUNT..].as_chunks::<DESCRIPTOR>();
    let ends = descriptors.iter().map(|descriptor| {
        let end = u32::from_le_bytes(field(descriptor, at::END));
        (descriptor, to_usize(end))
    });
    frame::split(series, ends).map(|(descriptor, bytes)| Series {
        channel: channel::Key::from_u128(u128::from_le_bytes(field(descriptor, 0))),
        data_type: data_type(descriptor),
        bytes,
    })
}

/// The most key sets whose maps one [`Decoder`] keeps.
const SETS: usize = 4;

/// Makes the frames of stored data entries again, of the key set that the caller
/// gives. It keeps the map of each of the last [`SETS`] key sets it was given and of
/// the channels of the last entry, so a run of entries of one key set and one list of
/// channels maps each series once. Each key set given to one decoder comes from one
/// interner.
#[derive(Debug, Default)]
pub(crate) struct Decoder {
    /// The key and the entry of each channel and type of each of the last [`SETS`]
    /// key sets given, the key set of the last frame first.
    maps: Vec<(key_set::Key, EntryOf)>,
    /// The channel and type of each series of the last entry, in entry order.
    channels: Vec<(channel::Key, Type)>,
    /// The entry in the key set of the last frame and the position in `channels` of
    /// each series that the key set holds, in increasing entry order.
    kept: Vec<(usize, usize)>,
    /// The start and length of each series in the body of the entry.
    spans: Vec<(usize, usize)>,
    lens: Vec<(usize, usize)>,
}

/// The entry in a key set of each channel and type.
type EntryOf = hash::Map<(channel::Key, Type), usize>;

impl Decoder {
    /// The live frame of key set `set` that `stored` holds, an entry of the index of
    /// the one group of `set`. It drops each series whose channel and type `set` does
    /// not hold. `None` when `stored` is not a data entry with samples.
    ///
    /// # Errors
    ///
    /// [`block::Error`] when `pool` has no block for the frame.
    ///
    /// # Panics
    ///
    /// As [`read`] does, and if `set` does not hold the index series of `stored`.
    pub(crate) fn frame(
        &mut self,
        pool: &block::Pool,
        set: &Arc<KeySet>,
        stored: &Stored,
    ) -> Result<Option<Frame>, block::Error> {
        if stored.tag != TAG {
            return Ok(None);
        }
        let Some(samples) = stored.samples else {
            return Ok(None);
        };
        let body = &stored.bytes;
        self.map(set, body);
        self.spans.clear();
        let start = body.as_ptr().addr();
        let spans = read(body)
            .map(|series| (series.bytes.as_ptr().addr() - start, series.bytes.len()));
        self.spans.extend(spans);
        self.lens.clear();
        let lens = self
            .kept
            .iter()
            .map(|&(entry, at)| (entry, self.spans[at].1));
        self.lens.extend(lens);
        let layout = Layout::new(set, &self.lens).unwrap_or_else(|error| {
            panic!(
                "the entry has no frame of key set {}: {error}",
                set.key().get()
            )
        });
        let mut draft = layout.draft(pool, Form::Encoded)?;
        for ((_, to), &(_, at)) in draft.iter_mut().zip(&self.kept) {
            let (from, len) = self.spans[at];
            to.copy_from_slice(&body[from..from + len]);
        }
        draft.set_count(0, samples.len.get());
        draft.set_seq(0, stored.first);
        Ok(Some(draft.freeze(Path::Live)))
    }

    /// Maps the series of `body`, the body of an entry, to the entries of `set`.
    fn map(&mut self, set: &KeySet, body: &[u8]) {
        let channels = || read(body).map(|series| (series.channel, series.data_type));
        let key = set.key();
        let last = self.maps.first().map(|&(last, _)| last);
        if last == Some(key) && channels().eq(self.channels.iter().copied()) {
            return;
        }
        let at = self.maps.iter().position(|&(other, _)| other == key);
        let at = at.unwrap_or_else(|| {
            let evicted = (self.maps.len() == SETS).then(|| self.maps.pop());
            let mut entry_of =
                evicted.flatten().map(|(_, map)| map).unwrap_or_default();
            entry_of.clear();
            let entries = set.entries().iter().enumerate();
            entry_of
                .extend(entries.map(|(at, entry)| ((entry.key, entry.data_type), at)));
            self.maps.push((key, entry_of));
            self.maps.len() - 1
        });
        self.maps[..=at].rotate_right(1);
        let entry_of = &self.maps[0].1;
        self.channels.clear();
        self.channels.extend(channels());
        self.kept.clear();
        let kept = self
            .channels
            .iter()
            .enumerate()
            .filter_map(|(at, channel)| {
                entry_of.get(channel).map(|&entry| (entry, at))
            });
        self.kept.extend(kept);
        self.kept.sort_unstable();
    }
}

/// The kind code, element code, and `n` of `data_type`. The `n` of a matrix is
/// `rows | columns << 16`.
fn codes(data_type: Type) -> (u8, u8, u32) {
    match data_type {
        Type::Scalar(element) => (0, code(element), 0),
        Type::Array { element, len } => (1, code(element), len),
        Type::List { element, max } => (2, code(element), max),
        Type::String => (3, 0, 0),
        Type::Bytes => (4, 0, 0),
        Type::Matrix {
            element,
            sides: Sides { rows, columns },
        } => (5, code(element), u32::from(rows) | u32::from(columns) << 16),
    }
}

/// The type that `descriptor` holds.
fn data_type(descriptor: &[u8; DESCRIPTOR]) -> Type {
    let (kind, element) = (descriptor[at::KIND], descriptor[at::ELEMENT]);
    let n = u32::from_le_bytes(field(descriptor, at::N));
    match kind {
        0 => Type::Scalar(scalar(element)),
        1 => Type::Array {
            element: scalar(element),
            len: n,
        },
        2 => Type::List {
            element: scalar(element),
            max: n,
        },
        3 => Type::String,
        4 => Type::Bytes,
        5 => Type::Matrix {
            element: scalar(element),
            sides: Sides {
                rows: u16::from_le_bytes(field(descriptor, at::N)),
                columns: u16::from_le_bytes(field(descriptor, at::N + 2)),
            },
        },
        _ => panic!("the stored body has an unknown kind {kind}"),
    }
}

const fn code(scalar: Scalar) -> u8 {
    match scalar {
        Scalar::Bool => 0,
        Scalar::I8 => 1,
        Scalar::I16 => 2,
        Scalar::I32 => 3,
        Scalar::I64 => 4,
        Scalar::U8 => 5,
        Scalar::U16 => 6,
        Scalar::U32 => 7,
        Scalar::U64 => 8,
        Scalar::F32 => 9,
        Scalar::F64 => 10,
        Scalar::Stamp => 11,
        Scalar::Span => 12,
        Scalar::Uuid => 13,
    }
}

fn scalar(code: u8) -> Scalar {
    match code {
        0 => Scalar::Bool,
        1 => Scalar::I8,
        2 => Scalar::I16,
        3 => Scalar::I32,
        4 => Scalar::I64,
        5 => Scalar::U8,
        6 => Scalar::U16,
        7 => Scalar::U32,
        8 => Scalar::U64,
        9 => Scalar::F32,
        10 => Scalar::F64,
        11 => Scalar::Stamp,
        12 => Scalar::Span,
        13 => Scalar::Uuid,
        _ => panic!("the stored body has an unknown scalar {code}"),
    }
}

/// The `N` bytes of `descriptor` from `at`.
fn field<const N: usize>(descriptor: &[u8; DESCRIPTOR], at: usize) -> [u8; N] {
    *descriptor[at..]
        .first_chunk()
        .expect("invariant: each field is inside the descriptor")
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).expect("invariant: a frame of at most u32::MAX bytes")
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).expect("invariant: a usize holds a u32")
}

#[cfg(test)]
mod tests {
    use std::iter;
    use std::sync::Arc;

    use proptest::collection::{btree_set, vec};
    use proptest::prelude::*;

    use types::channel::Slot;
    use types::frame::key_set::{Group, Interner};
    use types::frame::{Draft, Path};

    use super::*;
    use crate::common::{
        SCALARS, create_interner, create_pool, data_type, intern, key,
    };
    use crate::split;

    /// A live frame of `set` in `form` with each present entry and its bytes, in
    /// entry order.
    fn draft(
        pool: &block::Pool,
        set: &KeySet,
        form: Form,
        series: &[(usize, &[u8])],
    ) -> Frame {
        let lens: Vec<_> = series.iter().map(|&(e, bytes)| (e, bytes.len())).collect();
        let mut draft = Draft::new(pool, set, form, &lens).expect("a valid frame");
        for ((_, to), (_, from)) in draft.iter_mut().zip(series) {
            to.copy_from_slice(from);
        }
        draft.freeze(Path::Live)
    }

    /// An encoded live frame of `set` with each present entry and its bytes.
    fn frame(pool: &block::Pool, set: &KeySet, series: &[(usize, &[u8])]) -> Frame {
        draft(pool, set, Form::Encoded, series)
    }

    /// The two parts of a stored body, joined as the buffer reads them back.
    fn joined(parts: &[Block; 2]) -> Vec<u8> {
        [&parts[0][..], &parts[1][..]].concat()
    }

    /// The stored body of one index series of 8 bytes: 38 bytes.
    fn stored() -> Vec<u8> {
        let set = intern(&[Group {
            index: key(Slot::new(1)),
            data: &[],
        }]);
        let pool = create_pool(4096);
        let frame = frame(&pool, &set, &[(0, &[9; 8])]);
        joined(&body(&pool, &frame, &set).expect("room"))
    }

    fn matrix(element: Scalar, rows: u16, columns: u16) -> Type {
        Type::Matrix {
            element,
            sides: Sides { rows, columns },
        }
    }

    mod entry {
        use super::*;

        #[test]
        fn stores_the_body_at_the_range_of_the_frame_index() {
            // The data channel's slot is below its index's, so the first series of
            // the frame is not the index.
            let data = [(key(Slot::new(2)), Type::Scalar(Scalar::U8))];
            let set = intern(&[
                Group {
                    index: key(Slot::new(1)),
                    data: &[],
                },
                Group {
                    index: key(Slot::new(3)),
                    data: &data,
                },
            ]);
            let pool = create_pool(4096);
            let lens = [(1, 2), (2, 16)];
            let mut draft =
                Draft::new(&pool, &set, Form::Encoded, &lens).expect("a valid frame");
            draft.set_count(1, 2);
            draft.set_seq(1, 40);
            let frame = draft.freeze(Path::Backfill);
            let last = Stamp::from_nanos(7);
            let stored_at = Stamp::from_nanos(9);

            let entry = entry(&pool, &frame, &set, last, stored_at).expect("room");

            let place = (entry.index, entry.slot, entry.path, entry.first);
            assert_eq!(place, (key(Slot::new(3)), Slot::new(3), Path::Backfill, 40));
            let samples = Samples {
                len: NonZeroU32::new(2).expect("not 0"),
                last: Some(last),
            };
            assert_eq!(
                (entry.samples, entry.stored_at, entry.tag),
                (Some(samples), stored_at, 0)
            );
            let parts: Vec<_> = entry.parts.into_iter().collect();
            let body = body(&pool, &frame, &set).expect("room");
            assert_eq!([&parts[0][..], &parts[1][..]], [&body[0][..], &body[1][..]]);
        }

        #[test]
        #[should_panic(expected = "the frame has no series")]
        fn panics_on_a_frame_of_no_series_also_with_a_full_pool() {
            let set = intern(&[Group {
                index: key(Slot::new(1)),
                data: &[],
            }]);
            let frames = create_pool(4096);
            let frame = frame(&frames, &set, &[]);
            let heads = create_pool(4096);
            let mut held = Vec::new();
            while let Ok(block) = heads.alloc(COUNT) {
                held.push(block);
            }
            drop(entry(
                &heads,
                &frame,
                &set,
                Stamp::from_nanos(0),
                Stamp::from_nanos(0),
            ));
        }

        #[test]
        #[should_panic(expected = "the frame has no sample")]
        fn panics_on_a_frame_of_no_sample() {
            let set = intern(&[Group {
                index: key(Slot::new(1)),
                data: &[],
            }]);
            let pool = create_pool(4096);
            let draft = Draft::new(&pool, &set, Form::Encoded, &[(0, 8)])
                .expect("a valid frame");
            let frame = draft.freeze(Path::Live);
            drop(entry(
                &pool,
                &frame,
                &set,
                Stamp::from_nanos(0),
                Stamp::from_nanos(0),
            ));
        }

        /// The body bytes of the entry of a writer's raw frame of one sample on an
        /// index and on each of 256 data channels of `scalar`, as a shard splits and
        /// stores it.
        fn polled(scalar: Scalar) -> usize {
            let mut interner = Interner::new();
            let index = channel::Key::from_u128(1);
            interner.slots().index(index);
            let data: Vec<_> = (2..258)
                .map(|n| (channel::Key::from_u128(n), Type::Scalar(scalar)))
                .collect();
            for &(key, data_type) in &data {
                interner.slots().data(key, data_type);
            }
            let set = interner.intern(&[Group { index, data: &data }]);
            let pool = create_pool(1 << 20);
            let lens: Vec<_> = (0..set.entries().len())
                .map(|entry| {
                    let index = set.index(entry) == entry;
                    (entry, if index { 8 } else { scalar.width() })
                })
                .collect();
            let mut draft =
                Draft::new(&pool, &set, Form::Raw, &lens).expect("a valid frame");
            for (_, bytes) in draft.iter_mut() {
                bytes.fill(1);
            }
            let group = set.entries()[0].group;
            draft.set_count(group, 1);
            let mut scratch = split::Scratch::default();
            let mut split = scratch.split(&set, &mut split::encoders(&set), draft);
            let frame = split.frame(&pool, group).expect("room");
            let frame = frame.freeze(Path::Live);
            let stamp = Stamp::from_nanos(1);
            let entry = entry(&pool, &frame, &set, stamp, stamp).expect("room");
            entry.parts.into_iter().map(|part| part.len()).sum()
        }

        #[test]
        fn stores_a_poll_of_256_channels_in_26_bytes_a_series_and_its_vector() {
            // The count, and a descriptor of 26 bytes for each of the 257 series: 6686
            // bytes. Then 16 bytes of the index vector, and the vector of each data
            // series, each but the last padded to 8 bytes.
            let poll =
                |vector: usize| 6686 + 16 + 255 * vector.next_multiple_of(8) + vector;
            let expected = [
                (Scalar::Bool, poll(3)),
                (Scalar::I8, poll(3)),
                (Scalar::I16, poll(4)),
                (Scalar::I32, poll(8)),
                (Scalar::I64, poll(16)),
                (Scalar::U8, poll(3)),
                (Scalar::U16, poll(4)),
                (Scalar::U32, poll(8)),
                (Scalar::U64, poll(16)),
                (Scalar::F32, poll(8)),
                (Scalar::F64, poll(16)),
                (Scalar::Stamp, poll(16)),
                (Scalar::Span, poll(16)),
                (Scalar::Uuid, poll(32)),
            ];
            let stored = SCALARS.map(|scalar| (scalar, polled(scalar)));
            assert_eq!(stored, expected);
        }
    }

    mod body {
        use super::*;

        #[test]
        fn lays_out_the_header_then_the_series() {
            let index = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
            let channel = [
                16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
            ];
            let data = [(
                channel::Key::from_u128(u128::from_le_bytes(channel)),
                Type::Scalar(Scalar::U8),
            )];
            let set = Interner::new().intern(&[Group {
                index: channel::Key::from_u128(u128::from_le_bytes(index)),
                data: &data,
            }]);
            let pool = create_pool(4096);
            let stamps = [7; 16];
            let frame = frame(&pool, &set, &[(0, &stamps), (1, &[1, 2, 3])]);
            let parts = body(&pool, &frame, &set).expect("room");
            let header = [
                &[2, 0, 0, 0][..],
                &index,
                &[0, 11, 0, 0, 0, 0],
                &[16, 0, 0, 0],
                &channel,
                &[0, 5, 0, 0, 0, 0],
                &[19, 0, 0, 0],
            ]
            .concat();
            assert_eq!(parts[0][..], header);
            assert_eq!(parts[1][..], frame.body()[..]);
        }

        #[test]
        fn writes_and_reads_each_type_by_its_codes() {
            let cases = [
                (Type::Scalar(Scalar::Bool), [0, 0, 0, 0, 0, 0]),
                (Type::Scalar(Scalar::I8), [0, 1, 0, 0, 0, 0]),
                (Type::Scalar(Scalar::I16), [0, 2, 0, 0, 0, 0]),
                (Type::Scalar(Scalar::I32), [0, 3, 0, 0, 0, 0]),
                (Type::Scalar(Scalar::I64), [0, 4, 0, 0, 0, 0]),
                (Type::Scalar(Scalar::U8), [0, 5, 0, 0, 0, 0]),
                (Type::Scalar(Scalar::U16), [0, 6, 0, 0, 0, 0]),
                (Type::Scalar(Scalar::U32), [0, 7, 0, 0, 0, 0]),
                (Type::Scalar(Scalar::U64), [0, 8, 0, 0, 0, 0]),
                (Type::Scalar(Scalar::F32), [0, 9, 0, 0, 0, 0]),
                (Type::Scalar(Scalar::F64), [0, 10, 0, 0, 0, 0]),
                (Type::Scalar(Scalar::Stamp), [0, 11, 0, 0, 0, 0]),
                (Type::Scalar(Scalar::Span), [0, 12, 0, 0, 0, 0]),
                (Type::Scalar(Scalar::Uuid), [0, 13, 0, 0, 0, 0]),
                (
                    Type::Array {
                        element: Scalar::F32,
                        len: 3,
                    },
                    [1, 9, 3, 0, 0, 0],
                ),
                (matrix(Scalar::F32, 2, 3), [5, 9, 2, 0, 3, 0]),
                (matrix(Scalar::U8, 0x0102, 0x0304), [5, 5, 2, 1, 4, 3]),
                (
                    Type::List {
                        element: Scalar::U16,
                        max: 0x0102_0304,
                    },
                    [2, 6, 4, 3, 2, 1],
                ),
                (Type::String, [3, 0, 0, 0, 0, 0]),
                (Type::Bytes, [4, 0, 0, 0, 0, 0]),
            ];
            let data: Vec<_> = (2..)
                .zip(cases)
                .map(|(slot, (data_type, _))| (key(Slot::new(slot)), data_type))
                .collect();
            let set = intern(&[Group {
                index: key(Slot::new(1)),
                data: &data,
            }]);
            let pool = create_pool(4096);
            let series: Vec<(usize, &[u8])> = (0..set.entries().len())
                .map(|entry| (entry, &[][..]))
                .collect();
            let frame = frame(&pool, &set, &series);
            let parts = body(&pool, &frame, &set).expect("room");

            let (descriptors, rest) = parts[0][COUNT..].as_chunks::<DESCRIPTOR>();
            assert!(rest.is_empty(), "the header is whole descriptors");
            let written: Vec<[u8; 6]> = descriptors
                .iter()
                .map(|descriptor| field(descriptor, at::KIND))
                .collect();
            let codes = cases.map(|(_, codes)| codes);
            let expected: Vec<_> =
                iter::once([0, 11, 0, 0, 0, 0]).chain(codes).collect();
            assert_eq!(written, expected);

            let types: Vec<_> = read(&joined(&parts)).map(|s| s.data_type).collect();
            let expected: Vec<_> = set.entries().iter().map(|e| e.data_type).collect();
            assert_eq!(types, expected);
        }

        #[test]
        fn returns_the_pool_error_when_the_pool_is_full() {
            let set = intern(&[Group {
                index: key(Slot::new(1)),
                data: &[],
            }]);
            let frames = create_pool(4096);
            let frame = frame(&frames, &set, &[(0, &[0; 8])]);
            let heads = create_pool(4096);
            let mut held = Vec::new();
            while let Ok(block) = heads.alloc(COUNT + DESCRIPTOR) {
                held.push(block);
            }
            let available = 4096 - heads.committed();

            let error = body(&heads, &frame, &set).expect_err("full");

            assert_eq!(
                error,
                block::Error::Exhausted {
                    requested: 30,
                    available,
                }
            );
            assert_eq!(
                error.to_string(),
                format!("pool is full: asked for 30 bytes, {available} bytes free")
            );
        }

        mod when_misused {
            use super::*;

            #[test]
            #[should_panic(expected = "the frame is not encoded")]
            fn panics_on_a_raw_frame() {
                let set = intern(&[Group {
                    index: key(Slot::new(1)),
                    data: &[],
                }]);
                let pool = create_pool(4096);
                let frame = draft(&pool, &set, Form::Raw, &[(0, &[0; 8])]);
                drop(body(&pool, &frame, &set));
            }

            #[test]
            #[should_panic(expected = "the frame is not of the key set")]
            fn panics_on_a_frame_of_another_key_set() {
                let mut interner = create_interner(&[1, 2], &[]);
                let [of, other] = [1, 2].map(|slot| {
                    interner.intern(&[Group {
                        index: key(Slot::new(slot)),
                        data: &[],
                    }])
                });
                let pool = create_pool(4096);
                let frame = frame(&pool, &of, &[(0, &[0; 8])]);
                drop(body(&pool, &frame, &other));
            }

            #[test]
            #[should_panic(expected = "the frame has more than one group")]
            fn panics_on_a_frame_of_two_groups() {
                let set = intern(&[1, 2].map(|slot| Group {
                    index: key(Slot::new(slot)),
                    data: &[],
                }));
                let pool = create_pool(4096);
                let frame = frame(&pool, &set, &[(0, &[0; 8]), (1, &[0; 8])]);
                drop(body(&pool, &frame, &set));
            }
        }
    }

    mod read {
        use super::*;

        #[test]
        fn reads_the_body_of_one_series() {
            let body = stored();
            let series: Vec<_> = read(&body).collect();
            let expected = Series {
                channel: key(Slot::new(1)),
                data_type: Type::Scalar(Scalar::Stamp),
                bytes: &[9; 8],
            };
            assert_eq!(series, [expected]);
        }

        mod when_damaged {
            use super::*;

            #[test]
            #[should_panic(expected = "the stored body of 3 bytes has no count")]
            fn panics_on_a_body_without_a_count() {
                read(&stored()[..3]).for_each(drop);
            }

            #[test]
            #[should_panic(
                expected = "29 bytes is shorter than its header of 30 bytes"
            )]
            fn panics_on_a_body_shorter_than_its_header() {
                read(&stored()[..29]).for_each(drop);
            }

            #[test]
            #[should_panic(expected = "the stored body has an unknown kind 6")]
            fn panics_on_an_unknown_kind() {
                let mut body = stored();
                body[COUNT + at::KIND] = 6;
                read(&body).for_each(drop);
            }

            #[test]
            #[should_panic(expected = "the stored body has an unknown scalar 14")]
            fn panics_on_an_unknown_scalar() {
                let mut body = stored();
                body[COUNT + at::ELEMENT] = 14;
                read(&body).for_each(drop);
            }

            #[test]
            #[should_panic(expected = "the end 9 is past the body of 8 bytes")]
            fn panics_on_an_end_past_the_series_bytes() {
                let mut body = stored();
                body[COUNT + at::END] = 9;
                read(&body).for_each(drop);
            }

            #[test]
            #[should_panic(
                expected = "the last end 8 is not the end of the body of 9 bytes"
            )]
            fn panics_on_bytes_after_the_last_series() {
                let mut body = stored();
                body.push(0);
                read(&body).for_each(drop);
            }
        }
    }

    mod decoder {
        use types::frame::Range;

        use super::*;

        const I64: Type = Type::Scalar(Scalar::I64);

        /// A data channel of type `I64` for each slot of `slots`.
        fn data(slots: &[u32]) -> Vec<(channel::Key, Type)> {
            slots.iter().map(|&n| (key(Slot::new(n)), I64)).collect()
        }

        /// The stored entry of one sample at `seq` of each series of `set`, as the
        /// buffer reads it back.
        fn stored(pool: &block::Pool, set: &KeySet, seq: u64) -> (Frame, Stored) {
            let series: Vec<_> = (0..set.entries().len()).map(|e| (e, 8)).collect();
            let mut draft =
                Draft::new(pool, set, Form::Encoded, &series).expect("a valid frame");
            for ((e, to), byte) in draft.iter_mut().zip(1..) {
                to.fill(byte + u8::try_from(e).expect("few entries"));
            }
            draft.set_count(0, 1);
            draft.set_seq(0, seq);
            let frame = draft.freeze(Path::Live);
            let bytes = joined(&body(pool, &frame, set).expect("room"));
            let mut block = pool.alloc(bytes.len()).expect("room");
            block.copy_from_slice(&bytes);
            let stored = Stored {
                first: seq,
                samples: Some(Samples {
                    len: NonZeroU32::MIN,
                    last: Some(Stamp::from_nanos(9)),
                }),
                stored_at: Stamp::from_nanos(9),
                tag: TAG,
                bytes: block.freeze(),
            };
            (frame, stored)
        }

        /// The key and bytes of each present entry of `frame`, of key set `set`.
        fn series(frame: &Frame, set: &KeySet) -> Vec<(channel::Key, Vec<u8>)> {
            let entries = set.entries();
            frame
                .iter()
                .map(|(e, bytes)| (entries[e].key, bytes.to_vec()))
                .collect()
        }

        #[test]
        fn makes_the_frame_of_a_data_entry_again() {
            let data = data(&[2, 3]);
            let groups = [Group {
                index: key(Slot::new(1)),
                data: &data,
            }];
            let mut interner = create_interner(&[1], &groups);
            let set = interner.intern(&groups);
            let pool = create_pool(1 << 16);
            let (frame, stored) = stored(&pool, &set, 5);

            let decoded = Decoder::default().frame(&pool, &set, &stored);

            let decoded = decoded.expect("room").expect("a data entry");
            assert_eq!(decoded.key_set(), frame.key_set());
            assert_eq!(decoded.path(), Path::Live);
            assert_eq!(decoded.form(), Form::Encoded);
            assert_eq!(decoded.range(0), Some(Range { seq: 5, count: 1 }));
            assert_eq!(
                decoded.iter().collect::<Vec<_>>(),
                frame.iter().collect::<Vec<_>>()
            );
        }

        #[test]
        fn makes_no_frame_of_a_caller_record_or_another_tag() {
            let groups = [Group {
                index: key(Slot::new(1)),
                data: &[],
            }];
            let mut interner = create_interner(&[1], &groups);
            let set = interner.intern(&groups);
            let pool = create_pool(1 << 16);
            let (_, stored) = stored(&pool, &set, 5);
            let mut decoder = Decoder::default();
            for stored in [
                Stored {
                    samples: None,
                    ..stored.clone()
                },
                Stored {
                    tag: 1,
                    ..stored.clone()
                },
            ] {
                let made = decoder.frame(&pool, &set, &stored);
                assert!(made.expect("no block taken").is_none(), "{stored:?}");
            }
        }

        /// A channel defined again with a new type has a new slot, so the live path
        /// gives a reader of the new type no series of the old one. Neither does this.
        #[test]
        fn drops_each_series_of_a_channel_or_type_the_set_does_not_hold() {
            let stored_data = data(&[2, 3, 4]);
            let index = key(Slot::new(1));
            let groups = [Group {
                index,
                data: &stored_data,
            }];
            let mut interner = create_interner(&[1], &groups);
            let written = interner.intern(&groups);
            let f64 = Type::Scalar(Scalar::F64);
            let now = [(key(Slot::new(2)), f64), (key(Slot::new(4)), I64)];
            let set = interner.intern(&[Group { index, data: &now }]);
            let pool = create_pool(1 << 16);
            let (frame, stored) = stored(&pool, &written, 5);

            let decoded = Decoder::default().frame(&pool, &set, &stored);

            let decoded = decoded.expect("room").expect("a data entry");
            assert_eq!(decoded.key_set(), set.key());
            let kept: Vec<_> = series(&frame, &written)
                .into_iter()
                .filter(|(channel, _)| [index, key(Slot::new(4))].contains(channel))
                .collect();
            assert_eq!(series(&decoded, &set), kept);
            assert_eq!(decoded.range(0), Some(Range { seq: 5, count: 1 }));
        }

        /// After a restart, the slot table can give the channels of an entry stored
        /// before it slots in another order.
        #[test]
        fn maps_each_series_to_its_channel_in_a_set_of_another_slot_order() {
            let data = data(&[2, 3]);
            let index = key(Slot::new(1));
            let groups = [Group { index, data: &data }];
            let written = create_interner(&[1], &groups).intern(&groups);
            let mut interner = Interner::new();
            interner.slots().index(index);
            interner.slots().data(key(Slot::new(3)), I64);
            let set = interner.intern(&groups);
            let pool = create_pool(1 << 16);
            let (frame, stored) = stored(&pool, &written, 5);

            let decoded = Decoder::default().frame(&pool, &set, &stored);

            let decoded = decoded.expect("room").expect("a data entry");
            let mut want = series(&frame, &written);
            want.swap(1, 2);
            assert_eq!(series(&decoded, &set), want);
        }

        #[test]
        #[should_panic(expected = "the entry has no frame of key set")]
        fn panics_when_the_set_holds_no_index_of_the_entry() {
            let data = data(&[2]);
            let groups = [Group {
                index: key(Slot::new(1)),
                data: &data,
            }];
            let mut interner = create_interner(&[1, 5], &groups);
            let written = interner.intern(&groups);
            let other = interner.intern(&[Group {
                index: key(Slot::new(5)),
                data: &data,
            }]);
            let pool = create_pool(1 << 16);
            let (_, stored) = stored(&pool, &written, 5);

            drop(Decoder::default().frame(&pool, &other, &stored));
        }

        /// The decoder keeps its map of the last set and channels: each change of
        /// either maps again, also back to a set it mapped before.
        #[test]
        fn maps_again_at_a_change_of_set_or_channels() {
            let (one, two) = (data(&[2]), data(&[2, 3]));
            let index = key(Slot::new(1));
            let group = |data| Group { index, data };
            let mut interner = create_interner(&[1], &[group(&one), group(&two)]);
            let first = interner.intern(&[group(&one)]);
            let second = interner.intern(&[group(&two)]);
            let third = interner.intern(&[group(&data(&[3]))]);
            let pool = create_pool(1 << 16);
            let (_, a) = stored(&pool, &first, 1);
            let (_, b) = stored(&pool, &second, 2);
            let mut decoder = Decoder::default();
            let mut keys = |stored: &Stored, set: &Arc<KeySet>| {
                let made = decoder.frame(&pool, set, stored);
                let made = made.expect("room").expect("a data entry");
                series(&made, set)
                    .into_iter()
                    .map(|(key, _)| key)
                    .collect::<Vec<_>>()
            };
            let at = |slots: &[u32]| -> Vec<_> {
                slots.iter().map(|&n| key(Slot::new(n))).collect()
            };
            assert_eq!(keys(&a, &second), at(&[1, 2]));
            assert_eq!(keys(&b, &second), at(&[1, 2, 3]));
            assert_eq!(keys(&b, &third), at(&[1, 3]));
            assert_eq!(keys(&a, &third), at(&[1]));
            assert_eq!(keys(&a, &first), at(&[1, 2]));
            assert_eq!(keys(&b, &second), at(&[1, 2, 3]));
            assert_eq!(keys(&b, &third), at(&[1, 3]));
        }
        #[test]
        fn keeps_the_maps_of_the_last_key_sets_only() {
            let all = data(&[2, 3, 4, 5, 6]);
            let index = key(Slot::new(1));
            let mut interner = create_interner(&[1], &[Group { index, data: &all }]);
            let full = interner.intern(&[Group { index, data: &all }]);
            let sets: Vec<_> = (2..=6)
                .map(|n| {
                    interner.intern(&[Group {
                        index,
                        data: &data(&[n]),
                    }])
                })
                .collect();
            let pool = create_pool(1 << 16);
            let (_, stored) = stored(&pool, &full, 1);
            let mut decoder = Decoder::default();
            for n in [0, 1, 2, 3, 4, 0, 3] {
                let set = &sets[n];
                let made = decoder.frame(&pool, set, &stored);
                let made = made.expect("room").expect("a data entry");
                let keys: Vec<_> =
                    series(&made, set).into_iter().map(|(k, _)| k).collect();
                let slot = u32::try_from(n + 2).expect("few sets");
                assert_eq!(keys, [key(Slot::new(1)), key(Slot::new(slot))]);
            }
            // The bound on memory shows in no frame, so this reads `maps`.
            let held: Vec<_> = decoder.maps.iter().map(|&(set, _)| set).collect();
            assert_eq!(held, [3, 0, 4, 2].map(|n| sets[n].key()));
        }
    }

    mod round_trip {
        use super::*;

        /// One entry of a generated write.
        #[derive(Debug, Clone)]
        struct Entry {
            present: bool,
            bytes: Vec<u8>,
        }

        /// A write: the data types of each group, the distinct keys of its index and
        /// then its data channels, the present group, then each entry of their key
        /// set.
        fn write() -> impl Strategy<Value = Write> {
            vec(vec(data_type(0..=u32::MAX, 0..=u16::MAX), 0..4), 1..4).prop_flat_map(
                |groups| {
                    let entries: usize = groups.iter().map(|data| data.len() + 1).sum();
                    let keys = btree_set(any::<u128>(), entries)
                        .prop_map(|bits| {
                            bits.into_iter().map(channel::Key::from_u128).collect()
                        })
                        .prop_shuffle();
                    let entry = (any::<bool>(), vec(any::<u8>(), 0..24))
                        .prop_map(|(present, bytes)| Entry { present, bytes });
                    let group = 0..groups.len();
                    (Just(groups), keys, group, vec(entry, entries))
                },
            )
        }

        type Write = (Vec<Vec<Type>>, Vec<channel::Key>, usize, Vec<Entry>);

        /// A key set of `groups`, each the data types on one index, with `keys` in
        /// order.
        fn key_set(groups: &[Vec<Type>], keys: &[channel::Key]) -> Arc<KeySet> {
            let mut keys = keys.iter().copied();
            let mut next = || keys.next().expect("a key for each channel");
            let data: Vec<(channel::Key, Vec<(channel::Key, Type)>)> = groups
                .iter()
                .map(|types| (next(), types.iter().map(|&t| (next(), t)).collect()))
                .collect();
            let groups: Vec<_> = data
                .iter()
                .map(|(index, data)| Group {
                    index: *index,
                    data,
                })
                .collect();
            Interner::new().intern(&groups)
        }

        proptest! {
            #[test]
            fn reads_back_each_present_series((groups, keys, group, entries) in write()) {
                let set = key_set(&groups, &keys);
                let series: Vec<(usize, &[u8])> = (0..entries.len())
                    .filter(|&e| to_usize(set.entries()[e].group) == group)
                    .filter(|&e| entries[e].present && entries[set.index(e)].present)
                    .map(|entry| (entry, &entries[entry].bytes[..]))
                    .collect();
                let pool = create_pool(1 << 16);
                let frame = frame(&pool, &set, &series);

                let parts = body(&pool, &frame, &set).expect("room");

                prop_assert_eq!(parts[0].len(), COUNT + DESCRIPTOR * series.len());
                let body = joined(&parts);
                let read: Vec<_> = read(&body).collect();
                let expected: Vec<_> = series
                    .iter()
                    .map(|&(entry, bytes)| Series {
                        channel: set.entries()[entry].key,
                        data_type: set.entries()[entry].data_type,
                        bytes,
                    })
                    .collect();
                prop_assert_eq!(read, expected);
            }
        }
    }
}
