//! The write endpoints of InfluxDB over HTTP/1.1, in front of a [`Store`].

use std::future::poll_fn;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use env::net::{self, Listener, Tcp};
use env::tasks::Tasks;
use http::{Method, Request, Response, StatusCode, Uri};
use http_body::Body as _;
use hyper::body::Incoming;
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper::server::conn::http1;
use hyper::service::service_fn;

use super::Store;

/// The most bytes one read copies into `hyper`'s buffer.
const READ_MAX: usize = 8192;

/// Answers InfluxDB write requests on each stream that `listener` accepts, and writes
/// each body to `store`. Each stream runs on its own task of `tasks`, with HTTP/1.1
/// keep-alive. It runs until the caller drops it. A stream that the listener fails to
/// accept is lost, as on InfluxDB.
///
/// `POST /write?db=<db>` (InfluxDB 1) and `POST /api/v2/write?bucket=<bucket>`
/// (InfluxDB 2 and 3) give 204 when the store takes each line, and 400 with the text
/// of the store's error when it refuses one. The store still keeps each valid line,
/// as InfluxDB does. A request with no `db` or `bucket`, or with a `precision` other
/// than `ns`, gives 400 and stores nothing. Any other path gives 404, and another
/// method on a write path gives 405. The store is one database: it does not key
/// points by `db` or `bucket`, and it checks no token.
#[expect(clippy::infinite_loop, reason = "it serves until the caller drops it")]
pub async fn serve(mut listener: Listener, tasks: Tasks, store: Arc<Mutex<Store>>) {
    loop {
        let Ok(tcp) = poll_fn(|cx| listener.poll_accept(cx)).await else {
            continue;
        };
        let store = Arc::clone(&store);
        tasks.spawn(async move {
            let service =
                service_fn(move |request| answer(request, Arc::clone(&store)));
            let served = http1::Builder::new()
                .serve_connection(Stream(tcp), service)
                .await;
            // `hyper` answers a request that breaks HTTP with 400 before it gives the
            // error, and the client sees a stream that breaks.
            drop(served);
        });
    }
}

async fn answer(
    request: Request<Incoming>,
    store: Arc<Mutex<Store>>,
) -> Result<Response<String>, hyper::Error> {
    let (head, mut body) = request.into_parts();
    let mut bytes = Vec::new();
    while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        if let Ok(data) = frame?.into_data() {
            bytes.extend_from_slice(&data);
        }
    }
    Ok(write(&head.method, &head.uri, &bytes, &store))
}

fn write(
    method: &Method,
    uri: &Uri,
    body: &[u8],
    store: &Mutex<Store>,
) -> Response<String> {
    let database = match uri.path() {
        "/write" => "db",
        "/api/v2/write" => "bucket",
        path => return reply(StatusCode::NOT_FOUND, format!("no endpoint at {path}")),
    };
    if method != Method::POST {
        return reply(
            StatusCode::METHOD_NOT_ALLOWED,
            format!("{} takes POST, not {method}", uri.path()),
        );
    }
    let query = |key: &str| {
        uri.query()
            .unwrap_or_default()
            .split('&')
            .find_map(|pair| pair.strip_prefix(key)?.strip_prefix('='))
    };
    if query(database).is_none_or(str::is_empty) {
        return reply(
            StatusCode::BAD_REQUEST,
            format!("no {database} in the query"),
        );
    }
    if let Some(precision) = query("precision").filter(|&precision| precision != "ns") {
        return reply(
            StatusCode::BAD_REQUEST,
            format!("the store takes precision ns only, not {precision:?}"),
        );
    }
    let written = store
        .lock()
        .expect("invariant: no panic under the store lock")
        .write(body);
    match written {
        Ok(()) => reply(StatusCode::NO_CONTENT, String::new()),
        Err(error) => reply(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

fn reply(status: StatusCode, text: String) -> Response<String> {
    let mut response = Response::new(text);
    *response.status_mut() = status;
    response
}

/// A TCP stream that `hyper` reads and writes.
struct Stream(Tcp);

impl Read for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        let mut bytes = [0; READ_MAX];
        let Some(bytes) = bytes.get_mut(..buf.remaining().min(READ_MAX)) else {
            unreachable!("the length is at most READ_MAX")
        };
        if bytes.is_empty() {
            return Poll::Ready(Ok(()));
        }
        self.get_mut().0.poll_read(cx, bytes).map(|read| {
            let n = read.map_err(io)?;
            buf.put_slice(bytes.get(..n).expect("a read fits its buffer"));
            Ok(())
        })
    }
}

impl Write for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_write_vectored(cx, &[IoSlice::new(buf)])
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().0.poll_write(cx, bufs).map_err(io)
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        self.get_mut().0.poll_close(cx).map_err(io)
    }
}

fn io(error: net::Error) -> io::Error {
    io::Error::other(error)
}

#[cfg(test)]
mod tests;
