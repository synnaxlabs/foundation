//! The record format of the write-ahead ring. One record is one group commit:
//!
//! ```text
//! [len: u32][crc32c: u32][number: u64][payload: len bytes][padding]
//! ```
//!
//! Integers are little-endian. A record starts on a [`BLOCK`] boundary, so a later
//! record never rewrites a disk block that holds synced data. Numbers go up by one.
//! The CRC covers `len`, `number`, and the payload, so a record from an earlier lap
//! of the ring never reads as the next record. Padding is not checked.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

use crate::crc::Crc32c;

/// Every record starts at a multiple of this many bytes.
pub(crate) const BLOCK: usize = 4096;

/// Bytes of a record before its payload.
pub(crate) const HEADER: usize = 16;

/// One record read from the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Record<'a> {
    /// The bytes that the writer gave to [`header`], joined.
    pub(crate) payload: &'a [u8],
    /// Bytes from the start of this record to the start of the next one.
    pub(crate) size: usize,
}

/// Why no record with the expected number starts at an offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The space was never written, or less than a header is left.
    End,
    /// A whole record is here, but it has another number: an earlier lap wrote it.
    Stale {
        /// The number of the record found.
        found: u64,
    },
    /// The length or the CRC is wrong: a torn write or damaged bytes.
    Corrupt,
}

/// Makes the header of record `number` for a payload given in parts. The writer puts
/// the header, then the parts in order, at a [`BLOCK`] boundary.
///
/// # Panics
///
/// When the parts hold no bytes or more than `u32::MAX` bytes together.
pub(crate) fn header(number: u64, payload: &[&[u8]]) -> [u8; HEADER] {
    let len = payload
        .iter()
        .try_fold(0u32, |len, part| {
            len.checked_add(u32::try_from(part.len()).ok()?)
        })
        .unwrap_or_else(|| {
            panic!("invariant: record {number} holds more than u32::MAX bytes")
        });
    assert!(len != 0, "invariant: record {number} holds no bytes");
    let len = len.to_le_bytes();
    let number = number.to_le_bytes();
    let mut crc = Crc32c::new();
    crc.update(&len);
    crc.update(&number);
    for part in payload {
        crc.update(part);
    }
    let mut header = [0; HEADER];
    let (head, tail) = header.split_at_mut(8);
    let (head_len, head_crc) = head.split_at_mut(4);
    head_len.copy_from_slice(&len);
    head_crc.copy_from_slice(&crc.finish().to_le_bytes());
    tail.copy_from_slice(&number);
    header
}

/// Reads the record that starts at `bytes[0]` and checks that it is record `number`.
/// `bytes` runs from a [`BLOCK`] boundary to the end of the ring file.
///
/// # Errors
///
/// A [`Stop`] when no such record starts here. Each one ends the log at recovery.
pub(crate) fn read(bytes: &[u8], number: u64) -> Result<Record<'_>, Stop> {
    let Some((head, body)) = bytes.split_first_chunk::<HEADER>() else {
        return Err(Stop::End);
    };
    if *head == [0; HEADER] {
        return Err(Stop::End);
    }
    let [l0, l1, l2, l3, c0, c1, c2, c3, found @ ..] = *head;
    let len = [l0, l1, l2, l3];
    let payload = usize::try_from(u32::from_le_bytes(len))
        .ok()
        .filter(|&len| len != 0)
        .and_then(|len| body.get(..len))
        .ok_or(Stop::Corrupt)?;
    let size = HEADER
        .checked_add(payload.len())
        .and_then(|size| size.checked_next_multiple_of(BLOCK))
        .filter(|&size| size <= bytes.len())
        .ok_or(Stop::Corrupt)?;
    let mut crc = Crc32c::new();
    crc.update(&len);
    crc.update(&found);
    crc.update(payload);
    if crc.finish() != u32::from_le_bytes([c0, c1, c2, c3]) {
        return Err(Stop::Corrupt);
    }
    let found = u64::from_le_bytes(found);
    if found != number {
        return Err(Stop::Stale { found });
    }
    Ok(Record { payload, size })
}

#[cfg(test)]
#[expect(clippy::arithmetic_side_effects, reason = "a test may panic")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A whole record as the writer puts it on disk, padded with `fill`.
    fn image(number: u64, parts: &[&[u8]], fill: u8) -> Vec<u8> {
        let mut image = header(number, parts).to_vec();
        for part in parts {
            image.extend_from_slice(part);
        }
        image.resize(image.len().next_multiple_of(BLOCK), fill);
        image
    }

    fn parts() -> impl Strategy<Value = Vec<Vec<u8>>> {
        prop::collection::vec(prop::collection::vec(any::<u8>(), 0..3000), 1..4)
            .prop_filter("a record holds at least one byte", |parts| {
                parts.iter().any(|part| !part.is_empty())
            })
    }

    fn slices(parts: &[Vec<u8>]) -> Vec<&[u8]> {
        parts.iter().map(Vec::as_slice).collect()
    }

    mod header {
        use super::*;

        #[test]
        #[should_panic(expected = "invariant: record 7 holds no bytes")]
        fn panics_on_an_empty_payload() {
            let _ = header(7, &[&[], &[]]);
        }

        #[test]
        #[should_panic(expected = "invariant: record 7 holds more than u32::MAX bytes")]
        fn panics_on_a_payload_over_u32_max() {
            let part = vec![0; 1 << 20];
            let _ = header(7, &vec![part.as_slice(); 1 << 12]);
        }

        #[test]
        fn lays_out_len_crc_and_number_little_endian() {
            let header = header(0x0102_0304_0506_0708, &[b"ab", b"c"]);
            assert_eq!(header[..4], [3, 0, 0, 0]);
            assert_eq!(header[8..], [8, 7, 6, 5, 4, 3, 2, 1]);
            let mut crc = Crc32c::new();
            crc.update(&header[..4]);
            crc.update(&header[8..]);
            crc.update(b"abc");
            assert_eq!(header[4..8], crc.finish().to_le_bytes());
        }
    }

    mod read {
        use super::*;

        #[test]
        fn stops_at_the_end_on_a_zeroed_block() {
            assert_eq!(read(&[0; BLOCK], 0), Err(Stop::End));
        }

        #[test]
        fn stops_at_the_end_when_less_than_a_header_is_left() {
            let image = image(1, &[b"abc"], 0);
            assert_eq!(read(&image[..HEADER - 1], 1), Err(Stop::End));
            assert_eq!(read(&[], 1), Err(Stop::End));
        }

        #[test]
        fn reports_a_zero_length_with_other_header_bytes_as_corrupt() {
            let mut image = image(1, &[b"abc"], 0);
            image[..4].fill(0);
            assert_eq!(read(&image, 1), Err(Stop::Corrupt));
        }

        #[test]
        fn reads_a_payload_that_fills_its_last_block() {
            let payload = vec![0xA5; 2 * BLOCK - HEADER];
            let image = image(9, &[&payload], 0);
            assert_eq!(image.len(), 2 * BLOCK);
            let expected = Record {
                payload: &payload,
                size: 2 * BLOCK,
            };
            assert_eq!(read(&image, 9), Ok(expected));
        }

        #[test]
        fn reads_only_the_first_record_of_two() {
            let mut ring = image(4, &[b"first"], 0);
            ring.extend(image(5, &[b"second"], 0));
            let first = read(&ring, 4).expect("the first record is whole");
            assert_eq!(first.payload, b"first");
            let second = read(&ring[first.size..], 5);
            let expected = Record {
                payload: b"second",
                size: BLOCK,
            };
            assert_eq!(second, Ok(expected));
        }

        proptest! {
            #[test]
            fn returns_the_payload_and_the_padded_size(
                number in any::<u64>(),
                parts in parts(),
                fill in any::<u8>(),
                after in prop::collection::vec(any::<u8>(), 0..64),
            ) {
                let payload = parts.concat();
                let mut ring = image(number, &slices(&parts), fill);
                let size = ring.len();
                ring.extend(after);
                prop_assert_eq!(size % BLOCK, 0);
                prop_assert!(size >= HEADER + payload.len());
                prop_assert!(size < HEADER + payload.len() + BLOCK);
                let expected = Record { payload: &payload, size };
                prop_assert_eq!(read(&ring, number), Ok(expected));
            }

            #[test]
            fn reports_another_number_as_stale(
                number in any::<u64>(),
                expected in any::<u64>(),
                parts in parts(),
            ) {
                prop_assume!(number != expected);
                let image = image(number, &slices(&parts), 0);
                prop_assert_eq!(
                    read(&image, expected),
                    Err(Stop::Stale { found: number })
                );
            }

            #[test]
            fn reports_one_flipped_bit_as_corrupt(
                number in any::<u64>(),
                parts in parts(),
                at in any::<prop::sample::Index>(),
                bit in 0..8u32,
            ) {
                let written = HEADER + parts.concat().len();
                let mut image = image(number, &slices(&parts), 0);
                // A longer record may run into this one's padding; make it differ.
                image.extend([0xFF; BLOCK]);
                image[at.index(written)] ^= 1 << bit;
                prop_assert_eq!(read(&image, number), Err(Stop::Corrupt));
            }

            #[test]
            fn reports_a_truncated_record_as_corrupt(
                number in any::<u64>(),
                parts in parts(),
                at in any::<prop::sample::Index>(),
            ) {
                let image = image(number, &slices(&parts), 0);
                let kept = HEADER + at.index(image.len() - HEADER);
                prop_assert_eq!(read(&image[..kept], number), Err(Stop::Corrupt));
            }
        }
    }
}
