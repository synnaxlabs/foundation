//! The `opcua` kind: subscribes to OPC UA nodes and writes each change, or reads them
//! at a fixed rate and writes at most one frame per tick.

use std::ffi::CStr;
use std::{fmt, future};

use connector::kind::{self, Channels, Context, Error};
use connector::{browse, pace, poll, secret, status};
use document::diagnostic::{Code, Diagnostic};
use document::schema::Body;
use document::{Block, Document};
use env::net::Net;
use env::rng::Rng;
use types::name::Name;
use types::sample::{self, Scalar, Type};
#[cfg(feature = "sim")]
use types::time::Span;
use types::time::Stamp;

use crate::browse::Server;
use crate::client::{self, Client, Credential, Credentials, Large, Refusal};
use crate::config::{self, Config, Read, Security};
use crate::event;
use crate::ffi::Status;
use crate::node::Node;
use crate::value::{self, Cause, Fault, SCALARS, Value};

#[cfg(test)]
#[cfg(feature = "sim")]
mod tests;
mod watch;

#[cfg(feature = "sim")]
pub use watch::drives;

const SAMPLE_TYPE: Code = Code::new("opcua.sample-type");
const LARGE_READ: Code = Code::new("opcua.large-read");

/// The note of a run with no `read` block.
const NO_READ: &str = "the connector has no `read` block, so it reads nothing. \
                       `browse` lists the points of the server, and `import` adds a \
                       `read` block for each.";

/// The counts of the status of each connector: the samples that the home applied,
/// the ticks that the timer skipped or the changes after a full queue of the server,
/// the samples that a frame lost, and the values that a frame leaves out as Bad, as
/// Uncertain, and as of another type.
const COUNTS: [&str; 6] = {
    let [applied, lost] = poll::COUNTS;
    [applied, "missed", lost, "bad", "uncertain", "mistyped"]
};

/// The `opcua` kind: subscribes to the Value of OPC UA nodes and writes each change at
/// the mesh time of its timestamp, or, with `polled`, reads them at a fixed rate in
/// Read requests that each fit one message of the server, and writes at most one frame
/// per tick.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Kind;

impl kind::Kind for Kind {
    type Config = Config;
    const SCHEMA: Body = config::BODY;

    /// Reads `address`, `rate`, an optional bool `polled`, the sources `certificate`,
    /// `key`, and `trust` of a secure channel with the bool `unencrypted`, and zero or
    /// more `read` blocks, each with the channel as its label and a `node_id`, and no
    /// other key or block. A `certificate` needs `key` and `trust`, and each of the
    /// other three needs `certificate`.
    fn parse(&self, config: &Document) -> Result<Config, Vec<Diagnostic>> {
        config::parse(config)
    }

    /// Gives the channel of each `read` block as a write, in order, and the counts
    /// `in`, `missed`, `lost`, `bad`, `uncertain`, and `mistyped`.
    fn check(&self, config: &Config) -> Result<Channels, Vec<Diagnostic>> {
        let counts = COUNTS.map(|count| count.parse().expect("a count is a name"));
        Ok(Channels {
            reader: None,
            writes: config
                .reads
                .iter()
                .map(|read| read.channel.clone())
                .collect(),
            counts: counts.into(),
        })
    }

    /// Reads the secrets of the secure channel, when the config has one, and connects
    /// to the server over that channel in `SignAndEncrypt`, or in `Sign` with
    /// `unencrypted = true`. Until the cancel, it subscribes to each node, or with
    /// `polled` reads each node once per tick of the rate. A frame holds the values
    /// that are Good and of the sample type of their channel (Boolean for `bool`,
    /// `SByte` to `UInt64` for `i8` to `u64`, Float for `f32`, and Double for `f64`).
    /// It leaves out each other value: an Uncertain one adds 1 to the count
    /// `uncertain`, another one that is not Good adds 1 to `bad`, and one of another
    /// type adds 1 to `mistyped`. The note names the first node of another type of the
    /// latest frame that has one. A later value of that node that is of its sample
    /// type, or that is not Good, clears it. A frame that leaves out each value is not
    /// written.
    ///
    /// The subscription samples each node once per period of the rate. The server sends
    /// the changes once per publishing interval: the period, but at least 100 ms and at
    /// most 1 min. Each change goes in a frame at the mesh time of its device time: its
    /// source timestamp, or its server timestamp when that is earlier or the source
    /// timestamp is absent, or the device time of the change before it of its node in
    /// one Publish answer when that is later, so the changes of a node keep their
    /// order. The run maps the clock of the server to mesh time from a read of its
    /// current time at the start, each 10 s, and at once when a server timestamp is
    /// after the arrival of its change, or the newest server timestamp of the changes
    /// is older than that of the changes before. One frame holds the
    /// changes of one device time, one value per channel. The queue of each node in the
    /// server holds the samples of two publishing intervals, or of two round trips of
    /// the first read when they are longer. A change after values that the server
    /// dropped from a full queue adds 1 to `missed`. When the server answers the
    /// subscribe with `BadServiceUnsupported` or `BadNotImplemented`, the run closes
    /// the session and polls as `polled` does on a new session until the cancel or an
    /// error, and the note says so.
    ///
    /// Each frame of a tick of `polled` has the midpoint of the read. When 10 s pass
    /// with no read, the run reads the state of the server to check the connection.
    ///
    /// After a cancel or an error, the run closes its session: it sends `CloseSession`,
    /// waits at most 1 s for the answer, then closes the connection. So a run ends
    /// within 1 s of a cancel. Call [`Context::clear_error`] once the run reaches the
    /// device: until then, the status shows the error of the last run.
    ///
    /// With no `read` block, the run connects, clears the error, and shows the note
    /// that the connector reads nothing and that `browse` and `import` add reads. Then
    /// it keeps the session and checks the connection as above until the cancel. It
    /// writes no frame.
    ///
    /// # Errors
    ///
    /// The errors of [`Context::secret`] for each source of the secure channel, and
    /// [`Error::Retry`] with the source when the client refuses a credential, so the
    /// next run reads the sources again. The error of [`Context::writer`] and of each
    /// failed write. [`Error::Config`] with `connector.index` for each channel that is
    /// an index; that error comes alone. Else [`Error::Config`] with
    /// `opcua.sample-type` for each channel whose sample type is not `bool`, `i8` to
    /// `u64`, `f32`, or `f64`, and, when it polls (with `polled` or after the
    /// fallback), with `opcua.large-read` when the Read of one node alone is past one
    /// message of the server. [`Error::Device`] when the connect, a read, or one check
    /// fails: a check fails on a bad service result or on no answer in 5 s. Also
    /// [`Error::Device`], at once, when the client loses its channel or its session
    /// between two ticks. With no `polled`, also [`Error::Device`] when a read of the
    /// current time or the subscribe fails, when the server does not watch a node, and
    /// when the subscription ends, and [`Error::Config`] with `opcua.large-watch` when
    /// the `CreateMonitoredItems` call of one node alone is past one message of the
    /// server, and with `opcua.no-timestamp` for a Good change with no timestamp that
    /// maps to mesh time.
    fn run(&self, ctx: Context<Config>) -> impl Future<Output = Result<(), Error>> {
        run(ctx)
    }

    /// Gives `read "<channel>" { node_id = "<tag>" }`, which `parse` reads. It reads
    /// no fact: the browse of the kind gives no `Write`, so each point gets a read
    /// block.
    ///
    /// # Errors
    ///
    /// `opcua.bad-node-id`, with the tag, when `tag` is not a node ID that `parse`
    /// reads.
    fn block(
        &self,
        tag: &str,
        _: &browse::Facts,
        channel: &Name,
    ) -> Result<Vec<Block>, Vec<Diagnostic>> {
        config::block(tag, channel)
    }

    /// Gives the node of the `node_id` of a `read` block in the form of a browse, so
    /// `"ns=0;i=2258"` gives `"i=2258"`. `None` for another block, or a `node_id`
    /// that `parse` refuses.
    fn tag(&self, block: &Block) -> Option<String> {
        config::tag(block)
    }

    /// Reads only `address` and the secure channel, as `parse` does, connects as `run`
    /// does, and walks `query` over the tree of the server from its Objects folder. An
    /// entry is a forward hierarchical reference to an Object, a View, or a Variable,
    /// with the browse name as its name and the node ID as its tag. An Object or a View
    /// is a folder. A Variable is a point, of the sample type of a built-in type from
    /// Boolean to Double, or of no type when its data type has no built-in base. It is
    /// unreadable when the server does not let the user read it, when its value rank is
    /// that of an array, or when its type is another built-in type. A node whose ID is
    /// a GUID, opaque, a name that is empty or not UTF-8, or of another server is
    /// unreadable. Its tag is the text of its ID, with `%` and each byte that is not
    /// UTF-8 as `%XX`, and `s%=` in place of the `s=` of a name, so `parse` refuses it.
    /// In one folder, a second reference to a node gives no entry, and a name that an
    /// earlier entry has gets ` (<tag>)` after it. A cursor of an earlier browse skips
    /// the references before its page again, so it assumes that the server keeps their
    /// order. Other than points at a count past 100 000 and those of the walk of a data
    /// type, it holds at most as many continuation points as the server holds for a
    /// session (`MaxBrowseContinuationPoints`, or 64 when the server gives 0, no value,
    /// or a value that is not a `UInt16`). The browse keeps at most 16 MiB of the tags
    /// and names of the references before its pages. At the bound of points, and past
    /// 16 MiB until the bytes fit, it releases the point that it used least lately, of
    /// those at a count of at most 100 000. Then it closes the session, as `run` does,
    /// which frees the point of a call that the walk dropped. The header of each
    /// request holds the mesh time of the node, which it reads before the connect.
    ///
    /// # Errors
    ///
    /// The errors of `run` for the secure channel. [`Error::Config`] with the
    /// diagnostics of `parse` for `address` and the secure channel, and with
    /// `opcua.bad-cursor` for a cursor that is not a count, or that skips more than 100
    /// 000 references. [`Error::Retry`] when the node has no mesh time, as
    /// [`browse::Context::time`] gives it. [`Error::Device`] when the connect, a
    /// Browse, a `BrowseNext`, or a Read fails, when the tags and names of a folder up
    /// to the end of a page, with those of the points that the browse cannot release,
    /// pass 16 MiB, and when a browse for a cursor of an earlier browse reads past
    /// [`browse::VISITS`] pages of the server with no references. The errors of
    /// [`browse::walk`].
    fn browse(
        &self,
        ctx: &browse::Context<'_>,
        query: &browse::Query,
    ) -> impl Future<Output = Result<browse::Found, Error>> {
        walk(ctx, query)
    }
}

async fn walk(
    ctx: &browse::Context<'_>,
    query: &browse::Query,
) -> Result<browse::Found, Error> {
    let address =
        config::endpoint(ctx.config()).map_err(|error| Error::Config(vec![error]))?;
    let security = config::security(ctx.config()).map_err(Error::Config)?;
    let secured =
        Secured::read(security.as_ref(), async |source| ctx.secret(source).await)
            .await?;
    let time = event::Time::mesh(ctx.clock().clone(), ctx.time().await?);
    let found = session(
        time,
        tracing::Span::none(),
        ctx.net().clone(),
        &mut ctx.rng(),
        &address,
        secured.as_ref(),
        future::pending(),
        async |client| browse::walk(&mut Server::new(client), query, ctx.clock()).await,
    )
    .await?;
    found.expect("invariant: a session with no end gives a result")
}

async fn run(ctx: Context<Config>) -> Result<(), Error> {
    let security = ctx.config().security.as_ref();
    let secured =
        Secured::read(security, async |source| ctx.secret(source).await).await?;
    if ctx.config().reads.is_empty() {
        return idle(&ctx, secured.as_ref()).await;
    }
    let Some(mut frames) = Frames::open(&ctx).await? else {
        return Ok(());
    };
    let ran = connect(
        &ctx,
        frames.writer.time(),
        secured.as_ref(),
        async |client| {
            if ctx.config().polled {
                read(&ctx, client, &mut frames).await.map(|()| None)
            } else {
                watch::run(&ctx, client, &mut frames).await
            }
        },
    )
    .await?;
    match ran.transpose()?.flatten() {
        Some(status) => fall_back(&ctx, &mut frames, secured.as_ref(), status).await,
        None => Ok(()),
    }
}

/// Notes that the server answered the subscribe with `status`, and polls as [`read`]
/// does on a new session, so that the client and the server keep nothing of the
/// subscribe.
#[cold]
async fn fall_back(
    ctx: &Context<Config>,
    frames: &mut Frames<'_>,
    secured: Option<&Secured<'_>>,
    status: Status,
) -> Result<(), Error> {
    frames.note.polls = Some(status);
    frames.note.show(ctx, None);
    let mesh = frames.writer.time();
    let ran = connect(ctx, mesh, secured, async |client| {
        read(ctx, client, frames).await
    })
    .await?;
    ran.unwrap_or(Ok(()))
}

/// Connects, notes [`NO_READ`], and keeps the session until the cancel. A writer
/// refuses no channel, so this run opens none.
async fn idle(
    ctx: &Context<Config>,
    secured: Option<&Secured<'_>>,
) -> Result<(), Error> {
    // The status channels opened, so the node has mesh time and this gives at once.
    let mesh = ctx.time().await;
    let ran = connect(ctx, mesh, secured, async |client| {
        ctx.clear_error();
        ctx.note(&NO_READ);
        let idle = client.idle(ctx.cancel().wait()).await;
        idle.map_err(|status| failed(client.url(), Call::Check, status))
    })
    .await?;
    ran.unwrap_or(Ok(()))
}

/// Runs `work` on a session to the server of `ctx` until its cancel, as [`session`]
/// does, with the mesh time `mesh`, over the channel of `secured`.
async fn connect<T>(
    ctx: &Context<Config>,
    mesh: hub::Time,
    secured: Option<&Secured<'_>>,
    work: impl AsyncFnOnce(&mut Client) -> T,
) -> Result<Option<T>, Error> {
    session(
        event::Time::mesh(ctx.clock().clone(), mesh),
        ctx.span().clone(),
        ctx.net().clone(),
        &mut ctx.rng(),
        &ctx.config().address,
        secured,
        ctx.cancel().wait(),
        work,
    )
    .await
}

/// The credentials of a secure channel, read from the sources of the config.
struct Secured<'a> {
    security: &'a Security,
    certificate: secret::Value,
    key: secret::Value,
    trust: secret::Value,
    /// The user of `security` and its password.
    user: Option<(&'a config::User, secret::Value)>,
}

impl<'a> Secured<'a> {
    /// Reads each source of `security` with `read`, which is the `secret` of a
    /// context. `None` with no `security`.
    async fn read(
        security: Option<&'a Security>,
        read: impl AsyncFn(&secret::Source) -> Result<secret::Value, Error>,
    ) -> Result<Option<Self>, Error> {
        let Some(security) = security else {
            return Ok(None);
        };
        let (certificate, key, trust) = (
            read(&security.certificate).await?,
            read(&security.key).await?,
            read(&security.trust).await?,
        );
        let user = match &security.user {
            Some(user) => Some((user, read(&user.password).await?)),
            None => None,
        };
        Ok(Some(Self {
            security,
            certificate,
            key,
            trust,
            user,
        }))
    }

    fn credentials(&self) -> Credentials<'_> {
        Credentials {
            certificate: self.certificate.expose(),
            key: self.key.expose(),
            trust: self.trust.expose(),
            user: (self.user.as_ref()).map(|(user, password)| client::User {
                name: &user.name,
                password: password.expose(),
            }),
        }
    }

    /// The [`Error::Retry`] of `refusal`, with the source of the credential that it
    /// names and its fix.
    fn refused(&self, refusal: Refusal) -> Error {
        let (source, fix) = match refusal.credential() {
            Credential::Certificate => (
                &self.security.certificate,
                "Give a DER or PEM certificate with an RSA key, the URI of the node in \
                 its subject alternative name, and a validity window that holds the \
                 mesh time",
            ),
            Credential::Key => (
                &self.security.key,
                "Give the DER or PEM private key of `certificate` in clear, with no \
                 password",
            ),
            Credential::Trust => (
                &self.security.trust,
                "Give the DER or PEM certificate of the server, or of the CA that \
                 issued it",
            ),
        };
        Error::Retry(format!("{source}: {refusal}. {fix}").into())
    }

    /// The cause of a server that refused the secure channel, with the names of the
    /// certificate to install.
    fn distrusted(&self) -> String {
        format!(
            "the server refused the secure channel; most likely it does not trust the \
             certificate of the node. Install the certificate {}{} in the trust list of \
             the server",
            self.security.certificate,
            self.names()
        )
    }

    /// The thumbprint and the subject of the certificate, as ` (SHA-1 thumbprint ...,
    /// subject ...)`, with no subject when it is empty, or empty when the certificate
    /// gives no names.
    fn names(&self) -> String {
        match client::names(self.certificate.expose()) {
            None => String::new(),
            Some(names) if names.subject.is_empty() => {
                format!(" (SHA-1 thumbprint {})", names.thumbprint)
            }
            Some(names) => format!(
                " (SHA-1 thumbprint {}, subject {})",
                names.thumbprint, names.subject
            ),
        }
    }
}

/// Connects to `address` with `time`, `span`, `net`, `rng`, and the credentials of
/// `secured`, as [`Client::connect`] does, gives the client to `work`, and then closes
/// the session. `None` when `until` ends before the connect.
///
/// # Errors
///
/// [`Error::Device`] with the address when the connect fails. [`Error::Retry`] with
/// the source when the client refuses a credential.
#[expect(clippy::too_many_arguments, reason = "each is an input of the connect")]
async fn session<T>(
    time: event::Time,
    span: tracing::Span,
    net: Net,
    rng: &mut Rng,
    address: &CStr,
    secured: Option<&Secured<'_>>,
    until: impl Future<Output = ()>,
    work: impl AsyncFnOnce(&mut Client) -> T,
) -> Result<Option<T>, Error> {
    let credentials =
        secured.map(|secured| (secured.credentials(), secured.security.mode));
    let security =
        (credentials.as_ref()).map(|(credentials, mode)| (credentials, *mode));
    let connected =
        Client::connect(time, span, net, rng, address, security, until).await;
    let connected = connected.map_err(|error| match error {
        client::Error::Failed(status) => device(Failure::Connect {
            address: address.to_string_lossy().into_owned(),
            status,
            cause: cause(status, secured),
        }),
        client::Error::Refused(refusal) => {
            let secured =
                secured.expect("invariant: a client with no credentials refuses none");
            secured.refused(refusal)
        }
    })?;
    let Some(mut client) = connected else {
        return Ok(None);
    };
    let done = work(&mut client).await;
    client.close().await;
    Ok(Some(done))
}

/// The likely cause, which ends with its fix, of a connect over `secured`, or in the
/// mode None when it is `None`, that failed with `status`. `None` when the status names
/// no cause. A server can send the trust, time, URI, and use statuses about the
/// certificate of the node too, so their causes name both certificates.
fn cause(status: Status, secured: Option<&Secured<'_>>) -> Option<String> {
    if status == Status::BAD_IDENTITY_TOKEN_REJECTED {
        return Some(unoffered(secured));
    }
    let secured = secured?;
    let (certificate, trust) = (&secured.security.certificate, &secured.security.trust);
    let cause = match status {
        Status::BAD_SECURITY_CHECKS_FAILED => secured.distrusted(),
        Status::BAD_USER_ACCESS_DENIED | Status::BAD_IDENTITY_TOKEN_INVALID => {
            let user = secured.security.user.as_ref()?;
            let (name, password) = (&user.name, &user.password);
            format!(
                "the server refused the user {name:?} with the password {password}. \
                 Check `user` and the password {password}"
            )
        }
        Status::BAD_CERTIFICATE_UNTRUSTED => format!(
            "the trust holds neither the certificate of the server nor the CA that \
             issued it, or the server does not trust the certificate of the node \
             {certificate}. Check the trust {trust}, and install the certificate \
             {certificate}{} in the trust list of the server",
            secured.names()
        ),
        Status::BAD_CERTIFICATE_TIME_INVALID => format!(
            "the mesh time is outside the validity window of the certificate of the \
             server, or the clock of the server is outside the window of the \
             certificate of the node {certificate}. Check the trust {trust} and the \
             clock of the server"
        ),
        Status::BAD_CERTIFICATE_ISSUER_TIME_INVALID => format!(
            "the mesh time is outside the validity window of the CA that issued the \
             certificate of the server, or the clock of the server is outside the \
             window of the CA that issued the certificate of the node {certificate}. \
             Check the trust {trust}, the clock of the server, and the window of the \
             CA that issued the certificate {certificate}"
        ),
        Status::BAD_CERTIFICATE_URI_INVALID => format!(
            "the URI of the certificate of the server is not the application URI of \
             its endpoint, or the server refused the URI of the certificate of the \
             node {certificate}. Check the trust {trust} and the certificate \
             {certificate}"
        ),
        Status::BAD_CERTIFICATE_USE_NOT_ALLOWED => format!(
            "the certificate of the server, or the certificate of the node \
             {certificate}, is the certificate of a CA. Check the trust {trust}, and \
             make the certificate of the node with `basicConstraints = CA:FALSE`"
        ),
        _ => return None,
    };
    Some(cause)
}

/// The cause of a server that offers no endpoint in the mode of `secured`, or in the
/// mode None when it is `None`, with its user, else an anonymous user, and a policy of
/// the client.
fn unoffered(secured: Option<&Secured<'_>>) -> String {
    let (user, with) = match secured.and_then(|secured| secured.user.as_ref()) {
        Some(_) => ("a user name and password", " with user names and passwords"),
        None => ("an anonymous user", ""),
    };
    let (mode, fix) = match secured.map(|secured| secured.security.mode) {
        None => {
            return "the server offers no endpoint in the mode None that lets in an \
                    anonymous user. For a server with only secure endpoints, give \
                    `certificate`, `key`, and `trust`"
                .into();
        }
        Some(client::Mode::SignAndEncrypt) => (
            "SignAndEncrypt",
            format!(
                "For a server with only Sign endpoints, set `unencrypted = true`; else \
                 turn on one of these policies{with} on the server"
            ),
        ),
        Some(client::Mode::Sign) => (
            "Sign",
            format!(
                "Turn on one of these policies in the mode Sign{with} on the server"
            ),
        ),
    };
    let [first, second, third] = client::POLICIES;
    format!(
        "the server offers no endpoint in the mode {mode} with the policy {first}, \
         {second}, or {third} that lets in {user}. {fix}"
    )
}

/// Reads each node once per tick until the cancel, and writes the frame of each tick.
async fn read(
    ctx: &Context<Config>,
    client: &mut Client,
    frames: &mut Frames<'_>,
) -> Result<(), Error> {
    ctx.clear_error();
    let reads = &ctx.config().reads;
    let nodes: Vec<Node> = reads.iter().map(|read| read.node.clone()).collect();
    let nodes = client.nodes(&nodes);
    if let Some(large) = nodes.large().first() {
        return Err(too_large(LARGE_READ, "Read", reads, large));
    }
    let missed = ctx.count("missed");
    let mut timer = pace::Timer::new(ctx.clock(), ctx.config().rate);
    loop {
        let idle = client.idle(timer.tick(ctx.cancel())).await;
        let Some(tick) =
            idle.map_err(|status| failed(client.url(), Call::Check, status))?
        else {
            break;
        };
        missed.add(tick.missed);
        let Some((read, stamp)) = ctx
            .cancel()
            .race(frames.writer.request(client.read(&nodes)))
            .await
        else {
            break;
        };
        let values = match read {
            Ok(values) => values,
            Err(status) => return Err(failed(client.url(), Call::Read, status)),
        };
        frames.write(values.iter().map(Some), stamp)?;
    }
    Ok(())
}

/// Writes `count` frames of the reads of `ctx` with no server, each value 1 in the
/// sample type of its channel (`true` for `bool`) but the first `bad(i)` of write `i`
/// (from 0), which are `BadNotFound` and left out, and each frame 1 ms of its clock
/// after the last, and gives what `measure` gives for the check of the values and the
/// write of each frame. `measure` calls the work that it gets once. For benchmarks.
///
/// # Errors
///
/// As [`Kind::run`](kind::Kind::run), with no connect or read.
///
/// # Panics
///
/// When the home refuses a frame for a cause that a run never gets: `Backwards` after
/// the stamp that a refusal gives, `Expired`, or `Codec`. When `ctx` is cancelled
/// before the writer opens.
#[cfg(feature = "sim")]
pub async fn writes(
    ctx: &Context<Config>,
    count: usize,
    bad: impl Fn(usize) -> usize,
    mut measure: impl FnMut(&mut dyn FnMut()) -> u64,
) -> Result<Vec<u64>, Error> {
    let mut frames = Frames::open(ctx)
        .await?
        .expect("invariant: a benchmark run is not cancelled before its writer opens");
    let mut measures = Vec::with_capacity(count);
    for write in 0..count {
        let bad = bad(write);
        let types = frames.types.iter().enumerate();
        let values: Vec<_> = types
            .map(|(at, &scalar)| {
                if at < bad {
                    Err(Fault::status(Status::BAD_NOT_FOUND))
                } else {
                    Ok(Value::one(scalar))
                }
            })
            .collect();
        ctx.clock().sleep(Span::from_nanos(1_000_000)).await;
        let ((), stamp) = frames.writer.request(std::future::ready(())).await;
        let mut written = Ok(());
        measures.push(measure(&mut || {
            written = frames.write(values.iter().map(Some), stamp);
        }));
        written?;
    }
    Ok(measures)
}

/// The frames of the ticks of a run: the value of each read that is Good and of the
/// sample type of its channel.
struct Frames<'a> {
    /// The context of the run, whose reads give the values of each frame, in order.
    ctx: &'a Context<Config>,
    writer: poll::Writer,
    /// The sample type of the channel of each read, in order.
    types: Vec<Scalar>,
    left: Left,
    note: Note,
}

/// The counts of the values that a frame leaves out.
struct Left {
    bad: status::Count,
    uncertain: status::Count,
    mistyped: status::Count,
}

/// The note of a run.
struct Note {
    /// The answer to the subscribe for which the run polls, which the note names
    /// first.
    polls: Option<Status>,
    /// The read and the type that the note names, or `None` when it names none.
    noted: Option<(usize, value::Type)>,
}

impl<'a> Frames<'a> {
    /// Opens the writer of the channels of the reads of `ctx`, and gives its frames,
    /// or `None` at the cancel of `ctx`.
    ///
    /// # Errors
    ///
    /// The error of [`poll::Writer::open`], which comes alone. Else [`Error::Config`]
    /// with `opcua.sample-type` for each channel whose sample type is not one of
    /// [`SCALARS`].
    async fn open(ctx: &'a Context<Config>) -> Result<Option<Self>, Error> {
        let reads = &ctx.config().reads;
        let channels = reads.iter().map(|read| read.channel.clone()).collect();
        let Some(writer) = poll::Writer::open(ctx, channels).await? else {
            return Ok(None);
        };
        let (mut types, mut wrong) = (Vec::with_capacity(reads.len()), Vec::new());
        for (read, data_type) in reads.iter().zip(writer.types()) {
            match data_type {
                Type::Scalar(scalar) if SCALARS.contains(&scalar) => types.push(scalar),
                _ => wrong.push(sample_type(read, data_type)),
            }
        }
        if !wrong.is_empty() {
            return Err(Error::Config(wrong));
        }
        Ok(Some(Self {
            ctx,
            writer,
            types,
            left: Left {
                bad: ctx.count("bad"),
                uncertain: ctx.count("uncertain"),
                mistyped: ctx.count("mistyped"),
            },
            note: Note {
                polls: None,
                noted: None,
            },
        }))
    }

    /// Writes one frame of `values`, the value or `None` of each read in order, at
    /// `stamp`, with each value that is Good and of the sample type of its channel. It
    /// counts each other value in [`Left`]. The note of the context names the first
    /// node of another type, and a frame with none keeps the note unless it has a
    /// value of the node of the note.
    ///
    /// # Errors
    ///
    /// As [`poll::Writer::write`].
    ///
    /// # Panics
    ///
    /// When `values` does not have one value or `None` for each read.
    fn write<'v>(
        &mut self,
        values: impl IntoIterator<Item = Option<&'v Result<Value, Fault>>>,
        stamp: Stamp,
    ) -> Result<(), Error> {
        let (mut bad, mut uncertain, mut mistyped) = (0, 0, 0);
        let mut first = None;
        let noted = self.note.noted.map(|(at, _)| at);
        let mut absent = false;
        let kept = values.into_iter().enumerate().map(|(at, value)| {
            let Some(value) = value else {
                absent |= noted == Some(at);
                return None;
            };
            let sample = self.types[at];
            match Why::of(value, sample) {
                Ok(value) => Some(value),
                Err(Why::Bad) => {
                    bad += 1;
                    None
                }
                Err(Why::Uncertain) => {
                    uncertain += 1;
                    None
                }
                Err(Why::Mistyped(value)) => {
                    mistyped += 1;
                    first = first.or(Some((at, value, sample)));
                    None
                }
            }
        });
        // `write` takes each value before it returns, so the counts are whole after it.
        let written = self.writer.write(kept, stamp);
        self.left.add(bad, uncertain, mistyped);
        if first.is_some() || !absent {
            self.note.set(self.ctx, first);
        }
        written
    }

    /// Clears the note when it names a read whose last value is not Good, by
    /// `faulted` of each read, as a frame with that value does.
    fn faulted(&mut self, faulted: &[bool]) {
        if self.note.noted.is_some_and(|(at, _)| faulted[at]) {
            self.note.set(self.ctx, None);
        }
    }
}

impl Left {
    /// Adds the counts of the values that one frame leaves out.
    #[inline]
    fn add(&self, bad: u64, uncertain: u64, mistyped: u64) {
        self.bad.add(bad);
        self.uncertain.add(uncertain);
        self.mistyped.add(mistyped);
    }
}

impl Note {
    /// Sets the note of `ctx` for `mistyped`, when it is not the note of the frame
    /// before: the read at a value of another type, its type, and the sample type of
    /// its channel, after the note of [`fall_back`] for `polls`. `None` shows only
    /// that note, or none.
    #[inline]
    fn set(
        &mut self,
        ctx: &Context<Config>,
        mistyped: Option<(usize, value::Type, Scalar)>,
    ) {
        if mistyped.map(|(at, value, _)| (at, value)) != self.noted {
            self.show(ctx, mistyped);
        }
    }

    #[cold]
    fn show(
        &mut self,
        ctx: &Context<Config>,
        mistyped: Option<(usize, value::Type, Scalar)>,
    ) {
        self.noted = mistyped.map(|(at, value, _)| (at, value));
        let polls = self.polls.map(|status| {
            format!("polls: the server has no subscriptions ({status:?})")
        });
        let polls = polls.as_deref().unwrap_or_default();
        let Some((at, value, sample)) = mistyped else {
            ctx.note(&polls);
            return;
        };
        let read = &ctx.config().reads[at];
        let gap = if polls.is_empty() { "" } else { "; " };
        ctx.note(&format_args!(
            "{polls}{gap}leaves out the values of the node `{}` of the channel `{}`: \
             they are of type {value}, and the channel has samples of type {}",
            read.node,
            read.channel,
            Type::Scalar(sample),
        ));
    }
}

/// Why a frame leaves out a value.
#[derive(Debug, PartialEq)]
enum Why {
    Bad,
    Uncertain,
    /// The value is Good, of this type, which is not the type of its channel.
    Mistyped(value::Type),
}

impl Why {
    /// The sample that a frame keeps of `value`, whose channel has samples of type
    /// `sample`, or why the frame leaves it out.
    fn of(value: &Result<Value, Fault>, sample: Scalar) -> Result<sample::Value, Self> {
        match value {
            Ok(value) if value.scalar() == sample => Ok(sample::Value::from(*value)),
            Ok(value) => Err(Self::Mistyped(value.type_of())),
            Err(fault) => Err(Self::fault(*fault)),
        }
    }

    /// Why a frame leaves out a value of `fault`.
    fn fault(fault: Fault) -> Self {
        match fault.cause() {
            Cause::Type(opc_type) => Self::Mistyped(opc_type),
            Cause::Status(status) if status.uncertain() => Self::Uncertain,
            Cause::Status(_) => Self::Bad,
        }
    }
}

/// The diagnostic of the channel of `read`, whose sample type `data_type` is not one
/// of [`SCALARS`].
fn sample_type(read: &Read, data_type: Type) -> Diagnostic {
    let names = SCALARS.map(|scalar| Type::Scalar(scalar).to_string());
    let (last, others) = names.split_last().expect("invariant: SCALARS has types");
    Diagnostic::new(
        SAMPLE_TYPE,
        None,
        format!(
            "the channel `{}` has samples of type {data_type}, and the opcua kind \
             reads only {}, and {last}",
            read.channel,
            others.join(", "),
        ),
        "Give the channel one of these sample types, or remove its `read` block".into(),
    )
}

/// The error `code` of a read of `reads` whose node alone needs a `call` past one
/// message of the server.
fn too_large(
    code: Code,
    call: &str,
    reads: &[Read],
    &Large { index, bytes, most }: &Large,
) -> Error {
    let read = &reads[index];
    Error::Config(vec![Diagnostic::new(
        code,
        None,
        format!(
            "a {call} of the node `{}` of the channel `{}` needs {bytes} bytes, past \
             the {most} bytes of one message of the server",
            read.node, read.channel,
        ),
        "Raise the message size of the server".into(),
    )])
}

/// A call to the server that a [`Failure::Client`] names.
#[derive(Debug)]
pub(crate) enum Call {
    Read,
    Check,
    Browse,
    Subscribe,
}

/// Why a run ended with [`Error::Device`].
#[derive(Debug)]
pub(crate) enum Failure {
    /// The connect to the server at `address` failed with `status`, for `cause`, which
    /// ends with its fix, when the status names one.
    Connect {
        address: String,
        status: Status,
        cause: Option<String>,
    },
    /// `call` to the server at `address` failed with `status`.
    Client {
        address: String,
        call: Call,
        status: Status,
    },
    /// The server does not watch `node` of the read of `channel`, with `status`.
    Unwatched {
        node: String,
        channel: String,
        status: Status,
    },
    /// The subscription to the server at `address` ended with `status`.
    Ended { address: String, status: Status },
    /// The tags and names before the pages of a browse pass [`crate::browse::BYTES`].
    Names,
    /// A browse for a cursor of an earlier browse reads past
    /// [`connector::browse::VISITS`] pages of the server with no references.
    Empty,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect {
                address,
                status,
                cause,
            } => {
                write!(f, "the connect to {address} failed with {}", status.name())?;
                match cause {
                    Some(cause) => write!(f, ": {cause}"),
                    None => Ok(()),
                }
            }
            Self::Client {
                address,
                call,
                status,
            } => {
                let call = match call {
                    Call::Read => "the read from",
                    Call::Check => "the connectivity check of",
                    Call::Browse => "the browse of",
                    Call::Subscribe => "the subscribe to",
                };
                write!(f, "{call} {address} failed with {}", status.name())
            }
            Self::Unwatched {
                node,
                channel,
                status,
            } => write!(
                f,
                "the server does not watch the node `{node}` of the channel \
                 `{channel}`: {}",
                status.name()
            ),
            Self::Ended { address, status } => write!(
                f,
                "the subscription to {address} ended with {}",
                status.name()
            ),
            Self::Names => write!(
                f,
                "the tags and names of the browse pass {} MiB",
                crate::browse::BYTES >> 20
            ),
            Self::Empty => write!(
                f,
                "the browse reads past {} pages with no references",
                connector::browse::VISITS
            ),
        }
    }
}

impl std::error::Error for Failure {}

pub(crate) fn device(failure: Failure) -> Error {
    Error::Device(Box::new(failure))
}

/// The [`Error::Device`] of `call` to the server at `address` that failed with
/// `status`.
pub(crate) fn failed(address: &CStr, call: Call, status: Status) -> Error {
    device(Failure::Client {
        address: address.to_string_lossy().into_owned(),
        call,
        status,
    })
}
