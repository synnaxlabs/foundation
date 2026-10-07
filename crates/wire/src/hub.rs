//! A remote reader session: one hub stream from the reader's node to the home of an
//! index. After the header, the reader's node sends [`FromReader::Open`], and the home
//! sends [`FromHome`] messages.
//!
//! A run follows some messages: the keys of an open, and the ends and then the body
//! of a head. A run goes as stream messages back to back, with no prefix, each at most
//! the peer's `message_bytes_max`. Its message gives its length. No message carries an
//! empty run.
//!
//! Fields are little-endian.
//!
//! - [`FromReader`]: kind 1 (open, latest) or 2 (open, complete), then for complete
//!   `limit_bytes` (`u64`), then the channel count (`u32`). Kind 3 (credit),
//!   `limit_bytes` (`u64`).
//! - [`FromHome`]: kind 1 (opened). Kind 2 (head): path (`u8`, live 0, backfill 1),
//!   seq (`u64`), count (`u32`), and the series count (`u32`).
//! - [`keys`]: each channel key (`u128`).
//! - [`ends`]: place and end (each `u32`) for each series.

use std::{fmt, mem};

use types::frame::{Path, Range};

const LATEST: u8 = 1;
const COMPLETE: u8 = 2;
const CREDIT: u8 = 3;

const OPENED: u8 = 1;
const HEAD: u8 = 2;

/// Stop code: the home does not know a channel of the open.
pub const UNKNOWN: u32 = 16;
/// Stop code: the node is not the home of the open's index.
pub const NOT_HOME: u32 = 17;

/// A message from the reader's node to the home.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FromReader {
    /// The first message, which opens the session. Its [`keys`] follow.
    Open {
        /// Which frames the session gets.
        mode: Mode,
        /// The count of channels the reader reads, all on one index.
        channels: u32,
    },
    /// Each later message: the session's total grant since the open.
    Credit {
        /// The grant, in bytes.
        limit_bytes: u64,
    },
}

impl FromReader {
    /// The bytes of the encoded message.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Open {
                mode: Mode::Latest, ..
            } => 5,
            Self::Open {
                mode: Mode::Complete { .. },
                ..
            } => 13,
            Self::Credit { .. } => 9,
        }
    }

    /// Writes the message into `out`.
    ///
    /// # Panics
    ///
    /// When `out` is not [`FromReader::encoded_len`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        let mut out = Writer::new(out, self.encoded_len());
        match *self {
            Self::Open { mode, channels } => {
                match mode {
                    Mode::Latest => out.put(&[LATEST]),
                    Mode::Complete { limit_bytes } => {
                        out.put(&[COMPLETE]);
                        out.put(&limit_bytes.to_le_bytes());
                    }
                }
                out.put(&channels.to_le_bytes());
            }
            Self::Credit { limit_bytes } => {
                out.put(&[CREDIT]);
                out.put(&limit_bytes.to_le_bytes());
            }
        }
    }

    /// Decodes the message in `bytes`.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `bytes` is empty, [`Error::Kind`] when the first byte
    /// names no message, and [`Error::Length`] when the length fits no message of
    /// that kind.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let (&kind, rest) = bytes.split_first().ok_or(Error::Empty)?;
        let mut fields = Fields::new(rest, bytes.len());
        let message = match kind {
            LATEST => Self::Open {
                mode: Mode::Latest,
                channels: u32::from_le_bytes(fields.take()?),
            },
            COMPLETE => Self::Open {
                mode: Mode::Complete {
                    limit_bytes: u64::from_le_bytes(fields.take()?),
                },
                channels: u32::from_le_bytes(fields.take()?),
            },
            CREDIT => Self::Credit {
                limit_bytes: u64::from_le_bytes(fields.take()?),
            },
            kind => return Err(Error::Kind { kind }),
        };
        fields.end()?;
        Ok(message)
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

/// A message from the home to the reader's node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FromHome {
    /// The session is open at the home.
    Opened,
    /// The head of one frame. Its [`ends`] follow, then its body.
    Head(Head),
}

/// The head of one frame that the home sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Head {
    /// How the frame reached the home.
    pub path: Path,
    /// The samples of the frame.
    pub range: Range,
    /// The count of the frame's series, which is the count of its ends.
    pub series: u32,
}

impl FromHome {
    /// The bytes of the encoded message.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        match self {
            Self::Opened => 1,
            Self::Head(_) => 18,
        }
    }

    /// Writes the message into `out`.
    ///
    /// # Panics
    ///
    /// When `out` is not [`FromHome::encoded_len`] bytes.
    pub fn encode(&self, out: &mut [u8]) {
        let mut out = Writer::new(out, self.encoded_len());
        match self {
            Self::Opened => out.put(&[OPENED]),
            Self::Head(head) => {
                out.put(&[HEAD, path_byte(head.path)]);
                out.put(&head.range.seq.to_le_bytes());
                out.put(&head.range.count.to_le_bytes());
                out.put(&head.series.to_le_bytes());
            }
        }
    }

    /// Decodes the message in `bytes`.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `bytes` is empty, [`Error::Kind`] when the first byte
    /// names no message, [`Error::Length`] when the length fits no message of that
    /// kind, and [`Error::Path`] when a head names no path.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let (&kind, rest) = bytes.split_first().ok_or(Error::Empty)?;
        let mut fields = Fields::new(rest, bytes.len());
        match kind {
            OPENED => {
                fields.end()?;
                Ok(Self::Opened)
            }
            HEAD => {
                let [path] = fields.take()?;
                let seq = u64::from_le_bytes(fields.take()?);
                let count = u32::from_le_bytes(fields.take()?);
                let series = u32::from_le_bytes(fields.take()?);
                fields.end()?;
                Ok(Self::Head(Head {
                    path: path_of(path)?,
                    range: Range { seq, count },
                    series,
                }))
            }
            kind => Err(Error::Kind { kind }),
        }
    }
}

/// The run of keys after an open, in the open's order. A series of the session has
/// the place of its channel's first key in the run. The index, when the run does not
/// hold it, has the next place.
pub mod keys {
    use types::channel;

    use super::{Writer, widen};

    const KEY: usize = 16;

    /// The bytes of the run of an open of `channels`. The caller bounds `channels`
    /// before it allocates by it.
    #[must_use]
    pub fn len(channels: u32) -> usize {
        widen(channels).saturating_mul(KEY)
    }

    /// Writes the run of `keys` into `out`.
    ///
    /// # Panics
    ///
    /// When `out` is not 16 bytes for each key.
    pub fn encode(keys: &[channel::Key], out: &mut [u8]) {
        let out = Writer::new(out, keys.len().saturating_mul(KEY));
        for (out, key) in out.0.as_chunks_mut::<KEY>().0.iter_mut().zip(keys) {
            *out = key.as_u128().to_le_bytes();
        }
    }

    /// The keys in `run`.
    ///
    /// # Panics
    ///
    /// When `run` is not a whole count of keys.
    #[must_use]
    pub fn decode(run: &[u8]) -> impl ExactSizeIterator<Item = channel::Key> + '_ {
        let (keys, rest) = run.as_chunks::<KEY>();
        assert!(
            rest.is_empty(),
            "the run has {} bytes, and a key has {KEY}",
            run.len()
        );
        keys.iter()
            .map(|&key| channel::Key::from_u128(u128::from_le_bytes(key)))
    }
}

/// The run of ends after a head: the place of each series and the end of its bytes in
/// the body. The body follows, as long as the last end.
pub mod ends {
    use super::{Writer, widen};

    const END: usize = 8;

    /// The bytes of the ends of a head of `series`. The caller bounds `series` before
    /// it allocates by it.
    #[must_use]
    pub fn len(series: u32) -> usize {
        widen(series).saturating_mul(END)
    }

    /// Writes `ends`, each a place and an end, into `out`.
    ///
    /// # Panics
    ///
    /// When `out` is not 8 bytes for each end.
    pub fn encode(ends: impl ExactSizeIterator<Item = (u32, u32)>, out: &mut [u8]) {
        let out = Writer::new(out, ends.len().saturating_mul(END));
        for (out, (place, end)) in out.0.as_chunks_mut::<END>().0.iter_mut().zip(ends) {
            let [p0, p1, p2, p3] = place.to_le_bytes();
            let [e0, e1, e2, e3] = end.to_le_bytes();
            *out = [p0, p1, p2, p3, e0, e1, e2, e3];
        }
    }

    /// The ends in `run`, each a place and an end.
    ///
    /// # Panics
    ///
    /// When `run` is not a whole count of ends.
    #[must_use]
    pub fn decode(
        run: &[u8],
    ) -> impl ExactSizeIterator<Item = (u32, u32)> + DoubleEndedIterator + '_ {
        let (ends, rest) = run.as_chunks::<END>();
        assert!(
            rest.is_empty(),
            "the run has {} bytes, and an end has {END}",
            run.len()
        );
        ends.iter().map(|&[p0, p1, p2, p3, e0, e1, e2, e3]| {
            (
                u32::from_le_bytes([p0, p1, p2, p3]),
                u32::from_le_bytes([e0, e1, e2, e3]),
            )
        })
    }
}

/// A hub message that is not valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

/// Reads a message's fields from the front. Each error names the message's length.
struct Fields<'b> {
    rest: &'b [u8],
    len: usize,
}

impl<'b> Fields<'b> {
    fn new(rest: &'b [u8], len: usize) -> Self {
        Self { rest, len }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let (&field, rest) = self
            .rest
            .split_first_chunk()
            .ok_or(Error::Length { len: self.len })?;
        self.rest = rest;
        Ok(field)
    }

    fn end(&self) -> Result<(), Error> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(Error::Length { len: self.len })
        }
    }
}

fn widen(count: u32) -> usize {
    usize::try_from(count).expect("invariant: a usize holds a u32")
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

#[cfg(test)]
mod tests {
    use std::iter;

    use proptest::prelude::*;
    use types::channel;

    use super::*;

    fn key(bits: u128) -> channel::Key {
        channel::Key::from_u128(bits)
    }

    fn head(path: Path, seq: u64, count: u32, series: u32) -> FromHome {
        FromHome::Head(Head {
            path,
            range: Range { seq, count },
            series,
        })
    }

    fn live() -> FromHome {
        head(Path::Live, 0, 0, 0)
    }

    fn encode_reader(message: FromReader) -> Vec<u8> {
        let mut out = vec![0xaa; message.encoded_len()];
        message.encode(&mut out);
        out
    }

    fn encode_home(message: FromHome) -> Vec<u8> {
        let mut out = vec![0xaa; message.encoded_len()];
        message.encode(&mut out);
        out
    }

    fn encode_keys(keys: &[channel::Key]) -> Vec<u8> {
        let count = u32::try_from(keys.len()).expect("a test has few keys");
        let mut out = vec![0xaa; super::keys::len(count)];
        super::keys::encode(keys, &mut out);
        out
    }

    fn encode_ends(ends: &[(u32, u32)]) -> Vec<u8> {
        let count = u32::try_from(ends.len()).expect("a test has few ends");
        let mut out = vec![0xaa; super::ends::len(count)];
        super::ends::encode(ends.iter().copied(), &mut out);
        out
    }

    /// `kind`, then `len - 1` zeros.
    fn zeros(kind: u8, len: usize) -> Vec<u8> {
        iter::once(kind).chain(iter::repeat(0)).take(len).collect()
    }

    /// `decode` gives [`Error::Length`] for a message of `kind` with each length in
    /// `lens`.
    fn check<T: fmt::Debug + PartialEq>(
        decode: fn(&[u8]) -> Result<T, Error>,
        kind: u8,
        lens: &[usize],
    ) {
        for &len in lens {
            let bytes = zeros(kind, len);
            assert_eq!(decode(&bytes), Err(Error::Length { len }), "kind {kind}");
        }
    }

    mod from_reader {
        use super::*;

        #[test]
        fn pins_the_wire_values() {
            let latest = FromReader::Open {
                mode: Mode::Latest,
                channels: 0x0102_0304,
            };
            assert_eq!(encode_reader(latest), [1, 4, 3, 2, 1]);
            let complete = FromReader::Open {
                mode: Mode::Complete {
                    limit_bytes: 0x0102_0304_0506_0708,
                },
                channels: 2,
            };
            assert_eq!(
                encode_reader(complete),
                [2, 8, 7, 6, 5, 4, 3, 2, 1, 2, 0, 0, 0]
            );
            let credit = FromReader::Credit {
                limit_bytes: 0x0102_0304_0506_0708,
            };
            assert_eq!(encode_reader(credit), [3, 8, 7, 6, 5, 4, 3, 2, 1]);
        }

        #[test]
        fn refuses_an_empty_message() {
            assert_eq!(FromReader::decode(&[]), Err(Error::Empty));
        }

        #[test]
        fn refuses_unknown_kinds_before_the_length() {
            for kind in (0..=u8::MAX).filter(|kind| !(1..=3).contains(kind)) {
                for len in [1, 13] {
                    let bytes = zeros(kind, len);
                    assert_eq!(FromReader::decode(&bytes), Err(Error::Kind { kind }));
                }
            }
        }

        #[test]
        fn refuses_each_wrong_length() {
            check(FromReader::decode, 1, &[1, 4, 6, 13]);
            check(FromReader::decode, 2, &[1, 9, 12, 14]);
            check(FromReader::decode, 3, &[1, 5, 8, 10]);
        }

        #[test]
        #[should_panic(expected = "out has 4 bytes, and the message has 5")]
        fn panics_when_out_has_the_wrong_length() {
            let open = FromReader::Open {
                mode: Mode::Latest,
                channels: 1,
            };
            open.encode(&mut [0; 4]);
        }
    }

    mod from_home {
        use super::*;

        #[test]
        fn pins_the_wire_values() {
            assert_eq!(encode_home(FromHome::Opened), [1]);
            let backfill = head(Path::Backfill, 0x0102_0304_0506_0708, 0x0a0b_0c0d, 3);
            assert_eq!(
                encode_home(backfill),
                [
                    2, 1, 8, 7, 6, 5, 4, 3, 2, 1, 0x0d, 0x0c, 0x0b, 0x0a, 3, 0, 0, 0
                ]
            );
            assert_eq!(encode_home(live()), zeros(2, 18));
        }

        #[test]
        fn refuses_an_empty_message() {
            assert_eq!(FromHome::decode(&[]), Err(Error::Empty));
        }

        #[test]
        fn refuses_unknown_kinds_before_the_length() {
            for kind in (0..=u8::MAX).filter(|kind| !(1..=2).contains(kind)) {
                for len in [1, 18] {
                    let bytes = zeros(kind, len);
                    assert_eq!(FromHome::decode(&bytes), Err(Error::Kind { kind }));
                }
            }
        }

        #[test]
        fn refuses_each_wrong_length() {
            check(FromHome::decode, 1, &[2, 18]);
            check(FromHome::decode, 2, &[1, 2, 17, 19]);
        }

        #[test]
        fn refuses_unknown_paths() {
            for byte in 2..=u8::MAX {
                let mut bytes = encode_home(live());
                bytes[1] = byte;
                assert_eq!(FromHome::decode(&bytes), Err(Error::Path { byte }));
            }
        }

        #[test]
        fn checks_the_length_before_the_path() {
            assert_eq!(FromHome::decode(&[2, 9, 9]), Err(Error::Length { len: 3 }));
        }

        #[test]
        #[should_panic(expected = "out has 19 bytes, and the message has 18")]
        fn panics_when_out_has_the_wrong_length() {
            live().encode(&mut [0; 19]);
        }
    }

    mod keys {
        use super::*;

        #[test]
        fn pins_the_wire_values() {
            let keys = [key(0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10), key(1)];
            let mut bytes = vec![16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1];
            bytes.extend([1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            assert_eq!(encode_keys(&keys), bytes);
            assert_eq!(super::super::keys::len(2), 32);
        }

        #[test]
        fn has_no_bytes_for_no_channels() {
            assert_eq!(super::super::keys::len(0), 0);
            assert_eq!(super::super::keys::decode(&[]).len(), 0);
        }

        #[test]
        #[should_panic(expected = "the run has 17 bytes, and a key has 16")]
        fn panics_on_a_partial_key() {
            drop(super::super::keys::decode(&[0; 17]));
        }

        #[test]
        #[should_panic(expected = "out has 15 bytes, and the message has 16")]
        fn panics_when_out_has_the_wrong_length() {
            super::super::keys::encode(&[key(1)], &mut [0; 15]);
        }
    }

    mod ends {
        use super::*;

        #[test]
        fn pins_the_wire_values() {
            assert_eq!(
                encode_ends(&[(3, 8), (0, 21)]),
                [3, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 21, 0, 0, 0]
            );
            assert_eq!(super::super::ends::len(2), 16);
        }

        #[test]
        fn gives_the_body_length_as_the_last_end() {
            let run = encode_ends(&[(1, 8), (0, u32::MAX)]);
            let last = super::super::ends::decode(&run).next_back();
            assert_eq!(last, Some((0, u32::MAX)));
        }

        #[test]
        #[should_panic(expected = "the run has 9 bytes, and an end has 8")]
        fn panics_on_a_partial_end() {
            drop(super::super::ends::decode(&[0; 9]));
        }

        #[test]
        #[should_panic(expected = "out has 8 bytes, and the message has 16")]
        fn panics_when_out_has_the_wrong_length() {
            super::super::ends::encode([(0, 1), (1, 2)].into_iter(), &mut [0; 8]);
        }
    }

    #[test]
    fn pins_the_stop_codes() {
        assert_eq!((UNKNOWN, NOT_HOME), (16, 17));
    }

    #[test]
    fn names_each_error() {
        let cases = [
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
                Error::Path { byte: 2 },
                "the frame head names path 2, which this node does not know",
            ),
        ];
        for (error, text) in cases {
            assert_eq!(error.to_string(), text);
        }
    }

    fn from_reader() -> impl Strategy<Value = FromReader> {
        let mode = prop_oneof![
            Just(Mode::Latest),
            any::<u64>().prop_map(|limit_bytes| Mode::Complete { limit_bytes }),
        ];
        prop_oneof![
            (mode, any::<u32>())
                .prop_map(|(mode, channels)| FromReader::Open { mode, channels }),
            any::<u64>().prop_map(|limit_bytes| FromReader::Credit { limit_bytes }),
        ]
    }

    fn from_home() -> impl Strategy<Value = FromHome> {
        let path = prop_oneof![Just(Path::Live), Just(Path::Backfill)];
        prop_oneof![
            Just(FromHome::Opened),
            (path, any::<u64>(), any::<u32>(), any::<u32>())
                .prop_map(|(path, seq, count, series)| head(path, seq, count, series)),
        ]
    }

    /// A kind byte from 0 to 4, then random bytes, most of them a length that a
    /// message of some kind has. The byte after the kind is most often a path.
    fn bytes() -> impl Strategy<Value = Vec<u8>> {
        let rest = prop_oneof![Just(4), Just(8), Just(12), Just(17), 0..24_usize];
        let path = prop_oneof![3 => 0..2_u8, 1 => any::<u8>()];
        (0..5_u8, rest, path).prop_flat_map(|(kind, rest, path)| {
            proptest::collection::vec(any::<u8>(), rest).prop_map(move |mut rest| {
                if let Some(first) = rest.first_mut() {
                    *first = path;
                }
                [[kind].as_slice(), &rest].concat()
            })
        })
    }

    proptest! {
        #[test]
        fn round_trips_a_message_from_the_reader(message in from_reader()) {
            prop_assert_eq!(FromReader::decode(&encode_reader(message)), Ok(message));
        }

        #[test]
        fn round_trips_a_message_from_the_home(message in from_home()) {
            prop_assert_eq!(FromHome::decode(&encode_home(message)), Ok(message));
        }

        #[test]
        fn round_trips_keys(bits in proptest::collection::vec(any::<u128>(), 0..8)) {
            let keys: Vec<_> = bits.into_iter().map(key).collect();
            let decoded: Vec<_> = super::keys::decode(&encode_keys(&keys)).collect();
            prop_assert_eq!(decoded, keys);
        }

        #[test]
        fn round_trips_ends(
            ends in proptest::collection::vec(any::<(u32, u32)>(), 0..8),
        ) {
            let decoded: Vec<_> = super::ends::decode(&encode_ends(&ends)).collect();
            prop_assert_eq!(decoded, ends);
        }

        #[test]
        fn decodes_only_the_messages_it_encodes(bytes in bytes()) {
            if let Ok(message) = FromReader::decode(&bytes) {
                prop_assert_eq!(&encode_reader(message), &bytes);
            }
            if let Ok(message) = FromHome::decode(&bytes) {
                prop_assert_eq!(&encode_home(message), &bytes);
            }
        }
    }
}
