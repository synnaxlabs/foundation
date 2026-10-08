//! A read that fails inside the body of a message, when the peer resets the stream
//! before the message is whole, leaves the receiver with a list of at most 64 chunks,
//! not one sized by the message.

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
use crate::common::{CLIENT, PORT, SERVER, config, filled, part, public};

/// The most heap that the drop of the receiver gives back: a list of 64 chunks, since
/// each slot is 32 bytes.
const KEPT_MAX: usize = 2 << 10;
/// How long the server waits after the error before it drops the receiver.
const DROP: Span = Span::from_nanos(1_000_000_000);
/// How long the client lives after its reset: past the server's drop.
const LIVE: Span = Span::from_nanos(3_000_000_000);

/// The server's read, the polls of it that give `Pending`, and the net heap bytes
/// that the drop of the receiver then gives back.
type Out = (Option<Result<usize, Error>>, usize, usize);

pub(crate) fn main() {
    for len in [100_000, 240_000, 1 << 18] {
        let (read, pending, kept) = run(len);
        assert_eq!(
            read,
            Some(Err(Error::Reset { code: Code(0) })),
            "{len} bytes: the read"
        );
        assert!(pending > 0, "{len} bytes: the read waits for the body");
        assert!(
            kept <= KEPT_MAX,
            "{len} bytes: the receiver keeps {kept} bytes after a read that fails \
             inside a message"
        );
    }
}

/// The [`Out`] of the server's read of a message of `len` bytes, which the client
/// resets once that read gives `Pending`.
fn run(len: usize) -> Out {
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
            .dial(public(&SERVER), &[Address::Udp(address)])
            .await
            .expect("a session");
        let mut sender = session
            .open_sender(Class::Complete)
            .await
            .expect("a stream");
        sender.send(filled(&pool, len)).await.expect("sent");
        while waits.lock().expect("not poisoned").1 == 0 {
            node.clock().sleep(Span::from_nanos(100_000)).await;
        }
        drop(sender);
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
