//! The `opcua` kind: reads OPC UA nodes at a fixed rate, and writes at most one frame
//! per tick.

use std::ffi::CStr;
use std::{fmt, future};

use connector::kind::{self, Channels, Context, Error};
use connector::{browse, pace, poll, status};
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
use crate::client::{Client, Large};
use crate::config::{self, Config, Read};
use crate::event;
use crate::ffi::Status;
use crate::node::Node;
use crate::value::{self, Cause, Fault, SCALARS, Value};

#[cfg(test)]
#[cfg(feature = "sim")]
mod tests;

const SAMPLE_TYPE: Code = Code::new("opcua.sample-type");
const LARGE_READ: Code = Code::new("opcua.large-read");

/// The note of a run with no `read` block.
const NO_READ: &str = "the connector has no `read` block, so it reads nothing. \
                       `browse` lists the points of the server, and `import` adds a \
                       `read` block for each.";

/// The counts of the status of each connector: the samples that the home applied,
/// the ticks that the timer skipped, the samples that a frame lost, and the values
/// that a frame leaves out as Bad, as Uncertain, and as of another type.
const COUNTS: [&str; 6] = {
    let [applied, lost] = poll::COUNTS;
    [applied, "missed", lost, "bad", "uncertain", "mistyped"]
};

/// The `opcua` kind: reads the Value of OPC UA nodes at a fixed rate, in Read requests
/// that each fit one message of the server, and writes at most one frame per tick.
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct Kind;

impl kind::Kind for Kind {
    type Config = Config;
    const SCHEMA: Body = config::BODY;

    /// Reads `address`, `rate`, and zero or more `read` blocks, each with the channel
    /// as its label and a `node_id`, and no other key or block.
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

    /// Connects to the server, and reads each node once per tick of the rate until
    /// the cancel. Each tick writes a frame of the values that are Good and of the
    /// sample type of their channel (Boolean for `bool`, `SByte` to `UInt64` for `i8`
    /// to `u64`, Float for `f32`, and Double for `f64`), stamped at the midpoint of the
    /// read. It leaves out each other value: an Uncertain one adds 1 to the count
    /// `uncertain`, another one that is not Good adds 1 to `bad`, and one of another
    /// type adds 1 to `mistyped`. The note names the first node of another type of
    /// the latest tick that has one, and a tick with none clears it. A tick that
    /// leaves out each value writes no frame. When 10 s pass with no read, the run
    /// reads the state of the server to check the connection. After a cancel or an
    /// error, the run closes its session: it sends `CloseSession`, waits at most 1 s
    /// for the answer, then closes the connection. So a run ends within 1 s of a
    /// cancel. Call [`Context::clear_error`] once the run reaches the device: until
    /// then, the status shows the error of the last run.
    ///
    /// With no `read` block, the run connects, clears the error, and shows the note
    /// that the connector reads nothing and that `browse` and `import` add reads. Then
    /// it keeps the session and checks the connection as above until the cancel. It
    /// writes no frame.
    ///
    /// # Errors
    ///
    /// The error of [`Context::writer`] and of each failed write. [`Error::Config`]
    /// with `connector.index` for each channel that is an index; that error comes
    /// alone. Else [`Error::Config`] with `opcua.sample-type` for each channel whose
    /// sample type is not `bool`, `i8` to `u64`, `f32`, or `f64`, and with
    /// `opcua.large-read` when the Read of one node alone is past one message of the
    /// server. [`Error::Device`] when the connect, a read, or one check fails: a check
    /// fails on a bad service result or on no answer in 5 s. Also [`Error::Device`],
    /// at once, when the client loses its channel or its session between two ticks.
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

    /// Reads only `address`, as `parse` does, connects as `run` does, and walks `query`
    /// over the tree of the server from its Objects folder. An entry is a forward
    /// hierarchical reference to an Object, a View, or a Variable, with the browse name
    /// as its name and the node ID as its tag. An Object or a View is a folder. A
    /// Variable is a point, of the sample type of a built-in type from Boolean to
    /// Double, or of no type when its data type has no built-in base. It is unreadable
    /// when the server does not let the user read it, when its value rank is that of an
    /// array, or when its type is another built-in type. A node whose ID is a GUID,
    /// opaque, a name that is empty or not UTF-8, or of another server is unreadable.
    /// Its tag is the text of its ID, with `%` and each byte that is not UTF-8 as
    /// `%XX`, and `s%=` in place of the `s=` of a name, so `parse` refuses it. In one
    /// folder, a second reference to a node gives no entry, and a name that an earlier
    /// entry has gets ` (<tag>)` after it. A cursor of an earlier browse skips the
    /// references before its page again, so it assumes that the server keeps their
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
    /// [`Error::Config`] with the diagnostic of `parse` for `address`, and with
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
    let time = event::Time::mesh(ctx.clock().clone(), ctx.time().await?);
    let found = session(
        time,
        tracing::Span::none(),
        ctx.net().clone(),
        &mut ctx.rng(),
        &address,
        future::pending(),
        async |client| browse::walk(&mut Server::new(client), query, ctx.clock()).await,
    )
    .await?;
    found.expect("invariant: a session with no end gives a result")
}

async fn run(ctx: Context<Config>) -> Result<(), Error> {
    if ctx.config().reads.is_empty() {
        return idle(&ctx).await;
    }
    let Some(mut frames) = Frames::open(&ctx).await? else {
        return Ok(());
    };
    let time = event::Time::mesh(ctx.clock().clone(), frames.writer.time());
    let ran = session(
        time,
        ctx.span().clone(),
        ctx.net().clone(),
        &mut ctx.rng(),
        &ctx.config().address,
        ctx.cancel().wait(),
        async |client| read(&ctx, client, &mut frames).await,
    )
    .await?;
    ran.unwrap_or(Ok(()))
}

/// Connects, notes [`NO_READ`], and keeps the session until the cancel. A writer
/// refuses no channel, so this run opens none.
async fn idle(ctx: &Context<Config>) -> Result<(), Error> {
    // The status channels opened, so the node has mesh time and this gives at once.
    let mesh = ctx.time().await;
    let ran = session(
        event::Time::mesh(ctx.clock().clone(), mesh),
        ctx.span().clone(),
        ctx.net().clone(),
        &mut ctx.rng(),
        &ctx.config().address,
        ctx.cancel().wait(),
        async |client| {
            ctx.clear_error();
            ctx.note(&NO_READ);
            let idle = client.idle(ctx.cancel().wait()).await;
            idle.map_err(|status| failed(client.url(), Call::Check, status))
        },
    )
    .await?;
    ran.unwrap_or(Ok(()))
}

/// Connects to `address` with `time`, `span`, `net`, and `rng`, as [`Client::connect`]
/// does, gives the client to `work`, and then closes the session. `None` when `until`
/// ends before the connect.
///
/// # Errors
///
/// [`Error::Device`] with the address when the connect fails.
async fn session<T>(
    time: event::Time,
    span: tracing::Span,
    net: Net,
    rng: &mut Rng,
    address: &CStr,
    until: impl Future<Output = ()>,
    work: impl AsyncFnOnce(&mut Client) -> T,
) -> Result<Option<T>, Error> {
    let connected = Client::connect(time, span, net, rng, address, None, until).await;
    let connected =
        connected.map_err(|status| failed(address, Call::Connect, status))?;
    let Some(mut client) = connected else {
        return Ok(None);
    };
    let done = work(&mut client).await;
    client.close().await;
    Ok(Some(done))
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
        return Err(large_read(reads, large));
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
        frames.write(values, stamp)?;
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
            written = frames.write(&values, stamp);
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
}

/// The counts of the values that a frame leaves out.
struct Left {
    bad: status::Count,
    uncertain: status::Count,
    mistyped: status::Count,
    /// The read and the type that the note names, or `None` when it shows none.
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
                noted: None,
            },
        }))
    }

    /// Writes one frame of `values`, the values of the reads in order, at `stamp`,
    /// with each value that is Good and of the sample type of its channel. It counts
    /// each other value in [`Left`], and the note of the context names the first node
    /// of another type.
    ///
    /// # Errors
    ///
    /// As [`poll::Writer::write`].
    ///
    /// # Panics
    ///
    /// When `values` does not have one value for each read.
    fn write(
        &mut self,
        values: &[Result<Value, Fault>],
        stamp: Stamp,
    ) -> Result<(), Error> {
        let (mut bad, mut uncertain, mut mistyped) = (0, 0, 0);
        let mut first = None;
        let kept = values.iter().enumerate().map(|(at, value)| {
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
        self.left.note(self.ctx, first);
        written
    }
}

impl Left {
    /// Adds the counts of the values that one frame leaves out.
    fn add(&self, bad: u64, uncertain: u64, mistyped: u64) {
        self.bad.add(bad);
        self.uncertain.add(uncertain);
        self.mistyped.add(mistyped);
    }

    /// Sets the note of `ctx` for `mistyped`, when it is not the note of the tick
    /// before: the read at a value of another type, its type, and the sample type of
    /// its channel. `None` clears the note.
    #[inline]
    fn note(
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
        let Some((at, value, sample)) = mistyped else {
            ctx.note(&"");
            return;
        };
        let read = &ctx.config().reads[at];
        ctx.note(&format_args!(
            "leaves out the values of the node `{}` of the channel `{}`: they are of \
             type {value}, and the channel has samples of type {}",
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
            Err(fault) => Err(match fault.cause() {
                Cause::Type(opc_type) => Self::Mistyped(opc_type),
                Cause::Status(status) if status.uncertain() => Self::Uncertain,
                Cause::Status(_) => Self::Bad,
            }),
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

/// The error of a read of `reads` whose node alone needs a Read past one message of the
/// server.
fn large_read(reads: &[Read], &Large { index, bytes, most }: &Large) -> Error {
    let read = &reads[index];
    Error::Config(vec![Diagnostic::new(
        LARGE_READ,
        None,
        format!(
            "a Read of the node `{}` of the channel `{}` needs {bytes} bytes, past the \
             {most} bytes of one message of the server",
            read.node, read.channel,
        ),
        "Raise the message size of the server".into(),
    )])
}

/// A call to the server that a [`Failure::Client`] names.
#[derive(Debug)]
pub(crate) enum Call {
    Connect,
    Read,
    Check,
    Browse,
}

/// Why a run ended with [`Error::Device`].
#[derive(Debug)]
pub(crate) enum Failure {
    /// `call` to the server at `address` failed with `status`.
    Client {
        address: String,
        call: Call,
        status: Status,
    },
    /// The tags and names before the pages of a browse pass [`crate::browse::BYTES`].
    Names,
    /// A browse for a cursor of an earlier browse reads past
    /// [`connector::browse::VISITS`] pages of the server with no references.
    Empty,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client {
                address,
                call,
                status,
            } => {
                let call = match call {
                    Call::Connect => "the connect to",
                    Call::Read => "the read from",
                    Call::Check => "the connectivity check of",
                    Call::Browse => "the browse of",
                };
                write!(f, "{call} {address} failed with {}", status.name())
            }
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
