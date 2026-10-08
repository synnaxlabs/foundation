//! Keeps the spec that this node uses: reads the spec of each new committed pointer,
//! and names the pointer in use in a file.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::future::poll_fn;
use std::mem;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll, Waker};

use env::clock::{Clock, Sleep};
use env::files::{Files, Mode};
use spec::definition::Definition;
use spec::tree::{self, Chunks};
use types::digest::Digest;
use types::name::{Name, Prefix};
use types::time::Span;

use super::{Group, Mesh};
use crate::error::{Error, Stopped};
use crate::pointer::Pointer;
use crate::used::{Behind, Cause, Spec};

/// The directory of the file that names the pointer in use, in
/// [`Config::dir`](super::Config::dir).
pub(super) const SPEC: &str = "spec";
/// The time between two reads of a pointer that failed for a cause that can pass.
const RETRY: Span = Span::SECOND;

/// The spec that this node uses, and the newest committed pointer that it does not
/// use yet. It holds at most two trees: the tree in use, and the chunks of the tree
/// of the newest pointer that a read got.
pub(super) struct Used {
    // The pointer in use, or `None` when the node uses no spec.
    pointer: Option<Pointer>,
    definitions: Rc<BTreeMap<Name, Definition>>,
    // The chunks of the tree in use.
    pub(super) chunks: Chunks,
    pub(super) newest: Option<Newest>,
    // The version of the newest pointer whose first read ended.
    settled: u64,
    // The task of `keep`, while it waits for a pointer to read.
    task: Option<Waker>,
}

/// The newest committed pointer above the one in use. It stays until its spec takes
/// effect or a newer pointer replaces it.
pub(super) struct Newest {
    pub(super) pointer: Pointer,
    // The chunks that its change listed, until a read takes them.
    listed: BTreeSet<Digest>,
    // Each chunk of its tree that a read got from the store.
    got: Vec<Vec<u8>>,
    step: Step,
    // Why its last read failed.
    cause: Option<Cause>,
}

/// What the task does next for the newest pointer.
#[derive(Clone, Copy)]
enum Step {
    /// Read its spec now.
    Read,
    /// After `RETRY`, get the chunk that the last read missed, and read again only
    /// when the store gives it.
    Get(Digest),
    /// After `RETRY`, read again.
    Retry,
    /// Nothing: its tree or its spec is at fault.
    Stuck,
}

/// One call of the store or one read that the task makes for a pointer.
enum Job {
    Get(Digest),
    Read {
        listed: BTreeSet<Digest>,
        // The tree in use, and each chunk of `Newest::got`.
        chunks: Chunks,
    },
}

/// What a job gave.
enum Done {
    /// The spec read, had no problem, and its file is durable: its definitions and
    /// the chunks of its tree.
    Taken(BTreeMap<Name, Definition>, Chunks),
    /// The read failed, after it got `got` from the store.
    Failed { cause: Cause, got: Vec<Vec<u8>> },
    /// What the store gave for the chunk.
    Got(Digest, Result<Option<Vec<u8>>, blob::Error>),
}

impl Used {
    /// Takes the pointer of a `Spec` change that applied, with the chunks that the
    /// change listed. A pointer at or below one that this node knows, as on the
    /// replay of the log after an open, changes nothing.
    pub(super) fn committed(&mut self, pointer: Pointer, listed: BTreeSet<Digest>) {
        if pointer.version <= self.known() {
            return;
        }
        self.newest = Some(Newest {
            pointer,
            listed,
            got: Vec::new(),
            step: Step::Read,
            cause: None,
        });
        self.wake();
    }

    /// Wakes the task of [`keep`], so that it reads a new pointer or ends.
    pub(super) fn wake(&mut self) {
        if let Some(task) = self.task.take() {
            task.wake();
        }
    }

    // The version of the newest pointer that this node knows.
    fn known(&self) -> u64 {
        let used = self.pointer.map(|pointer| pointer.version);
        let newest = self.newest.as_ref().map(|newest| newest.pointer.version);
        used.max(newest).unwrap_or(0)
    }

    // The spec in use, and why the node does not use the newest pointer.
    fn spec(&self) -> Spec {
        let behind = self.newest.as_ref().and_then(|newest| {
            let cause = newest.cause.clone()?;
            Some(Behind {
                pointer: newest.pointer,
                cause,
            })
        });
        Spec {
            pointer: self.pointer,
            definitions: Rc::clone(&self.definitions),
            behind,
        }
    }

    // The next job for the newest pointer, once its step allows one.
    fn next(
        &mut self,
        clock: &Clock,
        retry: &mut Option<Sleep>,
        cx: &mut Context<'_>,
    ) -> Poll<(Pointer, Job)> {
        let ready = match self.newest.as_ref().map(|newest| newest.step) {
            None | Some(Step::Stuck) => false,
            Some(Step::Read) => true,
            Some(Step::Get(_) | Step::Retry) => {
                let sleep = retry.get_or_insert_with(|| clock.sleep(RETRY));
                Pin::new(sleep).poll(cx).is_ready()
            }
        };
        let Some(newest) = self.newest.as_mut().filter(|_| ready) else {
            self.task = Some(cx.waker().clone());
            return Poll::Pending;
        };
        *retry = None;
        let job = match newest.step {
            Step::Get(digest) => Job::Get(digest),
            Step::Read | Step::Retry | Step::Stuck => {
                let mut chunks = self.chunks.clone();
                for chunk in &newest.got {
                    chunks.insert(chunk.clone());
                }
                let listed = mem::take(&mut newest.listed);
                Job::Read { listed, chunks }
            }
        };
        Poll::Ready((newest.pointer, job))
    }

    // Records what the job for `pointer` gave. A job for a pointer that a newer one
    // replaced changes nothing, unless its spec took effect.
    fn settle(&mut self, pointer: Pointer, done: Done) {
        self.settled = self.settled.max(pointer.version);
        let newest = self
            .newest
            .as_mut()
            .filter(|newest| newest.pointer == pointer);
        match (done, newest) {
            (Done::Taken(definitions, chunks), newest) => {
                if newest.is_some() {
                    self.newest = None;
                }
                self.pointer = Some(pointer);
                self.definitions = Rc::new(definitions);
                self.chunks = chunks;
            }
            (_, None) => {}
            (Done::Failed { cause, got }, Some(newest)) => {
                newest.got.extend(got);
                newest.step = match &cause {
                    Cause::Read(spec::region::Error::Tree(tree::Error::Missing(
                        digest,
                    ))) => Step::Get(*digest),
                    Cause::Blob(_) | Cause::Files(_) => Step::Retry,
                    Cause::Read(_) | Cause::Problems(_) => Step::Stuck,
                };
                newest.cause = Some(cause);
            }
            (Done::Got(_, Ok(Some(chunk))), Some(newest)) => {
                newest.got.push(chunk);
                newest.step = Step::Read;
            }
            (Done::Got(digest, Ok(None)), Some(newest)) => {
                newest.cause = Some(missing(digest));
            }
            (Done::Got(_, Err(error)), Some(newest)) => {
                newest.cause = Some(Cause::Blob(error));
            }
        }
    }
}

// The cause of a read that missed the chunk of `digest`.
fn missing(digest: Digest) -> Cause {
    Cause::Read(spec::region::Error::Tree(tree::Error::Missing(digest)))
}

/// What [`open`] reads the spec in use from.
pub(super) struct Opening<'a> {
    pub(super) files: &'a Files,
    pub(super) dir: &'a Path,
    pub(super) store: &'a blob::Store,
    pub(super) prefix: &'a Prefix,
    // The root of the founding tree.
    pub(super) root: Digest,
    pub(super) definitions: BTreeMap<Name, Definition>,
    // The chunks of the founding tree.
    pub(super) chunks: Chunks,
}

/// Makes the directory of the file in `dir`, and reads the spec that its file names,
/// or the founding spec when there is no file. It removes each file but the one of
/// the highest version.
///
/// # Errors
///
/// [`Error::Files`] when a file call fails, and [`Error::Stray`] when the directory
/// holds a file that does not name a pointer.
pub(super) async fn open(opening: Opening<'_>) -> Result<Used, Error> {
    let Opening {
        files,
        dir,
        store,
        prefix,
        root,
        definitions,
        chunks,
    } = opening;
    let founding = Pointer { version: 0, root };
    let held = dir.join(SPEC);
    files.create_dir(&held).await.map_err(Error::Files)?;
    files.sync_dir(dir).await.map_err(Error::Files)?;
    let mut named = Vec::new();
    for name in files.list(&held).await.map_err(Error::Files)? {
        let path = held.join(&name);
        let Some(pointer) = pointer(&name) else {
            return Err(Error::Stray { path });
        };
        named.push((pointer, path));
    }
    named.sort_by_key(|(pointer, _)| pointer.version);
    let file = named.pop();
    for (_, path) in &named {
        files.remove(path).await.map_err(Error::Files)?;
    }
    files.sync_dir(&held).await.map_err(Error::Files)?;
    let Some((pointer, _)) = file else {
        let problems = spec::region::check(prefix, &definitions);
        if problems.is_empty() {
            return Ok(Used {
                pointer: Some(founding),
                definitions: Rc::new(definitions),
                chunks,
                newest: None,
                settled: 0,
                task: None,
            });
        }
        let failed = Done::Failed {
            cause: Cause::Problems(problems),
            got: Vec::new(),
        };
        return Ok(behind(founding, failed));
    };
    let mut got = Vec::new();
    let read = read(
        store,
        prefix,
        Chunks::default(),
        pointer,
        BTreeSet::new(),
        &mut got,
    );
    match read.await {
        Ok((definitions, chunks)) => Ok(Used {
            pointer: Some(pointer),
            definitions: Rc::new(definitions),
            chunks,
            newest: None,
            settled: pointer.version,
            task: None,
        }),
        Err(cause) => Ok(behind(pointer, Done::Failed { cause, got })),
    }
}

// The state of a node that uses no spec, since the read of the spec of `pointer`
// failed.
fn behind(pointer: Pointer, failed: Done) -> Used {
    let newest = Newest {
        pointer,
        listed: BTreeSet::new(),
        got: Vec::new(),
        step: Step::Read,
        cause: None,
    };
    let mut used = Used {
        pointer: None,
        definitions: Rc::default(),
        chunks: Chunks::default(),
        newest: Some(newest),
        settled: 0,
        task: None,
    };
    used.settle(pointer, failed);
    used
}

/// Runs the job of each new committed pointer of `group`, and makes a spec that reads
/// and has no problem the spec in use. It ends when the group stops or drops.
pub(super) async fn keep(
    group: Weak<RefCell<Group>>,
    store: Rc<blob::Store>,
    files: Files,
    dir: PathBuf,
    clock: Clock,
) {
    let held = dir.join(SPEC);
    let mut retry = None;
    loop {
        let next = poll_fn(|cx| {
            let Some(group) = group.upgrade() else {
                return Poll::Ready(None);
            };
            let mut group = group.borrow_mut();
            if group.running().is_err() {
                return Poll::Ready(None);
            }
            let prefix = group.state.prefix().clone();
            let next = group.used.next(&clock, &mut retry, cx);
            next.map(|next| Some((next, prefix)))
        });
        let Some(((pointer, job), prefix)) = next.await else {
            return;
        };
        let done = match job {
            Job::Get(digest) => {
                let got = store.get(digest).await;
                Done::Got(digest, got.map(|chunk| chunk.map(|chunk| chunk.to_vec())))
            }
            Job::Read { listed, chunks } => {
                let mut got = Vec::new();
                match read(&store, &prefix, chunks, pointer, listed, &mut got).await {
                    Ok((definitions, chunks)) => {
                        match name(&files, &held, pointer).await {
                            Ok(()) => Done::Taken(definitions, chunks),
                            Err(cause) => Done::Failed { cause, got },
                        }
                    }
                    Err(cause) => Done::Failed { cause, got },
                }
            }
        };
        let Some(group) = group.upgrade() else { return };
        let mut group = group.borrow_mut();
        group.used.settle(pointer, done);
        group.wake_calls();
    }
}

// The definitions of the spec of `pointer`, and the chunks of its tree, when it reads
// and has no problem. `chunks` holds the tree in use. The read gets each chunk of
// `listed` that `chunks` lacks and each chunk that the tree needs from `store`, and
// adds each to `got`.
async fn read(
    store: &blob::Store,
    prefix: &Prefix,
    mut chunks: Chunks,
    pointer: Pointer,
    listed: BTreeSet<Digest>,
    got: &mut Vec<Vec<u8>>,
) -> Result<(BTreeMap<Name, Definition>, Chunks), Cause> {
    for digest in listed {
        if chunks.get(digest).is_some() {
            continue;
        }
        if let Some(chunk) = store.get(digest).await.map_err(Cause::Blob)? {
            chunks.insert(chunk.to_vec());
            got.push(chunk.to_vec());
        }
    }
    let definitions = loop {
        let digest = match spec::region::definitions(&chunks, pointer.root) {
            Ok(definitions) => break definitions,
            Err(spec::region::Error::Tree(tree::Error::Missing(digest))) => digest,
            Err(error) => return Err(Cause::Read(error)),
        };
        let Some(chunk) = store.get(digest).await.map_err(Cause::Blob)? else {
            return Err(missing(digest));
        };
        chunks.insert(chunk.to_vec());
        got.push(chunk.to_vec());
    };
    let problems = spec::region::check(prefix, &definitions);
    if !problems.is_empty() {
        return Err(Cause::Problems(problems));
    }
    let mut kept = Chunks::default();
    spec::region::tree(&mut kept, &definitions);
    Ok((definitions, kept))
}

// Makes the file in `held` that names `pointer` durable, then removes each other file
// in `held`. A failed removal leaves a file that the next change or open removes.
async fn name(files: &Files, held: &Path, pointer: Pointer) -> Result<(), Cause> {
    let name = file(pointer);
    let created = files.open(&held.join(&name), Mode::Create { len: 0 }).await;
    drop(created.map_err(Cause::Files)?);
    files.sync_dir(held).await.map_err(Cause::Files)?;
    let Ok(names) = files.list(held).await else {
        return Ok(());
    };
    for old in names.into_iter().filter(|old| *old != name) {
        drop(files.remove(&held.join(old)).await);
    }
    Ok(())
}

// The pointer that the name of a file spells, `<version>-<root>`.
fn pointer(name: &Path) -> Option<Pointer> {
    let name = name.to_str()?;
    let (version, root) = name.split_once('-')?;
    let version = version.parse().ok()?;
    if root.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (byte, pair) in bytes.iter_mut().zip(root.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    let pointer = Pointer {
        version,
        root: Digest(bytes),
    };
    (file(pointer) == Path::new(name)).then_some(pointer)
}

// The name of the file that names `pointer`.
fn file(pointer: Pointer) -> PathBuf {
    PathBuf::from(format!("{}-{}", pointer.version, pointer.root))
}

impl Mesh {
    /// The spec that this node uses, once it has read the spec of the pointer that
    /// was committed at the call. It waits only for the first read of that pointer,
    /// never for a retry or a later pointer.
    ///
    /// # Errors
    ///
    /// [`Stopped`] when the group stops first.
    pub async fn spec(&self) -> Result<Spec, Stopped> {
        let call = Call::new(&self.group);
        let committed = self.group.borrow().state.pointer().version;
        poll_fn(|cx| {
            let mut group = self.group.borrow_mut();
            if group.used.settled >= committed {
                return Poll::Ready(Ok(group.used.spec()));
            }
            if let Some(stopped) = group.stopped.get() {
                return Poll::Ready(Err(stopped.clone()));
            }
            group.calls.insert(call.slot, cx.waker().clone());
            Poll::Pending
        })
        .await
    }
}

// The slot of a call in `Group::calls`, which it frees when it drops.
struct Call<'a> {
    group: &'a RefCell<Group>,
    slot: u64,
}

impl<'a> Call<'a> {
    fn new(group: &'a RefCell<Group>) -> Self {
        let slot = group.borrow_mut().slot();
        Self { group, slot }
    }
}

impl Drop for Call<'_> {
    fn drop(&mut self) {
        self.group.borrow_mut().calls.remove(&self.slot);
    }
}
