//! The node's budgets, in the file `budget` of the data directory.
//!
//! The file is 39 bytes: the tag, the pool budget and the disk budget as `u64`
//! (little-endian), and the CRC32C of those 35 bytes (little-endian), in one sector
//! ([`crate::sector`]).

use std::path::Path;

use env::files::Files;
use types::byte::Size;

use crate::Error;
use crate::sector::{self, Held};

/// The name of the file.
pub(crate) const FILE: &str = "budget";
/// The first bytes of the file; a new form gets a new tag.
const TAG: &[u8; 19] = b"foundation/budget/1";
/// The length of the file: the tag, two `u64`, and the CRC32C.
pub(crate) const LEN: usize = TAG.len() + 16 + 4;

/// The pool budget and the disk budget of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// The most bytes the node's pools may commit, split evenly across its shards.
    /// Each shard's part must hold the largest block its buffer reads, else
    /// [`crate::Node::join`] gives [`Error::Buffer`].
    pub pool: Size,
    /// The disk budget of the node's rings, split evenly across its shards; shard 0
    /// also takes the remainder. Each ring that the start makes fits its part. A ring
    /// with a checkpoint keeps its size, which can be more than its part.
    pub disk: Size,
}

/// The budgets that the data directory `files` keeps, or `None` when it keeps none.
/// Writes nothing, and reads also while another node runs on `files`.
///
/// # Errors
///
/// [`Error::Budget`] for a file `budget` that a node did not write, and
/// [`Error::Directory`] for a file call that fails.
pub async fn budget(files: &Files) -> Result<Option<Budget>, Error> {
    let held = sector::read(files, Path::new(FILE), TAG).await;
    match held.map_err(Error::Directory)? {
        Held::Nothing => Ok(None),
        Held::Written(bytes) => Ok(Some(decode(&bytes))),
        Held::Foreign => Err(Error::Budget),
    }
}

/// Writes `budget` to the file `budget` of `files` and makes it durable, when the
/// file is not there. A file that is there stays as it is. Call it under the lock of
/// the data directory, after the sync of the directory that the claim makes.
///
/// # Errors
///
/// [`Error::Budget`] for a file that a node did not write, and [`Error::Directory`]
/// for a file call that fails.
pub(crate) async fn keep(files: &Files, budget: Budget) -> Result<(), Error> {
    match self::budget(files).await? {
        None => sector::publish(files, Path::new(FILE), &encode(budget))
            .await
            .map_err(Error::Directory),
        Some(_) => Ok(()),
    }
}

/// The bytes of the file that holds `budget`.
fn encode(budget: Budget) -> [u8; LEN] {
    let mut bytes = [0; LEN];
    bytes[..TAG.len()].copy_from_slice(TAG);
    bytes[TAG.len()..][..8].copy_from_slice(&budget.pool.bytes().to_le_bytes());
    bytes[TAG.len() + 8..][..8].copy_from_slice(&budget.disk.bytes().to_le_bytes());
    sector::checksum(&mut bytes);
    bytes
}

/// The budgets in `bytes`, which hold the tag and a checksum that matches.
fn decode(bytes: &[u8; LEN]) -> Budget {
    let number = |at: usize| {
        let le = bytes[at..][..8].try_into().expect("invariant: 8 bytes");
        Size::from_bytes(u64::from_le_bytes(le))
    };
    Budget {
        pool: number(TAG.len()),
        disk: number(TAG.len() + 8),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn writes_the_tag_the_budgets_and_the_checksum() {
        let bytes = encode(Budget {
            pool: Size::from_bytes(0x0102),
            disk: Size::from_bytes(u64::MAX - 1),
        });
        assert_eq!(&bytes[..19], b"foundation/budget/1");
        assert_eq!(bytes[19..27], [2, 1, 0, 0, 0, 0, 0, 0], "little-endian");
        assert_eq!(
            bytes[27..35],
            [0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
        );
        assert_eq!(
            bytes[35..],
            crc32c::crc32c(&bytes[..35]).to_le_bytes(),
            "little-endian"
        );
    }

    proptest! {
        #[test]
        fn a_budget_reads_back(pool: u64, disk: u64) {
            let budget = Budget {
                pool: Size::from_bytes(pool),
                disk: Size::from_bytes(disk),
            };
            let bytes = encode(budget);
            prop_assert_eq!(sector::held(&bytes, TAG), Held::Written(bytes));
            prop_assert_eq!(decode(&bytes), budget);
        }

        /// Each byte that changes in the file of a budget makes the file one that a
        /// node did not write.
        #[test]
        fn a_changed_byte_is_not_a_budget(
            pool: u64,
            disk: u64,
            at in 0..LEN,
            flip in 1..=u8::MAX,
        ) {
            let mut bytes = encode(Budget {
                pool: Size::from_bytes(pool),
                disk: Size::from_bytes(disk),
            });
            bytes[at] ^= flip;
            prop_assert_eq!(sector::held(&bytes, TAG), Held::Foreign);
        }
    }
}
