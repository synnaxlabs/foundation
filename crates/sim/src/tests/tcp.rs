//! Tests of simulated TCP through `env::net`.

use std::future::{pending, poll_fn};
use std::io::IoSlice;
use std::net::SocketAddr;
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use env::net::{Error as Net, Listener, Tcp, tcp};
use types::time::{Monotonic, Span};

use super::{after, at, delay, millis, pair, panicked, shard};
use crate::{Crash, Error, link, node};

/// What a shard gave, once it ended.
type Slot<T> = Arc<Mutex<Option<T>>>;

/// Starts `body` on a shard of `node` named `name`, and gives the slot of its value.
fn start<T, F>(
    node: &node::Node,
    name: &str,
    body: impl FnOnce(node::Node) -> F + Send + 'static,
) -> Slot<T>
where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
{
    let slot = Slot::default();
    let (out, own) = (Arc::clone(&slot), node.clone());
    let handle = node.shards().start(shard(name), move |_| async move {
        let value = body(own).await;
        *out.lock().unwrap() = Some(value);
    });
    drop(handle.unwrap());
    slot
}

/// The value that the shard of `slot` gave.
fn take<T>(slot: &Slot<T>) -> T {
    slot.lock().unwrap().take().expect("the shard gave a value")
}

/// A node's clock after `legs` one-way delays of the default link.
fn legs(legs: i64) -> Monotonic {
    after(Span::from_nanos(legs * delay().nanos()))
}

/// Options with buffers of 1 MiB, of which 16 KiB may wait unsent.
fn options() -> tcp::Options {
    tcp::Options {
        send_buffer_bytes: 1 << 20,
        recv_buffer_bytes: 1 << 20,
        unsent_bytes_max: 1 << 14,
        delayed: false,
    }
}

/// A listener on `local` with a backlog of 4.
fn listen_on(node: &node::Node, local: SocketAddr, options: tcp::Options) -> Listener {
    let listen = tcp::Listen {
        local,
        backlog: 4,
        options,
    };
    node.net().listen(&listen).unwrap()
}

/// A listener on `port` of the IPv4 address of `node`.
fn listen(node: &node::Node, port: u16) -> Listener {
    listen_on(node, at(node, port), options())
}

/// A stream from `node` to `remote`.
async fn connect(
    node: &node::Node,
    remote: SocketAddr,
    options: tcp::Options,
) -> Result<Tcp, Net> {
    node.net().connect(&tcp::Config { remote, options }).await
}

async fn accept(listener: &mut Listener) -> Tcp {
    poll_fn(|cx| listener.poll_accept(cx)).await.unwrap()
}

/// Polls `future` once, and gives its output if it was ready.
async fn poll_once<F: Future>(future: F) -> Option<F::Output> {
    let mut future = pin!(future);
    poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Ready(output) => Poll::Ready(Some(output)),
        Poll::Pending => Poll::Ready(None),
    })
    .await
}

/// Writes every byte of `bytes`, and adds the count of each write to `written`.
async fn write_counted(
    tcp: &mut Tcp,
    bytes: &[u8],
    written: &AtomicUsize,
) -> Result<(), Net> {
    let mut rest = bytes;
    while !rest.is_empty() {
        let parts = [IoSlice::new(rest)];
        let n = poll_fn(|cx| tcp.poll_write(cx, &parts)).await?;
        written.fetch_add(n, Ordering::Relaxed);
        rest = &rest[n..];
    }
    Ok(())
}

async fn write_all(tcp: &mut Tcp, bytes: &[u8]) -> Result<(), Net> {
    write_counted(tcp, bytes, &AtomicUsize::new(0)).await
}

/// One read into a buffer of `size` bytes.
async fn read(tcp: &mut Tcp, size: usize) -> Result<Vec<u8>, Net> {
    let mut buffer = vec![0; size];
    let n = poll_fn(|cx| tcp.poll_read(cx, &mut buffer)).await?;
    buffer.truncate(n);
    Ok(buffer)
}

/// Reads until a count of 0 or an error, and gives the bytes and how reads ended.
async fn read_all(tcp: &mut Tcp) -> (Vec<u8>, Result<(), Net>) {
    let mut bytes = Vec::new();
    loop {
        match read(tcp, 4_096).await {
            Ok(read) if read.is_empty() => return (bytes, Ok(())),
            Ok(read) => bytes.extend(read),
            Err(e) => return (bytes, Err(e)),
        }
    }
}

async fn close(tcp: &mut Tcp) -> Result<(), Net> {
    poll_fn(|cx| tcp.poll_close(cx)).await
}

/// `len` bytes, each unlike its neighbors.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect()
}

#[test]
fn a_connect_is_ready_after_one_round_trip_and_its_accept_after_one_and_a_half() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let accepted = start(&b, "server", move |node| async move {
        let tcp = accept(&mut listener).await;
        (node.clock().now(), tcp.local(), tcp.peer())
    });
    let remote = at(&b, 4433);
    let connected = start(&a, "client", move |node| async move {
        let tcp = connect(&node, remote, options()).await.unwrap();
        (node.clock().now(), tcp.local(), tcp.peer())
    });
    sim.run().unwrap();
    let client = at(&a, 49_152);
    assert_eq!(take(&connected), (legs(2), client, remote));
    assert_eq!(take(&accepted), (legs(3), remote, client));
}

/// When a connect from `a` to port 4433 of `b` ends, and what it gives. `before` runs
/// on `b` first, and its listener lives through the run.
fn refused(
    before: fn(&node::Node) -> Option<Listener>,
) -> ((Monotonic, Option<Net>), SocketAddr) {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let _listener = before(&b);
    let remote = at(&b, 4433);
    let end = start(&a, "client", move |node| async move {
        let error = connect(&node, remote, options()).await.err();
        (node.clock().now(), error)
    });
    sim.run().unwrap();
    (take(&end), remote)
}

#[test]
fn a_connect_to_a_port_with_no_listener_is_refused_after_one_round_trip() {
    let befores: [fn(&node::Node) -> Option<Listener>; 3] = [
        |_| None,
        |b| {
            drop(listen(b, 4433));
            None
        },
        |b| Some(listen(b, 4434)),
    ];
    for (i, before) in befores.into_iter().enumerate() {
        let (end, remote) = refused(before);
        assert_eq!(end, (legs(2), Some(Net::Refused { remote })), "{i}");
    }
}

/// Sends `len` bytes from `a` to `b` on `link`, then closes, and gives the run's
/// digest and what `b` read.
fn send(
    seed: u64,
    link: link::Config,
    len: usize,
) -> (u64, (Vec<u8>, Result<(), Net>)) {
    let (mut sim, a, b) = pair(seed, link);
    let mut listener = listen(&b, 4433);
    let received = start(&b, "server", move |_| async move {
        let mut tcp = accept(&mut listener).await;
        read_all(&mut tcp).await
    });
    let remote = at(&b, 4433);
    start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        write_all(&mut tcp, &pattern(len)).await.unwrap();
        close(&mut tcp).await.unwrap();
    });
    sim.run().unwrap();
    (sim.digest(), take(&received))
}

#[test]
fn a_mebibyte_arrives_in_order_over_a_link_whose_jitter_is_its_delay() {
    let link = link::Config {
        jitter: delay(),
        ..link::Config::default()
    };
    for seed in 0..4 {
        let (_, received) = send(seed, link, 1 << 20);
        assert_eq!(received, (pattern(1 << 20), Ok(())), "seed {seed}");
    }
}

#[test]
fn one_seed_gives_one_digest() {
    let link = link::Config {
        jitter: delay(),
        ..link::Config::default()
    };
    let digest = send(3, link, 1 << 20).0;
    assert_eq!(send(3, link, 1 << 20).0, digest);
    assert_ne!(send(4, link, 1 << 20).0, digest);
}

#[test]
fn one_more_byte_gives_another_digest() {
    let link = link::Config::default();
    assert_ne!(send(0, link, 2).0, send(0, link, 1).0);
}

#[test]
fn bytes_written_over_time_arrive_in_order_over_a_link_whose_jitter_is_40_delays() {
    let link = link::Config {
        jitter: millis(10),
        ..link::Config::default()
    };
    for seed in 0..4 {
        let (mut sim, a, b) = pair(seed, link);
        let mut listener = listen(&b, 4433);
        let received = start(&b, "server", move |_| async move {
            let mut tcp = accept(&mut listener).await;
            read_all(&mut tcp).await
        });
        let remote = at(&b, 4433);
        start(&a, "client", move |node| async move {
            let mut tcp = connect(&node, remote, options()).await.unwrap();
            for chunk in pattern(100_000).chunks(1_000) {
                write_all(&mut tcp, chunk).await.unwrap();
                node.clock().sleep(Span::from_nanos(100_000)).await;
            }
            close(&mut tcp).await.unwrap();
        });
        sim.run().unwrap();
        assert_eq!(take(&received), (pattern(100_000), Ok(())), "seed {seed}");
    }
}

/// Writes 256 KiB from `a` with `options` to a reader on `b` that reads nothing for
/// 100 ms, and gives the bytes written by then and the bytes the reader got.
fn stalled(options: tcp::Options) -> (usize, usize) {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let written = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&written);
    let mut listener = listen_on(
        &b,
        at(&b, 4433),
        tcp::Options {
            recv_buffer_bytes: 1 << 16,
            ..self::options()
        },
    );
    let server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        node.clock().sleep(millis(100)).await;
        let stalled = count.load(Ordering::Relaxed);
        let (bytes, end) = read_all(&mut tcp).await;
        end.unwrap();
        (stalled, bytes.len())
    });
    let remote = at(&b, 4433);
    start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options).await.unwrap();
        for _ in 0..64 {
            write_counted(&mut tcp, &[7; 4_096], &written)
                .await
                .unwrap();
        }
        close(&mut tcp).await.unwrap();
    });
    sim.run().unwrap();
    take(&server)
}

#[test]
fn a_peer_that_reads_nothing_stops_the_writer_when_both_buffers_are_full() {
    let options = tcp::Options {
        send_buffer_bytes: 1 << 16,
        unsent_bytes_max: 1 << 16,
        ..options()
    };
    assert_eq!(stalled(options), (2 << 16, 1 << 18));
}

#[test]
fn a_write_is_pending_while_the_unsent_bytes_reach_their_most() {
    assert_eq!(stalled(options()), ((1 << 16) + (1 << 14), 1 << 18));
}

#[test]
fn a_write_is_pending_while_the_send_buffer_is_full() {
    let options = tcp::Options {
        send_buffer_bytes: 1 << 14,
        ..options()
    };
    let unsent = tcp::Options {
        unsent_bytes_max: 1 << 20,
        ..options
    };
    assert_eq!(stalled(unsent), ((1 << 16) + (1 << 14), 1 << 18));
}

#[test]
fn a_write_takes_no_more_bytes_than_the_send_buffer_has_room_for() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let _listener = listen(&b, 4433);
    let remote = at(&b, 4433);
    let small = tcp::Options {
        send_buffer_bytes: 1 << 14,
        ..options()
    };
    let wrote = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, small).await.unwrap();
        let (one, two) = (vec![1; 12_288], vec![2; 12_288]);
        let parts = [IoSlice::new(&one), IoSlice::new(&two)];
        poll_fn(|cx| tcp.poll_write(cx, &parts)).await
    });
    sim.run().unwrap();
    assert_eq!(take(&wrote), Ok(1 << 14));
}

/// Fills the receive buffer of `recv` bytes of a reader on `b`, which then reads up
/// to `size` bytes. Gives the bytes of that read, and the bytes that the writer on
/// `a` could write next.
fn opened(recv: usize, size: usize) -> (usize, usize) {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let written = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&written);
    let reader = tcp::Options {
        recv_buffer_bytes: recv,
        ..options()
    };
    let mut listener = listen_on(&b, at(&b, 4433), reader);
    let server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        node.clock().sleep(millis(10)).await;
        let before = count.load(Ordering::Relaxed);
        let got = read(&mut tcp, size).await.unwrap().len();
        node.clock().sleep(millis(10)).await;
        let more = count.load(Ordering::Relaxed) - before;
        read_all(&mut tcp).await.1.unwrap();
        (got, more)
    });
    let remote = at(&b, 4433);
    let writer = tcp::Options {
        send_buffer_bytes: 1 << 14,
        ..options()
    };
    start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, writer).await.unwrap();
        write_counted(&mut tcp, &pattern(1 << 18), &written)
            .await
            .unwrap();
        close(&mut tcp).await.unwrap();
    });
    sim.run().unwrap();
    take(&server)
}

#[test]
fn a_read_opens_the_window_once_it_frees_a_segment_or_half_the_buffer() {
    let cases = [
        ((1 << 16, 1), (1, 0)),
        ((1 << 16, 1_459), (1_459, 0)),
        ((1 << 16, 1_460), (1_460, 1_460)),
        ((1_000, 499), (499, 0)),
        ((1_000, 500), (500, 500)),
        ((2_000, 4_096), (2_000, 2_000)),
    ];
    for ((recv, size), opens) in cases {
        assert_eq!(opened(recv, size), opens, "{recv} {size}");
    }
}

#[test]
fn a_close_sends_its_end_after_the_bytes_and_leaves_reads_open() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let server = start(&b, "server", move |_| async move {
        let mut tcp = accept(&mut listener).await;
        let got = read_all(&mut tcp).await;
        let again = read(&mut tcp, 1).await;
        write_all(&mut tcp, b"bye").await.unwrap();
        close(&mut tcp).await.unwrap();
        (got, again)
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        write_all(&mut tcp, &pattern(100_000)).await.unwrap();
        close(&mut tcp).await.unwrap();
        let late = write_all(&mut tcp, b"late").await;
        (late, read_all(&mut tcp).await)
    });
    sim.run().unwrap();
    assert_eq!(take(&server), ((pattern(100_000), Ok(())), Ok(Vec::new())));
    let late = Err(Net::Io { code: 32 });
    assert_eq!(take(&client), (late, (b"bye".to_vec(), Ok(()))));
}

#[test]
fn a_stream_whose_acceptor_closed_first_ends_with_no_reset() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        close(&mut tcp).await.unwrap();
        node.clock().sleep(millis(10)).await;
        read_all(&mut tcp).await
    });
    let remote = at(&b, 4433);
    start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        read_all(&mut tcp).await.1.unwrap();
        write_all(&mut tcp, b"late").await.unwrap();
        close(&mut tcp).await.unwrap();
    });
    sim.run().unwrap();
    assert_eq!(take(&server), (b"late".to_vec(), Ok(())));
}

#[test]
fn a_drop_before_close_resets_the_peer_after_the_bytes_that_arrived() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        node.clock().sleep(millis(10)).await;
        (tcp.peer(), read_all(&mut tcp).await)
    });
    let remote = at(&b, 4433);
    start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        write_all(&mut tcp, &pattern(10_000)).await.unwrap();
    });
    sim.run().unwrap();
    let (peer, read) = take(&server);
    assert_eq!(read, (pattern(10_000), Err(Net::Reset { remote: peer })));
}

#[test]
fn a_drop_after_close_with_bytes_unread_resets_the_peer() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        write_all(&mut tcp, b"unread").await.unwrap();
        node.clock().sleep(millis(10)).await;
        (tcp.peer(), write_all(&mut tcp, b"more").await)
    });
    let remote = at(&b, 4433);
    start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(millis(5)).await;
        close(&mut tcp).await.unwrap();
    });
    sim.run().unwrap();
    let (peer, write) = take(&server);
    assert_eq!(write, Err(Net::Reset { remote: peer }));
}

#[test]
fn a_drop_after_close_still_sends_every_byte_and_the_end() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen_on(
        &b,
        at(&b, 4433),
        tcp::Options {
            recv_buffer_bytes: 1 << 14,
            ..options()
        },
    );
    let server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        node.clock().sleep(millis(10)).await;
        read_all(&mut tcp).await
    });
    let remote = at(&b, 4433);
    let unsent = tcp::Options {
        unsent_bytes_max: 1 << 20,
        ..options()
    };
    start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, unsent).await.unwrap();
        write_all(&mut tcp, &pattern(100_000)).await.unwrap();
        close(&mut tcp).await.unwrap();
    });
    sim.run().unwrap();
    assert_eq!(take(&server), (pattern(100_000), Ok(())));
}

#[test]
fn a_dropped_connect_leaves_nothing_to_accept() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let server = start(&b, "server", move |node| async move {
        let peer = accept(&mut listener).await.peer();
        node.clock().sleep(millis(10)).await;
        let next = poll_once(poll_fn(|cx| listener.poll_accept(cx))).await;
        (peer, next.is_none())
    });
    let remote = at(&b, 4433);
    start(&a, "client", move |node| async move {
        assert!(poll_once(connect(&node, remote, options())).await.is_none());
        let _tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(millis(20)).await;
    });
    sim.run().unwrap();
    assert_eq!(take(&server), (at(&a, 49_153), true));
}

#[test]
fn a_dropped_listener_resets_the_streams_it_did_not_accept() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let listener = listen(&b, 4433);
    start(&b, "server", move |node| async move {
        node.clock().sleep(millis(10)).await;
        drop(listener);
    });
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        read(&mut tcp, 1).await
    });
    sim.run().unwrap();
    assert_eq!(take(&client), Err(Net::Reset { remote }));
}

#[test]
fn a_listener_on_the_unspecified_address_accepts_on_the_nodes_address() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let unspecified = SocketAddr::from(([0, 0, 0, 0], 4433));
    let mut listener = listen_on(&b, unspecified, options());
    let local = start(&b, "server", move |_| async move {
        (listener.local(), accept(&mut listener).await.local())
    });
    let remote = at(&b, 4433);
    start(&a, "client", move |node| async move {
        let _tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(millis(1)).await;
    });
    sim.run().unwrap();
    assert_eq!(take(&local), (unspecified, remote));
}

#[test]
fn a_second_listener_on_one_address_finds_it_in_use() {
    let (_sim, a, _b) = pair(0, link::Config::default());
    let _first = listen(&a, 4433);
    let listen = tcp::Listen {
        local: at(&a, 4433),
        backlog: 4,
        options: options(),
    };
    let local = at(&a, 4433);
    assert_eq!(
        a.net().listen(&listen).err(),
        Some(Net::AddressInUse { local })
    );
}

#[test]
fn a_listener_on_an_address_of_another_node_is_not_available() {
    let (_sim, a, b) = pair(0, link::Config::default());
    let listen = tcp::Listen {
        local: at(&b, 4433),
        backlog: 4,
        options: options(),
    };
    assert_eq!(a.net().listen(&listen).err(), Some(Net::Io { code: 99 }));
}

#[test]
fn port_0_listens_on_the_lowest_free_port_from_49152() {
    let (_sim, a, _b) = pair(0, link::Config::default());
    let first = listen(&a, 0);
    let second = listen(&a, 0);
    drop(first);
    let third = listen(&a, 0);
    let ports = [&second, &third].map(|listener| listener.local().port());
    assert_eq!(ports, [49_153, 49_152]);
}

#[test]
fn a_listener_on_the_unspecified_address_holds_its_port_on_each_address() {
    let (_sim, a, _b) = pair(0, link::Config::default());
    let (unspecified, specific) =
        (SocketAddr::from(([0, 0, 0, 0], 4433)), at(&a, 4433));
    for (first, second) in [(unspecified, specific), (specific, unspecified)] {
        let _first = listen_on(&a, first, options());
        let listen = tcp::Listen {
            local: second,
            backlog: 4,
            options: options(),
        };
        let in_use = Net::AddressInUse { local: second };
        assert_eq!(a.net().listen(&listen).err(), Some(in_use), "{first}");
    }
}

#[test]
fn two_listeners_of_one_node_each_accept_their_own_streams() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listeners = [listen(&b, 4433), listen(&b, 4434)];
    let server = start(&b, "server", move |_| async move {
        let mut locals = Vec::new();
        for listener in &mut listeners {
            locals.push(accept(listener).await.local());
        }
        locals
    });
    let remotes = [at(&b, 4433), at(&b, 4434)];
    start(&a, "client", move |node| async move {
        let mut streams = Vec::new();
        for remote in remotes {
            streams.push(connect(&node, remote, options()).await.unwrap());
        }
        node.clock().sleep(millis(1)).await;
    });
    sim.run().unwrap();
    assert_eq!(take(&server), remotes);
}

#[test]
fn a_connect_skips_the_ports_of_the_streams_and_listeners_of_its_node() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let _listeners = [listen(&b, 49_152), listen(&a, 4433), listen(&a, 49_153)];
    let remote = at(&b, 49_152);
    let ports = start(&a, "client", move |node| async move {
        let first = connect(&node, remote, options()).await.unwrap();
        let second = connect(&node, remote, options()).await.unwrap();
        [first, second].map(|tcp| tcp.local().port())
    });
    sim.run().unwrap();
    assert_eq!(take(&ports), [49_152, 49_154]);
}

/// Starts and drops a connect from `node` to `closed` once per port past 49152, so
/// that a node whose first connect took 49152 takes it again next.
async fn wrap(node: &node::Node, closed: SocketAddr) {
    for _ in 49_153..=u16::MAX {
        assert!(poll_once(connect(node, closed, options())).await.is_none());
    }
}

#[test]
fn a_finished_stream_frees_its_port_for_a_connect_after_the_ports_wrap() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let server = start(&b, "server", move |_| async move {
        let mut tcp = accept(&mut listener).await;
        read_all(&mut tcp).await.1.unwrap();
        close(&mut tcp).await.unwrap();
        drop(tcp);
        accept(&mut listener).await.peer()
    });
    let (remote, closed) = (at(&b, 4433), at(&b, 9));
    start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        close(&mut tcp).await.unwrap();
        drop(tcp);
        node.clock().sleep(millis(10)).await;
        wrap(&node, closed).await;
        let _tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(millis(1)).await;
    });
    sim.run().unwrap();
    assert_eq!(take(&server), at(&a, 49_152));
}

/// Crashes `a` by `crash` 10 ms into a stream to `b`. Gives what `b` then got: a
/// read polled once at 20 ms, a write of one byte, and a read 10 ms later. Also
/// gives the address of `a`'s end.
#[expect(clippy::type_complexity, reason = "the three results of the peer")]
fn crashed(
    crash: Crash,
) -> (
    (
        Option<Result<Vec<u8>, Net>>,
        Result<usize, Net>,
        Result<Vec<u8>, Net>,
    ),
    SocketAddr,
) {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        node.clock().sleep(millis(20)).await;
        let first = poll_once(read(&mut tcp, 1)).await;
        let parts = [IoSlice::new(&[1])];
        let wrote = poll_fn(|cx| tcp.poll_write(cx, &parts)).await;
        node.clock().sleep(millis(10)).await;
        (first, wrote, read(&mut tcp, 1).await)
    });
    let remote = at(&b, 4433);
    start(&a, "client", move |node| async move {
        let _tcp = connect(&node, remote, options()).await.unwrap();
        pending::<()>().await;
    });
    sim.run_for(millis(10)).unwrap();
    sim.crash(&a, crash);
    sim.run().unwrap();
    (take(&server), at(&a, 49_152))
}

#[test]
fn a_power_cut_resets_the_peer_only_once_the_peer_sends() {
    let (got, peer) = crashed(Crash::Power);
    let reset = Net::Reset { remote: peer };
    assert_eq!(got, (None, Ok(1), Err(reset)));
}

#[test]
fn a_process_crash_resets_the_peer() {
    let (got, peer) = crashed(Crash::Process);
    let reset = Net::Reset { remote: peer };
    assert_eq!(
        got,
        (Some(Err(reset.clone())), Err(reset.clone()), Err(reset))
    );
}

#[test]
fn a_power_cut_leaves_the_listeners_of_other_nodes() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let c = sim.node(node::Config::default());
    let mut listener = listen(&b, 4433);
    let server = start(&b, "server", move |_| async move {
        accept(&mut listener).await.peer()
    });
    sim.run_for(millis(1)).unwrap();
    sim.crash(&a, Crash::Power);
    let remote = at(&b, 4433);
    start(&c, "client", move |node| async move {
        let _tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(millis(1)).await;
    });
    sim.run().unwrap();
    assert_eq!(take(&server), at(&c, 49_152));
}

/// The error of a run in which a shard of `a` calls `call` on a link of `link`.
fn yet<F: Future + 'static>(
    link: link::Config,
    call: impl FnOnce(node::Node, SocketAddr) -> F + Send + 'static,
) -> Error {
    let (mut sim, a, b) = pair(0, link);
    let _listener = listen(&b, 4433);
    let remote = at(&b, 4433);
    start(&a, "client", move |node| async move {
        drop(call(node, remote).await);
    });
    sim.run().unwrap_err()
}

#[test]
fn a_delayed_stream_panics() {
    let delayed = tcp::Options {
        delayed: true,
        ..options()
    };
    let message = "sim does not simulate delayed TCP sends yet";
    let connect = yet(link::Config::default(), move |node, remote| async move {
        connect(&node, remote, delayed).await.err()
    });
    assert_eq!(connect, panicked("client", message));
    let listen = yet(link::Config::default(), move |node, _| async move {
        let local = at(&node, 4433);
        listen_on(&node, local, delayed).local()
    });
    assert_eq!(listen, panicked("client", message));
}

#[test]
fn a_stream_on_a_lossy_link_panics() {
    let lossy = link::Config {
        loss: 0.01,
        ..link::Config::default()
    };
    let error = yet(lossy, |node, remote| async move {
        connect(&node, remote, options()).await.err()
    });
    let message = "sim does not simulate TCP on a lossy link yet";
    assert_eq!(error, panicked("client", message));
}

#[test]
fn a_connect_to_an_address_with_no_node_panics() {
    let message = "sim does not simulate TCP to an address with no node yet";
    // A node can have 10.0.0.3, but the run has two nodes.
    for ip in [[192, 0, 2, 1], [10, 0, 0, 3]] {
        let error = yet(link::Config::default(), move |node, _| async move {
            let remote = SocketAddr::from((ip, 4433));
            connect(&node, remote, options()).await.err()
        });
        assert_eq!(error, panicked("client", message), "{ip:?}");
    }
}

#[test]
#[should_panic(expected = "sim does not simulate a full TCP backlog yet")]
fn a_connect_to_a_full_backlog_panics() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let listen = tcp::Listen {
        local: at(&b, 4433),
        backlog: 0,
        options: options(),
    };
    let _listener = b.net().listen(&listen).unwrap();
    let remote = at(&b, 4433);
    start(&a, "client", move |node| async move {
        let _first = connect(&node, remote, options()).await.unwrap();
        let _second = connect(&node, remote, options()).await;
    });
    drop(sim.run());
}

#[test]
#[should_panic(expected = "sim does not simulate TCP on a lossy link yet")]
fn a_drop_between_runs_on_a_lossy_link_panics_at_the_next_run() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let _server = start(
        &b,
        "server",
        move |_| async move { accept(&mut listener).await },
    );
    let remote = at(&b, 4433);
    let client = start(&a, "client", move |node| async move {
        connect(&node, remote, options()).await.unwrap()
    });
    sim.run().unwrap();
    let lossy = link::Config {
        loss: 0.01,
        ..link::Config::default()
    };
    sim.link(&a, &b, lossy);
    drop(take(&client));
    drop(sim.run_for(Span::ZERO));
}

#[test]
fn a_receive_buffer_of_the_most_bytes_takes_a_stream() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let most = tcp::Options {
        recv_buffer_bytes: usize::MAX,
        ..options()
    };
    let mut listener = listen_on(&b, at(&b, 4433), most);
    let server = start(&b, "server", move |_| async move {
        let mut tcp = accept(&mut listener).await;
        read_all(&mut tcp).await
    });
    let remote = at(&b, 4433);
    let _client = start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        write_all(&mut tcp, &pattern(10_000)).await.unwrap();
        close(&mut tcp).await.unwrap();
    });
    sim.run().unwrap();
    assert_eq!(take(&server), (pattern(10_000), Ok(())));
}

#[test]
fn a_reset_frees_the_port_of_an_orphan_for_a_connect_after_the_ports_wrap() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        close(&mut tcp).await.unwrap();
        drop(tcp);
        node.clock().sleep(millis(10)).await;
        accept(&mut listener).await.peer()
    });
    let (remote, closed) = (at(&b, 4433), at(&b, 9));
    start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        read_all(&mut tcp).await.1.unwrap();
        drop(tcp);
        node.clock().sleep(millis(5)).await;
        wrap(&node, closed).await;
        let _tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(millis(1)).await;
    });
    sim.run().unwrap();
    assert_eq!(take(&server), at(&a, 49_152));
}

#[test]
fn bytes_to_an_orphan_reset_the_writer() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let mut listener = listen(&b, 4433);
    let server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        node.clock().sleep(millis(10)).await;
        write_all(&mut tcp, b"x").await.unwrap();
        node.clock().sleep(millis(1)).await;
        write_all(&mut tcp, b"y").await
    });
    let remote = at(&b, 4433);
    start(&a, "client", move |node| async move {
        let mut tcp = connect(&node, remote, options()).await.unwrap();
        close(&mut tcp).await.unwrap();
    });
    sim.run().unwrap();
    let reset = Net::Reset {
        remote: at(&a, 49_152),
    };
    assert_eq!(take(&server), Err(reset));
}

#[test]
fn a_power_cut_frees_the_port_of_an_orphan_for_a_connect_after_the_ports_wrap() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let small = tcp::Options {
        recv_buffer_bytes: 1 << 14,
        ..options()
    };
    let mut listener = listen_on(&b, at(&b, 4433), small);
    let server = start(&b, "server", move |node| async move {
        let mut tcp = accept(&mut listener).await;
        node.clock().sleep(millis(20)).await;
        let (bytes, read) = read_all(&mut tcp).await;
        drop(tcp);
        (bytes.len(), read, accept(&mut listener).await.peer())
    });
    let (remote, closed) = (at(&b, 4433), at(&b, 9));
    start(&a, "client", move |node| async move {
        let queued = tcp::Options {
            unsent_bytes_max: 1 << 20,
            ..options()
        };
        let mut tcp = connect(&node, remote, queued).await.unwrap();
        write_all(&mut tcp, &pattern(100_000)).await.unwrap();
        close(&mut tcp).await.unwrap();
        drop(tcp);
        pending::<()>().await;
    });
    sim.run_for(millis(10)).unwrap();
    sim.crash(&a, Crash::Power);
    start(&a, "restarted", move |node| async move {
        node.clock().sleep(millis(15)).await;
        wrap(&node, closed).await;
        let _tcp = connect(&node, remote, options()).await.unwrap();
        node.clock().sleep(millis(1)).await;
    });
    sim.run().unwrap();
    let peer = at(&a, 49_152);
    let reset = Net::Reset { remote: peer };
    assert_eq!(take(&server), (1 << 14, Err(reset), peer));
}
