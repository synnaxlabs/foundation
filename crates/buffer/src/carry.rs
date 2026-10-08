//! The body of a carry record: the durable tails of paths whose newest data record
//! a later trim passes.
//!
//! ```text
//! [count: u32][count tails]
//! ```
//!
//! A tail is `index: u128, path: u8, seq: u64, given: u64, last: u8 + i64`,
//! little-endian and fixed width, with `last` as in an entry header.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

use std::mem;

use types::channel;
use types::frame::Path;
use types::time::Stamp;

use crate::entry::{self, Fields, Invalid};
use crate::log::Mark;

/// The encoded size of one [`Tail`].
const LEN: usize = 16 + 1 + 8 + 8 + 9;

/// The most tails one record holds. The body of a full record fits in the first
/// window that a walk reads, so the walk holds it whole.
pub(crate) const TAILS_MAX: usize = entry::ENTRIES_MAX;

/// Where one path of an index stood on disk when a commit carried it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tail {
    pub(crate) index: channel::Key,
    pub(crate) path: Path,
    /// The mark after the path's newest durable entry.
    pub(crate) end: Mark,
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
            &self.end.seq.to_le_bytes(),
            &self.end.given.to_le_bytes(),
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
            end: Mark { seq, given },
            stamp,
        })
    }
}

/// The size of the body that holds `count` tails.
///
/// # Panics
///
/// When `count` is over [`TAILS_MAX`].
#[cfg_attr(not(test), expect(dead_code, reason = "a commit writes carry records"))]
pub(crate) const fn body_len(count: usize) -> usize {
    assert!(
        count <= TAILS_MAX,
        "invariant: a carry record holds the tails"
    );
    match count.checked_mul(LEN) {
        Some(tails) => tails.checked_add(4),
        None => None,
    }
    .expect("invariant: a carry body fits in memory")
}

/// Writes the body that holds `tails` at the front of `into` and returns its size,
/// `body_len(tails.len())`.
///
/// # Panics
///
/// When there are over [`TAILS_MAX`] tails, or `into` is shorter than the body.
#[cfg_attr(not(test), expect(dead_code, reason = "a commit writes carry records"))]
pub(crate) fn write(tails: &[Tail], into: &mut [u8]) -> usize {
    let len = body_len(tails.len());
    let Some((body, _)) = into.split_at_mut_checked(len) else {
        panic!(
            "invariant: the body of {} tails takes {len} bytes, given {}",
            tails.len(),
            into.len()
        );
    };
    let count = u32::try_from(tails.len()).expect("invariant: TAILS_MAX fits a u32");
    let (slot, mut rest) = body.split_at_mut(4);
    slot.copy_from_slice(&count.to_le_bytes());
    for tail in tails {
        let (slot, after) = rest.split_at_mut(LEN);
        slot.copy_from_slice(&tail.encode());
        rest = after;
    }
    len
}

/// Gives each tail of a whole carry body, in order. The tails end at the first
/// invalid one, which is the last item.
///
/// # Errors
///
/// [`Invalid::Truncated`] when the body holds no count or fewer tails than the
/// count, [`Invalid::Count`] when the count is over [`TAILS_MAX`], and
/// [`Invalid::Trailing`] when bytes follow the last tail. Each tail checks its path
/// and presence bytes.
pub(crate) fn parse(
    body: &[u8],
) -> Result<impl Iterator<Item = Result<Tail, Invalid>>, Invalid> {
    let (count, tails) = body.split_first_chunk::<4>().ok_or(Invalid::Truncated)?;
    let count = u32::from_le_bytes(*count);
    let len = usize::try_from(count)
        .ok()
        .filter(|&count| count <= TAILS_MAX)
        .ok_or(Invalid::Count(count))?
        .checked_mul(LEN)
        .expect("invariant: TAILS_MAX tails fit in memory");
    let Some((tails, rest)) = tails.split_at_checked(len) else {
        return Err(Invalid::Truncated);
    };
    if !rest.is_empty() {
        return Err(Invalid::Trailing(rest.len()));
    }
    let mut failed = false;
    Ok(tails
        .as_chunks::<LEN>()
        .0
        .iter()
        .map(Tail::decode)
        .take_while(move |tail| !mem::replace(&mut failed, tail.is_err())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn tail(given: u64, stamp: Option<i64>) -> Tail {
        Tail {
            index: channel::Key::from_u128(0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10),
            path: Path::Backfill,
            end: Mark {
                seq: 0x1112_1314_1516_1718,
                given,
            },
            stamp: stamp.map(Stamp::from_nanos),
        }
    }

    fn body(tails: &[Tail]) -> Vec<u8> {
        let mut body = vec![0; body_len(tails.len())];
        assert_eq!(
            write(tails, &mut body),
            body.len(),
            "the tails fill the body"
        );
        body
    }

    fn parsed(body: &[u8]) -> Result<Vec<Tail>, Invalid> {
        parse(body)?.collect()
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
        assert_eq!(expected.len(), body_len(1));
    }

    #[test]
    fn writes_no_stamp_as_a_zero_presence_byte_and_zero_stamp() {
        let body = body(&[tail(0, None)]);
        assert_eq!(body.get(37..46), Some(&[0; 9][..]));
        assert_eq!(parsed(&body), Ok(vec![tail(0, None)]));
    }

    #[test]
    fn an_empty_body_has_no_tails_and_takes_four_bytes() {
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
    fn ends_the_tails_at_a_wrong_path_byte() {
        let mut body = body(&[tail(1, None), tail(2, None), tail(3, None)]);
        body[4 + LEN + 16] = 2;
        let tails: Vec<_> = parse(&body).expect("the count holds").collect();
        assert_eq!(tails, [Ok(tail(1, None)), Err(Invalid::Path(2))]);
    }

    #[test]
    fn ends_the_tails_at_a_wrong_presence_byte() {
        let mut body = body(&[tail(1, None), tail(2, None)]);
        body[4 + 33] = 7;
        let tails: Vec<_> = parse(&body).expect("the count holds").collect();
        assert_eq!(tails, [Err(Invalid::Presence(7))]);
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
                end: Mark { seq, given },
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
    }
}
