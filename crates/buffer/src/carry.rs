//! The body of a carry record: the durable tails of paths whose holder a later trim
//! passes, and the entries that it keeps.
//!
//! ```text
//! [tails: u16][entries: u16][tails][entry headers][entry bytes]
//! ```
//!
//! A tail is `index: u128, path: u8, seq: u64, given: u64, last: u8 + i64`,
//! little-endian and fixed width, with `last` as in an entry header. An entry header
//! is as in the entry table, and the bytes follow in the order of the headers.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

use types::channel;
use types::frame::Path;
use types::time::Stamp;

use crate::entry::{self, Fields, Header, Invalid};

/// The encoded size of one [`Tail`].
const LEN: usize = 16 + 1 + 8 + 8 + 9;

/// The size of the two counts.
const COUNTS: usize = 4;

/// The most tails one record holds. The body of a full record fits in the first
/// window that a walk reads, so the walk holds it whole. A body also stays within
/// `body_max`, so a layout with a small one holds fewer.
pub(crate) const TAILS_MAX: usize = entry::ENTRIES_MAX;

/// The most entries one record holds.
pub(crate) const ENTRIES_MAX: usize = entry::ENTRIES_MAX;

/// Where one path of an index stood on disk when a commit carried it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tail {
    pub(crate) index: channel::Key,
    pub(crate) path: Path,
    /// The seq after the path's newest durable entry.
    pub(crate) seq: u64,
    /// How many durable entries with no samples are at `seq`.
    pub(crate) given: u64,
    /// The `last` of the path's newest durable entry that has one.
    pub(crate) stamp: Option<Stamp>,
}

impl Tail {
    fn encode(&self) -> [u8; LEN] {
        let mut out = [0; LEN];
        let mut rest: &mut [u8] = &mut out;
        for field in [
            &self.index.as_u128().to_le_bytes()[..],
            &[entry::path_byte(self.path)],
            &self.seq.to_le_bytes(),
            &self.given.to_le_bytes(),
            &entry::last_bytes(self.stamp),
        ] {
            let (slot, after) = rest.split_at_mut(field.len());
            slot.copy_from_slice(field);
            rest = after;
        }
        out
    }

    fn decode(bytes: &[u8; LEN]) -> Result<Self, Invalid> {
        let mut fields = Fields(bytes);
        let index = channel::Key::from_u128(u128::from_le_bytes(fields.take()));
        let path = fields.path()?;
        let seq = u64::from_le_bytes(fields.take());
        let given = u64::from_le_bytes(fields.take());
        let stamp = fields.last()?;
        Ok(Self {
            index,
            path,
            seq,
            given,
            stamp,
        })
    }
}

/// The most tails whose body, with no entry, fits in `len` bytes, at most
/// [`TAILS_MAX`].
pub(crate) fn count(len: usize) -> usize {
    (len.saturating_sub(COUNTS) / LEN).min(TAILS_MAX)
}

/// The size of the body that holds `tails` tails and `entries` entries of `bytes`
/// bytes together.
///
/// # Panics
///
/// When `tails` is over [`TAILS_MAX`] or `entries` is over [`ENTRIES_MAX`].
pub(crate) const fn body_len(tails: usize, entries: usize, bytes: usize) -> usize {
    assert!(
        tails <= TAILS_MAX && entries <= ENTRIES_MAX,
        "invariant: a carry record holds the tails and the entries"
    );
    let tails = tails.checked_mul(LEN);
    let headers = entries.checked_mul(entry::HEADER_LEN);
    match (tails, headers) {
        (Some(tails), Some(headers)) => match COUNTS.checked_add(tails) {
            Some(len) => match len.checked_add(headers) {
                Some(len) => len.checked_add(bytes),
                None => None,
            },
            None => None,
        },
        _ => None,
    }
    .expect("invariant: a carry body fits in memory")
}

/// Writes the body that holds `tails` and `entries`, each header with its bytes, at
/// the front of `into` and returns its size, as [`body_len`] gives it.
///
/// # Panics
///
/// When there are over [`TAILS_MAX`] tails or [`ENTRIES_MAX`] entries, an entry's
/// header names another size than its bytes, or `into` is shorter than the body.
pub(crate) fn write(
    tails: &[Tail],
    entries: &[(Header, &[u8])],
    into: &mut [u8],
) -> usize {
    let bytes = entries.iter().map(|(_, bytes)| bytes.len()).sum::<usize>();
    let len = body_len(tails.len(), entries.len(), bytes);
    let Some((body, _)) = into.split_at_mut_checked(len) else {
        panic!(
            "invariant: the body of {} tails and {} entries takes {len} bytes, given {}",
            tails.len(),
            entries.len(),
            into.len()
        );
    };
    let counted = |count: usize| {
        u16::try_from(count).expect("invariant: a count of at most 1023 fits a u16")
    };
    let (slot, mut rest) = body.split_at_mut(COUNTS);
    let (first, second) = slot.split_at_mut(2);
    first.copy_from_slice(&counted(tails.len()).to_le_bytes());
    second.copy_from_slice(&counted(entries.len()).to_le_bytes());
    for tail in tails {
        let (slot, after) = rest.split_at_mut(LEN);
        slot.copy_from_slice(&tail.encode());
        rest = after;
    }
    for (header, bytes) in entries {
        assert!(
            usize::try_from(header.bytes).ok() == Some(bytes.len()),
            "invariant: the header of {} bytes has {} bytes",
            header.bytes,
            bytes.len()
        );
        let (slot, after) = rest.split_at_mut(entry::HEADER_LEN);
        slot.copy_from_slice(&header.encode());
        rest = after;
    }
    for (_, bytes) in entries {
        let (slot, after) = rest.split_at_mut(bytes.len());
        slot.copy_from_slice(bytes);
        rest = after;
    }
    len
}

/// The tails and the entries of a whole carry body, each in order.
#[derive(Clone, Debug)]
pub(crate) struct Parsed<'a> {
    /// The tails. They end at the first invalid one, which is the last item.
    pub(crate) tails: Tails<'a>,
    /// Each entry header with its bytes. They end at the first invalid one, which
    /// is the last item. After the last valid entry, an item gives
    /// [`Invalid::Trailing`] when bytes follow that no header names.
    pub(crate) entries: Entries<'a>,
}

/// The tails of a [`Parsed`] body.
#[derive(Clone, Debug)]
pub(crate) struct Tails<'a> {
    tails: std::slice::Iter<'a, [u8; LEN]>,
    failed: bool,
}

impl Iterator for Tails<'_> {
    type Item = Result<Tail, Invalid>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let tail = Tail::decode(self.tails.next()?);
        self.failed = tail.is_err();
        Some(tail)
    }
}

/// The entries of a [`Parsed`] body.
#[derive(Clone, Debug)]
pub(crate) struct Entries<'a> {
    headers: entry::Headers<'a>,
    body: &'a [u8],
}

impl<'a> Iterator for Entries<'a> {
    type Item = Result<(Header, &'a [u8]), Invalid>;

    fn next(&mut self) -> Option<Self::Item> {
        let body = self.body;
        Some(self.headers.next()?.map(|(header, offset)| {
            let len =
                usize::try_from(header.bytes).expect("invariant: a u32 fits usize");
            let bytes = offset
                .checked_add(len)
                .and_then(|end| body.get(offset..end));
            (
                header,
                bytes.expect("invariant: the headers check the body holds the bytes"),
            )
        }))
    }
}

/// Gives the tails and the entries of a whole carry body.
///
/// # Errors
///
/// [`Invalid::Truncated`] when the body holds no counts, or fewer tails or entry
/// headers than its counts, and [`Invalid::Count`] when a count is over its
/// maximum. Each tail checks its path byte and its presence byte, and each entry
/// checks its header and that the body holds its bytes, as an entry table does.
pub(crate) fn parse(body: &[u8]) -> Result<Parsed<'_>, Invalid> {
    let (counts, rest) = body
        .split_first_chunk::<COUNTS>()
        .ok_or(Invalid::Truncated)?;
    let [a, b, c, d] = *counts;
    let tails = counted(u16::from_le_bytes([a, b]), TAILS_MAX)?;
    let entries = counted(u16::from_le_bytes([c, d]), ENTRIES_MAX)?;
    let tails_len = tails
        .checked_mul(LEN)
        .expect("invariant: the tails fit in memory");
    let table_len = entries
        .checked_mul(entry::HEADER_LEN)
        .expect("invariant: the headers fit in memory");
    let Some((tails, rest)) = rest.split_at_checked(tails_len) else {
        return Err(Invalid::Truncated);
    };
    let Some((table, _)) = rest.split_at_checked(table_len) else {
        return Err(Invalid::Truncated);
    };
    let offset = body_len(tails.len() / LEN, entries, 0);
    Ok(Parsed {
        tails: Tails {
            tails: tails.as_chunks::<LEN>().0.iter(),
            failed: false,
        },
        entries: Entries {
            headers: entry::headers(table, offset, body.len()),
            body,
        },
    })
}

/// `count` as a `usize`, at most `max`.
fn counted(count: u16, max: usize) -> Result<usize, Invalid> {
    Some(usize::from(count))
        .filter(|&count| count <= max)
        .ok_or(Invalid::Count(u32::from(count)))
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;
    use crate::entry::Kind;
    use crate::wal::Layout;
    use proptest::prelude::*;

    fn tail(given: u64, stamp: Option<i64>) -> Tail {
        Tail {
            index: channel::Key::from_u128(0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10),
            path: Path::Backfill,
            seq: 0x1112_1314_1516_1718,
            given,
            stamp: stamp.map(Stamp::from_nanos),
        }
    }

    fn header(tag: u8, bytes: u32) -> Header {
        Header {
            index: channel::Key::from_u128(7),
            path: Path::Live,
            first: 3,
            stored_at: Stamp::from_nanos(11),
            kind: Kind::Record(NonZeroU64::MIN),
            tag,
            bytes,
        }
    }

    fn body(tails: &[Tail]) -> Vec<u8> {
        with_entries(tails, &[])
    }

    fn with_entries(tails: &[Tail], entries: &[(Header, &[u8])]) -> Vec<u8> {
        let bytes = entries.iter().map(|(_, bytes)| bytes.len()).sum();
        let mut body = vec![0; body_len(tails.len(), entries.len(), bytes)];
        assert_eq!(
            write(tails, entries, &mut body),
            body.len(),
            "the tails and the entries fill the body"
        );
        body
    }

    type Whole = (Vec<Tail>, Vec<(Header, Vec<u8>)>);

    fn whole(body: &[u8]) -> Result<Whole, Invalid> {
        let Parsed { tails, entries } = parse(body)?;
        let tails = tails.collect::<Result<_, _>>()?;
        let entries = entries
            .map(|entry| entry.map(|(header, bytes)| (header, bytes.to_vec())))
            .collect::<Result<_, _>>()?;
        Ok((tails, entries))
    }

    fn parsed(body: &[u8]) -> Result<Vec<Tail>, Invalid> {
        let (tails, entries) = whole(body)?;
        assert_eq!(entries, [], "the body holds no entry");
        Ok(tails)
    }

    #[test]
    fn lays_out_the_fields_little_endian() {
        let expected = [
            vec![1, 0, 0, 0],
            vec![
                0x10, 0x0F, 0x0E, 0x0D, 0x0C, 0x0B, 0x0A, 0x09, 0x08, 0x07, 0x06, 0x05,
                0x04, 0x03, 0x02, 0x01,
            ],
            vec![1],
            vec![0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11],
            vec![0x22, 0x21, 0, 0, 0, 0, 0, 0],
            vec![1, 0xFE, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
        ]
        .concat();
        assert_eq!(body(&[tail(0x2122, Some(-2))]), expected);
        assert_eq!(expected.len(), body_len(1, 0, 0));
    }

    #[test]
    fn writes_no_stamp_as_a_zero_presence_byte_and_zero_stamp() {
        let body = body(&[tail(0, None)]);
        assert_eq!(body.get(37..46), Some(&[0; 9][..]));
        assert_eq!(parsed(&body), Ok(vec![tail(0, None)]));
    }

    #[test]
    fn an_empty_body_has_no_tails_and_no_entries_and_takes_four_bytes() {
        assert_eq!(body(&[]), [0, 0, 0, 0]);
        assert_eq!(parsed(&[0, 0, 0, 0]), Ok(vec![]));
    }

    #[test]
    fn refuses_a_body_that_ends_early() {
        let whole = body(&[tail(1, Some(5)), tail(2, None)]);
        for cut in 0..whole.len() {
            assert_eq!(
                parsed(&whole[..cut]),
                Err(Invalid::Truncated),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn refuses_bytes_after_the_last_tail() {
        let mut body = body(&[tail(1, Some(5))]);
        body.extend_from_slice(&[0; 3]);
        assert_eq!(parsed(&body), Err(Invalid::Trailing(3)));
    }

    #[test]
    fn refuses_a_count_over_the_maximum() {
        let over = u32::try_from(TAILS_MAX + 1).expect("fits");
        let mut long = over.to_le_bytes().to_vec();
        long.resize(4 + (TAILS_MAX + 1) * LEN, 0);
        assert_eq!(parsed(&long), Err(Invalid::Count(over)));
        let full = vec![tail(0, None); TAILS_MAX];
        assert_eq!(parsed(&body(&full)).map(|tails| tails.len()), Ok(TAILS_MAX));
    }

    #[test]
    fn refuses_an_entry_count_over_the_maximum() {
        let over = u16::try_from(ENTRIES_MAX + 1).expect("fits");
        let mut long = [[0, 0], over.to_le_bytes()].concat();
        long.resize(4 + (ENTRIES_MAX + 1) * entry::HEADER_LEN, 0);
        assert_eq!(whole(&long), Err(Invalid::Count(1024)));
        let header = header(1, 0);
        let full = vec![(header, &[][..]); ENTRIES_MAX];
        let entries =
            whole(&with_entries(&[], &full)).map(|(_, entries)| entries.len());
        assert_eq!(entries, Ok(ENTRIES_MAX));
    }

    #[test]
    fn one_block_body_holds_one_entry_of_entry_max_min_bytes() {
        let bytes = vec![9; Layout::ENTRY_MAX_MIN];
        let len = u32::try_from(bytes.len()).expect("fits");
        let body = with_entries(&[], &[(header(1, len), &bytes)]);
        assert_eq!(body.len(), 4087);
        assert_eq!(body.len(), body_len(0, 1, Layout::ENTRY_MAX_MIN));
        assert_eq!(whole(&body), Ok((vec![], vec![(header(1, len), bytes)])));
    }

    #[test]
    fn lays_out_the_entries_after_the_tails() {
        let body = with_entries(
            &[tail(1, None)],
            &[(header(1, 2), &[0xA1, 0xA2]), (header(2, 1), &[0xB1])],
        );
        assert_eq!(body.get(..4), Some(&[1, 0, 2, 0][..]));
        let headers = 4 + LEN;
        let bytes = headers + 2 * entry::HEADER_LEN;
        let first = header(1, 2).encode();
        assert_eq!(
            body.get(headers..headers + entry::HEADER_LEN),
            Some(&first[..])
        );
        assert_eq!(body.get(bytes..), Some(&[0xA1, 0xA2, 0xB1][..]));
    }

    #[test]
    fn refuses_a_body_that_ends_in_the_entries() {
        let whole_body = with_entries(&[tail(1, None)], &[(header(1, 3), &[1, 2, 3])]);
        for cut in 0..whole_body.len() {
            assert_eq!(
                whole(&whole_body[..cut]),
                Err(Invalid::Truncated),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn refuses_bytes_after_the_last_entry() {
        let mut body = with_entries(&[], &[(header(1, 1), &[5])]);
        body.push(0);
        assert_eq!(whole(&body), Err(Invalid::Trailing(1)));
    }

    #[test]
    fn ends_the_tails_at_a_wrong_path_byte() {
        let mut body = body(&[tail(1, None), tail(2, None), tail(3, None)]);
        body[4 + LEN + 16] = 2;
        let tails: Vec<_> = parse(&body).expect("the counts hold").tails.collect();
        assert_eq!(tails, [Ok(tail(1, None)), Err(Invalid::Path(2))]);
    }

    #[test]
    fn ends_the_tails_at_a_wrong_presence_byte() {
        let mut body = body(&[tail(1, None), tail(2, None)]);
        body[4 + 33] = 7;
        let tails: Vec<_> = parse(&body).expect("the counts hold").tails.collect();
        assert_eq!(tails, [Err(Invalid::Presence(7))]);
    }

    #[test]
    fn takes_each_given_up_to_the_largest_u64() {
        let body = body(&[tail(u64::MAX, None), tail(0, None)]);
        let tails: Vec<_> = parse(&body).expect("the counts hold").tails.collect();
        assert_eq!(tails, [Ok(tail(u64::MAX, None)), Ok(tail(0, None))]);
    }

    fn any_tail() -> impl Strategy<Value = Tail> {
        (
            any::<u128>(),
            prop::bool::ANY,
            any::<u64>(),
            any::<u64>(),
            prop::option::of(any::<i64>()),
        )
            .prop_map(|(index, backfill, seq, given, stamp)| Tail {
                index: channel::Key::from_u128(index),
                path: if backfill { Path::Backfill } else { Path::Live },
                seq,
                given,
                stamp: stamp.map(Stamp::from_nanos),
            })
    }

    proptest! {
        #[test]
        fn gives_back_the_tails_it_wrote(
            tails in prop::collection::vec(any_tail(), 0..40),
        ) {
            prop_assert_eq!(parsed(&body(&tails)), Ok(tails));
        }

        #[test]
        fn gives_back_the_tails_and_the_entries_it_wrote(
            tails in prop::collection::vec(any_tail(), 0..8),
            entries in prop::collection::vec(
                (1..=u8::MAX, prop::collection::vec(any::<u8>(), 0..64)),
                0..8,
            ),
        ) {
            let entries: Vec<_> = entries
                .into_iter()
                .map(|(tag, bytes)| {
                    let len = u32::try_from(bytes.len()).expect("fits");
                    (header(tag, len), bytes)
                })
                .collect();
            let given: Vec<_> = entries
                .iter()
                .map(|(header, bytes)| (*header, &bytes[..]))
                .collect();
            prop_assert_eq!(whole(&with_entries(&tails, &given)), Ok((tails, entries)));
        }
    }
}
