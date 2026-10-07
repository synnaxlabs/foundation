use std::future::poll_fn;
use std::io::IoSlice;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use ::sim::{Sim, node};
use bytes::Bytes;
use env::net::{self, tcp};
use env::thread::Handle;
use http::{Method, Request, Response, StatusCode};
use types::time::Span;

use super::serve;
use crate::http::{Client, Config};

const PORT: u16 = 8086;
const TIMEOUT: Span = Span::from_nanos(10_000_000_000);

fn shard(name: &str) -> env::shards::Config {
    env::shards::Config {
        name: name.into(),
        core: Some(0),
    }
}

/// How a stream ended for the client, with the bytes it read.
#[derive(Debug, PartialEq)]
enum End {
    /// The server closed it.
    Closed(String),
    /// The read failed with this error.
    Failed(String),
}

/// A server node that runs `serve` with an answer that echoes each request, and a
/// client node.
struct Network {
    sim: Sim,
    client: node::Node,
    server: node::Node,
    remote: SocketAddr,
    /// The error that `serve` gave, once it ends.
    failed: Arc<Mutex<Option<net::Error>>>,
    /// Each request that reached the answer, as the answer echoes it.
    seen: Arc<Mutex<Vec<String>>>,
    handles: Vec<Handle>,
}

/// Echoes `request` as its method, URI, version, and body, and sends back the value
/// of its `x-key` header.
fn echo(request: &Request<Bytes>) -> Response<Bytes> {
    let text = format!(
        "{} {} {:?} {}",
        request.method(),
        request.uri(),
        request.version(),
        String::from_utf8_lossy(request.body()),
    );
    let mut response = Response::new(Bytes::from(text));
    if let Some(key) = request.headers().get("x-key") {
        response.headers_mut().insert("x-key", key.clone());
    }
    response
}

impl Network {
    fn new() -> Self {
        let mut sim = Sim::new(::sim::Config {
            seed: 7,
            steps_max: 10_000_000,
            ..::sim::Config::default()
        });
        let client = sim.node(node::Config::default());
        let server = sim.node(node::Config::default());
        let remote = SocketAddr::new(server.addresses()[0], PORT);
        let listen = tcp::Listen {
            local: remote,
            backlog: 4,
            options: super::super::OPTIONS,
        };
        let listener = server.net().listen(&listen).expect("the port is free");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let slot = Arc::clone(&seen);
        let answer = move |request: Request<Bytes>| {
            let response = echo(&request);
            let text = String::from_utf8(response.body().to_vec()).expect("UTF-8");
            slot.lock().expect("no panic under the lock").push(text);
            response
        };
        let failed = Arc::new(Mutex::new(None));
        let ended = Arc::clone(&failed);
        let handle = server
            .shards()
            .start(shard("server"), move |tasks| async move {
                let error = serve(listener, tasks, answer).await;
                *ended.lock().expect("no panic under the lock") = Some(error);
                // The streams it accepted end when this shard ends.
                std::future::pending::<()>().await;
            })
            .expect("the shard starts");
        Self {
            sim,
            client,
            server,
            remote,
            failed,
            seen,
            handles: vec![handle],
        }
    }

    /// Sends `requests` in order on one client, and gives the status, `x-key`
    /// header, and body of each response.
    fn send(
        &mut self,
        requests: Vec<Request<Bytes>>,
    ) -> Vec<(StatusCode, String, String)> {
        let (net, clock) = (self.client.net(), self.client.clock());
        let out = Arc::new(Mutex::new(Vec::new()));
        let slot = Arc::clone(&out);
        let handle = self
            .client
            .shards()
            .start(shard("client"), move |tasks| async move {
                let client = Client::new(Config {
                    net,
                    clock,
                    tasks,
                    timeout: TIMEOUT,
                    body_max: 1024,
                });
                for request in requests {
                    let response = client.send(request).await.expect("an answer");
                    let key = response
                        .headers()
                        .get("x-key")
                        .map_or("", |key| key.to_str().expect("a visible value"));
                    let text =
                        String::from_utf8(response.body().to_vec()).expect("UTF-8");
                    slot.lock().expect("no panic under the lock").push((
                        response.status(),
                        key.to_owned(),
                        text,
                    ));
                }
            })
            .expect("the shard starts");
        self.handles.push(handle);
        self.sim.run_for(Span::MINUTE).expect("the run ends");
        std::mem::take(&mut *out.lock().expect("no panic under the lock"))
    }

    /// Writes `request` on a new stream, closes the write side when `half_closed`,
    /// and reads until the server ends the stream.
    fn exchange(&mut self, request: &'static [u8], half_closed: bool) -> End {
        let (net, remote) = (self.client.net(), self.remote);
        let out = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&out);
        let handle = self
            .client
            .shards()
            .start(shard("client"), move |_| async move {
                let config = tcp::Config {
                    remote,
                    options: super::super::OPTIONS,
                };
                let mut tcp = net.connect(&config).await.expect("the server listens");
                let mut rest = request;
                while !rest.is_empty() {
                    let n = poll_fn(|cx| tcp.poll_write(cx, &[IoSlice::new(rest)]))
                        .await
                        .expect("the write works");
                    rest = rest.get(n..).expect("a write fits its buffer");
                }
                if half_closed {
                    poll_fn(|cx| tcp.poll_close(cx))
                        .await
                        .expect("the close works");
                }
                let mut read = Vec::new();
                let mut bytes = [0; 1024];
                let ended = loop {
                    match poll_fn(|cx| tcp.poll_read(cx, &mut bytes)).await {
                        Ok(0) => {
                            break End::Closed(String::from_utf8(read).expect("UTF-8"));
                        }
                        Ok(n) => read.extend_from_slice(bytes.get(..n).expect("fits")),
                        Err(error) => break End::Failed(error.to_string()),
                    }
                };
                *slot.lock().expect("no panic under the lock") = Some(ended);
            })
            .expect("the shard starts");
        self.handles.push(handle);
        self.sim.run_for(Span::MINUTE).expect("the run ends");
        out.lock()
            .expect("no panic under the lock")
            .take()
            .expect("the exchange ends")
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().expect("no panic under the lock").clone()
    }

    fn request(&self, method: Method, path: &str, body: &str) -> Request<Bytes> {
        Request::builder()
            .method(method)
            .uri(format!("http://{}{path}", self.remote))
            .header("x-key", path)
            .body(Bytes::from(body.to_owned()))
            .expect("a valid request")
    }
}

#[test]
fn answers_each_request_of_a_client_with_its_whole_body() {
    let mut network = Network::new();
    let requests = vec![
        network.request(Method::POST, "/write?db=a", "m v=1 1"),
        network.request(Method::GET, "/ping", ""),
    ];
    assert_eq!(
        network.send(requests),
        [
            (
                StatusCode::OK,
                "/write?db=a".into(),
                "POST /write?db=a HTTP/1.1 m v=1 1".into()
            ),
            (StatusCode::OK, "/ping".into(), "GET /ping HTTP/1.1 ".into()),
        ]
    );
}

#[test]
fn answers_and_ends_the_stream_on_connection_close() {
    const REQUEST: &[u8] = b"POST /a HTTP/1.1\r\nhost: h\r\n\
        content-length: 3\r\nconnection: close\r\n\r\nabc";
    let mut network = Network::new();
    assert_eq!(
        network.exchange(REQUEST, false),
        End::Closed(
            "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 20\r\n\r\n\
            POST /a HTTP/1.1 abc"
                .into()
        )
    );
}

#[test]
fn answers_a_client_that_closes_its_write_side_after_the_request() {
    const REQUEST: &[u8] = b"POST /a HTTP/1.1\r\ncontent-length: 3\r\n\r\nabc";
    let mut network = Network::new();
    assert_eq!(
        network.exchange(REQUEST, true),
        End::Closed(
            "HTTP/1.1 200 OK\r\ncontent-length: 20\r\n\r\nPOST /a HTTP/1.1 abc".into()
        )
    );
}

#[test]
fn answers_each_of_two_requests_in_one_write_in_order() {
    const REQUEST: &[u8] = b"POST /a HTTP/1.1\r\ncontent-length: 1\r\n\r\n1\
        POST /b HTTP/1.1\r\ncontent-length: 1\r\nconnection: Close\r\n\r\n2";
    let mut network = Network::new();
    assert_eq!(
        network.exchange(REQUEST, false),
        End::Closed(
            "HTTP/1.1 200 OK\r\ncontent-length: 18\r\n\r\nPOST /a HTTP/1.1 1\
            HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 18\r\n\r\n\
            POST /b HTTP/1.1 2"
                .into()
        )
    );
}

#[test]
fn answers_an_http_1_0_request_and_ends_the_stream() {
    const REQUEST: &[u8] = b"GET /a HTTP/1.0\r\n\r\n";
    let mut network = Network::new();
    assert_eq!(
        network.exchange(REQUEST, false),
        End::Closed(
            "HTTP/1.0 200 OK\r\ncontent-length: 16\r\n\r\nGET /a HTTP/1.0 ".into()
        )
    );
}

#[test]
fn answers_a_body_that_ends_early_with_400_and_nothing_reaches_answer() {
    const REQUEST: &[u8] = b"POST /a HTTP/1.1\r\ncontent-length: 30\r\n\r\nabc";
    let mut network = Network::new();
    assert_eq!(network.exchange(REQUEST, true), End::Closed(REFUSED.into()));
    assert_eq!(network.seen(), [""; 0]);
}

#[test]
fn resets_a_stream_whose_head_ends_early_and_answers_nothing() {
    const REQUEST: &[u8] = b"POST /a HTTP/1.1\r\ncontent-len";
    let mut network = Network::new();
    assert_eq!(
        network.exchange(REQUEST, true),
        End::Failed("10.0.0.2:8086 reset the stream".into())
    );
    assert_eq!(network.seen(), [""; 0]);
}

const REFUSED: &str = "HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\
    content-length: 0\r\n\r\n";

#[test]
fn answers_a_request_that_breaks_http_with_400_and_ends_the_stream() {
    const REQUEST: &[u8] = b"POST /a HTTP/1.1\r\nbad header\r\n\r\n\
        GET /b HTTP/1.1\r\n\r\n";
    let mut network = Network::new();
    assert_eq!(
        network.exchange(REQUEST, false),
        End::Closed(REFUSED.into())
    );
    assert_eq!(network.seen(), [""; 0]);
}

#[test]
fn answers_a_content_length_that_is_not_one_number_with_400() {
    for request in [
        &b"POST /a HTTP/1.1\r\ncontent-length: +3\r\n\r\nabc"[..],
        b"POST /a HTTP/1.1\r\ncontent-length: 3\r\ncontent-length: 4\r\n\r\nabcd",
    ] {
        let mut network = Network::new();
        assert_eq!(
            network.exchange(request, false),
            End::Closed(REFUSED.into())
        );
        assert_eq!(network.seen(), [""; 0]);
    }
}

#[test]
fn answers_a_chunked_body_that_breaks_http_with_400_and_ends_the_stream() {
    for request in [
        &b"POST /a HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\nzz\r\nabc\r\n0\r\n\r\n"[..],
        b"POST /a HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabcXX0\r\n\r\n",
    ] {
        let mut network = Network::new();
        assert_eq!(
            network.exchange(request, false),
            End::Closed(REFUSED.into())
        );
        assert_eq!(network.seen(), [""; 0]);
    }
}

#[test]
fn answers_a_chunked_body_whole() {
    const REQUEST: &[u8] = b"POST /a HTTP/1.1\r\ntransfer-encoding: chunked\r\n\r\n\
        3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
    let mut network = Network::new();
    assert_eq!(
        network.exchange(REQUEST, true),
        End::Closed(
            "HTTP/1.1 200 OK\r\ncontent-length: 22\r\n\r\nPOST /a HTTP/1.1 abcde"
                .into()
        )
    );
    assert_eq!(network.seen(), ["POST /a HTTP/1.1 abcde"]);
}

#[test]
fn answers_a_head_with_too_many_headers_with_431_and_ends_the_stream() {
    let mut request = b"GET /a HTTP/1.1\r\n".to_vec();
    for _ in 0..101 {
        request.extend_from_slice(b"x: 1\r\n");
    }
    request.extend_from_slice(b"\r\n");
    let mut network = Network::new();
    assert_eq!(
        network.exchange(request.leak(), false),
        End::Closed(
            "HTTP/1.1 431 Request Header Fields Too Large\r\nconnection: close\r\n\
             content-length: 0\r\n\r\n"
                .into()
        )
    );
    assert_eq!(network.seen(), [""; 0]);
}

#[test]
fn answers_a_uri_that_is_too_long_with_414_and_ends_the_stream() {
    let request = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(70_000));
    let mut network = Network::new();
    assert_eq!(
        network.exchange(request.into_bytes().leak(), false),
        End::Closed(
            "HTTP/1.1 414 URI Too Long\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
                .into()
        )
    );
    assert_eq!(network.seen(), [""; 0]);
}

#[test]
fn ends_the_stream_of_an_http2_preface_with_no_answer() {
    const REQUEST: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
    let mut network = Network::new();
    assert_eq!(
        network.exchange(REQUEST, false),
        End::Failed("10.0.0.2:8086 reset the stream".into())
    );
    assert_eq!(network.seen(), [""; 0]);
}

#[test]
fn gives_the_error_of_a_failed_accept_and_serves_the_streams_it_accepted() {
    let mut network = Network::new();
    let (net, clock, remote) =
        (network.client.net(), network.client.clock(), network.remote);
    let out = Arc::new(Mutex::new(Vec::new()));
    let slot = Arc::clone(&out);
    let handle = network
        .client
        .shards()
        .start(shard("client"), move |tasks| async move {
            let client = Client::new(Config {
                net: net.clone(),
                clock: clock.clone(),
                tasks,
                timeout: TIMEOUT,
                body_max: 1024,
            });
            for path in ["/before", "/after"] {
                let request = Request::post(format!("http://{remote}{path}"))
                    .body(Bytes::new())
                    .expect("a valid request");
                let response = client.send(request).await.expect("an answer");
                slot.lock()
                    .expect("no panic under the lock")
                    .push(response.status());
                clock.sleep(Span::SECOND).await;
            }
            let config = tcp::Config {
                remote,
                options: super::super::OPTIONS,
            };
            let refused = net.connect(&config).await.map(drop);
            assert_eq!(refused, Err(net::Error::Refused { remote }));
        })
        .expect("the shard starts");
    network.handles.push(handle);
    network
        .sim
        .run_for(Span::from_nanos(500_000_000))
        .expect("the run goes on");
    network.server.fail_listener(remote);
    network.sim.run_for(Span::MINUTE).expect("the run ends");
    let failed = network
        .failed
        .lock()
        .expect("no panic under the lock")
        .take();
    assert_eq!(failed, Some(net::Error::Io { code: 5 }));
    assert_eq!(
        *out.lock().expect("no panic under the lock"),
        [StatusCode::OK; 2]
    );
    assert_eq!(
        network.seen(),
        ["POST /before HTTP/1.1 ", "POST /after HTTP/1.1 "]
    );
}
