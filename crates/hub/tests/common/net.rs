//! A transport on a sim node, the keys it proves, and the accept of a hub stream, for
//! the tests and benches that serve a hub stream.

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::rc::Rc;

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use block::Pool;
use env::tasks::Tasks;
use transport::stream::Incoming;
use transport::{Port, Transport};
use types::ed25519::{PrivateKey, PublicKey};
use types::time::Span;
use wire::Protocol;

/// The UDP port of each transport.
pub(crate) const PORT: u16 = 7000;
pub(crate) const HOME: PrivateKey = PrivateKey([1; 32]);
pub(crate) const PEER: PrivateKey = PrivateKey([2; 32]);
/// The node key of the hub under test, which each hello names as its `via`.
pub(crate) const NODE: types::node::Key = types::node::Key::from_u128(1);

pub(crate) fn public_key(key: &PrivateKey) -> PublicKey {
    let pair = Ed25519KeyPair::from_seed_unchecked(&key.0).expect("a key pair");
    PublicKey::new(pair.public_key().as_ref().try_into().expect("32 bytes"))
        .expect("a public key")
}

/// A transport of `node` at `PORT` that proves `key` and takes messages of at most
/// `message` bytes.
pub(crate) fn transport(
    node: &sim::node::Node,
    tasks: &Tasks,
    pool: &Rc<Pool>,
    key: PrivateKey,
    message: usize,
) -> Transport {
    transport_sized(node, tasks, pool, key, (message, 1 << 20))
}

/// As [`transport`], with messages of at most `sizes.0` bytes and a window of
/// `sizes.1` bytes.
pub(crate) fn transport_sized(
    node: &sim::node::Node,
    tasks: &Tasks,
    pool: &Rc<Pool>,
    key: PrivateKey,
    (message, window): (usize, usize),
) -> Transport {
    let at = SocketAddr::new(node.addresses()[0], PORT);
    let mut parts = Port::bind(&node.net(), at)
        .expect("binds")
        .split(NonZeroUsize::MIN);
    let config = transport::Config {
        private_key: key,
        message_bytes_max: NonZeroUsize::new(message).expect("not 0"),
        window_bytes: window,
        streams_max: NonZeroU32::new(16).expect("not 0"),
        idle: Span::from_nanos(60 * Span::SECOND.nanos()),
        clock: node.clock(),
        entropy: node.entropy(),
        tasks: tasks.clone(),
        pool: Rc::clone(pool),
    };
    Transport::new(config, parts.pop().expect("one part")).expect("a transport")
}

/// A pool for a transport, so a test that fills the hub's pool does not fill it.
pub(crate) fn own_pool() -> Rc<Pool> {
    let config = block::Config { budget: 1 << 20 };
    Rc::new(Pool::new(
        config.clone(),
        block::Heap::new(config.reservation()),
    ))
}

/// The first session of `transport`, and its first stream, after the stream's header.
pub(crate) async fn accept(transport: &Transport) -> (transport::Session, Incoming) {
    let session = transport.accept().await.expect("a session");
    let incoming = stream(&session).await;
    (session, incoming)
}

/// The next stream of `session`, after its header.
pub(crate) async fn stream(session: &transport::Session) -> Incoming {
    let mut incoming = session.accept().await.expect("a stream");
    header(&mut incoming).await;
    incoming
}

/// Reads the header of `incoming`, and checks that it names the hub.
pub(crate) async fn header(incoming: &mut Incoming) {
    let header = incoming.receiver.recv().await.expect("a header");
    let header = header.expect("the header comes before the finish");
    assert_eq!(
        wire::header::decode(&header),
        Ok((Protocol::Hub, &[][..])),
        "the stream is of the hub"
    );
}
