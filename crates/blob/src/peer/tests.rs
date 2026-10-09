//! `serve` over a real transport between two simulated nodes, driven by raw
//! `wire::blob` messages.

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use block::Pool;
use env::files::Operation;
use env::tasks::Tasks;
use sim::Crash;
use transport::{Address, Class, Port, Session, Transport};
use types::ed25519::PrivateKey;
use types::time::Span;
use wire::Protocol;
use wire::blob::{Requester, TOO_LARGE};

use super::*;
use crate::Config;

const PORT: u16 = 7000;
const SERVER: PrivateKey = PrivateKey([1; 32]);
const REQUESTER: PrivateKey = PrivateKey([2; 32]);
/// The requester's message limit: the least that `transport` takes, so the server
/// sends a chunk of more bytes in parts.
const MESSAGE: usize = 1472;
/// The directory of the server's store.
const DIR: &str = "blob";
/// The pool budget of each store and transport.
const BUDGET: usize = 1 << 20;

fn create_pool() -> Rc<Pool> {
    let config = block::Config { budget: BUDGET };
    let memory = block::Heap::new(config.reservation());
    Rc::new(Pool::new(config, memory))
}

/// A chunk of `len` bytes of `byte`, with its digest.
fn chunk(byte: u8, len: usize) -> (Digest, Vec<u8>) {
    let bytes = vec![byte; len];
    (Digest::of(&bytes), bytes)
}

fn path(digest: Digest) -> PathBuf {
    Path::new(DIR).join(digest.to_string())
}

/// The store of the server's node, which leaves `floor_bytes` free.
async fn open(node: &sim::node::Node, floor_bytes: u64) -> Store {
    Store::open(Config {
        files: node.files(),
        dir: DIR.into(),
        pool: create_pool(),
        floor_bytes,
    })
    .await
    .expect("the store opens")
}

/// Puts `bytes` in `store` under `digest`.
async fn put(store: &Store, digest: Digest, bytes: &[u8]) {
    let block = store.pool.copy(bytes).expect("the pool has room");
    store.put(digest, &block).await.expect("the put stores");
}

/// The bytes free on the disk of `node` once the directory of the store exists.
async fn free(node: &sim::node::Node) -> u64 {
    node.files().create_dir(Path::new(DIR)).await.unwrap();
    node.files().free().await.unwrap()
}

/// A transport of `node` at `PORT` that proves `key` and takes messages of at most
/// `message` bytes.
fn transport(
    node: &sim::node::Node,
    tasks: &Tasks,
    key: PrivateKey,
    message: usize,
) -> Transport {
    let at = SocketAddr::new(node.addresses()[0], PORT);
    let mut parts = Port::bind(&node.net(), at)
        .expect("binds")
        .split(NonZeroUsize::MIN);
    let config = transport::Config {
        private_key: key,
        message_bytes_max: NonZeroUsize::new(message).expect("not 0"),
        window_bytes: 1 << 20,
        streams_max: NonZeroU32::new(16).expect("not 0"),
        idle: Span::from_nanos(60 * Span::SECOND.nanos()),
        clock: node.clock(),
        entropy: node.entropy(),
        tasks: tasks.clone(),
        pool: create_pool(),
    };
    Transport::new(config, parts.pop().expect("one part")).expect("a transport")
}

/// The requester's node and its session to the server.
struct Peer {
    pool: Rc<Pool>,
    session: Session,
}

impl Peer {
    /// Opens a blob stream to the server and sends its header.
    async fn open(&self) -> Stream {
        let (sender, receiver) = self.session.open(Class::Complete).await.unwrap();
        let mut stream = Stream {
            pool: Rc::clone(&self.pool),
            sender,
            receiver,
        };
        stream.send(&wire::header::encode(Protocol::Blob)).await;
        stream
    }
}

/// The requester's end of one blob stream.
struct Stream {
    pool: Rc<Pool>,
    sender: Sender,
    receiver: Receiver,
}

impl Stream {
    async fn send(&mut self, bytes: &[u8]) {
        let block = self.pool.copy(bytes).expect("the pool has room");
        self.sender.send(block).await.expect("sends");
    }

    async fn get(&mut self, digests: &[Digest]) {
        let mut out = vec![0; wire::blob::get::encoded_len(digests.len())];
        wire::blob::get::encode(digests, &mut out);
        self.send(&out).await;
    }

    /// Sends the head of a put of `len` bytes.
    async fn put(&mut self, digest: Digest, len: u32) {
        let mut out = [0; Put::LEN];
        Put { digest, len }.encode(&mut out);
        self.send(&out).await;
    }

    async fn recv(&mut self) -> Result<Option<Vec<u8>>, transport::Error> {
        Ok(self.receiver.recv().await?.map(|block| block.to_vec()))
    }

    async fn reply(&mut self) -> Reply {
        let message = self.recv().await.expect("a reply").expect("not the end");
        Requester::new(BUDGET)
            .decode(&message)
            .expect("a reply decodes")
    }

    /// Finishes the requester's half and waits for the end of the server's.
    async fn finish(&mut self) {
        self.sender.finish().unwrap();
        assert_eq!(self.recv().await, Ok(None));
    }

    /// The error of the next read, once the server ended the stream with a code.
    async fn reset(&mut self) -> transport::Error {
        self.recv().await.expect_err("the server ends the stream")
    }
}

/// The code of a stream that the server reset.
fn reset(code: u32) -> transport::Error {
    transport::Error::Reset { code: Code(code) }
}

/// Runs one session from the requester's node to the server's node, whose disk has
/// `disk_bytes`. The server's node accepts one stream, reads its header, and gives
/// the stream to `server`; the requester's node gives its session to `requester`.
/// Panics unless both return. Gives the run and the server's node, for a crash.
fn run<S, R>(
    seed: u64,
    disk_bytes: u64,
    server: impl FnOnce(sim::node::Node, Incoming) -> S + Send + 'static,
    requester: impl FnOnce(Peer) -> R + Send + 'static,
) -> (sim::Sim, sim::node::Node)
where
    S: Future<Output = ()> + 'static,
    R: Future<Output = ()> + 'static,
{
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let server_node = sim.node(sim::node::Config {
        disk_bytes,
        ..sim::node::Config::default()
    });
    let requester_node = sim.node(sim::node::Config::default());
    let at = Address::Udp(SocketAddr::new(server_node.addresses()[0], PORT));
    let shard = |name: &str| env::shards::Config {
        name: name.into(),
        core: None,
    };
    let server_ended = Arc::new(AtomicBool::new(false));
    let requester_ended = Arc::new(AtomicBool::new(false));
    let node = server_node.clone();
    let done = Arc::clone(&server_ended);
    let main = move |tasks: Tasks| async move {
        let transport = transport(&node, &tasks, SERVER, 1 << 16);
        let session = transport.accept().await.expect("a session");
        let mut incoming = session.accept().await.expect("a stream");
        let header = incoming.receiver.recv().await.expect("a header");
        let header = header.expect("the header comes before the finish");
        assert_eq!(wire::header::decode(&header), Ok((Protocol::Blob, &[][..])));
        drop(header);
        server(node, incoming).await;
        done.store(true, Ordering::Relaxed);
        drop(session.closed().await);
    };
    drop(
        server_node
            .shards()
            .start(shard("server"), main)
            .expect("starts"),
    );
    let node = requester_node.clone();
    let done = Arc::clone(&requester_ended);
    let main = move |tasks: Tasks| async move {
        let transport = transport(&node, &tasks, REQUESTER, MESSAGE);
        let session = transport.dial(SERVER.public(), &[at]).await.expect("dials");
        let peer = Peer {
            pool: create_pool(),
            session: session.clone(),
        };
        requester(peer).await;
        done.store(true, Ordering::Relaxed);
        session.close(Code(0));
        node.clock().sleep(Span::MILLISECOND).await;
    };
    drop(
        requester_node
            .shards()
            .start(shard("requester"), main)
            .expect("starts"),
    );
    sim.run().expect("the run ends");
    assert!(server_ended.load(Ordering::Relaxed), "the server returns");
    assert!(
        requester_ended.load(Ordering::Relaxed),
        "the requester returns"
    );
    (sim, server_node)
}

/// The disk of the server's node.
const DISK: u64 = 64 << 20;

#[test]
fn a_get_gives_each_chunk_and_absent_in_order() {
    let (a, a_bytes) = chunk(1, 100);
    let (b, b_bytes) = chunk(2, 0);
    let (absent, _) = chunk(3, 100);
    let stored = (a_bytes.clone(), b_bytes.clone());
    run(
        0,
        DISK,
        move |node, incoming| async move {
            let store = open(&node, 0).await;
            put(&store, a, &stored.0).await;
            put(&store, b, &stored.1).await;
            assert_eq!(serve(&store, incoming).await, Ok(()));
        },
        move |peer| async move {
            let mut stream = peer.open().await;
            stream.get(&[a, absent, b]).await;
            assert_eq!(
                stream.reply().await,
                Reply::Chunk {
                    digest: a,
                    len: 100
                }
            );
            assert_eq!(stream.recv().await, Ok(Some(a_bytes)));
            assert_eq!(stream.reply().await, Reply::Absent { digest: absent });
            assert_eq!(stream.reply().await, Reply::Chunk { digest: b, len: 0 });
            stream.finish().await;
        },
    );
}

#[test]
fn a_chunk_over_the_message_limit_comes_in_parts_that_join_to_it() {
    let len = 2 * MESSAGE + 56;
    let (digest, bytes) = chunk(7, len);
    let stored = bytes.clone();
    run(
        0,
        DISK,
        move |node, incoming| async move {
            let store = open(&node, 0).await;
            put(&store, digest, &stored).await;
            assert_eq!(serve(&store, incoming).await, Ok(()));
        },
        move |peer| async move {
            let mut stream = peer.open().await;
            stream.get(&[digest]).await;
            let len = u32::try_from(len).unwrap();
            assert_eq!(stream.reply().await, Reply::Chunk { digest, len });
            let mut joined = Vec::new();
            for expected in [MESSAGE, MESSAGE, 56] {
                let part = stream.recv().await.unwrap().unwrap();
                assert_eq!(part.len(), expected);
                joined.extend_from_slice(&part);
            }
            assert_eq!(joined, bytes);
            stream.finish().await;
        },
    );
}

#[test]
fn a_put_gives_stored_once_the_chunk_is_durable() {
    let (digest, bytes) = chunk(7, 3000);
    let sent = bytes.clone();
    let (mut sim, node) = run(
        0,
        DISK,
        |node, incoming| async move {
            let store = open(&node, 0).await;
            assert_eq!(serve(&store, incoming).await, Ok(()));
        },
        move |peer| async move {
            let mut stream = peer.open().await;
            stream.put(digest, 3000).await;
            stream.send(&sent[..1000]).await;
            stream.send(&sent[1000..]).await;
            assert_eq!(stream.reply().await, Reply::Stored { digest });
            stream.finish().await;
        },
    );
    sim.crash(&node, Crash::Power);
    sim.run_on(&node, move |node, _| async move {
        let store = open(&node, 0).await;
        let got = store
            .get(digest)
            .await
            .unwrap()
            .expect("the chunk is durable");
        assert_eq!(&got[..], &bytes[..]);
    })
    .unwrap();
}

#[test]
fn a_put_of_the_empty_chunk_has_no_body() {
    let (digest, _) = chunk(0, 0);
    run(
        0,
        DISK,
        move |node, incoming| async move {
            let store = open(&node, 0).await;
            assert_eq!(serve(&store, incoming).await, Ok(()));
            assert!(store.get(digest).await.unwrap().is_some());
        },
        move |peer| async move {
            let mut stream = peer.open().await;
            stream.put(digest, 0).await;
            assert_eq!(stream.reply().await, Reply::Stored { digest });
            stream.finish().await;
        },
    );
}

#[test]
fn a_put_whose_bytes_hash_to_another_digest_stops_with_mismatch() {
    let (found, bytes) = chunk(7, 3000);
    let digest = Digest::of(b"another chunk");
    run(
        0,
        DISK,
        move |node, incoming| async move {
            let store = open(&node, 0).await;
            let served = serve(&store, incoming).await;
            let mismatch = crate::Error::Mismatch { digest, found };
            assert_eq!(served, Err(Error::Store(mismatch)));
            assert!(store.get(digest).await.unwrap().is_none());
            let left: Vec<PathBuf> = Vec::new();
            assert_eq!(node.files().list(Path::new(DIR)).await.unwrap(), left);
        },
        move |peer| async move {
            let mut stream = peer.open().await;
            stream.put(digest, 3000).await;
            stream.send(&bytes).await;
            assert_eq!(stream.reset().await, reset(MISMATCH));
        },
    );
}

#[test]
fn a_put_that_leaves_the_floor_free_gives_stored() {
    let (digest, bytes) = chunk(7, 3000);
    run(
        0,
        DISK,
        move |node, incoming| async move {
            let floor_bytes = free(&node).await - 3000;
            let store = open(&node, floor_bytes).await;
            assert_eq!(serve(&store, incoming).await, Ok(()));
        },
        move |peer| async move {
            let mut stream = peer.open().await;
            stream.put(digest, 3000).await;
            stream.send(&bytes).await;
            assert_eq!(stream.reply().await, Reply::Stored { digest });
            stream.finish().await;
        },
    );
}

#[test]
fn a_put_one_byte_under_the_floor_stops_with_full_and_stores_nothing() {
    let (digest, bytes) = chunk(7, 3000);
    run(
        0,
        DISK,
        move |node, incoming| async move {
            let free_bytes = free(&node).await;
            let floor_bytes = free_bytes - 2999;
            let store = open(&node, floor_bytes).await;
            let floor = crate::Error::Floor {
                len: 3000,
                free_bytes,
                floor_bytes,
            };
            assert_eq!(serve(&store, incoming).await, Err(Error::Store(floor)));
            assert!(store.get(digest).await.unwrap().is_none());
        },
        move |peer| async move {
            let mut stream = peer.open().await;
            stream.put(digest, 3000).await;
            stream.send(&bytes).await;
            assert_eq!(stream.reset().await, reset(FULL));
        },
    );
}

#[test]
fn a_put_on_a_full_disk_stops_with_full() {
    let (digest, bytes) = chunk(7, 128 << 10);
    run(
        0,
        64 << 10,
        move |node, incoming| async move {
            let store = open(&node, 0).await;
            let full = files::Error::Full { path: path(digest) };
            let served = serve(&store, incoming).await;
            assert_eq!(served, Err(Error::Store(crate::Error::Files(full))));
            assert!(store.get(digest).await.unwrap().is_none());
        },
        move |peer| async move {
            let mut stream = peer.open().await;
            stream.put(digest, 128 << 10).await;
            for part in bytes.chunks(1 << 16) {
                stream.send(part).await;
            }
            assert_eq!(stream.reset().await, reset(FULL));
        },
    );
}

#[test]
fn a_stream_that_ends_in_a_body_stops_as_malformed_and_stores_nothing() {
    let (digest, bytes) = chunk(7, 3000);
    run(
        0,
        DISK,
        move |node, incoming| async move {
            let store = open(&node, 0).await;
            let unfinished = wire::blob::Error::Unfinished { remain: 2000 };
            assert_eq!(serve(&store, incoming).await, Err(Error::Wire(unfinished)));
            assert!(store.get(digest).await.unwrap().is_none());
        },
        move |peer| async move {
            let mut stream = peer.open().await;
            stream.put(digest, 3000).await;
            stream.send(&bytes[..1000]).await;
            stream.sender.finish().unwrap();
            assert_eq!(stream.reset().await, reset(MALFORMED));
        },
    );
}

#[test]
fn a_put_over_the_largest_block_stops_with_too_large() {
    let digest = Digest::of(b"a large chunk");
    let max = create_pool().largest();
    let len = u32::try_from(max + 1).unwrap();
    run(
        0,
        DISK,
        move |node, incoming| async move {
            let store = open(&node, 0).await;
            let large = wire::blob::Error::TooLarge { len, max };
            assert_eq!(serve(&store, incoming).await, Err(Error::Wire(large)));
        },
        move |peer| async move {
            let mut stream = peer.open().await;
            stream.put(digest, len).await;
            assert_eq!(stream.reset().await, reset(TOO_LARGE));
        },
    );
}

#[test]
fn a_message_of_no_kind_stops_as_malformed() {
    run(
        0,
        DISK,
        |node, incoming| async move {
            let store = open(&node, 0).await;
            let kind = wire::blob::Error::Kind { kind: 9 };
            assert_eq!(serve(&store, incoming).await, Err(Error::Wire(kind)));
        },
        |peer| async move {
            let mut stream = peer.open().await;
            stream.send(&[9]).await;
            assert_eq!(stream.reset().await, reset(MALFORMED));
        },
    );
}

#[test]
fn a_failed_read_of_the_store_ends_the_stream_with_code_0() {
    let (digest, bytes) = chunk(7, 3000);
    run(
        0,
        DISK,
        move |node, incoming| async move {
            let store = open(&node, 0).await;
            put(&store, digest, &bytes).await;
            node.fail_file(&path(digest), Operation::Open);
            let io = files::Error::Io {
                path: path(digest),
                operation: Operation::Open,
                code: 5,
            };
            let served = serve(&store, incoming).await;
            assert_eq!(served, Err(Error::Store(crate::Error::Files(io))));
        },
        move |peer| async move {
            let mut stream = peer.open().await;
            stream.get(&[digest]).await;
            assert_eq!(stream.reset().await, reset(0));
        },
    );
}

#[test]
fn a_one_way_stream_stops_as_malformed() {
    run(
        0,
        DISK,
        |node, incoming| async move {
            let store = open(&node, 0).await;
            assert_eq!(serve(&store, incoming).await, Err(Error::OneWay));
        },
        |peer| async move {
            let mut sender = peer.session.open_sender(Class::Complete).await.unwrap();
            let header = wire::header::encode(Protocol::Blob);
            sender.send(peer.pool.copy(&header).unwrap()).await.unwrap();
            let stopped = transport::Error::Stopped {
                code: Code(MALFORMED),
            };
            loop {
                let get = peer.pool.copy(&[1; 33]).unwrap();
                if let Err(error) = sender.send(get).await {
                    assert_eq!(error, stopped);
                    break;
                }
            }
        },
    );
}

#[test]
fn an_error_displays_its_cause() {
    let wire = wire::blob::Error::Kind { kind: 9 };
    assert_eq!(
        Error::Wire(wire).to_string(),
        format!("a blob message is not valid: {wire}")
    );
    assert_eq!(
        Error::OneWay.to_string(),
        "a blob stream needs a two-way stream"
    );
    let store = crate::Error::Mismatch {
        digest: Digest::of(b"a"),
        found: Digest::of(b"b"),
    };
    assert_eq!(Error::Store(store.clone()).to_string(), store.to_string());
    let stream = transport::Error::TimedOut;
    assert_eq!(
        Error::Stream(stream.clone()).to_string(),
        format!("the blob stream failed: {stream}")
    );
}
