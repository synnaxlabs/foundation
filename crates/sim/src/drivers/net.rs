//! The `env::net` drivers of a simulated node.

use std::future::poll_fn;
use std::io::{IoSlice, IoSliceMut};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::task::{Context, Poll, ready};

use env::net::udp::{self, Meta, Transmit, sender};
use env::net::{Connect, Error, listener, tcp};
use types::time::Monotonic;

use super::{Node, Owner};
use crate::net::tcp::{Key, Tcp};
use crate::net::udp::Bound;
use crate::state::lock;

const DELAYED: &str = "sim does not simulate delayed TCP sends yet";
const NO_NODE: &str = "sim does not simulate TCP to an address with no node yet";

impl Node {
    /// Runs `call` on the TCP sockets with the true time now. Drops the wakers that
    /// it gives after the lock.
    ///
    /// # Panics
    ///
    /// When the call meets a case that sim does not simulate yet.
    fn tcp<T, W>(&self, call: impl FnOnce(&mut Tcp<'_>, Monotonic) -> (T, W)) -> T {
        let mut state = lock(&self.shared);
        let now = state.now();
        let (value, wakers) = call(&mut state.net().tcp(), now);
        let yet = state.net().yet();
        drop(state);
        drop(wakers);
        if let Some(yet) = yet {
            panic!("{yet}");
        }
        value
    }
}

impl env::net::Driver for Node {
    fn udp(&self, config: &udp::Config) -> Result<Box<dyn udp::Driver>, Error> {
        let (life, bound) = {
            let mut state = lock(&self.shared);
            (
                state.life(self.node),
                state.net().udp().bind(self.node, config),
            )
        };
        Ok(Box::new(Socket {
            node: self.clone(),
            bound: bound?,
            owner: Owner::new(HALF, life),
        }))
    }

    fn connect<'a>(&'a self, config: &'a tcp::Config) -> Connect<'a> {
        let (remote, options) = (config.remote, config.options);
        assert!(!options.delayed, "{DELAYED}");
        let hosted = lock(&self.shared).hosts(remote.ip());
        assert!(hosted, "{NO_NODE}");
        Box::pin(async move {
            let connect =
                |tcp: &mut Tcp<'_>, now| tcp.connect(self.node, now, remote, options);
            let life = lock(&self.shared).life(self.node);
            let key = self.tcp(|tcp, now| (connect(tcp, now), ()))?;
            let stream: Box<dyn tcp::Driver> =
                Box::new(Stream::new(self.clone(), key, life));
            poll_fn(|cx| self.tcp(|tcp, _| tcp.connected(key, cx.waker()))).await?;
            Ok(stream)
        })
    }

    fn listen(&self, config: &tcp::Listen) -> Result<Box<dyn listener::Driver>, Error> {
        assert!(!config.options.delayed, "{DELAYED}");
        let life = lock(&self.shared).life(self.node);
        let (key, local) = self.tcp(|tcp, _| (tcp.listen(self.node, config), ()))?;
        Ok(Box::new(Listener {
            node: self.clone(),
            key,
            local,
            owner: Owner::new(LISTENER, life),
        }))
    }
}

/// A listener, in a panic.
const LISTENER: &str = "a TCP listener";
/// A stream, in a panic.
const STREAM: &str = "a TCP stream";

/// One TCP listener. A drop resets the streams that it did not accept.
struct Listener {
    node: Node,
    key: u64,
    local: SocketAddr,
    owner: Owner,
}

impl listener::Driver for Listener {
    fn local(&self) -> SocketAddr {
        self.local
    }

    fn poll_accept(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Box<dyn tcp::Driver>, Error>> {
        self.owner.check(&self.node);
        let key = ready!(self.node.tcp(|tcp, _| tcp.accept(self.key, cx.waker())));
        let stream = Stream::new(self.node.clone(), key, self.owner.life);
        Poll::Ready(Ok(Box::new(stream)))
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let mut state = lock(&self.node.shared);
        let now = state.now();
        let waker = state.net().tcp().unlisten(now, self.key);
        drop(state);
        drop(waker);
    }
}

/// One end of a TCP stream. A drop resets the peer, or lets the bytes and the FIN go
/// after a close.
struct Stream {
    node: Node,
    key: Key,
    owner: Owner,
}

impl Stream {
    /// The end `key` of `node`, opened in `life` of the node.
    fn new(node: Node, key: Key, life: u64) -> Self {
        Self {
            node,
            key,
            owner: Owner::new(STREAM, life),
        }
    }
}

impl tcp::Driver for Stream {
    fn local(&self) -> SocketAddr {
        self.key.local
    }

    fn peer(&self) -> SocketAddr {
        self.key.peer
    }

    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<Result<usize, Error>> {
        self.owner.check(&self.node);
        let key = self.key;
        (self.node).tcp(|tcp, now| tcp.read(now, key, cx.waker(), buffer))
    }

    fn poll_write(
        &mut self,
        cx: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<Result<usize, Error>> {
        self.owner.check(&self.node);
        let key = self.key;
        (self.node).tcp(|tcp, now| tcp.write(now, key, cx.waker(), buffers))
    }

    fn poll_close(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.owner.check(&self.node);
        let key = self.key;
        Poll::Ready(self.node.tcp(|tcp, now| (tcp.close(now, key), ())))
    }
}

impl Drop for Stream {
    /// Leaves a case that sim does not simulate yet for the run to raise, since a
    /// drop never panics.
    fn drop(&mut self) {
        let mut state = lock(&self.node.shared);
        let now = state.now();
        let wakers = state.net().tcp().drop(now, self.key);
        drop(state);
        drop(wakers);
    }
}

/// A socket or sender clone, in a panic.
const HALF: &str = "a socket half";

/// One UDP socket. A drop closes it.
struct Socket {
    node: Node,
    bound: Bound,
    owner: Owner,
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
            owner: Owner::new(HALF, self.owner.life),
        })
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        buffers: &mut [IoSliceMut<'_>],
        meta: &mut [Meta],
    ) -> Poll<Result<usize, Error>> {
        self.owner.check(&self.node);
        let waker = cx.waker().clone();
        let (poll, unused) = lock(&self.node.shared).net().udp().recv(
            self.bound.key,
            waker,
            buffers,
            meta,
        );
        drop(unused);
        poll.map(Ok)
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        let waker = lock(&self.node.shared).net().udp().close(self.bound.key);
        drop(waker);
    }
}

/// One sender clone of a socket.
struct Sender {
    node: Node,
    key: u64,
    owner: Owner,
}

impl sender::Driver for Sender {
    fn poll_send(
        &mut self,
        _: &mut Context<'_>,
        transmit: &Transmit<'_>,
    ) -> Poll<Result<(), Error>> {
        self.owner.check(&self.node);
        let mut state = lock(&self.node.shared);
        let now = state.now();
        Poll::Ready(state.net().udp().send(now, self.key, transmit))
    }
}
