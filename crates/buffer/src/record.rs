//! The record format of the write-ahead ring. One record is one group commit, or one
//! mark of the ring itself:
//!
//! ```text
//! [len: u32][crc32c: u32][kind: u8][body: len bytes][padding]
//! ```
//!
//! Integers are little-endian. A record starts on an [`ALIGN`] boundary, so a later
//! record never rewrites a disk block that holds synced data. Padding is not checked.
//!
//! The CRC covers `len`, `kind`, and the body, and it continues from the CRC of the
//! record before: the chain. The writer starts a chain from a random value each
//! time it opens the ring. Bytes from an earlier chain, at a record start or inside
//! a body, then never read as the next record.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::string_slice
)]

use crate::crc32c;

/// Every record starts at a multiple of this many bytes.
pub(crate) const ALIGN: usize = 4096;

/// [`ALIGN`] as an offset in the ring.
#[expect(clippy::as_conversions, reason = "4096 fits any width")]
pub(crate) const BLOCK: u64 = ALIGN as u64;

/// Bytes of a record before its body.
pub(crate) const HEADER_LEN: usize = 9;

/// What a record holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// One group commit.
    Data,
    /// The rest of the area is not used; the next record is at its start.
    Wrap,
    /// The chain continues from the value in the body.
    Restart,
}

impl Kind {
    /// The kind byte on disk. Zero is never a kind, so a zeroed block is no record.
    pub(crate) const fn byte(self) -> u8 {
        match self {
            Self::Data => 1,
            Self::Wrap => 2,
            Self::Restart => 3,
        }
    }

    /// The kind for a byte from disk, or `None` for a byte that is not a kind.
    pub(crate) const fn decode(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Data),
            2 => Some(Self::Wrap),
            3 => Some(Self::Restart),
            _ => None,
        }
    }
}

/// The header of a record, before its CRC is checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    /// The kind byte. It is not zero; [`Kind::decode`] says whether it is known.
    pub(crate) kind: u8,
    /// Bytes of the body.
    pub(crate) len: usize,
    /// Bytes from the start of this record to the start of the next one.
    pub(crate) size: usize,
    /// The CRC the header claims: the chain value of the next record when the
    /// body checks out.
    pub(crate) crc: u32,
}

impl Head {
    /// The chain value after the header fields, which the body continues.
    pub(crate) fn chain(self, chain: u32) -> u32 {
        let len = u32::try_from(self.len).expect("invariant: a body length fits u32");
        let [l0, l1, l2, l3] = len.to_le_bytes();
        crc32c::append(chain, &[l0, l1, l2, l3, self.kind])
    }
}

/// The body of a record: the bytes that the writer gave to [`header`], joined.
/// `start` is its first bytes, in the first block of the record; `len` is its
/// whole length. A body read in one window has it all in `start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Body<'a> {
    pub(crate) start: &'a [u8],
    pub(crate) len: usize,
}

impl<'a> Body<'a> {
    /// The whole body, when `start` holds it all.
    pub(crate) fn whole(self) -> Option<&'a [u8]> {
        (self.start.len() == self.len).then_some(self.start)
    }
}

/// One record read from the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Record<'a> {
    /// The kind byte. It is not zero; [`Kind::decode`] says whether it is known.
    pub(crate) kind: u8,
    pub(crate) body: Body<'a>,
    /// Bytes from the start of this record to the start of the next one.
    pub(crate) size: usize,
    /// The chain value of the next record.
    pub(crate) crc: u32,
}

/// Makes the header of the record that follows `chain` for a body given in parts,
/// and returns it with the chain value of the next record. The writer puts the
/// header, then the parts in order, at an [`ALIGN`] boundary.
///
/// # Panics
///
/// When the parts hold more than `u32::MAX` bytes together.
pub(crate) fn header<'a>(
    chain: u32,
    kind: Kind,
    body: impl IntoIterator<Item = &'a [u8], IntoIter: Clone>,
) -> ([u8; HEADER_LEN], u32) {
    let body = body.into_iter();
    let len = body
        .clone()
        .try_fold(0u32, |len, part| {
            len.checked_add(u32::try_from(part.len()).ok()?)
        })
        .expect("invariant: a record holds at most u32::MAX bytes");
    let [l0, l1, l2, l3] = len.to_le_bytes();
    let kind = kind.byte();
    let mut crc = crc32c::append(chain, &[l0, l1, l2, l3, kind]);
    for part in body {
        crc = crc32c::append(crc, part);
    }
    let [c0, c1, c2, c3] = crc.to_le_bytes();
    ([l0, l1, l2, l3, c0, c1, c2, c3, kind], crc)
}

/// The header that the record at `bytes[0]` claims, before its CRC is checked, or
/// `None` when the bytes hold no header, the kind is zero, or the size overflows.
pub(crate) fn head(bytes: &[u8]) -> Option<Head> {
    let (head, _) = bytes.split_first_chunk::<HEADER_LEN>()?;
    let [l0, l1, l2, l3, c0, c1, c2, c3, kind] = *head;
    if kind == 0 {
        return None;
    }
    let len = usize::try_from(u32::from_le_bytes([l0, l1, l2, l3])).ok()?;
    let size = HEADER_LEN
        .checked_add(len)
        .and_then(|size| size.checked_next_multiple_of(ALIGN))?;
    Some(Head {
        kind,
        len,
        size,
        crc: u32::from_le_bytes([c0, c1, c2, c3]),
    })
}

/// Reads the record at `bytes[0]` and checks that it follows `chain`. `bytes` starts
/// at an [`ALIGN`] boundary of the ring. A record that runs past them is not read.
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
    let head = self::head(bytes)?;
    let body = bytes.get(HEADER_LEN..)?.get(..head.len)?;
    if crc32c::append(head.chain(chain), body) != head.crc {
        return None;
    }
    Some(Record {
        kind: head.kind,
        body: Body {
            start: body,
            len: head.len,
        },
        size: head.size,
        crc: head.crc,
    })
}

#[cfg(test)]
#[expect(clippy::arithmetic_side_effects, reason = "a test may panic")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A whole record as the writer puts it on disk, padded with `fill`, and the
    /// chain value of the next record.
    fn image(chain: u32, kind: Kind, parts: &[&[u8]], fill: u8) -> (Vec<u8>, u32) {
        let (header, crc) = header(chain, kind, parts.iter().copied());
        let mut image = header.to_vec();
        for part in parts {
            image.extend_from_slice(part);
        }
        image.resize(image.len().next_multiple_of(ALIGN), fill);
        (image, crc)
    }

    fn data(chain: u32, body: &[u8]) -> (Vec<u8>, u32) {
        image(chain, Kind::Data, &[body], 0)
    }

    fn kind() -> impl Strategy<Value = Kind> {
        prop::sample::select(vec![Kind::Data, Kind::Wrap, Kind::Restart])
    }

    fn parts() -> impl Strategy<Value = Vec<Vec<u8>>> {
        prop::collection::vec(prop::collection::vec(any::<u8>(), 0..3000), 0..4)
    }

    fn slices(parts: &[Vec<u8>]) -> Vec<&[u8]> {
        parts.iter().map(Vec::as_slice).collect()
    }

    mod kind {
        use super::*;

        #[test]
        fn decodes_only_the_bytes_of_the_three_kinds() {
            let known = [(1, Kind::Data), (2, Kind::Wrap), (3, Kind::Restart)];
            for byte in 0..=u8::MAX {
                let kind = known.iter().find(|(known, _)| *known == byte);
                assert_eq!(Kind::decode(byte), kind.map(|(_, kind)| *kind));
            }
            for (byte, kind) in known {
                assert_eq!(kind.byte(), byte);
            }
        }
    }

    mod header {
        use super::*;

        #[test]
        #[should_panic(expected = "a record holds at most u32::MAX bytes")]
        fn panics_on_a_body_over_u32_max() {
            let part = vec![0; 1 << 20];
            let _ = header(7, Kind::Data, vec![part.as_slice(); 1 << 12]);
        }

        /// The CRC values come from another CRC32C implementation.
        #[test]
        fn lays_out_len_crc_and_kind_little_endian() {
            let first = header(0x0102_0304, Kind::Data, [b"ab".as_slice(), b"c"]);
            let bytes = [3, 0, 0, 0, 0xCC, 0x3B, 0x83, 0xA6, 1];
            assert_eq!(first, (bytes, 0xA683_3BCC));
            let second = header(first.1, Kind::Wrap, [b"defg".as_slice()]);
            let bytes = [4, 0, 0, 0, 0x5C, 0x6F, 0xB9, 0xEF, 2];
            assert_eq!(second, (bytes, 0xEFB9_6F5C));
        }
    }

    mod read {
        use super::*;

        #[test]
        fn claims_the_header_without_its_crc_checked() {
            let (mut image, crc) = data(1, &[7; 5000]);
            let claimed = Head {
                kind: 1,
                len: 5000,
                size: 8192,
                crc,
            };
            assert_eq!(head(&image), Some(claimed));
            image[4] ^= 1;
            assert_eq!(head(&image).map(|head| head.size), Some(8192));
            assert_eq!(head(&image).map(|head| head.crc), Some(crc ^ 1));
            image[8] = 0;
            assert_eq!(head(&image), None);
            assert_eq!(head(&image[..8]), None);
            let mut huge = [0; HEADER_LEN];
            huge[..4].copy_from_slice(&u32::MAX.to_le_bytes());
            huge[8] = 1;
            let expected = (usize::try_from(u32::MAX).ok())
                .and_then(|len| (HEADER_LEN + len).checked_next_multiple_of(ALIGN));
            assert_eq!(head(&huge).map(|head| head.size), expected);
        }

        #[test]
        fn continues_the_chain_with_the_header_fields() {
            let (image, crc) = data(1, b"abc");
            let head = head(&image).expect("a header");
            assert_eq!(crc32c::append(head.chain(1), b"abc"), crc);
            assert_eq!(
                Body {
                    start: b"ab",
                    len: 3
                }
                .whole(),
                None
            );
            assert_eq!(
                Body {
                    start: b"abc",
                    len: 3
                }
                .whole(),
                Some(&b"abc"[..])
            );
        }

        #[test]
        fn reads_no_record_from_a_zeroed_block() {
            assert_eq!(read(&[0; ALIGN], 0), None);
        }

        #[test]
        fn reads_no_record_from_no_bytes() {
            assert_eq!(read(&[], 1), None);
        }

        #[test]
        #[should_panic(expected = "4095 bytes of ring are not a multiple of 4096")]
        fn panics_on_bytes_that_are_not_aligned() {
            let _record = read(&[0; ALIGN - 1], 1);
        }

        #[test]
        fn rejects_a_zero_kind_with_its_right_crc() {
            let crc = crc32c::append(1, &[0; 5]);
            let mut image = vec![0; ALIGN];
            image[4..8].copy_from_slice(&crc.to_le_bytes());
            assert_eq!(read(&image, 1), None);
        }

        #[test]
        fn reads_a_record_with_no_body_and_a_kind_it_does_not_know() {
            let crc = crc32c::append(1, &[0, 0, 0, 0, 9]);
            let mut image = vec![0; ALIGN];
            image[4..8].copy_from_slice(&crc.to_le_bytes());
            image[8] = 9;
            let expected = Record {
                kind: 9,
                body: Body { start: &[], len: 0 },
                size: ALIGN,
                crc,
            };
            assert_eq!(read(&image, 1), Some(expected));
        }

        #[test]
        fn pads_a_record_to_4096_bytes() {
            let cases = [(0, 4096), (4087, 4096), (4088, 8192), (8183, 8192)];
            for (len, size) in cases {
                let (image, _) = data(1, &vec![0xA5; len]);
                assert_eq!(image.len(), size, "body of {len} bytes");
                assert_eq!(read(&image, 1).map(|record| record.size), Some(size));
            }
        }

        #[test]
        fn reads_the_second_record_with_the_crc_of_the_first() {
            let (mut ring, crc) = data(4, b"first");
            ring.extend(data(crc, b"second").0);
            let first = read(&ring, 4).expect("the first record is whole");
            assert_eq!((first.body.whole(), first.crc), (Some(&b"first"[..]), crc));
            let second =
                read(&ring[first.size..], first.crc).and_then(|r| r.body.whole());
            assert_eq!(second, Some(&b"second"[..]));
        }

        /// The log was cut at a damaged record and written again from there. The
        /// old record after the cut point must not return.
        #[test]
        fn drops_an_old_record_after_a_rewritten_one() {
            let (mut ring, crc) = data(4, b"old three");
            ring.extend(data(crc, b"old four").0);
            let (new, crc) = data(4, b"new three");
            ring[..ALIGN].copy_from_slice(&new);
            assert_eq!(
                read(&ring, 4).and_then(|r| r.body.whole()),
                Some(&b"new three"[..])
            );
            assert_eq!(read(&ring[ALIGN..], crc), None);
        }

        /// A body from an earlier chain holds the image of a whole record, and a
        /// shorter record of the new chain ends where that image starts.
        #[test]
        fn drops_a_record_image_inside_an_old_body() {
            let (inner, _) = data(4, b"forged");
            let filler = vec![0xA5; ALIGN - HEADER_LEN];
            let (mut ring, _) = image(9, Kind::Data, &[&filler, &inner], 0);
            let (new, crc) = data(4, b"new");
            ring[..ALIGN].copy_from_slice(&new);
            assert_eq!(
                read(&ring[ALIGN..], 4).and_then(|r| r.body.whole()),
                Some(&b"forged"[..])
            );
            assert_eq!(read(&ring[ALIGN..], crc), None);
        }

        proptest! {
            #[test]
            fn returns_the_kind_the_body_the_padded_size_and_the_crc(
                chain in any::<u32>(),
                kind in kind(),
                parts in parts(),
                fill in any::<u8>(),
                after in prop::collection::vec(any::<u8>(), ALIGN),
            ) {
                let body = parts.concat();
                let (mut ring, crc) = image(chain, kind, &slices(&parts), fill);
                let size = ring.len();
                ring.extend(after);
                prop_assert_eq!(size % ALIGN, 0);
                prop_assert!(size >= HEADER_LEN + body.len());
                prop_assert!(size < HEADER_LEN + body.len() + ALIGN);
                let body = Body { start: &body, len: body.len() };
                let expected = Record { kind: kind.byte(), body, size, crc };
                prop_assert_eq!(read(&ring, chain), Some(expected));
            }

            #[test]
            fn rejects_another_chain(
                chain in any::<u32>(),
                other in any::<u32>(),
                parts in parts(),
            ) {
                prop_assume!(chain != other);
                let (image, _) = image(chain, Kind::Data, &slices(&parts), 0);
                prop_assert_eq!(read(&image, other), None);
            }

            #[test]
            fn rejects_one_flipped_bit(
                chain in any::<u32>(),
                kind in kind(),
                parts in parts(),
                at in any::<prop::sample::Index>(),
                bit in 0..8u32,
            ) {
                let written = HEADER_LEN + parts.concat().len();
                let (mut image, _) = image(chain, kind, &slices(&parts), 0);
                // A longer record may run into this one's padding; make it differ.
                image.extend([0xFF; ALIGN]);
                image[at.index(written)] ^= 1 << bit;
                prop_assert_eq!(read(&image, chain), None);
            }

            #[test]
            fn rejects_a_record_cut_at_a_boundary(
                chain in any::<u32>(),
                body in prop::collection::vec(any::<u8>(), ALIGN..3 * ALIGN),
                at in any::<prop::sample::Index>(),
            ) {
                let (image, _) = data(chain, &body);
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
