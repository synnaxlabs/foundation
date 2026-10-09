//! The cost of writes over the loopback, through `os::net` and through a plain Tokio
//! stream with the same options: one 64-byte round trip, one write of 1 MiB, and 256
//! writes of 64 bytes back to back. The last two reach the unsent bound.

use std::future::poll_fn;
use std::io::IoSlice;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};

use divan::Bencher;
use env::net::{Tcp, tcp};
use env::shards::Config;
use env::thread::Handle;
use rustix::net::sockopt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::runtime::{Builder, Runtime};

const MESSAGE: [u8; 64] = [7; 64];
/// One round trip per sample, so the median and the slowest are those of one trip.
const TRIPS: u32 = 10_000;
const BULK: usize = 1 << 20;
const BURST: usize = 256;

fn main() {
    divan::main();
}

fn options() -> tcp::Options {
    tcp::Options {
        send_buffer_bytes: 1 << 16,
        recv_buffer_bytes: 1 << 16,
        unsent_bytes_max: 1 << 14,
        delayed: false,
    }
}

/// The two streams under test, behind the polls the round trip makes.
trait Io: Unpin {
    fn poll_read(&mut self, cx: &mut Context<'_>, buffer: &mut [u8]) -> Poll<usize>;
    fn poll_write(&mut self, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<usize>;
}

impl Io for Tcp {
    fn poll_read(&mut self, cx: &mut Context<'_>, buffer: &mut [u8]) -> Poll<usize> {
        Tcp::poll_read(self, cx, buffer).map(|read| read.expect("the peer is up"))
    }

    fn poll_write(&mut self, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<usize> {
        Tcp::poll_write(self, cx, &[IoSlice::new(bytes)])
            .map(|written| written.expect("the peer is up"))
    }
}

impl Io for TcpStream {
    fn poll_read(&mut self, cx: &mut Context<'_>, buffer: &mut [u8]) -> Poll<usize> {
        let mut read = ReadBuf::new(buffer);
        AsyncRead::poll_read(Pin::new(self), cx, &mut read).map(|outcome| {
            outcome.expect("the peer is up");
            read.filled().len()
        })
    }

    fn poll_write(&mut self, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<usize> {
        AsyncWrite::poll_write(Pin::new(self), cx, bytes)
            .map(|written| written.expect("the peer is up"))
    }
}

async fn write_all(stream: &mut impl Io, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let written = poll_fn(|cx| stream.poll_write(cx, bytes)).await;
        bytes = &bytes[written..];
    }
}

/// Reads until `buffer` is full, or gives `false` when the stream ended first.
async fn read_all(stream: &mut impl Io, mut buffer: &mut [u8]) -> bool {
    while !buffer.is_empty() {
        let read = poll_fn(|cx| stream.poll_read(cx, buffer)).await;
        if read == 0 {
            return false;
        }
        buffer = &mut buffer[read..];
    }
    true
}

/// Sends `MESSAGE` and reads the echo.
async fn round_trip(stream: &mut impl Io, buffer: &mut [u8; 64]) {
    write_all(stream, &MESSAGE).await;
    assert!(read_all(stream, buffer).await, "the echo is up");
}

/// Echoes each message of `stream` until the peer closes.
async fn echo(mut stream: impl Io) {
    let mut buffer = [0; 64];
    while read_all(&mut stream, &mut buffer).await {
        write_all(&mut stream, &buffer).await;
    }
}

/// Reads `chunk` bytes, then sends one byte, until the peer closes.
async fn sink(mut stream: impl Io, chunk: usize) {
    let mut buffer = vec![0; chunk];
    while read_all(&mut stream, &mut buffer).await {
        write_all(&mut stream, &[1]).await;
    }
}

/// Writes `bytes` and reads the sink's byte.
async fn bulk(stream: &mut impl Io, bytes: &[u8]) {
    write_all(stream, bytes).await;
    assert!(read_all(stream, &mut [0; 1]).await, "the sink is up");
}

/// Writes `MESSAGE` `BURST` times and reads the sink's byte.
async fn burst(stream: &mut impl Io) {
    for _ in 0..BURST {
        write_all(stream, &MESSAGE).await;
    }
    assert!(read_all(stream, &mut [0; 1]).await, "the sink is up");
}

/// A shard that runs `main`.
fn shard<F: Future<Output = ()> + 'static>(
    main: impl FnOnce() -> F + Send + 'static,
) -> Handle {
    let config = Config {
        name: "echo".into(),
        core: None,
    };
    os::shards()
        .expect("the OS gives the cores")
        .start(config, |_| main())
        .expect("the shard starts")
}

/// The runtime of the client: the one a shard has.
fn runtime() -> Runtime {
    Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("the runtime builds")
}

/// A client connected through `os::net` to a shard that runs `serve` on the stream.
fn connect_os<F: Future<Output = ()> + 'static>(
    runtime: &Runtime,
    serve: impl FnOnce(Tcp) -> F + Send + 'static,
) -> (Tcp, Handle) {
    let net = os::net();
    let listen = tcp::Listen {
        local: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
        backlog: 1,
        options: options(),
    };
    let mut listener = net.listen(&listen).expect("the loopback listens");
    let config = tcp::Config {
        remote: listener.local(),
        options: options(),
    };
    let server = shard(move || async move {
        let stream = poll_fn(|cx| listener.poll_accept(cx)).await;
        serve(stream.expect("the client connects")).await;
    });
    let client = runtime.block_on(net.connect(&config));
    (client.expect("the shard accepts"), server)
}

fn apply(stream: &TcpStream) {
    let options = options();
    sockopt::set_socket_send_buffer_size(stream, options.send_buffer_bytes)
        .expect("the buffer sets");
    sockopt::set_socket_recv_buffer_size(stream, options.recv_buffer_bytes)
        .expect("the buffer sets");
    sockopt::set_tcp_nodelay(stream, true).expect("nodelay sets");
}

/// A client connected through Tokio alone, with the options of [`connect_os`] except
/// the unsent bound, to a shard that runs `serve` on the stream.
#[expect(
    clippy::disallowed_methods,
    reason = "the bench compares with Tokio alone"
)]
fn connect_tokio<F: Future<Output = ()> + 'static>(
    runtime: &Runtime,
    serve: impl FnOnce(TcpStream) -> F + Send + 'static,
) -> (TcpStream, Handle) {
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0));
    let listener = listener.expect("the loopback listens");
    let remote = listener
        .local_addr()
        .expect("a bound socket has an address");
    let server = shard(move || async move {
        listener.set_nonblocking(true).expect("the flag sets");
        let listener = tokio::net::TcpListener::from_std(listener);
        let accepted = listener.expect("the runtime has I/O").accept().await;
        let (stream, _) = accepted.expect("the client connects");
        apply(&stream);
        serve(stream).await;
    });
    let client = runtime.block_on(TcpStream::connect(remote));
    let client = client.expect("the shard accepts");
    apply(&client);
    (client, server)
}

fn trips(bencher: Bencher<'_, '_>, runtime: &Runtime, client: &mut impl Io) {
    let mut buffer = [0; 64];
    bencher.bench_local(|| runtime.block_on(round_trip(client, &mut buffer)));
}

/// One round trip of 64 bytes through `os::net`, both ends on a Tokio I/O driver.
#[divan::bench(sample_size = 1, sample_count = TRIPS)]
fn os_net(bencher: Bencher<'_, '_>) {
    let runtime = runtime();
    let (mut client, server) = connect_os(&runtime, echo);
    trips(bencher, &runtime, &mut client);
    close(&runtime, client, server);
}

/// Closes `client` before the drop, which would reset the peer, and a shard aborts
/// at a panic.
fn close(runtime: &Runtime, mut client: Tcp, server: Handle) {
    let closed = runtime.block_on(poll_fn(|cx| client.poll_close(cx)));
    closed.expect("the close queues the FIN");
    drop(client);
    server.join().expect("the shard ends at the FIN");
}

/// The same round trip through a plain Tokio stream, for comparison.
#[divan::bench(sample_size = 1, sample_count = TRIPS)]
fn tokio(bencher: Bencher<'_, '_>) {
    let runtime = runtime();
    let (mut client, server) = connect_tokio(&runtime, echo);
    trips(bencher, &runtime, &mut client);
    drop(client);
    server.join().expect("the echo ends at the FIN");
}

/// One write of 1 MiB through `os::net`, then the sink's byte.
#[divan::bench(sample_size = 1, sample_count = 300)]
fn os_net_bulk(bencher: Bencher<'_, '_>) {
    let runtime = runtime();
    let (mut client, server) = connect_os(&runtime, |stream| sink(stream, BULK));
    let bytes = vec![7; BULK];
    bencher
        .counter(divan::counter::BytesCount::new(BULK))
        .bench_local(|| runtime.block_on(bulk(&mut client, &bytes)));
    close(&runtime, client, server);
}

/// The same write through a plain Tokio stream, for comparison.
#[divan::bench(sample_size = 1, sample_count = 300)]
fn tokio_bulk(bencher: Bencher<'_, '_>) {
    let runtime = runtime();
    let (mut client, server) = connect_tokio(&runtime, |stream| sink(stream, BULK));
    let bytes = vec![7; BULK];
    bencher
        .counter(divan::counter::BytesCount::new(BULK))
        .bench_local(|| runtime.block_on(bulk(&mut client, &bytes)));
    drop(client);
    server.join().expect("the sink ends at the FIN");
}

/// 256 writes of 64 bytes back to back through `os::net`, then the sink's byte.
#[divan::bench(sample_size = 1, sample_count = 2000)]
fn os_net_burst(bencher: Bencher<'_, '_>) {
    let runtime = runtime();
    let (mut client, server) = connect_os(&runtime, |stream| sink(stream, BURST * 64));
    bencher
        .counter(divan::counter::ItemsCount::new(BURST))
        .bench_local(|| runtime.block_on(burst(&mut client)));
    close(&runtime, client, server);
}

/// The same writes through a plain Tokio stream, for comparison.
#[divan::bench(sample_size = 1, sample_count = 2000)]
fn tokio_burst(bencher: Bencher<'_, '_>) {
    let runtime = runtime();
    let (mut client, server) =
        connect_tokio(&runtime, |stream| sink(stream, BURST * 64));
    bencher
        .counter(divan::counter::ItemsCount::new(BURST))
        .bench_local(|| runtime.block_on(burst(&mut client)));
    drop(client);
    server.join().expect("the sink ends at the FIN");
}
