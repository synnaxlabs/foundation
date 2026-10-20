//! The file `join` of a mesh directory: the request that this node sends to join its
//! region. It holds the card and the ticket's admission, a signature over that card
//! only, and never the private key of the ticket.
//!
//! The file holds an 8-byte check of the rest, the format version, then what
//! `Request::encode` gives. A write removes `join`, writes `join.new`, renames it to
//! `join`, and syncs the directory. The file stays for the life of the directory: each
//! open of a founding that lacks this node reads it.

use std::path::Path;

use env::files::{Files, Mode};

use crate::change;
use crate::error::Error;
use crate::file::{self, Blocks};
use crate::region::Request;

/// The name of the join file in a mesh directory.
const FILE: &str = "join";
const VERSION: u16 = 1;

/// The request in the join file of `dir`, or `None` when `dir` or the file is not
/// there. A `mode` of `Mode::Write` first waits for each call on the file that a
/// dropped [`write`] left to run; a read open does not.
///
/// # Errors
///
/// - [`Error::JoinFile`] when the file does not read back whole.
/// - [`Error::Pool`] when the pool has no block for the read.
/// - [`Error::Files`] when a file call fails.
pub(crate) async fn read(
    files: &Files,
    dir: &Path,
    blocks: &Blocks,
    mode: Mode,
) -> Result<Option<Request>, Error> {
    let path = dir.join(FILE);
    let read = file::read(files, &path, blocks, mode, VERSION, change::BYTES_MAX);
    let Some(framed) = read.await? else {
        return Ok(None);
    };
    let request = framed.and_then(|framed| {
        let mut body = framed.body();
        Request::decode(&mut body).filter(|_| body.is_empty())
    });
    request.map(Some).ok_or(Error::JoinFile { path })
}

/// Writes `request` as the join file of `dir`, which [`durable`] made. The file is
/// durable when it returns `Ok`.
///
/// # Errors
///
/// - [`Error::Pool`] when the pool has no block for the write.
/// - [`Error::Files`] when a file call fails.
pub(crate) async fn write(
    files: &Files,
    dir: &Path,
    blocks: &Blocks,
    request: &Request,
) -> Result<(), Error> {
    let mut body = Vec::new();
    request.encode(&mut body);
    Ok(file::replace(files, dir, FILE, blocks, VERSION, &body).await?)
}

/// Makes `dir` and the name of each file in it durable, so that a join file that a
/// crash left in the page cache stays.
///
/// # Errors
///
/// - [`Error::Files`] when a file call fails.
pub(crate) async fn durable(files: &Files, dir: &Path) -> Result<(), Error> {
    file::make_dir(files, dir).await.map_err(Error::Files)?;
    files.sync_dir(dir).await.map_err(Error::Files)
}
