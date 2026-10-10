//! The file `founding` of a mesh directory: the region before the first entry of the
//! log. An open whose log holds no record is a first open: it writes the file. Each
//! open whose log holds a record checks `Config::founding` against it.
//!
//! The file holds an 8-byte check of the rest, the format version, then what
//! `region::Founding::encode` gives. A first open removes `founding`, writes
//! `founding.new`, renames it to `founding`, and syncs the directory, before raft
//! writes a record. So a crash before the first record leaves a first open.

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

/// Writes `given` to `dir` when `logged` is false, or checks it against the founding
/// in `dir`. `logged` states that the log of `dir` holds a record. The caller holds
/// the lock of the log.
///
/// # Errors
///
/// - [`Error::Founding`] when `dir` holds another founding.
/// - [`Error::Unfounded`] when the log holds a record and `dir` holds no founding, or
///   a founding that does not read back whole.
/// - [`Error::Pool`] when the pool has no block, or no block of one sector.
/// - [`Error::Files`] when a file call fails.
pub(super) async fn keep(
    files: &Files,
    dir: &Path,
    pool: &Pool,
    given: &Founding,
    logged: bool,
) -> Result<(), Error> {
    if !logged {
        return write(files, dir, pool, &given.encode()).await;
    }
    let path = dir.join(FILE);
    let unfounded = || Error::Unfounded { path: path.clone() };
    let names = files.list(dir).await.map_err(Error::Files)?;
    if !names.iter().any(|name| name == Path::new(FILE)) {
        return Err(unfounded());
    }
    let opened = files.open(&path, Mode::Read).await.map_err(Error::Files)?;
    let stored = file::read(&opened, pool).await;
    opened.close().await;
    let stored = stored?;
    let (check, rest) = stored.split_first_chunk::<CHECK>().ok_or_else(unfounded)?;
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
    Err(Error::Founding {
        stored: Box::new(stored),
        given: Box::new(given),
    })
}

// `File::rename` refuses an existing target, so the old `founding` goes first.
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
    let (path, new) = (dir.join(FILE), dir.join(NEW));
    files.remove(&path).await.map_err(Error::Files)?;
    files.remove(&new).await.map_err(Error::Files)?;
    let len = file::wide(bytes.len());
    let mut file = files
        .open(&new, Mode::Create { len })
        .await
        .map_err(Error::Files)?;
    let mut at = 0;
    for part in bytes.chunks(file::chunk(pool).map_err(Error::Pool)?) {
        let block = block(pool, part).map_err(Error::Pool)?;
        file.write_at(at, &[block]).await.map_err(Error::Files)?;
        at = at.saturating_add(file::wide(part.len()));
    }
    file.rename(&path).await.map_err(Error::Files)?;
    file.close().await;
    files.sync_dir(dir).await.map_err(Error::Files)
}
