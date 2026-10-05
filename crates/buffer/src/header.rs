//! The ring header: what the ring knows about itself between opens. Two blocks at
//! the start of the ring hold the last two checkpoints, so a torn write of one
//! leaves the other whole.
//!
//! ```text
//! [magic: 8][version: u16][area: u64][body_max: u32][tail offset: u64]
//! [tail chain: u32][seq: u64][crc32c: u32][zero padding to 4096]
//! ```
//!
//! Integers are little-endian. The CRC covers the bytes before it.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions
)]

use crate::crc32c::Crc32c;
use crate::record::ALIGN;
use crate::wal::{Layout, Position, Unaligned, Unfit};

const MAGIC: [u8; 8] = *b"FNDNRING";
const VERSION: u16 = 1;
const FIELDS: usize = 42;
const CRC: usize = 46;

/// Bytes of the ring before its area: the two header blocks.
pub(crate) const LEN: u64 = 2 * 4096;

/// Why neither header block can be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// No block has the magic and a valid CRC: the file is not a ring, or both
    /// blocks are damaged.
    Missing,
    /// The newer valid block has a format version this build does not read.
    Version(u16),
    /// The newer valid block holds sizes that do not make a ring.
    Unfit(Unfit),
    /// The newer valid block holds a tail that is not on a block boundary.
    Unaligned(Unaligned),
}

impl From<Unfit> for Error {
    fn from(unfit: Unfit) -> Self {
        Self::Unfit(unfit)
    }
}

impl From<Unaligned> for Error {
    fn from(unaligned: Unaligned) -> Self {
        Self::Unaligned(unaligned)
    }
}

/// What the ring knows about itself between opens: its sizes, the boundary before
/// its oldest live record, and the number of this checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) layout: Layout,
    pub(crate) tail: Position,
    pub(crate) seq: u64,
}

impl Header {
    /// Encodes this checkpoint as one block.
    pub(crate) fn encode(&self) -> [u8; ALIGN] {
        let body_max = u32::try_from(self.layout.body_max())
            .expect("invariant: a layout holds a body of at most u32::MAX bytes");
        let mut block = [0; ALIGN];
        let mut rest = block.as_mut_slice();
        for field in [
            &MAGIC[..],
            &VERSION.to_le_bytes(),
            &self.layout.area().to_le_bytes(),
            &body_max.to_le_bytes(),
            &self.tail.offset().to_le_bytes(),
            &self.tail.chain().to_le_bytes(),
            &self.seq.to_le_bytes(),
        ] {
            let (slot, after) = rest.split_at_mut(field.len());
            slot.copy_from_slice(field);
            rest = after;
        }
        let (fields, rest) = block.split_at_mut(FIELDS);
        let mut crc = Crc32c::new();
        crc.update(fields);
        let (slot, _) = rest.split_at_mut(4);
        slot.copy_from_slice(&crc.finish().to_le_bytes());
        block
    }

    /// Where this checkpoint goes: block `seq mod 2`.
    pub(crate) fn place(&self) -> u64 {
        (self.seq % 2).saturating_mul(4096)
    }

    /// Reads the newer valid one of the two header blocks.
    ///
    /// # Errors
    ///
    /// [`Error`] when neither block can be used.
    pub(crate) fn decode(
        first: &[u8; ALIGN],
        second: &[u8; ALIGN],
    ) -> Result<Self, Error> {
        let newer = [first, second]
            .into_iter()
            .filter_map(Raw::read)
            .max_by_key(|raw| raw.seq)
            .ok_or(Error::Missing)?;
        if newer.version != VERSION {
            return Err(Error::Version(newer.version));
        }
        let body_max = usize::try_from(newer.body_max).unwrap_or(usize::MAX);
        Ok(Self {
            layout: Layout::new(newer.area, body_max)?,
            tail: Position::new(newer.offset, newer.chain)?,
            seq: newer.seq,
        })
    }
}

/// The fields of one block whose magic and CRC are right.
struct Raw {
    version: u16,
    area: u64,
    body_max: u32,
    offset: u64,
    chain: u32,
    seq: u64,
}

impl Raw {
    fn read(block: &[u8; ALIGN]) -> Option<Self> {
        let (fields, rest) = block.split_first_chunk::<FIELDS>()?;
        let (stored, _) = rest.split_first_chunk::<4>()?;
        let mut crc = Crc32c::new();
        crc.update(fields);
        if crc.finish() != u32::from_le_bytes(*stored) {
            return None;
        }
        let (magic, rest) = fields.split_first_chunk::<8>()?;
        if *magic != MAGIC {
            return None;
        }
        let (version, rest) = rest.split_first_chunk::<2>()?;
        let (area, rest) = rest.split_first_chunk::<8>()?;
        let (body_max, rest) = rest.split_first_chunk::<4>()?;
        let (offset, rest) = rest.split_first_chunk::<8>()?;
        let (chain, rest) = rest.split_first_chunk::<4>()?;
        let (seq, _) = rest.split_first_chunk::<8>()?;
        Some(Self {
            version: u16::from_le_bytes(*version),
            area: u64::from_le_bytes(*area),
            body_max: u32::from_le_bytes(*body_max),
            offset: u64::from_le_bytes(*offset),
            chain: u32::from_le_bytes(*chain),
            seq: u64::from_le_bytes(*seq),
        })
    }
}

#[cfg(test)]
#[expect(clippy::arithmetic_side_effects, reason = "a test may panic")]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn header(area: u64, body_max: usize, offset: u64, chain: u32, seq: u64) -> Header {
        Header {
            layout: Layout::new(area, body_max).expect("the test sizes make a ring"),
            tail: Position::new(offset, chain).expect("the test tail is aligned"),
            seq,
        }
    }

    fn any_header() -> impl Strategy<Value = Header> {
        (1..64u64, 0..64u64, any::<u32>(), any::<u64>()).prop_flat_map(
            |(blocks, tail, chain, seq)| {
                let most = usize::try_from(blocks * 4096).expect("a small size") - 9;
                (4..=most).prop_map(move |body_max| {
                    header((2 * blocks - 1) * 4096, body_max, tail * 4096, chain, seq)
                })
            },
        )
    }

    /// The CRC comes from another CRC32C implementation.
    #[test]
    fn lays_out_the_fields_little_endian_with_the_crc_after_them() {
        let block = header(8 * 4096, 3 * 4096, 2 * 4096, 0x0102_0304, 7).encode();
        let mut expected = b"FNDNRING".to_vec();
        expected.extend([1, 0]);
        expected.extend((8 * 4096u64).to_le_bytes());
        expected.extend((3 * 4096u32).to_le_bytes());
        expected.extend((2 * 4096u64).to_le_bytes());
        expected.extend(0x0102_0304u32.to_le_bytes());
        expected.extend(7u64.to_le_bytes());
        expected.extend([0xBF, 0xC6, 0xC7, 0x09]);
        assert_eq!(&block[..CRC], &expected[..]);
        assert!(block[CRC..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn puts_even_checkpoints_in_the_first_block_and_odd_in_the_second() {
        let places: Vec<u64> = (0..4)
            .map(|seq| header(8 * 4096, 4087, 0, 1, seq).place())
            .collect();
        assert_eq!(places, [0, 4096, 0, 4096]);
        assert_eq!(LEN, 2 * 4096);
    }

    #[test]
    fn reads_no_header_from_zeroed_blocks() {
        assert_eq!(
            Header::decode(&[0; ALIGN], &[0; ALIGN]),
            Err(Error::Missing)
        );
    }

    #[test]
    fn reads_no_header_from_a_wrong_magic_with_a_right_crc() {
        let mut block = header(8 * 4096, 4087, 0, 1, 0).encode();
        block[..8].copy_from_slice(b"SYNXRING");
        let mut crc = Crc32c::new();
        crc.update(&block[..FIELDS]);
        block[FIELDS..CRC].copy_from_slice(&crc.finish().to_le_bytes());
        assert_eq!(Header::decode(&block, &[0; ALIGN]), Err(Error::Missing));
    }

    #[test]
    fn refuses_a_version_it_does_not_read() {
        let mut block = header(8 * 4096, 4087, 0, 1, 3).encode();
        block[8..10].copy_from_slice(&2u16.to_le_bytes());
        let mut crc = Crc32c::new();
        crc.update(&block[..FIELDS]);
        block[FIELDS..CRC].copy_from_slice(&crc.finish().to_le_bytes());
        let older = header(8 * 4096, 4087, 0, 1, 2).encode();
        assert_eq!(Header::decode(&block, &older), Err(Error::Version(2)));
    }

    #[test]
    fn refuses_sizes_that_do_not_make_a_ring_and_an_unaligned_tail() {
        let unfit = |area, body_max| Error::Unfit(Unfit { area, body_max });
        let cases: [(&str, usize, &[u8], Error); 3] = [
            (
                "a body of 3 bytes",
                18,
                &3u32.to_le_bytes(),
                unfit(8 * 4096, 3),
            ),
            (
                "an area of a part block",
                10,
                &(8 * 4096u64 + 1).to_le_bytes(),
                unfit(8 * 4096 + 1, 4087),
            ),
            (
                "a tail off a boundary",
                22,
                &4097u64.to_le_bytes(),
                Error::Unaligned(Unaligned { offset: 4097 }),
            ),
        ];
        for (case, at, bytes, expected) in cases {
            let mut block = header(8 * 4096, 4087, 0, 1, 0).encode();
            block[at..at + bytes.len()].copy_from_slice(bytes);
            let mut crc = Crc32c::new();
            crc.update(&block[..FIELDS]);
            block[FIELDS..CRC].copy_from_slice(&crc.finish().to_le_bytes());
            assert_eq!(Header::decode(&block, &block), Err(expected), "{case}");
        }
    }

    proptest! {
        #[test]
        fn reads_the_newer_valid_block(
            newer in any_header(),
            older in any_header(),
            swap in any::<bool>(),
        ) {
            prop_assume!(newer.seq != older.seq);
            let (newer, older) = if newer.seq > older.seq {
                (newer, older)
            } else {
                (older, newer)
            };
            let (a, b) = (newer.encode(), older.encode());
            let (first, second) = if swap { (&b, &a) } else { (&a, &b) };
            prop_assert_eq!(Header::decode(first, second), Ok(newer));
        }

        #[test]
        fn reads_the_whole_block_when_the_other_is_zeroed_or_damaged(
            header in any_header(),
            other in any_header(),
            at in any::<prop::sample::Index>(),
            bit in 0..8u32,
            swap in any::<bool>(),
        ) {
            let whole = header.encode();
            let mut damaged = other.encode();
            damaged[at.index(CRC)] ^= 1 << bit;
            for other in [[0; ALIGN], damaged] {
                let (first, second) = if swap { (&other, &whole) } else { (&whole, &other) };
                prop_assert_eq!(Header::decode(first, second), Ok(header));
            }
            prop_assert_eq!(Header::decode(&damaged, &damaged), Err(Error::Missing));
        }

        #[test]
        fn ignores_the_padding(header in any_header(), fill in any::<u8>()) {
            let mut block = header.encode();
            block[CRC..].fill(fill);
            prop_assert_eq!(Header::decode(&block, &[0; ALIGN]), Ok(header));
        }
    }
}
