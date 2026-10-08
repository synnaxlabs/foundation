//! Remote reader sessions that `Hub::serve` serves, over a real transport between two
//! simulated nodes.

use std::future::poll_fn;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use block::Pool;
use env::files::Operation;
use env::tasks::Tasks;
use hub::{Channel, serve};
use transport::stream::{Incoming, Receiver, Sender};
use transport::{Address, Class, Code, Port, Transport};
use types::channel;
use types::ed25519::{PrivateKey, PublicKey};
use types::frame::Form;
use types::frame::Path as FramePath;
use types::sample::Type;
use types::time::Span;
use wire::Protocol;
use wire::hub::{Credit, FromHome, Head, Mode, Open, Reader, keys};

use super::{
    AREA, BODY_MAX, I64, LIVE, RING, SETTLE, STAMP, Test, fill, scrambled, write,
    write_series, write_wide,
};

/// The UDP port of each transport.
const PORT: u16 = 7000;
const HOME: PrivateKey = PrivateKey([1; 32]);
const PEER: PrivateKey = PrivateKey([2; 32]);
/// The peer's message limit: the least that `transport` takes, so the home cuts a
/// body and its ends into several messages.
const PEER_MESSAGE: usize = 1472;
/// How long the peer waits for a reply that must not come.
const QUIET: Span = Span::from_nanos(100_000_000);

fn public_key(key: &PrivateKey) -> PublicKey {
    let pair = Ed25519KeyPair::from_seed_unchecked(&key.0).expect("a key pair");
    PublicKey::new(pair.public_key().as_ref().try_into().expect("32 bytes"))
        .expect("a public key")
}

/// A transport of `node` at `PORT` that proves `key` and takes messages of at most
/// `message` bytes.
fn transport(
    node: &sim::node::Node,
    tasks: &Tasks,
    pool: &Rc<Pool>,
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
        pool: Rc::clone(pool),
    };
    Transport::new(config, parts.pop().expect("one part")).expect("a transport")
}

/// The reader's node: its end of one hub stream.
struct Peer {
    node: sim::node::Node,
    pool: Rc<Pool>,
    sender: Sender,
    /// `None` on a one-way stream.
    receiver: Option<Receiver>,
}

impl Peer {
    async fn send(&mut self, bytes: &[u8]) -> Result<(), transport::Error> {
        let mut block = self.pool.alloc(bytes.len()).expect("the pool has room");
        block.copy_from_slice(bytes);
        self.sender.send(block.freeze()).await
    }

    /// Sends an open of `keys` in `mode`, the keys in one message.
    async fn open(&mut self, mode: Mode, keys: &[u128]) {
        let channels = u32::try_from(keys.len()).expect("the keys fit a u32");
        let open = Open { mode, channels };
        let mut out = vec![0; open.encoded_len()];
        open.encode(&mut out);
        self.send(&out).await.expect("sends the open");
        let keys: Vec<_> = keys.iter().map(|&k| channel::Key::from_u128(k)).collect();
        let mut out = vec![0; keys.len() * keys::LEN];
        keys::encode(&keys, &mut out);
        self.send(&out).await.expect("sends the keys");
    }

    async fn credit(&mut self, limit_bytes: u64) -> Result<(), transport::Error> {
        let mut out = [0; Credit::LEN];
        Credit { limit_bytes }.encode(&mut out);
        self.send(&out).await
    }

    /// The next message from the home, `None` once the home finished.
    async fn recv(&mut self) -> Result<Option<Vec<u8>>, transport::Error> {
        let receiver = self.receiver.as_mut().expect("a two-way stream");
        Ok(receiver.recv().await?.map(|block| block.to_vec()))
    }

    async fn sleep(&self, span: Span) {
        self.node.clock().sleep(span).await;
    }
}

/// A pool for a transport, so a test that fills the hub's pool does not fill it.
fn own_pool() -> Rc<Pool> {
    let config = block::Config { budget: 1 << 20 };
    Rc::new(Pool::new(
        config.clone(),
        block::Heap::new(config.reservation()),
    ))
}

/// Runs one remote reader session over a transport: the home's node makes a [`Test`]
/// hub with mesh time, accepts the stream that the peer's node opens with `class`,
/// reads its header, and gives the test and the stream to `home`; the peer's node
/// gives its end to `peer`. The stream is one way when `one_way`.
fn session<H, P>(
    seed: u64,
    class: Class,
    one_way: bool,
    home: impl FnOnce(Test, Incoming) -> H + Send + 'static,
    peer: impl FnOnce(Peer) -> P + Send + 'static,
) where
    H: Future<Output = ()> + 'static,
    P: Future<Output = ()> + 'static,
{
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let nodes = [1, 2].map(|_| sim.node(sim::node::Config::default()));
    let at = Address::Udp(SocketAddr::new(nodes[0].addresses()[0], PORT));
    let shard = |name: &str| env::shards::Config {
        name: name.into(),
        core: None,
    };
    let node = nodes[0].clone();
    let main = move |tasks: Tasks| async move {
        let layout = buffer::Layout::new(AREA, BODY_MAX).expect("a ring");
        let mut test = Test::new(node.clone(), tasks.clone(), layout).await;
        test.sync().await;
        let transport = transport(&node, &tasks, &own_pool(), HOME, 1 << 16);
        let session = transport.accept().await.expect("a session");
        let mut incoming = session.accept().await.expect("a stream");
        let header = incoming.receiver.recv().await.expect("a header");
        let header = header.expect("the header comes before the finish");
        assert_eq!(wire::header::decode(&header), Ok((Protocol::Hub, &[][..])));
        drop(header);
        home(test, incoming).await;
        drop(session.closed().await);
    };
    drop(
        nodes[0]
            .shards()
            .start(shard("home"), main)
            .expect("starts"),
    );
    let node = nodes[1].clone();
    let main = move |tasks: Tasks| async move {
        let pool = own_pool();
        let transport = transport(&node, &tasks, &pool, PEER, PEER_MESSAGE);
        let session = transport
            .dial(public_key(&HOME), &[at])
            .await
            .expect("dials");
        let (sender, receiver) = if one_way {
            (session.open_sender(class).await.expect("opens"), None)
        } else {
            let (sender, receiver) = session.open(class).await.expect("opens");
            (sender, Some(receiver))
        };
        let mut end = Peer {
            node: node.clone(),
            pool,
            sender,
            receiver,
        };
        end.send(&wire::header::encode(Protocol::Hub))
            .await
            .expect("sends the header");
        peer(end).await;
        session.close(Code(0));
        node.clock().sleep(Span::MILLISECOND).await;
    };
    drop(
        nodes[1]
            .shards()
            .start(shard("peer"), main)
            .expect("starts"),
    );
    sim.run().expect("the run ends");
}

/// Runs a session whose home only serves it, and gives what `serve` returned, or
/// `None` when it did not return.
fn served<P>(
    seed: u64,
    class: Class,
    one_way: bool,
    peer: impl FnOnce(Peer) -> P + Send + 'static,
) -> Option<Result<(), serve::Error>>
where
    P: Future<Output = ()> + 'static,
{
    let result = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&result);
    session(
        seed,
        class,
        one_way,
        move |test, incoming| async move {
            let served = test.hub.serve(incoming).await;
            *kept.lock().expect("not poisoned") = Some(served);
        },
        peer,
    );
    result.lock().expect("not poisoned").take()
}

/// The stop code that a send of the peer gets once the home stopped the stream.
async fn stopped(peer: &mut Peer) -> transport::Error {
    loop {
        if let Err(error) = peer.credit(0).await {
            return error;
        }
        peer.sleep(Span::MILLISECOND).await;
    }
}

#[test]
fn opens_a_session_of_known_keys() {
    let served = served(41, Class::Complete, false, |mut peer| async move {
        peer.open(Mode::Complete { limit_bytes: 0 }, &[1, 2]).await;
        assert_eq!(peer.recv().await, Ok(Some(vec![1])));
        peer.sender.finish().expect("finishes");
        assert_eq!(peer.recv().await, Ok(None));
    });
    assert_eq!(served, Some(Ok(())));
}

#[test]
fn finishes_when_the_peer_finishes_before_the_open() {
    let served = served(59, Class::Complete, false, |mut peer| async move {
        peer.sender.finish().expect("finishes");
        assert_eq!(peer.recv().await, Ok(None));
    });
    assert_eq!(served, Some(Ok(())));
}

#[test]
fn finishes_when_the_peer_finishes_in_the_keys_run() {
    let served = served(60, Class::Complete, false, |mut peer| async move {
        let open = Open {
            mode: Mode::Complete { limit_bytes: 0 },
            channels: 2,
        };
        let mut out = vec![0; open.encoded_len()];
        open.encode(&mut out);
        peer.send(&out).await.expect("sends the open");
        let mut out = vec![0; keys::LEN];
        keys::encode(&[channel::Key::from_u128(1)], &mut out);
        peer.send(&out).await.expect("sends a key");
        peer.sender.finish().expect("finishes");
        assert_eq!(peer.recv().await, Ok(None));
    });
    assert_eq!(served, Some(Ok(())));
}

#[test]
fn stops_an_open_of_an_unknown_key_with_unknown() {
    let served = served(42, Class::Complete, false, |mut peer| async move {
        peer.open(Mode::Complete { limit_bytes: 0 }, &[1, 9]).await;
        let code = Code(wire::hub::UNKNOWN);
        assert_eq!(peer.recv().await, Err(transport::Error::Reset { code }));
        assert_eq!(stopped(&mut peer).await, transport::Error::Stopped { code });
    });
    let unknown = serve::Error::Unknown(channel::Key::from_u128(9));
    assert_eq!(served, Some(Err(unknown.clone())));
    assert_eq!(
        unknown.to_string(),
        "the open names channel 00000000-0000-0000-0000-000000000009, which this \
         node does not know"
    );
}

/// Runs a session whose peer opens `keys` in a complete session, and gives what
/// `serve` returned once the peer saw its stream stopped with `MALFORMED`.
fn refused(seed: u64, keys: &'static [u128]) -> Option<Result<(), serve::Error>> {
    served(seed, Class::Complete, false, move |mut peer| async move {
        peer.open(Mode::Complete { limit_bytes: 0 }, keys).await;
        let code = Code(wire::header::MALFORMED);
        assert_eq!(peer.recv().await, Err(transport::Error::Reset { code }));
        assert_eq!(stopped(&mut peer).await, transport::Error::Stopped { code });
    })
}

#[test]
fn stops_an_open_of_channels_on_two_indexes_as_malformed() {
    let served = refused(43, &[1, 2, 3, 4]);
    assert_eq!(served, Some(Err(serve::Error::ManyIndexes)));
    assert_eq!(
        serve::Error::ManyIndexes.to_string(),
        "the open names channels on more than one index"
    );
}

#[test]
fn stops_an_open_without_the_index_of_its_channels_as_malformed() {
    let served = refused(44, &[2, 5]);
    assert_eq!(served, Some(Err(serve::Error::NoIndex)));
    assert_eq!(
        serve::Error::NoIndex.to_string(),
        "the open does not name the index of its channels"
    );
}

#[test]
fn stops_a_one_way_stream_as_malformed() {
    let served = served(45, Class::Complete, true, |mut peer| async move {
        let code = Code(wire::header::MALFORMED);
        assert_eq!(stopped(&mut peer).await, transport::Error::Stopped { code });
    });
    assert_eq!(served, Some(Err(serve::Error::OneWay)));
    assert_eq!(
        serve::Error::OneWay.to_string(),
        "a hub session needs a two-way stream"
    );
}

#[test]
fn stops_a_second_open_as_malformed() {
    let served = served(46, Class::Complete, false, |mut peer| async move {
        peer.open(Mode::Complete { limit_bytes: 0 }, &[1, 2]).await;
        assert_eq!(peer.recv().await, Ok(Some(vec![1])));
        peer.open(Mode::Complete { limit_bytes: 0 }, &[1, 2]).await;
        let code = Code(wire::header::MALFORMED);
        assert_eq!(peer.recv().await, Err(transport::Error::Reset { code }));
    });
    let reopen = wire::hub::Error::Reopen { kind: 2 };
    assert_eq!(served, Some(Err(serve::Error::Message(reopen))));
    assert_eq!(
        serve::Error::Message(reopen).to_string(),
        "a hub message is not valid: the hub message has kind 2, which opens the \
         session, and the session is open"
    );
}

#[test]
fn stops_a_credit_in_a_latest_session_as_malformed() {
    let served = served(47, Class::Latest, false, |mut peer| async move {
        peer.open(Mode::Latest, &[1, 2]).await;
        assert_eq!(peer.recv().await, Ok(Some(vec![1])));
        peer.credit(1).await.expect("sends the credit");
        let code = Code(wire::header::MALFORMED);
        assert_eq!(peer.recv().await, Err(transport::Error::Reset { code }));
    });
    let latest = wire::hub::Error::Latest { kind: 3 };
    assert_eq!(served, Some(Err(serve::Error::Message(latest))));
}

#[test]
fn sends_behind_and_finishes_when_a_complete_session_misses_a_frame() {
    let result = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&result);
    let home = move |test: Test, incoming| async move {
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        let clock = test.clock.clone();
        test.tasks.spawn(async move {
            clock.sleep(SETTLE).await;
            super::write_wide(&mut writer, now, 0);
            clock.sleep(SETTLE).await;
            // The first frame waits for credit, so it misses at this commit.
            super::write_wide(&mut writer, now, 1);
            clock.sleep(SETTLE).await;
        });
        let served = test.hub.serve(incoming).await;
        *kept.lock().expect("not poisoned") = Some(served);
    };
    session(48, Class::Complete, false, home, |mut peer| async move {
        peer.open(Mode::Complete { limit_bytes: 0 }, &[1, 2]).await;
        assert_eq!(peer.recv().await, Ok(Some(vec![1])));
        assert_eq!(peer.recv().await, Ok(Some(vec![3])));
        assert_eq!(peer.recv().await, Ok(None));
    });
    let served = result.lock().expect("not poisoned").take();
    assert_eq!(served, Some(Ok(())));
}

#[test]
fn ends_with_the_error_of_the_stream_when_the_peer_resets_it() {
    let served = served(49, Class::Complete, false, |mut peer| async move {
        peer.open(Mode::Complete { limit_bytes: 0 }, &[1, 2]).await;
        assert_eq!(peer.recv().await, Ok(Some(vec![1])));
        let Peer { node, sender, .. } = peer;
        sender.reset(Code(7));
        node.clock().sleep(QUIET).await;
    });
    let reset = transport::Error::Reset { code: Code(7) };
    assert_eq!(served, Some(Err(serve::Error::Stream(reset.clone()))));
    assert_eq!(
        serve::Error::Stream(reset).to_string(),
        "the stream of the hub session failed: the peer reset the stream (7)"
    );
}

#[test]
fn stops_an_open_on_a_stream_of_another_class_as_malformed() {
    let opens = [
        (Class::Command, Mode::Complete { limit_bytes: 0 }),
        (Class::Complete, Mode::Latest),
        (Class::Latest, Mode::Complete { limit_bytes: 0 }),
    ];
    for (seed, (class, mode)) in (50..).zip(opens) {
        let served = served(seed, class, false, move |mut peer| async move {
            peer.open(mode, &[1, 2]).await;
            let code = Code(wire::header::MALFORMED);
            assert_eq!(peer.recv().await, Err(transport::Error::Reset { code }));
        });
        assert_eq!(served, Some(Err(serve::Error::Class(class))));
    }
    assert_eq!(
        serve::Error::Class(Class::Command).to_string(),
        "the stream has class Command, which is not the class of the open's mode"
    );
}

/// What a sync of the ring that fails gives.
fn failed() -> env::files::Error {
    env::files::Error::Io {
        path: PathBuf::from(RING),
        operation: Operation::Sync,
        code: 5,
    }
}

/// The peer's end of a session whose home stops it with `FAILED`, after `Opened` or
/// before.
async fn stopped_as_failed(mut peer: Peer) {
    peer.open(
        Mode::Complete {
            limit_bytes: 1 << 20,
        },
        &[1, 2],
    )
    .await;
    let code = Code(wire::hub::FAILED);
    let mut message = peer.recv().await;
    if message == Ok(Some(vec![1])) {
        message = peer.recv().await;
    }
    assert_eq!(message, Err(transport::Error::Reset { code }));
}

#[test]
fn stops_an_open_on_a_failed_home_with_failed() {
    let result = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&result);
    let home = move |test: Test, incoming| async move {
        let mut writer = test.writer("a", &["value"]).await;
        test.node.fail_file(Path::new(RING), Operation::Sync);
        write(&mut writer, &[test.now()], &[1]);
        test.clock.sleep(SETTLE).await;
        let served = test.hub.serve(incoming).await;
        *kept.lock().expect("not poisoned") = Some(served);
    };
    session(53, Class::Complete, false, home, stopped_as_failed);
    let served = result.lock().expect("not poisoned").take();
    assert_eq!(served, Some(Err(serve::Error::Buffer(failed()))));
    assert_eq!(
        serve::Error::Buffer(failed()).to_string(),
        "the buffer of the shard failed: sync of shard-0/ring failed with OS error 5"
    );
}

#[test]
fn stops_a_session_whose_home_fails_after_the_open_with_failed() {
    let result = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&result);
    let home = move |test: Test, incoming| async move {
        let mut writer = test.writer("a", &["value"]).await;
        let (node, clock, now) = (test.node.clone(), test.clock.clone(), test.now());
        test.tasks.spawn(async move {
            clock.sleep(SETTLE).await;
            node.fail_file(Path::new(RING), Operation::Sync);
            write(&mut writer, &[now], &[1]);
            clock.sleep(SETTLE).await;
        });
        let served = test.hub.serve(incoming).await;
        *kept.lock().expect("not poisoned") = Some(served);
    };
    session(54, Class::Complete, false, home, |mut peer| async move {
        peer.open(
            Mode::Complete {
                limit_bytes: 1 << 20,
            },
            &[1, 2],
        )
        .await;
        assert_eq!(peer.recv().await, Ok(Some(vec![1])));
        let code = Code(wire::hub::FAILED);
        assert_eq!(peer.recv().await, Err(transport::Error::Reset { code }));
    });
    let served = result.lock().expect("not poisoned").take();
    assert_eq!(served, Some(Err(serve::Error::Buffer(failed()))));
}

/// A drop of `serve` after `Opened` drops the session, so once the hub drops too, its
/// state and the commit task go.
#[test]
fn closes_the_session_when_the_future_drops() {
    let home = |test: Test, incoming| async move {
        {
            let mut serve = pin!(test.hub.serve(incoming));
            let mut sleep = pin!(test.clock.sleep(SETTLE));
            let served = poll_fn(|cx| match serve.as_mut().poll(cx) {
                Poll::Ready(served) => Poll::Ready(Some(served)),
                Poll::Pending => sleep.as_mut().poll(cx).map(|()| None),
            })
            .await;
            assert_eq!(served, None, "serve waits for a frame");
        }
        let Test {
            clock, hub, ended, ..
        } = test;
        clock.sleep(SETTLE).await;
        assert_eq!(ended.get(), 0, "the hub holds the home");
        drop(hub);
        clock.sleep(SETTLE).await;
        assert_eq!(ended.get(), 1, "the commit task ended");
    };
    session(55, Class::Complete, false, home, |mut peer| async move {
        peer.open(Mode::Complete { limit_bytes: 0 }, &[1, 2]).await;
        assert_eq!(peer.recv().await, Ok(Some(vec![1])));
        let code = Code(0);
        assert_eq!(peer.recv().await, Err(transport::Error::Reset { code }));
    });
}

/// One frame that the peer read.
#[derive(Debug)]
struct Got {
    head: Head,
    ends: Vec<(u32, u32)>,
    body: Vec<u8>,
}

/// Opens `keys` as a complete session with a window of `limit_bytes`, and reads
/// `Opened`.
async fn open_complete(peer: &mut Peer, keys: &[u128], limit_bytes: u64) -> Reader {
    let mode = Mode::Complete { limit_bytes };
    peer.open(mode, keys).await;
    let channels = u32::try_from(keys.len()).expect("the keys fit a u32");
    let mut reader = Reader::new(&Open { mode, channels });
    let opened = peer.recv().await.expect("receives").expect("a message");
    assert!(matches!(reader.decode(&opened), Ok(FromHome::Opened)));
    reader
}

/// The next frame from the home, or `None` after `Behind` and the finish.
async fn got(peer: &mut Peer, reader: &mut Reader) -> Option<Got> {
    let mut next = async || peer.recv().await.expect("receives").expect("a message");
    let message = next().await;
    let head = match reader.decode(&message).expect("a valid message") {
        FromHome::Head(head) => head,
        FromHome::Behind => {
            assert_eq!(peer.recv().await, Ok(None));
            return None;
        }
        other => panic!("{other:?} came, not a head"),
    };
    let (mut ends, mut body) = (Vec::new(), Vec::new());
    loop {
        let message = next().await;
        let Ok(FromHome::Ends { ends: run, last }) = reader.decode(&message) else {
            panic!("a message of the ends run");
        };
        ends.extend(run);
        if last {
            break;
        }
    }
    while reader.body().is_some() {
        let message = next().await;
        let Ok(FromHome::Body { bytes, .. }) = reader.decode(&message) else {
            panic!("a message of the body");
        };
        body.extend_from_slice(bytes);
    }
    Some(Got { head, ends, body })
}

/// The places of the series of `got`, in order.
fn places(got: &Got) -> Vec<u32> {
    got.ends.iter().map(|&(place, _)| place).collect()
}

/// The samples of each series of `got`, in place order, decoded as `types`. Each
/// series starts at the end before it rounded up to a multiple of 8.
fn decoded(got: &Got, types: &[Type]) -> Vec<Vec<i64>> {
    let count = usize::try_from(got.head.range.count).expect("a count");
    let mut start = 0;
    got.ends
        .iter()
        .zip(types)
        .map(|(&(_, end), &data_type)| {
            let end = usize::try_from(end).expect("fits");
            let mut out = vec![0; count * 8];
            codec::decode(data_type, count, &got.body[start..end], &mut out)
                .expect("decodes");
            start = end.next_multiple_of(8);
            let (chunks, _) = out.as_chunks::<8>();
            chunks
                .iter()
                .map(|chunk| i64::from_le_bytes(*chunk))
                .collect()
        })
        .collect()
}

/// The open lists `value` twice: its series has the place of its first listing, and
/// `time` has place 2.
#[test]
fn sends_each_frame_through_the_places_of_the_open() {
    let home = |test: Test, incoming| async move {
        let mut writer = test.writer("a", &["value"]).await;
        let (clock, now) = (test.clock.clone(), test.now());
        test.tasks.spawn(async move {
            clock.sleep(SETTLE).await;
            write(&mut writer, &[now, now + 1], &[10, 20]);
            write(&mut writer, &[now + 2], &[30]);
            clock.sleep(SETTLE).await;
        });
        assert_eq!(test.hub.serve(incoming).await, Ok(()));
    };
    session(56, Class::Complete, false, home, |mut peer| async move {
        let mut reader = open_complete(&mut peer, &[2, 2, 1], 1 << 20).await;
        let mut first = None;
        for (values, stamps) in [(&[10, 20][..], &[0, 1][..]), (&[30], &[2])] {
            let got = got(&mut peer, &mut reader).await.expect("a frame");
            assert_eq!(got.head.path, FramePath::Live);
            assert_eq!(got.head.series, 2);
            assert_eq!(places(&got), [0, 2]);
            let [got_values, got_stamps] =
                <[_; 2]>::try_from(decoded(&got, &[I64, STAMP])).expect("two series");
            let first = *first.get_or_insert(got_stamps[0]);
            let stamps: Vec<_> = stamps.iter().map(|stamp| first + stamp).collect();
            assert_eq!(got_values, values);
            assert_eq!(got_stamps, stamps);
        }
        peer.sender.finish().expect("finishes");
        assert_eq!(peer.recv().await, Ok(None));
    });
}

/// A frame of 191 series: its ends and its body each pass `PEER_MESSAGE`, so each
/// goes in two or more messages.
#[test]
fn sends_a_frame_wider_than_a_message_of_the_peer() {
    const KEYS: std::ops::Range<u128> = 10..200;
    let home = |test: Test, incoming| async move {
        let names: Vec<_> = KEYS.map(|key| format!("v{key}")).collect();
        for (key, name) in KEYS.zip(&names) {
            test.hub.define(Channel {
                key: channel::Key::from_u128(key),
                name: super::name(name),
                data_type: I64,
                index: channel::Key::from_u128(1),
            });
        }
        let names: Vec<_> = names.iter().map(String::as_str).collect();
        let mut writer = test.writer("a", &names).await;
        let (clock, now) = (test.clock.clone(), test.now());
        test.tasks.spawn(async move {
            clock.sleep(SETTLE).await;
            let values: Vec<_> = KEYS
                .map(|key| [i64::try_from(key).expect("fits")])
                .collect();
            let stamps = [now];
            let series: Vec<_> = KEYS
                .zip(&values)
                .map(|(key, value)| (key, &value[..]))
                .chain([(1, &stamps[..])])
                .collect();
            write_series(&mut writer, &series);
            clock.sleep(SETTLE).await;
        });
        assert_eq!(test.hub.serve(incoming).await, Ok(()));
    };
    session(64, Class::Complete, false, home, |mut peer| async move {
        let keys: Vec<_> = KEYS.chain([1]).collect();
        let mut reader = open_complete(&mut peer, &keys, 1 << 20).await;
        let got = got(&mut peer, &mut reader).await.expect("a frame");
        let series = u32::try_from(keys.len()).expect("fits");
        assert_eq!(places(&got), (0..series).collect::<Vec<_>>());
        let types: Vec<_> = KEYS.map(|_| I64).collect();
        let values: Vec<_> = KEYS
            .map(|key| vec![i64::try_from(key).expect("fits")])
            .collect();
        assert_eq!(decoded(&got, &types), values);
        peer.sender.finish().expect("finishes");
        assert_eq!(peer.recv().await, Ok(None));
    });
}

/// A text series that encodes to more than `PEER_MESSAGE` bytes, not a multiple of 8:
/// the cut falls inside it, and its zeros go in the second message.
#[test]
fn sends_the_zeros_after_a_series_cut_at_the_message_limit() {
    let text: Vec<_> = (0..2001_u32)
        .map(|i| b' ' + u8::try_from((i * 37 + i * i) % 95).expect("fits"))
        .collect();
    let raw = [&2001_u32.to_le_bytes()[..], &text].concat();
    let written = raw.clone();
    let home = |test: Test, incoming| async move {
        test.hub.define(Channel {
            key: channel::Key::from_u128(6),
            name: super::name("text"),
            data_type: Type::String,
            index: channel::Key::from_u128(1),
        });
        let mut writer = test.writer("a", &["text", "value"]).await;
        let (clock, now) = (test.clock.clone(), test.now());
        test.tasks.spawn(async move {
            clock.sleep(SETTLE).await;
            let set = Arc::clone(writer.set());
            let [index, text, value] = [1, 6, 2].map(|key| super::entry(&set, key));
            let series = [(index, 8), (text, written.len()), (value, 8)];
            let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
            for (entry, bytes) in [
                (index, &now.to_le_bytes()[..]),
                (text, &written),
                (value, &30_i64.to_le_bytes()),
            ] {
                let series = draft.series_mut(entry).expect("the series is present");
                series.copy_from_slice(bytes);
            }
            draft.set_count(0, 1);
            writer.write(LIVE, draft).expect("the home takes it");
            clock.sleep(SETTLE).await;
        });
        assert_eq!(test.hub.serve(incoming).await, Ok(()));
    };
    session(66, Class::Complete, false, home, |mut peer| async move {
        let mut reader = open_complete(&mut peer, &[6, 2, 1], 1 << 20).await;
        let got = got(&mut peer, &mut reader).await.expect("a frame");
        assert_eq!(places(&got), [0, 1, 2]);
        let end = usize::try_from(got.ends[0].1).expect("fits");
        assert!(end > PEER_MESSAGE);
        assert_ne!(end % 8, 0);
        assert!(
            got.body[end..end.next_multiple_of(8)]
                .iter()
                .all(|&b| b == 0)
        );
        let mut text = vec![0; raw.len()];
        codec::decode(Type::String, 1, &got.body[..end], &mut text).expect("decodes");
        assert_eq!(text, raw);
        let value =
            end.next_multiple_of(8)..usize::try_from(got.ends[1].1).expect("fits");
        let mut out = [0; 8];
        codec::decode(I64, 1, &got.body[value], &mut out).expect("decodes");
        assert_eq!(i64::from_le_bytes(out), 30);
        peer.sender.finish().expect("finishes");
        assert_eq!(peer.recv().await, Ok(None));
    });
}

/// A credit raises a grant of 0, so the session gets the frame written after it.
#[test]
fn sends_a_frame_once_a_credit_raises_the_grant() {
    let home = |test: Test, incoming| async move {
        let mut writer = test.writer("a", &["value"]).await;
        let (clock, now) = (test.clock.clone(), test.now());
        test.tasks.spawn(async move {
            clock.sleep(SETTLE).await;
            write(&mut writer, &[now], &[10]);
            clock.sleep(SETTLE).await;
        });
        assert_eq!(test.hub.serve(incoming).await, Ok(()));
    };
    session(61, Class::Complete, false, home, |mut peer| async move {
        let mut reader = open_complete(&mut peer, &[2, 1], 0).await;
        peer.credit(1 << 20).await.expect("sends the credit");
        let got = got(&mut peer, &mut reader).await.expect("a frame");
        assert_eq!(places(&got), [0, 1]);
        assert_eq!(decoded(&got, &[I64])[0], [10]);
        peer.sender.finish().expect("finishes");
        assert_eq!(peer.recv().await, Ok(None));
    });
}

/// A session of one commit of about three windows gets each frame. The peer sends a
/// credit only when the session has spent the last one, so a frame waits for each.
#[test]
fn sends_each_frame_of_a_commit_past_the_window_as_credits_come() {
    const LIMIT: u64 = 1 << 14;
    const FRAMES: i64 = 6;
    let home = |test: Test, incoming| async move {
        let mut writer = test.writer("a", &["value"]).await;
        let (clock, now) = (test.clock.clone(), test.now());
        test.tasks.spawn(async move {
            clock.sleep(SETTLE).await;
            for n in 0..FRAMES {
                write_wide(&mut writer, now, n);
            }
            clock.sleep(SETTLE).await;
        });
        assert_eq!(test.hub.serve(incoming).await, Ok(()));
    };
    session(63, Class::Complete, false, home, |mut peer| async move {
        let mut reader = open_complete(&mut peer, &[1, 2], LIMIT).await;
        let (mut firsts, mut spent, mut credit, mut grants) = (Vec::new(), 0, LIMIT, 0);
        for _ in 0..FRAMES {
            let got = got(&mut peer, &mut reader).await.expect("a frame");
            firsts.push(decoded(&got, &[STAMP])[0][0]);
            let series = usize::try_from(got.head.series).expect("fits");
            spent += types::frame::charge(series, got.body.len());
            if spent >= credit {
                credit = spent + LIMIT;
                grants += 1;
                peer.credit(credit).await.expect("sends the credit");
            }
        }
        assert!(grants >= 2, "{grants} grants, {spent} bytes");
        let expected: Vec<_> = (0..FRAMES).map(|n| firsts[0] + n * 1000).collect();
        assert_eq!(firsts, expected);
        peer.sender.finish().expect("finishes");
        assert_eq!(peer.recv().await, Ok(None));
    });
}

/// Over many runs, a session that spends its window and sends no credit gets each
/// frame before the one it missed at the next commit, in order, then `Behind` and the
/// finish.
#[test]
fn sends_each_frame_before_a_miss_then_behind() {
    for seed in 0..32 {
        let home = |test: Test, incoming| async move {
            let mut writer = test.writer("a", &["value"]).await;
            let (clock, now) = (test.clock.clone(), test.now());
            test.tasks.spawn(async move {
                clock.sleep(SETTLE).await;
                for n in 0..8 {
                    write_wide(&mut writer, now, n);
                }
                clock.sleep(SETTLE).await;
                // The frames that wait for credit miss at this commit.
                write_wide(&mut writer, now, 8);
                clock.sleep(SETTLE).await;
            });
            assert_eq!(test.hub.serve(incoming).await, Ok(()));
        };
        session(
            1000 + seed,
            Class::Complete,
            false,
            home,
            |mut peer| async move {
                let mut reader = open_complete(&mut peer, &[1, 2], 1 << 15).await;
                let mut firsts = Vec::new();
                while let Some(got) = got(&mut peer, &mut reader).await {
                    firsts.push(decoded(&got, &[STAMP])[0][0]);
                }
                assert!(!firsts.is_empty() && firsts.len() < 8, "{firsts:?}");
                let expected: Vec<_> = (0..)
                    .map(|n| firsts[0] + n * 1000)
                    .take(firsts.len())
                    .collect();
                assert_eq!(firsts, expected);
            },
        );
    }
}

/// A session of `value` and `time` spends the charge of the frame the peer builds,
/// not of the home's frame, which also holds `value-c`: it gets each frame until those
/// charges reach its window, then `Behind` at the next commit.
#[test]
fn charges_a_complete_session_by_the_frame_the_peer_builds() {
    const LIMIT: u64 = 1 << 16;
    let home = |test: Test, incoming| async move {
        let mut writer = test.writer("a", &["value", "value-c"]).await;
        let (clock, now) = (test.clock.clone(), test.now());
        test.tasks.spawn(async move {
            clock.sleep(SETTLE).await;
            for n in 0..16 {
                let stamps: Vec<_> = (now + n * 1000..now + (n + 1) * 1000).collect();
                let values = scrambled(&stamps);
                write_series(&mut writer, &[(1, &stamps), (2, &values), (5, &values)]);
            }
            clock.sleep(SETTLE).await;
            // The frames that wait for credit miss at this commit.
            write_wide(&mut writer, now, 16);
            clock.sleep(SETTLE).await;
        });
        assert_eq!(test.hub.serve(incoming).await, Ok(()));
    };
    session(62, Class::Complete, false, home, |mut peer| async move {
        let mut reader = open_complete(&mut peer, &[2, 1], LIMIT).await;
        let mut charges = Vec::new();
        while let Some(got) = got(&mut peer, &mut reader).await {
            let series = usize::try_from(got.head.series).expect("fits");
            charges.push(types::frame::charge(series, got.body.len()));
        }
        let (last, before) = charges.split_last().expect("a frame");
        assert!(charges.len() < 16, "{charges:?}");
        assert!(before.iter().sum::<u64>() < LIMIT, "{charges:?}");
        assert!(before.iter().sum::<u64>() + last >= LIMIT, "{charges:?}");
    });
}

#[test]
fn stops_an_open_whose_reply_finds_the_pool_empty_with_busy() {
    let result = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&result);
    let home = move |test: Test, incoming| async move {
        test.clock.sleep(SETTLE).await;
        let blocks = fill(&test.pool);
        let served = test.hub.serve(incoming).await;
        drop(blocks);
        *kept.lock().expect("not poisoned") = Some(served);
    };
    session(57, Class::Complete, false, home, |mut peer| async move {
        peer.open(Mode::Complete { limit_bytes: 0 }, &[1, 2]).await;
        let code = Code(wire::hub::BUSY);
        assert_eq!(peer.recv().await, Err(transport::Error::Reset { code }));
    });
    let served = result.lock().expect("not poisoned").take();
    let exhausted = block::Error::Exhausted {
        requested: 1,
        available: 64,
    };
    assert_eq!(served, Some(Err(serve::Error::Pool(exhausted.clone()))));
    assert_eq!(
        serve::Error::Pool(exhausted).to_string(),
        "the home's pool had no block for a reply: pool is full: asked for 1 bytes, 64 \
         bytes free"
    );
}

/// A latest session gets the newest frame at its open. `Opened` and `Head` take the
/// one small block that the pool has left, each in turn, and the ends of 11 series
/// need a larger one. The reset drops `Opened` and `Head` in flight.
#[test]
fn stops_a_session_whose_ends_find_the_pool_empty_with_busy() {
    const WIDE: [u128; 11] = [1, 2, 5, 10, 11, 12, 13, 14, 15, 16, 17];
    let result = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&result);
    let home = move |test: Test, incoming| async move {
        let names: Vec<_> = WIDE[1..].iter().map(|key| format!("v{key}")).collect();
        for (&key, name) in WIDE[3..].iter().zip(&names[2..]) {
            test.hub.define(Channel {
                key: channel::Key::from_u128(key),
                name: super::name(name),
                data_type: I64,
                index: channel::Key::from_u128(1),
            });
        }
        let names = [
            &["value", "value-c"][..],
            &names[2..].iter().map(String::as_str).collect::<Vec<_>>(),
        ]
        .concat();
        let mut writer = test.writer("a", &names).await;
        let (stamps, values) = ([test.now()], [1]);
        let series: Vec<_> = WIDE
            .iter()
            .map(|&key| (key, if key == 1 { &stamps[..] } else { &values[..] }))
            .collect();
        write_series(&mut writer, &series);
        test.clock.sleep(SETTLE).await;
        let small = test.pool.alloc(1).expect("a block");
        let blocks = fill(&test.pool);
        drop(small);
        let served = test.hub.serve(incoming).await;
        drop(blocks);
        *kept.lock().expect("not poisoned") = Some(served);
    };
    session(58, Class::Latest, false, home, |mut peer| async move {
        peer.open(Mode::Latest, &WIDE).await;
        let code = Code(wire::hub::BUSY);
        assert_eq!(peer.recv().await, Err(transport::Error::Reset { code }));
    });
    let served = result.lock().expect("not poisoned").take();
    let exhausted = block::Error::Exhausted {
        requested: 88,
        available: 64,
    };
    assert_eq!(served, Some(Err(serve::Error::Pool(exhausted))));
}
