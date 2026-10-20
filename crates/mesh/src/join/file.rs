//! The file `join` of a mesh directory: the request that this node sends to join its
//! region. It holds the card and the ticket's admission, a signature over that card
//! only, and never the private key of the ticket.
//!
//! The file holds an 8-byte check of the rest, the format version, then what
//! `Request::encode` gives. A write removes `join`, writes `join.new`, renames it to
//! `join`, and syncs the directory.

use std::path::Path;

use env::files::{self, Files, Mode};

use crate::error::Error;
use crate::file::{self, Blocks};
use crate::region::Request;

/// The name of the join file in a mesh directory.
const FILE: &str = "join";
const NEW: &str = "join.new";
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
    let opened = match files.open(&path, mode).await {
        Ok(opened) => opened,
        Err(files::Error::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(Error::Files(error)),
    };
    let bytes = blocks.read(&opened).await;
    opened.close().await;
    let bytes = bytes?;
    let request = match file::unframe(&bytes) {
        Some((VERSION, mut body)) => {
            Request::decode(&mut body).filter(|_| body.is_empty())
        }
        _ => None,
    };
    request.map(Some).ok_or(Error::JoinFile { path })
}

/// Makes `dir`, durable in its parent, and writes `request` as its join file. The file
/// is durable when it returns `Ok`.
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
    files.create_dir(dir).await.map_err(Error::Files)?;
    let parent = dir.parent().unwrap_or(Path::new(""));
    files.sync_dir(parent).await.map_err(Error::Files)?;
    file::replace(files, dir, [FILE, NEW], blocks, VERSION, &body).await?;
    files.sync_dir(dir).await.map_err(Error::Files)
}
