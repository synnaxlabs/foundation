//! The `env::serial` drivers of a simulated node.

use std::future::ready;
use std::task::{Context, Poll};

use env::serial::{Config, Error, Open, port};

use super::{Node, Owner};
use crate::serial::End;
use crate::state::lock;

impl env::serial::Driver for Node {
    fn open<'a>(&'a self, config: &'a Config) -> Open<'a> {
        let (life, end) = {
            let mut state = lock(&self.shared);
            (
                state.life(self.node),
                state.serial().open(self.node, config),
            )
        };
        let port = end.map(|end| -> Box<dyn port::Driver> {
            Box::new(Port {
                node: self.clone(),
                end,
                life,
                owner: Owner::new("a serial port"),
            })
        });
        Box::pin(ready(port))
    }
}

/// One open end of a line. A drop closes it, unless a crash of its node did.
struct Port {
    node: Node,
    end: End,
    /// The life of the node at the open.
    life: u64,
    owner: Owner,
}

impl Port {
    /// Binds the port to the sim thread that polls it first.
    ///
    /// # Panics
    ///
    /// After a crash of its node, and as [`Owner::check`] does.
    fn check(&self) {
        let node = self.node.node;
        let life = lock(&self.node.shared).life(node);
        assert!(
            life == self.life,
            "a serial port of node {node} polls after a crash of the node"
        );
        self.owner.check(&self.node);
    }
}

impl port::Driver for Port {
    fn poll_read(
        &mut self,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<Result<usize, Error>> {
        self.check();
        let waker = cx.waker().clone();
        let (poll, unused) = lock(&self.node.shared)
            .serial()
            .read(self.end, waker, buffer);
        drop(unused);
        poll
    }

    fn poll_write(
        &mut self,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<Result<usize, Error>> {
        self.check();
        let waker = cx.waker().clone();
        let (poll, unused) = {
            let mut state = lock(&self.node.shared);
            let now = state.now();
            state.serial().write(now, self.end, waker, bytes)
        };
        drop(unused);
        poll
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        let wakers = {
            let mut state = lock(&self.node.shared);
            let current = state.life(self.node.node) == self.life;
            current.then(|| state.serial().close(self.end))
        };
        drop(wakers);
    }
}
