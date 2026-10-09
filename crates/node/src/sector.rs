//! A file of the data directory that fits one sector, which a crash keeps whole or
//! old, so a write never tears it. It starts with a tag, and its last 4 bytes are the
//! CRC32C of the bytes before them (little-endian).

use std::path::Path;

use env::files::{File, Files, Mode};

/// The pool of the file's block. A call can come while the shard's buffer holds the
/// blocks of the shard's pool, so that pool can lack room for it.
const POOL: block::Config = block::Config { budget: 4096 };

/// What a file of `N` bytes holds.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Held<const N: usize> {
    /// No file, no bytes, or `N` zero bytes: a crash kept a node from writing it.
    Nothing,
    /// The bytes that a node wrote, with the tag and a checksum that matches.
    Written([u8; N]),
    /// A file that no node wrote: another tag or checksum, or, for [`read`], another
    /// length.
    Foreign,
}

/// Opens `path` in `files` to write, made of `N` zero bytes when it is not there or
/// has no bytes, and reads what it holds. Call it under the lock of the data
/// directory.
///
/// # Errors
///
/// The error of each file call that fails: [`env::files::Error::Length`] for a file
/// of another length that is not 0.
pub(crate) async fn open<const N: usize>(
    files: &Files,
    path: &Path,
    tag: &[u8],
) -> Result<(File, Held<N>), env::files::Error> {
    let file = files.open(path, Mode::Create { len: N as u64 }).await?;
    let held = held(&bytes(&file).await?, tag);
    Ok((file, held))
}

/// What `path` in `files` holds. Opens it to read only, so it makes nothing and
/// waits for no lock.
///
/// # Errors
///
/// The error of each file call that fails, except [`env::files::Error::NotFound`],
/// which gives [`Held::Nothing`].
pub(crate) async fn read<const N: usize>(
    files: &Files,
    path: &Path,
    tag: &[u8],
) -> Result<Held<N>, env::files::Error> {
    let file = match files.open(path, Mode::Read).await {
        Ok(file) => file,
        Err(env::files::Error::NotFound { .. }) => return Ok(Held::Nothing),
        Err(error) => return Err(error),
    };
    match file.len() {
        0 => Ok(Held::Nothing),
        len if len == N as u64 => Ok(held(&bytes(&file).await?, tag)),
        _ => Ok(Held::Foreign),
    }
}

/// Writes `bytes`, which [`checksum`] completed, to `file` of `files`, which
/// [`open`] gave, and makes the file and its name durable.
///
/// # Errors
///
/// The error of the first file call that fails.
pub(crate) async fn write<const N: usize>(
    files: &Files,
    file: &File,
    bytes: &[u8; N],
) -> Result<(), env::files::Error> {
    let pool = block::Pool::heap(POOL);
    let block = pool
        .copy(bytes)
        .expect("invariant: the pool holds a sector");
    file.write_at(0, &[block]).await?;
    file.sync().await?;
    files.sync_dir(Path::new("")).await
}

/// What `bytes` hold, for a file whose tag is `tag`.
pub(crate) fn held<const N: usize>(bytes: &[u8; N], tag: &[u8]) -> Held<N> {
    let (body, sum) = bytes.split_at(N - 4);
    if *bytes == [0; N] {
        Held::Nothing
    } else if body.starts_with(tag) && crc32c::crc32c(body).to_le_bytes() == *sum {
        Held::Written(*bytes)
    } else {
        Held::Foreign
    }
}

/// Writes the CRC32C of the bytes of `bytes` before its last 4 into those 4.
pub(crate) fn checksum<const N: usize>(bytes: &mut [u8; N]) {
    let (body, sum) = bytes.split_at_mut(N - 4);
    sum.copy_from_slice(&crc32c::crc32c(body).to_le_bytes());
}

/// The `N` bytes of `file`, which has `N` bytes.
async fn bytes<const N: usize>(file: &File) -> Result<[u8; N], env::files::Error> {
    const { assert!(N <= env::files::SECTOR, "the file fits one sector") };
    let pool = block::Pool::heap(POOL);
    let into = pool.alloc(N).expect("invariant: the pool holds a sector");
    let read = file.read_at(0, into).await?;
    Ok((&*read).try_into().expect("invariant: a read fills it"))
}
