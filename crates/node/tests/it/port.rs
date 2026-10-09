//! A node on the real OS frees its port before it frees the lock of its data
//! directory.

use std::cell::RefCell;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use transport::{Address, Class, Code};
use types::byte::Size;
use types::ed25519::PrivateKey;
use types::time::Span;

use crate::rig::Rig;

/// The private key of the node.
const KEY: PrivateKey = PrivateKey([2; 32]);

/// How long the test waits for a step of another thread.
const PATIENCE: Duration = Duration::from_secs(60);

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
        budget: Size::from_bytes(cores * (512 << 10)),
        memory: Box::new(|len| Ok(block::Heap::new(len))),
        files: Box::new(move || {
            let disk = files(&dir, &threads, &io);
            Box::new(move || env::files::Files::new(disk))
        }),
        entropy: os::entropy(),
        disk: Size::from_bytes(cores * (8 << 20)),
        net: os::net(),
        listen,
        region: None,
    })
}

/// Takes the lock of the data directory as soon as no node holds it.
async fn lock(files: &env::files::Files) -> env::files::File {
    let clock = os::clock();
    loop {
        let mode = env::files::Mode::Create { len: 0 };
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
fn wait<T>(receiver: &mpsc::Receiver<T>) -> T {
    #[expect(
        clippy::disallowed_methods,
        reason = "the test thread waits on a shard in real time"
    )]
    receiver
        .recv_timeout(PATIENCE)
        .expect("the shard sends within 60 s")
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

/// A program's transport on a free port of loopback, and its pool.
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
