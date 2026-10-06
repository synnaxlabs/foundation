//! The entry table of a group commit: what each `append` stored, and where its
//! bytes are in the record body.
//!
//! ```text
//! [count: u32][count headers][bytes of entry 1][bytes of entry 2]...
//! ```
//!
//! A header is `index: u128, path: u8, first: u64, len: u32, stored_at: i64,
//! last: u8 + i64, tag: u8, bytes: u32`, little-endian and fixed width. `last` is a
//! presence byte then the stamp; under presence 0 the stamp is written as 0 and not
//! read. One table block and the callers' blocks make one vectored write, with no
//! copy and no block per entry.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

use std::array;
use std::iter;

use block::Block;
use types::channel::{self, Slot};
use types::frame::Path;
use types::time::Stamp;

/// The encoded size of one [`Header`].
pub(crate) const HEADER_LEN: usize = 16 + 1 + 8 + 4 + 8 + 9 + 1 + 4;

/// The most entries one record holds, and the most parts: with the header block,
/// one record is one vectored write within `IOV_MAX`.
pub(crate) const ENTRIES_MAX: usize = 1023;

/// Bytes of the largest table, `table_len(ENTRIES_MAX)`.
pub(crate) const TABLE_MAX: usize = 4 + ENTRIES_MAX * HEADER_LEN;

/// The most blocks in one entry: a caller's record is at most a header block and
/// a view of one frame's block.
pub const PARTS_MAX: usize = 2;

/// The bytes of one entry, in up to [`PARTS_MAX`] blocks. The ring writes them in
/// place, with no copy, and drops them when the commit that writes them ends.
#[derive(Clone, Debug, Default)]
pub struct Parts([Option<Block>; PARTS_MAX]);

impl Parts {
    /// The blocks, in order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Block> {
        self.0.iter().flatten()
    }

    /// How many blocks.
    pub(crate) fn len(&self) -> usize {
        self.iter().count()
    }

    /// Bytes in all the blocks together.
    pub(crate) fn bytes(&self) -> usize {
        self.iter().map(|block| block.len()).sum()
    }
}

impl From<Block> for Parts {
    fn from(block: Block) -> Self {
        Self([Some(block), None])
    }
}

impl From<Option<Block>> for Parts {
    fn from(block: Option<Block>) -> Self {
        Self([block, None])
    }
}

impl From<[Block; PARTS_MAX]> for Parts {
    fn from([first, second]: [Block; PARTS_MAX]) -> Self {
        Self([Some(first), Some(second)])
    }
}

impl IntoIterator for Parts {
    type Item = Block;
    type IntoIter = iter::Flatten<array::IntoIter<Option<Block>, PARTS_MAX>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter().flatten()
    }
}

/// What one `append` stores: one frame's samples of one index on one path, or a
/// record the caller owns (a position, a handoff, a gap).
#[derive(Clone, Debug)]
pub struct Entry {
    /// The index.
    pub index: channel::Key,
    /// The node's slot of `index`. Never stored.
    pub slot: Slot,
    /// The path.
    pub path: Path,
    /// The first seq.
    pub first: u64,
    /// How many samples. A caller record has `len` 0.
    pub len: u32,
    /// Mesh time at which the home stored it; retention trims by it.
    pub stored_at: Stamp,
    /// The newest stamp of the samples, `None` for a caller record.
    pub last: Option<Stamp>,
    /// A tag the caller gives and reads back. The buffer does not read it.
    pub tag: u8,
    /// The bytes, written in place; see [`Parts`].
    pub parts: Parts,
}

impl Entry {
    /// The header of this entry, with the size of its parts.
    ///
    /// # Panics
    ///
    /// When the parts hold more than `u32::MAX` bytes: a group checks the body
    /// before it takes the entry.
    pub(crate) fn header(&self) -> Header {
        let len = self.parts.bytes();
        let Ok(bytes) = u32::try_from(len) else {
            unreachable!("invariant: the entry of {len} bytes is under the maximum");
        };
        Header {
            index: self.index,
            path: self.path,
            first: self.first,
            len: self.len,
            stored_at: self.stored_at,
            last: self.last,
            tag: self.tag,
            bytes,
        }
    }
}

/// What one `append` stored, without its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) index: channel::Key,
    pub(crate) path: Path,
    /// The first seq.
    pub(crate) first: u64,
    /// How many samples. A caller record has `len` 0.
    pub(crate) len: u32,
    /// Mesh time at which the home stored it.
    pub(crate) stored_at: Stamp,
    /// The newest stamp of the entry's samples; none for a caller record.
    pub(crate) last: Option<Stamp>,
    /// A tag the caller gives and reads back.
    pub(crate) tag: u8,
    /// The size of the entry's bytes in the body.
    pub(crate) bytes: u32,
}

impl Header {
    fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0; HEADER_LEN];
        let mut rest: &mut [u8] = &mut out;
        for field in [
            &self.index.as_u128().to_le_bytes()[..],
            &[path_byte(self.path)],
            &self.first.to_le_bytes(),
            &self.len.to_le_bytes(),
            &self.stored_at.nanos().to_le_bytes(),
            &[u8::from(self.last.is_some())],
            &self.last.map_or(0, Stamp::nanos).to_le_bytes(),
            &[self.tag],
            &self.bytes.to_le_bytes(),
        ] {
            let (slot, after) = rest.split_at_mut(field.len());
            slot.copy_from_slice(field);
            rest = after;
        }
        out
    }

    fn decode(bytes: &[u8; HEADER_LEN]) -> Result<Self, Invalid> {
        let mut fields = Cursor(bytes);
        let index = channel::Key::from_u128(u128::from_le_bytes(fields.take()));
        let path = match fields.take::<1>() {
            [0] => Path::Live,
            [1] => Path::Backfill,
            [byte] => return Err(Invalid::Path(byte)),
        };
        let first = u64::from_le_bytes(fields.take());
        let len = u32::from_le_bytes(fields.take());
        let stored_at = Stamp::from_nanos(i64::from_le_bytes(fields.take()));
        let last = match fields.take::<1>() {
            [0] => {
                fields.take::<8>();
                None
            }
            [1] => Some(Stamp::from_nanos(i64::from_le_bytes(fields.take()))),
            [byte] => return Err(Invalid::Presence(byte)),
        };
        let [tag] = fields.take();
        let bytes = u32::from_le_bytes(fields.take());
        Ok(Self {
            index,
            path,
            first,
            len,
            stored_at,
            last,
            tag,
            bytes,
        })
    }
}

/// Reads fixed-width fields from the front of a header.
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let (field, rest) = self
            .0
            .split_first_chunk()
            .expect("invariant: a header slice holds every field");
        self.0 = rest;
        *field
    }
}

const fn path_byte(path: Path) -> u8 {
    match path {
        Path::Live => 0,
        Path::Backfill => 1,
    }
}

/// The size of the table that holds `count` headers.
///
/// # Panics
///
/// When the table would be over `usize::MAX` bytes.
pub(crate) const fn table_len(count: usize) -> usize {
    match count.checked_mul(HEADER_LEN) {
        Some(headers) => headers.checked_add(4),
        None => None,
    }
    .expect("invariant: a table fits in memory")
}

/// Writes the table of `headers` at the front of `into` and returns its size,
/// `table_len(headers.len())`.
///
/// # Panics
///
/// When `into` is shorter than the table, or when there are over `u32::MAX`
/// headers.
pub(crate) fn write_table(headers: &[Header], into: &mut [u8]) -> usize {
    let len = table_len(headers.len());
    let Some((table, _)) = into.split_at_mut_checked(len) else {
        panic!(
            "invariant: the table of {} headers takes {len} bytes, given {}",
            headers.len(),
            into.len()
        );
    };
    let count = u32::try_from(headers.len()).expect("invariant: a group fits a u32");
    let (slot, mut rest) = table.split_at_mut(4);
    slot.copy_from_slice(&count.to_le_bytes());
    for header in headers {
        let (slot, after) = rest.split_at_mut(HEADER_LEN);
        slot.copy_from_slice(&header.encode());
        rest = after;
    }
    len
}

/// Why a body is not an entry table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Invalid {
    /// The body ends before the table or the bytes it names.
    Truncated,
    /// A count of entries over [`ENTRIES_MAX`].
    Count(u32),
    /// A path byte that is not live or backfill.
    Path(u8),
    /// A `last` presence byte that is not 0 or 1.
    Presence(u8),
    /// This many bytes follow the last entry and no header names them.
    Trailing(usize),
}

/// Reads the table at the start of a body and gives each header, in order.
/// `start` is the first bytes of the body and holds the whole table; `len` is the
/// length of the body. The headers end at the first invalid one, which is the
/// last item.
///
/// # Errors
///
/// [`Invalid::Truncated`] when `start` holds no count or fewer headers than the
/// count, and [`Invalid::Count`] when the count is over [`ENTRIES_MAX`]. Each header
/// checks its own fields and that the body holds its bytes.
///
/// # Panics
///
/// When `start` is longer than `len`.
pub(crate) fn parse(start: &[u8], len: usize) -> Result<Headers<'_>, Invalid> {
    assert!(start.len() <= len, "invariant: the body holds its start");
    let (count, rest) = start.split_first_chunk::<4>().ok_or(Invalid::Truncated)?;
    let count = u32::from_le_bytes(*count);
    let headers_len = usize::try_from(count)
        .ok()
        .filter(|&entries| entries <= ENTRIES_MAX)
        .and_then(|entries| entries.checked_mul(HEADER_LEN))
        .ok_or(Invalid::Count(count))?;
    let (headers, rest) = rest
        .split_at_checked(headers_len)
        .ok_or(Invalid::Truncated)?;
    let bytes = len
        .checked_sub(start.len())
        .and_then(|outside| outside.checked_add(rest.len()))
        .expect("invariant: the body holds its start");
    Ok(Headers { headers, bytes })
}

/// The headers of one group commit, in order.
#[derive(Clone, Debug)]
pub(crate) struct Headers<'a> {
    headers: &'a [u8],
    /// Bytes of the body after the table that no header given so far names.
    bytes: usize,
}

impl Iterator for Headers<'_> {
    type Item = Result<Header, Invalid>;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.header()?;
        if item.is_err() {
            self.headers = &[];
            self.bytes = 0;
        }
        Some(item)
    }
}

impl Headers<'_> {
    fn header(&mut self) -> Option<Result<Header, Invalid>> {
        let Some((header, rest)) = self.headers.split_first_chunk::<HEADER_LEN>()
        else {
            return (self.bytes != 0).then_some(Err(Invalid::Trailing(self.bytes)));
        };
        self.headers = rest;
        let header = match Header::decode(header) {
            Ok(header) => header,
            Err(invalid) => return Some(Err(invalid)),
        };
        let Some(bytes) = usize::try_from(header.bytes)
            .ok()
            .and_then(|len| self.bytes.checked_sub(len))
        else {
            return Some(Err(Invalid::Truncated));
        };
        self.bytes = bytes;
        Some(Ok(header))
    }
}

/// Each header with its bytes, from a body that is whole in memory.
#[cfg(test)]
pub(crate) fn parsed(body: &[u8]) -> Result<Vec<(Header, &[u8])>, Invalid> {
    let headers = parse(body, body.len())?;
    let table = body
        .len()
        .checked_sub(headers.bytes)
        .expect("the table is in the body");
    let mut bytes = body.get(table..).unwrap_or(&[]);
    headers
        .map(|header| {
            let header = header?;
            let len = usize::try_from(header.bytes).expect("the body holds the bytes");
            let (mine, rest) = bytes.split_at(len);
            bytes = rest;
            Ok((header, mine))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn header(bytes: u32) -> Header {
        Header {
            index: channel::Key::from_u128(0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10),
            path: Path::Backfill,
            first: 0x1112_1314_1516_1718,
            len: 0x2122_2324,
            stored_at: Stamp::from_nanos(0x3132_3334_3536_3738),
            last: Some(Stamp::from_nanos(-2)),
            tag: 0x41,
            bytes,
        }
    }

    /// `write_table` and the entries' bytes, as the body of one record.
    fn body(entries: &[(Header, &[u8])]) -> Vec<u8> {
        let headers: Vec<Header> = entries.iter().map(|(header, _)| *header).collect();
        let mut body = vec![0; table_len(headers.len())];
        let len = write_table(&headers, &mut body);
        assert_eq!(len, body.len(), "the table fills the slice");
        for (_, bytes) in entries {
            body.extend_from_slice(bytes);
        }
        body
    }

    #[test]
    fn lays_out_the_fields_little_endian() {
        let mut expected = vec![
            0x10, 0x0F, 0x0E, 0x0D, 0x0C, 0x0B, 0x0A, 0x09, 0x08, 0x07, 0x06, 0x05,
            0x04, 0x03, 0x02, 0x01, 1, 0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11,
            0x24, 0x23, 0x22, 0x21, 0x38, 0x37, 0x36, 0x35, 0x34, 0x33, 0x32, 0x31, 1,
            0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x41, 3, 0, 0, 0,
        ];
        assert_eq!(header(3).encode().to_vec(), expected);
        assert_eq!(expected.len(), HEADER_LEN);
        let mut table = vec![1, 0, 0, 0];
        table.append(&mut expected);
        assert_eq!(
            body(&[(header(3), b"abc")]),
            [table, b"abc".to_vec()].concat()
        );
    }

    #[test]
    fn encodes_no_last_stamp_as_a_zero_presence_byte_and_zero_stamp() {
        let header = Header {
            last: None,
            ..header(0)
        };
        let encoded = header.encode();
        assert_eq!(&encoded[37..46], &[0; 9]);
        assert_eq!(Header::decode(&encoded), Ok(header));
    }

    #[test]
    fn an_empty_table_has_no_entries_and_takes_four_bytes() {
        assert_eq!(table_len(0), 4);
        assert_eq!(parsed(&[0, 0, 0, 0]), Ok(vec![]));
    }

    #[test]
    fn refuses_a_body_that_ends_early() {
        let whole = body(&[(header(3), b"abc"), (header(2), b"de")]);
        for cut in 0..whole.len() {
            let result = parsed(&whole[..cut]);
            assert_eq!(result, Err(Invalid::Truncated), "cut at {cut}");
        }
        assert_eq!(parsed(&whole).map(|entries| entries.len()), Ok(2));
    }

    #[test]
    fn refuses_a_count_over_the_most_entries() {
        let over = u32::try_from(ENTRIES_MAX + 1).expect("fits");
        let mut whole = over.to_le_bytes().to_vec();
        whole.resize(table_len(ENTRIES_MAX + 1), 0);
        assert_eq!(parsed(&whole), Err(Invalid::Count(over)));
        assert_eq!(
            parsed(&u32::MAX.to_le_bytes()),
            Err(Invalid::Count(u32::MAX))
        );
    }

    #[test]
    fn refuses_a_path_or_presence_byte_it_does_not_know() {
        let mut whole = body(&[(header(1), b"a")]);
        whole[4 + 16] = 2;
        assert_eq!(parsed(&whole), Err(Invalid::Path(2)));
        whole[4 + 16] = 0;
        whole[4 + 37] = 7;
        assert_eq!(parsed(&whole), Err(Invalid::Presence(7)));
    }

    #[test]
    fn ends_at_the_first_bad_entry() {
        let mut whole = body(&[(header(3), b"abc"), (header(2), b"de")]);
        whole[4 + 16] = 2;
        let mut entries = parse(&whole, whole.len()).expect("the table is whole");
        assert_eq!(entries.next(), Some(Err(Invalid::Path(2))), "the bad entry");
        assert_eq!(entries.next(), None, "nothing after the bad entry");
        whole[4 + 16] = 1;
        let short = &whole[..whole.len() - 1];
        let mut entries = parse(short, short.len()).expect("the table is whole");
        assert!(
            entries.next().is_some_and(|entry| entry.is_ok()),
            "the first"
        );
        assert_eq!(entries.next(), Some(Err(Invalid::Truncated)), "the cut");
        assert_eq!(entries.next(), None, "nothing after the cut");
    }

    #[test]
    fn refuses_bytes_that_no_header_names() {
        let mut whole = body(&[(header(3), b"abc")]);
        whole.extend_from_slice(b"junk");
        assert_eq!(parsed(&whole), Err(Invalid::Trailing(4)));
        let mut entries = parse(&whole, whole.len()).expect("the table is whole");
        assert!(
            entries.next().is_some_and(|entry| entry.is_ok()),
            "the entry"
        );
        assert_eq!(entries.next(), Some(Err(Invalid::Trailing(4))), "the junk");
        assert_eq!(entries.next(), None, "nothing after the junk");
        assert_eq!(parsed(&[0, 0, 0, 0, 1]), Err(Invalid::Trailing(1)));
    }

    #[test]
    #[should_panic(
        expected = "invariant: the table of 1 headers takes 55 bytes, given 54"
    )]
    fn write_table_panics_on_a_short_slice() {
        write_table(&[header(0)], &mut [0; 54]);
    }

    #[test]
    fn write_table_fills_the_front_of_a_longer_slice() {
        let mut block = [0xFF; 64];
        assert_eq!(write_table(&[header(3)], &mut block), 55, "the table size");
        assert_eq!(block[..4], [1, 0, 0, 0], "the count");
        assert_eq!(block[4..55], header(3).encode(), "the header");
        assert_eq!(block[55..], [0xFF; 9], "the rest is untouched");
    }

    fn any_header() -> impl Strategy<Value = Header> {
        (
            any::<u128>(),
            any::<bool>(),
            any::<u64>(),
            any::<u32>(),
            any::<i64>(),
            any::<Option<i64>>(),
            any::<u8>(),
        )
            .prop_map(|(index, backfill, first, len, stored_at, last, tag)| {
                Header {
                    index: channel::Key::from_u128(index),
                    path: if backfill { Path::Backfill } else { Path::Live },
                    first,
                    len,
                    stored_at: Stamp::from_nanos(stored_at),
                    last: last.map(Stamp::from_nanos),
                    tag,
                    bytes: 0,
                }
            })
    }

    fn any_entry() -> impl Strategy<Value = (Header, Vec<u8>)> {
        (any_header(), prop::collection::vec(any::<u8>(), 0..64)).prop_map(
            |(header, bytes)| {
                let bytes_len = u32::try_from(bytes.len()).expect("under 64");
                (
                    Header {
                        bytes: bytes_len,
                        ..header
                    },
                    bytes,
                )
            },
        )
    }

    proptest! {
        #[test]
        fn gives_back_every_entry_in_order(
            entries in prop::collection::vec(any_entry(), 0..8),
        ) {
            let borrowed: Vec<(Header, &[u8])> =
                entries.iter().map(|(header, bytes)| (*header, &bytes[..])).collect();
            let whole = body(&borrowed);
            prop_assert_eq!(parsed(&whole), Ok(borrowed));
        }
    }
}
