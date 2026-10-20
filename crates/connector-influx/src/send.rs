//! The loop that sends the lines of a reader's frames to InfluxDB.

use std::cell::RefCell;
use std::future::{self, Future as _, poll_fn};
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;

use bytes::Bytes;
use connector::http::{Client, Pem};
use connector::kind::{Context, Error};
use connector::retry::{self, Backoff};
use connector::secret;
use http::header::{AUTHORIZATION, HeaderValue};
use http::uri::Authority;
use http::{Method, Request, StatusCode, Uri};
use hub::reader::{Ended, Received};
use types::time::Span;

use crate::batch::{Ack, Batch, Taken};
use crate::{CONFIRMED, Config, LOST, OVERSIZED, Target};

/// The longest name of a database, bucket, or org, in bytes.
pub(crate) const NAME_MAX: usize = 4_096;
const _: () = assert!(
    "/api/v2/write?org=&bucket=&precision=ns".len() + 2 * 3 * NAME_MAX <= 65_534,
    "the longest write path, two names with each byte percent-encoded, fits in the \
     65,534 bytes that `http` takes"
);
/// The longest a write request may take.
const TIMEOUT: Span = Span::from_nanos(10_000_000_000);
/// The largest answer body read, for the text of an error.
const BODY_MAX: usize = 1 << 16;
/// The waits between the attempts of one request.
const RETRY: retry::Config = retry::Config {
    first: Span::SECOND,
    cap: Span::MINUTE,
};

enum Event<'a> {
    Cancelled,
    /// What the request body that InfluxDB took acks and counts.
    Sent(Option<Ack>),
    Frame(Result<Received<'a>, Ended>),
}

/// Writes the frames of the run's reader to InfluxDB until the cancel, with one
/// request in flight. While one is in flight it reads the next batch, until a request
/// body closes, or holds 5,000 lines or 4 MiB of them, so the lines of a frame can span
/// requests.
/// It acks a frame and counts its samples, lost seqs, and oversized samples only after
/// InfluxDB answers 204 to the request of its last line, or with no request when no
/// line waits for one.
pub(crate) async fn run(mut ctx: Context<Config>) -> Result<(), Error> {
    let mut reader = ctx.reader().expect("invariant: check gives a reader");
    let ctx = &ctx;
    let config = ctx.config();
    let client = RefCell::new(Rc::new(client(ctx).await?));
    let uri = write_uri(&config.address, &config.target);
    let token = RefCell::new(Token::read(ctx).await?);
    let (confirmed, lost) = (ctx.count(CONFIRMED), ctx.count(LOST));
    let oversized = ctx.count(OVERSIZED);
    let mut batch = Batch::new(config.measurement.clone(), &config.target, ctx.name());
    let mut cancelled = pin!(ctx.cancel().wait());
    let mut flight = pin!(None);
    loop {
        if flight.is_none()
            && let Some(Taken { body, ack }) = batch.take()
        {
            let (client, uri, token) = (&client, &uri, &token);
            flight.set(Some(async move {
                if !body.is_empty() {
                    send(client, ctx, uri, token, body.into()).await;
                }
                ack
            }));
        }
        let full = batch.full();
        let event = {
            let mut frame = pin!(async {
                if full {
                    future::pending().await
                } else {
                    reader.next().await
                }
            });
            poll_fn(|cx| {
                if cancelled.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Event::Cancelled);
                }
                if let Some(sending) = flight.as_mut().as_pin_mut()
                    && let Poll::Ready(sent) = sending.poll(cx)
                {
                    return Poll::Ready(Event::Sent(sent));
                }
                frame.as_mut().poll(cx).map(Event::Frame)
            })
            .await
        };
        match event {
            Event::Cancelled => return Ok(()),
            Event::Sent(ack) => {
                flight.set(None);
                if let Some(ack) = ack {
                    reader.ack(ack.position);
                    confirmed.add(ack.counts.samples);
                    lost.add(ack.counts.lost);
                    oversized.add(ack.counts.oversized);
                }
            }
            Event::Frame(Ok(received)) => batch.add(&received)?,
            Event::Frame(Err(ended)) => return Err(ended.into()),
        }
    }
}

/// The client of a run, which checks a certificate at the mesh time of its hub. It
/// trusts the roots of the node and the `ca`.
///
/// # Errors
///
/// The error of [`Context::secret`] when a read of the `ca` fails, and
/// [`Error::Retry`] when the `ca` holds no PEM certificate or one that does not parse.
async fn client(ctx: &Context<Config>) -> Result<Client, Error> {
    let config = ctx.config();
    let roots = match &config.ca {
        Some(source) => {
            let pem = ctx.secret(source).await?;
            let roots = ctx.roots().with_pem(&String::from_utf8_lossy(pem.expose()));
            roots.map_err(|error| {
                let held = match error {
                    Pem::Empty => "no PEM certificate",
                    Pem::Invalid => "a PEM certificate that does not parse",
                };
                Error::Retry(
                    format!(
                        "the ca {source} holds {held}. Write the CA certificate of the \
                         server in PEM"
                    )
                    .into(),
                )
            })?
        }
        None => ctx.roots().clone(),
    };
    // The reader opened, so the node has mesh time and this gives at once.
    let time = ctx.time().await;
    Ok(Client::new(connector::http::Config {
        net: ctx.net().clone(),
        clock: ctx.clock().clone(),
        tasks: ctx.tasks().clone(),
        timeout: TIMEOUT,
        body_max: BODY_MAX,
        tls: Some(connector::http::Tls { roots, time }),
    }))
}

/// What an attempt that failed makes the next attempt read again.
enum Read {
    Token,
    Ca,
}

/// Shows the answer `status` with `body` of InfluxDB at `at` as the run's error.
/// Gives whether the token is the cause.
fn answered(
    ctx: &Context<Config>,
    at: &Authority,
    status: StatusCode,
    body: &[u8],
) -> bool {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    let token = matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN);
    if let Some(source) = ctx.config().token.as_ref().filter(|_| token) {
        ctx.error(&format_args!(
            "InfluxDB at {at} answered {status}. Check the token {source}: {text}"
        ));
        return true;
    }
    ctx.error(&format_args!(
        "InfluxDB at {at} answered {status}: {text}{}",
        fix(status)
    ));
    false
}

/// Sends `body` until InfluxDB answers 204. Shows each other answer as the run's
/// error, with the fix for a 400, a 404, a 413, or a 422, and waits a backoff before
/// the next attempt. After a 401 or a 403, it reads the token again before that
/// attempt, and after a TLS failure of a run with a `ca`, the `ca` and a new `client`.
/// A read that fails shows its error and keeps the old value, and `send` makes it again
/// after each backoff, with no attempt, until it gives a value. Never ends after the
/// cancel of the run, which the caller sees first.
async fn send(
    client: &RefCell<Rc<Client>>,
    ctx: &Context<Config>,
    uri: &Uri,
    token: &RefCell<Option<Token>>,
    body: Bytes,
) {
    let mut backoff = None;
    loop {
        let request = request(uri, &body, token.borrow().as_ref());
        // A block, so that `answer` does not grow the future across the backoff.
        let read = {
            let sending = Rc::clone(&client.borrow());
            let mut read = None;
            let answer = match sending.send(request).await {
                Ok(answer) if answer.status() == StatusCode::NO_CONTENT => {
                    ctx.clear_error();
                    return;
                }
                Ok(answer) => Some((answer.status(), answer.into_body())),
                Err(connector::http::Error::TooLarge { status, body, .. }) => {
                    Some((status, body))
                }
                Err(error) => {
                    if let (connector::http::Error::Tls { .. }, Some(ca)) =
                        (&error, &ctx.config().ca)
                    {
                        ctx.error(&format_args!(
                            "the write to InfluxDB failed: {error}. Check the ca {ca}"
                        ));
                        read = Some(Read::Ca);
                    } else {
                        ctx.error(&format_args!(
                            "the write to InfluxDB failed: {error}"
                        ));
                    }
                    None
                }
            };
            match answer {
                Some((status, body)) => {
                    let at =
                        uri.authority().expect("invariant: a write URI has a host");
                    answered(ctx, at, status, &body).then_some(Read::Token)
                }
                None => read,
            }
        };
        let backoff =
            backoff.get_or_insert_with(|| Backoff::new(ctx.clock(), ctx.rng(), RETRY));
        loop {
            if !backoff.wait(ctx.cancel()).await {
                future::pending::<()>().await;
            }
            let Some(read) = &read else {
                break;
            };
            // Boxed, so each rare read does not grow the future of `send`.
            let Err(error) = Box::pin(read_again(read, ctx, client, token)).await
            else {
                break;
            };
            ctx.error(&error);
        }
    }
}

/// Makes the read that `read` names, and on success puts its value in `client` or
/// `token`.
///
/// # Errors
///
/// The error of [`Token::read`] or of [`client`].
async fn read_again(
    read: &Read,
    ctx: &Context<Config>,
    client: &RefCell<Rc<Client>>,
    token: &RefCell<Option<Token>>,
) -> Result<(), Error> {
    match read {
        Read::Token => *token.borrow_mut() = Token::read(ctx).await?,
        Read::Ca => *client.borrow_mut() = Rc::new(self::client(ctx).await?),
    }
    Ok(())
}

/// The fix that the error text of an answer of `status` ends with, after its body.
fn fix(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => {
            ". For a field type conflict, write to another `measurement` or delete the \
             old field in InfluxDB. For points older than the retention period, raise \
             it or write to another bucket"
        }
        StatusCode::NOT_FOUND => {
            ". Create the database, the bucket, or the org in InfluxDB, or name one \
             that exists"
        }
        StatusCode::PAYLOAD_TOO_LARGE => {
            ". Raise the request size limit of InfluxDB (`--max-http-request-size` of \
             InfluxDB 3, `max-body-size` of InfluxDB 1), or write fewer channels or \
             shorter samples at each sample time"
        }
        _ => "",
    }
}

/// The request of one attempt to write `body` to `uri`, with the header of `token`.
pub fn request(uri: &Uri, body: &Bytes, token: Option<&Token>) -> Request<Bytes> {
    let mut request = Request::new(body.clone());
    *request.method_mut() = Method::POST;
    *request.uri_mut() = uri.clone();
    if let Some(token) = token {
        request.headers_mut().insert(AUTHORIZATION, token.header());
    }
    request
}

/// The token that each request of a run sends in its `Authorization` header.
#[derive(Debug)]
pub struct Token {
    /// `Token <value>`, which a header holds.
    header: secret::Value,
}

impl Token {
    /// The token of `value`, or `None` when it holds a byte that a header cannot hold.
    #[must_use]
    pub fn new(value: &secret::Value) -> Option<Self> {
        let mut header = Vec::with_capacity(value.expose().len().saturating_add(6));
        header.extend_from_slice(b"Token ");
        header.extend_from_slice(value.expose());
        let header = secret::Value::new(header);
        HeaderValue::from_bytes(header.expose()).ok()?;
        Some(Self { header })
    }

    /// Reads the token of the run's config, or gives `None` when it has none.
    ///
    /// # Errors
    ///
    /// The error of [`Context::secret`], and [`Error::Retry`] when the value holds a
    /// byte that a header cannot hold.
    async fn read(ctx: &Context<Config>) -> Result<Option<Self>, Error> {
        let Some(source) = &ctx.config().token else {
            return Ok(None);
        };
        let value = ctx.secret(source).await?;
        let token = Self::new(&value).ok_or_else(|| {
            Error::Retry(
                format!(
                    "the token {source} holds a byte that an HTTP header cannot hold. \
                     Write the token with no control byte"
                )
                .into(),
            )
        })?;
        Ok(Some(token))
    }

    /// The `Authorization` header, marked sensitive.
    fn header(&self) -> HeaderValue {
        let header = HeaderValue::from_bytes(self.header.expose());
        let mut header = header.expect("invariant: `new` checked the bytes");
        header.set_sensitive(true);
        header
    }
}

/// The write URI of `target` at `address`, with nanosecond times.
fn write_uri(address: &Uri, target: &Target) -> Uri {
    let mut query = form_urlencoded::Serializer::new(String::new());
    let path = match target {
        Target::Database(database) => {
            query.append_pair("db", database);
            "/write"
        }
        Target::Bucket { name, org } => {
            query.append_pair("org", org).append_pair("bucket", name);
            "/api/v2/write"
        }
    };
    let query = query.append_pair("precision", "ns").finish();
    let mut parts = address.clone().into_parts();
    parts.path_and_query = Some(
        format!("{path}?{query}")
            .parse()
            .expect("invariant: names of at most NAME_MAX bytes fit in a query"),
    );
    Uri::from_parts(parts).expect("invariant: an address with a path is a URI")
}

#[cfg(test)]
mod tests;
