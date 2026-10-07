//! One HTTP/1.1 client for every connector, over `env`.

mod body;
mod pool;
mod stream;

use std::fmt;
use std::future::poll_fn;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::task::Poll;

use bytes::Bytes;
use env::clock::Clock;
use env::net::{self, Net, tcp};
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

/// Sends HTTP/1.1 requests over `env`. It keeps one idle connection for each origin
/// and reuses it. It does not reuse a connection idle longer than 90 s, and the next
/// send closes it. It drops a connection that the server closed, and the connection
/// of a request that failed. A dropped client closes its idle connections. It stays
/// on the thread that made it.
///
/// When a request on a reused connection fails before its response, the client sends
/// it once more on a new connection: always when the connection did not write it, and
/// for an idempotent method when it did. A request with another method then fails,
/// because the server may have acted on it.
///
/// ```
/// use bytes::Bytes;
/// use connector::http::{Client, Error};
///
/// async fn ping(client: &Client) -> Result<u16, Error> {
///     let request = http::Request::get("http://10.0.0.2:8086/ping")
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
    /// byte. A
    /// timeout below zero acts as zero. One that passes the end of the clock never
    /// fires.
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
    /// port, which defaults to 80. The client sets `Host` when the request has none,
    /// and sends the path and query only.
    ///
    /// # Errors
    ///
    /// - [`Error::Uri`] when the scheme is not `http`, the URI has user info, or the
    ///   host is not an IP address.
    /// - [`Error::Connect`] when the host did not take the connection.
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
        let remote = remote(&parts.uri)?;
        origin_form(&mut parts);
        let request = Request::from_parts(parts, Whole(Some(body)));
        let request = match self.pool.take(remote, self.clock.now()) {
            Some(mut sender) => {
                // The server may have closed the idle stream before this request
                // reached it. RFC 9112 lets a client send an idempotent request again.
                let spare = request.method().is_idempotent().then(|| copy(&request));
                match sender.try_send_request(request).await {
                    Ok(response) => return self.read(remote, sender, response).await,
                    Err(mut error) => match error.take_message().or(spare) {
                        Some(request) => request,
                        None => return Err(error.into_error().into()),
                    },
                }
            }
            None => request,
        };
        let mut sender = self.connect(remote).await?;
        let response = sender.send_request(request).await?;
        self.read(remote, sender, response).await
    }

    /// Reads the whole body of `response`, then keeps `sender` for `remote`.
    async fn read(
        &self,
        remote: SocketAddr,
        sender: SendRequest<Whole>,
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
        self.pool.put(remote, sender, self.clock.now());
        Ok(Response::from_parts(parts, Bytes::from(bytes)))
    }

    async fn connect(&self, remote: SocketAddr) -> Result<SendRequest<Whole>, Error> {
        let config = tcp::Config {
            remote,
            options: OPTIONS,
        };
        let tcp = self.net.connect(&config).await.map_err(Error::Connect)?;
        let (sender, connection) = http1::handshake(Stream(tcp)).await?;
        // `hyper` gives a connection error to the request in flight, which reports it.
        // An idle connection that fails is closed, and the pool does not reuse it.
        self.tasks.spawn(async move {
            let _reported: Result<(), hyper::Error> = connection.await;
        });
        Ok(sender)
    }
}

/// The address `uri` names.
fn remote(uri: &Uri) -> Result<SocketAddr, Error> {
    let fail = || Error::Uri { uri: uri.clone() };
    if uri.scheme() != Some(&Scheme::HTTP) {
        return Err(fail());
    }
    let authority = uri.authority().ok_or_else(fail)?;
    if authority.as_str().contains('@') {
        return Err(fail());
    }
    let host = authority.host();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let ip: IpAddr = host.parse().map_err(|_not_ip| fail())?;
    Ok(SocketAddr::new(ip, authority.port_u16().unwrap_or(80)))
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
    /// The scheme is not `http`, the URI has user info, or the host is not an IP
    /// address.
    Uri {
        /// The URI of the request.
        uri: Uri,
    },
    /// The host did not take the connection.
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
            Self::Uri { uri } => write!(f, "{uri} is not an http URI with an IP host"),
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
