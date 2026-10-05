//! The record format of the write-ahead ring. One record is one group commit:
//!
//! ```text
//! [len: u32][crc32c: u32][payload: len bytes][padding]
//! ```
//!
//! Integers are little-endian. A record starts on an [`ALIGN`] boundary, so a later
//! record never rewrites a disk block that holds synced data. Padding is not checked.
//!
//! The CRC covers `len` and the payload, and it continues from the CRC of the record
//! before: the chain. The writer starts a chain from a random value each time it
//! opens the ring. Bytes from an earlier chain, at a record start or inside a
//! payload, then never read as the next record.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

use crate::crc32c::Crc32c;

/// Every record starts at a multiple of this many bytes.
pub(crate) const ALIGN: usize = 4096;

/// Bytes of a record before its payload.
pub(crate) const HEADER_LEN: usize = 8;

/// One record read from the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Record<'a> {
    /// The bytes that the writer gave to [`header`], joined.
    pub(crate) payload: &'a [u8],
    /// Bytes from the start of this record to the start of the next one.
    pub(crate) size: usize,
    /// The chain value of the next record.
    pub(crate) crc: u32,
}

/// Makes the header of the record that follows `chain` for a payload given in parts,
/// and returns it with the chain value of the next record. The writer puts the
/// header, then the parts in order, at an [`ALIGN`] boundary.
///
/// # Panics
///
/// When the parts hold no bytes or more than `u32::MAX` bytes together.
pub(crate) fn header(chain: u32, payload: &[&[u8]]) -> ([u8; HEADER_LEN], u32) {
    let len = payload
        .iter()
        .try_fold(0u32, |len, part| {
            len.checked_add(u32::try_from(part.len()).ok()?)
        })
        .expect("invariant: a record holds at most u32::MAX bytes");
    assert!(len != 0, "invariant: a record holds at least one byte");
    let [l0, l1, l2, l3] = len.to_le_bytes();
    let mut crc = Crc32c::resume(chain);
    crc.update(&[l0, l1, l2, l3]);
    for part in payload {
        crc.update(part);
    }
    let crc = crc.finish();
    let [c0, c1, c2, c3] = crc.to_le_bytes();
    ([l0, l1, l2, l3, c0, c1, c2, c3], crc)
}

/// Reads the record at `bytes[0]` and checks that it follows `chain`. `bytes` runs
/// from an [`ALIGN`] boundary to the end of the ring file.
///
/// Returns `None` when the chain ends here: the space was never written, the write
/// was torn, the bytes are damaged, or they belong to another chain.
///
/// # Panics
///
/// When the length of `bytes` is not a multiple of [`ALIGN`].
pub(crate) fn read(bytes: &[u8], chain: u32) -> Option<Record<'_>> {
    assert!(
        bytes.len().is_multiple_of(ALIGN),
        "invariant: {} bytes of ring are not a multiple of {ALIGN}",
        bytes.len()
    );
    let (head, body) = bytes.split_first_chunk::<HEADER_LEN>()?;
    let [l0, l1, l2, l3, c0, c1, c2, c3] = *head;
    let len = [l0, l1, l2, l3];
    let payload = usize::try_from(u32::from_le_bytes(len))
        .ok()
        .filter(|&len| len != 0)
        .and_then(|len| body.get(..len))?;
    let mut crc = Crc32c::resume(chain);
    crc.update(&len);
    crc.update(payload);
    let crc = crc.finish();
    if crc != u32::from_le_bytes([c0, c1, c2, c3]) {
        return None;
    }
    let size = HEADER_LEN
        .checked_add(payload.len())
        .and_then(|size| size.checked_next_multiple_of(ALIGN))?;
    Some(Record { payload, size, crc })
}

#[cfg(test)]
#[expect(clippy::arithmetic_side_effects, reason = "a test may panic")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A whole record as the writer puts it on disk, padded with `fill`, and the
    /// chain value of the next record.
    fn image(chain: u32, parts: &[&[u8]], fill: u8) -> (Vec<u8>, u32) {
        let (header, crc) = header(chain, parts);
        let mut image = header.to_vec();
        for part in parts {
            image.extend_from_slice(part);
        }
        image.resize(image.len().next_multiple_of(ALIGN), fill);
        (image, crc)
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
        #[should_panic(expected = "invariant: a record holds at least one byte")]
        fn panics_on_an_empty_payload() {
            let _ = header(7, &[&[], &[]]);
        }

        #[test]
        #[should_panic(expected = "a record holds at most u32::MAX bytes")]
        fn panics_on_a_payload_over_u32_max() {
            let part = vec![0; 1 << 20];
            let _ = header(7, &vec![part.as_slice(); 1 << 12]);
        }

        /// The CRC values come from another CRC32C implementation.
        #[test]
        fn lays_out_len_and_crc_little_endian() {
            let first = header(0x0102_0304, &[b"ab", b"c"]);
            assert_eq!(first, ([3, 0, 0, 0, 0x13, 0xF8, 0x2F, 0x69], 0x692F_F813));
            let second = header(first.1, &[b"defg"]);
            assert_eq!(second, ([4, 0, 0, 0, 0x8C, 0xCE, 0x87, 0x66], 0x6687_CE8C));
        }
    }

    mod read {
        use super::*;

        #[test]
        fn reads_no_record_from_a_zeroed_block() {
            assert_eq!(read(&[0; ALIGN], 0), None);
        }

        #[test]
        fn reads_no_record_at_the_end_of_the_ring_file() {
            assert_eq!(read(&[], 1), None);
        }

        #[test]
        #[should_panic(expected = "4095 bytes of ring are not a multiple of 4096")]
        fn panics_on_bytes_that_are_not_aligned() {
            let _stop = read(&[0; ALIGN - 1], 1);
        }

        #[test]
        fn rejects_a_zero_length_with_its_right_crc() {
            let mut crc = Crc32c::resume(1);
            crc.update(&[0; 4]);
            let mut image = vec![0; ALIGN];
            image[4..8].copy_from_slice(&crc.finish().to_le_bytes());
            assert_eq!(read(&image, 1), None);
        }

        #[test]
        fn pads_a_record_to_4096_bytes() {
            let cases = [(1, 4096), (4088, 4096), (4089, 8192), (8184, 8192)];
            for (len, size) in cases {
                let (image, _) = image(1, &[&vec![0xA5; len]], 0);
                assert_eq!(image.len(), size, "payload of {len} bytes");
                assert_eq!(read(&image, 1).map(|record| record.size), Some(size));
            }
        }

        #[test]
        fn reads_the_second_record_with_the_crc_of_the_first() {
            let (mut ring, crc) = image(4, &[b"first"], 0);
            ring.extend(image(crc, &[b"second"], 0).0);
            let first = read(&ring, 4).expect("the first record is whole");
            assert_eq!((first.payload, first.crc), (&b"first"[..], crc));
            let second = read(&ring[first.size..], first.crc).map(|r| r.payload);
            assert_eq!(second, Some(&b"second"[..]));
        }

        /// The log was cut at a damaged record and written again from there. The
        /// old record after the cut point must not return.
        #[test]
        fn drops_an_old_record_after_a_rewritten_one() {
            let (mut ring, crc) = image(4, &[b"old three"], 0);
            ring.extend(image(crc, &[b"old four"], 0).0);
            let (new, crc) = image(4, &[b"new three"], 0);
            ring[..ALIGN].copy_from_slice(&new);
            assert_eq!(read(&ring, 4).map(|r| r.payload), Some(&b"new three"[..]));
            assert_eq!(read(&ring[ALIGN..], crc), None);
        }

        /// A payload from an earlier chain holds the image of a whole record, and
        /// a shorter record of the new chain ends where that image starts.
        #[test]
        fn drops_a_record_image_inside_an_old_payload() {
            let (inner, _) = image(4, &[b"forged"], 0);
            let filler = vec![0xA5; ALIGN - HEADER_LEN];
            let (mut ring, _) = image(9, &[&filler, &inner], 0);
            let (new, crc) = image(4, &[b"new"], 0);
            ring[..ALIGN].copy_from_slice(&new);
            assert_eq!(
                read(&ring[ALIGN..], 4).map(|r| r.payload),
                Some(&b"forged"[..])
            );
            assert_eq!(read(&ring[ALIGN..], crc), None);
        }

        proptest! {
            #[test]
            fn returns_the_payload_the_padded_size_and_the_crc(
                chain in any::<u32>(),
                parts in parts(),
                fill in any::<u8>(),
                after in prop::collection::vec(any::<u8>(), ALIGN),
            ) {
                let payload = parts.concat();
                let (mut ring, crc) = image(chain, &slices(&parts), fill);
                let size = ring.len();
                ring.extend(after);
                prop_assert_eq!(size % ALIGN, 0);
                prop_assert!(size >= HEADER_LEN + payload.len());
                prop_assert!(size < HEADER_LEN + payload.len() + ALIGN);
                let expected = Record { payload: &payload, size, crc };
                prop_assert_eq!(read(&ring, chain), Some(expected));
            }

            #[test]
            fn rejects_another_chain(
                chain in any::<u32>(),
                other in any::<u32>(),
                parts in parts(),
            ) {
                prop_assume!(chain != other);
                let (image, _) = image(chain, &slices(&parts), 0);
                prop_assert_eq!(read(&image, other), None);
            }

            #[test]
            fn rejects_one_flipped_bit(
                chain in any::<u32>(),
                parts in parts(),
                at in any::<prop::sample::Index>(),
                bit in 0..8u32,
            ) {
                let written = HEADER_LEN + parts.concat().len();
                let (mut image, _) = image(chain, &slices(&parts), 0);
                // A longer record may run into this one's padding; make it differ.
                image.extend([0xFF; ALIGN]);
                image[at.index(written)] ^= 1 << bit;
                prop_assert_eq!(read(&image, chain), None);
            }

            #[test]
            fn rejects_a_record_cut_at_a_boundary(
                chain in any::<u32>(),
                payload in prop::collection::vec(any::<u8>(), ALIGN..3 * ALIGN),
                at in any::<prop::sample::Index>(),
            ) {
                let (image, _) = image(chain, &[&payload], 0);
                let kept = (1 + at.index(image.len() / ALIGN - 1)) * ALIGN;
                prop_assert_eq!(read(&image[..kept], chain), None);
            }

            #[test]
            fn never_reads_past_any_bytes(
                len in 0..8192u32,
                rest in prop::collection::vec(any::<u8>(), 2 * ALIGN - 4),
                chain in any::<u32>(),
            ) {
                let mut bytes = len.to_le_bytes().to_vec();
                bytes.extend(rest);
                if let Some(record) = read(&bytes, chain) {
                    prop_assert!(record.size <= bytes.len());
                    prop_assert_eq!(record.size % ALIGN, 0);
                }
            }
        }
    }
}
