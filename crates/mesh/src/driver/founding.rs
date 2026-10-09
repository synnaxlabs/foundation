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
use env::files::{Files, Mode};

use crate::bytes::block;
use crate::error::Error;
use crate::file::{self, CHECK};
use crate::region::Founding;

/// The name of the founding file in a mesh directory.
pub(super) const FILE: &str = "founding";
const NEW: &str = "founding.new";
const VERSION: u16 = 1;

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
/// - [`Error::Files`] when a file call fails.
pub(super) async fn keep(
    files: &Files,
    dir: &Path,
    pool: &Pool,
    given: &Founding,
    logged: bool,
) -> Result<(), Error> {
    let names = files.list(dir).await.map_err(Error::Files)?;
    let path = dir.join(FILE);
    if names.iter().any(|name| name == Path::new(FILE)) {
        let file = files.open(&path, Mode::Read).await.map_err(Error::Files)?;
        let stored = file::read(&file, pool).await;
        file.close().await;
        let stored = stored?;
        let unfounded = || Error::Unfounded { path: path.clone() };
        let (check, rest) =
            stored.split_first_chunk::<CHECK>().ok_or_else(unfounded)?;
        let (version, body) = rest.split_first_chunk::<2>().ok_or_else(unfounded)?;
        if *check != file::check(rest) || u16::from_le_bytes(*version) != VERSION {
            return Err(unfounded());
        }
        if body == given.encode() {
            return Ok(());
        }
        let stored = Founding::decode(body).ok_or_else(unfounded)?;
        let mut given = given.clone();
        given.members.sort_by_key(|member| member.card.key());
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

async fn write(
    files: &Files,
    dir: &Path,
    pool: &Pool,
    given: &[u8],
) -> Result<(), Error> {
    let mut rest = VERSION.to_le_bytes().to_vec();
    rest.extend(given);
    let mut bytes = file::check(&rest).to_vec();
    bytes.extend(rest);
    let new = dir.join(NEW);
    files.remove(&new).await.map_err(Error::Files)?;
    let len = file::wide(bytes.len());
    let mut file = files
        .open(&new, Mode::Create { len })
        .await
        .map_err(Error::Files)?;
    let mut at = 0;
    for part in bytes.chunks(file::chunk(pool)) {
        let block = block(pool, part).map_err(Error::Pool)?;
        file.write_at(at, &[block]).await.map_err(Error::Files)?;
        at = at.saturating_add(file::wide(part.len()));
    }
    file.rename(&dir.join(FILE)).await.map_err(Error::Files)?;
    file.close().await;
    files.sync_dir(dir).await.map_err(Error::Files)
}
