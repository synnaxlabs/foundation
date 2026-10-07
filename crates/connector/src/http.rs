//! One HTTP/1.1 client for every connector, over `env`.

mod body;
mod pool;
mod stream;

use std::cell::Cell;
use std::fmt;
use std::future::poll_fn;
use std::net::Ipv6Addr;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::task::Poll;

use bytes::Bytes;
use env::clock::{Clock, Sleep};
use env::net::{self, Net, Tcp, tcp};
use env::tasks::Tasks;
use http::uri::{PathAndQuery, Scheme};
use http::{HeaderValue, Request, Response, Uri, header};
use http_body::Body as _;
use hyper::body::Incoming;
use hyper::client::conn::http1::{self, SendRequest};
use types::time::{Monotonic, Span};

use self::body::Whole;
use self::pool::Pool;
use self::stream::Stream;

/// The shortest share of the timeout that a connect to one address gets.
const ATTEMPT_MIN: Span = Span::from_nanos(2 * Span::SECOND.nanos());

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
    /// address in order. Each address gets an equal share of the time left, but at
    /// least 2 s or all that is left. The client sets `Host` when the request has
    /// none, and sends the path and query only.
    ///
    /// # Errors
    ///
    /// - [`Error::Uri`] when the scheme is not `http`, the host is empty or a bad
    ///   IPv6 address, the port is not a number from 1 to 65535, or the URI has user
    ///   info.
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
        let sleep = deadline.map(|deadline| self.clock.sleep_until(deadline));
        before(Box::pin(self.exchange(request, deadline)), sleep)
            .await
            .unwrap_or(Err(Error::TimedOut))
    }

    async fn exchange(
        &self,
        request: Request<Bytes>,
        deadline: Option<Monotonic>,
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
        let mut connection = self.connect(&origin, deadline).await?;
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

    async fn connect(
        &self,
        origin: &Origin,
        deadline: Option<Monotonic>,
    ) -> Result<Connection, Error> {
        let tcp = self.dial(origin, deadline).await?;
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

    /// Connects to the first address of `origin` that takes the stream. Each
    /// address gets a share of the time left to `deadline`. Gives
    /// [`Error::TimedOut`] when no time is left before an address, and else the
    /// connect error of the first address when none takes the stream.
    async fn dial(
        &self,
        origin: &Origin,
        deadline: Option<Monotonic>,
    ) -> Result<Tcp, Error> {
        let remotes = (self.net.resolve(&origin.host, origin.port).await)
            .map_err(Error::Connect)?;
        let mut first = None;
        for (i, &remote) in remotes.iter().enumerate() {
            let config = tcp::Config {
                remote,
                options: OPTIONS,
            };
            let left = remotes.len() - i;
            let now = self.clock.now();
            if deadline.is_some_and(|deadline| deadline <= now) {
                return Err(Error::TimedOut);
            }
            let sleep = deadline
                .and_then(|deadline| share(now, deadline, left))
                .map(|end| self.clock.sleep_until(end));
            let connect = before(self.net.connect(&config), sleep).await;
            match connect.unwrap_or(Err(net::Error::TimedOut { remote })) {
                Ok(tcp) => return Ok(tcp),
                Err(error) => {
                    first.get_or_insert(error);
                }
            }
        }
        Err(Error::Connect(
            first.expect("invariant: a lookup gives an address"),
        ))
    }
}

/// Polls `future` until it ends, or gives `None` when `sleep` fires first. With no
/// `sleep`, it waits for `future`.
async fn before<T>(
    future: impl Future<Output = T>,
    mut sleep: Option<Sleep>,
) -> Option<T> {
    let mut future = pin!(future);
    poll_fn(|cx| {
        if let Poll::Ready(out) = future.as_mut().poll(cx) {
            return Poll::Ready(Some(out));
        }
        match &mut sleep {
            Some(sleep) => Pin::new(sleep).poll(cx).map(|()| None),
            None => Poll::Pending,
        }
    })
    .await
}

/// The end of a connect to the first of `left` addresses, as in Go: an equal share of
/// the time from `now` to `deadline`, but at least [`ATTEMPT_MIN`]. Gives `None` when
/// the share is all the time left, so that only the request timeout ends the connect.
fn share(now: Monotonic, deadline: Monotonic, left: usize) -> Option<Monotonic> {
    let remaining = (deadline - now).nanos();
    let left = i64::try_from(left).expect("invariant: a lookup gives few addresses");
    let share = (remaining / left).max(ATTEMPT_MIN.nanos());
    (share < remaining).then(|| now + Span::from_nanos(share))
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
    let host = authority.host();
    let bracketed = host.strip_prefix('[').map(|h| h.strip_suffix(']'));
    let host_valid = match bracketed {
        Some(v6) => v6.is_some_and(|v6| v6.parse::<Ipv6Addr>().is_ok()),
        None => !host.is_empty(),
    };
    if authority.as_str().contains('@') || !host_valid {
        return Err(fail());
    }
    let rest = &authority.as_str()[host.len()..];
    // `http` takes any text after `]`, and `u16::from_str` takes a leading `+`.
    let digits = rest.strip_prefix(':').unwrap_or(rest);
    let port = match digits {
        "" => 80,
        _ if !rest.starts_with(':') || !digits.bytes().all(|b| b.is_ascii_digit()) => {
            return Err(fail());
        }
        _ => digits
            .parse()
            .ok()
            .filter(|&port| port != 0)
            .ok_or_else(fail)?,
    };
    Ok(Origin {
        host: host.to_ascii_lowercase(),
        port,
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
    /// The scheme is not `http`, the host is empty or a bad IPv6 address, the port is
    /// not a number from 1 to 65535, or the URI has user info.
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
            Self::Uri { uri } => write!(
                f,
                "{uri} is not an http URI with a valid host and port, and no user info"
            ),
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
