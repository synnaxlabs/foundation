use std::cell::Cell;
use std::fs::Permissions;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::num::{NonZeroU32, NonZeroUsize};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use tokio::runtime::Builder;
use tracing::span::{Attributes, Id, Record};
use tracing::{Dispatch, Event, Metadata, Subscriber};
use types::time::Span;

use super::*;

#[test]
fn a_client_moves_to_and_is_shared_between_threads() {
    fn shared<T: Send + Sync>() {}
    shared::<Client>();
}

/// The entropy of a simulated node of the default seed: the same bytes each time.
fn fixed() -> env::entropy::Entropy {
    let mut sim = sim::Sim::new(sim::Config::default());
    sim.node(sim::node::Config::default()).entropy()
}

/// `private_key` reads the OS, so only `drawn` takes fixed bytes; the test pins it as
/// `laptop.architect-2` ruled (2026-10-19T23:47:10Z,
/// <https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6279865842>).
#[test]
fn a_private_key_is_the_first_32_bytes_of_the_entropy() {
    let mut bytes = [0; 32];
    fixed().fill(&mut bytes);
    assert_eq!(drawn(&fixed()).0, bytes);
}

#[test]
fn each_private_key_is_new() {
    assert_ne!(private_key().0, private_key().0);
}

/// A transport of the node of `private_key` at `at` on `net`, `clock`, `entropy`, and
/// `tasks`, with an idle timeout of 60 s, and the address where it is bound.
fn listening(
    private_key: PrivateKey,
    at: SocketAddr,
    net: &env::net::Net,
    clock: env::clock::Clock,
    entropy: env::entropy::Entropy,
    tasks: Tasks,
) -> (transport::Transport, SocketAddr) {
    let pool = block::Config::new(1 << 20).expect("the budget fits");
    let memory = block::Heap::new(pool.reservation());
    let config = transport::Config {
        private_key,
        message_bytes_max: NonZeroUsize::new(1 << 16).expect("not zero"),
        window_bytes: 1 << 20,
        streams_max: NonZeroU32::new(16).expect("not zero"),
        idle: Span::from_nanos(60 * Span::SECOND.nanos()),
        clock,
        entropy,
        tasks,
        pool: Rc::new(block::Pool::new(pool, memory)),
    };
    let bound_port = transport::Port::bind(net, at).expect("a port");
    let transport::Address::Udp(at) = bound_port.addresses()[0] else {
        unreachable!("a UDP port");
    };
    let part = bound_port.split(NonZeroUsize::MIN).pop().expect("one part");
    let transport = transport::Transport::new(config, part).expect("a transport");
    (transport, at)
}

/// The node sees the close of an answer at once, not at the end of its idle timeout.
#[test]
fn answering_closes_its_session_with_code_0_on_the_os() {
    let server_key = PrivateKey([2; 32]);
    let proved = server_key.public();
    let (bound, address) = mpsc::channel();
    let (sent, ended) = mpsc::channel();
    let shards = os::shards().expect("shards");
    let config = env::shards::Config {
        name: "server".into(),
        core: None,
    };
    let _server = shards
        .start(config, move |tasks| async move {
            let clock = os::clock();
            let at = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
            let (transport, at) = listening(
                server_key,
                at,
                &os::net(),
                clock.clone(),
                os::entropy(),
                tasks,
            );
            bound.send(at).expect("the test keeps the receiver");
            let session = transport.accept().await.expect("a session");
            let start = clock.now();
            let error = session.closed().await;
            sent.send((error, clock.now() - start))
                .expect("the test keeps the receiver");
        })
        .expect("the server starts");
    #[expect(clippy::disallowed_methods, reason = "the test waits for the server")]
    let at = address.recv().expect("the server binds");
    let node = Node::answering(vec![at]).expect("an answer");
    assert_eq!(node.public_key, proved);
    #[expect(clippy::disallowed_methods, reason = "the test waits for the server")]
    let ended = ended.recv_timeout(Duration::from_secs(5));
    let (error, waited) = ended.expect("the server saw the end");
    assert_eq!(
        error,
        transport::Error::PeerClosed {
            code: transport::Code(0)
        }
    );
    assert!(waited < Span::SECOND, "{waited:?}");
}

/// Starts a shard named `name`, with no core, on `node` that runs `main`.
fn start<F: Future<Output = ()> + 'static>(
    node: &sim::node::Node,
    name: &str,
    main: impl FnOnce(Tasks) -> F + Send + 'static,
) {
    let config = env::shards::Config {
        name: name.into(),
        core: None,
    };
    drop(node.shards().start(config, main).expect("a shard"));
}

/// In each run where the links lose 10% of datagrams, a node that accepted the
/// session of an answer sees its close within a second, not at the end of its idle
/// timeout: while the close drains, a packet of the node makes the answer send the
/// close again. The answer gives the key of the node. A run with a lost client Finished
/// has no accept, as the close ends the handshake of the node.
#[test]
fn a_node_that_accepted_an_answer_sees_its_close_when_a_close_is_lost() {
    let link = sim::link::Config {
        loss: 0.1,
        ..sim::link::Config::default()
    };
    let mut accepts = 0;
    for value in 0..64 {
        let mut sim = sim::Sim::new(sim::Config {
            seed: value,
            ..sim::Config::default()
        });
        let program = sim.node(sim::node::Config::default());
        let node = sim.node(sim::node::Config::default());
        sim.link(&program, &node, link);
        sim.link(&node, &program, link);
        let at = SocketAddr::new(node.addresses()[0], 4100);
        // `Some(None)` from the accept to the end.
        let ended = Arc::new(Mutex::new(None));
        let (slot, own) = (Arc::clone(&ended), node.clone());
        start(&node, "node", move |tasks| async move {
            let clock = own.clock();
            let key = PrivateKey([2; 32]);
            let (transport, _) =
                listening(key, at, &own.net(), clock.clone(), own.entropy(), tasks);
            let session = transport.accept().await.expect("a session");
            *slot.lock().expect("no panic under the lock") = Some(None);
            let start = clock.now();
            let error = session.closed().await;
            let end = Some((error, clock.now() - start));
            *slot.lock().expect("no panic under the lock") = Some(end);
        });
        let answer = Arc::new(Mutex::new(None));
        let (slot, own) = (Arc::clone(&answer), program.clone());
        start(&program, "program", move |tasks| async move {
            let config = transport::client::Config {
                net: own.net(),
                clock: own.clock(),
                entropy: own.entropy(),
                tasks,
                pool: Rc::new(block::Pool::heap(POOL)),
            };
            let key = answered(&[transport::Address::Udp(at)], config).await;
            *slot.lock().expect("no panic under the lock") = Some(key);
        });
        let run = sim.run_for(Span::from_nanos(10 * Span::SECOND.nanos()));
        run.expect("the run ends");
        let accepted = ended.lock().expect("no panic under the lock").take();
        if let Some(end) = accepted {
            accepts += 1;
            let (error, waited) = end.unwrap_or_else(|| panic!("run {value}: no end"));
            let closed = transport::Error::PeerClosed {
                code: transport::Code(0),
            };
            assert_eq!(error, closed, "run {value}");
            assert!(waited < Span::SECOND, "run {value}: {waited:?}");
            let key = answer.lock().expect("no panic under the lock").take();
            let public_key = PrivateKey([2; 32]).public();
            assert_eq!(key, Some(Ok(public_key)), "run {value}");
        }
    }
    assert!(accepts >= 48, "{accepts} of 64 runs accepted");
}

#[test]
fn the_pool_holds_a_datagram() {
    let pool = block::Pool::heap(POOL);
    assert!(pool.largest() >= 1472, "{}", pool.largest());
}

#[test]
fn the_frames_pool_holds_a_frame_of_32_kib() {
    let pool = block::Pool::heap(FRAMES);
    assert!(pool.largest() >= 32 << 10, "{}", pool.largest());
}

#[test]
fn the_reads_pool_holds_a_frame_of_7_mib_in_504_mib_of_address_space() {
    let pool = reads().unwrap();
    assert_eq!(pool.largest(), 7_340_032);
    assert_eq!(READS.reservation(), (504 << 20) + 64);
}

/// The whole pages of the bytes at `range` that are in memory, from
/// `/proc/self/pagemap`.
#[cfg(target_os = "linux")]
fn present(range: std::ops::Range<*const u8>) -> usize {
    use std::os::unix::fs::FileExt;
    const PAGE: usize = 4096;
    #[expect(
        clippy::disallowed_methods,
        reason = "the test reads its pages in /proc"
    )]
    let pagemap = std::fs::File::open("/proc/self/pagemap").unwrap();
    (range.start.addr().div_ceil(PAGE)..range.end.addr() / PAGE)
        .filter(|page| {
            let mut entry = [0; 8];
            let at = u64::try_from(page * 8).unwrap();
            pagemap.read_exact_at(&mut entry, at).unwrap();
            u64::from_le_bytes(entry) >> 63 == 1
        })
        .count()
}

/// Only Linux: the test counts the pages of a frame in `/proc/self/pagemap`, which
/// macOS does not have. Frames of one size fill the budget, then frames of another
/// size need it.
#[cfg(target_os = "linux")]
#[test]
fn the_reads_pool_gives_back_the_pages_of_a_size_that_another_size_needs() {
    let pool = reads().unwrap();
    let filled = |len| {
        let mut frames = Vec::new();
        while let Ok(mut frame) = pool.alloc(len) {
            frame.fill(0xa5);
            frames.push(frame);
        }
        frames
    };
    let first = filled(1_000_000);
    let ranges: Vec<_> = first.iter().map(|frame| frame.as_ptr_range()).collect();
    let pages: Vec<_> = ranges.iter().cloned().map(present).collect();
    assert_eq!(pages, [243; 7]);
    drop(first);
    let second = filled(800_000);
    assert!(second.len() >= 9, "{}", second.len());
    let kept: Vec<_> = ranges.into_iter().map(present).collect();
    assert_eq!(kept, [0; 7]);
}

#[test]
fn each_error_names_its_cause() {
    let thread = Error::Thread {
        reason: "out of memory".into(),
    };
    let bind = Error::Bind(transport::Error::TimedOut);
    let session = Error::Session(program::Error::Unanswered);
    let stream = Error::Stream(program::Error::Unanswered);
    let local = Error::Local(sector::local::Error::NotRunning);
    let value = Error::Value {
        place: 2,
        scalar: Scalar::F64,
        data_type: Type::Scalar(Scalar::I64),
    };
    let errors = [
        thread,
        bind,
        session,
        stream,
        local,
        Error::Panicked,
        value,
        Error::Closed,
        Error::Decode {
            place: 3,
            cause: codec::Error::Tag { vector: 1, tag: 9 },
        },
        Error::Memory(os::memory::Error::Reserve {
            len: 4096,
            code: 12,
        }),
        Error::Memory(os::memory::Error::Refused),
        Error::Partial {
            written: vec!["cell.c".parse().unwrap(), "cell.time".parse().unwrap()],
            unwritten: vec!["plant.a".parse().unwrap(), "plant.b".parse().unwrap()],
            outcomes: Vec::new(),
        },
        Error::Partial {
            written: Vec::new(),
            unwritten: vec!["plant.a".parse().unwrap()],
            outcomes: Vec::new(),
        },
    ];
    assert_eq!(
        errors.map(|e| e.to_string()),
        [
            "cannot start the client thread: out of memory",
            "cannot bind the local UDP socket: the peer stopped answering",
            "the node finished a stream with no answer",
            "the node finished a stream with no answer",
            "no node runs on the data directory",
            "the client thread panicked",
            "place 2 holds i64, not f64",
            "the client is closed",
            "the series of place 3 does not decode: vector 1 has tag 9, which this sample \
             type does not use",
            "the client cannot reserve 4096 bytes of address space for the frames of its \
             readers: Cannot allocate memory (os error 12); raise the address-space limit \
             of the process",
            "the OS refused memory for the frames of the client's readers; free memory or \
             raise the commit limit of the system",
            "channels plant.a, plant.b were not written; channels cell.c, cell.time \
             were",
            "channels plant.a were not written; no channel was",
        ]
    );
}

// These tests build the client through the private `Client::start`, with a `main`
// that they control. `connect` is `start` with `run` as its `main`. A client that
// connects needs a node, and these tests run none. No input of `connect` makes a
// correct client's thread panic, show when it ends, or log a given event.

/// A client whose thread sends `Ok`, waits for its end, and then runs `after`.
fn client(after: impl FnOnce() -> Result<(), Error> + Send + 'static) -> Client {
    let started = Client::start(|_, outcome, ended, _| async move {
        outcome.send(Ok(())).unwrap();
        drop(ended.await);
        if let Err(error) = after() {
            outcome.send(Err(error)).unwrap();
        }
    });
    started.expect("a client")
}

#[test]
fn a_close_ends_the_thread_and_waits_for_it() {
    let (ended, end) = mpsc::channel();
    let client = client(move || {
        ended.send(()).unwrap();
        Ok(())
    });
    assert_eq!(client.close(), Ok(()));
    assert_eq!(end.try_recv(), Ok(()));
}

#[test]
fn an_open_of_a_writer_sends_its_config_to_the_thread() {
    let (sent, received) = mpsc::channel();
    let started = Client::start(|_, outcome, ended, mut opens| async move {
        outcome.send(Ok(())).unwrap();
        let open = opens.recv().await.unwrap();
        sent.send(format!("{open:?}")).unwrap();
        drop(open);
        drop(ended.await);
    });
    let client = started.expect("a client");
    let config = write::Config::new(vec!["count".parse().unwrap()]);
    assert_eq!(client.open_writer(config).map(drop), Err(Error::Panicked));
    let open = received.try_recv().unwrap();
    assert!(open.contains("count"), "{open}");
    assert_eq!(client.close(), Ok(()));
}

#[test]
fn an_open_of_a_writer_after_the_thread_panicked_gives_panicked() {
    let started = Client::start(|_, outcome, _, _| async move {
        outcome.send(Ok(())).unwrap();
        panic!("the session failed");
    });
    let client = started.expect("a client");
    let config = write::Config::new(vec!["count".parse().unwrap()]);
    assert_eq!(client.open_writer(config).map(drop), Err(Error::Panicked));
    assert_eq!(client.close(), Err(Error::Panicked));
}

#[test]
fn a_close_gives_the_error_that_ended_the_session() {
    let failed = Error::Session(program::Error::Unanswered);
    let error = failed.clone();
    let client = client(move || Err(error));
    assert_eq!(client.close(), Err(failed));
}

#[test]
fn a_close_gives_panicked_when_the_thread_panicked() {
    let client = client(|| panic!("the session failed"));
    assert_eq!(client.close(), Err(Error::Panicked));
}

#[test]
fn a_close_gives_panicked_when_a_task_panicked() {
    let started = Client::start(|tasks, outcome, _, _| async move {
        outcome.send(Ok(())).unwrap();
        tasks.spawn(async { panic!("a task failed") });
        std::future::pending::<()>().await;
    });
    let client = started.expect("a client");
    assert_eq!(client.close(), Err(Error::Panicked));
}

#[test]
fn a_drop_ends_the_thread_and_does_not_wait() {
    let (go, wait) = mpsc::channel::<()>();
    let (ended, end) = mpsc::channel();
    let client = client(move || {
        #[expect(clippy::disallowed_methods, reason = "the thread waits for the test")]
        wait.recv().unwrap();
        ended.send(()).unwrap();
        Ok(())
    });
    drop(client);
    go.send(()).unwrap();
    #[expect(clippy::disallowed_methods, reason = "the test waits for the thread")]
    let outcome = end.recv_timeout(Duration::from_secs(10));
    assert_eq!(outcome, Ok(()));
}

#[test]
fn a_connect_gives_panicked_when_the_thread_panics_after_its_error() {
    let started = Client::start(|_, outcome, _, _| async move {
        let error = Error::Session(program::Error::Unanswered);
        outcome.send(Err(error)).unwrap();
        panic!("the session failed");
    });
    assert_eq!(started.err(), Some(Error::Panicked));
}

#[test]
fn a_connect_gives_panicked_when_the_thread_panics_before_its_outcome() {
    let started = Client::start(|_, _, _, _| async { panic!("the session failed") });
    assert_eq!(started.err(), Some(Error::Panicked));
}

/// Counts the events that reach it. Clones share the count.
#[derive(Clone, Default)]
struct Count(Arc<AtomicUsize>);

impl Subscriber for Count {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, _: &Event<'_>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

#[test]
fn the_thread_logs_to_the_dispatch_of_the_caller_of_connect() {
    let count = Count::default();
    let client =
        tracing::dispatcher::with_default(&Dispatch::new(count.clone()), || {
            Client::start(|_, outcome, _, _| async move {
                tracing::info!("the client started");
                outcome.send(Ok(())).unwrap();
            })
        });
    assert_eq!(client.expect("a client").close(), Ok(()));
    assert_eq!(count.0.load(Ordering::Relaxed), 1);
}

/// Connects to a node at `addresses`, and checks that the error names each one with
/// `cause`.
fn unreachable(addresses: Vec<SocketAddr>, cause: &transport::Error) {
    let peer = PrivateKey([7; 32]).public();
    let node = Node {
        addresses: addresses.clone(),
        public_key: peer,
    };
    let subject = Subject {
        name: "plant.operator".parse().unwrap(),
        private_key: PrivateKey([8; 32]),
    };
    let attempts = addresses
        .into_iter()
        .map(|address| (transport::Address::Udp(address), cause.clone()))
        .collect();
    let Err(error) = Client::connect(&node, subject) else {
        panic!("a client of an unreachable node connected");
    };
    let unreachable = transport::Error::Unreachable {
        peer: Some(peer),
        attempts,
    };
    assert_eq!(
        error,
        Error::Session(program::Error::Transport(unreachable))
    );
}

/// Port 0 of IPv6 loopback.
fn port_0() -> SocketAddr {
    SocketAddr::from((Ipv6Addr::LOCALHOST, 0))
}

#[test]
fn a_client_of_port_0_is_unroutable() {
    unreachable(vec![port_0()], &transport::Error::Unroutable);
}

#[test]
fn a_client_of_a_node_with_no_address_is_unreachable_with_no_attempt() {
    unreachable(vec![], &transport::Error::Unroutable);
}

/// Asks for the node at `addresses`, and checks that the error names each one with
/// `cause`.
fn none_answering(addresses: Vec<SocketAddr>, cause: &transport::Error) {
    let attempts = addresses
        .iter()
        .map(|&address| (transport::Address::Udp(address), cause.clone()))
        .collect();
    let error = Node::answering(addresses).expect_err("no node answers");
    let unreachable = transport::Error::Unreachable {
        peer: None,
        attempts,
    };
    assert_eq!(
        error,
        Error::Session(program::Error::Transport(unreachable))
    );
}

#[test]
fn answering_of_port_0_is_unroutable() {
    none_answering(vec![port_0()], &transport::Error::Unroutable);
}

#[test]
fn answering_of_no_address_is_unreachable_with_no_attempt() {
    none_answering(vec![], &transport::Error::Unroutable);
}

/// Runs a transport of `private_key` on a shard of `os`, on a free port of IPv4
/// loopback, until the sender drops. Gives its address, the sender, and the shard.
fn serve_key(private_key: PrivateKey) -> (SocketAddr, oneshot::Sender<()>, Handle) {
    let net = os::net();
    let port = transport::Port::bind(&net, (Ipv4Addr::LOCALHOST, 0).into()).unwrap();
    let [transport::Address::Udp(at)] = port.addresses()[..] else {
        panic!("a port of one UDP socket");
    };
    let mut parts = port.split(NonZeroUsize::MIN);
    let (stop, stopped) = oneshot::channel();
    let shard = shard(move |tasks| async move {
        let config = transport::Config {
            private_key,
            message_bytes_max: NonZeroUsize::new(1 << 16).unwrap(),
            window_bytes: 1 << 20,
            streams_max: NonZeroU32::new(16).unwrap(),
            idle: Span::from_nanos(30 * Span::SECOND.nanos()),
            clock: os::clock(),
            entropy: os::entropy(),
            tasks,
            pool: Rc::new(block::Pool::heap(POOL)),
        };
        let port = parts.pop().unwrap();
        let transport = transport::Transport::new(config, port).unwrap();
        drop(stopped.await);
        drop(transport);
    });
    (at, stop, shard.unwrap())
}

#[test]
fn a_node_of_another_key_gives_other_key_with_both_keys() {
    let (at, stop, shard) = serve_key(PrivateKey([9; 32]));
    let proved = PrivateKey([9; 32]).public();
    let pinned = PrivateKey([7; 32]).public();
    let node = Node {
        addresses: vec![at],
        public_key: pinned,
    };
    let subject = Subject {
        name: "plant.operator".parse().unwrap(),
        private_key: PrivateKey([8; 32]),
    };
    let error = Client::connect(&node, subject).expect_err("the key is not pinned");
    assert_eq!(error, Error::OtherKey { pinned, proved });
    assert_eq!(
        error.to_string(),
        format!("the node proves public key {proved}, not the pinned {pinned}")
    );
    drop(stop);
    shard.join().unwrap();
}

/// A refused address whose node now proves the pinned key keeps the error of the
/// connect: the node may have taken its key back between the two dials.
#[test]
fn a_refused_address_that_now_proves_the_pinned_key_keeps_the_error() {
    let (at, stop, server) = serve_key(PrivateKey([7; 32]));
    let pinned = PrivateKey([7; 32]).public();
    let refused = || {
        let cause = transport::Error::Authentication { expected: pinned };
        program::Error::Transport(transport::Error::Unreachable {
            peer: Some(pinned),
            attempts: vec![(transport::Address::Udp(at), cause)].into(),
        })
    };
    let error = refused();
    let (given, answer) = oneshot::channel();
    let client = shard(move |tasks| async move {
        let transport = transport::Client::new(transport::client::Config {
            net: os::net(),
            clock: os::clock(),
            entropy: os::entropy(),
            tasks,
            pool: Rc::new(block::Pool::heap(POOL)),
        })
        .unwrap();
        drop(given.send(other_key(&transport, pinned, error).await));
    });
    client.unwrap().join().unwrap();
    let answer = answer.blocking_recv().expect("the shard gives its answer");
    assert_eq!(answer, Error::Session(refused()));
    drop(stop);
    server.join().unwrap();
}

/// A pinned address where no node answers times out once: no second dial follows a
/// dial that no key refused.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the time of the dials is the claim"
)]
fn a_pinned_address_with_no_node_gives_the_error_of_the_one_dial() {
    let silent = transport::Port::bind(&os::net(), (Ipv4Addr::LOCALHOST, 0).into());
    let silent = silent.unwrap();
    let [transport::Address::Udp(at)] = silent.addresses()[..] else {
        panic!("a port of one UDP socket");
    };
    let started = std::time::Instant::now();
    unreachable(vec![at], &transport::Error::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(45),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_stream_task_runs_to_its_end_while_the_client_is_open() {
    let runtime = Builder::new_current_thread().build().unwrap();
    let (_open, closed) = watch::channel(());
    let ran = Cell::new(false);
    runtime.block_on(serve(async { ran.set(true) }, closed));
    assert!(ran.get());
}

#[test]
fn a_stream_task_stops_at_the_close_of_the_client() {
    let runtime = Builder::new_current_thread().build().unwrap();
    let (open, closed) = watch::channel(());
    drop(open);
    runtime.block_on(serve(std::future::pending(), closed));
}

#[test]
fn a_connect_inside_a_tokio_runtime_blocks_and_gives_its_error() {
    let runtime = Builder::new_current_thread().build().unwrap();
    runtime.block_on(async {
        unreachable(vec![port_0()], &transport::Error::Unroutable);
    });
}

/// A data directory of its own for the test on this thread, removed when it drops.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let thread = std::thread::current();
        let test = thread.name().expect("invariant: libtest names the thread");
        let name = format!("foundation-sdk-{}-{test}", std::process::id());
        let dir = std::env::temp_dir().join(name.replace("::", "-"));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        Self(dir)
    }

    /// Writes `bytes` to the file `name` of `data`.
    fn write(&self, name: &str, bytes: &[u8]) {
        std::fs::write(self.0.join("data").join(name), bytes).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
}

/// The bytes of `address` of the node of key 7 at `bound`.
fn address(bound: SocketAddr) -> [u8; sector::address::LEN] {
    sector::address::encode(&sector::address::Address {
        key: types::node::Key::from_u128(7),
        public_key: PrivateKey([7; 32]).public(),
        bound,
    })
}

#[test]
fn local_gives_the_node_and_its_admin() {
    let dir = Scratch::new();
    dir.write(
        "address",
        &address(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 7400))),
    );
    dir.write("admin.key", &sector::admin::encode(&PrivateKey([3; 32])));
    let (node, subject) = Node::local(&dir.0).expect("a node runs");
    let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 7400));
    assert_eq!(node.addresses, [loopback]);
    assert_eq!(node.public_key, PrivateKey([7; 32]).public());
    let subject = subject.expect("an admin");
    assert_eq!(subject.name, spec::founding::admin());
    assert_eq!(subject.private_key.0, [3; 32]);
}

#[test]
fn local_with_no_admin_key_gives_no_subject() {
    let dir = Scratch::new();
    let bound = SocketAddr::from((Ipv6Addr::LOCALHOST, 7401));
    dir.write("address", &address(bound));
    let (node, subject) = Node::local(&dir.0).expect("a node runs");
    assert_eq!(node.addresses, [bound]);
    assert!(subject.is_none(), "{subject:?}");
}

/// The error of [`Node::local`] on `dir`.
fn local_error(dir: &Path) -> Error {
    Node::local(dir).expect_err("no node")
}

const NOT_RUNNING: Error = Error::Local(sector::local::Error::NotRunning);

#[test]
fn local_with_no_address_gives_not_running() {
    let dir = Scratch::new();
    assert_eq!(local_error(&dir.0), NOT_RUNNING);
}

#[test]
fn local_with_no_directory_gives_not_running() {
    let dir = Scratch::new();
    assert_eq!(local_error(&dir.0.join("other")), NOT_RUNNING);
    std::fs::remove_dir(dir.0.join("data")).unwrap();
    assert_eq!(local_error(&dir.0), NOT_RUNNING);
}

#[test]
fn local_with_a_file_in_place_of_the_directory_gives_not_running() {
    let dir = Scratch::new();
    std::fs::write(dir.0.join("file"), b"").unwrap();
    assert_eq!(local_error(&dir.0.join("file")), NOT_RUNNING);
    std::fs::remove_dir(dir.0.join("data")).unwrap();
    std::fs::write(dir.0.join("data"), b"").unwrap();
    assert_eq!(local_error(&dir.0), NOT_RUNNING);
}

/// Run as root, the open succeeds, and the test fails.
#[test]
fn local_with_a_data_that_cannot_open_gives_files() {
    let dir = Scratch::new();
    let data = dir.0.join("data");
    std::fs::set_permissions(&data, Permissions::from_mode(0o000)).unwrap();
    let error = local_error(&dir.0);
    std::fs::set_permissions(&data, Permissions::from_mode(0o700)).unwrap();
    let io = env::files::Error::Io {
        path: ".".into(),
        operation: env::files::Operation::Open,
        code: 13,
    };
    assert_eq!(error, Error::Local(sector::local::Error::Files(io)));
}

#[test]
fn local_gives_the_version_of_a_file_of_another_version() {
    let dir = Scratch::new();
    dir.write("address", b"foundation/address/2");
    let version = sector::local::Error::Version {
        file: "address",
        found: b'2',
        expected: b'1',
    };
    let error = local_error(&dir.0);
    assert_eq!(error.to_string(), version.to_string());
    assert_eq!(error, Error::Local(version));
}
