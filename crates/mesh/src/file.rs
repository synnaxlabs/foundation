//! What the files of a mesh directory share: the check of their bytes, and the read
//! and write of a file in blocks of a pool.

use std::rc::Rc;

use block::Pool;
use env::files::{self, File};
use types::digest::Digest;

use crate::bytes::block;

/// The bytes of a check.
pub(crate) const CHECK: usize = 8;
/// The most bytes in one block of a read or a write.
pub(crate) const CHUNK: usize = 64 << 10;

/// A failed file call or a pool with no block.
#[derive(Debug)]
pub(crate) enum Failed {
    Files(files::Error),
    Pool(block::Error),
}

/// The check of `bytes`: the first bytes of their digest.
pub(crate) fn check(bytes: &[u8]) -> [u8; CHECK] {
    let digest = Digest::of(bytes).0;
    *digest
        .first_chunk()
        .expect("invariant: a digest has 32 bytes")
}

/// A pool whose largest block is one sector or more, and the reads and writes of a
/// file in its blocks.
#[derive(Debug)]
pub(crate) struct Blocks {
    pool: Rc<Pool>,
    // The most bytes in one block: [`CHUNK`], or the most whole sectors in the
    // largest block of `pool`.
    chunk: usize,
}

impl Blocks {
    /// Reads and writes in blocks of `pool`.
    ///
    /// # Errors
    ///
    /// [`block::Error::TooLarge`] when the largest block of `pool` is under one
    /// sector.
    pub(crate) fn new(pool: Rc<Pool>) -> Result<Self, block::Error> {
        let largest = pool.largest();
        if largest < files::SECTOR {
            let requested = files::SECTOR;
            return Err(block::Error::TooLarge { requested, largest });
        }
        let chunk = largest.saturating_sub(largest % files::SECTOR).min(CHUNK);
        Ok(Self { pool, chunk })
    }

    /// The bytes of `file`, read one block at a time.
    pub(crate) async fn read(&self, file: &File) -> Result<Vec<u8>, Failed> {
        let mut bytes = Vec::new();
        while wide(bytes.len()) < file.len() {
            let offset = wide(bytes.len());
            let len = narrow(file.len().saturating_sub(offset)).min(self.chunk);
            let block = self.pool.alloc(len).map_err(Failed::Pool)?;
            let block = file.read_at(offset, block).await.map_err(Failed::Files)?;
            bytes.extend_from_slice(&block);
        }
        Ok(bytes)
    }

    /// Writes `bytes` at `at` of `file`, one block at a time, from the end of `bytes`
    /// to its start, and takes each block that it wrote off the end of `bytes`. Each
    /// block but the one at the end ends at a multiple of the block size in the
    /// file, so no two blocks share a sector.
    pub(crate) async fn write(
        &self,
        file: &File,
        at: u64,
        bytes: &mut &[u8],
    ) -> Result<(), Failed> {
        while !bytes.is_empty() {
            let end = narrow(at).saturating_add(bytes.len());
            let over = end
                .checked_rem(self.chunk)
                .expect("invariant: a block is one sector or more");
            let len = if over == 0 { self.chunk } else { over }.min(bytes.len());
            let (rest, part) = bytes.split_at(bytes.len().saturating_sub(len));
            let block = block(&self.pool, part).map_err(Failed::Pool)?;
            let offset = at.saturating_add(wide(rest.len()));
            file.write_at(offset, &[block])
                .await
                .map_err(Failed::Files)?;
            *bytes = rest;
        }
        Ok(())
    }
}

/// `len` as a file offset.
pub(crate) fn wide(len: usize) -> u64 {
    u64::try_from(len).expect("invariant: a length fits in 64 bits")
}

/// A file offset as a length. A file is read whole into memory, so each offset in it
/// fits.
pub(crate) fn narrow(offset: u64) -> usize {
    usize::try_from(offset).expect("invariant: a file offset fits in memory")
}
