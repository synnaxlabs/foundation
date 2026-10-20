//! The session of the program in the tests of `program`.

use std::rc::Rc;

use program::{Config, Error, Session};
use transport::Address;
use types::ed25519::PrivateKey;

use crate::agent::{SUBJECT, name};
use crate::net::{HOME, NODE, own_pool, public_key};

/// Connects to the home at `at` from `node` as [`SUBJECT`], signing with `key`, with a
/// session pool that holds a body at the cap while the transport sends it, and a pool
/// of frames as large.
pub(crate) async fn connect(
    node: &sim::node::Node,
    tasks: env::tasks::Tasks,
    at: Address,
    key: PrivateKey,
) -> Result<Session, Error> {
    connect_with(node, tasks, at, key, ample(), ample()).await
}

/// As [`connect`], where the session sends from `pool` and its readers take frames
/// from `frames`, and the transport receives into a pool of its own.
pub(crate) async fn connect_with(
    node: &sim::node::Node,
    tasks: env::tasks::Tasks,
    at: Address,
    key: PrivateKey,
    pool: Rc<block::Pool>,
    frames: Rc<block::Pool>,
) -> Result<Session, Error> {
    connect_receiving(node, tasks, at, key, pool, frames, own_pool()).await
}

/// As [`connect_with`], where each message that the transport receives lands in a block
/// of `received`.
pub(crate) async fn connect_receiving(
    node: &sim::node::Node,
    tasks: env::tasks::Tasks,
    at: Address,
    key: PrivateKey,
    pool: Rc<block::Pool>,
    frames: Rc<block::Pool>,
    received: Rc<block::Pool>,
) -> Result<Session, Error> {
    let config = transport::client::Config {
        net: node.net(),
        clock: node.clock(),
        entropy: node.entropy(),
        tasks: tasks.clone(),
        pool: received,
    };
    let transport = transport::Client::new(config).expect("a client");
    let config = Config {
        via: NODE,
        node: public_key(&HOME),
        addresses: vec![at],
        subject: name(SUBJECT),
        key,
        clock: node.clock(),
        entropy: node.entropy(),
        tasks,
        pool,
        frames: Rc::clone(&frames),
        reads: frames,
    };
    Session::connect(&transport, config).await
}

/// A pool that holds a body at the cap while the transport sends it.
pub(crate) fn ample() -> Rc<block::Pool> {
    let config = block::Config::new(2 << 24).expect("the budget fits");
    Rc::new(block::Pool::new(
        config.clone(),
        block::Heap::new(config.reservation()),
    ))
}
