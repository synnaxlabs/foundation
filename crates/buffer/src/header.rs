//! The ring header: what the ring knows about itself between opens. Two blocks at
//! the start of the ring hold the last two checkpoints, so a torn write of one
//! leaves the other whole.
//!
//! ```text
//! [magic: 8][version: u16][area: u64][body_max: u32][tail offset: u64]
//! [tail chain: u32][seq: u64][crc32c: u32][zero padding]
//! ```
//!
//! Integers are little-endian. The CRC is at offset 42 and covers the rest of the
//! first 512-byte sector, so a checkpoint is in one sector, which a crash keeps
//! whole or old. A later version may put fields after the CRC in that sector and
//! this version still reads the block and reports its version. Nothing reads the
//! bytes past the sector.

#![deny(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::as_conversions
)]

use crate::crc32c;
use crate::record::{ALIGN, BLOCK};
use crate::wal::{Layout, Position, Unaligned, Unfit};

const MAGIC: [u8; 8] = *b"FNDNRING";
const VERSION: u16 = 1;
/// The place of the CRC: right after the fields.
const CRC: usize = 8 + 2 + 8 + 4 + 8 + 4 + 8;
/// A disk sector, which a crash keeps whole or old. The CRC covers the first one.
const SECTOR: usize = 512;

/// Why neither header block can be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// No block has the magic: the file is not a ring.
    Missing,
    /// Both blocks have the magic and a wrong CRC, which no crash leaves: the ring
    /// is lost.
    Damaged,
    /// A whole block has a format version this build does not read.
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

/// One checkpoint of the ring: its sizes and the boundary before its oldest live
/// record. [`new`](Self::new) makes the first; [`next`](Self::next) each later one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) layout: Layout,
    pub(crate) tail: Position,
    seq: u64,
}

impl Header {
    /// The first checkpoint of a new ring, with the tail at offset 0 and `chain` as
    /// its random chain value. Write its block to both header places.
    pub(crate) fn new(layout: Layout, chain: u32) -> Self {
        let tail = Position::new(0, chain).unwrap_or_else(|unaligned| {
            unreachable!("invariant: offset 0 is aligned: {unaligned:?}")
        });
        Self {
            layout,
            tail,
            seq: 0,
        }
    }

    /// The checkpoint after this one, with the tail moved to `tail`.
    #[cfg_attr(not(test), expect(dead_code, reason = "trimming moves the tail"))]
    pub(crate) fn next(self, tail: Position) -> Self {
        Self {
            tail,
            seq: self.seq.wrapping_add(1),
            ..self
        }
    }

    /// The byte offset in the ring file of the block that holds this checkpoint:
    /// the two checkpoints alternate between the first two blocks.
    #[cfg_attr(not(test), expect(dead_code, reason = "trimming moves the tail"))]
    pub(crate) fn place(&self) -> u64 {
        if self.seq.is_multiple_of(2) { 0 } else { BLOCK }
    }

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
        let crc = crc(&block).to_le_bytes();
        let (_, rest) = block.split_at_mut(CRC);
        let (slot, _) = rest
            .split_first_chunk_mut::<4>()
            .expect("invariant: the CRC is in the block");
        *slot = crc;
        block
    }

    /// Reads the newer whole one of the two header blocks: the one whose seq comes
    /// after the other's, wrapped as [`Self::next`] wraps it. A new ring has the
    /// same block in both places; on a tie, the first.
    ///
    /// # Errors
    ///
    /// [`Error`] when neither block can be used.
    pub(crate) fn decode(
        first: &[u8; ALIGN],
        second: &[u8; ALIGN],
    ) -> Result<Self, Error> {
        let mut newer = None;
        let mut magic = false;
        for block in [first, second] {
            magic |= block.starts_with(&MAGIC);
            let Some(fields) = Fields::read(block) else {
                continue;
            };
            if fields.version != VERSION {
                return Err(Error::Version(fields.version));
            }
            if newer.is_none_or(|newer: Fields| later(fields.seq, newer.seq)) {
                newer = Some(fields);
            }
        }
        let lost = if magic {
            Error::Damaged
        } else {
            Error::Missing
        };
        let fields = newer.ok_or(lost)?;
        let body_max = usize::try_from(fields.body_max).unwrap_or(usize::MAX);
        Ok(Self {
            layout: Layout::new(fields.area, body_max)?,
            tail: Position::new(fields.offset, fields.chain)?,
            seq: fields.seq,
        })
    }
}

/// Whether checkpoint `seq` comes after `than`. The two blocks hold consecutive
/// checkpoints, so the sign of the wrapped difference orders them.
fn later(seq: u64, than: u64) -> bool {
    seq.wrapping_sub(than).cast_signed() > 0
}

/// The CRC of the first sector of `block`, less the four bytes that hold it.
fn crc(block: &[u8; ALIGN]) -> u32 {
    let (sector, _) = block.split_at(SECTOR);
    let (fields, rest) = sector.split_at(CRC);
    let (_, after) = rest.split_at(4);
    crc32c::append(crc32c::append(0, fields), after)
}

/// The fields of one block whose magic and CRC are right.
#[derive(Clone, Copy)]
struct Fields {
    version: u16,
    area: u64,
    body_max: u32,
    offset: u64,
    chain: u32,
    seq: u64,
}

impl Fields {
    fn read(block: &[u8; ALIGN]) -> Option<Self> {
        let (fields, rest) = block.split_at(CRC);
        let (stored, _) = rest.split_first_chunk::<4>()?;
        if crc(block) != u32::from_le_bytes(*stored) {
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

    use crate::entry::table_len;
    use crate::record::AREA_START;

    fn layout(area: u64, body_max: usize) -> Layout {
        Layout::new(area, body_max).expect("the test sizes make a ring")
    }

    /// Checkpoint `seq` of a ring, with its tail at `offset`.
    fn header(area: u64, body_max: usize, offset: u64, chain: u32, seq: u64) -> Header {
        Header {
            layout: layout(area, body_max),
            tail: Position::new(offset, chain).expect("the test tail is aligned"),
            seq,
        }
    }

    /// Bytes to put into a block at offsets.
    type Patch<'a> = &'a [(usize, &'a [u8])];

    /// Puts the CRC of the block after its fields.
    fn seal(block: &mut [u8; ALIGN]) {
        let crc = crc(block).to_le_bytes();
        block[CRC..CRC + 4].copy_from_slice(&crc);
    }

    fn any_header() -> impl Strategy<Value = Header> {
        (1..64u64, 0..64u64, any::<u32>(), any::<u64>()).prop_flat_map(
            |(blocks, tail, chain, seq)| {
                let most = usize::try_from(blocks * 4096).expect("a small size") - 9;
                (table_len(1)..=most).prop_map(move |body_max| {
                    header((2 * blocks - 1) * 4096, body_max, tail * 4096, chain, seq)
                })
            },
        )
    }

    /// The CRC comes from another CRC32C implementation, over the first sector
    /// less the CRC itself.
    #[test]
    fn lays_out_the_fields_little_endian_then_the_crc() {
        let block = header(8 * 4096, 3 * 4096, 2 * 4096, 0x0102_0304, 7).encode();
        let mut expected = b"FNDNRING".to_vec();
        expected.extend([1, 0]);
        expected.extend((8 * 4096u64).to_le_bytes());
        expected.extend((3 * 4096u32).to_le_bytes());
        expected.extend((2 * 4096u64).to_le_bytes());
        expected.extend(0x0102_0304u32.to_le_bytes());
        expected.extend(7u64.to_le_bytes());
        assert_eq!(&block[..CRC], &expected[..]);
        assert_eq!(CRC, 42);
        assert_eq!(block[CRC..CRC + 4], [0xAF, 0xFD, 0x89, 0xEE]);
        assert!(block[CRC + 4..].iter().all(|byte| *byte == 0));
    }

    /// A crash keeps a sector whole or old, so a checkpoint in one sector is never
    /// torn. Bytes past the first sector are not read.
    #[test]
    fn checks_only_the_first_sector() {
        let whole = header(8 * 4096, 4087, 0, 1, 0);
        let mut block = whole.encode();
        block[SECTOR] = 1;
        block[ALIGN - 1] = 1;
        assert_eq!(Header::decode(&block, &[0; ALIGN]), Ok(whole));
        block[SECTOR - 1] = 1;
        assert_eq!(Header::decode(&block, &[0; ALIGN]), Err(Error::Damaged));
    }

    #[test]
    fn starts_at_offset_zero_and_alternates_the_two_blocks() {
        let first = Header::new(layout(8 * 4096, 4087), 9);
        assert_eq!(first, header(8 * 4096, 4087, 0, 9, 0));
        let second = first.next(Position::new(4096, 3).expect("aligned"));
        assert_eq!(second, header(8 * 4096, 4087, 4096, 3, 1));
        let third = second.next(Position::new(8192, 4).expect("aligned"));
        let places: Vec<u64> =
            [first, second, third].iter().map(Header::place).collect();
        assert_eq!(places, [0, 4096, 0]);
        assert!(places.iter().all(|place| place + BLOCK <= AREA_START));
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
        seal(&mut block);
        assert_eq!(Header::decode(&block, &[0; ALIGN]), Err(Error::Missing));
    }

    #[test]
    fn tells_a_damaged_ring_from_no_ring() {
        let mut block = header(8 * 4096, 4087, 0, 1, 0).encode();
        block[CRC] ^= 1;
        assert_eq!(Header::decode(&block, &[0; ALIGN]), Err(Error::Damaged));
        assert_eq!(Header::decode(&[0; ALIGN], &block), Err(Error::Damaged));
    }

    #[test]
    fn takes_the_first_block_on_a_tie() {
        let a = header(8 * 4096, 4087, 0, 1, 4);
        let b = header(8 * 4096, 4087, 4096, 2, 4);
        assert_eq!(Header::decode(&a.encode(), &b.encode()), Ok(a));
        assert_eq!(Header::decode(&b.encode(), &a.encode()), Ok(b));
    }

    /// A later version may put fields after the CRC in the first sector; this
    /// version must still see that the block is whole and name the version, even
    /// when the other block is older and readable.
    #[test]
    fn refuses_a_version_it_does_not_read() {
        let mut block = header(8 * 4096, 4087, 0, 1, 3).encode();
        block[8..10].copy_from_slice(&2u16.to_le_bytes());
        block[CRC + 4..CRC + 12].copy_from_slice(&u64::MAX.to_le_bytes());
        seal(&mut block);
        let older = header(8 * 4096, 4087, 0, 1, 2).encode();
        assert_eq!(Header::decode(&block, &older), Err(Error::Version(2)));
        assert_eq!(Header::decode(&older, &block), Err(Error::Version(2)));
    }

    /// Sizes come before the tail: a ring with bad sizes has no tail to check.
    #[test]
    fn refuses_sizes_that_do_not_make_a_ring_and_an_unaligned_tail() {
        let unfit = |area, body_max| Error::Unfit(Unfit { area, body_max });
        let unaligned = Error::Unaligned(Unaligned { offset: 4097 });
        let part = (8 * 4096u64 + 1).to_le_bytes();
        let off = 4097u64.to_le_bytes();
        let small = u32::try_from(table_len(1) - 1).expect("a small size");
        let cases: [(&str, Patch<'_>, Error); 4] = [
            (
                "a body under one entry table",
                &[(18, &small.to_le_bytes())],
                unfit(8 * 4096, table_len(1) - 1),
            ),
            (
                "an area of a part block",
                &[(10, &part)],
                unfit(8 * 4096 + 1, 4087),
            ),
            ("a tail off a boundary", &[(22, &off)], unaligned),
            (
                "both",
                &[(10, &part), (22, &off)],
                unfit(8 * 4096 + 1, 4087),
            ),
        ];
        for (case, fields, expected) in cases {
            let mut block = header(8 * 4096, 4087, 0, 1, 0).encode();
            for (at, bytes) in fields {
                block[*at..at + bytes.len()].copy_from_slice(bytes);
            }
            seal(&mut block);
            assert_eq!(Header::decode(&block, &block), Err(expected), "{case}");
        }
    }

    #[test]
    fn reads_the_next_checkpoint_from_either_block() {
        let old = header(8 * 4096, 4087, 0, 1, 4);
        let new = old.next(Position::new(4096, 2).expect("aligned"));
        assert_eq!((old.place(), new.place()), (0, 4096));
        assert_eq!(Header::decode(&old.encode(), &new.encode()), Ok(new));
        assert_eq!(Header::decode(&new.encode(), &old.encode()), Ok(new));
    }

    #[test]
    fn reads_the_checkpoint_after_the_last_seq() {
        let old = header(8 * 4096, 4087, 4096, 3, u64::MAX);
        let new = old.next(Position::new(8192, 4).expect("aligned"));
        assert_eq!((old.place(), new.place()), (4096, 0));
        assert_eq!(Header::decode(&old.encode(), &new.encode()), Ok(new));
        assert_eq!(Header::decode(&new.encode(), &old.encode()), Ok(new));
    }

    proptest! {
        /// The newer block is from 1 to `2^63 - 1` checkpoints after the older,
        /// and most often the next one.
        #[test]
        fn reads_the_newer_valid_block(
            older in any_header(),
            tail in 0..64u64,
            apart in prop_oneof![1..4u64, 1..(1u64 << 63)],
            swap in any::<bool>(),
        ) {
            let newer = Header {
                tail: Position::new(tail * 4096, older.tail.chain())
                    .expect("aligned"),
                seq: older.seq.wrapping_add(apart),
                ..older
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
            damaged[at.index(SECTOR)] ^= 1 << bit;
            for other in [[0; ALIGN], damaged] {
                let (first, second) =
                    if swap { (&other, &whole) } else { (&whole, &other) };
                prop_assert_eq!(Header::decode(first, second), Ok(header));
            }
            let lost = if damaged.starts_with(&MAGIC) {
                Error::Damaged
            } else {
                Error::Missing
            };
            prop_assert_eq!(Header::decode(&damaged, &damaged), Err(lost));
        }
    }
}
