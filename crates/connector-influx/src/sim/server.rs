//! The write endpoints of InfluxDB over HTTP/1.1, in front of a [`Store`].

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use env::net::{self, Listener};
use env::tasks::Tasks;
use http::{Method, Request, Response, StatusCode};

use super::Store;

/// Answers InfluxDB write requests on each stream that `listener` accepts, and writes
/// each body to `store`, with the rules of [`connector::http::sim::serve`].
///
/// `POST /write?db=<db>` (InfluxDB 1) and `POST /api/v2/write?bucket=<bucket>`
/// (InfluxDB 2 and 3) give 204 when the store takes each line, and 400 with the text
/// of the store's error when it refuses one. The store still keeps each valid line,
/// as InfluxDB does. `database` is the one `db` and `bucket` that the store holds:
/// another name gives 404. It compares names as the query writes them, with no
/// percent-decoding. A request with a missing or empty `db` or `bucket`, a write to
/// `/api/v2/write` with a missing or empty `org` and `orgID`, or a `precision` other
/// than `ns` gives 400 and stores nothing; a missing or empty `precision` is `ns`.
/// Any other path gives 404, and another method on a write path gives 405. It checks
/// no token.
///
/// # Errors
///
/// The error of the first accept that fails. It runs until then.
pub async fn serve(
    listener: Listener,
    tasks: Tasks,
    store: Arc<Mutex<Store>>,
    database: String,
) -> net::Error {
    let answer = move |request: Request<Bytes>| route(&request, &store, &database);
    connector::http::sim::serve(listener, tasks, answer).await
}

fn route(
    request: &Request<Bytes>,
    store: &Mutex<Store>,
    database: &str,
) -> Response<Bytes> {
    let (method, uri) = (request.method(), request.uri());
    let key = match uri.path() {
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
            .filter(|value| !value.is_empty())
    };
    let Some(given) = query(key) else {
        return reply(StatusCode::BAD_REQUEST, format!("no {key} in the query"));
    };
    if key == "bucket" && query("org").is_none() && query("orgID").is_none() {
        return reply(
            StatusCode::BAD_REQUEST,
            "no org or orgID in the query".into(),
        );
    }
    if given != database {
        return reply(StatusCode::NOT_FOUND, format!("no {key} named {given:?}"));
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
        .write(request.body());
    match written {
        Ok(()) => reply(StatusCode::NO_CONTENT, String::new()),
        Err(error) => reply(StatusCode::BAD_REQUEST, error.to_string()),
    }
}

fn reply(status: StatusCode, text: String) -> Response<Bytes> {
    let mut response = Response::new(Bytes::from(text));
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests;
