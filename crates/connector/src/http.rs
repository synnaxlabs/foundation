//! One HTTP/1.1 client for every connector, over `env`.

mod body;
mod pool;
mod stream;

use std::cell::Cell;
use std::fmt;
use std::future::poll_fn;
use std::pin::Pin;
use std::rc::Rc;
use std::task::Poll;

use bytes::Bytes;
use env::clock::Clock;
use env::net::{self, Net, Tcp, tcp};
use env::tasks::Tasks;
use http::uri::{PathAndQuery, Scheme};
use http::{HeaderValue, Request, Response, Uri, header};
use http_body::Body as _;
use hyper::body::Incoming;
use hyper::client::conn::http1::{self, SendRequest};
use types::time::Span;

use self::body::Whole;
use self::pool::Pool;
use self::stream::Stream;

const OPTIONS: tcp::Options = tcp::Options {
    send_buffer_bytes: 1 << 16,
    recv_buffer_bytes: 1 << 16,
    unsent_bytes_max: 1 << 14,
    delayed: false,
};

/// Sends HTTP/1.1 requests over `env`. It keeps one idle connection for each origin,
/// the host and port of a URI, and reuses it with no new name lookup. It does not
/// reuse a connection idle longer than 90 s, and the next send closes it. It drops a
/// connection that the server closed, and the connection of a request that failed. A
/// dropped client closes its idle connections. It stays on the thread that made it.
///
/// When a request on a reused connection fails before its response, the client sends
/// it once more on a new connection: always when the connection did not write it, and
/// for an idempotent method when no byte of a response came. Else it fails, because
/// the server may have acted on it.
///
/// ```
/// use bytes::Bytes;
/// use connector::http::{Client, Error};
///
/// async fn ping(client: &Client) -> Result<u16, Error> {
///     let request = http::Request::get("http://influx:8086/ping")
///         .body(Bytes::new())
///         .expect("a valid request");
///     Ok(client.send(request).await?.status().as_u16())
/// }
/// ```
#[derive(Debug)]
pub struct Client {
    net: Net,
    clock: Clock,
    tasks: Tasks,
    timeout: Span,
    body_max: usize,
    pool: Pool,
}

/// What a [`Client`] needs.
#[derive(Debug)]
pub struct Config {
    /// Connects streams.
    pub net: Net,
    /// Gives each request's deadline.
    pub clock: Clock,
    /// Runs each connection's I/O.
    pub tasks: Tasks,
    /// The longest a request may take, from the call to `send` to the last body
    /// byte. A timeout below zero acts as zero. One that passes the end of the clock
    /// never fires.
    pub timeout: Span,
    /// The largest response body the client reads.
    pub body_max: usize,
}

impl Client {
    /// Makes a client.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            net: config.net,
            clock: config.clock,
            tasks: config.tasks,
            timeout: config.timeout.max(Span::ZERO),
            body_max: config.body_max,
            pool: Pool::default(),
        }
    }

    /// Sends `request` and reads the whole response. The URI gives the host and the
    /// port, which defaults to 80. A new connection looks up the host and tries each
    /// address in order. The client sets `Host` when the request has none, and sends
    /// the path and query only.
    ///
    /// # Errors
    ///
    /// - [`Error::Uri`] when the scheme is not `http`, the URI has no host, or the
    ///   URI has user info.
    /// - [`Error::Connect`] when the name lookup failed, or no address of the host
    ///   took the connection.
    /// - [`Error::TimedOut`] when the whole exchange took longer than the timeout.
    /// - [`Error::TooLarge`] when the response body is larger than the cap.
    /// - [`Error::Protocol`] when the stream failed, the server broke HTTP, or the
    ///   server closed early.
    pub async fn send(
        &self,
        request: Request<Bytes>,
    ) -> Result<Response<Bytes>, Error> {
        let deadline = self.clock.now().checked_add(self.timeout);
        let mut exchange = Box::pin(self.exchange(request));
        let mut sleep = deadline.map(|deadline| self.clock.sleep_until(deadline));
        poll_fn(|cx| {
            if let Poll::Ready(out) = exchange.as_mut().poll(cx) {
                return Poll::Ready(out);
            }
            match &mut sleep {
                Some(sleep) => Pin::new(sleep).poll(cx).map(|()| Err(Error::TimedOut)),
                None => Poll::Pending,
            }
        })
        .await
    }

    async fn exchange(
        &self,
        request: Request<Bytes>,
    ) -> Result<Response<Bytes>, Error> {
        let (mut parts, body) = request.into_parts();
        let origin = origin(&parts.uri)?;
        origin_form(&mut parts);
        let request = Request::from_parts(parts, Whole(Some(body)));
        let request = match self.pool.take(&origin, self.clock.now()) {
            Some(mut connection) => {
                let received = connection.received.get();
                let spare = request.method().is_idempotent().then(|| copy(&request));
                match connection.sender.try_send_request(request).await {
                    Ok(response) => {
                        return self.read(origin, connection, response).await;
                    }
                    Err(mut error) => {
                        // With no byte of a response, the server may have closed the
                        // idle stream before the request reached it.
                        let unanswered = connection.received.get() == received;
                        match error.take_message().or(spare.filter(|_| unanswered)) {
                            Some(request) => request,
                            None => return Err(error.into_error().into()),
                        }
                    }
                }
            }
            None => request,
        };
        let mut connection = self.connect(&origin).await?;
        let response = connection.sender.send_request(request).await?;
        self.read(origin, connection, response).await
    }

    /// Reads the whole body of `response`, then keeps `connection` for `origin`.
    async fn read(
        &self,
        origin: Origin,
        connection: Connection,
        response: Response<Incoming>,
    ) -> Result<Response<Bytes>, Error> {
        let (parts, mut incoming) = response.into_parts();
        let mut bytes = Vec::new();
        while let Some(frame) =
            poll_fn(|cx| Pin::new(&mut incoming).poll_frame(cx)).await
        {
            if let Ok(data) = frame?.into_data() {
                if data.len() > self.body_max.saturating_sub(bytes.len()) {
                    return Err(Error::TooLarge { max: self.body_max });
                }
                bytes.extend_from_slice(&data);
            }
        }
        self.pool.put(origin, connection, self.clock.now());
        Ok(Response::from_parts(parts, Bytes::from(bytes)))
    }

    async fn connect(&self, origin: &Origin) -> Result<Connection, Error> {
        let tcp = self.dial(origin).await.map_err(Error::Connect)?;
        let received = Rc::default();
        let stream = Stream {
            tcp,
            received: Rc::clone(&received),
        };
        let (sender, connection) = http1::handshake(stream).await?;
        // `hyper` gives a connection error to the request in flight, which reports it.
        // An idle connection that fails is closed, and the pool does not reuse it.
        self.tasks.spawn(async move {
            let _reported: Result<(), hyper::Error> = connection.await;
        });
        Ok(Connection { sender, received })
    }

    /// Connects to the first address of `origin` that takes the stream. When none
    /// does, gives the error of the first.
    async fn dial(&self, origin: &Origin) -> Result<Tcp, net::Error> {
        let mut first = None;
        for remote in self.net.resolve(&origin.host, origin.port).await? {
            let config = tcp::Config {
                remote,
                options: OPTIONS,
            };
            match self.net.connect(&config).await {
                Ok(tcp) => return Ok(tcp),
                Err(error) => {
                    first.get_or_insert(error);
                }
            }
        }
        Err(first.expect("invariant: a lookup gives an address"))
    }
}

/// Where requests go: a host in ASCII lowercase, and a port.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Origin {
    host: String,
    port: u16,
}

/// A connection to a server.
#[derive(Debug)]
struct Connection {
    sender: SendRequest<Whole>,
    /// The count of bytes that its stream read, which wraps.
    received: Rc<Cell<u64>>,
}

/// The origin of `uri`.
fn origin(uri: &Uri) -> Result<Origin, Error> {
    let fail = || Error::Uri { uri: uri.clone() };
    if uri.scheme() != Some(&Scheme::HTTP) {
        return Err(fail());
    }
    let authority = uri.authority().ok_or_else(fail)?;
    if authority.as_str().contains('@') || authority.host().is_empty() {
        return Err(fail());
    }
    Ok(Origin {
        host: authority.host().to_ascii_lowercase(),
        port: authority.port_u16().unwrap_or(80),
    })
}

/// A copy of `request`, to send again.
fn copy(request: &Request<Whole>) -> Request<Whole> {
    let mut copy = Request::new(Whole(request.body().0.clone()));
    *copy.method_mut() = request.method().clone();
    *copy.uri_mut() = request.uri().clone();
    *copy.version_mut() = request.version();
    copy.headers_mut().clone_from(request.headers());
    copy
}

/// Moves the authority into `Host`, unless the request has one, and leaves the path
/// and query in the URI.
fn origin_form(parts: &mut http::request::Parts) {
    if !parts.headers.contains_key(header::HOST)
        && let Some(authority) = parts.uri.authority()
    {
        let host = HeaderValue::from_str(authority.as_str())
            .expect("an authority is a valid header value");
        parts.headers.insert(header::HOST, host);
    }
    let mut uri = http::uri::Parts::default();
    uri.path_and_query = Some(
        parts
            .uri
            .path_and_query()
            .cloned()
            .unwrap_or_else(|| PathAndQuery::from_static("/")),
    );
    parts.uri = Uri::from_parts(uri).expect("a path alone is a valid URI");
}

/// Why an exchange failed.
#[derive(Debug)]
pub enum Error {
    /// The scheme is not `http`, the URI has no host, or the URI has user info.
    Uri {
        /// The URI of the request.
        uri: Uri,
    },
    /// The name lookup failed, or no address of the host took the connection.
    Connect(net::Error),
    /// The exchange took longer than the timeout.
    TimedOut,
    /// The response body is larger than the cap.
    TooLarge {
        /// The cap, in bytes.
        max: usize,
    },
    /// The stream failed, the server broke HTTP, or the server closed early.
    Protocol(Failure),
}

/// What went wrong in an exchange after the connect. Its sources reach the `env`
/// error when the stream failed.
#[derive(Debug)]
pub struct Failure(hyper::Error);

impl From<hyper::Error> for Error {
    fn from(error: hyper::Error) -> Self {
        Self::Protocol(Failure(error))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uri { uri } => write!(f, "{uri} is not an http URI with a host"),
            Self::Connect(error) => write!(f, "the connect failed: {error}"),
            Self::TimedOut => write!(f, "the exchange timed out"),
            Self::TooLarge { max } => {
                write!(f, "the response body is larger than {max} bytes")
            }
            Self::Protocol(failure) => write!(f, "the exchange failed: {failure}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Connect(error) => Some(error),
            Self::Protocol(failure) => Some(failure),
            Self::Uri { .. } | Self::TimedOut | Self::TooLarge { .. } => None,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for Failure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

#[cfg(test)]
mod tests;
