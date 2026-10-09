//! What the files of a mesh directory share: the check of their bytes, and the read
//! of a whole file.

use block::Pool;
use env::files::{self, File};
use types::digest::Digest;

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

/// The most bytes in one block of a read or a write: [`CHUNK`], or less when the pool
/// has no such block. It is whole sectors, so 0 when the largest block of `pool` is
/// under one sector.
pub(crate) fn chunk(pool: &Pool) -> usize {
    let largest = pool.largest();
    largest.saturating_sub(largest % files::SECTOR).min(CHUNK)
}

/// The bytes of `file`, read one block of `pool` at a time. The largest block of
/// `pool` is one sector or more.
pub(crate) async fn read(file: &File, pool: &Pool) -> Result<Vec<u8>, Failed> {
    let chunk = chunk(pool);
    let mut bytes = Vec::new();
    while wide(bytes.len()) < file.len() {
        let offset = wide(bytes.len());
        let len = narrow(file.len().saturating_sub(offset)).min(chunk);
        let block = pool.alloc(len).map_err(Failed::Pool)?;
        let block = file.read_at(offset, block).await.map_err(Failed::Files)?;
        bytes.extend_from_slice(&block);
    }
    Ok(bytes)
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
