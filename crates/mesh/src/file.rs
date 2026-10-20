//! What the files of a mesh directory share: the check and the frame of their bytes,
//! and the read and write of a file in blocks of a pool.

use std::path::Path;
use std::rc::Rc;

use block::Pool;
use env::files::{self, File, Files, Mode};
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

/// `body` as a file of a mesh directory holds it: the check of the rest, `format`,
/// then `body`.
pub(crate) fn frame(format: u16, body: &[u8]) -> Vec<u8> {
    let mut rest = format.to_le_bytes().to_vec();
    rest.extend(body);
    let mut bytes = check(&rest).to_vec();
    bytes.extend(rest);
    bytes
}

/// The format and the body of `bytes`, as [`frame`] gives them, or `None` when
/// `bytes` is shorter than a frame or its check fails.
pub(crate) fn unframe(bytes: &[u8]) -> Option<(u16, &[u8])> {
    let (claimed, rest) = bytes.split_first_chunk::<CHECK>()?;
    let (format, body) = rest.split_first_chunk::<2>()?;
    (*claimed == check(rest)).then(|| (u16::from_le_bytes(*format), body))
}

/// A pool whose largest block is one sector or more, and the reads and writes of a
/// file in its blocks.
#[derive(Clone, Debug)]
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
            let block = self.pool.copy(part).map_err(Failed::Pool)?;
            let offset = at.saturating_add(wide(rest.len()));
            file.write_at(offset, &[block])
                .await
                .map_err(Failed::Files)?;
            *bytes = rest;
        }
        Ok(())
    }
}

/// Writes `body` in its frame of `format` as the file `name` of `dir`, in place of the
/// file there. It removes `name` and `new`, writes `new`, and renames it to `name`,
/// which makes its bytes durable. So a crash leaves the old file, the new one, or
/// none. The caller syncs `dir`.
pub(crate) async fn replace(
    files: &Files,
    dir: &Path,
    [name, new]: [&str; 2],
    blocks: &Blocks,
    format: u16,
    body: &[u8],
) -> Result<(), Failed> {
    let bytes = frame(format, body);
    let (path, new) = (dir.join(name), dir.join(new));
    // `File::rename` refuses an existing target, so the old file goes first.
    files.remove(&path).await.map_err(Failed::Files)?;
    files.remove(&new).await.map_err(Failed::Files)?;
    let len = wide(bytes.len());
    let mut opened = files
        .open(&new, Mode::Create { len })
        .await
        .map_err(Failed::Files)?;
    blocks.write(&opened, 0, &mut &bytes[..]).await?;
    opened.rename(&path).await.map_err(Failed::Files)?;
    opened.close().await;
    Ok(())
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

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn a_frame_round_trips(
            format: u16,
            body in prop::collection::vec(any::<u8>(), 0..64),
        ) {
            let bytes = frame(format, &body);
            prop_assert_eq!(unframe(&bytes), Some((format, &body[..])));
        }

        // A changed byte, and each prefix shorter than the check and the format, give
        // no frame.
        #[test]
        fn a_changed_or_short_frame_does_not_unframe(
            format: u16,
            body in prop::collection::vec(any::<u8>(), 0..64),
            at in any::<prop::sample::Index>(),
        ) {
            let mut bytes = frame(format, &body);
            for len in 0..CHECK + 2 {
                prop_assert_eq!(unframe(&bytes[..len]), None);
            }
            let at = at.index(bytes.len());
            if let Some(byte) = bytes.get_mut(at) {
                *byte ^= 1;
            }
            prop_assert_eq!(unframe(&bytes), None);
        }
    }
}
