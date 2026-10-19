use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::rc::Rc;

use super::*;
use sector::address::{self, Address, FILE, LEN};

/// What a data directory that [`keyed`] made holds while a node with no region runs
/// on two cores, and after it stops.
const STOPPED: [&str; 8] = [
    "address", "budget", "lock", "name", "node.key", "shard-0", "shard-1", "shards-2",
];

/// Starts a node with no region on `host`, which [`keyed`] made, with its port at
/// `port`.
fn start(host: &sim::node::Node, port: u16) -> Node {
    Node::start(Config {
        listen: SocketAddr::new(host.addresses()[0], port),
        ..config(host, Size::MEBIBYTE, Box::new(heap))
    })
}

/// Stops `node`, runs `sim` until it ends, and gives what `join` gives.
fn stop(sim: &mut sim::Sim, node: Node) -> Result<(), Error> {
    node.stop();
    assert_eq!(sim.run(), Ok(()));
    node.join()
}

/// Runs `body` on a shard of its own on `host` while `sim` runs for a millisecond,
/// and gives what it gave. A node on `host` can run meanwhile.
fn meanwhile<T: Send + 'static, F: Future<Output = T> + 'static>(
    sim: &mut sim::Sim,
    host: &sim::node::Node,
    body: impl FnOnce(env::files::Files) -> F + Send + 'static,
) -> T {
    let out = Arc::new(Mutex::new(None));
    let (slot, own) = (Arc::clone(&out), host.clone());
    let shard = env::shards::Config {
        name: "reader".into(),
        core: None,
    };
    let started = host.shards().start(shard, move |_| async move {
        let value = body(own.files()).await;
        *slot.lock().unwrap() = Some(value);
    });
    drop(started.expect("the reader starts"));
    assert_eq!(sim.run_for(Span::MILLISECOND), Ok(()));
    let value = out.lock().unwrap().take();
    value.expect("the body ends in a millisecond")
}

/// The bytes of the file `address` of `files`, or `None` when the file is not there
/// or has no bytes, as a crash in the open that makes it can leave it.
///
/// # Panics
///
/// When the file has another length.
async fn read(files: &env::files::Files) -> Option<[u8; LEN]> {
    let file = match files.open(Path::new(FILE), env::files::Mode::Read).await {
        Err(env::files::Error::NotFound { .. }) => return None,
        file => file.expect("opens"),
    };
    if file.len() == 0 {
        return None;
    }
    assert_eq!(file.len(), LEN as u64, "a file of 99 bytes");
    let pool = block::Pool::heap(block::Config::new(4096).expect("the budget fits"));
    let into = pool.alloc(LEN).expect("a block");
    let bytes = file.read_at(0, into).await.expect("reads").to_vec();
    Some(bytes.try_into().expect("a read of 99 bytes"))
}

/// The bytes of the file `address` of `host`, as [`read`] gives them. A node on
/// `host` can run meanwhile.
fn bytes(sim: &mut sim::Sim, host: &sim::node::Node) -> Option<[u8; LEN]> {
    meanwhile(sim, host, |files| async move { read(&files).await })
}

/// The address in the file `address` of `host`, or `None` when the file is not
/// there or holds zero bytes. A node on `host` can run meanwhile.
///
/// # Panics
///
/// When the file is not one that a node wrote whole.
fn held(sim: &mut sim::Sim, host: &sim::node::Node) -> Option<Address> {
    let bytes = bytes(sim, host)?;
    (bytes != [0; LEN])
        .then(|| address::decode(&bytes).expect("a file that a node wrote"))
}

/// Makes a file that takes each free byte of the disk of `host`, as another writer
/// can.
fn fill(sim: &mut sim::Sim, host: &sim::node::Node) {
    let free = sim
        .run_on(host, |host, _| async move {
            let files = host.files();
            files.sync_dir(Path::new("")).await.expect("syncs");
            let len = files.free().await.expect("the free bytes");
            let mode = env::files::Mode::Create { len };
            let file = files.open(Path::new("other"), mode).await;
            let file = file.expect("the file of the other writer opens");
            file.sync().await.expect("syncs");
            file.close().await;
            files.sync_dir(Path::new("")).await.expect("syncs");
            files.free().await.expect("the free bytes")
        })
        .expect("the run ends");
    assert_eq!(free, 0);
}

/// The entries of the data directory of `host`, sorted. A node on `host` can run
/// meanwhile.
fn entries(sim: &mut sim::Sim, host: &sim::node::Node) -> Vec<PathBuf> {
    let mut entries = meanwhile(sim, host, |files| async move {
        files
            .list(Path::new(""))
            .await
            .expect("the directory lists")
    });
    entries.sort();
    entries
}

/// The address of a node on `host` that [`keyed`] made, with its port at `port`.
fn of(host: &sim::node::Node, port: u16) -> Address {
    Address {
        key: OWN,
        public_key: KEY.public(),
        bound: SocketAddr::new(host.addresses()[0], port),
    }
}

/// Starts a program on a new host of `sim` that dials `address`. Gives the result of
/// the dial once the run reaches it.
fn dial(
    sim: &mut sim::Sim,
    address: Address,
) -> Arc<Mutex<Option<Result<(), transport::Error>>>> {
    let peer = sim.node(sim::node::Config::default());
    let out = Arc::new(Mutex::new(None));
    let dialed = Arc::clone(&out);
    let shard = env::shards::Config {
        name: "peer".into(),
        core: None,
    };
    let own = peer.clone();
    let started = peer.shards().start(shard, move |tasks| async move {
        let config = block::Config::new(1 << 20).expect("the budget fits");
        let memory = block::Heap::new(config.reservation());
        let client = transport::Client::new(transport::client::Config {
            net: own.net(),
            clock: own.clock(),
            entropy: own.entropy(),
            tasks,
            pool: Rc::new(block::Pool::new(config, memory)),
        });
        let client = client.expect("a client");
        let at = [transport::Address::Udp(address.bound)];
        let session = client.dial(address.public_key, &at).await;
        *dialed.lock().unwrap() = Some(session.map(drop));
    });
    drop(started.expect("the peer starts"));
    out
}

/// A node that runs holds its key, the public key of its transport, and the address
/// that its port is bound at in `address`, and a program that dials them reaches it.
/// The file holds no address after a clean stop.
#[test]
fn a_running_node_holds_its_keys_and_its_bound_address() {
    for (port, stored) in [(PORT, false), (0, false), (PORT, true), (0, true)] {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = keyed(&mut sim, 2);
        let node = Node::start(Config {
            region: if stored {
                Region::Stored
            } else {
                Region::Absent
            },
            listen: SocketAddr::new(host.addresses()[0], port),
            ..config(&host, Size::MEBIBYTE, Box::new(heap))
        });
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        let found = held(&mut sim, &host).expect("a node that runs holds its address");
        if port == 0 {
            assert_ne!(found.bound.port(), 0, "stored {stored}");
        }
        let bound = if port == 0 { found.bound.port() } else { port };
        assert_eq!(found, of(&host, bound), "port {port}, stored {stored}");
        let dialed = dial(&mut sim, found);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        let dialed = dialed.lock().unwrap().take();
        assert_eq!(dialed, Some(Ok(())), "port {port}, stored {stored}");
        assert_eq!(stop(&mut sim, node), Ok(()));
        assert_eq!(held(&mut sim, &host), None);
    }
}

/// A node whose port binds at the unspecified IP serves: a program that dials the
/// host's address at that port reaches it.
#[test]
fn a_node_at_the_unspecified_ip_serves() {
    for ip in [
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    ] {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = keyed(&mut sim, 2);
        let node = Node::start(Config {
            listen: SocketAddr::new(ip, PORT),
            ..config(&host, Size::MEBIBYTE, Box::new(heap))
        });
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        let dialed = dial(&mut sim, of(&host, PORT));
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        let dialed = dialed.lock().unwrap().take();
        assert_eq!(dialed, Some(Ok(())), "{ip}");
        assert_eq!(stop(&mut sim, node), Ok(()), "{ip}");
    }
}

/// The input `address-valid` of the fuzz corpus is the file that a node holds while
/// it runs, and `address-valid-body` with its CRC32C is `address-valid`.
#[test]
fn a_running_node_holds_the_address_of_the_corpus() {
    let valid: &[u8; LEN] =
        include_bytes!("../../../../oracles/fuzz/node_identity/address-valid");
    let body =
        include_bytes!("../../../../oracles/fuzz/node_identity/address-valid-body");
    assert_eq!(sector::summed(body).as_ref(), Some(valid));
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let node = start(&host, PORT);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    assert_eq!(held(&mut sim, &host), address::decode(valid));
    assert_eq!(stop(&mut sim, node), Ok(()));
}

/// A task given that reads `address` at its first poll reads the node's address.
#[test]
fn a_task_reads_the_address_at_its_first_poll() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let node = start(&host, PORT);
    let out = Arc::new(Mutex::new(None));
    let (slot, own) = (Arc::clone(&out), host.clone());
    node.spawn(move |_| async move {
        let bytes = read(&own.files()).await;
        *slot.lock().unwrap() = Some(bytes.and_then(|bytes| address::decode(&bytes)));
    });
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    let read = out.lock().unwrap().take().expect("the task ran");
    assert_eq!(read, Some(of(&host, PORT)));
    assert_eq!(stop(&mut sim, node), Ok(()));
}

/// A clean stop writes `address` empty: the input `address-zeros` of the fuzz
/// corpus, which holds no address.
#[test]
fn a_clean_stop_writes_the_address_empty() {
    let empty: &[u8; LEN] =
        include_bytes!("../../../../oracles/fuzz/node_identity/address-zeros");
    assert_eq!(address::decode(empty), None);
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let node = start(&host, PORT);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    assert_eq!(entries(&mut sim, &host), STOPPED.map(PathBuf::from));
    assert_eq!(stop(&mut sim, node), Ok(()));
    assert_eq!(listed(&mut sim, &host, ""), STOPPED.map(PathBuf::from));
    assert_eq!(bytes(&mut sim, &host).as_ref(), Some(empty));
}

/// Whether a node holds `address` when it has run for `after`: what a process crash
/// then leaves.
fn published(after: Span) -> bool {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let node = start(&host, PORT);
    assert_eq!(sim.run_for(after), Ok(()), "at {after:?}");
    sim.crash(&host, sim::Crash::Process);
    drop(node);
    held(&mut sim, &host).is_some()
}

/// A stop at any point of the start, then a probe that takes the lock as soon as it
/// is free: `address` holds no address by then.
#[test]
fn the_lock_outlives_the_address() {
    let mut seen = [false; 2];
    for step in 0..600 {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = keyed(&mut sim, 2);
        let node = start(&host, PORT);
        let after = Span::from_nanos(step * 5_000);
        assert_eq!(sim.run_for(after), Ok(()), "at {after:?}");
        node.stop();
        let found = sim
            .run_on(&host, |host, _| async move {
                let (lock, _) = take_lock(&host).await;
                let found = read(&host.files()).await;
                drop(lock);
                found
            })
            .expect("the probe ends");
        let empty = found.is_none_or(|bytes| bytes == [0; LEN]);
        assert!(empty, "at {after:?}: {found:02x?}");
        let busy = Error::Directory(env::files::Error::Busy {
            path: PathBuf::from("lock"),
        });
        // A probe that takes the lock before the claim refuses the node.
        let joined = node.join();
        assert!(joined == Ok(()) || joined == Err(busy), "at {after:?}");
        seen[usize::from(published(after))] = true;
    }
    assert_eq!(seen, [true; 2], "a stop before and after the write");
}

/// A crash leaves `address`, and the next start writes its own in its place.
#[test]
fn a_crash_leaves_the_address_until_the_next_start_writes_its_own() {
    for crash in [sim::Crash::Process, sim::Crash::Power] {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = keyed(&mut sim, 2);
        let node = start(&host, PORT);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        sim.crash(&host, crash);
        drop(node);
        assert_eq!(held(&mut sim, &host), Some(of(&host, PORT)), "{crash:?}");
        let node = start(&host, PORT + 1);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        let next = held(&mut sim, &host);
        assert_eq!(next, Some(of(&host, PORT + 1)), "{crash:?}");
        assert_eq!(stop(&mut sim, node), Ok(()));
        assert_eq!(listed(&mut sim, &host, ""), STOPPED.map(PathBuf::from));
    }
}

/// A start makes `address` again when it has another length, as a later form or a
/// write by hand can leave it.
#[test]
fn a_start_makes_again_an_address_of_another_length() {
    for len in [0, 7, 200] {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = keyed(&mut sim, 2);
        write(&mut sim, &host, FILE, vec![1; len]);
        let node = start(&host, PORT);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        assert_eq!(held(&mut sim, &host), Some(of(&host, PORT)), "{len} bytes");
        assert_eq!(stop(&mut sim, node), Ok(()), "{len} bytes");
        assert_eq!(held(&mut sim, &host), None, "{len} bytes");
    }
}

/// A power cut at any time of a start leaves no `address` or the whole one.
#[test]
fn a_power_cut_during_a_start_leaves_no_address_or_the_whole_address() {
    let mut seen = [false; 2];
    for seed in 0..4 {
        for after in (0..4_000_000).step_by(25_000).map(Span::from_nanos) {
            let mut sim = sim::Sim::new(sim::Config {
                seed,
                ..sim::Config::default()
            });
            let host = keyed(&mut sim, 2);
            let node = start(&host, PORT);
            assert_eq!(sim.run_for(after), Ok(()), "seed {seed} at {after:?}");
            sim.crash(&host, sim::Crash::Power);
            drop(node);
            let held = held(&mut sim, &host);
            let whole = held == Some(of(&host, PORT));
            assert!(
                held.is_none() || whole,
                "seed {seed} at {after:?}: {held:?}"
            );
            seen[usize::from(whole)] = true;
        }
    }
    assert_eq!(seen, [true; 2], "a power cut before and after the write");
}

/// A failed file call of the write stops the node with its error, and the next start
/// writes.
#[test]
fn a_failed_file_call_of_the_write_stops_the_node() {
    use env::files::Operation::{Open, Sync, WriteAt};
    for (operation, text) in [
        (Open, "open of address failed with OS error 5"),
        (WriteAt, "write_at of address failed with OS error 5"),
        (Sync, "sync of address failed with OS error 5"),
    ] {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = keyed(&mut sim, 2);
        host.fail_file(Path::new(FILE), operation);
        let node = start(&host, PORT);
        assert_eq!(sim.run(), Ok(()), "{operation:?}");
        let error = Error::Directory(env::files::Error::Io {
            path: PathBuf::from(FILE),
            operation,
            code: 5,
        });
        assert_eq!(node.join(), Err(error.clone()), "{operation:?}");
        assert_eq!(
            error.to_string(),
            format!("cannot use the data directory: {text}")
        );
        assert_eq!(held(&mut sim, &host), None, "{operation:?}");
        let node = start(&host, PORT);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        assert_eq!(
            held(&mut sim, &host),
            Some(of(&host, PORT)),
            "{operation:?}"
        );
        assert_eq!(stop(&mut sim, node), Ok(()), "{operation:?}");
    }
}

/// The start does not read `address`: a read of it that fails stops no node.
#[test]
fn a_start_does_not_read_the_address() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    host.fail_file(Path::new(FILE), env::files::Operation::ReadAt);
    let node = start(&host, PORT);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    assert_eq!(stop(&mut sim, node), Ok(()));
}

/// After a crash left `address`, a start whose open or write of it fails writes it
/// empty, so no address stays once the lock drops.
#[test]
fn a_failed_write_after_a_crash_writes_the_address_empty() {
    use env::files::Operation::{Open, Sync, WriteAt};
    for operation in [Open, WriteAt, Sync] {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = keyed(&mut sim, 2);
        let node = start(&host, PORT);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        sim.crash(&host, sim::Crash::Process);
        drop(node);
        assert_eq!(
            held(&mut sim, &host),
            Some(of(&host, PORT)),
            "{operation:?}"
        );
        host.fail_file(Path::new(FILE), operation);
        let node = start(&host, PORT + 1);
        assert_eq!(sim.run(), Ok(()), "{operation:?}");
        let error = Error::Directory(env::files::Error::Io {
            path: PathBuf::from(FILE),
            operation,
            code: 5,
        });
        assert_eq!(node.join(), Err(error), "{operation:?}");
        assert_eq!(bytes(&mut sim, &host), Some([0; LEN]), "{operation:?}");
    }
}

/// The empty write at the stop makes no sync: a sync of `address` that would fail does
/// not fail the stop.
#[test]
fn the_empty_write_at_the_stop_does_not_sync() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let node = start(&host, PORT);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    host.fail_file(Path::new(FILE), env::files::Operation::Sync);
    assert_eq!(stop(&mut sim, node), Ok(()));
    assert_eq!(bytes(&mut sim, &host), Some([0; LEN]));
}

/// A hold whose write fails at its sync gives the error, does not poll `serve`, and
/// writes the file empty.
#[test]
fn a_hold_whose_write_fails_does_not_poll_serve() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    host.fail_file(Path::new(FILE), env::files::Operation::Sync);
    let address = of(&host, PORT);
    let held = sim.run_on(&host, move |host, _| async move {
        let serve = std::pin::pin!(async { panic!("serve is polled") });
        crate::address::hold(&host.files(), &address, serve).await
    });
    let error = Error::Directory(env::files::Error::Io {
        path: PathBuf::from(FILE),
        operation: env::files::Operation::Sync,
        code: 5,
    });
    assert_eq!(held, Ok(Err(error)));
    assert_eq!(bytes(&mut sim, &host), Some([0; LEN]));
}

/// A failed remove of an `address` of another length stops the node with its error,
/// and leaves the file.
#[test]
fn a_failed_remove_of_an_address_of_another_length_stops_the_node() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    write(&mut sim, &host, FILE, vec![1; 7]);
    host.fail_file(Path::new(FILE), env::files::Operation::Remove);
    let node = start(&host, PORT);
    assert_eq!(sim.run(), Ok(()));
    let error = Error::Directory(env::files::Error::Io {
        path: PathBuf::from(FILE),
        operation: env::files::Operation::Remove,
        code: 5,
    });
    assert_eq!(node.join(), Err(error));
    let len = sim.run_on(&host, |host, _| async move {
        let file = host
            .files()
            .open(Path::new(FILE), env::files::Mode::Read)
            .await;
        file.expect("the file stays").len()
    });
    assert_eq!(len, Ok(7));
}

/// A task given whose drop panics, on a node whose write of `address` fails: `join`
/// ranks the error of the write above the panic.
#[test]
fn a_panic_after_a_failed_write_gives_the_error_of_the_write() {
    struct Panics;
    impl Drop for Panics {
        fn drop(&mut self) {
            panic!("a task's drop panics");
        }
    }
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    host.fail_file(Path::new(FILE), env::files::Operation::Open);
    let node = start(&host, PORT);
    let panics = Panics;
    node.spawn(move |_| async move { drop(panics) });
    assert_eq!(
        sim.run(),
        Err(sim::Error::Panicked {
            thread: "shard-0".into(),
            message: "a task's drop panics".into(),
            seed: 0,
        })
    );
    assert_eq!(sim.run(), Ok(()));
    let error = Error::Directory(env::files::Error::Io {
        path: PathBuf::from(FILE),
        operation: env::files::Operation::Open,
        code: 5,
    });
    assert_eq!(node.join(), Err(error));
}

/// A sync of the data directory that fails at any time of a start stops the node,
/// which holds no address in `address` once it has ended. The last sync of a start
/// is the one of the write.
#[test]
fn a_failed_sync_of_the_data_directory_leaves_no_address() {
    let sync = env::files::Error::Io {
        path: PathBuf::new(),
        operation: env::files::Operation::SyncDir,
        code: 5,
    };
    let mut last = None;
    for step in 0..600 {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = keyed(&mut sim, 2);
        let node = start(&host, PORT);
        let after = Span::from_nanos(step * 5_000);
        assert_eq!(sim.run_for(after), Ok(()), "at {after:?}");
        host.fail_file(Path::new(""), env::files::Operation::SyncDir);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()), "at {after:?}");
        let joined = stop(&mut sim, node);
        let files = ::buffer::Error::Files(sync.clone());
        let opened = |core| Error::Buffer {
            core,
            error: files.clone(),
        };
        let failed = [Error::Directory(sync.clone()), opened(0), opened(1)];
        match &joined {
            Ok(()) => {}
            Err(error) => {
                assert!(failed.contains(error), "at {after:?}: {error:?}");
                last = Some(error.clone());
            }
        }
        assert_eq!(held(&mut sim, &host), None, "at {after:?}");
        if step == 599 {
            assert_eq!(joined, Ok(()), "a start with no sync left");
        }
    }
    assert_eq!(last, Some(Error::Directory(sync)), "the sync of the write");
}

/// An empty write of `address` that fails at the stop gives its error at `join` and
/// leaves the address, and the next start writes its own.
#[test]
fn a_failed_empty_write_at_the_stop_fails_the_node() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let node = start(&host, PORT);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    host.fail_file(Path::new(FILE), env::files::Operation::WriteAt);
    let error = Error::Directory(env::files::Error::Io {
        path: PathBuf::from(FILE),
        operation: env::files::Operation::WriteAt,
        code: 5,
    });
    assert_eq!(stop(&mut sim, node), Err(error));
    assert_eq!(held(&mut sim, &host), Some(of(&host, PORT)));
    let node = start(&host, PORT + 1);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    assert_eq!(held(&mut sim, &host), Some(of(&host, PORT + 1)));
    assert_eq!(stop(&mut sim, node), Ok(()));
    assert_eq!(held(&mut sim, &host), None);
}

/// An empty write of `address` that fails after the transport stopped the node:
/// `join` gives the error of the transport.
#[test]
fn a_failed_empty_write_after_the_transport_stops_gives_the_transport_error() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    host.fail_file(Path::new(FILE), env::files::Operation::WriteAt);
    host.fail_udp(listen(&host));
    assert_eq!(sim.run(), Ok(()));
    let error = transport::Error::Network {
        error: env::net::Error::Io { code: 5 },
    };
    assert_eq!(node.join(), Err(Error::Transport(error)));
    assert_eq!(
        held(&mut sim, &host),
        Some(of(&host, PORT)),
        "the empty write failed"
    );
}

/// A node whose rings and `address` are made starts again after another writer
/// filled the disk.
#[test]
fn a_restart_on_a_full_disk_serves() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let node = start(&host, PORT);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    assert_eq!(stop(&mut sim, node), Ok(()));
    fill(&mut sim, &host);
    let node = start(&host, PORT);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    assert_eq!(held(&mut sim, &host), Some(of(&host, PORT)));
    assert_eq!(stop(&mut sim, node), Ok(()));
}

/// After a crash leaves `address`, a start on a disk that another writer filled
/// writes its own address over the old one.
#[test]
fn a_restart_after_a_crash_on_a_full_disk_serves() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let node = start(&host, PORT);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    sim.crash(&host, sim::Crash::Process);
    drop(node);
    assert_eq!(held(&mut sim, &host), Some(of(&host, PORT)));
    fill(&mut sim, &host);
    let node = start(&host, PORT + 1);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    assert_eq!(held(&mut sim, &host), Some(of(&host, PORT + 1)));
    assert_eq!(stop(&mut sim, node), Ok(()));
}

/// A start whose make of `address` finds no room on the disk stops the node with
/// [`env::files::Error::Full`] before it serves.
#[test]
fn a_start_with_no_room_for_the_address_fails() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let node = start(&host, PORT);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    assert_eq!(stop(&mut sim, node), Ok(()));
    sim.run_on(&host, |host, _| async move {
        let files = host.files();
        files.remove(Path::new(FILE)).await.expect("removes");
        files.sync_dir(Path::new("")).await.expect("syncs");
    })
    .expect("the run ends");
    fill(&mut sim, &host);
    let node = start(&host, PORT);
    assert_eq!(sim.run(), Ok(()));
    let full = env::files::Error::Full {
        path: PathBuf::from(FILE),
    };
    assert_eq!(node.join(), Err(Error::Directory(full)));
    assert_eq!(bytes(&mut sim, &host), None);
}
