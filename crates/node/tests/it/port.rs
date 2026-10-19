//! A node on the real OS frees its port before it frees the lock of its data
//! directory.

use std::cell::RefCell;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use env::files::{Descriptor, Driver, Mode, Request};
use transport::{Address, Class, Code};
use types::ed25519::PrivateKey;
use types::time::Span;

use crate::common::{PATIENCE, config, files, free, recv, start, wait};
use crate::rig::Rig;

/// The private key of the node.
const KEY: PrivateKey = PrivateKey([2; 32]);

/// How long the node's load of its key takes once it holds its port. A dial waits
/// 30 s, and the node takes no session until it has its key.
const LOAD: Span = Span::from_nanos(31_000_000_000);

/// A peer holds a session when the node stops. The probe takes the lock as soon as
/// the node frees it, and binds the node's port at once, with no retry.
#[test]
fn the_port_binds_at_once_when_the_lock_is_free() {
    let rig = Rig::new();
    let (shards, threads) = (os::shards().unwrap(), os::threads().unwrap());
    let io = Rc::new(RefCell::new(Vec::new()));
    let disk = files(&rig.dir, &threads, &io);
    run(&shards, "key", move |_| async move {
        let files = env::files::Files::new(disk);
        let own = types::node::Key::from_u128(1);
        node::create_key(&files, &os::entropy(), own, KEY)
            .await
            .unwrap();
    });
    let listen = free();
    let region = node::Region::Absent;
    let slow = move |disk| Slow { disk, listen };
    let config = config(&rig, &shards, &threads, &io, listen, region, slow);
    let node = node::Node::start(config);
    // A task runs once the node takes sessions.
    let (serves, serving) = mpsc::channel();
    node.spawn(move |_| {
        serves.send(()).unwrap();
        async {}
    });
    let load = Duration::from_nanos(LOAD.nanos().unsigned_abs());
    if let Err(error) = recv(&serving, PATIENCE + load) {
        node.stop();
        panic!("the node does not serve ({error}): {:?}", node.join());
    }
    let (opened, open) = mpsc::channel();
    let (closed, close) = mpsc::channel();
    let peer = start(&shards, "peer", move |tasks| async move {
        let (client, pool) = client(tasks);
        let session = client
            .dial(KEY.public(), &[Address::Udp(listen)])
            .await
            .expect("a session");
        // The node rejects a stream only once it has the session too.
        let (mut sender, mut receiver) = session.open(Class::Command).await.unwrap();
        sender.send(pool.copy(&[0]).unwrap()).await.unwrap();
        opened.send(receiver.recv().await.err()).unwrap();
        closed.send(session.closed().await).unwrap();
    });
    let rejected = transport::Error::Reset {
        code: Code(wire::header::REJECTED),
    };
    assert_eq!(wait(&open), Some(rejected));
    node.stop();
    let disk = files(&rig.dir, &threads, &io);
    let (bound, bind) = mpsc::channel();
    let probe = start(&shards, "probe", move |_| async move {
        let files = env::files::Files::new(disk);
        let lock = lock(&files).await;
        bound
            .send(transport::Port::bind(&os::net(), listen).map(drop))
            .unwrap();
        drop(lock);
    });
    assert_eq!(wait(&bind), Ok(()));
    let peer_closed = transport::Error::PeerClosed { code: Code(0) };
    assert_eq!(wait(&close), peer_closed);
    let flush = node.flush();
    assert_eq!(node.join(), Ok(()));
    assert!(flush.wait(PATIENCE));
    for shard in [peer, probe] {
        shard.join().unwrap();
    }
    for thread in io.take() {
        thread.join().unwrap();
    }
}

/// A disk whose open of `node.key` takes [`LOAD`] longer while a socket holds
/// `listen`. The claim opens the key on the task that binds the port, before the bind,
/// so only the load waits.
struct Slow {
    disk: os::Disk,
    listen: SocketAddr,
}

impl Driver for Slow {
    fn open<'a>(
        &'a self,
        path: &'a Path,
        mode: Mode,
    ) -> Request<'a, Box<dyn Descriptor>> {
        Box::pin(async move {
            if path == Path::new("node.key") && held(self.listen) {
                os::clock().sleep(LOAD).await;
            }
            self.disk.open(path, mode).await
        })
    }

    fn open_absolute<'a>(&'a self, path: &'a Path) -> Request<'a, Box<dyn Descriptor>> {
        self.disk.open_absolute(path)
    }

    fn list<'a>(&'a self, dir: &'a Path) -> Request<'a, Vec<PathBuf>> {
        self.disk.list(dir)
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

/// Whether a socket holds `listen`. A free address stays free: the probe frees it at
/// once.
fn held(listen: SocketAddr) -> bool {
    match transport::Port::bind(&os::net(), listen) {
        Ok(_) => false,
        Err(env::net::Error::AddressInUse { .. }) => true,
        Err(error) => panic!("the probe of {listen} fails: {error}"),
    }
}

/// Takes the lock of the data directory as soon as no node holds it.
async fn lock(files: &env::files::Files) -> env::files::File {
    let clock = os::clock();
    loop {
        let mode = Mode::Create { len: 0 };
        match files.open(Path::new("lock"), mode).await {
            Err(env::files::Error::Busy { .. }) => {
                clock.sleep(Span::from_nanos(1_000)).await;
            }
            opened => return opened.expect("the lock opens"),
        }
    }
}

/// Runs shard `name` on `main` until it ends.
fn run<F: Future<Output = ()> + 'static>(
    shards: &env::shards::Shards,
    name: &str,
    main: impl FnOnce(env::tasks::Tasks) -> F + Send + 'static,
) {
    start(shards, name, main).join().unwrap();
}

/// A program's transport on a free port of `[::]`, and its pool.
fn client(tasks: env::tasks::Tasks) -> (transport::Client, Rc<block::Pool>) {
    let config = block::Config::new(1 << 20).expect("the budget fits");
    let memory = block::Heap::new(config.reservation());
    let pool = Rc::new(block::Pool::new(config, memory));
    let config = transport::client::Config {
        net: os::net(),
        clock: os::clock(),
        entropy: os::entropy(),
        tasks,
        pool: Rc::clone(&pool),
    };
    (transport::Client::new(config).unwrap(), pool)
}
