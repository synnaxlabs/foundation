//! The HTTP/1.1 server of the protocol simulators of HTTP connectors.

use std::future::poll_fn;
use std::io::IoSlice;
use std::rc::Rc;

use bytes::Bytes;
use env::net::{self, Listener, Tcp};
use env::tasks::Tasks;
use http::header::{CONNECTION, CONTENT_ENCODING, CONTENT_LENGTH, TRANSFER_ENCODING};
use http::request::Parts;
use http::{
    HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
};

/// The most header fields of one request.
const HEADERS_MAX: usize = 64;
/// The most bytes that one read takes.
const READ_MAX: usize = 8192;

/// Answers HTTP/1.1 requests on each stream that `listener` accepts, each stream on
/// its own task of `tasks`, with keep-alive. `answer` gets each request with its
/// whole body. It reads no clock and sets no timeout. A stream runs until its client
/// closes it or the shard ends; a client that closes its write side after a whole
/// request still gets the answer. A request that breaks HTTP/1.1 gets 400 and one
/// with a `transfer-encoding` gets 501, and each ends its stream. A request with a
/// `content-encoding` other than `identity` gets 415. None of them reaches `answer`.
///
/// It runs until the listener fails, and returns that error. Dropping it stops only
/// the accepts.
pub async fn serve(
    mut listener: Listener,
    tasks: Tasks,
    answer: impl Fn(Request<Bytes>) -> Response<Bytes> + 'static,
) -> net::Error {
    let answer = Rc::new(answer);
    loop {
        match poll_fn(|cx| listener.poll_accept(cx)).await {
            Ok(tcp) => {
                let answer = Rc::clone(&answer);
                // A stream that fails has no one to tell.
                tasks.spawn(async move { drop(stream(tcp, &*answer).await) });
            }
            Err(error) => return error,
        }
    }
}

/// A whole request head.
struct Head {
    /// The length of the head.
    len: usize,
    parts: Parts,
    /// The length of the body.
    body: usize,
    /// The stream ends after the answer.
    closing: bool,
}

/// A head that ends the stream after this answer.
type Refused = (StatusCode, String);

/// Serves one stream until it ends. A stream that ends inside a request drops with
/// no close, and the client sees a reset.
async fn stream(
    mut tcp: Tcp,
    answer: &impl Fn(Request<Bytes>) -> Response<Bytes>,
) -> Result<(), net::Error> {
    let mut buffer = Vec::new();
    loop {
        let Head {
            len,
            parts,
            body,
            closing,
        } = match head(&buffer) {
            Ok(Some(head)) => head,
            Ok(None) => {
                if read(&mut tcp, &mut buffer).await? == 0 {
                    return if buffer.is_empty() {
                        close(&mut tcp).await
                    } else {
                        Ok(())
                    };
                }
                continue;
            }
            Err((status, text)) => {
                write(&mut tcp, &reply(status, text), true).await?;
                return close(&mut tcp).await;
            }
        };
        buffer.drain(..len);
        while buffer.len() < body {
            if read(&mut tcp, &mut buffer).await? == 0 {
                return Ok(());
            }
        }
        let body = Bytes::copy_from_slice(buffer.drain(..body).as_slice());
        let response = match encoding(&parts) {
            Some(encoding) => reply(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("the server decodes no content-encoding, not {encoding:?}"),
            ),
            None => answer(Request::from_parts(parts, body)),
        };
        write(&mut tcp, &response, closing).await?;
        if closing {
            return close(&mut tcp).await;
        }
    }
}

/// The head at the start of `buffer`, or `None` when not all of it has come.
fn head(buffer: &[u8]) -> Result<Option<Head>, Refused> {
    let mut headers = [httparse::EMPTY_HEADER; HEADERS_MAX];
    let mut request = httparse::Request::new(&mut headers);
    let len = match request.parse(buffer) {
        Ok(httparse::Status::Complete(len)) => len,
        Ok(httparse::Status::Partial) => return Ok(None),
        Err(error) => return Err(broken(error)),
    };
    let whole = "invariant: a whole head has a method, a path, and a version";
    let (Some(method), Some(path), Some(version)) =
        (request.method, request.path, request.version)
    else {
        unreachable!("{whole}")
    };
    let mut parts = Request::new(()).into_parts().0;
    parts.version = if version == 0 {
        Version::HTTP_10
    } else {
        Version::HTTP_11
    };
    let (Ok(method), Ok(uri)) =
        (Method::from_bytes(method.as_bytes()), Uri::try_from(path))
    else {
        return Err(broken(format!(
            "{method} {path} is not a method and a path"
        )));
    };
    (parts.method, parts.uri) = (method, uri);
    for header in request.headers.iter() {
        let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(header.name.as_bytes()),
            HeaderValue::from_bytes(header.value),
        ) else {
            return Err(broken(format!("the header {} is not valid", header.name)));
        };
        parts.headers.append(name, value);
    }
    if parts.headers.contains_key(TRANSFER_ENCODING) {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            "the server takes no transfer-encoding".into(),
        ));
    }
    let body = match length(&parts) {
        Ok(body) => body,
        Err(text) => return Err(broken(text)),
    };
    let closing = parts.version == Version::HTTP_10
        || parts.headers.get_all(CONNECTION).iter().any(|value| {
            value
                .as_bytes()
                .split(|&byte| byte == b',')
                .any(|option| option.trim_ascii().eq_ignore_ascii_case(b"close"))
        });
    Ok(Some(Head {
        len,
        parts,
        body,
        closing,
    }))
}

/// The body length that the `content-length` fields give, or 0 with none.
fn length(parts: &Parts) -> Result<usize, String> {
    let mut length = None;
    for value in parts.headers.get_all(CONTENT_LENGTH) {
        let text = String::from_utf8_lossy(value.as_bytes());
        let parsed = (!text.is_empty()
            && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.parse::<usize>().ok())
        .flatten()
        .ok_or_else(|| format!("content-length {text:?} is not a length"))?;
        match length {
            Some(first) if first != parsed => {
                return Err(format!("content-length {first} and {parsed} differ"));
            }
            _ => length = Some(parsed),
        }
    }
    Ok(length.unwrap_or(0))
}

/// The first `content-encoding` other than `identity`.
fn encoding(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get_all(CONTENT_ENCODING)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_owned())
        .find(|value| !value.eq_ignore_ascii_case("identity"))
}

fn broken(error: impl std::fmt::Display) -> Refused {
    (
        StatusCode::BAD_REQUEST,
        format!("the request breaks HTTP/1.1: {error}"),
    )
}

fn reply(status: StatusCode, text: String) -> Response<Bytes> {
    let mut response = Response::new(Bytes::from(text));
    *response.status_mut() = status;
    response
}

async fn read(tcp: &mut Tcp, buffer: &mut Vec<u8>) -> Result<usize, net::Error> {
    let mut bytes = [0; READ_MAX];
    let n = poll_fn(|cx| tcp.poll_read(cx, &mut bytes)).await?;
    buffer.extend_from_slice(bytes.get(..n).expect("a read fits its buffer"));
    Ok(n)
}

/// Writes `response` with a `content-length`, or with none for a status that has
/// no body, and with `connection: close` when `closing`.
///
/// # Panics
///
/// When `response` has a body and a status that has none.
async fn write(
    tcp: &mut Tcp,
    response: &Response<Bytes>,
    closing: bool,
) -> Result<(), net::Error> {
    let status = response.status();
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        status.as_str(),
        status.canonical_reason().unwrap_or_default()
    )
    .into_bytes();
    for (name, value) in response.headers() {
        head.extend_from_slice(name.as_str().as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    let body = response.body();
    if status.is_informational()
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
    {
        assert!(body.is_empty(), "invariant: a {status} answer has no body");
    } else {
        head.extend_from_slice(
            format!("content-length: {}\r\n", body.len()).as_bytes(),
        );
    }
    if closing {
        head.extend_from_slice(b"connection: close\r\n");
    }
    head.extend_from_slice(b"\r\n");
    let (mut head, mut body) = (head.as_slice(), body.as_ref());
    while !head.is_empty() || !body.is_empty() {
        let slices = [IoSlice::new(head), IoSlice::new(body)];
        let mut n = poll_fn(|cx| tcp.poll_write(cx, &slices)).await?;
        let taken = n.min(head.len());
        head = head.get(taken..).expect("the count fits the head");
        n = n
            .checked_sub(taken)
            .expect("the count is at least what was taken");
        body = body.get(n..).expect("a write fits its buffers");
    }
    Ok(())
}

async fn close(tcp: &mut Tcp) -> Result<(), net::Error> {
    poll_fn(|cx| tcp.poll_close(cx)).await
}

#[cfg(test)]
mod tests;
