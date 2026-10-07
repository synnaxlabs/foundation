use std::future::poll_fn;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use connector::http::{Client, Config};
use env::net::tcp;
use env::thread::Handle;
use http::{Method, Request, StatusCode};
use hyper::rt::Write as _;
use sim::{Sim, node};
use types::time::Span;

use super::{Stream, serve};
use crate::sim::Store;

const PORT: u16 = 8086;
const TIMEOUT: Span = Span::from_nanos(10_000_000_000);
const OPTIONS: tcp::Options = tcp::Options {
    send_buffer_bytes: 1 << 16,
    recv_buffer_bytes: 1 << 16,
    unsent_bytes_max: 1 << 14,
    delayed: false,
};

fn shard(name: &str) -> env::shards::Config {
    env::shards::Config {
        name: name.into(),
        core: Some(0),
    }
}

/// A server node that serves a store, and a client node.
struct Network {
    sim: Sim,
    client: node::Node,
    remote: SocketAddr,
    store: Arc<Mutex<Store>>,
    handles: Vec<Handle>,
}

impl Network {
    fn new() -> Self {
        let mut sim = Sim::new(sim::Config {
            seed: 7,
            steps_max: 10_000_000,
            ..sim::Config::default()
        });
        let client = sim.node(node::Config::default());
        let server = sim.node(node::Config::default());
        let remote = SocketAddr::new(server.addresses()[0], PORT);
        let listen = tcp::Listen {
            local: remote,
            backlog: 4,
            options: OPTIONS,
        };
        let listener = server.net().listen(&listen).expect("the port is free");
        let store = Arc::new(Mutex::new(Store::default()));
        let held = Arc::clone(&store);
        let handle = server
            .shards()
            .start(shard("server"), move |tasks| serve(listener, tasks, held))
            .expect("the shard starts");
        Self {
            sim,
            client,
            remote,
            store,
            handles: vec![handle],
        }
    }

    /// Sends each request of each list in order, one client for each list, and gives
    /// the status and body of each response. The clients live until the last
    /// response, so each keeps its stream open.
    fn send(&mut self, clients: Vec<Vec<Request<Bytes>>>) -> Vec<(StatusCode, String)> {
        let (net, clock) = (self.client.net(), self.client.clock());
        let out = Arc::new(Mutex::new(Vec::new()));
        let slot = Arc::clone(&out);
        let handle = self
            .client
            .shards()
            .start(shard("client"), move |tasks| async move {
                let mut held = Vec::new();
                for requests in clients {
                    let client = Client::new(Config {
                        net: net.clone(),
                        clock: clock.clone(),
                        tasks: tasks.clone(),
                        timeout: TIMEOUT,
                        body_max: 1024,
                    });
                    for request in requests {
                        let response = client.send(request).await.expect("an answer");
                        let text = String::from_utf8(response.body().to_vec())
                            .expect("a UTF-8 body");
                        slot.lock()
                            .expect("no panic under the lock")
                            .push((response.status(), text));
                    }
                    held.push(client);
                }
            })
            .expect("the shard starts");
        self.handles.push(handle);
        self.sim.run_for(Span::MINUTE).expect("the run ends");
        std::mem::take(&mut *out.lock().expect("no panic under the lock"))
    }

    /// Writes `request` in one plain write of the adapter, and reads until the server
    /// ends the stream. Gives the bytes written and the bytes read, or the read error.
    fn exchange(&mut self, request: &'static [u8]) -> (usize, Result<String, String>) {
        let (net, remote) = (self.client.net(), self.remote);
        let out = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&out);
        let handle = self
            .client
            .shards()
            .start(shard("client"), move |_| async move {
                let config = tcp::Config {
                    remote,
                    options: OPTIONS,
                };
                let tcp = net.connect(&config).await.expect("the server listens");
                let mut stream = Stream(tcp);
                assert!(
                    stream.is_write_vectored(),
                    "hyper copies each body into its buffer unless the stream is vectored"
                );
                let written = poll_fn(|cx| Pin::new(&mut stream).poll_write(cx, request))
                    .await
                    .expect("the write works");
                let mut read = Vec::new();
                let mut bytes = [0; 1024];
                let ended = loop {
                    match poll_fn(|cx| stream.0.poll_read(cx, &mut bytes)).await {
                        Ok(0) => break Ok(String::from_utf8(read).expect("UTF-8")),
                        Ok(n) => read.extend_from_slice(bytes.get(..n).expect("fits")),
                        Err(error) => break Err(error.to_string()),
                    }
                };
                *slot.lock().expect("no panic under the lock") = Some((written, ended));
            })
            .expect("the shard starts");
        self.handles.push(handle);
        self.sim.run_for(Span::MINUTE).expect("the run ends");
        out.lock()
            .expect("no panic under the lock")
            .take()
            .expect("the exchange ends")
    }

    fn request(&self, method: Method, path: &str, body: &str) -> Request<Bytes> {
        Request::builder()
            .method(method)
            .uri(format!("http://{}{path}", self.remote))
            .body(Bytes::from(body.to_owned()))
            .expect("a valid request")
    }

    fn post(&self, path: &str, body: &str) -> Request<Bytes> {
        self.request(Method::POST, path, body)
    }

    fn times(&self, measurement: &str) -> Vec<i64> {
        self.store
            .lock()
            .expect("no panic under the lock")
            .points(measurement, &[])
            .map(|point| point.time.nanos())
            .collect()
    }
}

fn answer(status: StatusCode, text: &str) -> (StatusCode, String) {
    (status, text.into())
}

#[test]
fn stores_a_body_on_each_write_path() {
    let mut network = Network::new();
    let requests = vec![
        network.post("/write?db=edge", "m v=1 1"),
        network.post("/api/v2/write?org=o&bucket=edge&precision=ns", "m v=2 2"),
    ];
    assert_eq!(
        network.send(vec![requests]),
        [
            answer(StatusCode::NO_CONTENT, ""),
            answer(StatusCode::NO_CONTENT, ""),
        ]
    );
    assert_eq!(network.times("m"), [1, 2]);
}

#[test]
fn refuses_a_line_with_the_store_error_and_keeps_the_valid_lines() {
    let mut network = Network::new();
    let request = network.post("/write?db=edge", "m v=1 1\nm v=1");
    assert_eq!(
        network.send(vec![vec![request]]),
        [answer(
            StatusCode::BAD_REQUEST,
            "the line \"m v=1\" has no time"
        )]
    );
    assert_eq!(network.times("m"), [1]);
}

#[test]
fn refuses_a_request_with_no_database_and_stores_nothing() {
    let mut network = Network::new();
    let requests = vec![
        network.post("/write", "m v=1 1"),
        network.post("/write?db=", "m v=1 1"),
        network.post("/write?bucket=edge", "m v=1 1"),
        network.post("/api/v2/write?org=o", "m v=1 1"),
        network.post("/api/v2/write?db=edge", "m v=1 1"),
    ];
    let db = answer(StatusCode::BAD_REQUEST, "no db in the query");
    let bucket = answer(StatusCode::BAD_REQUEST, "no bucket in the query");
    assert_eq!(
        network.send(vec![requests]),
        [db.clone(), db.clone(), db, bucket.clone(), bucket]
    );
    assert_eq!(network.times("m"), [0_i64; 0]);
}

#[test]
fn refuses_a_precision_other_than_nanoseconds_and_stores_nothing() {
    let mut network = Network::new();
    let requests = vec![
        network.post("/write?db=edge&precision=ms", "m v=1 1"),
        network.post("/api/v2/write?bucket=edge&precision=n", "m v=1 1"),
    ];
    assert_eq!(
        network.send(vec![requests]),
        [
            answer(
                StatusCode::BAD_REQUEST,
                "the store takes precision ns only, not \"ms\""
            ),
            answer(
                StatusCode::BAD_REQUEST,
                "the store takes precision ns only, not \"n\""
            ),
        ]
    );
    assert_eq!(network.times("m"), [0_i64; 0]);
}

#[test]
fn answers_another_path_with_404_and_another_method_with_405() {
    let mut network = Network::new();
    let requests = vec![
        network.post("/query?db=edge", "m v=1 1"),
        network.post("/write/?db=edge", "m v=1 1"),
        network.request(Method::GET, "/write?db=edge", ""),
        network.request(Method::PUT, "/api/v2/write?bucket=edge", "m v=1 1"),
    ];
    assert_eq!(
        network.send(vec![requests]),
        [
            answer(StatusCode::NOT_FOUND, "no endpoint at /query"),
            answer(StatusCode::NOT_FOUND, "no endpoint at /write/"),
            answer(StatusCode::METHOD_NOT_ALLOWED, "/write takes POST, not GET"),
            answer(
                StatusCode::METHOD_NOT_ALLOWED,
                "/api/v2/write takes POST, not PUT"
            ),
        ]
    );
    assert_eq!(network.times("m"), [0_i64; 0]);
}

#[test]
fn serves_a_second_stream_while_the_first_stays_open() {
    let mut network = Network::new();
    let first = vec![network.post("/write?db=edge", "m v=1 1")];
    let second = vec![network.post("/write?db=edge", "m v=2 2")];
    assert_eq!(
        network.send(vec![first, second]),
        [
            answer(StatusCode::NO_CONTENT, ""),
            answer(StatusCode::NO_CONTENT, ""),
        ]
    );
    assert_eq!(network.times("m"), [1, 2]);
}

#[test]
fn writes_a_whole_plain_write_and_ends_the_stream_on_connection_close() {
    const REQUEST: &[u8] = b"POST /write?db=edge HTTP/1.1\r\nhost: a\r\n\
        content-length: 7\r\nconnection: close\r\n\r\nm v=1 1";
    let mut network = Network::new();
    assert_eq!(
        network.exchange(REQUEST),
        (
            REQUEST.len(),
            Ok("HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n".into())
        )
    );
    assert_eq!(network.times("m"), [1]);
}
