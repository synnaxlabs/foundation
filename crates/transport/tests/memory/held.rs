//! A message that waits for a block from the pool is held on the heap, outside the
//! pool. When its stream resets or its session closes, the read that gives the error
//! frees it, though the caller keeps the receiver.

use std::future::poll_fn;
use std::net::SocketAddr;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use sim::Sim;
use sim::node::Node;
use transport::{Address, Class, Code, Error, Transport};
use types::time::Span;

use crate::common::{CLIENT, PORT, SERVER, config, filled, part, public};
use crate::{ALLOCATOR, fill};

const LEN: usize = 60_000;
/// When the client ends the stream or the session, after its send.
const END: Span = Span::from_nanos(500_000_000);
/// When the server reads the end, after the message waits for a block.
const READ: Span = Span::from_nanos(1_000_000_000);

/// How the client ends the message that waits.
#[derive(Clone, Copy, Debug)]
enum End {
    /// It drops its sender, which resets the stream with code 0.
    Reset,
    /// It closes the session with code 7.
    Close,
}

pub(crate) fn main() {
    for (end, error) in [
        (End::Reset, Error::Reset { code: Code(0) }),
        (End::Close, Error::PeerClosed { code: Code(7) }),
    ] {
        let (read, freed) = run(end);
        assert_eq!(read, Some(error), "{end:?}: the read after the end");
        assert_eq!(
            freed, LEN,
            "{end:?}: the read that gives the error frees the buffer of the message, \
             made at its length"
        );
    }
}

/// The error of the server's read after the client ends the stream in the way of
/// `end`, and the heap bytes that read freed.
fn run(end: End) -> (Option<Error>, usize) {
    let mut sim = Sim::new(sim::Config::default());
    let client = sim.node(sim::node::Config::default());
    let server = sim.node(sim::node::Config::default());
    let at = SocketAddr::new(server.addresses()[0], PORT);
    let out = Arc::new(Mutex::new((None, 0)));
    serve(&server, Arc::clone(&out));
    sim.run_on(&client, move |node, tasks| async move {
        let config = config(&node, tasks, CLIENT);
        let pool = Rc::clone(&config.pool);
        let transport = Transport::new(config, part(&node, 0)).expect("a transport");
        let session = transport
            .dial(public(&SERVER), &[Address::Udp(at)])
            .await
            .expect("a session");
        let mut sender = session
            .open_sender(Class::Complete)
            .await
            .expect("a stream");
        sender.send(filled(&pool, LEN)).await.expect("sent");
        let clock = node.clock();
        clock.sleep(END).await;
        match end {
            End::Reset => drop(sender),
            End::Close => session.close(Code(7)),
        }
        clock.sleep(READ).await;
        clock.sleep(END).await;
    })
    .expect("the run ends");
    let out = out.lock().expect("not poisoned");
    (out.0.clone(), out.1)
}

/// Starts the server on `node`. It fills its pool, lets the client's message wait for
/// a block, and puts the error of its read after the end and the bytes that read
/// freed in `out`.
fn serve(node: &Node, out: Arc<Mutex<(Option<Error>, usize)>>) {
    let own = node.clone();
    let shard = env::shards::Config {
        name: "server".into(),
        core: None,
    };
    let started = node.shards().start(shard, move |tasks| async move {
        let config = config(&own, tasks, SERVER);
        let full = fill(&config.pool, LEN);
        let transport = Transport::new(config, part(&own, PORT)).expect("a transport");
        let session = transport.accept().await.expect("a session");
        let mut receiver = session.accept().await.expect("a stream").receiver;
        let clock = own.clock();
        let (read, freed) = {
            let mut recv = pin!(receiver.recv());
            while transport.status().waited == Span::ZERO {
                let read = poll_once(recv.as_mut()).await;
                assert!(read.is_pending(), "no block: {read:?}");
                clock.sleep(Span::MILLISECOND).await;
            }
            clock.sleep(READ).await;
            let before = ALLOCATOR.held();
            let read = poll_once(recv.as_mut()).await;
            (read, before.saturating_sub(ALLOCATOR.held()))
        };
        let error = match read {
            Poll::Ready(Err(error)) => Some(error),
            _ => None,
        };
        *out.lock().expect("not poisoned") = (error, freed);
        drop((receiver, full));
    });
    drop(started.expect("a shard"));
}

/// Polls `future` once.
async fn poll_once<F: Future + ?Sized>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
}
