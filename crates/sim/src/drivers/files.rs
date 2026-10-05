//! The `env::files` drivers of a simulated node.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use block::{Block, Unique};
use env::files::{Mode, Request};

use super::Node;
use crate::disk::Handle;
use crate::files::{Call, Done, Ended, Held};
use crate::state::lock;

impl Node {
    /// Starts `call` on `path` now, keeping `held` until it ends.
    ///
    /// # Panics
    ///
    /// Outside a thread that the sim started, and on a thread of another node.
    fn submit(&self, path: &Path, call: Call, held: Option<Held>) -> Wait {
        self.running("a file call");
        let key = lock(&self.shared).submit(self.node, path, call, held);
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
        let wait = self.submit(path, call, None);
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
            let Done::Open { handle, len } = done else {
                unreachable!("invariant: an open gives a file")
            };
            let node = self.clone();
            let path = path.to_path_buf();
            let descriptor: Box<dyn env::files::Descriptor> = Box::new(Descriptor {
                node,
                path,
                handle,
                len,
            });
            descriptor
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
        let waker = cx.waker().clone();
        let (poll, unused) = lock(&self.node.shared).files().poll(self.key, waker);
        drop(unused);
        self.taken = poll.is_ready();
        poll
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        if !self.taken {
            let (node, key) = (self.node.node, self.key);
            let unused = lock(&self.node.shared).files().abandon(node, key);
            drop(unused);
        }
    }
}

/// One open file of a node. A drop closes it.
struct Descriptor {
    node: Node,
    /// The path that opened it.
    path: PathBuf,
    handle: Handle,
    len: u64,
}

impl env::files::Descriptor for Descriptor {
    fn len(&self) -> u64 {
        self.len
    }

    fn write_at<'a>(&'a self, offset: u64, parts: &'a [Block]) -> Request<'a, ()> {
        let bytes = parts.iter().flat_map(|part| part.iter().copied()).collect();
        let call = Call::Write {
            handle: self.handle,
            offset,
            bytes,
        };
        let held = Some(Held::Parts(parts.to_vec()));
        let wait = self.node.submit(&self.path, call, held);
        Box::pin(async move { wait.await.result.map(drop) })
    }

    fn read_at(&self, offset: u64, into: Unique) -> Request<'_, Unique> {
        let len = u64::try_from(into.len()).expect("invariant: usize fits u64");
        let call = Call::Read {
            handle: self.handle,
            offset,
            len,
        };
        let wait = self.node.submit(&self.path, call, Some(Held::Into(into)));
        Box::pin(async move {
            let Ended { result, held } = wait.await;
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
        self.node.request(&self.path, call, drop)
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        lock(&self.node.shared)
            .files()
            .close(self.node.node, self.handle);
    }
}
