//! The write endpoints of InfluxDB over HTTP/1.1, in front of a [`Store`].

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use connector::cancel::Token;
use connector::http::sim::Scheme;
use env::net::{self, Listener};
use env::tasks::Tasks;
use http::header::{AUTHORIZATION, CONTENT_ENCODING};
use http::{Method, Request, Response, StatusCode};

use super::Store;

/// Answers InfluxDB write requests on each stream that `listener` accepts, as `scheme`
/// says, and writes each body to `store`, with the rules of
/// [`connector::http::sim::serve`].
///
/// `POST /write?db=<db>` (InfluxDB 1) and `POST /api/v2/write?bucket=<bucket>`
/// (InfluxDB 2 and 3) give 204 when the store takes each line, and 400 with the text
/// of the store's error when it refuses one. The store still keeps each valid line,
/// as InfluxDB does. `database` is the one `db` and `bucket` that the store holds:
/// another name gives 404. It decodes the query as a form, as InfluxDB does: `+` is a
/// space, and `%XX` a byte. A request with a missing or empty `db` or `bucket`, a write to
/// `/api/v2/write` with a missing or empty `org` and `orgID`, or a `precision` other
/// than `ns` gives 400 and stores nothing; a missing or empty `precision` is `ns`.
/// Any other path gives 404, and another method on a write path gives 405. A request
/// whose `content-encoding` names a coding other than `identity` gets 415 and stores
/// nothing. `token`, when given, is the one token that it takes, as `Token <token>`
/// in `Authorization`. A request without it gets 401 and stores nothing. It checks
/// the path, then the method, then the token, then the `content-encoding`, then the
/// query, and gives the answer of the first check that fails.
///
/// It runs and returns as [`connector::http::sim::serve`] does.
///
/// # Errors
///
/// As [`connector::http::sim::serve`].
pub async fn serve(
    listener: Listener,
    tasks: Tasks,
    until: Token,
    scheme: Scheme,
    store: Arc<Mutex<Store>>,
    database: String,
    token: Option<String>,
) -> Result<(), net::Error> {
    let answer = answer(store, database, token);
    connector::http::sim::serve(listener, tasks, until, scheme, answer).await
}

fn answer(
    store: Arc<Mutex<Store>>,
    database: String,
    token: Option<String>,
) -> impl Fn(Request<Bytes>) -> Response<Bytes> + 'static {
    let header = token.map(|token| format!("Token {token}"));
    move |request| route(&request, &store, &database, header.as_deref())
}

fn route(
    request: &Request<Bytes>,
    store: &Mutex<Store>,
    database: &str,
    header: Option<&str>,
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
    if let Some(header) = header {
        let given = request.headers().get(AUTHORIZATION);
        if given.is_none_or(|given| given.as_bytes() != header.as_bytes()) {
            return reply(
                StatusCode::UNAUTHORIZED,
                r#"{"code":"unauthorized","message":"unauthorized access"}"#.into(),
            );
        }
    }
    let lines = request.headers().get_all(CONTENT_ENCODING).iter();
    let refused = (lines.flat_map(|line| line.as_bytes().split(|&byte| byte == b',')))
        .map(<[u8]>::trim_ascii)
        .find(|coding| !coding.is_empty() && !coding.eq_ignore_ascii_case(b"identity"));
    if let Some(coding) = refused {
        let coding = String::from_utf8_lossy(coding);
        return reply(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("the store decodes no content-encoding, not {coding:?}"),
        );
    }
    let query = |key: &str| {
        form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
            .find_map(|(given, value)| (given == key).then_some(value))
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
    if *given != *database {
        return reply(StatusCode::NOT_FOUND, format!("no {key} named {given:?}"));
    }
    if let Some(precision) = query("precision").filter(|precision| precision != "ns") {
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
