//! The HTTP/1.1 server of the protocol simulators of HTTP connectors.

use std::convert::Infallible;
use std::future::poll_fn;
use std::pin::Pin;
use std::rc::Rc;

use bytes::Bytes;
use env::net::{self, Listener};
use env::tasks::Tasks;
use http::header::{CONNECTION, CONTENT_ENCODING};
use http::request::Parts;
use http::{HeaderValue, Request, Response, StatusCode};
use http_body::Body as _;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;

use super::body::Whole;
use super::stream::Stream;

/// Answers HTTP/1.1 requests on each stream that `listener` accepts, each stream on
/// its own task of `tasks`, with keep-alive. `answer` gets each request with its
/// whole body. It sends no `date` header and sets no timer, so no clock value
/// changes what it does. A client that closes its write side after a whole request
/// still gets the answer. A request that breaks HTTP gets 400, or 414 for a URI or
/// 431 for a head that is too long, and its stream ends. An HTTP/2 preface ends the
/// stream with no answer. A request with a `content-encoding` other than `identity`
/// gets 415 and does not reach `answer`. Dropping the future stops only the accepts:
/// each stream it accepted runs on.
///
/// # Errors
///
/// The error of the first accept that fails. It runs until then.
pub async fn serve(
    mut listener: Listener,
    tasks: Tasks,
    answer: impl Fn(Request<Bytes>) -> Response<Bytes> + 'static,
) -> net::Error {
    let answer = Rc::new(answer);
    loop {
        let tcp = match poll_fn(|cx| listener.poll_accept(cx)).await {
            Ok(tcp) => tcp,
            Err(error) => return error,
        };
        let answer = Rc::clone(&answer);
        let service = service_fn(move |request| respond(request, Rc::clone(&answer)));
        let stream = Stream {
            tcp,
            received: Rc::default(),
        };
        // The person approved this server on the condition that it gets no timer.
        let served = http1::Builder::new()
            .auto_date_header(false)
            .header_read_timeout(None)
            .half_close(true)
            .serve_connection(stream, service);
        // A stream that fails has no one to tell.
        tasks.spawn(async move { drop(served.await) });
    }
}

async fn respond(
    request: Request<Incoming>,
    answer: Rc<impl Fn(Request<Bytes>) -> Response<Bytes>>,
) -> Result<Response<Whole>, Infallible> {
    let (parts, mut body) = request.into_parts();
    let mut bytes = Vec::new();
    while let Some(frame) = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
        let Ok(frame) = frame else {
            let mut response = reply(StatusCode::BAD_REQUEST, String::new());
            let close = HeaderValue::from_static("close");
            response.headers_mut().insert(CONNECTION, close);
            return Ok(response.map(|body| Whole(Some(body))));
        };
        if let Ok(data) = frame.into_data() {
            bytes.extend_from_slice(&data);
        }
    }
    let response = match encoding(&parts) {
        Some(encoding) => reply(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("the server decodes no content-encoding, not {encoding:?}"),
        ),
        None => answer(Request::from_parts(parts, Bytes::from(bytes))),
    };
    Ok(response.map(|body| Whole(Some(body))))
}

fn reply(status: StatusCode, text: String) -> Response<Bytes> {
    let mut response = Response::new(Bytes::from(text));
    *response.status_mut() = status;
    response
}

/// The first `content-encoding` other than `identity`.
fn encoding(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get_all(CONTENT_ENCODING)
        .iter()
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .find(|value| !value.eq_ignore_ascii_case("identity"))
}

#[cfg(test)]
mod tests;
