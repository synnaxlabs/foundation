//! The file `founding` of a mesh directory: the region before the first entry of the
//! log. The first open writes it, and each later open checks `Config::founding`
//! against it.
//!
//! The file holds an 8-byte check of the rest, the format version, then what
//! `region::Founding::encode` gives. The first open writes `founding.new`, renames it
//! to `founding`, and syncs the directory before raft writes a record. So a crash
//! leaves `founding` whole or absent, and a log with no record and no `founding` is a
//! first open.

use std::path::Path;

use block::Pool;
use env::files::{self, Files, Mode};
use types::digest::Digest;

use crate::bytes::block;
use crate::error::Error;
use crate::log;
use crate::region::Founding;

const FILE: &str = "founding";
const NEW: &str = "founding.new";
const VERSION: u16 = 1;
const CHECK: usize = 8;

/// Writes `given` to `dir` at its first open, or checks it against the founding that
/// the first open wrote. `logged` states that the log of `dir` holds a record. The
/// caller holds the lock of the log, and the largest block of `pool` is one sector or
/// more.
///
/// # Errors
///
/// - [`Error::Founding`] when the first open wrote another founding.
/// - [`Error::Unfounded`] when the log holds a record and `dir` holds no founding, or
///   a founding that does not read back whole.
/// - [`Error::Pool`] when the pool has no block.
/// - [`Error::Log`] with [`log::Error::Files`] when a file call fails.
pub(super) async fn keep(
    files: &Files,
    dir: &Path,
    pool: &Pool,
    given: &Founding,
    logged: bool,
) -> Result<(), Error> {
    let names = files.list(dir).await.map_err(failed)?;
    let path = dir.join(FILE);
    if names.iter().any(|name| name == Path::new(FILE)) {
        let stored = read(files, &path, pool).await?;
        let unfounded = || Error::Unfounded { path: path.clone() };
        let (check, rest) =
            stored.split_first_chunk::<CHECK>().ok_or_else(unfounded)?;
        let (version, body) = rest.split_first_chunk::<2>().ok_or_else(unfounded)?;
        if *check != digest(rest) || u16::from_le_bytes(*version) != VERSION {
            return Err(unfounded());
        }
        let stored = Founding::decode(body).ok_or_else(unfounded)?;
        let mut given = given.clone();
        given.members.sort_by_key(|member| member.card.key());
        if stored == given {
            return Ok(());
        }
        return Err(Error::Founding {
            stored: Box::new(stored),
            given: Box::new(given),
        });
    }
    if logged {
        return Err(Error::Unfounded { path });
    }
    write(files, dir, pool, &given.encode()).await
}

async fn read(files: &Files, path: &Path, pool: &Pool) -> Result<Vec<u8>, Error> {
    let file = files.open(path, Mode::Read).await.map_err(failed)?;
    let len =
        usize::try_from(file.len()).expect("invariant: a founding fits in memory");
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        let part = pool
            .alloc(len.saturating_sub(bytes.len()).min(pool.largest()))
            .map_err(Error::Pool)?;
        let part = file
            .read_at(offset(bytes.len()), part)
            .await
            .map_err(failed)?;
        bytes.extend_from_slice(&part);
    }
    file.close().await;
    Ok(bytes)
}

async fn write(
    files: &Files,
    dir: &Path,
    pool: &Pool,
    given: &[u8],
) -> Result<(), Error> {
    let mut rest = VERSION.to_le_bytes().to_vec();
    rest.extend(given);
    let mut bytes = digest(&rest).to_vec();
    bytes.extend(rest);
    let new = dir.join(NEW);
    files.remove(&new).await.map_err(failed)?;
    let len = offset(bytes.len());
    let mut file = files
        .open(&new, Mode::Create { len })
        .await
        .map_err(failed)?;
    let mut at = 0;
    for part in bytes.chunks(pool.largest()) {
        let block = block(pool, part).map_err(Error::Pool)?;
        file.write_at(at, &[block]).await.map_err(failed)?;
        at = at.saturating_add(offset(part.len()));
    }
    file.rename(&dir.join(FILE)).await.map_err(failed)?;
    file.close().await;
    files.sync_dir(dir).await.map_err(failed)
}

// The check of some bytes: the first bytes of their digest.
fn digest(bytes: &[u8]) -> [u8; CHECK] {
    *Digest::of(bytes)
        .0
        .first_chunk()
        .expect("invariant: a digest has 32 bytes")
}

fn offset(at: usize) -> u64 {
    u64::try_from(at).expect("invariant: an offset fits in 64 bits")
}

fn failed(error: files::Error) -> Error {
    Error::Log(log::Error::Files(error))
}
