use std::cell::Cell;
use std::future::{Future as _, poll_fn};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;

use env::clock::Clock;
use env::net::{Net, Tcp, tcp};
use env::rng::Rng;
use sim::{Sim, node};
use types::time::Span;

use super::{Client, Node, Reads, answer};
use crate::connection::CHUNKS;
use crate::connection::Manager;
use crate::ffi::{self, Status};
use crate::server::Server;

/// The port of the test server.
const PORT: u16 = 4840;

/// The Double variable of the test server.
const VALUE: Node = Node {
    namespace: 1,
    key: 1000,
};

/// A `ByteString` whose answer needs 8 chunks, [`CHUNKS`], from [`read_large`].
const FITS: Node = Node {
    namespace: 1,
    key: 1002,
};

/// A `ByteString` whose answer needs 9 chunks, one past [`CHUNKS`], from
/// [`read_large`].
const PAST: Node = Node {
    namespace: 1,
    key: 1003,
};

/// The size of a chunk, the send buffer size of a channel.
const CHUNK: usize = 1 << 16;

/// The current time of the server, a `DateTime`.
const TIME: Node = Node {
    namespace: 0,
    key: ffi::test::TIME,
};

/// The side of a client in a run of [`serve`].
struct Peer<'s> {
    /// The test server.
    server: &'s Server,
    clock: Clock,
    net: Net,
    rng: Rng,
    /// The endpoint of the test server.
    url: String,
    /// The value of [`VALUE`].
    value: f64,
    /// The server does not run while it is set.
    paused: Rc<Cell<bool>>,
}

impl Peer<'_> {
    async fn connect(&mut self, url: &str) -> Result<Client, Status> {
        let (clock, net) = (Clock::clone(&self.clock), Net::clone(&self.net));
        Client::connect(clock, net, &mut self.rng, url).await
    }

    async fn connected(&mut self) -> Client {
        let url = self.url.clone();
        self.connect(&url).await.expect("the client connects")
    }
}

/// Runs `test` on a node of a run of `seed`, and gives its value and the digest of
/// the run.
fn run<T: Send + 'static>(
    seed: u64,
    test: impl AsyncFnOnce(node::Node) -> T + Send + 'static,
) -> (T, u64) {
    let mut sim = Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let node = sim.node(node::Config::default());
    let value = sim
        .run_on(&node, move |node, _| test(node))
        .expect("the run ends");
    (value, sim.digest())
}

/// Starts a test server on `node`, with room for `sessions` sessions and a Double at
/// [`VALUE`] from its random values, runs `body` while the server runs, except while
/// [`Peer::paused`] is set, then closes the server.
async fn serve<T>(
    node: &node::Node,
    sessions: u16,
    body: impl AsyncFnOnce(Peer<'_>) -> T,
) -> T {
    let mut rng = node.entropy().rng();
    let local = SocketAddr::new(node.addresses()[0], PORT);
    let manager = Manager::listening(node.clock(), node.net(), local, 4, &mut rng)
        .expect("the port is free");
    // SAFETY: the close below deletes the server before the manager drops.
    let server = unsafe { Server::start(&manager, c"", sessions) };
    let value = f64::from(u32::try_from(rng.below(1 << 20)).expect("fits"));
    server.add_double(VALUE, value);
    let peer = Peer {
        server: &server,
        clock: node.clock(),
        net: node.net(),
        rng,
        url: format!("opc.tcp://{local}"),
        value,
        paused: Rc::default(),
    };
    let paused = Rc::clone(&peer.paused);
    let out = {
        let mut server_drive = pin!(manager.drive(|_| {
            manager.events().run();
            Poll::<()>::Pending
        }));
        let mut body = pin!(body(peer));
        poll_fn(|cx| {
            if !paused.get() {
                assert!(server_drive.as_mut().poll(cx).is_pending(), "it never ends");
            }
            body.as_mut().poll(cx)
        })
        .await
    };
    server.close(&manager).await;
    out
}

/// The value of [`VALUE`] and the answer of a read of it, in a run of `seed`.
fn spike(seed: u64) -> ((f64, Result<f64, Status>), u64) {
    run(seed, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            (peer.value, client.read(VALUE).await)
        })
        .await
    })
}

#[test]
fn a_client_reads_a_double_and_a_seed_replays() {
    let ((value, read), digest) = spike(7);
    assert_eq!(read, Ok(value));
    assert_eq!(spike(7), ((value, Ok(value)), digest));
    let ((other, read), _) = spike(8);
    assert_eq!(read, Ok(other));
    assert_ne!(other.to_bits(), value.to_bits());
}

#[test]
fn a_read_of_an_unknown_node_fails_with_its_status() {
    let (read, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            let unknown = Node { key: 1001, ..VALUE };
            client.read(unknown).await
        })
        .await
    });
    assert_eq!(read, Err(Status::BAD_NODE_ID_UNKNOWN));
}

#[test]
fn a_read_of_another_type_fails_with_a_type_mismatch() {
    let (read, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            client.read(TIME).await
        })
        .await
    });
    assert_eq!(read, Err(Status::BAD_TYPE_MISMATCH));
}

#[test]
fn a_connect_with_no_listener_fails_with_its_status() {
    let (status, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let address = SocketAddr::new(node.addresses()[0], PORT + 1);
            peer.connect(&format!("opc.tcp://{address}")).await.err()
        })
        .await
    });
    assert_eq!(status, Some(Status::BAD_CONNECTION_REJECTED));
}

#[test]
fn a_connect_to_a_url_that_open62541_refuses_fails_with_its_status() {
    let (statuses, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut statuses = Vec::new();
            for url in ["http://127.0.0.1:4840", "opc.tcp://a\0b:4840"] {
                statuses.push(peer.connect(url).await.err());
            }
            statuses
        })
        .await
    });
    assert_eq!(
        statuses,
        [
            Some(Status::BAD_TCP_ENDPOINT_URL_INVALID),
            Some(Status::BAD_TCP_ENDPOINT_URL_INVALID)
        ]
    );
}

#[test]
fn a_read_after_the_server_closes_fails_and_open62541_refuses_the_next() {
    let (reads, _) = run(0, async |node| {
        let mut client = serve(&node, 4, async |mut peer| peer.connected().await).await;
        (client.read(VALUE).await, client.read(VALUE).await)
    });
    assert_eq!(
        reads,
        (
            Err(Status::BAD_SECURE_CHANNEL_CLOSED),
            Err(Status::BAD_SERVER_NOT_CONNECTED)
        )
    );
}

/// The server answers a read whose future dropped before the next read, and the
/// callback drops that answer, so the next read gets its own answer.
#[test]
fn a_read_after_a_dropped_read_gets_its_own_answer() {
    let (reads, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            {
                let mut dropped = pin!(client.read(VALUE));
                poll_fn(|cx| {
                    assert!(dropped.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
            }
            peer.clock.sleep(Span::from_nanos(10_000_000)).await;
            (
                client.read(TIME).await,
                client.read(VALUE).await,
                peer.value,
            )
        })
        .await
    });
    let (time, value, expected) = reads;
    assert_eq!(
        (time, value),
        (Err(Status::BAD_TYPE_MISMATCH), Ok(expected))
    );
}

/// Starts `count` reads of `client`, each polled once and then dropped.
async fn drop_reads(client: &mut Client, count: u16) {
    for _ in 0..count {
        let mut read = pin!(client.read(VALUE));
        poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending(), "a read waits");
            Poll::Ready(())
        })
        .await;
    }
}

/// Without the bound on the reads in flight, the server answers the 257 requests of
/// the reads and the connect in one pass, and its 257th send closes the channel.
#[test]
fn a_read_after_255_dropped_reads_gives_the_value() {
    let ((read, value), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            drop_reads(&mut client, 255).await;
            let read = client.read(VALUE).await;
            client.close().await;
            (read, peer.value)
        })
        .await
    });
    assert_eq!(read, Ok(value));
}

/// While the server stops, a read after `dropped` dropped reads starts 2 s after them.
/// Gives its answer and the time it took.
fn read_after(dropped: u16) -> (Result<f64, Status>, Span) {
    let (out, _) = run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            peer.paused.set(true);
            drop_reads(&mut client, dropped).await;
            peer.clock.sleep(Span::from_nanos(2_000_000_000)).await;
            let start = peer.clock.now();
            let read = client.read(VALUE).await;
            let took = peer.clock.now() - start;
            peer.paused.set(false);
            client.close().await;
            (read, took)
        })
        .await
    });
    out
}

/// Below the bound, a read sends its request and times out. At the bound, the dropped
/// reads stay in flight, so it fails at the end of its wait and sends nothing.
#[test]
fn a_read_at_the_bound_fails_when_no_read_ends() {
    let (below, below_took) = read_after(15);
    let (at, at_took) = read_after(16);
    assert_eq!(
        (below, at),
        (
            Err(Status::BAD_TIMEOUT),
            Err(Status::BAD_TOO_MANY_OPERATIONS)
        )
    );
    let timeout = Span::from_nanos(5_000_000_000)..=Span::from_nanos(6_000_000_000);
    assert!(timeout.contains(&below_took), "{below_took:?}");
    assert!(timeout.contains(&at_took), "{at_took:?}");
}

/// Reads [`VALUE`] while [`Peer::paused`] is set, except for one pass of the server
/// `at` after the read starts. Gives its answer and the time it took.
async fn read_with_pass(
    client: &mut Client,
    peer: &Peer<'_>,
    at: Span,
) -> (Result<f64, Status>, Span) {
    let start = peer.clock.now();
    let mut pass = pin!(peer.clock.sleep(at));
    let (mut woke, mut passed) = (false, false);
    let mut read = pin!(client.read(VALUE));
    let read = poll_fn(|cx| {
        if !woke && pass.as_mut().poll(cx).is_ready() {
            woke = true;
            peer.paused.set(false);
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        if woke && !passed {
            passed = true;
            peer.paused.set(true);
        }
        read.as_mut().poll(cx)
    })
    .await;
    (read, peer.clock.now() - start)
}

/// A read at the bound whose wait ends 3 s in still times out 5 s after it starts.
#[test]
fn the_timeout_of_a_read_counts_its_wait() {
    let ((read, took), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            peer.paused.set(true);
            drop_reads(&mut client, 16).await;
            let out =
                read_with_pass(&mut client, &peer, Span::from_nanos(3_000_000_000))
                    .await;
            peer.paused.set(false);
            client.close().await;
            out
        })
        .await
    });
    assert_eq!(read, Err(Status::BAD_TIMEOUT));
    let timeout = Span::from_nanos(5_000_000_000)..=Span::from_nanos(6_000_000_000);
    assert!(timeout.contains(&took), "{took:?}");
}

/// The answers of 16 dropped reads end them all, so a read after one more dropped read
/// sends its request at once and gets its answer in one pass of the server.
#[test]
fn the_answers_of_dropped_reads_end_them() {
    let ((read, value), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            drop_reads(&mut client, 16).await;
            peer.clock.sleep(Span::from_nanos(1_000_000_000)).await;
            peer.paused.set(true);
            drop_reads(&mut client, 1).await;
            let (read, _) =
                read_with_pass(&mut client, &peer, Span::from_nanos(1_000_000_000))
                    .await;
            peer.paused.set(false);
            client.close().await;
            (read, peer.value)
        })
        .await
    });
    assert_eq!(read, Ok(value));
}

/// A stalled server keeps the request of each read that timed out, and answers them
/// all in one pass when it runs again. Without the bound on its requests, 17 rounds of
/// 16 dropped reads send 272, and the 257th answer closes the channel.
#[test]
fn a_read_after_272_dropped_reads_on_a_stalled_server_gives_the_value() {
    let ((read, value), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            peer.paused.set(true);
            for _ in 0..17 {
                drop_reads(&mut client, 16).await;
                peer.clock.sleep(Span::from_nanos(6_000_000_000)).await;
            }
            peer.paused.set(false);
            peer.clock.sleep(Span::from_nanos(10_000_000)).await;
            let read = client.read(VALUE).await;
            client.close().await;
            (read, peer.value)
        })
        .await
    });
    assert_eq!(read, Ok(value));
}

/// Reads [`FITS`] and then [`PAST`] from a server that sends at most `chunks` chunks in
/// a message.
fn read_large(chunks: u32) -> (Result<f64, Status>, Result<f64, Status>) {
    let (reads, _) = run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            peer.server.add_bytes(FITS, 7 * CHUNK);
            peer.server.add_bytes(PAST, 8 * CHUNK);
            peer.server.set_chunks(chunks);
            let mut client = peer.connected().await;
            let reads = (client.read(FITS).await, client.read(PAST).await);
            client.close().await;
            reads
        })
        .await
    });
    reads
}

/// The answer of [`FITS`] arrives and is not a Double. The answer of [`PAST`] needs
/// more than [`CHUNKS`] chunks. The test server refuses to send it, and sends an
/// error that closes the channel. A server that sends it makes the client close the
/// channel at the chunk past the bound.
#[test]
fn an_answer_of_more_chunks_than_the_bound_fails() {
    let large = (
        Err(Status::BAD_TYPE_MISMATCH),
        Err(Status::BAD_SECURE_CHANNEL_CLOSED),
    );
    assert_eq!(read_large(CHUNKS), large);
    assert_eq!(read_large(0), large);
}

/// A drop of a client with an active session ends, and calls the callback of each
/// read in flight, so the answer of a client must outlive its delete. The server then
/// closes. The test reads through C on the private client, since `read` borrows the
/// client, so no public call keeps a read in flight across a drop.
#[test]
fn a_drop_of_a_connected_client_ends_and_answers_each_read() {
    let (value, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let client = peer.connected().await;
            let outside = Reads::default();
            let mut request = 0;
            // SAFETY: the client lives, and `outside` outlives it.
            let status = Status(unsafe {
                ffi::shim_client_read(
                    client.raw.as_ptr(),
                    VALUE.namespace,
                    VALUE.key,
                    answer,
                    std::ptr::from_ref(&outside).cast_mut().cast(),
                    &raw mut request,
                )
            });
            assert_eq!(status, Status::GOOD);
            outside.pending.set(1);
            outside.request.set(Some(request));
            drop(client);
            outside.value.take()
        })
        .await
    });
    assert_eq!(value, Some(Err(Status::BAD_SESSION_CLOSED)));
}

/// A server with room for one session takes a second client only after the first
/// closes its session.
#[test]
fn a_close_frees_the_session_on_the_server_and_a_drop_does_not() {
    let (connects, _) = run(0, async |node| {
        serve(&node, 1, async |mut peer| {
            peer.connected().await.close().await;
            drop(peer.connected().await);
            let url = peer.url.clone();
            peer.connect(&url).await.err()
        })
        .await
    });
    assert_eq!(connects, Some(Status::BAD_TOO_MANY_SESSIONS));
}

/// Connects to a listener on `node` that gives `accepted` each stream it accepts, and
/// gives the status of the failed connect and the time it took.
async fn connect_to_listener(
    node: &node::Node,
    mut accepted: impl FnMut(Tcp),
) -> (Status, Span) {
    let local = SocketAddr::new(node.addresses()[0], PORT);
    let listen = tcp::Listen {
        local,
        backlog: 4,
        options: tcp::Options {
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 16,
            unsent_bytes_max: NonZeroUsize::new(1 << 14).expect("not 0"),
            delayed: false,
        },
    };
    let mut listener = node.net().listen(&listen).expect("the port is free");
    let clock = node.clock();
    let start = clock.now();
    let mut rng = node.entropy().rng();
    let url = format!("opc.tcp://{local}");
    let mut connect = pin!(Client::connect(node.clock(), node.net(), &mut rng, &url));
    let status = poll_fn(|cx| {
        while let Poll::Ready(stream) = listener.poll_accept(cx) {
            accepted(stream.expect("the accept works"));
        }
        connect.as_mut().poll(cx).map(Result::err)
    })
    .await;
    (status.expect("the connect fails"), clock.now() - start)
}

#[test]
fn a_connect_to_a_listener_that_never_answers_times_out() {
    let mut streams = Vec::new();
    let (out, _) = run(0, async move |node| {
        connect_to_listener(&node, |stream| streams.push(stream)).await
    });
    assert_eq!(out, (Status::BAD_TIMEOUT, Span::from_nanos(5_000_000_000)));
}

#[test]
fn a_connect_to_a_listener_that_closes_each_stream_times_out() {
    let (out, _) = run(0, async |node| connect_to_listener(&node, drop).await);
    assert_eq!(out, (Status::BAD_TIMEOUT, Span::from_nanos(5_000_000_000)));
}

/// A close waits for the answer of the server, which a server that stops never gives,
/// until the request times out after 5 s. open62541 checks the timeouts of requests
/// once a second.
#[test]
fn a_close_of_a_client_whose_server_stops_ends_at_the_request_timeout() {
    let (took, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let client = peer.connected().await;
            peer.paused.set(true);
            let start = peer.clock.now();
            client.close().await;
            peer.paused.set(false);
            peer.clock.now() - start
        })
        .await
    });
    let window = Span::from_nanos(5_000_000_000)..=Span::from_nanos(6_000_000_000);
    assert!(window.contains(&took), "{took:?}");
}

/// A read restores the request timeout of the client after it sends, so a close after a
/// read on a server that stops still ends 5 to 6 s after it starts.
#[test]
fn a_close_after_a_read_of_a_client_whose_server_stops_ends_at_the_request_timeout() {
    let (took, _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            assert_eq!(client.read(VALUE).await, Ok(peer.value));
            peer.paused.set(true);
            let start = peer.clock.now();
            client.close().await;
            peer.paused.set(false);
            peer.clock.now() - start
        })
        .await
    });
    let window = Span::from_nanos(5_000_000_000)..=Span::from_nanos(6_000_000_000);
    assert!(window.contains(&took), "{took:?}");
}

/// A read that a server that stops never answers ends with `BadTimeout` at the request
/// timeout, as a close does.
#[test]
fn a_read_of_a_client_whose_server_stops_times_out() {
    let ((value, took), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            peer.paused.set(true);
            let start = peer.clock.now();
            let value = client.read(VALUE).await;
            let took = peer.clock.now() - start;
            peer.paused.set(false);
            client.close().await;
            (value, took)
        })
        .await
    });
    assert_eq!(value, Err(Status::BAD_TIMEOUT));
    let window = Span::from_nanos(5_000_000_000)..=Span::from_nanos(6_000_000_000);
    assert!(window.contains(&took), "{took:?}");
}

/// A connect that times out after the server makes its session closes the session, so
/// a server with room for one session takes the next client. In the run of seed 0, the
/// server makes the session 3 ms after the connect starts and activates it later.
#[test]
fn a_connect_that_times_out_closes_its_session() {
    times_out_and_closes(&[Span::from_nanos(3_000_000)]);
}

/// A connect that times out before the server answers its `CreateSession` closes the
/// session that the server makes after the deadline. In the run of seed 0, the server
/// gets the `CreateSession` before 2.5 ms and answers it 100 us after the deadline.
#[test]
fn a_connect_that_times_out_before_the_server_makes_its_session_closes_it() {
    times_out_and_closes(&[
        Span::from_nanos(2_500_000),
        Span::from_nanos(5_000_100_000),
    ]);
}

/// A connect that times out before a server that stops answers its `CreateSession`
/// ends once that request times out, and leaves no session.
#[test]
fn a_connect_whose_create_session_gets_no_answer_ends() {
    times_out_and_closes(&[Span::from_nanos(2_500_000)]);
}

/// The close after a connect that times out can take two more request timeouts: one for
/// the answer to the `CreateSession`, then one for the `CloseSession`. The server gets
/// the `CreateSession` just before the deadline and answers it at 9 s, or gets it just
/// after the check of timeouts at 4 s and answers it just before its timeout, and then
/// stops. open62541 checks the timeouts of requests once a second from the start of the
/// connect, so the close ends at most 10 s after the deadline.
#[test]
fn a_connect_whose_create_session_answers_late_ends_two_request_timeouts_later() {
    let before_the_deadline = times_out_and_closes(&[
        Span::from_nanos(0),
        Span::from_nanos(4_998_000_000),
        Span::from_nanos(4_999_500_000),
        Span::from_nanos(9_000_000_000),
        Span::from_nanos(9_000_100_000),
    ]);
    let after_a_check = times_out_and_closes(&[
        Span::from_nanos(0),
        Span::from_nanos(4_001_000_000),
        Span::from_nanos(4_002_500_000),
        Span::from_nanos(9_999_000_000),
        Span::from_nanos(9_999_100_000),
    ]);
    let bound = Span::from_nanos(15_000_000_000);
    assert_eq!((before_the_deadline, after_a_check), (bound, bound));
}

/// Connects to a server with room for one session that stops at the first time of
/// `flips` after the connect starts, runs again at the next, and so on, and runs from
/// the end of the connect. Asserts that the connect fails with `BadTimeout` and that
/// a second client then connects, so the first left no session. Gives the time that
/// the first connect took.
fn times_out_and_closes(flips: &[Span]) -> Span {
    let flips = flips.to_vec();
    let ((first, took, second), _) = run(0, async move |node| {
        serve(&node, 1, async |mut peer| {
            let (url, paused) = (peer.url.clone(), Rc::clone(&peer.paused));
            let (clock, net) = (Clock::clone(&peer.clock), Net::clone(&peer.net));
            let start = peer.clock.now();
            let first = {
                let mut connect =
                    pin!(Client::connect(clock, net, &mut peer.rng, &url));
                let mut flips: Vec<_> = flips
                    .iter()
                    .map(|at| Box::pin(peer.clock.sleep(*at)))
                    .collect();
                let mut flipped = 0;
                poll_fn(|cx| {
                    while flipped < flips.len()
                        && flips[flipped].as_mut().poll(cx).is_ready()
                    {
                        flipped += 1;
                        paused.set(!paused.get());
                        // `serve` runs the server before this body.
                        cx.waker().wake_by_ref();
                    }
                    connect.as_mut().poll(cx)
                })
                .await
                .err()
            };
            let took = peer.clock.now() - start;
            paused.set(false);
            let second = match peer.connect(&url).await {
                Ok(client) => {
                    client.close().await;
                    None
                }
                Err(status) => Some(status),
            };
            (first, took, second)
        })
        .await
    });
    assert_eq!((first, second), (Some(Status::BAD_TIMEOUT), None));
    took
}

/// A server adds a namespace after a client connects, and a Double in it. The client
/// reads it, then a new client reads it.
#[test]
fn a_read_of_a_namespace_that_the_server_adds_after_the_connect_gets_its_value() {
    let ((value, reads), _) = run(0, async |node| {
        serve(&node, 4, async |mut peer| {
            let mut client = peer.connected().await;
            let first = client.read(VALUE).await;
            // SAFETY: the server lives, and copies the name.
            let namespace = unsafe {
                ffi::test::UA_Server_addNamespace(
                    peer.server.raw(),
                    c"urn:added".as_ptr(),
                )
            };
            let added = Node {
                namespace,
                key: 1000,
            };
            peer.server.add_double(added, 2.5);
            let second = client.read(added).await;
            client.close().await;
            let mut fresh = peer.connected().await;
            let third = fresh.read(added).await;
            fresh.close().await;
            (peer.value, (first, namespace, second, third))
        })
        .await
    });
    assert_eq!(reads, (Ok(value), 2, Ok(2.5), Ok(2.5)));
}

/// The body of an abort chunk with the Error `BadResponseTooLarge` and a null Reason.
const TOO_LARGE: [u8; 8] = [0x00, 0x00, 0xB9, 0x80, 0xFF, 0xFF, 0xFF, 0xFF];

/// The abort chunk of the final MSG chunk `chunk`: its message, security, and sequence
/// headers, then `body` (Part 6, 6.7.3).
fn abort(chunk: &[u8], body: &[u8]) -> Vec<u8> {
    // The policy None adds nothing past the 24 bytes of headers.
    let mut abort = chunk[..24].to_vec();
    abort[3] = b'A';
    abort.extend_from_slice(body);
    let size = u32::try_from(abort.len()).expect("fits");
    abort[4..8].copy_from_slice(&size.to_le_bytes());
    abort
}

fn too_large(chunk: &[u8]) -> Vec<u8> {
    abort(chunk, &TOO_LARGE)
}

fn empty(chunk: &[u8]) -> Vec<u8> {
    abort(chunk, &[])
}

/// An abort of `chunk` with the request id `u32::MAX`, which no request has.
fn unknown(chunk: &[u8]) -> Vec<u8> {
    let mut abort = too_large(chunk);
    abort[20..24].copy_from_slice(&u32::MAX.to_le_bytes());
    abort
}

/// Forwards the bytes of one stream from `listener` to `server`. While `aborts` is
/// above 0, it sends `abort` of the next final MSG chunk from the server in its place.
async fn abort_proxy(
    net: Net,
    listener: &mut env::net::Listener,
    server: SocketAddr,
    aborts: &Cell<u16>,
    abort: fn(&[u8]) -> Vec<u8>,
) {
    let mut down = poll_fn(|cx| listener.poll_accept(cx))
        .await
        .expect("the accept works");
    let options = tcp::Options {
        send_buffer_bytes: 1 << 16,
        recv_buffer_bytes: 1 << 16,
        unsent_bytes_max: NonZeroUsize::new(1 << 14).expect("not 0"),
        delayed: false,
    };
    let mut up = net
        .connect(&tcp::Config {
            remote: server,
            options,
        })
        .await
        .expect("the server accepts");
    let (mut to_server, mut from_server, mut to_client) =
        (Vec::new(), Vec::new(), Vec::new());
    let mut buffer = vec![0u8; 1 << 16];
    poll_fn(|cx| {
        loop {
            let mut moved = false;
            match down.poll_read(cx, &mut buffer) {
                Poll::Ready(Ok(n)) if n > 0 => {
                    to_server.extend_from_slice(&buffer[..n]);
                    moved = true;
                }
                Poll::Ready(_) => return Poll::Ready(()),
                Poll::Pending => {}
            }
            match up.poll_read(cx, &mut buffer) {
                Poll::Ready(Ok(n)) if n > 0 => {
                    from_server.extend_from_slice(&buffer[..n]);
                    moved = true;
                }
                Poll::Ready(_) => return Poll::Ready(()),
                Poll::Pending => {}
            }
            while from_server.len() >= 8 {
                let size = u32::from_le_bytes(from_server[4..8].try_into().expect("4"));
                let size = usize::try_from(size).expect("fits");
                if from_server.len() < size {
                    break;
                }
                let chunk: Vec<u8> = from_server.drain(..size).collect();
                if &chunk[..4] == b"MSGF" && aborts.get() > 0 {
                    to_client.extend_from_slice(&abort(&chunk));
                    aborts.set(aborts.get() - 1);
                } else {
                    to_client.extend_from_slice(&chunk);
                }
            }
            if !to_server.is_empty()
                && let Poll::Ready(Ok(n)) =
                    up.poll_write(cx, &[std::io::IoSlice::new(&to_server)])
            {
                to_server.drain(..n);
                moved = true;
            }
            if !to_client.is_empty()
                && let Poll::Ready(Ok(n)) =
                    down.poll_write(cx, &[std::io::IoSlice::new(&to_client)])
            {
                to_client.drain(..n);
                moved = true;
            }
            if !moved {
                return Poll::Pending;
            }
        }
    })
    .await;
}

/// After `dropped` dropped reads, the answers of `aborted` reads, then of two more
/// reads, and the value of the server. The proxy sends `abort` of the answers of the
/// dropped reads and of the `aborted` reads in their place.
fn read_aborted(
    dropped: u16,
    aborted: u16,
    abort: fn(&[u8]) -> Vec<u8>,
) -> (
    Vec<Result<f64, Status>>,
    Result<f64, Status>,
    Result<f64, Status>,
    f64,
) {
    run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            let local = SocketAddr::new(node.addresses()[0], PORT + 1);
            let listen = tcp::Listen {
                local,
                backlog: 4,
                options: tcp::Options {
                    send_buffer_bytes: 1 << 16,
                    recv_buffer_bytes: 1 << 16,
                    unsent_bytes_max: NonZeroUsize::new(1 << 14).expect("not 0"),
                    delayed: false,
                },
            };
            let mut listener = node.net().listen(&listen).expect("the port is free");
            let server = SocketAddr::new(node.addresses()[0], PORT);
            let left = Cell::new(0u16);
            let mut proxy =
                pin!(abort_proxy(node.net(), &mut listener, server, &left, abort));
            let url = format!("opc.tcp://{local}");
            let value = peer.value;
            let mut reads = pin!(async {
                let mut client = peer.connect(&url).await.expect("the client connects");
                // open62541 reads the namespace array after the connect.
                assert_eq!(client.read(VALUE).await, Ok(peer.value));
                left.set(dropped + aborted);
                drop_reads(&mut client, dropped).await;
                let mut answers = Vec::new();
                for _ in 0..aborted {
                    answers.push(client.read(VALUE).await);
                }
                let after = client.read(VALUE).await;
                assert_eq!(left.get(), 0, "the server answered each read");
                let again = client.read(VALUE).await;
                client.close().await;
                (answers, after, again)
            });
            let mut ended = false;
            let (answers, after, again) = poll_fn(|cx| {
                if !ended {
                    ended = proxy.as_mut().poll(cx).is_ready();
                }
                reads.as_mut().poll(cx)
            })
            .await;
            (answers, after, again, value)
        })
        .await
    })
    .0
}

/// An abort chunk answers its read with its Error, so after 16 of them the client
/// holds no read and a later read that the server answers in full gives the value.
#[test]
fn a_read_after_16_aborted_answers_gives_the_value() {
    let (aborted, after, again, value) = read_aborted(0, 16, too_large);
    assert_eq!(aborted, vec![Err(Status::BAD_RESPONSE_TOO_LARGE); 16]);
    assert_eq!((after, again), (Ok(value), Ok(value)));
}

/// An abort chunk with no Error ends its read with `BadDecodingError`.
#[test]
fn an_abort_with_no_error_ends_its_read_with_bad_decoding_error() {
    let (aborted, after, again, value) = read_aborted(0, 1, empty);
    assert_eq!(aborted, vec![Err(Status::BAD_DECODING_ERROR)]);
    assert_eq!((after, again), (Ok(value), Ok(value)));
}

/// An abort ends the request of its id, not the newest request, so the abort of the
/// answer of a dropped read leaves the read after it in flight.
#[test]
fn the_abort_of_a_dropped_read_leaves_the_next_read_its_value() {
    let (aborted, after, again, value) = read_aborted(1, 0, too_large);
    assert_eq!(aborted, Vec::new());
    assert_eq!((after, again), (Ok(value), Ok(value)));
}

/// An abort with an id that no request has ends no read, so its read times out, and
/// the client reads on.
#[test]
fn an_abort_with_an_unknown_id_ends_no_read() {
    let (aborted, after, again, value) = read_aborted(0, 1, unknown);
    assert_eq!(aborted, vec![Err(Status::BAD_TIMEOUT)]);
    assert_eq!((after, again), (Ok(value), Ok(value)));
}
