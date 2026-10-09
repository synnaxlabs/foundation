//! A file of the data directory that fits one sector, which a crash keeps whole or
//! old, so a write never tears it. Its last 4 bytes are the CRC32C of the bytes
//! before them (little-endian).

use std::path::Path;

use env::files::{File, Files, Mode};

/// The pool of the file's block. A call can come while the shard's buffer holds the
/// blocks of the shard's pool, so that pool can lack room for it.
const POOL: block::Config = block::Config { budget: 4096 };

/// Opens `path` in `files`, made of `N` zero bytes when it is not there or has no
/// bytes, and reads it.
///
/// # Errors
///
/// [`env::files::Error::Length`] for a file of another length that is not 0, and
/// the error of each other file call that fails.
pub(crate) async fn open<const N: usize>(
    files: &Files,
    path: &Path,
) -> Result<(File, [u8; N]), env::files::Error> {
    let file = files.open(path, Mode::Create { len: N as u64 }).await?;
    let bytes = read(&file).await?;
    Ok((file, bytes))
}

/// The bytes of `file`, which has `N` bytes.
///
/// # Errors
///
/// The error of the read.
pub(crate) async fn read<const N: usize>(
    file: &File,
) -> Result<[u8; N], env::files::Error> {
    const { assert!(N <= env::files::SECTOR, "the file fits one sector") };
    let pool = block::Pool::heap(POOL);
    let into = pool.alloc(N).expect("invariant: the pool holds a sector");
    let read = file.read_at(0, into).await?;
    Ok((&*read).try_into().expect("invariant: a read fills it"))
}

/// Writes `bytes` to `file` of `files`, then makes the file and its name durable.
///
/// # Errors
///
/// The error of the first file call that fails.
pub(crate) async fn write<const N: usize>(
    files: &Files,
    file: &File,
    bytes: &[u8; N],
) -> Result<(), env::files::Error> {
    const { assert!(N <= env::files::SECTOR, "the file fits one sector") };
    let pool = block::Pool::heap(POOL);
    let block = pool
        .copy(bytes)
        .expect("invariant: the pool holds a sector");
    file.write_at(0, &[block]).await?;
    file.sync().await?;
    files.sync_dir(Path::new("")).await
}

/// Writes the CRC32C of the bytes of `bytes` before its last 4 into those 4.
pub(crate) fn seal(bytes: &mut [u8]) {
    let (body, crc) = bytes.split_at_mut(bytes.len() - 4);
    crc.copy_from_slice(&crc32c::crc32c(body).to_le_bytes());
}

/// Whether the last 4 bytes of `bytes` are the CRC32C of the bytes before them.
pub(crate) fn sealed(bytes: &[u8]) -> bool {
    let (body, crc) = bytes.split_at(bytes.len() - 4);
    crc32c::crc32c(body).to_le_bytes() == *crc
}
