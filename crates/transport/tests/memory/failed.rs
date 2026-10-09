//! A read that fails inside the body of a message, when the peer resets the stream or
//! closes the session before the message is whole, leaves the receiver with a list of
//! at most 64 chunks, not one sized by the message.

use std::cell::OnceCell;
use std::future::poll_fn;
use std::net::SocketAddr;
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use sim::Sim;
use sim::node::Node;
use transport::{Address, Class, Code, Error, Transport};
use types::time::Span;

use crate::ALLOCATOR;
use crate::common::{CLIENT, PORT, SERVER, config, filled, part};

/// The most heap that the drop of the receiver gives back: a list of 64 chunks, since
/// each slot is 32 bytes.
const KEPT_MAX: usize = 2 << 10;
/// The heap of the cell that holds a closed session's error, which the drop of its
/// receiver frees: an `Rc` box, with its two counts.
const CLOSED: usize = 2 * size_of::<usize>() + size_of::<OnceCell<Error>>();
/// How long the server waits after the error before it drops the receiver.
const DROP: Span = Span::from_nanos(1_000_000_000);
/// How long the client lives after its reset: past the server's drop.
const LIVE: Span = Span::from_nanos(3_000_000_000);

/// The server's read, the polls of it that give `Pending`, and the net heap bytes
/// that the drop of the receiver then gives back.
type Out = (Option<Result<usize, Error>>, usize, usize);

/// How the client ends the message in hand.
#[derive(Clone, Copy, Debug)]
enum End {
    /// It drops its sender, which resets the stream with code 0.
    Reset,
    /// It closes the session with code 7.
    Close,
}

pub(crate) fn main() {
    for end in [End::Reset, End::Close] {
        for len in [100_000, 240_000, 1 << 18] {
            let (read, _, kept) = run(end, len);
            let (error, kept_max) = match end {
                End::Reset => (Error::Reset { code: Code(0) }, KEPT_MAX),
                End::Close => (Error::PeerClosed { code: Code(7) }, KEPT_MAX + CLOSED),
            };
            assert_eq!(read, Some(Err(error)), "{end:?}, {len} bytes: the read");
            assert!(
                kept <= kept_max,
                "{end:?}, {len} bytes: the receiver keeps {kept} bytes after a read \
                 that fails inside a message"
            );
        }
    }
}

/// The [`Out`] of the server's read of a message of `len` bytes, which the client
/// ends in the way of `end` once that read gives `Pending` or ends. So a read that
/// gives the error gave `Pending` first.
fn run(end: End, len: usize) -> Out {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let address = SocketAddr::new(server.addresses()[0], PORT);
    let out = Arc::new(Mutex::new((None, 0, 0)));
    serve(&server, Arc::clone(&out));
    let waits = Arc::clone(&out);
    sim.run_on(&client, move |node, tasks| async move {
        let config = config(&node, tasks, CLIENT);
        let pool = Rc::clone(&config.pool);
        let transport = Transport::new(config, part(&node, 0)).expect("a transport");
        let session = transport
            .dial(SERVER.public(), &[Address::Udp(address)])
            .await
            .expect("a session");
        let mut sender = session
            .open_sender(Class::Complete)
            .await
            .expect("a stream");
        sender.send(filled(&pool, len)).await.expect("sent");
        while matches!(*waits.lock().expect("not poisoned"), (None, 0, _)) {
            node.clock().sleep(Span::from_nanos(100_000)).await;
        }
        match end {
            End::Reset => drop(sender),
            End::Close => session.close(Code(7)),
        }
        node.clock().sleep(LIVE).await;
    })
    .expect("the run ends");
    out.lock().expect("not poisoned").clone()
}

/// Starts the server on `node`. It polls its read each millisecond until it is ready,
/// and puts the count of polls that give `Pending` in `out` as they occur. After the
/// read it drops the receiver and puts the [`Out`] in `out`.
fn serve(node: &Node, out: Arc<Mutex<Out>>) {
    let own = node.clone();
    let shard = env::shards::Config {
        name: "server".into(),
        core: None,
    };
    let started = node.shards().start(shard, move |tasks| async move {
        let config = config(&own, tasks, SERVER);
        let transport = Transport::new(config, part(&own, PORT)).expect("a transport");
        let session = transport.accept().await.expect("a session");
        let mut receiver = session.accept().await.expect("a stream").receiver;
        let clock = own.clock();
        let mut pending = 0;
        let read = {
            let mut recv = pin!(receiver.recv());
            loop {
                if let Poll::Ready(read) =
                    poll_fn(|cx| Poll::Ready(recv.as_mut().poll(cx))).await
                {
                    break read;
                }
                pending += 1;
                out.lock().expect("not poisoned").1 = pending;
                clock.sleep(Span::MILLISECOND).await;
            }
        };
        let read = read.map(|block| block.map_or(0, |block| block.len()));
        clock.sleep(DROP).await;
        let before = ALLOCATOR.held();
        drop(receiver);
        let kept = before.saturating_sub(ALLOCATOR.held());
        *out.lock().expect("not poisoned") = (Some(read), pending, kept);
    });
    drop(started.expect("a shard"));
}
