#![expect(clippy::arithmetic_side_effects, reason = "a test may panic")]

use std::future::poll_fn;
use std::io::IoSlice;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use env::clock::Clock;
use env::net::{self, Tcp, tcp};
use env::thread::Handle;
use http::{Request, Response, StatusCode};
use sim::{Sim, node};
use types::time::Span;

use super::{Client, Config, Error};

const PORT: u16 = 8086;
const TIMEOUT: Span = Span::from_nanos(10_000_000_000);
const BODY_MAX: usize = 64;

fn shard(name: &str) -> env::shards::Config {
    env::shards::Config {
        name: name.into(),
        core: Some(0),
    }
}

/// A run with a client node and a server node.
struct Network {
    sim: Sim,
    client: node::Node,
    server: node::Node,
    handles: Vec<Handle>,
    /// How long the last `send` took.
    elapsed: Option<Span>,
}

impl Network {
    fn new(seed: u64) -> Self {
        let mut sim = Sim::new(sim::Config {
            seed,
            steps_max: 10_000_000,
            ..sim::Config::default()
        });
        let client = sim.node(node::Config::default());
        let server = sim.node(node::Config::default());
        Self {
            sim,
            client,
            server,
            handles: Vec::new(),
            elapsed: None,
        }
    }

    fn remote(&self) -> SocketAddr {
        self.remote_on(PORT)
    }

    fn remote_on(&self, port: u16) -> SocketAddr {
        SocketAddr::new(self.server.addresses()[0], port)
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.remote())
    }

    /// Answers the first stream on the server node with `answer`, and gives the bytes
    /// of the request it read. `answer` gives the stream back to keep it open.
    fn serve<F: Future<Output = Option<Tcp>> + 'static>(
        &mut self,
        answer: impl FnOnce(Tcp, Vec<u8>, Clock) -> F + Send + 'static,
    ) -> Arc<Mutex<Vec<u8>>> {
        let listen = tcp::Listen {
            local: self.remote(),
            backlog: 4,
            options: super::OPTIONS,
        };
        let mut listener = self.server.net().listen(&listen).expect("the port is free");
        let clock = self.server.clock();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let slot = Arc::clone(&seen);
        let handle = self
            .server
            .shards()
            .start(shard("server"), move |_| async move {
                let mut stream = poll_fn(|cx| listener.poll_accept(cx))
                    .await
                    .expect("a stream comes");
                let request = request(&mut stream).await.expect("the read works");
                slot.lock()
                    .expect("no panic under the lock")
                    .clone_from(&request);
                let stream = answer(stream, request, clock.clone()).await;
                clock.sleep(Span::MINUTE).await;
                drop((stream, listener));
            });
        self.handles.push(handle.expect("the shard starts"));
        seen
    }

    /// Sends `request` from the client node and gives the outcome.
    fn send(&mut self, request: Request<Bytes>) -> Result<Response<Bytes>, Error> {
        let mut outcomes = self.run(vec![Step::Send(request)]);
        outcomes.pop().expect("one send gives one outcome")
    }

    /// Runs `steps` with one client on the client node, then drops the client, and
    /// gives the outcome of each send. The client shard lives on for a minute after
    /// the drop, so the connection tasks end on their own.
    fn run(&mut self, steps: Vec<Step>) -> Vec<Result<Response<Bytes>, Error>> {
        let (net, clock) = (self.client.net(), self.client.clock());
        let waits = steps
            .iter()
            .map(|step| match step {
                Step::Wait(span) => *span,
                Step::Send(_) => TIMEOUT,
            })
            .fold(Span::MINUTE, |total, span| {
                Span::from_nanos(total.nanos() + span.nanos())
            });
        let out = Arc::new(Mutex::new(Vec::new()));
        let slot = Arc::clone(&out);
        let elapsed = Arc::new(Mutex::new(None));
        let last = Arc::clone(&elapsed);
        let handle =
            self.client
                .shards()
                .start(shard("client"), move |tasks| async move {
                    let client = Client::new(Config {
                        net,
                        clock: clock.clone(),
                        tasks,
                        timeout: TIMEOUT,
                        body_max: BODY_MAX,
                    });
                    for step in steps {
                        match step {
                            Step::Wait(span) => clock.sleep(span).await,
                            Step::Send(request) => {
                                let start = clock.now();
                                let outcome = client.send(request).await;
                                *last.lock().expect("no panic under the lock") =
                                    Some(Span::from_nanos(
                                        i64::try_from(clock.now().0 - start.0)
                                            .expect("a short run"),
                                    ));
                                slot.lock()
                                    .expect("no panic under the lock")
                                    .push(outcome);
                            }
                        }
                    }
                    drop(client);
                    clock.sleep(Span::MINUTE).await;
                });
        self.handles.push(handle.expect("the shard starts"));
        self.sim.run_for(waits).expect("the run ends");
        self.elapsed = *elapsed.lock().expect("no panic under the lock");
        std::mem::take(&mut *out.lock().expect("no panic under the lock"))
    }
}

/// What the client does next in [`Network::run`].
#[expect(clippy::large_enum_variant, reason = "a test makes a few")]
enum Step {
    Send(Request<Bytes>),
    Wait(Span),
}

/// Reads one request: the head, then a body of its `content-length`. Gives fewer
/// bytes when the stream ends first.
async fn request(stream: &mut Tcp) -> Result<Vec<u8>, net::Error> {
    let mut bytes = Vec::new();
    loop {
        let text = String::from_utf8_lossy(&bytes);
        if let Some(end) = text.find("\r\n\r\n") {
            let length = text[..end]
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .map_or(0, |n| n.parse::<usize>().expect("a length"));
            if bytes.len() >= end + 4 + length {
                return Ok(bytes);
            }
        }
        let mut buffer = [0; 512];
        let n = poll_fn(|cx| stream.poll_read(cx, &mut buffer)).await?;
        if n == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&buffer[..n]);
    }
}

async fn write(stream: &mut Tcp, bytes: &[u8]) {
    let mut sent = 0;
    while sent < bytes.len() {
        let parts = [IoSlice::new(&bytes[sent..])];
        sent += poll_fn(|cx| stream.poll_write(cx, &parts))
            .await
            .expect("the write works");
    }
}

/// What a server does with a stream.
type Answer = std::pin::Pin<Box<dyn Future<Output = Option<Tcp>>>>;

/// Answers with `response`, as given.
fn reply(
    response: &'static str,
) -> impl FnOnce(Tcp, Vec<u8>, Clock) -> Answer + Send + 'static {
    move |mut stream, _, _| {
        Box::pin(async move {
            write(&mut stream, response.as_bytes()).await;
            Some(stream)
        })
    }
}

fn get(url: &str) -> Request<Bytes> {
    Request::get(url)
        .body(Bytes::new())
        .expect("a valid request")
}

/// The messages of the error's sources, outermost first.
fn chain(error: &Error) -> String {
    let mut messages = Vec::new();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        messages.push(cause.to_string());
        source = cause.source();
    }
    messages.join(" <- ")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("text")
}

#[test]
fn sends_the_path_and_host_and_reads_a_length_body() {
    let mut network = Network::new(1);
    let seen = network.serve(reply("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok"));
    let url = network.url("/ping?wait=1");
    let response = network.send(get(&url)).expect("the server answers");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.body().as_ref(), b"ok");
    let host = network.remote();
    assert_eq!(
        text(&seen.lock().expect("no panic under the lock")),
        format!("GET /ping?wait=1 HTTP/1.1\r\nhost: {host}\r\n\r\n"),
    );
}

#[test]
fn sends_a_slash_when_the_built_uri_has_an_empty_path() {
    for (path, target) in [("", "/"), ("?db=a", "/?db=a")] {
        let mut network = Network::new(16);
        let seen =
            network.serve(reply("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok"));
        let host = network.remote();
        let uri = http::Uri::builder()
            .scheme("http")
            .authority(host.to_string())
            .path_and_query(path)
            .build()
            .expect("a valid URI");
        let request = Request::get(uri)
            .body(Bytes::new())
            .expect("a valid request");
        network.send(request).expect("the server answers");
        assert_eq!(
            text(&seen.lock().expect("no panic under the lock")),
            format!("GET {target} HTTP/1.1\r\nhost: {host}\r\n\r\n"),
        );
    }
}

#[test]
fn sends_a_body_with_its_length() {
    let mut network = Network::new(2);
    let seen = network.serve(reply("HTTP/1.1 204 No Content\r\n\r\n"));
    let request = Request::post(network.url("/api/v2/write"))
        .header("host", "influx")
        .body(Bytes::from_static(b"m v=1 5"))
        .expect("a valid request");
    let response = network.send(request).expect("the server answers");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        text(&seen.lock().expect("no panic under the lock")),
        "POST /api/v2/write HTTP/1.1\r\nhost: influx\r\ncontent-length: 7\r\n\r\n\
         m v=1 5",
    );
}

#[test]
fn reads_a_chunked_body() {
    let mut network = Network::new(3);
    network.serve(reply(
        "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n\
         3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
    ));
    let response = network
        .send(get(&network.url("/")))
        .expect("the server answers");
    assert_eq!(response.body().as_ref(), b"abcde");
}

#[test]
fn reads_a_body_of_exactly_the_cap() {
    let mut network = Network::new(4);
    let body = "x".repeat(BODY_MAX);
    let response: &'static str =
        format!("HTTP/1.1 200 OK\r\ncontent-length: {BODY_MAX}\r\n\r\n{body}").leak();
    network.serve(reply(response));
    let response = network
        .send(get(&network.url("/")))
        .expect("the server answers");
    assert_eq!(response.body().len(), BODY_MAX);
}

#[test]
fn refuses_a_body_over_the_cap() {
    let mut network = Network::new(5);
    let body = "x".repeat(BODY_MAX + 1);
    let response: &'static str = format!(
        "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n\
         {:x}\r\n{body}\r\n0\r\n\r\n",
        BODY_MAX + 1
    )
    .leak();
    network.serve(reply(response));
    let error = network
        .send(get(&network.url("/")))
        .expect_err("over the cap");
    assert!(
        matches!(error, Error::TooLarge { max: BODY_MAX }),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "the response body is larger than 64 bytes"
    );
}

#[test]
fn refuses_chunks_that_together_pass_the_cap() {
    let mut network = Network::new(13);
    let chunk = "x".repeat(40);
    let response: &'static str = format!(
        "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n\
         28\r\n{chunk}\r\n28\r\n{chunk}\r\n0\r\n\r\n"
    )
    .leak();
    network.serve(reply(response));
    let error = network
        .send(get(&network.url("/")))
        .expect_err("over the cap");
    assert!(
        matches!(error, Error::TooLarge { max: BODY_MAX }),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "the response body is larger than 64 bytes"
    );
}

/// Answers with `response`, then reads until the client ends the stream, and gives
/// what that read saw.
fn reply_then_read(
    response: &'static [u8],
    end: &Arc<Mutex<Option<String>>>,
) -> impl FnOnce(Tcp, Vec<u8>, Clock) -> Answer + Send + 'static {
    let slot = Arc::clone(end);
    move |mut stream, _, _| {
        Box::pin(async move {
            write(&mut stream, response).await;
            let mut buffer = [0; 16];
            let read = poll_fn(|cx| stream.poll_read(cx, &mut buffer)).await;
            *slot.lock().expect("no panic under the lock") = Some(format!("{read:?}"));
            Some(stream)
        })
    }
}

#[test]
fn closes_the_stream_when_the_client_drops() {
    let mut network = Network::new(14);
    let end = Arc::new(Mutex::new(None));
    network.serve(reply_then_read(
        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok",
        &end,
    ));
    network
        .send(get(&network.url("/")))
        .expect("the server answers");
    let end = end.lock().expect("no panic under the lock").take();
    assert_eq!(end.as_deref(), Some("Ok(0)"));
}

#[test]
fn closes_the_stream_after_a_timeout() {
    let mut network = Network::new(15);
    let end = Arc::new(Mutex::new(None));
    network.serve(reply_then_read(b"", &end));
    let error = network.send(get(&network.url("/"))).expect_err("no answer");
    assert!(matches!(error, Error::TimedOut), "{error:?}");
    assert_eq!(error.to_string(), "the exchange timed out");
    let end = end.lock().expect("no panic under the lock").take();
    assert_eq!(end.as_deref(), Some("Ok(0)"));
}

#[test]
fn times_out_when_the_server_never_answers() {
    let mut network = Network::new(6);
    network.serve(|stream, _, clock| {
        Box::pin(async move {
            clock.sleep(Span::MINUTE).await;
            Some(stream)
        })
    });
    let error = network.send(get(&network.url("/"))).expect_err("no answer");
    assert!(matches!(error, Error::TimedOut), "{error:?}");
    assert_eq!(error.to_string(), "the exchange timed out");
    assert_eq!(network.elapsed, Some(TIMEOUT));
}

#[test]
fn fails_when_the_server_closes_before_the_response() {
    let mut network = Network::new(7);
    network.serve(|mut stream, _, _| {
        Box::pin(async move {
            write(
                &mut stream,
                b"HTTP/1.1 200 OK\r\ncontent-length: 9\r\n\r\nabc",
            )
            .await;
            poll_fn(|cx| stream.poll_close(cx))
                .await
                .expect("the close works");
            Some(stream)
        })
    });
    let error = network
        .send(get(&network.url("/")))
        .expect_err("an early close");
    assert!(matches!(error, Error::Protocol(_)), "{error:?}");
    assert_eq!(
        error.to_string(),
        "the exchange failed: error reading a body from connection",
    );
}

#[test]
fn fails_when_the_server_breaks_http() {
    let mut network = Network::new(8);
    network.serve(reply("HTTP/1.1 abc\r\n\r\n"));
    let error = network
        .send(get(&network.url("/")))
        .expect_err("a bad status");
    assert!(matches!(error, Error::Protocol(_)), "{error:?}");
    assert_eq!(
        error.to_string(),
        "the exchange failed: invalid HTTP status-code parsed"
    );
}

#[test]
fn gives_the_stream_error_when_the_server_drops_the_stream() {
    let mut network = Network::new(11);
    network.serve(|stream, _, _| {
        Box::pin(async move {
            drop(stream);
            None
        })
    });
    let remote = network.remote();
    let error = network
        .send(get(&network.url("/")))
        .expect_err("a dropped stream");
    assert!(matches!(error, Error::Protocol(_)), "{error:?}");
    assert_eq!(error.to_string(), "the exchange failed: connection error");
    assert_eq!(
        chain(&error),
        format!("connection error <- {remote} reset the stream")
    );
}

#[test]
fn gives_the_connect_error_when_nothing_listens() {
    let mut network = Network::new(9);
    let remote = network.remote();
    let error = network
        .send(get(&network.url("/")))
        .expect_err("no listener");
    assert!(
        matches!(
            error,
            Error::Connect(net::Error::Refused { remote: r }) if r == remote
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!("the connect failed: {remote} refused the connection")
    );
}

/// The error of a send to `uri`.
fn refused(uri: &str) -> Error {
    Network::new(10).send(get(uri)).expect_err("a refused URI")
}

#[test]
fn refuses_a_scheme_other_than_http() {
    for uri in ["https://10.0.0.2/", "/write", "https://admin:secret@[]:0/"] {
        let error = refused(uri);
        assert!(matches!(error, Error::Scheme), "{uri}: {error:?}");
        assert_eq!(error.to_string(), "the scheme of the URI is not http");
    }
}

#[test]
fn refuses_user_info_and_keeps_none_of_it() {
    for uri in [
        "http://admin:hunter2@10.0.0.2:8086/",
        "http://admin:hunter2@[influx]:99999/",
    ] {
        let error = refused(uri);
        assert!(matches!(error, Error::UserInfo), "{uri}: {error:?}");
        let message = error.to_string();
        assert_eq!(
            message,
            "the URI holds user info; give a credential through a secret"
        );
        let shown = format!("{message} {error:?}");
        assert!(
            !shown.contains("admin") && !shown.contains("hunter2"),
            "{shown}"
        );
    }
}

#[test]
fn refuses_a_host_that_is_not_valid() {
    for (uri, host) in [
        ("http://:8086/", ""),
        ("http://[]/", "[]"),
        ("http://[influx]/", "[influx]"),
        ("http://[influx]:99999/", "[influx]"),
        ("http://[fd00::2]x/", "[fd00::2]x"),
        ("http://[fd00::2]8086/", "[fd00::2]8086"),
        ("http://[fd00::2]x:80/", "[fd00::2]x"),
        ("http://a[::1]/", "a["),
        ("http://a[::1]:80/", "a["),
    ] {
        let error = refused(uri);
        assert!(
            matches!(&error, Error::Host { host: h } if h == host),
            "{uri}: {error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!("the URI has no valid host: \"{host}\"")
        );
    }
}

#[test]
fn refuses_a_port_that_is_not_a_u16() {
    for (authority, port) in [
        ("10.0.0.2:99999", "99999"),
        ("10.0.0.2:65536", "65536"),
        ("10.0.0.2:8086x", "8086x"),
        ("10.0.0.2:-1", "-1"),
        ("influx:99999999", "99999999"),
        ("influx:+80", "+80"),
        ("influx:0", "0"),
        ("[fd00::2]:80x", "80x"),
        ("[::]:80x", "80x"),
    ] {
        let error = refused(&format!("http://{authority}/"));
        assert!(
            matches!(&error, Error::Port { port: p } if p == port),
            "{authority}: {error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!("the port \"{port}\" of the URI is not a number from 1 to 65535")
        );
    }
}

// A stream that is not vectored sends the same bytes, so `Client::send` cannot show
// it, and this test builds the private `Stream`.
#[test]
fn stream_is_vectored_and_writes_a_whole_plain_write() {
    const HEAD: &[u8] = b"GET / HTTP/1.1\r\nhost: a\r\n\r\n";
    let mut network = Network::new(14);
    let seen = network.serve(|stream, _, _| async { Some(stream) });
    let config = tcp::Config {
        remote: network.remote(),
        options: super::OPTIONS,
    };
    let (net, clock) = (network.client.net(), network.client.clock());
    let written = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&written);
    let handle = network
        .client
        .shards()
        .start(shard("client"), move |_| async move {
            let tcp = net.connect(&config).await.expect("the server listens");
            let mut stream = super::Stream {
                tcp,
                received: std::rc::Rc::default(),
            };
            assert!(
                hyper::rt::Write::is_write_vectored(&stream),
                "hyper copies each body into its buffer unless the stream is vectored"
            );
            let n = poll_fn(|cx| {
                hyper::rt::Write::poll_write(std::pin::Pin::new(&mut stream), cx, HEAD)
            })
            .await
            .expect("the write works");
            *slot.lock().expect("no panic under the lock") = Some(n);
            clock.sleep(Span::MINUTE).await;
            drop(stream);
        });
    network.handles.push(handle.expect("the shard starts"));
    network.sim.run_for(Span::MINUTE).expect("the run ends");
    assert_eq!(*written.lock().expect("no panic"), Some(HEAD.len()));
    assert_eq!(text(&seen.lock().expect("no panic")), text(HEAD));
}

mod common;
mod name;
mod pool;
