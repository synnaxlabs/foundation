//! A remote reader session: one hub stream from the reader's node to the home of an
//! index. After the header, the reader's node sends an [`Open`], and each later
//! message is a [`Message`] or part of a frame's body.
//!
//! Fields are little-endian. A stream message has its own length, so no message
//! counts what follows.
//!
//! - [`Open`]: kind 1 (latest) or 2 (complete), then for complete `limit_bytes`
//!   (`u64`), then each channel key (`u128`).
//! - [`Message`]: kind 1 `Opened`, 1 byte. Kind 2 `Credit`, `limit_bytes` (`u64`), 9
//!   bytes. Kind 3 `Frame`: path (`u8`, live 0, backfill 1), form (`u8`, raw 0,
//!   encoded 1), seq (`u64`), count (`u32`), then place and end (each `u32`) for each
//!   series. 15 bytes and 8 per series.

use std::{fmt, mem, slice};

use types::channel;
use types::frame::{Form, Path, Range};

const LATEST: u8 = 1;
const COMPLETE: u8 = 2;

const OPENED: u8 = 1;
const CREDIT: u8 = 2;
const FRAME: u8 = 3;

/// Bytes of a frame head before its ends: kind, path, form, seq, and count.
const HEAD: usize = 15;
/// Bytes of one end: place and end.
const END: usize = 8;
/// Bytes of one channel key.
const KEY: usize = 16;

/// Stop code: the home does not know a channel of the open.
pub const UNKNOWN: u32 = 16;
/// Stop code: the node is not the home of the open's index.
pub const NOT_HOME: u32 = 17;

/// The first hub message of a reader stream, from the reader's node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Open {
    /// Which frames the session gets.
    pub mode: Mode,
    /// The channels the reader reads, all on one index. A [`Head`] names each by its
    /// place in this list.
    pub channels: Vec<channel::Key>,
}

impl Open {
    /// The bytes of the encoded open.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        let mode = match self.mode {
            Mode::Latest => 1,
            Mode::Complete { .. } => 9,
        };
        self.channels.len().saturating_mul(KEY).saturating_add(mode)
    }

    /// Writes the open into `out`.
    ///
    /// # Panics
    ///
    /// When `out` is not [`Open::encoded_len`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        let mut out = Writer::new(out, self.encoded_len());
        match self.mode {
            Mode::Latest => out.put(&[LATEST]),
            Mode::Complete { limit_bytes } => {
                out.put(&[COMPLETE]);
                out.put(&limit_bytes.to_le_bytes());
            }
        }
        for key in &self.channels {
            out.put(&key.as_u128().to_le_bytes());
        }
    }

    /// Decodes the open in `bytes`, the stream's message after the header.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `bytes` is empty, [`Error::Kind`] when the first byte
    /// names no open, and [`Error::Length`] when the length fits no open of that kind.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let length = || Error::Length { len: bytes.len() };
        let (&kind, rest) = bytes.split_first().ok_or(Error::Empty)?;
        let (mode, keys) = match kind {
            LATEST => (Mode::Latest, rest),
            COMPLETE => {
                let (&limit, rest) = rest.split_first_chunk().ok_or_else(length)?;
                let limit_bytes = u64::from_le_bytes(limit);
                (Mode::Complete { limit_bytes }, rest)
            }
            kind => return Err(Error::Kind { kind }),
        };
        let (keys, rest) = keys.as_chunks::<KEY>();
        if !rest.is_empty() {
            return Err(length());
        }
        let channels = keys
            .iter()
            .map(|&key| channel::Key::from_u128(u128::from_le_bytes(key)))
            .collect();
        Ok(Self { mode, channels })
    }
}

/// Which frames a reader session gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The newest live frame, before its commit.
    Latest,
    /// Each live frame after its commit.
    Complete {
        /// The first grant of credit, in bytes.
        limit_bytes: u64,
    },
}

/// A message of a reader stream after the [`Open`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message<'a> {
    /// Home to reader: the session is open at the home.
    Opened,
    /// Reader to home: the session's total grant since the open.
    Credit {
        /// The grant, in bytes.
        limit_bytes: u64,
    },
    /// Home to reader: the head of one frame. Its body follows as messages, back to
    /// back, [`Head::body_len`] bytes in all. No message follows an empty body.
    Frame(Head<'a>),
}

impl<'a> Message<'a> {
    /// The bytes of the encoded message.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Opened => 1,
            Self::Credit { .. } => 9,
            Self::Frame(head) => {
                head.ends().len().saturating_mul(END).saturating_add(HEAD)
            }
        }
    }

    /// Writes the message into `out`.
    ///
    /// # Panics
    ///
    /// When `out` is not [`Message::encoded_len`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        let mut out = Writer::new(out, self.encoded_len());
        match self {
            Self::Opened => out.put(&[OPENED]),
            Self::Credit { limit_bytes } => {
                out.put(&[CREDIT]);
                out.put(&limit_bytes.to_le_bytes());
            }
            Self::Frame(head) => {
                out.put(&[FRAME, path_byte(head.path), form_byte(head.form)]);
                out.put(&head.range.seq.to_le_bytes());
                out.put(&head.range.count.to_le_bytes());
                for (place, end) in head.ends() {
                    let end =
                        u32::try_from(end).expect("invariant: Head::new checks ends");
                    out.put(&place.to_le_bytes());
                    out.put(&end.to_le_bytes());
                }
            }
        }
    }

    /// Decodes the message in `bytes`. A head borrows its ends from `bytes`.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `bytes` is empty, [`Error::Kind`] when the first byte
    /// names no message, [`Error::Length`] when the length fits no message of that
    /// kind, and [`Error::Path`] or [`Error::Form`] when a head names no path or form.
    pub fn decode(bytes: &'a [u8]) -> Result<Self, Error> {
        let length = || Error::Length { len: bytes.len() };
        let (&kind, rest) = bytes.split_first().ok_or(Error::Empty)?;
        match kind {
            OPENED if rest.is_empty() => Ok(Self::Opened),
            CREDIT => {
                let Ok(limit) = <[u8; 8]>::try_from(rest) else {
                    return Err(length());
                };
                Ok(Self::Credit {
                    limit_bytes: u64::from_le_bytes(limit),
                })
            }
            FRAME => {
                let (&[path, form], rest) =
                    rest.split_first_chunk().ok_or_else(length)?;
                let (&seq, rest) = rest.split_first_chunk().ok_or_else(length)?;
                let (&count, ends) = rest.split_first_chunk().ok_or_else(length)?;
                let (ends, rest) = ends.as_chunks::<END>();
                if !rest.is_empty() {
                    return Err(length());
                }
                Ok(Self::Frame(Head {
                    path: path_of(path)?,
                    form: form_of(form)?,
                    range: Range {
                        seq: u64::from_le_bytes(seq),
                        count: u32::from_le_bytes(count),
                    },
                    ends: Ends::Wire(ends),
                }))
            }
            OPENED => Err(length()),
            kind => Err(Error::Kind { kind }),
        }
    }
}

/// The head of one frame of a reader's view: the frame's path, form, and range of the
/// index, and the end of each series in the body.
#[derive(Clone, Copy, Debug)]
pub struct Head<'a> {
    path: Path,
    form: Form,
    range: Range,
    ends: Ends<'a>,
}

/// The ends of a head, as the home gives them or as they arrived.
#[derive(Clone, Copy, Debug)]
enum Ends<'a> {
    Given(&'a [(u32, usize)]),
    Wire(&'a [[u8; 8]]),
}

impl<'a> Head<'a> {
    /// A head with `ends`: each series as `(place in Open::channels, end in the
    /// body)`, in body order.
    ///
    /// # Panics
    ///
    /// When an end is over `u32::MAX`. The ends of a frame never are.
    #[must_use]
    pub fn new(path: Path, form: Form, range: Range, ends: &'a [(u32, usize)]) -> Self {
        if let Some((place, end)) =
            ends.iter().find(|&&(_, end)| u32::try_from(end).is_err())
        {
            panic!("the end {end} of place {place} is over u32::MAX");
        }
        Self {
            path,
            form,
            range,
            ends: Ends::Given(ends),
        }
    }

    /// The frame's path.
    #[must_use]
    pub fn path(&self) -> Path {
        self.path
    }

    /// The frame's form.
    #[must_use]
    pub fn form(&self) -> Form {
        self.form
    }

    /// The seq and count of the index's samples.
    #[must_use]
    pub fn range(&self) -> Range {
        self.range
    }

    /// Each series as `(place, end)`, in body order, as `types::frame::check` and
    /// `split` take them. Nothing checks them: run `check` on the body.
    #[must_use]
    pub fn ends(&self) -> impl ExactSizeIterator<Item = (u32, usize)> + 'a {
        match self.ends {
            Ends::Given(ends) => EndsIter::Given(ends.iter()),
            Ends::Wire(ends) => EndsIter::Wire(ends.iter()),
        }
    }

    /// The bytes of the body: the last end, or 0 with no series. The peer sets it.
    #[must_use]
    pub fn body_len(&self) -> usize {
        let last = match self.ends {
            Ends::Given(ends) => ends.last().copied(),
            Ends::Wire(ends) => ends.last().copied().map(end_of),
        };
        last.map_or(0, |(_, end)| end)
    }
}

impl PartialEq for Head<'_> {
    fn eq(&self, other: &Self) -> bool {
        (self.path, self.form, self.range) == (other.path, other.form, other.range)
            && self.ends().eq(other.ends())
    }
}

impl Eq for Head<'_> {}

enum EndsIter<'a> {
    Given(slice::Iter<'a, (u32, usize)>),
    Wire(slice::Iter<'a, [u8; END]>),
}

impl Iterator for EndsIter<'_> {
    type Item = (u32, usize);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Given(ends) => ends.next().copied(),
            Self::Wire(ends) => ends.next().copied().map(end_of),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Given(ends) => ends.size_hint(),
            Self::Wire(ends) => ends.size_hint(),
        }
    }
}

impl ExactSizeIterator for EndsIter<'_> {}

/// A hub message that is not valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The message has no bytes.
    Empty,
    /// The first byte names no message.
    Kind {
        /// The first byte.
        kind: u8,
    },
    /// No message of its kind has this length.
    Length {
        /// The bytes of the message.
        len: usize,
    },
    /// A head names no path.
    Path {
        /// The path byte.
        byte: u8,
    },
    /// A head names no form.
    Form {
        /// The form byte.
        byte: u8,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the hub message is empty"),
            Self::Kind { kind } => write!(
                f,
                "the hub message has kind {kind}, which this node does not know"
            ),
            Self::Length { len } => write!(
                f,
                "the hub message has {len} bytes, which no message of its kind has"
            ),
            Self::Path { byte } => write!(
                f,
                "the frame head names path {byte}, which this node does not know"
            ),
            Self::Form { byte } => write!(
                f,
                "the frame head names form {byte}, which this node does not know"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Fills `out` from the front, one field at a time.
struct Writer<'o>(&'o mut [u8]);

impl<'o> Writer<'o> {
    fn new(out: &'o mut [u8], len: usize) -> Self {
        assert!(
            out.len() == len,
            "out has {} bytes, and the message has {len}",
            out.len()
        );
        Self(out)
    }

    fn put(&mut self, bytes: &[u8]) {
        let (field, rest) = mem::take(&mut self.0).split_at_mut(bytes.len());
        field.copy_from_slice(bytes);
        self.0 = rest;
    }
}

fn end_of(end: [u8; END]) -> (u32, usize) {
    let [p0, p1, p2, p3, e0, e1, e2, e3] = end;
    let end = u32::from_le_bytes([e0, e1, e2, e3]);
    let end = usize::try_from(end).expect("invariant: a usize holds a u32");
    (u32::from_le_bytes([p0, p1, p2, p3]), end)
}

fn path_byte(path: Path) -> u8 {
    match path {
        Path::Live => 0,
        Path::Backfill => 1,
    }
}

fn path_of(byte: u8) -> Result<Path, Error> {
    match byte {
        0 => Ok(Path::Live),
        1 => Ok(Path::Backfill),
        byte => Err(Error::Path { byte }),
    }
}

fn form_byte(form: Form) -> u8 {
    match form {
        Form::Raw => 0,
        Form::Encoded => 1,
    }
}

fn form_of(byte: u8) -> Result<Form, Error> {
    match byte {
        0 => Ok(Form::Raw),
        1 => Ok(Form::Encoded),
        byte => Err(Error::Form { byte }),
    }
}

#[cfg(test)]
mod tests {
    use std::iter;

    use proptest::prelude::*;

    use super::*;

    fn key(bits: u128) -> channel::Key {
        channel::Key::from_u128(bits)
    }

    fn range(seq: u64, count: u32) -> Range {
        Range { seq, count }
    }

    fn encode_open(open: &Open) -> Vec<u8> {
        let mut out = vec![0xaa; open.encoded_len()];
        open.encode(&mut out);
        out
    }

    fn encode_message(message: &Message<'_>) -> Vec<u8> {
        let mut out = vec![0xaa; message.encoded_len()];
        message.encode(&mut out);
        out
    }

    /// `kind`, then `len - 1` zeros.
    fn zeros(kind: u8, len: usize) -> Vec<u8> {
        iter::once(kind).chain(iter::repeat(0)).take(len).collect()
    }

    mod open {
        use super::*;

        #[test]
        fn pins_the_wire_values() {
            let latest = Open {
                mode: Mode::Latest,
                channels: vec![key(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10), key(1)],
            };
            let complete = Open {
                mode: Mode::Complete {
                    limit_bytes: 0x0102_0304_0506_0708,
                },
                channels: vec![],
            };
            let cases = [
                (
                    latest,
                    [
                        [1].as_slice(),
                        &[16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1],
                        &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                    ]
                    .concat(),
                ),
                (complete, vec![2, 8, 7, 6, 5, 4, 3, 2, 1]),
            ];
            for (open, bytes) in cases {
                assert_eq!(encode_open(&open), bytes, "{open:?}");
                assert_eq!(Open::decode(&bytes), Ok(open));
            }
        }

        #[test]
        fn rejects_an_empty_open() {
            assert_eq!(Open::decode(&[]), Err(Error::Empty));
        }

        #[test]
        fn rejects_unknown_kinds() {
            for kind in (0..=u8::MAX).filter(|kind| !(1..=2).contains(kind)) {
                assert_eq!(Open::decode(&zeros(kind, 17)), Err(Error::Kind { kind }));
            }
        }

        #[test]
        fn rejects_a_length_that_holds_no_whole_key() {
            for (kind, lens) in [(1, [2, 16, 18, 32]), (2, [1, 8, 10, 24])] {
                for len in lens {
                    let error = Err(Error::Length { len });
                    assert_eq!(Open::decode(&zeros(kind, len)), error, "{kind}");
                }
            }
        }

        #[test]
        #[should_panic(expected = "out has 8 bytes, and the message has 9")]
        fn encode_panics_on_a_short_out() {
            let open = Open {
                mode: Mode::Complete { limit_bytes: 1 },
                channels: vec![],
            };
            open.encode(&mut [0; 8]);
        }

        #[test]
        #[should_panic(expected = "out has 18 bytes, and the message has 17")]
        fn encode_panics_on_a_long_out() {
            let open = Open {
                mode: Mode::Latest,
                channels: vec![key(1)],
            };
            open.encode(&mut [0; 18]);
        }
    }

    mod message {
        use super::*;

        #[test]
        fn pins_the_wire_values() {
            let ends = [(3, 8), (0, 21)];
            let head = Head::new(
                Path::Backfill,
                Form::Encoded,
                range(0x0102_0304_0506_0708, 0x0a0b_0c0d),
                &ends,
            );
            let live = Head::new(Path::Live, Form::Raw, range(0, 0), &[]);
            let cases = [
                (Message::Opened, vec![1]),
                (
                    Message::Credit {
                        limit_bytes: 0x0102_0304_0506_0708,
                    },
                    vec![2, 8, 7, 6, 5, 4, 3, 2, 1],
                ),
                (
                    Message::Frame(head),
                    [
                        [3, 1, 1].as_slice(),
                        &[8, 7, 6, 5, 4, 3, 2, 1],
                        &[0x0d, 0x0c, 0x0b, 0x0a],
                        &[3, 0, 0, 0, 8, 0, 0, 0],
                        &[0, 0, 0, 0, 21, 0, 0, 0],
                    ]
                    .concat(),
                ),
                (Message::Frame(live), zeros(3, 15)),
            ];
            for (message, bytes) in cases {
                assert_eq!(encode_message(&message), bytes, "{message:?}");
                assert_eq!(Message::decode(&bytes), Ok(message));
            }
        }

        #[test]
        fn rejects_an_empty_message() {
            assert_eq!(Message::decode(&[]), Err(Error::Empty));
        }

        #[test]
        fn rejects_unknown_kinds() {
            for kind in (0..=u8::MAX).filter(|kind| !(1..=3).contains(kind)) {
                let error = Err(Error::Kind { kind });
                assert_eq!(Message::decode(&zeros(kind, 15)), error);
                assert_eq!(Message::decode(&[kind]), error);
            }
        }

        #[test]
        fn rejects_a_length_of_no_message_of_its_kind() {
            for (kind, lens) in [
                (1, [2, 9, 15, 23]),
                (2, [1, 8, 10, 17]),
                (3, [1, 14, 16, 22]),
            ] {
                for len in lens {
                    let error = Err(Error::Length { len });
                    assert_eq!(Message::decode(&zeros(kind, len)), error, "{kind}");
                }
            }
        }

        #[test]
        fn rejects_a_head_that_names_no_path_or_form() {
            for byte in 2..=u8::MAX {
                let mut bytes = zeros(3, 23);
                bytes[1] = byte;
                assert_eq!(Message::decode(&bytes), Err(Error::Path { byte }));
                let mut bytes = zeros(3, 15);
                bytes[2] = byte;
                assert_eq!(Message::decode(&bytes), Err(Error::Form { byte }));
            }
        }

        #[test]
        fn checks_the_length_before_the_path() {
            let bytes = [3, 9, 9];
            assert_eq!(Message::decode(&bytes), Err(Error::Length { len: 3 }));
        }

        #[test]
        #[should_panic(expected = "out has 0 bytes, and the message has 1")]
        fn encode_panics_on_a_short_out() {
            Message::Opened.encode(&mut []);
        }

        #[test]
        #[should_panic(expected = "out has 24 bytes, and the message has 23")]
        fn encode_panics_on_a_long_out() {
            let head = Head::new(Path::Live, Form::Raw, range(0, 0), &[(0, 8)]);
            Message::Frame(head).encode(&mut [0; 24]);
        }
    }

    mod head {
        use super::*;

        #[test]
        fn gives_its_ends_in_body_order() {
            let ends = [(4, 8), (1, 8), (0, 13)];
            let head = Head::new(Path::Live, Form::Raw, range(7, 2), &ends);
            assert_eq!(head.ends().len(), 3);
            assert_eq!(head.ends().collect::<Vec<_>>(), ends);
            assert_eq!(head.body_len(), 13);
            let bytes = encode_message(&Message::Frame(head));
            let Ok(Message::Frame(decoded)) = Message::decode(&bytes) else {
                panic!("the head did not decode: {bytes:?}");
            };
            assert_eq!(decoded.ends().len(), 3);
            assert_eq!(decoded.ends().collect::<Vec<_>>(), ends);
            assert_eq!(decoded.body_len(), 13);
            assert_eq!(
                (decoded.path(), decoded.form(), decoded.range()),
                (Path::Live, Form::Raw, range(7, 2))
            );
        }

        #[test]
        fn has_an_empty_body_with_no_series() {
            let head = Head::new(Path::Live, Form::Raw, range(0, 5), &[]);
            assert_eq!(head.body_len(), 0);
            assert_eq!(head.ends().len(), 0);
        }

        #[test]
        fn takes_an_end_of_u32_max() {
            let ends = [(0, 4_294_967_295)];
            let head = Head::new(Path::Live, Form::Raw, range(0, 0), &ends);
            assert_eq!(head.body_len(), 4_294_967_295);
        }

        #[test]
        #[should_panic(expected = "the end 4294967296 of place 2 is over u32::MAX")]
        fn panics_on_an_end_over_u32_max() {
            let ends = [(0, 8), (2, 4_294_967_296)];
            let _head: Head<'_> = Head::new(Path::Live, Form::Raw, range(0, 0), &ends);
        }

        #[test]
        fn differs_by_each_part() {
            let ends = [(0, 8)];
            let head = Head::new(Path::Live, Form::Raw, range(1, 2), &ends);
            for other in [
                Head::new(Path::Backfill, Form::Raw, range(1, 2), &ends),
                Head::new(Path::Live, Form::Encoded, range(1, 2), &ends),
                Head::new(Path::Live, Form::Raw, range(0, 2), &ends),
                Head::new(Path::Live, Form::Raw, range(1, 3), &ends),
                Head::new(Path::Live, Form::Raw, range(1, 2), &[(1, 8)]),
                Head::new(Path::Live, Form::Raw, range(1, 2), &[(0, 9)]),
                Head::new(Path::Live, Form::Raw, range(1, 2), &[(0, 8), (1, 8)]),
            ] {
                assert_ne!(head, other);
            }
            assert_eq!(head, Head::new(Path::Live, Form::Raw, range(1, 2), &ends));
        }
    }

    #[test]
    fn pins_the_stop_codes() {
        assert_eq!((UNKNOWN, NOT_HOME), (16, 17));
    }

    #[test]
    fn describes_each_error() {
        for (error, text) in [
            (Error::Empty, "the hub message is empty"),
            (
                Error::Kind { kind: 9 },
                "the hub message has kind 9, which this node does not know",
            ),
            (
                Error::Length { len: 4 },
                "the hub message has 4 bytes, which no message of its kind has",
            ),
            (
                Error::Path { byte: 7 },
                "the frame head names path 7, which this node does not know",
            ),
            (
                Error::Form { byte: 8 },
                "the frame head names form 8, which this node does not know",
            ),
        ] {
            assert_eq!(error.to_string(), text);
        }
    }

    fn open() -> impl Strategy<Value = Open> {
        let mode = prop_oneof![
            Just(Mode::Latest),
            any::<u64>().prop_map(|limit_bytes| Mode::Complete { limit_bytes }),
        ];
        let channels = proptest::collection::vec(any::<u128>().prop_map(key), 0..8);
        (mode, channels).prop_map(|(mode, channels)| Open { mode, channels })
    }

    /// The parts of a head: path, form, range, and ends.
    type Parts = (Path, Form, Range, Vec<(u32, usize)>);

    fn parts() -> impl Strategy<Value = Parts> {
        let path = prop_oneof![Just(Path::Live), Just(Path::Backfill)];
        let form = prop_oneof![Just(Form::Raw), Just(Form::Encoded)];
        let range =
            (any::<u64>(), any::<u32>()).prop_map(|(seq, count)| range(seq, count));
        let end = any::<u32>().prop_map(|end| usize::try_from(end).expect("a usize"));
        let ends = proptest::collection::vec((any::<u32>(), end), 0..8);
        (path, form, range, ends)
    }

    /// A kind byte from 0 to 4, then random bytes, most of them a length that a
    /// message of some kind has. The two bytes after the kind are most often a path
    /// and a form that a frame head can have.
    fn bytes() -> impl Strategy<Value = Vec<u8>> {
        let rest =
            prop_oneof![Just(8), Just(14), Just(16), Just(22), Just(32), 0..48_usize];
        let small = prop_oneof![3 => 0..2_u8, 1 => any::<u8>()];
        (0..5_u8, rest, small.clone(), small).prop_flat_map(
            |(kind, rest, path, form)| {
                proptest::collection::vec(any::<u8>(), rest).prop_map(
                    move |mut rest| {
                        for (byte, small) in rest.iter_mut().zip([path, form]) {
                            *byte = small;
                        }
                        [[kind].as_slice(), &rest].concat()
                    },
                )
            },
        )
    }

    proptest! {
        #[test]
        fn round_trips_an_open(open in open(), stale in any::<u8>()) {
            let mut out = vec![stale; open.encoded_len()];
            open.encode(&mut out);
            prop_assert_eq!(Open::decode(&out), Ok(open));
        }

        #[test]
        fn round_trips_a_message(
            (path, form, range, ends) in parts(),
            limit_bytes in any::<u64>(),
            stale in any::<u8>(),
        ) {
            let head = Head::new(path, form, range, &ends);
            let messages = [
                Message::Opened,
                Message::Credit { limit_bytes },
                Message::Frame(head),
            ];
            for message in messages {
                let mut out = vec![stale; message.encoded_len()];
                message.encode(&mut out);
                prop_assert_eq!(Message::decode(&out), Ok(message));
            }
        }

        #[test]
        fn decodes_only_the_opens_it_encodes(bytes in bytes()) {
            if let Ok(open) = Open::decode(&bytes) {
                prop_assert_eq!(encode_open(&open), bytes);
            }
        }

        #[test]
        fn decodes_only_the_messages_it_encodes(bytes in bytes()) {
            if let Ok(message) = Message::decode(&bytes) {
                prop_assert_eq!(encode_message(&message), bytes);
            }
        }
    }
}
