//! The stored body of a data entry: a header that describes each series of an index
//! frame by its channel and type, then the frame's series bytes.

use block::Block;
use types::channel;
use types::frame::key_set::KeySet;
use types::frame::{self, Form, Frame};
use types::sample::{Scalar, Type};

/// The buffer tag of a data entry.
pub(crate) const TAG: u8 = 0;

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

/// The stored body of `frame`, an encoded index frame of `set`, as the two parts of
/// one buffer entry: a header block from `pool`, then [`Frame::body`]. Copies no
/// series byte.
///
/// # Errors
///
/// [`block::Error`] when `pool` has no block for the header.
///
/// # Panics
///
/// If `frame` is not encoded, not of `set`, or of more than one group.
pub(crate) fn body(
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

/// Each series of `body`, a stored body that [`body`] made, in entry order. Copies
/// nothing.
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
    frame::series(series, ends).map(|(descriptor, bytes)| Series {
        channel: channel::Key::from_u128(u128::from_le_bytes(field(descriptor, 0))),
        data_type: data_type(descriptor),
        bytes,
    })
}

/// The kind code, element code, and `n` of `data_type`.
const fn codes(data_type: Type) -> (u8, u8, u32) {
    match data_type {
        Type::Scalar(element) => (0, code(element), 0),
        Type::Array { element, len } => (1, code(element), len),
        Type::List { element, max } => (2, code(element), max),
        Type::String => (3, 0, 0),
        Type::Bytes => (4, 0, 0),
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

    use proptest::collection::vec;
    use proptest::prelude::*;

    use types::channel::Slot;
    use types::frame::key_set::{Group, Interner};
    use types::frame::{Draft, Path};

    use super::*;
    use crate::common::{interner, key, pool};

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
        let set = interner().intern(&[Group {
            index: key(Slot::new(1)),
            data: &[],
        }]);
        let pool = pool(4096);
        let frame = frame(&pool, &set, &[(0, &[9; 8])]);
        joined(&body(&pool, &frame, &set).expect("room"))
    }

    mod body {
        use super::*;

        #[test]
        fn lays_out_the_header_then_the_series() {
            let data = [(key(Slot::new(2)), Type::Scalar(Scalar::U8))];
            let set = interner().intern(&[Group {
                index: key(Slot::new(1)),
                data: &data,
            }]);
            let pool = pool(4096);
            let stamps = [7; 16];
            let frame = frame(&pool, &set, &[(0, &stamps), (1, &[1, 2, 3])]);
            let parts = body(&pool, &frame, &set).expect("room");
            let header = [
                &[2, 0, 0, 0][..],
                &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
                &[0, 11, 0, 0, 0, 0],
                &[16, 0, 0, 0],
                &[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2],
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
            let set = interner().intern(&[Group {
                index: key(Slot::new(1)),
                data: &data,
            }]);
            let pool = pool(4096);
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
            let set = interner().intern(&[Group {
                index: key(Slot::new(1)),
                data: &[],
            }]);
            let frames = pool(4096);
            let frame = frame(&frames, &set, &[(0, &[0; 8])]);
            let heads = pool(4096);
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
                let set = interner().intern(&[Group {
                    index: key(Slot::new(1)),
                    data: &[],
                }]);
                let pool = pool(4096);
                let frame = draft(&pool, &set, Form::Raw, &[(0, &[0; 8])]);
                drop(body(&pool, &frame, &set));
            }

            #[test]
            #[should_panic(expected = "the frame is not of the key set")]
            fn panics_on_a_frame_of_another_key_set() {
                let mut interner = interner();
                let [of, other] = [1, 2].map(|slot| {
                    interner.intern(&[Group {
                        index: key(Slot::new(slot)),
                        data: &[],
                    }])
                });
                let pool = pool(4096);
                let frame = frame(&pool, &of, &[(0, &[0; 8])]);
                drop(body(&pool, &frame, &other));
            }

            #[test]
            #[should_panic(expected = "the frame has more than one group")]
            fn panics_on_a_frame_of_two_groups() {
                let set = interner().intern(&[1, 2].map(|slot| Group {
                    index: key(Slot::new(slot)),
                    data: &[],
                }));
                let pool = pool(4096);
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
            #[should_panic(expected = "the stored body has an unknown kind 5")]
            fn panics_on_an_unknown_kind() {
                let mut body = stored();
                body[COUNT + at::KIND] = 5;
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

    mod round_trip {
        use super::*;

        fn data_type() -> impl Strategy<Value = Type> {
            let scalar = || proptest::sample::select(SCALARS.to_vec());
            prop_oneof![
                scalar().prop_map(Type::Scalar),
                (scalar(), any::<u32>())
                    .prop_map(|(element, len)| Type::Array { element, len }),
                (scalar(), any::<u32>())
                    .prop_map(|(element, max)| Type::List { element, max }),
                Just(Type::String),
                Just(Type::Bytes),
            ]
        }

        /// One entry of a generated write.
        #[derive(Debug, Clone)]
        struct Entry {
            present: bool,
            bytes: Vec<u8>,
        }

        /// The data types of each group, the present group, then each entry of
        /// their key set.
        fn write() -> impl Strategy<Value = (Vec<Vec<Type>>, usize, Vec<Entry>)> {
            vec(vec(data_type(), 0..4), 1..4).prop_flat_map(|groups| {
                let entries: usize = groups.iter().map(|data| data.len() + 1).sum();
                let entry = (any::<bool>(), vec(any::<u8>(), 0..24))
                    .prop_map(|(present, bytes)| Entry { present, bytes });
                (0..groups.len(), vec(entry, entries), Just(groups))
                    .prop_map(|(group, entries, groups)| (groups, group, entries))
            })
        }

        /// A key set of `groups`, each the data types on one index.
        fn key_set(groups: &[Vec<Type>]) -> Arc<KeySet> {
            let mut keys = (1..).map(channel::Key::from_u128);
            let mut next = || keys.next().expect("keys never end");
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
            fn reads_back_each_present_series((groups, group, entries) in write()) {
                let set = key_set(&groups);
                let series: Vec<(usize, &[u8])> = (0..entries.len())
                    .filter(|&e| to_usize(set.entries()[e].group) == group)
                    .filter(|&e| entries[e].present && entries[set.index(e)].present)
                    .map(|entry| (entry, &entries[entry].bytes[..]))
                    .collect();
                let pool = pool(1 << 16);
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
