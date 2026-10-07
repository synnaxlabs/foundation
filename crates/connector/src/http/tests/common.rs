//! A server that answers each request, for the tests of sibling modules.

use std::future::poll_fn;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use env::clock::Clock;
use env::net::{Tcp, tcp};
use types::time::{Monotonic, Span};

use super::{Network, request, shard, write};
use crate::http::Error;

pub(super) const OK: &str = "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";

/// What the server does with one request.
#[derive(Clone)]
pub(super) enum Reply {
    /// Writes the bytes and waits for the next request.
    Bytes(String),
    /// Writes the bytes, waits for the span, and closes the stream, as a server
    /// with a keep-alive timeout does.
    Close(String, Span),
    /// Writes nothing and waits for the next request.
    Nothing,
}

/// What a server saw.
#[derive(Default)]
pub(super) struct Log {
    /// The stream of each request, in the order they came, by accept order.
    pub(super) requests: Vec<usize>,
    /// The bytes of each request, in the order they came.
    pub(super) bytes: Vec<Vec<u8>>,
    /// When each stream came and ended, by accept order.
    pub(super) streams: Vec<(Monotonic, Option<Monotonic>)>,
}

impl Network {
    /// Serves each stream on `port` until it ends. `answer` gets the index of each
    /// request, counted over all streams.
    pub(super) fn serve_each(
        &mut self,
        port: u16,
        answer: impl Fn(usize) -> Reply + Send + Sync + 'static,
    ) -> Arc<Mutex<Log>> {
        let listen = tcp::Listen {
            local: self.remote_on(port),
            backlog: 4,
            options: crate::http::OPTIONS,
        };
        let mut listener = self.server.net().listen(&listen).expect("the port is free");
        let clock = self.server.clock();
        let log = Arc::new(Mutex::new(Log::default()));
        let slot = Arc::clone(&log);
        let answer = Arc::new(answer);
        let handle =
            self.server
                .shards()
                .start(shard("server"), move |tasks| async move {
                    loop {
                        let stream = poll_fn(|cx| listener.poll_accept(cx))
                            .await
                            .expect("a stream comes");
                        let index = {
                            let mut log = slot.lock().expect("no panic under the lock");
                            log.streams.push((clock.now(), None));
                            log.streams.len() - 1
                        };
                        let (log, answer, clock) =
                            (Arc::clone(&slot), Arc::clone(&answer), clock.clone());
                        tasks.spawn(async move {
                            answer_each(stream, index, &log, &*answer, &clock).await;
                            log.lock().expect("no panic under the lock").streams
                                [index]
                                .1 = Some(clock.now());
                        });
                    }
                });
        self.handles.push(handle.expect("the shard starts"));
        log
    }
}

/// Answers each request on `stream` until the stream ends.
async fn answer_each(
    mut stream: Tcp,
    index: usize,
    log: &Mutex<Log>,
    answer: &(dyn Fn(usize) -> Reply + Send + Sync),
    clock: &Clock,
) {
    loop {
        let bytes = match request(&mut stream).await {
            Ok(bytes) if !bytes.is_empty() => bytes,
            _ended => return,
        };
        let count = {
            let mut log = log.lock().expect("no panic under the lock");
            log.bytes.push(bytes);
            log.requests.push(index);
            log.requests.len() - 1
        };
        match answer(count) {
            Reply::Bytes(bytes) => write(&mut stream, bytes.as_bytes()).await,
            Reply::Close(bytes, idle) => {
                write(&mut stream, bytes.as_bytes()).await;
                clock.sleep(idle).await;
                poll_fn(|cx| stream.poll_close(cx))
                    .await
                    .expect("the close works");
                return;
            }
            Reply::Nothing => {}
        }
    }
}

pub(super) fn ok(_: usize) -> Reply {
    Reply::Bytes(OK.into())
}

pub(super) fn all_ok(outcomes: &[Result<http::Response<Bytes>, Error>]) {
    for outcome in outcomes {
        let response = outcome.as_ref().expect("the server answers");
        assert_eq!(response.body().as_ref(), b"ok");
    }
}
