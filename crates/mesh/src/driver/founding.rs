//! The file `founding` of a mesh directory: the region before the first entry of the
//! log. An open whose log holds no record is a first open: it writes the file. Each
//! open whose log holds a record checks `Config::founding` against it.
//!
//! The file holds an 8-byte check of the rest, the format version, then what
//! `region::Founding::encode` gives. A first open removes `founding`, writes
//! `founding.new`, renames it to `founding`, and syncs the directory, before raft
//! writes a record. So a crash before the first record leaves a first open.

use std::path::Path;
use std::rc::Rc;

use block::Pool;
use env::files::{self, Files, Mode};

use crate::error::Error;
use crate::file::{self, Blocks, CHECK};
use crate::region::Founding;

/// The name of the founding file in a mesh directory.
pub(super) const FILE: &str = "founding";
const NEW: &str = "founding.new";
const VERSION: u16 = 1;

/// The founding that the mesh directory `dir` of `files` holds, or `None` when `dir`
/// is not there or no [`Mesh::open`](crate::Mesh::open) wrote a founding in it.
/// [`Mesh::open`](crate::Mesh::open) with this founding opens the same region.
///
/// # Errors
///
/// - [`Error::Unfounded`] when `dir` holds a founding file that does not read back
///   whole.
/// - [`Error::Pool`] when `pool` has no block for the read.
/// - [`Error::Files`] when a file call fails.
pub async fn founding(
    files: &Files,
    dir: &Path,
    pool: Rc<Pool>,
) -> Result<Option<Founding>, Error> {
    let blocks = Blocks::new(pool).map_err(Error::Pool)?;
    let Some(body) = read(files, dir, &blocks).await? else {
        return Ok(None);
    };
    let founding = Founding::decode(&body).ok_or_else(|| unfounded(dir))?;
    Ok(Some(founding))
}

/// Writes `given` to `dir` when `logged` is false, or checks it against the founding
/// in `dir`. `logged` states that the log of `dir` holds a record. The caller holds
/// the lock of the log. A founding that it writes is durable when it returns.
///
/// # Errors
///
/// - [`Error::Founding`] when `dir` holds another founding.
/// - [`Error::Unfounded`] when the log holds a record and `dir` holds no founding, or
///   a founding that does not read back whole.
/// - [`Error::Pool`] when the pool has no block.
/// - [`Error::Files`] when a file call fails.
pub(super) async fn keep(
    files: &Files,
    dir: &Path,
    blocks: &Blocks,
    given: &Founding,
    logged: bool,
) -> Result<(), Error> {
    if !logged {
        return write(files, dir, blocks, &given.encode()).await;
    }
    let body = read(files, dir, blocks)
        .await?
        .ok_or_else(|| unfounded(dir))?;
    if body == given.encode() {
        return Ok(());
    }
    let stored = Founding::decode(&body).ok_or_else(|| unfounded(dir))?;
    let mut given = given.clone();
    given.members.sort_by_key(|member| member.card.key());
    Err(Error::Founding {
        stored: Box::new(stored),
        given: Box::new(given),
    })
}

// The body of the founding file in `dir`, after its check and version, or `None` when
// `dir` or the file is not there.
async fn read(
    files: &Files,
    dir: &Path,
    blocks: &Blocks,
) -> Result<Option<Vec<u8>>, Error> {
    let names = match files.list(dir).await {
        Ok(names) => names,
        Err(files::Error::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(Error::Files(error)),
    };
    if !names.iter().any(|name| name == Path::new(FILE)) {
        return Ok(None);
    }
    let opened = files
        .open(&dir.join(FILE), Mode::Read)
        .await
        .map_err(Error::Files)?;
    let stored = blocks.read(&opened).await;
    opened.close().await;
    let stored = stored?;
    let (check, rest) = stored
        .split_first_chunk::<CHECK>()
        .ok_or_else(|| unfounded(dir))?;
    let (version, body) = rest
        .split_first_chunk::<2>()
        .ok_or_else(|| unfounded(dir))?;
    if *check != file::check(rest) || u16::from_le_bytes(*version) != VERSION {
        return Err(unfounded(dir));
    }
    Ok(Some(body.to_vec()))
}

fn unfounded(dir: &Path) -> Error {
    Error::Unfounded {
        path: dir.join(FILE),
    }
}

// `File::rename` refuses an existing target, so the old `founding` goes first.
async fn write(
    files: &Files,
    dir: &Path,
    blocks: &Blocks,
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
    let mut opened = files
        .open(&new, Mode::Create { len })
        .await
        .map_err(Error::Files)?;
    blocks.write(&opened, 0, &mut &bytes[..]).await?;
    opened.rename(&path).await.map_err(Error::Files)?;
    opened.close().await;
    files.sync_dir(dir).await.map_err(Error::Files)
}
