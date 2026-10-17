//! The `env::files` drivers of a simulated node.

use std::future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use block::{Block, Unique};
use env::files::{Error, Mode, Operation, Request};
use env::paths::Found;

use super::Node;
use crate::DIRECTORY;
use crate::disk::Handle;
use crate::files::{Call, Done, Ended, Held};
use crate::log::{self, Kept};
use crate::state::lock;

/// The data directory that [`env::files::Driver::shown`] joins to, as `os` gives an
/// absolute path.
const ROOT: &str = "/data";

impl Node {
    /// Starts `call` on `path` now, keeping `held` until it ends.
    ///
    /// # Panics
    ///
    /// Outside a thread that the sim started, and on a thread of another node.
    fn submit(&self, path: PathBuf, call: Call, held: Option<Held>) -> Wait {
        self.running("a file call");
        let mut state = lock(&self.shared);
        let now = state.now();
        let key = state
            .files()
            .submit(now, self.node, path, call, held.map(Kept::new));
        drop(state);
        Wait {
            node: self.clone(),
            key,
            taken: false,
        }
    }

    /// Starts `call` on `path`, and maps what it gives when it succeeds by `map`.
    fn request<'a, T: 'a>(
        &self,
        path: &Path,
        call: Call,
        map: impl FnOnce(Done) -> T + 'a,
    ) -> Request<'a, T> {
        let wait = self.submit(path.to_path_buf(), call, None);
        Box::pin(async move { Ok(map(wait.await.result?)) })
    }
}

impl env::files::Driver for Node {
    fn open<'a>(
        &'a self,
        path: &'a Path,
        mode: Mode,
    ) -> Request<'a, Box<dyn env::files::Descriptor>> {
        self.request(path, Call::Open(mode), |done| {
            let Done::Open {
                handle,
                len,
                exposed,
            } = done
            else {
                unreachable!("invariant: an open gives a file")
            };
            lock(&self.shared).files().opened(self.node, handle, path);
            let node = self.clone();
            let descriptor: Box<dyn env::files::Descriptor> = Box::new(Descriptor {
                node,
                handle,
                len,
                exposed,
            });
            descriptor
        })
    }

    fn open_absolute<'a>(
        &'a self,
        path: &'a Path,
    ) -> Request<'a, Box<dyn env::files::Descriptor>> {
        let wait = self.submit(path.to_path_buf(), Call::Host(Operation::Open), None);
        let key = wait.key;
        Box::pin(async move {
            wait.await.result?;
            let found = lock(&self.shared).paths().read(self.node, path, usize::MAX);
            let io = |code| Error::Io {
                path: path.to_path_buf(),
                operation: Operation::Open,
                code,
            };
            match found {
                Ok(Found::File(bytes)) => {
                    lock(&self.shared).files().opened_host(self.node, key, path);
                    let descriptor: Box<dyn env::files::Descriptor> = Box::new(Host {
                        node: self.clone(),
                        key,
                        path: path.to_path_buf(),
                        bytes,
                    });
                    Ok(descriptor)
                }
                Ok(Found::Directory(_)) => Err(io(DIRECTORY)),
                Ok(Found::Large(_)) => {
                    unreachable!("invariant: no read is over usize::MAX")
                }
                Err(env::paths::Error::NotFound) => Err(Error::NotFound {
                    path: path.to_path_buf(),
                }),
                Err(env::paths::Error::Io { code }) => Err(io(code)),
                Err(
                    env::paths::Error::Outside
                    | env::paths::Error::Hidden
                    | env::paths::Error::Vacant
                    | env::paths::Error::Replaced,
                ) => {
                    unreachable!("invariant: the paths of a host have no limit")
                }
            }
        })
    }

    fn list<'a>(&'a self, dir: &'a Path) -> Request<'a, Vec<PathBuf>> {
        self.request(dir, Call::List, |done| {
            let Done::Names(names) = done else {
                unreachable!("invariant: a list gives names")
            };
            names
        })
    }

    fn create_dir<'a>(&'a self, dir: &'a Path) -> Request<'a, ()> {
        self.request(dir, Call::CreateDir, drop)
    }

    fn remove<'a>(&'a self, path: &'a Path) -> Request<'a, ()> {
        self.request(path, Call::Remove, drop)
    }

    fn sync_dir<'a>(&'a self, dir: &'a Path) -> Request<'a, ()> {
        self.request(dir, Call::SyncDir, drop)
    }

    fn free(&self) -> Request<'_, u64> {
        self.request(Path::new(""), Call::Free, |done| {
            let Done::Free(bytes) = done else {
                unreachable!("invariant: a free gives bytes")
            };
            bytes
        })
    }

    fn shown(&self, path: &Path) -> PathBuf {
        Path::new(ROOT).join(path)
    }
}

/// A file call in flight. A drop before the call ends leaves it to end in the run.
struct Wait {
    node: Node,
    key: u64,
    /// The future gave the result.
    taken: bool,
}

impl Future for Wait {
    type Output = Ended;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Ended> {
        let waker = log::Waker::new(cx.waker().clone());
        let (poll, unused) = lock(&self.node.shared).files().poll(self.key, waker);
        drop(unused);
        self.taken = poll.is_ready();
        poll
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        if !self.taken {
            let unused = lock(&self.node.shared).files().abandon(self.key);
            drop(unused);
        }
    }
}

/// One open file of a node. A drop closes it.
struct Descriptor {
    node: Node,
    handle: Handle,
    len: u64,
    exposed: bool,
}

impl Descriptor {
    /// Starts `call` on the path of this descriptor now, keeping `held` until it ends.
    fn submit(&self, call: Call, held: Option<Held>) -> Wait {
        let path = lock(&self.node.shared)
            .files()
            .path(self.node.node, self.handle);
        self.node.submit(path, call, held)
    }
}

impl env::files::Descriptor for Descriptor {
    fn len(&self) -> u64 {
        self.len
    }

    fn exposed(&self) -> bool {
        self.exposed
    }

    fn write_at<'a>(&'a self, offset: u64, parts: &'a [Block]) -> Request<'a, ()> {
        let bytes = parts.iter().flat_map(|part| part.iter().copied()).collect();
        let call = Call::Write {
            handle: self.handle,
            offset,
            bytes,
        };
        let held = Some(Held::Parts(parts.to_vec()));
        let wait = self.submit(call, held);
        Box::pin(async move { wait.await.result.map(drop) })
    }

    fn read_at(&self, offset: u64, into: Unique) -> Request<'_, Unique> {
        let len = u64::try_from(into.len()).expect("invariant: usize fits u64");
        let call = Call::Read {
            handle: self.handle,
            offset,
            len,
        };
        let wait = self.submit(call, Some(Held::Into(into)));
        Box::pin(async move {
            let Ended { result, held } = wait.await;
            let held = held.map(Kept::into_inner);
            let (Done::Read(bytes), Some(Held::Into(mut into))) = (result?, held)
            else {
                unreachable!("invariant: a read gives bytes and keeps its block")
            };
            into.copy_from_slice(&bytes);
            Ok(into)
        })
    }

    fn sync(&self) -> Request<'_, ()> {
        let call = Call::Sync {
            handle: self.handle,
        };
        let wait = self.submit(call, None);
        Box::pin(async move { wait.await.result.map(drop) })
    }

    fn rename<'a>(&'a self, from: &'a Path, to: &'a Path) -> Request<'a, ()> {
        let call = Call::Rename {
            handle: self.handle,
            to: to.to_path_buf(),
        };
        self.node.request(from, call, drop)
    }

    fn close(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(Close(Some(*self)))
    }

    fn remove(
        self: Box<Self>,
        path: PathBuf,
    ) -> Pin<Box<dyn Future<Output = Result<(), env::files::Error>>>> {
        let call = Call::Unlink {
            handle: self.handle,
        };
        let wait = self.node.submit(path, call, None);
        // The call holds the file until it ends, as the descriptor of `os` does.
        drop(self);
        Box::pin(async move { wait.await.result.map(drop) })
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // No drop follows the crash that released the hold: a descriptor is `!Send`,
        // no target holds a `thread_local!`, and a crash drops every task of its
        // node.
        let unused = lock(&self.node.shared)
            .files()
            .close(self.node.node, self.handle);
        drop(unused);
    }
}

/// A file of the host, read whole at its open. `File` opens it to read only, so it
/// never writes, renames, or removes it.
struct Host {
    node: Node,
    /// The key of its open, which keys its descriptor.
    key: u64,
    path: PathBuf,
    bytes: Vec<u8>,
}

impl env::files::Descriptor for Host {
    fn len(&self) -> u64 {
        u64::try_from(self.bytes.len()).expect("invariant: usize fits u64")
    }

    /// The host tree keeps no modes.
    fn exposed(&self) -> bool {
        false
    }

    fn write_at<'a>(&'a self, _: u64, _: &'a [Block]) -> Request<'a, ()> {
        unreachable!("invariant: a host file is open to read")
    }

    fn read_at(&self, offset: u64, into: Unique) -> Request<'_, Unique> {
        let call = Call::Host(Operation::ReadAt);
        let held = Some(Held::Into(into));
        let wait = self.node.submit(self.path.clone(), call, held);
        Box::pin(async move {
            let Ended { result, held } = wait.await;
            let held = held.map(Kept::into_inner);
            result?;
            let Some(Held::Into(mut into)) = held else {
                unreachable!("invariant: a read keeps its block")
            };
            let start = usize::try_from(offset).expect("invariant: File checked it");
            let bytes = self.bytes.get(start..start + into.len());
            into.copy_from_slice(bytes.expect("invariant: File checked the range"));
            Ok(into)
        })
    }

    fn sync(&self) -> Request<'_, ()> {
        Box::pin(future::ready(Ok(())))
    }

    fn rename<'a>(&'a self, _: &'a Path, _: &'a Path) -> Request<'a, ()> {
        unreachable!("invariant: a host file is open to read")
    }

    fn close(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(future::ready(()))
    }

    fn remove(
        self: Box<Self>,
        _: PathBuf,
    ) -> Pin<Box<dyn Future<Output = Result<(), Error>>>> {
        unreachable!("invariant: a host file is open to read")
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // As for `Descriptor`, no drop follows the crash that closed it.
        lock(&self.node.shared)
            .files()
            .close_host(self.node.node, self.key);
    }
}

/// The close of a descriptor, which ends when its calls end. A drop closes it at once.
struct Close(Option<Descriptor>);

impl Future for Close {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let descriptor =
            (self.0.as_ref()).expect("invariant: a close is not polled after it ends");
        let waker = log::Waker::new(cx.waker().clone());
        let (poll, unused) = lock(&descriptor.node.shared)
            .files()
            .poll_close(descriptor.handle, waker);
        drop(unused);
        if poll.is_ready() {
            // The drop closes the descriptor, which locks the state.
            drop(self.0.take());
        }
        poll
    }
}
