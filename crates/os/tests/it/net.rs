//! TCP streams and listeners on the loopback of the real network: bytes, FIN, resets,
//! errors, addresses, wakeups, and the thread rules.

use std::future::poll_fn;
use std::io::IoSlice;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use env::net::{Error, Listener, Net, Tcp, tcp, udp};
use env::shards::Config;
use tokio::sync::Notify;
use tokio::time::timeout;

use crate::common::assert_joins;

const LOCALHOST: Ipv4Addr = Ipv4Addr::LOCALHOST;
/// The bound of each wait in these tests.
const BOUND: Duration = Duration::from_secs(10);

fn net() -> Net {
    os::net()
}

fn options() -> tcp::Options {
    tcp::Options {
        send_buffer_bytes: 1 << 16,
        recv_buffer_bytes: 1 << 16,
        unsent_bytes_max: 1 << 14,
        delayed: false,
    }
}

fn listen_config(local: SocketAddr) -> tcp::Listen {
    tcp::Listen {
        local,
        backlog: 8,
        options: options(),
    }
}

fn connect_config(remote: SocketAddr) -> tcp::Config {
    tcp::Config {
        remote,
        options: options(),
    }
}

/// Listens on a free port of the loopback.
fn listen(net: &Net) -> Listener {
    net.listen(&listen_config(SocketAddr::new(LOCALHOST.into(), 0)))
        .expect("the loopback has a free port")
}

async fn connect(net: &Net, remote: SocketAddr) -> Tcp {
    net.connect(&connect_config(remote))
        .await
        .expect("the listener accepts the connect")
}

async fn accept(listener: &mut Listener) -> Tcp {
    poll_fn(|cx| listener.poll_accept(cx))
        .await
        .expect("a stream waits in the backlog")
}

/// A listener, a connected stream, and its accepted peer, on the loopback.
async fn create_pair(net: &Net) -> (Listener, Tcp, Tcp) {
    let mut listener = listen(net);
    let client = connect(net, listener.local()).await;
    let server = accept(&mut listener).await;
    (listener, client, server)
}

/// One vectored write of `parts`.
async fn write(tcp: &mut Tcp, parts: &[&[u8]]) -> Result<usize, Error> {
    let slices: Vec<IoSlice<'_>> = parts.iter().map(|p| IoSlice::new(p)).collect();
    poll_fn(|cx| tcp.poll_write(cx, &slices)).await
}

async fn read(tcp: &mut Tcp, buffer: &mut [u8]) -> Result<usize, Error> {
    poll_fn(|cx| tcp.poll_read(cx, buffer)).await
}

/// Reads until `buffer` is full.
async fn read_exact(tcp: &mut Tcp, buffer: &mut [u8]) {
    let mut filled = 0;
    while filled < buffer.len() {
        let n = read(tcp, &mut buffer[filled..])
            .await
            .expect("the peer's bytes arrive");
        assert_ne!(n, 0, "the peer sent FIN before all its bytes");
        filled += n;
    }
}

async fn close(tcp: &mut Tcp) -> Result<(), Error> {
    poll_fn(|cx| tcp.poll_close(cx)).await
}

/// Runs `body` on a dedicated thread of `os`, as a connector does, and gives its
/// result once the thread ends. A panic in the body fails the test.
fn on_thread<T: Send + 'static, F: Future<Output = T> + 'static>(
    name: &str,
    body: impl FnOnce() -> F + Send + 'static,
) -> T {
    let (sent, received) = mpsc::channel();
    let handle = os::threads()
        .expect("the OS gives the cores of this process")
        .start(name, move || async move {
            sent.send(body().await)
                .expect("the test waits for the result");
        })
        .expect("the thread starts");
    assert_joins(handle, Ok(()));
    received.try_recv().expect("the body sent its result")
}

/// Runs `body` on a shard of `os`.
fn on_shard<T: Send + 'static, F: Future<Output = T> + 'static>(
    body: impl FnOnce() -> F + Send + 'static,
) -> T {
    let (sent, received) = mpsc::channel();
    let config = Config {
        name: "shard-net".into(),
        core: None,
    };
    let handle = os::shards()
        .expect("the OS gives the cores of this process")
        .start(config, move |_| async move {
            sent.send(body().await)
                .expect("the test waits for the result");
        })
        .expect("the shard starts");
    assert_joins(handle, Ok(()));
    received.try_recv().expect("the body sent its result")
}

/// A current-thread runtime with an I/O driver, for a poll on the test thread.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .expect("a current-thread runtime builds")
}

fn runtime_with_no_io() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds")
}

#[test]
fn a_vectored_write_reaches_the_peer_in_order() {
    on_thread("net-write", || async {
        let net = net();
        let (_listener, mut client, mut server) = create_pair(&net).await;
        let written = write(&mut client, &[b"foo", b"bar", b"baz"]).await;
        assert_eq!(written, Ok(9));
        let mut received = [0; 9];
        read_exact(&mut server, &mut received).await;
        assert_eq!(&received, b"foobarbaz");
    });
}

/// An empty first part holds the write to no part on macOS, which caps each write.
#[test]
fn a_vectored_write_with_an_empty_first_part_takes_bytes() {
    on_thread("net-write-empty-first", || async {
        let net = net();
        let (_listener, mut client, mut server) = create_pair(&net).await;
        let big = vec![7; 2 * options().unsent_bytes_max];
        let written = write(&mut client, &[&[], &big]).await;
        assert!(matches!(written, Ok(1..)), "{written:?}");
        let mut received = [0; 1];
        read_exact(&mut server, &mut received).await;
    });
}

#[test]
fn a_round_trip_runs_on_a_shard() {
    on_shard(|| async {
        let net = net();
        let (_listener, mut client, mut server) = create_pair(&net).await;
        assert_eq!(write(&mut client, &[b"ping"]).await, Ok(4));
        let mut received = [0; 4];
        read_exact(&mut server, &mut received).await;
        assert_eq!(write(&mut server, &[b"pong"]).await, Ok(4));
        read_exact(&mut client, &mut received).await;
        assert_eq!(&received, b"pong");
    });
}

#[test]
fn close_delivers_the_queued_bytes_then_fin() {
    on_thread("net-close", || async {
        let net = net();
        let (_listener, mut client, mut server) = create_pair(&net).await;
        assert_eq!(write(&mut client, &[b"hello"]).await, Ok(5));
        assert_eq!(close(&mut client).await, Ok(()));
        let mut received = [0; 5];
        read_exact(&mut server, &mut received).await;
        assert_eq!(&received, b"hello");
        assert_eq!(read(&mut server, &mut [0; 8]).await, Ok(0));
    });
}

#[test]
fn a_drop_before_close_resets_the_peer() {
    on_thread("net-drop", || async {
        let net = net();
        let (_listener, mut client, mut server) = create_pair(&net).await;
        assert_eq!(write(&mut client, &[b"lost"]).await, Ok(4));
        let remote = client.local();
        drop(client);
        let mut received = [0; 8];
        let mut outcome = read(&mut server, &mut received).await;
        if outcome == Ok(4) {
            outcome = read(&mut server, &mut received).await;
        }
        assert_eq!(outcome, Err(Error::Reset { remote }));
    });
}

#[test]
fn an_accepted_stream_dropped_before_close_resets_its_peer() {
    on_thread("net-drop-accepted", || async {
        let net = net();
        let (_listener, mut client, server) = create_pair(&net).await;
        let remote = server.local();
        drop(server);
        read_reset(&mut client, remote).await;
    });
}

/// Writes `bytes` to `tcp` with one poll, with no waker, and gives the count taken.
/// The stream has polled before, so the kernel's readiness is known.
fn write_once(tcp: &mut Tcp, bytes: &[u8]) -> Poll<usize> {
    let mut cx = Context::from_waker(Waker::noop());
    tcp.poll_write(&mut cx, &[IoSlice::new(bytes)])
        .map(|outcome| outcome.expect("the write takes bytes or waits"))
}

#[test]
fn a_drop_after_close_delivers_the_bytes_the_kernel_still_holds() {
    on_thread("net-close-drop", || async {
        let net = net();
        let (_listener, mut client, mut server) = create_pair(&net).await;
        assert_eq!(write(&mut client, &[&[7]]).await, Ok(1));
        let bytes = vec![7; 1 << 20];
        let queued = write(&mut client, &[&bytes]).await;
        let queued = queued.expect("an empty send buffer takes bytes") + 1;
        assert!(queued < 1 << 20, "the kernel holds the rest: {queued}");
        assert_eq!(close(&mut client).await, Ok(()));
        drop(client);
        let mut received = 0;
        let mut buffer = vec![0; 1 << 16];
        loop {
            match read(&mut server, &mut buffer).await {
                Ok(0) => break,
                Ok(n) => received += n,
                Err(e) => panic!("the close delivers each byte: {e}"),
            }
        }
        assert_eq!(received, queued);
    });
}

/// With a peer that reads nothing, the kernel sends until the peer's receive buffer
/// is full. The write then waits at the unsent bound, with most of the send buffer
/// still free. macOS waits for the write event once the bytes written since the last
/// wait reach the bound, so the next poll that no event preceded waits.
#[test]
fn a_write_waits_at_the_unsent_bound_not_the_send_buffer() {
    on_thread("net-unsent", || async {
        let net = net();
        let mut listener = listen(&net);
        let mut config = connect_config(listener.local());
        config.options.send_buffer_bytes = 1 << 20;
        let mut client = net.connect(&config).await.expect("the listener accepts");
        let _server = accept(&mut listener).await;
        let mut written = write(&mut client, &[&[7]]).await.expect("one byte");
        let bytes = vec![7; 1 << 22];
        while let Poll::Ready(n) = write_once(&mut client, &bytes) {
            written += n;
            assert!(written < 1 << 20, "the send buffer never fills: {written}");
        }
        if cfg!(target_os = "macos") {
            let max = config.options.unsent_bytes_max;
            assert!(written < 2 * max, "a write waits for the event: {written}");
        } else {
            assert!(
                written > 1 << 16,
                "the peer's buffer fills first: {written}"
            );
        }
    });
}

/// Writes of 64 bytes that total less than the unsent bound each go at once, with no
/// read on the peer: macOS waits for the write event only once the bound is reached.
#[test]
fn small_writes_below_the_unsent_bound_wait_for_no_event() {
    on_thread("net-small", || async {
        let net = net();
        let (_listener, mut client, _server) = create_pair(&net).await;
        assert_eq!(write(&mut client, &[&[7; 64]]).await, Ok(64));
        for _ in 2..options().unsent_bytes_max / 64 {
            assert_eq!(write_once(&mut client, &[7; 64]), Poll::Ready(64));
        }
    });
}

/// Linux takes the FIN before the RST: the read gives 0, and a write gives the reset.
/// macOS takes both from a queue, so its read gives the reset or 0.
#[test]
fn a_drop_with_unread_bytes_after_close_resets_the_peer() {
    on_thread("net-unread", || async {
        let net = net();
        let (_listener, mut client, mut server) = create_pair(&net).await;
        assert_eq!(write(&mut server, &[b"data"]).await, Ok(4));
        assert_eq!(read(&mut client, &mut [0; 1]).await, Ok(1));
        assert_eq!(close(&mut client).await, Ok(()));
        let remote = client.local();
        drop(client);
        let reset = Err(Error::Reset { remote });
        let outcome = read(&mut server, &mut [0; 8]).await;
        if cfg!(target_os = "macos") && outcome == reset {
            return;
        }
        assert_eq!(outcome, Ok(0));
        let written = timeout(BOUND, async {
            loop {
                match write(&mut server, &[b"more"]).await {
                    Ok(_) => tokio::task::yield_now().await,
                    outcome => break outcome,
                }
            }
        });
        assert_eq!(
            written.await.expect("the reset arrives in the bound"),
            reset
        );
    });
}

/// Reads `server` until the reset of its peer shows, past any bytes before it.
async fn read_reset(server: &mut Tcp, remote: SocketAddr) {
    let reset = Err(Error::Reset { remote });
    let outcome = timeout(BOUND, async {
        loop {
            match read(server, &mut [0; 8]).await {
                Ok(n) if n > 0 => {}
                outcome => break outcome,
            }
        }
    });
    assert_eq!(
        outcome.await.expect("the reset arrives in the bound"),
        reset
    );
}

#[test]
fn each_poll_after_a_read_found_the_reset_is_reset() {
    on_thread("net-reset-read", || async {
        let net = net();
        let (_listener, client, mut server) = create_pair(&net).await;
        let remote = client.local();
        drop(client);
        read_reset(&mut server, remote).await;
        let reset = Err(Error::Reset { remote });
        assert_eq!(read(&mut server, &mut [0; 8]).await, reset);
        assert_eq!(write(&mut server, &[b"x"]).await, reset);
        assert_eq!(close(&mut server).await, reset.map(|_: usize| ()));
    });
}

/// Drops `dropped`, and waits until its reset reached `kept`, with no poll that
/// reports it. Gives the address of the dropped end.
async fn reset_unseen(dropped: Tcp, kept: &mut Tcp) -> SocketAddr {
    let counted = Arc::new(Counted {
        wakes: AtomicUsize::new(0),
        woken: Notify::new(),
    });
    let waker = Waker::from(Arc::clone(&counted));
    let mut cx = Context::from_waker(&waker);
    assert_eq!(kept.poll_read(&mut cx, &mut [0; 8]), Poll::Pending);
    let remote = dropped.local();
    drop(dropped);
    timeout(BOUND, counted.woken.notified())
        .await
        .expect("the reset wakes the reader");
    remote
}

#[test]
fn a_close_after_an_unseen_reset_is_reset() {
    on_thread("net-reset-close", || async {
        let net = net();
        let (_listener, client, mut server) = create_pair(&net).await;
        let remote = reset_unseen(client, &mut server).await;
        assert_eq!(close(&mut server).await, Err(Error::Reset { remote }));
        assert_eq!(
            read(&mut server, &mut [0; 8]).await,
            Err(Error::Reset { remote })
        );
    });
}

#[test]
fn each_poll_after_a_write_found_the_reset_is_reset() {
    on_thread("net-reset-write", || async {
        let net = net();
        let (_listener, client, mut server) = create_pair(&net).await;
        let remote = reset_unseen(client, &mut server).await;
        let reset = Err(Error::Reset { remote });
        assert_eq!(write(&mut server, &[b"x"]).await, reset);
        assert_eq!(read(&mut server, &mut [0; 8]).await, reset);
    });
}

#[test]
fn a_second_close_after_an_unseen_reset_is_reset() {
    on_thread("net-close-twice", || async {
        let net = net();
        let (_listener, mut client, server) = create_pair(&net).await;
        assert_eq!(close(&mut client).await, Ok(()));
        let remote = reset_unseen(server, &mut client).await;
        assert_eq!(close(&mut client).await, Err(Error::Reset { remote }));
    });
}

/// Writes to `tcp` until a write fails, and gives the failure.
async fn write_until_failed(tcp: &mut Tcp) -> Error {
    let failed = timeout(BOUND, async {
        loop {
            if let Err(e) = write(tcp, &[b"x"]).await {
                break e;
            }
        }
    });
    failed.await.expect("the reset arrives in the bound")
}

#[test]
fn bytes_the_peer_sent_before_a_reset_a_write_found_are_still_read() {
    on_thread("net-reset-data", || async {
        let net = net();
        let (_listener, mut client, mut server) = create_pair(&net).await;
        let remote = client.local();
        assert_eq!(write(&mut client, &[b"data"]).await, Ok(4));
        drop(client);
        let reset = Error::Reset { remote };
        assert_eq!(write_until_failed(&mut server).await, reset);
        let mut received = [0; 8];
        assert_eq!(read(&mut server, &mut received).await, Ok(4));
        assert_eq!(&received[..4], b"data");
        assert_eq!(read(&mut server, &mut received).await, Err(reset));
    });
}

#[test]
fn a_write_after_the_close_is_a_broken_pipe() {
    on_thread("net-write-closed", || async {
        let net = net();
        let (_listener, mut client, _server) = create_pair(&net).await;
        assert_eq!(close(&mut client).await, Ok(()));
        let pipe = Err(Error::Io { code: 32 });
        assert_eq!(write(&mut client, &[b"late"]).await, pipe);
        assert_eq!(
            write(&mut client, &[b"late"]).await,
            pipe,
            "no end of stream"
        );
        assert_eq!(close(&mut client).await, Ok(()));
    });
}

#[test]
fn a_write_after_the_close_and_an_unseen_reset_is_reset() {
    on_thread("net-write-closed-reset", || async {
        let net = net();
        let (_listener, mut client, server) = create_pair(&net).await;
        assert_eq!(close(&mut client).await, Ok(()));
        let remote = reset_unseen(server, &mut client).await;
        let reset = Err(Error::Reset { remote });
        assert_eq!(write(&mut client, &[b"late"]).await, reset);
        assert_eq!(read(&mut client, &mut [0; 8]).await, reset);
        assert_eq!(close(&mut client).await, reset.map(|_: usize| ()));
    });
}

#[test]
fn a_listen_binds_a_port_in_time_wait() {
    on_thread("net-time-wait", || async {
        let net = net();
        let (listener, mut client, mut server) = create_pair(&net).await;
        let local = listener.local();
        assert_eq!(close(&mut server).await, Ok(()));
        assert_eq!(read(&mut client, &mut [0; 8]).await, Ok(0));
        assert_eq!(close(&mut client).await, Ok(()));
        assert_eq!(read(&mut server, &mut [0; 8]).await, Ok(0));
        drop((listener, client, server));
        let again = net.listen(&listen_config(local)).map(|l| l.local());
        assert_eq!(again, Ok(local));
    });
}

#[test]
fn an_ipv4_peer_of_an_any_v6_listener_has_a_plain_ipv4_address() {
    on_thread("net-mapped", || async {
        let net = net();
        let any = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0);
        let mut listener = net.listen(&listen_config(any)).expect("v6 any binds");
        let remote = SocketAddr::new(LOCALHOST.into(), listener.local().port());
        let client = connect(&net, remote).await;
        let server = accept(&mut listener).await;
        assert_eq!(server.peer(), client.local());
        assert_eq!(server.local(), client.peer());
    });
}

#[test]
fn a_listener_on_a_mapped_address_agrees_with_its_streams() {
    on_thread("net-mapped", || async {
        let net = net();
        let mapped: SocketAddr = "[::ffff:127.0.0.1]:0".parse().unwrap();
        let mut listener = net.listen(&listen_config(mapped)).unwrap();
        let client = connect(&net, listener.local()).await;
        let server = accept(&mut listener).await;
        assert_eq!(listener.local().ip(), LOCALHOST);
        assert_eq!(server.local(), listener.local());
        assert_eq!(client.peer(), server.local());
        assert_eq!(server.peer(), client.local());
    });
}

/// Connects to `remote`, reached through `listener`, whose stream resets after the
/// handshake and before the connect ends.
async fn connect_reset(net: &Net, listener: &mut Listener, remote: SocketAddr) -> Tcp {
    let config = connect_config(remote);
    let mut connecting = pin!(net.connect(&config));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(connecting.as_mut().poll(&mut cx).is_pending());
    drop(accept(listener).await);
    // macOS takes the reset on loopback from a queue, in order: once a later
    // handshake ends, the reset has come.
    let after = connect(net, listener.local()).await;
    drop((accept(listener).await, after));
    connecting.await.expect("the handshake completed")
}

/// The connect has taken the reset, so the stream gives it with no kernel error left.
#[test]
fn a_connect_whose_peer_resets_after_the_handshake_gives_a_stream_that_is_reset() {
    on_thread("net-connect", || async {
        let net = net();
        let mut listener = listen(&net);
        let remote = listener.local();
        let mut client = connect_reset(&net, &mut listener, remote).await;
        assert_eq!(client.peer(), remote);
        read_reset(&mut client, remote).await;
        let reset = Error::Reset { remote };
        assert_eq!(write(&mut client, &[b"late"]).await, Err(reset.clone()));
        assert_eq!(close(&mut client).await, Err(reset));
    });
}

/// After a reset the kernel holds no peer, so the stream names the remote as
/// `canonical` gives it.
#[test]
fn a_connect_reset_in_its_handshake_names_the_remote_it_was_given() {
    on_thread("net-connect", || async {
        let net = net();
        let mut v4 = listen(&net);
        let local = v4.local();
        let mapped = SocketAddr::new(LOCALHOST.to_ipv6_mapped().into(), local.port());
        let any = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), local.port());
        let v6 = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 0);
        let mut v6 = net.listen(&listen_config(v6)).expect("::1 binds");
        let port = v6.local().port();
        let scoped = SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 2, 7).into();
        let cases = [
            (false, mapped, local),
            (false, any, any),
            (true, scoped, scoped),
        ];
        for (on_v6, remote, named) in cases {
            let listener = if on_v6 { &mut v6 } else { &mut v4 };
            let mut client = connect_reset(&net, listener, remote).await;
            assert_eq!(client.peer(), named, "{remote}");
            read_reset(&mut client, named).await;
        }
    });
}

#[test]
fn a_connect_to_a_mapped_address_names_plain_ipv4_ends() {
    on_thread("net-mapped", || async {
        let net = net();
        let mut listener = listen(&net);
        let port = listener.local().port();
        let mapped = SocketAddr::new(LOCALHOST.to_ipv6_mapped().into(), port);
        let client = connect(&net, mapped).await;
        let server = accept(&mut listener).await;
        assert_eq!(client.peer(), listener.local());
        assert_eq!(client.local(), server.peer());
    });
}

#[test]
fn a_connect_with_a_scope_and_flow_label_the_kernel_ignores_names_the_real_peer() {
    on_thread("net-scope", || async {
        let net = net();
        let v6 = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 0);
        let mut listener = net.listen(&listen_config(v6)).expect("::1 binds");
        let port = listener.local().port();
        let remote = SocketAddrV6::new(Ipv6Addr::LOCALHOST, port, 2, 7);
        let client = connect(&net, remote.into()).await;
        let server = accept(&mut listener).await;
        assert_eq!(server.peer(), client.local());
        assert_eq!(client.peer(), server.local());
    });
}

#[test]
fn a_refused_connect_to_a_mapped_address_names_the_ipv4_address() {
    on_thread("net-refused", || async {
        let net = net();
        let remote = listen(&net).local();
        let mapped = SocketAddr::new(LOCALHOST.to_ipv6_mapped().into(), remote.port());
        let outcome = net.connect(&connect_config(mapped)).await.map(|_| ());
        assert_eq!(outcome, Err(Error::Refused { remote }));
    });
}

#[test]
#[cfg(target_pointer_width = "64")]
fn an_unsent_bound_past_a_c_int_fails_the_listen_and_the_connect() {
    on_thread("net-bad-option", || async {
        let net = net();
        let listener = listen(&net);
        let mut config = connect_config(listener.local());
        config.options.unsent_bytes_max = usize::MAX;
        let invalid = Err(Error::Io { code: 22 });
        let outcome = net.connect(&config).await.map(|_| ());
        assert_eq!(outcome, invalid);
        let mut config = listen_config(SocketAddr::new(LOCALHOST.into(), 0));
        config.options.unsent_bytes_max = usize::MAX;
        assert_eq!(net.listen(&config).map(|_| ()), invalid);
    });
}

#[test]
fn a_connect_to_a_port_with_no_listener_is_refused() {
    on_thread("net-refused", || async {
        let net = net();
        let remote = listen(&net).local();
        let outcome = net.connect(&connect_config(remote)).await.map(|_| ());
        assert_eq!(outcome, Err(Error::Refused { remote }));
    });
}

#[test]
fn a_listen_on_a_held_address_is_in_use() {
    let net = net();
    let held = listen(&net);
    let local = held.local();
    let outcome = net.listen(&listen_config(local)).map(|_| ());
    assert_eq!(outcome, Err(Error::AddressInUse { local }));
}

#[test]
fn port_zero_binds_a_free_port_and_each_end_knows_the_addresses() {
    on_thread("net-addresses", || async {
        let net = net();
        let (listener, client, server) = create_pair(&net).await;
        let local = listener.local();
        assert_ne!(local.port(), 0);
        assert_eq!(local.ip(), LOCALHOST);
        assert_eq!(client.peer(), local);
        assert_eq!(server.local(), local);
        assert_eq!(server.peer(), client.local());
        assert_eq!(client.local().ip(), LOCALHOST);
        assert_ne!(client.local().port(), local.port());
    });
}

/// A waker that counts its wakes and ends a wait for one.
struct Counted {
    wakes: AtomicUsize,
    woken: Notify,
}

impl Wake for Counted {
    fn wake(self: Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
        self.woken.notify_one();
    }
}

#[test]
fn a_read_with_no_data_is_pending_until_the_peer_writes() {
    on_thread("net-pending", || async {
        let net = net();
        let (_listener, mut client, mut server) = create_pair(&net).await;
        let counted = Arc::new(Counted {
            wakes: AtomicUsize::new(0),
            woken: Notify::new(),
        });
        let waker = Waker::from(Arc::clone(&counted));
        let mut cx = Context::from_waker(&waker);
        let mut received = [0; 8];
        assert_eq!(server.poll_read(&mut cx, &mut received), Poll::Pending);
        assert_eq!(counted.wakes.load(Ordering::SeqCst), 0);
        assert_eq!(write(&mut client, &[b"wake"]).await, Ok(4));
        timeout(BOUND, counted.woken.notified())
            .await
            .expect("the write wakes the reader");
        assert_ne!(counted.wakes.load(Ordering::SeqCst), 0);
        assert_eq!(server.poll_read(&mut cx, &mut received), Poll::Ready(Ok(4)));
        assert_eq!(&received[..4], b"wake");
    });
}

#[test]
#[should_panic(expected = "a TCP stream polls only on the thread of its first poll")]
fn a_stream_poll_on_a_second_thread_panics() {
    let (mut client, _server, _listener) = on_thread("net-first", || async {
        let net = net();
        let (listener, mut client, server) = create_pair(&net).await;
        assert_eq!(write(&mut client, &[b"x"]).await, Ok(1));
        (client, server, listener)
    });
    runtime().block_on(async {
        drop(write(&mut client, &[b"y"]).await);
    });
}

#[test]
#[should_panic(expected = "a TCP stream polls only on the thread of its first poll")]
fn a_poll_after_the_close_on_a_second_thread_panics() {
    let (mut client, _server, _listener) = on_thread("net-first", || async {
        let net = net();
        let (listener, mut client, server) = create_pair(&net).await;
        assert_eq!(close(&mut client).await, Ok(()));
        (client, server, listener)
    });
    runtime().block_on(async {
        drop(write(&mut client, &[b"y"]).await);
    });
}

#[test]
#[should_panic(expected = "a TCP stream polls only on the thread of its first poll")]
fn a_poll_after_a_reset_on_a_second_thread_panics() {
    let (mut server, _listener) = on_thread("net-first", || async {
        let net = net();
        let (listener, client, mut server) = create_pair(&net).await;
        let remote = client.local();
        drop(client);
        read_reset(&mut server, remote).await;
        (server, listener)
    });
    runtime().block_on(async {
        drop(write(&mut server, &[b"y"]).await);
    });
}

#[test]
#[should_panic(expected = "a TCP stream polls only on the thread of its first poll")]
fn a_close_after_a_reset_on_a_second_thread_panics() {
    let (mut server, _listener) = on_thread("net-first", || async {
        let net = net();
        let (listener, client, mut server) = create_pair(&net).await;
        let remote = client.local();
        drop(client);
        read_reset(&mut server, remote).await;
        (server, listener)
    });
    runtime().block_on(async {
        drop(close(&mut server).await);
    });
}

#[test]
#[should_panic(expected = "a TCP listener polls only on the thread of its first poll")]
fn a_listener_poll_on_a_second_thread_panics() {
    let (mut listener, _client, _server) = on_thread("net-first", || async {
        let net = net();
        let (listener, client, server) = create_pair(&net).await;
        (listener, client, server)
    });
    runtime().block_on(async {
        drop(poll_fn(|cx| listener.poll_accept(cx)).await);
    });
}

#[test]
#[should_panic(expected = "must be called from the context of a Tokio 1.x runtime")]
fn a_first_stream_poll_with_no_runtime_panics() {
    let (mut client, _listener) = on_thread("net-connect", || async {
        let net = net();
        let listener = listen(&net);
        (connect(&net, listener.local()).await, listener)
    });
    let mut cx = Context::from_waker(Waker::noop());
    drop(client.poll_read(&mut cx, &mut [0; 8]));
}

#[test]
#[should_panic(expected = "must be called from the context of a Tokio 1.x runtime")]
fn a_connect_with_no_runtime_panics() {
    let net = net();
    let listener = listen(&net);
    let mut cx = Context::from_waker(Waker::noop());
    let mut connecting = pin!(connect(&net, listener.local()));
    drop(connecting.as_mut().poll(&mut cx));
}

#[test]
#[should_panic(expected = "A Tokio 1.x context was found, but IO is disabled")]
fn a_first_listener_poll_in_a_runtime_with_no_io_driver_panics() {
    let net = net();
    let mut listener = listen(&net);
    runtime_with_no_io().block_on(async {
        drop(poll_fn(|cx| listener.poll_accept(cx)).await);
    });
}

#[test]
#[should_panic(expected = "A Tokio 1.x context was found, but IO is disabled")]
fn a_first_stream_poll_in_a_runtime_with_no_io_driver_panics() {
    let (mut client, _listener) = on_thread("net-connect", || async {
        let net = net();
        let listener = listen(&net);
        (connect(&net, listener.local()).await, listener)
    });
    runtime_with_no_io().block_on(async {
        drop(read(&mut client, &mut [0; 8]).await);
    });
}

#[test]
#[should_panic(expected = "A Tokio 1.x context was found, but IO is disabled")]
fn a_connect_in_a_runtime_with_no_io_driver_panics() {
    let net = net();
    let listener = listen(&net);
    runtime_with_no_io().block_on(async {
        drop(net.connect(&connect_config(listener.local())).await);
    });
}

#[test]
fn an_accepted_stream_moves_to_another_thread_before_its_first_poll() {
    let (_listener, client, server) = on_thread("net-accept", || async {
        let net = net();
        create_pair(&net).await
    });
    on_thread("net-use", move || async move {
        let (mut client, mut server) = (client, server);
        assert_eq!(write(&mut client, &[b"moved"]).await, Ok(5));
        let mut received = [0; 5];
        read_exact(&mut server, &mut received).await;
        assert_eq!(&received, b"moved");
    });
}

#[test]
#[should_panic(expected = "os::net has no UDP driver yet")]
fn udp_panics() {
    let config = udp::Config {
        local: SocketAddr::new(LOCALHOST.into(), 0),
        send_buffer_bytes: 1 << 16,
        recv_buffer_bytes: 1 << 16,
    };
    drop(net().udp(&config));
}

#[test]
#[should_panic(expected = "os::net has no resolver yet")]
fn resolve_of_a_host_name_panics() {
    runtime().block_on(async {
        drop(net().resolve("localhost", 80).await);
    });
}

#[test]
fn resolve_of_a_literal_needs_no_resolver() {
    let found = runtime().block_on(net().resolve("127.0.0.1", 80));
    assert_eq!(found, Ok(vec![SocketAddr::new(LOCALHOST.into(), 80)]));
}
