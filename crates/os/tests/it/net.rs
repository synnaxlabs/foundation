//! TCP streams and listeners on the loopback of the real network: bytes, FIN, resets,
//! errors, addresses, wakeups, and the thread rules.

use std::future::poll_fn;
use std::io::IoSlice;
use std::net::{Ipv4Addr, SocketAddr};
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

/// Linux takes the FIN before the RST: the read gives 0, and a write gives the reset.
/// macOS gives the reset on the read.
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
        if cfg!(target_os = "macos") {
            assert_eq!(outcome, reset);
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

/// Drops `client`, and waits until its reset reached `server`, with no poll that
/// reports it. Gives the address of the dropped end.
async fn reset_unseen(client: Tcp, server: &mut Tcp) -> SocketAddr {
    let counted = Arc::new(Counted {
        wakes: AtomicUsize::new(0),
        woken: Notify::new(),
    });
    let waker = Waker::from(Arc::clone(&counted));
    let mut cx = Context::from_waker(&waker);
    assert_eq!(server.poll_read(&mut cx, &mut [0; 8]), Poll::Pending);
    let remote = client.local();
    drop(client);
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
#[should_panic(expected = "A Tokio 1.x context was found, but IO is disabled")]
fn a_first_listener_poll_in_a_runtime_with_no_io_driver_panics() {
    let net = net();
    let mut listener = listen(&net);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime builds");
    runtime.block_on(async {
        drop(poll_fn(|cx| listener.poll_accept(cx)).await);
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
