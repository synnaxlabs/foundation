//! A node on the real OS frees its port before it frees the lock of its data
//! directory.

use std::cell::RefCell;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use env::files::{Descriptor, Driver, Mode, Request};
use transport::{Address, Class, Code};
use types::byte::Size;
use types::ed25519::PrivateKey;
use types::time::Span;

use crate::rig::Rig;

/// The private key of the node.
const KEY: PrivateKey = PrivateKey([2; 32]);

/// How long the test waits for a step of another thread.
const PATIENCE: Duration = Duration::from_secs(60);

/// How long the node's open of its key takes. A dial waits 30 s, and the node takes
/// no session until it has its key.
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
        node::create_key(&files, own, KEY).await.unwrap();
    });
    let listen = free();
    let node = start_node(&rig, &shards, &threads, &io, listen);
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
    assert_eq!(node.join(), Ok(()));
    for shard in [peer, probe] {
        shard.join().unwrap();
    }
    for thread in io.take() {
        thread.join().unwrap();
    }
}

/// Starts a node on the real OS in `rig`, which listens on `listen`.
fn start_node(
    rig: &Rig,
    shards: &env::shards::Shards,
    threads: &env::threads::Threads,
    io: &Rc<RefCell<Vec<env::thread::Handle>>>,
    listen: SocketAddr,
) -> node::Node {
    let cores = u64::try_from(shards.cores().get()).unwrap();
    let (dir, threads, io) = (rig.dir.clone(), threads.clone(), Rc::clone(io));
    node::Node::start(node::Config {
        shards: shards.clone(),
        clock: os::clock(),
        wall: os::wall().unwrap(),
        budget: node::Budget {
            pool: Size::from_bytes(cores * (512 << 10)),
            disk: Size::from_bytes(cores * (8 << 20)),
        },
        memory: Box::new(|len| Ok(block::Heap::new(len))),
        files: Box::new(move || {
            let disk = Slow(files(&dir, &threads, &io));
            Box::new(move || env::files::Files::new(disk))
        }),
        entropy: os::entropy(),
        net: os::net(),
        listen,
        region: None,
        name: "edge".parse().expect("a name"),
    })
}

/// A disk whose open of `node.key` takes [`LOAD`] longer.
struct Slow(os::Disk);

impl Driver for Slow {
    fn open<'a>(
        &'a self,
        path: &'a Path,
        mode: Mode,
    ) -> Request<'a, Box<dyn Descriptor>> {
        Box::pin(async move {
            if path == Path::new("node.key") {
                os::clock().sleep(LOAD).await;
            }
            self.0.open(path, mode).await
        })
    }

    fn list<'a>(&'a self, dir: &'a Path) -> Request<'a, Vec<PathBuf>> {
        self.0.list(dir)
    }

    fn create_dir<'a>(&'a self, dir: &'a Path) -> Request<'a, ()> {
        self.0.create_dir(dir)
    }

    fn remove<'a>(&'a self, path: &'a Path) -> Request<'a, ()> {
        self.0.remove(path)
    }

    fn sync_dir<'a>(&'a self, dir: &'a Path) -> Request<'a, ()> {
        self.0.sync_dir(dir)
    }

    fn free(&self) -> Request<'_, u64> {
        self.0.free()
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

/// What a shard sends on `receiver`.
///
/// # Panics
///
/// When it sends nothing within 60 s.
#[track_caller]
fn wait<T>(receiver: &mpsc::Receiver<T>) -> T {
    recv(receiver, PATIENCE).expect("the shard sends within 60 s")
}

/// What a shard sends on `receiver` within `timeout`.
///
/// # Errors
///
/// When it sends nothing within `timeout`, or the shard drops its sender.
fn recv<T>(
    receiver: &mpsc::Receiver<T>,
    timeout: Duration,
) -> Result<T, mpsc::RecvTimeoutError> {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test thread waits on a shard in real time"
    )]
    receiver.recv_timeout(timeout)
}

/// The disk of `dir` for one shard. `io` gets the handle of its I/O thread.
fn files(
    dir: &Path,
    threads: &env::threads::Threads,
    io: &RefCell<Vec<env::thread::Handle>>,
) -> os::Disk {
    let mut io = io.borrow_mut();
    let name = format!("io-{}", io.len());
    let (disk, thread) = os::files(dir, threads, &name).unwrap();
    io.push(thread);
    disk
}

/// A free UDP port of loopback.
fn free() -> SocketAddr {
    let at = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let port = transport::Port::bind(&os::net(), at).unwrap();
    let [Address::Udp(free)] = port.addresses()[..] else {
        panic!("invariant: a port has one UDP socket");
    };
    free
}

/// Starts shard `name` on `main`.
fn start<F: Future<Output = ()> + 'static>(
    shards: &env::shards::Shards,
    name: &str,
    main: impl FnOnce(env::tasks::Tasks) -> F + Send + 'static,
) -> env::thread::Handle {
    let config = env::shards::Config {
        name: name.into(),
        core: None,
    };
    shards.start(config, main).unwrap()
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
    let config = block::Config { budget: 1 << 20 };
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
