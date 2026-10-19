//! The file `founding` of a mesh directory: the region before the first entry of the
//! log. An open whose log holds no record is a first open: it writes the file. Each
//! open whose log holds a record checks `Config::founding` against it.
//!
//! The file holds an 8-byte check of the rest, the format version, then what
//! `region::Founding::encode` gives. A first open whose directory holds another
//! founding or none removes `founding`, writes `founding.new`, renames it to
//! `founding`, and syncs the directory, before raft writes a record. So a crash before
//! the first record leaves a first open, and a reopen with the same founding keeps it.

use std::cell::{Cell, RefCell};
use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::task::{Poll, Waker};

use block::Pool;
use env::files::{self, Files, Mode};

use crate::error::Error;
use crate::file::{self, Blocks};
use crate::message::FOUNDING_BYTES_MAX;
use crate::region::Founding;

/// The name of the founding file in a mesh directory.
pub(super) const FILE: &str = "founding";
const NEW: &str = "founding.new";
const VERSION: u16 = 1;

/// The founding that the mesh directory `dir` of `files` holds, or `None` when `dir`
/// is not there or holds no founding file. [`Mesh::open`](crate::Mesh::open) with this
/// founding opens the same region.
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
    let Some(body) = read(files, dir, &blocks, Mode::Read).await? else {
        return Ok(None);
    };
    let founding = Founding::decode(body.body()).ok_or_else(|| unfounded(dir))?;
    Ok(Some(founding))
}

/// Writes `given` to `dir` when `logged` is false and `dir` holds another founding or
/// none, or checks it against the founding in `dir`. `logged` states that the log of
/// `dir` holds a record. The caller holds the lock of the log. The founding in `dir` is
/// durable when it returns `Ok`.
///
/// # Errors
///
/// - [`Error::Unjoinable`] when the log holds no record and the byte form of `given`
///   is over [`FOUNDING_BYTES_MAX`] bytes.
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
    let encoded = given.encode();
    // A later open does not check it, so a node with a stored founding always opens.
    if !logged && encoded.len() > FOUNDING_BYTES_MAX {
        return Err(Error::Unjoinable {
            bytes: encoded.len(),
            most: FOUNDING_BYTES_MAX,
        });
    }
    let body = match read(files, dir, blocks, Mode::Write).await {
        // A first open writes over it, and an open with a record refuses it below.
        Err(Error::Unfounded { .. }) => None,
        body => body?,
    };
    let body = body.as_ref().map(Stored::body);
    if !logged {
        if body != Some(&encoded[..]) {
            write(files, dir, blocks, &encoded).await?;
        }
        // Also for the same founding: a crash after the rename of an earlier open can
        // leave its name not durable.
        return files.sync_dir(dir).await.map_err(Error::Files);
    }
    if body == Some(&encoded[..]) {
        return Ok(());
    }
    let body = body.ok_or_else(|| unfounded(dir))?;
    let stored = Founding::decode(body).ok_or_else(|| unfounded(dir))?;
    let mut given = given.clone();
    given.members.sort_by_key(|member| member.card.key());
    Err(Error::Founding {
        stored: Box::new(stored),
        given: Box::new(given),
    })
}

/// The bytes of the founding file: the check and the version, then the body.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Stored {
    framed: Vec<u8>,
    start: usize,
}

impl Stored {
    /// The byte form of the founding.
    pub(super) fn body(&self) -> &[u8] {
        self.framed
            .get(self.start..)
            .expect("invariant: the body starts inside the file")
    }
}

// The founding file in `dir`, after a check of its check and version, or `None` when
// `dir` or the file is not there. A `mode` of `Mode::Write` first waits for each call
// on the file that a dropped `keep` left to run, such as its remove; a read open does
// not.
async fn read(
    files: &Files,
    dir: &Path,
    blocks: &Blocks,
    mode: Mode,
) -> Result<Option<Stored>, Error> {
    let opened = match files.open(&dir.join(FILE), mode).await {
        Ok(opened) => opened,
        Err(files::Error::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(Error::Files(error)),
    };
    let stored = blocks.read(&opened).await;
    opened.close().await;
    let stored = stored?;
    let start = match file::unframe(&stored) {
        Some((VERSION, body)) => (stored.len().checked_sub(body.len()))
            .expect("invariant: the body is a tail of the file"),
        _ => return Err(unfounded(dir)),
    };
    Ok(Some(Stored {
        framed: stored,
        start,
    }))
}

fn unfounded(dir: &Path) -> Error {
    Error::Unfounded {
        path: dir.join(FILE),
    }
}

// `File::rename` refuses an existing target, so the old `founding` goes first. The
// caller syncs `dir`.
async fn write(
    files: &Files,
    dir: &Path,
    blocks: &Blocks,
    given: &[u8],
) -> Result<(), Error> {
    let bytes = file::frame(VERSION, given);
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
    Ok(())
}

/// The founding file that join answers send. An idle voter holds no copy: each answer
/// holds one, and answers at one time share it.
pub(super) struct Shared {
    files: Files,
    dir: PathBuf,
    blocks: Blocks,
    turn: RefCell<Turn>,
}

#[derive(Default)]
struct Turn {
    copy: Weak<Stored>,
    reading: bool,
    // Each call that waits, with the slot that a read that ends fills with its copy.
    // The read can end in the poll that also drops its last copy, before a woken call
    // runs.
    waiting: Vec<(Waker, Rc<Slot>)>,
}

type Slot = Cell<Option<Rc<Stored>>>;

impl Shared {
    /// The founding file of `dir`, which an open of the mesh checked, read with
    /// `blocks`.
    pub(super) fn new(files: Files, dir: PathBuf, blocks: Blocks) -> Self {
        Self {
            files,
            dir,
            blocks,
            turn: RefCell::default(),
        }
    }

    /// Gives the copy that another answer holds, or reads the file. While one call
    /// reads, each other call waits for its copy.
    pub(super) async fn get(&self) -> Result<Rc<Stored>, Error> {
        let slot = Rc::new(Slot::default());
        poll_fn(|cx| {
            let mut turn = self.turn.borrow_mut();
            if !turn.reading {
                return Poll::Ready(());
            }
            turn.waiting.push((cx.waker().clone(), Rc::clone(&slot)));
            Poll::Pending
        })
        .await;
        if let Some(copy) = slot.take().or_else(|| self.turn.borrow().copy.upgrade()) {
            return Ok(copy);
        }
        let reading = Reading::new(&self.turn);
        let read = read(&self.files, &self.dir, &self.blocks, Mode::Read);
        let stored = read.await?.ok_or_else(|| unfounded(&self.dir))?;
        let copy = Rc::new(stored);
        reading.give(&copy);
        Ok(copy)
    }
}

// The turn of the call that reads. It ends when the call ends or drops, and wakes each
// call that waits.
struct Reading<'a>(&'a RefCell<Turn>);

impl<'a> Reading<'a> {
    fn new(turn: &'a RefCell<Turn>) -> Self {
        turn.borrow_mut().reading = true;
        Self(turn)
    }

    // Ends the turn with `copy`, which each call that waits gets.
    fn give(self, copy: &Rc<Stored>) {
        let mut turn = self.0.borrow_mut();
        turn.copy = Rc::downgrade(copy);
        for (_, slot) in &turn.waiting {
            slot.set(Some(Rc::clone(copy)));
        }
    }
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        let mut turn = self.0.borrow_mut();
        turn.reading = false;
        turn.waiting.drain(..).for_each(|(waker, _)| waker.wake());
    }
}
