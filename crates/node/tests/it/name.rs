//! A read of the name while a node starts on the real OS.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use env::files::{Descriptor, Driver, Mode, Request};
use types::time::Span;

use crate::common::{PATIENCE, config, files, free, start, wait};
use crate::rig::Rig;

/// A read of the name that reads the directory of the mesh only after a first start
/// founds a region gives the name. It reads the founding before `node.key`, which the
/// start makes durable first.
#[test]
fn a_read_that_spans_a_first_start_gives_the_name() {
    let rig = Rig::new();
    let (shards, threads) = (os::shards().unwrap(), os::threads().unwrap());
    let io = Rc::new(RefCell::new(Vec::new()));
    let (holds, held) = mpsc::channel();
    let released = Arc::new(AtomicBool::new(false));
    let disk = Held {
        disk: files(&rig.dir, &threads, &io),
        holds,
        released: Arc::clone(&released),
    };
    let (reads, read) = mpsc::channel();
    let reader = start(&shards, "reader", move |_| async move {
        let files = env::files::Files::new(disk);
        reads.send(node::name(&files, None).await).unwrap();
    });
    wait(&held);
    let region = node::Region::Stored;
    let config = config(&rig, &shards, &threads, &io, free(), region, |disk| disk);
    let node = node::Node::start(config);
    let (serves, serving) = mpsc::channel();
    node.spawn(move |_| {
        serves.send(()).unwrap();
        async {}
    });
    wait(&serving);
    node.stop();
    let flush = node.flush();
    assert_eq!(node.join(), Ok(()));
    assert!(flush.wait(PATIENCE));
    released.store(true, Ordering::Release);
    assert_eq!(wait(&read), Ok("edge".parse().unwrap()));
    reader.join().unwrap();
    assert_eq!(held.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    for thread in io.take() {
        thread.join().unwrap();
    }
}

/// `foundation profile` takes no lock. A read of the public key that reads `node.key`
/// before a first start and the directory `mesh` after it must not say that the key
/// is lost: the start made `node.key` durable before `mesh`.
#[test]
fn a_public_key_read_that_spans_a_first_start_is_not_lost() {
    let rig = Rig::new();
    let (shards, threads) = (os::shards().unwrap(), os::threads().unwrap());
    let io = Rc::new(RefCell::new(Vec::new()));
    let (holds, held) = mpsc::channel();
    let released = Arc::new(AtomicBool::new(false));
    let disk = Held {
        disk: files(&rig.dir, &threads, &io),
        holds,
        released: Arc::clone(&released),
    };
    let (reads, read) = mpsc::channel();
    let reader = start(&shards, "reader", move |_| async move {
        let files = env::files::Files::new(disk);
        reads.send(node::public_key(&files).await).unwrap();
    });
    wait(&held);
    let region = node::Region::Stored;
    let config = config(&rig, &shards, &threads, &io, free(), region, |disk| disk);
    let node = node::Node::start(config);
    let (serves, serving) = mpsc::channel();
    node.spawn(move |_| {
        serves.send(()).unwrap();
        async {}
    });
    wait(&serving);
    node.stop();
    let flush = node.flush();
    assert_eq!(node.join(), Ok(()));
    assert!(flush.wait(PATIENCE));
    released.store(true, Ordering::Release);
    let result = wait(&read);
    assert!(
        matches!(result, Ok(_) | Err(node::Error::Unstarted)),
        "{result:?}"
    );
    reader.join().unwrap();
    for thread in io.take() {
        thread.join().unwrap();
    }
}

/// A disk whose list of the directory `mesh` waits until `released` is set. Sends on
/// `holds` at each such list.
struct Held {
    disk: os::Disk,
    holds: mpsc::Sender<()>,
    released: Arc<AtomicBool>,
}

impl Driver for Held {
    fn open<'a>(
        &'a self,
        path: &'a Path,
        mode: Mode,
    ) -> Request<'a, Box<dyn Descriptor>> {
        self.disk.open(path, mode)
    }

    fn open_absolute<'a>(&'a self, path: &'a Path) -> Request<'a, Box<dyn Descriptor>> {
        self.disk.open_absolute(path)
    }

    fn list<'a>(&'a self, dir: &'a Path) -> Request<'a, Vec<PathBuf>> {
        Box::pin(async move {
            if dir == Path::new("mesh") {
                self.holds.send(()).unwrap();
                while !self.released.load(Ordering::Acquire) {
                    os::clock().sleep(Span::from_nanos(1_000_000)).await;
                }
            }
            self.disk.list(dir).await
        })
    }

    fn create_dir<'a>(&'a self, dir: &'a Path) -> Request<'a, ()> {
        self.disk.create_dir(dir)
    }

    fn remove<'a>(&'a self, path: &'a Path) -> Request<'a, ()> {
        self.disk.remove(path)
    }

    fn sync_dir<'a>(&'a self, dir: &'a Path) -> Request<'a, ()> {
        self.disk.sync_dir(dir)
    }

    fn free(&self) -> Request<'_, u64> {
        self.disk.free()
    }

    fn shown(&self, path: &Path) -> PathBuf {
        self.disk.shown(path)
    }
}
