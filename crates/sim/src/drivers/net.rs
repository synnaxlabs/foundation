//! The `env::net` drivers of a simulated node.

use std::io::IoSliceMut;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use env::net::udp::{self, Meta, Transmit, sender};
use env::net::{Connect, Error, listener, tcp};

use super::Node;
use crate::net::Bound;
use crate::state::lock;

impl env::net::Driver for Node {
    fn udp(&self, config: &udp::Config) -> Result<Box<dyn udp::Driver>, Error> {
        let bound = lock(&self.shared).bind(self.node, config)?;
        Ok(Box::new(Socket {
            node: self.clone(),
            bound,
            thread: OnceLock::new(),
        }))
    }

    fn connect<'a>(&'a self, _: &'a tcp::Config) -> Connect<'a> {
        panic!("sim does not simulate TCP yet")
    }

    fn listen(&self, _: &tcp::Listen) -> Result<Box<dyn listener::Driver>, Error> {
        panic!("sim does not simulate TCP yet")
    }
}

/// One UDP socket. A drop closes it.
struct Socket {
    node: Node,
    bound: Bound,
    /// The thread of the first receive.
    thread: OnceLock<u64>,
}

impl udp::Driver for Socket {
    fn local(&self) -> SocketAddr {
        self.bound.local
    }

    fn send_batch_max(&self) -> NonZeroUsize {
        self.bound.send_batch_max
    }

    fn recv_batch_max(&self) -> NonZeroUsize {
        self.bound.recv_batch_max
    }

    fn sender(&self) -> Box<dyn sender::Driver> {
        Box::new(Sender {
            node: self.node.clone(),
            key: self.bound.key,
            thread: OnceLock::new(),
        })
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [Meta],
    ) -> Poll<Result<usize, Error>> {
        self.node.own(&self.thread, "a socket half");
        let waker = cx.waker().clone();
        let (poll, unused) =
            lock(&self.node.shared).recv(self.bound.key, waker, buffers, meta);
        drop(unused);
        poll.map(Ok)
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let waker = lock(&self.node.shared).close(self.bound.key);
        drop(waker);
    }
}

/// One sender clone of a socket.
struct Sender {
    node: Node,
    key: u64,
    thread: OnceLock<u64>,
}

impl sender::Driver for Sender {
    fn poll_send(
        &mut self,
        _: &mut Context<'_>,
        transmit: &Transmit<'_>,
    ) -> Poll<Result<(), Error>> {
        self.node.own(&self.thread, "a socket half");
        Poll::Ready(lock(&self.node.shared).send(self.key, transmit))
    }
}
