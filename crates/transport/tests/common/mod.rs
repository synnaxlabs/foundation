//! The keys, transport config, port part, and messages that the transport test
//! binaries share.

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::rc::Rc;

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use block::{Block, Heap, Pool};
use sim::node::Node;
use transport::{Config, Port};
use types::ed25519::PublicKey;
use types::node::PrivateKey;
use types::time::Span;

pub(crate) const CLIENT: PrivateKey = PrivateKey([1; 32]);
pub(crate) const SERVER: PrivateKey = PrivateKey([2; 32]);
/// The port that the server binds.
pub(crate) const PORT: u16 = 4433;

/// The config of a transport on `node` with `key`: messages of at most 256 KiB, and a
/// pool of 1 MiB of its own.
pub(crate) fn config(node: &Node, tasks: env::tasks::Tasks, key: PrivateKey) -> Config {
    let pool = block::Config { budget: 1 << 20 };
    let memory = Heap::new(pool.reservation());
    Config {
        private_key: key,
        message_bytes_max: NonZeroUsize::new(1 << 18).expect("not zero"),
        window_bytes: 1 << 20,
        streams_max: NonZeroU32::new(16).expect("not zero"),
        idle: Span::from_nanos(10 * Span::SECOND.nanos()),
        clock: node.clock(),
        entropy: node.entropy(),
        tasks,
        pool: Rc::new(Pool::new(pool, memory)),
    }
}

/// The one part of a port that `node` binds at `port`, or at a free port for 0.
pub(crate) fn part(node: &Node, port: u16) -> transport::port::Part {
    let at = SocketAddr::new(node.addresses()[0], port);
    let port = Port::bind(&node.net(), at).expect("a port");
    port.split(NonZeroUsize::MIN).pop().expect("one part")
}

/// The public key of `key`.
pub(crate) fn public(key: &PrivateKey) -> PublicKey {
    let pair = Ed25519KeyPair::from_seed_unchecked(&key.0).expect("32 bytes");
    PublicKey::new(pair.public_key().as_ref().try_into().expect("32 bytes"))
        .expect("aws-lc makes no key of small order")
}

/// A block of `len` bytes from `pool`.
pub(crate) fn filled(pool: &Pool, len: usize) -> Block {
    let mut block = pool.alloc(len).expect("the pool has room");
    block.fill(0x5a);
    block.freeze()
}
