//! The contract every connector kind implements, and the table of kinds.

use std::collections::BTreeMap;
use std::fmt;
use std::pin::Pin;
use std::rc::Rc;

use document::diagnostic::{Code, Diagnostic};
use document::schema::Body;
use document::{Block, Document, Span};
use env::clock::Clock;
use env::net::Net;
use env::rng::Rng;
use env::tasks::Tasks;
use types::authority::Authority;
use types::name::Name;
use types::time;

use crate::{browse, cancel, http, reader, status, supervisor};

/// The noun of the document that [`Kind::parse`] gets, for the text of a diagnostic.
pub const NOUN: &str = "the connector";

/// One connector kind: it owns its config's fields and how it runs. `node` builds one
/// value per kind for the life of the process; every connector of the kind shares it,
/// so it holds registries, dialers, and vendor libraries.
pub trait Kind: Send + Sync + 'static {
    /// The decoded config of one connector.
    type Config;

    /// The attributes and blocks of the config of one connector of the kind. `parse`
    /// refuses each one that it does not state.
    const SCHEMA: Body;

    /// Decodes a connector's config. Returns every problem, with positions.
    ///
    /// # Errors
    ///
    /// One diagnostic for each problem in `config`.
    fn parse(&self, config: &Document) -> Result<Self::Config, Vec<Diagnostic>>;

    /// Checks what the device can do and returns the channels the connector reads and
    /// writes.
    ///
    /// # Errors
    ///
    /// One diagnostic for each thing the device cannot do.
    fn check(&self, config: &Self::Config) -> Result<Channels, Vec<Diagnostic>>;

    /// Finds the devices that this node can reach, as connector blocks. The default
    /// gives [`Error::Config`] with `connector.no-discover`: the kind cannot find
    /// devices.
    ///
    /// # Errors
    ///
    /// The default gives [`Error::Config`] with `connector.no-discover`.
    fn discover(
        &self,
        cancel: &cancel::Token,
    ) -> impl Future<Output = Result<Vec<Document>, Error>> {
        let _: &cancel::Token = cancel;
        std::future::ready(Err(config_error(
            NO_DISCOVER,
            &"this connector kind cannot find its devices",
            "Write the connector block by hand",
        )))
    }

    /// Runs one connector until `ctx.cancel()` is cancelled or an error leaves it.
    /// Call [`Context::clear_error`] once the run reaches the device: until then, the
    /// status shows the error of the last run.
    fn run(
        &self,
        ctx: Context<Self::Config>,
    ) -> impl Future<Output = Result<(), Error>>;

    /// The blocks to add to the body of the connector block, so that it reads or
    /// writes the point `tag` as the channel `channel`. `tag` and `facts` are those of
    /// a [`browse::Shape::Point`] that a browse of the kind gave. The blocks hold only
    /// how the kind reaches the point: the `channel` block holds the sample type and
    /// the unit. A point whose `facts.access` is `Write` gets a write block. Each other
    /// point, `None` too, gets a read block, since a write to a device needs the
    /// user's choice. For each value that the kind needs and the browse does not give,
    /// the blocks hold [`fill`](crate::import::fill).
    ///
    /// # Errors
    ///
    /// The default gives `connector.no-import`.
    fn block(
        &self,
        tag: &str,
        facts: &browse::Facts,
        channel: &Name,
    ) -> Result<Vec<Block>, Vec<Diagnostic>> {
        let _: (&str, &browse::Facts, &Name) = (tag, facts, channel);
        Err(vec![Diagnostic::new(
            NO_IMPORT,
            None,
            "this connector kind cannot write the block of a point".into(),
            "Write the blocks of the points by hand".into(),
        )])
    }

    /// The tag of the point that `block`, a block of the body of a connector block of
    /// the kind, reads or writes, in the form that a browse of the kind gives it, or
    /// `None` when it names no point. A kind that gives blocks with [`Kind::block`]
    /// gives this too. The default gives `None`.
    fn tag(&self, block: &Block) -> Option<String> {
        let _: &Block = block;
        None
    }

    /// Browses the device of the connector block `ctx.config()` as `query` asks. A
    /// kind connects to its device and gives [`browse::walk`] over it. The default
    /// gives [`Error::Config`] with `connector.no-browse`: the kind cannot browse.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the block does not name a device that the kind can
    /// reach, and the errors of [`browse::walk`]. [`Error::Device`] when the device
    /// fails. The errors of [`browse::Context::secret`] for each secret that it reads.
    fn browse(
        &self,
        ctx: &browse::Context<'_>,
        query: &browse::Query,
    ) -> impl Future<Output = Result<browse::Found, Error>> {
        let _: (&browse::Context<'_>, &browse::Query) = (ctx, query);
        std::future::ready(Err(config_error(
            NO_BROWSE,
            &"this connector kind cannot browse its device",
            "Write the blocks of the points by hand",
        )))
    }
}

/// What a checked connector reads and writes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Channels {
    /// The settings of the reader of the channels it reads from the mesh: commands for
    /// the device, or samples it sends out. Get them from [`reader::read`]. The
    /// supervisor opens the reader under the connector's name before each run. `None`
    /// for a kind that reads no channel.
    pub reader: Option<reader::Settings>,
    /// The channels it writes to the mesh: samples from the device.
    pub writes: Vec<Name>,
    /// The last segment of each count channel of the kind,
    /// `<connector>.status.<count>`.
    pub counts: Vec<Name>,
}

/// Why a run or a discovery stopped. One handler for each variant.
#[derive(Debug)]
pub enum Error {
    /// Not retried: a browse or a discover fails, and a run stops until the next spec
    /// change.
    Config(Vec<Diagnostic>),
    /// The device or endpoint is in a bad state. Restarted with backoff.
    Device(Box<dyn std::error::Error + Send + Sync>),
    /// One attempt failed. A composition handles it; when one leaves `run`, it is
    /// restarted with backoff.
    Retry(Box<dyn std::error::Error + Send + Sync>),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(diagnostics) => {
                let mut separator = "";
                for diagnostic in diagnostics {
                    write!(f, "{separator}{diagnostic}")?;
                    separator = "; ";
                }
                Ok(())
            }
            Self::Device(source) => write!(f, "the device is in a bad state: {source}"),
            Self::Retry(source) => write!(f, "an attempt failed: {source}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(_) => None,
            Self::Device(source) | Self::Retry(source) => Some(source.as_ref()),
        }
    }
}

/// One run's capabilities. It is not `Send`: it stays on the supervisor's shard, so a
/// kind's own thread takes clones of the parts it needs.
#[derive(Debug)]
pub struct Context<C> {
    name: Name,
    config: C,
    cancel: cancel::Token,
    tasks: Tasks,
    status: status::Status,
    reader: Option<hub::reader::Reader>,
    // Its `tasks` field is the shard's, which no run counts: spawn only through
    // `self.tasks`.
    inputs: Rc<supervisor::Config>,
}

impl<C> Context<C> {
    /// Makes the context of one run, whose tasks spawn through `tasks`.
    pub(crate) fn new(
        name: Name,
        config: C,
        cancel: cancel::Token,
        tasks: Tasks,
        status: status::Status,
        reader: Option<hub::reader::Reader>,
        inputs: Rc<supervisor::Config>,
    ) -> Self {
        Self {
            name,
            config,
            cancel,
            tasks,
            status,
            reader,
            inputs,
        }
    }

    /// Gives the context `config`.
    fn with<D>(self, config: D) -> Context<D> {
        Context {
            name: self.name,
            config,
            cancel: self.cancel,
            tasks: self.tasks,
            status: self.status,
            reader: self.reader,
            inputs: self.inputs,
        }
    }

    /// The connector's name.
    #[must_use]
    pub fn name(&self) -> &Name {
        &self.name
    }

    /// The connector's config.
    #[must_use]
    pub fn config(&self) -> &C {
        &self.config
    }

    /// Cancelled when the run must stop, and when it has returned.
    #[must_use]
    pub fn cancel(&self) -> &cancel::Token {
        &self.cancel
    }

    /// The node's clock.
    #[must_use]
    pub fn clock(&self) -> &Clock {
        &self.inputs.clock
    }

    /// A new random source, seeded from the node's entropy, that simulation replays.
    #[must_use]
    pub fn rng(&self) -> Rng {
        self.inputs.entropy.rng()
    }

    /// Opens a writer session on `channels` as this connector: the session's subject
    /// is the connector's name. Waits while the mesh names no home for the index of
    /// `channels`, and gives `None` once [`Self::cancel`] is cancelled, also when it
    /// was cancelled before the call. A kind writes
    /// `let Some(writer) = ctx.writer(..).await? else { return Ok(()) };`.
    ///
    /// Return a failed write with `?`: KIND TABLE gives its class. After
    /// [`hub::writer::Failure::Removed`], the next run opens its writer by name, so a
    /// channel that the spec defined again with a new key is written. The frame of that
    /// write is lost, and so is each sample of the device until the next run opens its
    /// writer.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] with one diagnostic and no span, for a writer that only a
    /// spec change fixes: `connector.unknown-channel` for a name that no channel has,
    /// `connector.no-channel` for no name, and `connector.lease` for a lease that is
    /// not longer than zero. [`Error::Retry`] with the hub's error when the home of an
    /// index is another node, or the mesh stopped.
    ///
    /// # Panics
    ///
    /// When the hub gives an error that this open cannot get.
    pub async fn writer(
        &self,
        channels: Vec<Name>,
        authority: Authority,
        lease: Option<time::Span>,
    ) -> Result<Option<hub::writer::Writer>, Error> {
        let config = hub::writer::Config {
            subject: self.name.clone(),
            authority,
            lease,
            channels,
        };
        let opened = self.cancel.race(self.inputs.hub.writer(config)).await;
        opened.transpose().map_err(writer_error)
    }

    /// Takes the reader that the supervisor opened for this run, under the connector's
    /// name, from the settings that `check` gave. `None` when `check` gave none, or
    /// after the first call. Return the end of the reader with `?`: KIND TABLE gives
    /// its class. After [`hub::reader::Ended::Removed`], the supervisor opens a new
    /// reader from the selector before the next run, so a channel that the spec defined
    /// again with a new key is read, and the open gives `Config` when the selector
    /// matches nothing.
    /// A reader with a hold opens again at its held position while its index keeps its
    /// key, and reads the frames after it from disk. Past a frame that the buffer lost
    /// for no room, the next run reads the frame after it. Each close starts the hold
    /// again, so a hold longer than the wait between two runs never ends.
    pub fn reader(&mut self) -> Option<hub::reader::Reader> {
        self.reader.take()
    }

    /// The count `name` of the connector's status, `<connector>.status.<name>`, to
    /// set from the kind's data path. The call searches the names of the counts, so
    /// take each count once, before the data path.
    ///
    /// # Panics
    ///
    /// When the kind's `check` did not name `name` as a count.
    #[must_use]
    pub fn count(&self, name: &str) -> status::Count {
        self.status.count(name)
    }

    /// Shows `error` in the status of the running connector, as an attempt that
    /// failed with the class `retry`. The status gives it within one second. The end
    /// of the run replaces it, and a call after the run returned shows nothing. Each
    /// time the status takes a show that changes its text or its class, it also
    /// writes the log line `error` (WARN).
    pub fn error(&self, error: &dyn fmt::Display) {
        self.status.show(error);
    }

    /// Clears the class and the text of the error in the status of the running
    /// connector, within one second: the one that [`Context::error`] showed, or the
    /// end of the last run. Until the first call of `error` or `clear_error` in a
    /// run, the status keeps the end of the last run. A call after the run returned
    /// shows nothing. Each time the status takes a clear that changes its text or its
    /// class, it also writes the log line `cleared` (INFO).
    pub fn clear_error(&self) {
        self.status.clear();
    }

    /// Shows `note` in the status of the running connector until the run ends or the
    /// next call: a short text of how the run works that the user must know, such as a
    /// mode that the device forces or a config that reads nothing. The status gives it
    /// within one second. A note of more than 1024 bytes keeps its first and last 510
    /// bytes or less at char boundaries, joined by `…`. An empty `note` shows none. The
    /// end of the run clears it, and a call after the run returned shows nothing. Each
    /// time the status takes a note that changes its note, it also writes the log line
    /// `note` (INFO).
    pub fn note(&self, note: &dyn fmt::Display) {
        self.status.note(note);
    }

    /// Connects streams and datagrams.
    #[must_use]
    pub fn net(&self) -> &Net {
        &self.inputs.net
    }

    /// The roots that an `https` connect trusts, as `supervisor::Config::roots`
    /// gives them.
    #[must_use]
    pub fn roots(&self) -> &http::Roots {
        &self.inputs.roots
    }

    /// The mesh time of the run's hub, which stamps samples. Waits for the node's first
    /// mesh time. Dropping the future ends the wait, so race it with
    /// [`Context::cancel`]. [`Context::clock`] is the clock of deadlines.
    pub fn time(&self) -> impl Future<Output = hub::Time> {
        self.inputs.hub.time()
    }

    /// Reads the value of `source`. Each call reads the file or the store again, so
    /// a kind that reads it at the start of each run sees a value changed there at
    /// its next run. An `env()` gives the value of the node's start. Send its bytes
    /// only to the system they unlock, and never into a status or a log.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] with code `connector.bad-secret-name` when the store cannot
    /// hold the name of a `secret()`, with the store's text as the fix.
    /// [`Error::Retry`] for each other error, with a text that names the source and
    /// holds the fix, such as where to put the value. The supervisor restarts the
    /// run with backoff, so a value put in its place heals it. For `env()`, restart
    /// the node with the variable set.
    pub async fn secret(
        &self,
        source: &secret::Source,
    ) -> Result<secret::Value, Error> {
        read_secret(&self.inputs.secrets, source).await
    }

    /// Runs the kind's own tasks on its shard. Each task must end when
    /// [`Context::cancel`] is cancelled. The next run of the same
    /// [`Opened::run`](crate::supervisor::Opened::run) call starts only after each of
    /// them ends.
    #[must_use]
    pub fn tasks(&self) -> &Tasks {
        &self.tasks
    }

    /// The span `connector` of this run, with the fields `connector` and `kind`. Give
    /// it as the parent of each event of the kind
    /// (`tracing::warn!(parent: ctx.span(), ..)`), so its line names the connector. No
    /// poll enters it. A task spawned through [`Context::tasks`] that writes an event
    /// takes a clone of it.
    #[must_use]
    pub fn span(&self) -> &tracing::Span {
        self.status.span()
    }
}

/// The value of `source` from `secrets`, with the errors of [`Context::secret`].
pub(crate) async fn read_secret(
    secrets: &secret::Resolver,
    source: &secret::Source,
) -> Result<secret::Value, Error> {
    let get = secrets.get(source).await;
    get.map_err(|error| match error {
        secret::Error::BadName { detail } => {
            let error =
                format!("the secret {source} has a name that the store cannot hold");
            config_error(BAD_SECRET_NAME, &error, &detail)
        }
        error => {
            let source = source.clone();
            Error::Retry(Box::new(Unread { source, error }))
        }
    })
}

/// The kinds this binary has, by name. `node` builds it.
#[derive(Default)]
pub struct Table {
    kinds: BTreeMap<&'static str, Box<dyn Erased>>,
}

impl Table {
    /// Makes a table with no kinds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `kind` as `name`.
    ///
    /// # Panics
    ///
    /// When `name` is already in the table: the table is a literal in `node`.
    #[must_use]
    pub fn with(mut self, name: &'static str, kind: impl Kind) -> Self {
        let old = self.kinds.insert(name, Box::new(kind));
        assert!(old.is_none(), "the kind {name:?} is in the table twice");
        self
    }

    /// Gives the name of each kind, in byte order.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.kinds.keys().copied()
    }

    /// Each kind, by name, with the body of its config, in byte order of the names.
    pub fn schemas(&self) -> impl Iterator<Item = (&'static str, &'static Body)> + '_ {
        self.kinds.iter().map(|(name, kind)| (*name, kind.schema()))
    }

    /// Parses and checks one connector's config.
    ///
    /// # Errors
    ///
    /// The diagnostics of the kind's `parse` or `check`, each with no span placed at
    /// `at`, where the file names the kind. An unknown kind gives one diagnostic,
    /// `connector.unknown-kind` at `at`.
    ///
    /// # Panics
    ///
    /// When the kind names a count that [`status`] names in any case, a count of more
    /// than one segment, or one count twice in any case: the kind's code is wrong.
    pub fn check(
        &self,
        kind: &str,
        at: Option<Span>,
        config: &Document,
    ) -> Result<Channels, Vec<Diagnostic>> {
        let place = |mut diagnostics: Vec<Diagnostic>| {
            for diagnostic in &mut diagnostics {
                diagnostic.span = diagnostic.span.or(at);
            }
            diagnostics
        };
        let channels = self.get(kind, at)?.check(config).map_err(place)?;
        status::check(kind, &channels.counts);
        Ok(channels)
    }

    /// Finds the devices of `kind` that this node can reach, as connector blocks.
    ///
    /// # Errors
    ///
    /// The kind's error, or [`Error::Config`] with `connector.unknown-kind`.
    ///
    /// The future is not `Send`: call it on a shard.
    pub async fn discover(
        &self,
        kind: &str,
        cancel: &cancel::Token,
    ) -> Result<Vec<Document>, Error> {
        self.get(kind, None)
            .map_err(Error::Config)?
            .discover(cancel)
            .await
    }

    /// The blocks of `kind` that read or write the point `tag` as `channel`, as
    /// [`Kind::block`] gives them.
    ///
    /// # Errors
    ///
    /// The kind's diagnostics, or `connector.unknown-kind`.
    pub fn block(
        &self,
        kind: &str,
        tag: &str,
        facts: &browse::Facts,
        channel: &Name,
    ) -> Result<Vec<Block>, Vec<Diagnostic>> {
        self.get(kind, None)?.block(tag, facts, channel)
    }

    /// The tag of the point that `block` of a connector block of `kind` reads or
    /// writes, as [`Kind::tag`] gives it.
    ///
    /// # Errors
    ///
    /// `connector.unknown-kind`.
    pub fn tag(
        &self,
        kind: &str,
        block: &Block,
    ) -> Result<Option<String>, Vec<Diagnostic>> {
        Ok(self.get(kind, None)?.tag(block))
    }

    /// Starts a browse with `kind` as `query` asks.
    pub(crate) fn browse<'a>(
        &'a self,
        kind: &str,
        ctx: &'a browse::Context<'a>,
        query: &'a browse::Query,
    ) -> Result<Browse<'a>, Vec<Diagnostic>> {
        Ok(self.get(kind, None)?.browse(ctx, query))
    }

    /// Parses `config` and starts one run of `kind` with `ctx`.
    pub(crate) fn run<'a>(
        &'a self,
        kind: &str,
        config: &Document,
        ctx: Context<()>,
    ) -> Result<Run<'a>, Vec<Diagnostic>> {
        self.get(kind, None)?.run(config, ctx)
    }

    fn get(
        &self,
        kind: &str,
        at: Option<Span>,
    ) -> Result<&dyn Erased, Vec<Diagnostic>> {
        let erased = self.kinds.get(kind).ok_or_else(|| {
            let names: Vec<_> = self.kinds.keys().copied().collect();
            let fix = if names.is_empty() {
                "Use a build that has connector kinds".into()
            } else {
                format!("Use one of {names:?}")
            };
            vec![Diagnostic::new(
                UNKNOWN_KIND,
                at,
                format!("this build has no connector kind {kind:?}"),
                fix,
            )]
        })?;
        Ok(erased.as_ref())
    }
}

impl fmt::Debug for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.kinds.keys()).finish()
    }
}

const UNKNOWN_KIND: Code = Code::new("connector.unknown-kind");
const NO_BROWSE: Code = Code::new("connector.no-browse");
const NO_DISCOVER: Code = Code::new("connector.no-discover");
const NO_IMPORT: Code = Code::new("connector.no-import");
const UNKNOWN_CHANNEL: Code = Code::new("connector.unknown-channel");
const NO_CHANNEL: Code = Code::new("connector.no-channel");
const MANY_INDEXES: Code = Code::new("connector.many-indexes");
const LEASE: Code = Code::new("connector.lease");
const LARGE_FRAME: Code = Code::new("connector.large-frame");
const BAD_SECRET_NAME: Code = Code::new("connector.bad-secret-name");
/// The fix of [`LARGE_FRAME`].
const FEWER_CHANNELS: &str = "Give the connector fewer channels";

/// A secret whose source gave no value.
#[derive(Debug)]
struct Unread {
    source: secret::Source,
    error: secret::Error,
}

impl fmt::Display for Unread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the secret {}: {}", self.source, self.error)
    }
}

impl std::error::Error for Unread {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// The hub's `Full`, with the fixes that a spec can make.
#[derive(Debug)]
struct Full(hub::reader::Error);

impl fmt::Display for Full {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: read the index from fewer connectors in complete mode, set `mode = \
             \"latest\"` on one, or wait for the hold of a removed reader to end",
            self.0
        )
    }
}

impl std::error::Error for Full {}

/// The class of `error` of the open of a named reader.
///
/// # Panics
///
/// For `Unsynced`, which the supervisor's open never gives, and for an error of a
/// reader away from its home, since a named reader opens only at its home.
pub(crate) fn reader_error(error: hub::reader::Error) -> Error {
    use hub::reader::Error as Hub;
    let (code, fix) = match error {
        Hub::Empty => (NO_CHANNEL, "Select a channel that the spec defines"),
        Hub::ManyIndexes { .. } => (MANY_INDEXES, "Select channels of one index"),
        Hub::Full => return Error::Retry(Box::new(Full(error))),
        Hub::Remote { .. } | Hub::Mesh(_) | Hub::Unrecorded => {
            return Error::Retry(Box::new(error));
        }
        Hub::Unsynced => panic!("invariant: a reader opened before mesh time: {error}"),
        Hub::Transport(_) | Hub::Refused(_) | Hub::Message(_) | Hub::Pool(_) => {
            panic!("invariant: a named reader away from its home gave: {error}")
        }
    };
    config_error(code, &error, fix)
}

/// The class of `error` of a writer.
///
/// # Panics
///
/// For `Unsynced`: the hub waits for mesh time.
fn writer_error(error: hub::writer::Error) -> Error {
    use hub::home::writer::Error as Home;
    use hub::writer::Error as Hub;
    let (code, fix) = match error {
        Hub::Unknown(_) => (UNKNOWN_CHANNEL, "Define the channel in the spec"),
        Hub::Empty => (NO_CHANNEL, "Give the connector a channel to write"),
        Hub::Home(home @ Home::Lease { .. }) => (LEASE, home.fix()),
        Hub::Remote { .. } | Hub::Mesh(_) => return Error::Retry(Box::new(error)),
        Hub::Home(Home::Unsynced) => {
            panic!("invariant: a writer of the hub gave: {error}")
        }
    };
    config_error(code, &error, fix)
}

/// The class of the end of the reader of a run, for a kind that returns it with `?`:
/// `Retry` for `Removed`, `Replaced`, `Buffer`, and `Pool`.
///
/// # Panics
///
/// For an end that only a reader away from its home gives, because a named reader opens
/// only at its home.
#[expect(
    clippy::fallible_impl_from,
    reason = "each panic is an end that no run gets"
)]
impl From<hub::reader::Ended> for Error {
    fn from(ended: hub::reader::Ended) -> Self {
        use hub::reader::Ended;
        match ended {
            Ended::Removed(_) | Ended::Replaced | Ended::Buffer(_) | Ended::Pool(_) => {
                Self::Retry(Box::new(ended))
            }
            Ended::Stream(_)
            | Ended::Refused(_)
            | Ended::Message(_)
            | Ended::Frame(_)
            | Ended::Credit { .. } => {
                panic!("invariant: a named reader away from its home gave: {ended}")
            }
        }
    }
}

/// The class of a failed write of a run, for a kind that returns it with `?`: `Config`
/// (`connector.large-frame`) for `Home(Large)`, and `Retry` for `Removed`,
/// `Home(Full)`, and `Home(Disk)`.
///
/// # Panics
///
/// For [`hub::home::Error::Resend`], because a kind labels no frame resend.
#[expect(clippy::fallible_impl_from, reason = "a kind labels no frame resend")]
impl From<hub::writer::Failure> for Error {
    fn from(failure: hub::writer::Failure) -> Self {
        use hub::home::Error as Home;
        use hub::writer::Failure;
        match failure {
            Failure::Removed(_) | Failure::Home(Home::Full | Home::Disk(_)) => {
                Self::Retry(Box::new(failure))
            }
            Failure::Home(Home::Large) => {
                config_error(LARGE_FRAME, &failure, FEWER_CHANNELS)
            }
            Failure::Home(Home::Resend) => {
                panic!("invariant: a kind labels no frame resend")
            }
        }
    }
}

/// The class of a failed draft of a frame of a run, for a kind that returns it with
/// `?`: `Config` (`connector.large-frame`) for a pool `TooLarge`, because only a spec
/// change or a kind that splits its frames fixes it, and `Retry` for a pool
/// `Exhausted` and `Refused`.
///
/// # Panics
///
/// For `OutOfRange`, `Unordered`, and `IndexAbsent`, because a kind gives its series
/// from its own code, and for `End`, which a draft never gives.
#[expect(
    clippy::fallible_impl_from,
    reason = "each panic is a defect of the kind's series or an end that no draft gives"
)]
impl From<types::frame::Error> for Error {
    fn from(error: types::frame::Error) -> Self {
        use types::frame::Error as Frame;
        match error {
            Frame::Pool(
                block::Error::Exhausted { .. } | block::Error::Refused { .. },
            ) => Self::Retry(Box::new(error)),
            Frame::Pool(block::Error::TooLarge { .. }) => {
                config_error(LARGE_FRAME, &error, FEWER_CHANNELS)
            }
            Frame::OutOfRange { .. }
            | Frame::Unordered { .. }
            | Frame::IndexAbsent { .. } => {
                panic!("invariant: the series follow the key set: {error}")
            }
            Frame::End(_) => {
                panic!("invariant: a draft gives no end of another node: {error}")
            }
        }
    }
}

/// [`Error::Config`] with one diagnostic and no span, whose message is `error`.
pub(crate) fn config_error(code: Code, error: &dyn fmt::Display, fix: &str) -> Error {
    Error::Config(vec![Diagnostic::new(
        code,
        None,
        error.to_string(),
        fix.into(),
    )])
}

/// One run of a kind.
pub(crate) type Run<'a> = Pin<Box<dyn Future<Output = Result<(), Error>> + 'a>>;

/// One browse of a kind.
pub(crate) type Browse<'a> =
    Pin<Box<dyn Future<Output = Result<browse::Found, Error>> + 'a>>;

/// A [`Kind`] with its config type erased, so one table holds every kind.
trait Erased: Send + Sync {
    fn schema(&self) -> &'static Body;

    fn check(&self, config: &Document) -> Result<Channels, Vec<Diagnostic>>;

    fn discover<'a>(
        &'a self,
        cancel: &'a cancel::Token,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Document>, Error>> + 'a>>;

    fn run(
        &self,
        config: &Document,
        ctx: Context<()>,
    ) -> Result<Run<'_>, Vec<Diagnostic>>;

    fn browse<'a>(
        &'a self,
        ctx: &'a browse::Context<'a>,
        query: &'a browse::Query,
    ) -> Browse<'a>;

    fn block(
        &self,
        tag: &str,
        facts: &browse::Facts,
        channel: &Name,
    ) -> Result<Vec<Block>, Vec<Diagnostic>>;

    fn tag(&self, block: &Block) -> Option<String>;
}

impl<K: Kind> Erased for K {
    fn schema(&self) -> &'static Body {
        const { &K::SCHEMA }
    }

    fn check(&self, config: &Document) -> Result<Channels, Vec<Diagnostic>> {
        Kind::check(self, &self.parse(config)?)
    }

    fn discover<'a>(
        &'a self,
        cancel: &'a cancel::Token,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Document>, Error>> + 'a>> {
        Box::pin(Kind::discover(self, cancel))
    }

    fn run(
        &self,
        config: &Document,
        ctx: Context<()>,
    ) -> Result<Run<'_>, Vec<Diagnostic>> {
        let config = self.parse(config)?;
        Ok(Box::pin(Kind::run(self, ctx.with(config))))
    }

    fn browse<'a>(
        &'a self,
        ctx: &'a browse::Context<'a>,
        query: &'a browse::Query,
    ) -> Browse<'a> {
        Box::pin(Kind::browse(self, ctx, query))
    }

    fn block(
        &self,
        tag: &str,
        facts: &browse::Facts,
        channel: &Name,
    ) -> Result<Vec<Block>, Vec<Diagnostic>> {
        Kind::block(self, tag, facts, channel)
    }

    fn tag(&self, block: &Block) -> Option<String> {
        Kind::tag(self, block)
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use document::value::{self, Value};
    use document::{Attribute, Map, Position, Source};

    use std::cell::RefCell;

    use super::*;
    use crate::cancel::Token;
    use crate::common::{create_config, run, run_on};
    use crate::testing;

    const MISSING: Code = Code::new("test.missing");
    const RANGE: Code = Code::new("test.range");

    fn ms(n: i64) -> types::time::Span {
        types::time::Span::from_nanos(n * 1_000_000)
    }

    fn name(text: &str) -> Name {
        text.parse().expect("a valid name")
    }

    /// A kind whose config is one integer attribute `n`. A device takes `n` up to 8.
    struct Counter;

    impl Kind for Counter {
        type Config = i128;
        const SCHEMA: document::schema::Body = crate::testing::READS;

        fn parse(&self, config: &Document) -> Result<i128, Vec<Diagnostic>> {
            match config.attributes.get("n").map(|a| &a.value) {
                Some(Value {
                    kind: value::Kind::Integer(n),
                    ..
                }) => Ok(*n),
                Some(value) => Err(vec![Diagnostic::new(
                    MISSING,
                    value.span,
                    "n is not an integer".into(),
                    "Fix it".into(),
                )]),
                None => Err(vec![diagnostic(MISSING, "no integer n")]),
            }
        }

        fn check(&self, n: &i128) -> Result<Channels, Vec<Diagnostic>> {
            if *n > 8 {
                return Err(vec![diagnostic(RANGE, "n is over 8")]);
            }
            Ok(Channels {
                reader: None,
                writes: (0..*n).map(|i| name(&format!("counter.c{i}"))).collect(),
                counts: vec![name("frames")],
            })
        }

        fn discover(
            &self,
            _: &cancel::Token,
        ) -> impl Future<Output = Result<Vec<Document>, Error>> {
            std::future::ready(Ok(vec![config(2)]))
        }

        async fn run(&self, ctx: Context<i128>) -> Result<(), Error> {
            ctx.cancel().wait().await;
            Ok(())
        }
    }

    fn diagnostic(code: Code, message: &str) -> Diagnostic {
        Diagnostic::new(code, None, message.into(), "Fix it".into())
    }

    fn config(n: i128) -> Document {
        let n = Attribute {
            key: "n".into(),
            key_span: None,
            value: Value {
                kind: value::Kind::Integer(n),
                span: None,
            },
        };
        let attributes = Map::new(vec![n]).expect("one key");
        Document {
            attributes,
            blocks: Vec::new(),
        }
    }

    fn table() -> Table {
        Table::new().with("counter", Counter)
    }

    #[test]
    fn checks_a_config_through_its_kind() {
        let channels = table().check("counter", None, &config(2));
        let writes = vec![name("counter.c0"), name("counter.c1")];
        assert_eq!(
            channels,
            Ok(Channels {
                reader: None,
                writes,
                counts: vec![name("frames")],
            })
        );
    }

    #[test]
    fn returns_the_diagnostics_of_parse() {
        let result = table().check("counter", None, &Document::default());
        assert_eq!(result, Err(vec![diagnostic(MISSING, "no integer n")]));
    }

    #[test]
    fn returns_the_diagnostics_of_check() {
        let result = table().check("counter", None, &config(9));
        assert_eq!(result, Err(vec![diagnostic(RANGE, "n is over 8")]));
    }

    fn unknown(kind: &str, names: &str) -> Diagnostic {
        Diagnostic::new(
            UNKNOWN_KIND,
            None,
            format!("this build has no connector kind \"{kind}\""),
            format!("Use one of {names}"),
        )
    }

    #[test]
    fn names_the_known_kinds_for_an_unknown_kind() {
        let table = table().with("other", Counter);
        let expected = unknown("modbus", "[\"counter\", \"other\"]");
        assert_eq!(table.check("modbus", None, &config(1)), Err(vec![expected]));
    }

    #[test]
    fn gives_the_name_of_each_kind_in_byte_order() {
        let table = Table::new().with("other", Counter).with("counter", Counter);
        assert_eq!(table.names().collect::<Vec<_>>(), ["counter", "other"]);
        assert_eq!(Table::new().names().count(), 0);
    }

    /// A kind that takes no config, with a body of its own.
    struct Empty;

    impl Kind for Empty {
        type Config = ();
        const SCHEMA: Body = Body {
            summary: "Takes no config.",
            attributes: &[],
            blocks: &[],
            examples: &[""],
        };

        fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
            Ok(())
        }

        fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
            Ok(Channels::default())
        }

        fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), Error>> {
            std::future::ready(Ok(()))
        }
    }

    #[test]
    fn gives_the_body_of_each_kind_by_name_in_byte_order() {
        let table = Table::new().with("other", Empty).with("counter", Counter);
        let schemas: Vec<_> = table.schemas().collect();
        let expected = [("counter", &testing::READS), ("other", &Empty::SCHEMA)];
        assert_eq!(schemas, expected);
        assert_eq!(Table::new().schemas().count(), 0);
    }

    /// The span from `start` to `end` on line 0 of source 2.
    fn span(start: u32, end: u32) -> Option<Span> {
        let position = |offset| Position {
            offset,
            line: 0,
            column: offset,
        };
        Span::new(Source(2), position(start), position(end))
    }

    #[test]
    fn puts_an_unknown_kind_where_the_file_names_it() {
        let at = span(7, 15);
        let mut expected = unknown("modbus", "[\"counter\"]");
        expected.span = at;
        assert_eq!(table().check("modbus", at, &config(1)), Err(vec![expected]));
    }

    #[test]
    fn places_each_diagnostic_of_the_kind_with_no_span_at_the_kind() {
        let at = span(7, 15);
        let placed = |mut diagnostic: Diagnostic| {
            diagnostic.span = at;
            diagnostic
        };
        let missing = placed(diagnostic(MISSING, "no integer n"));
        let range = placed(diagnostic(RANGE, "n is over 8"));
        let table = table();
        assert_eq!(
            table.check("counter", at, &Document::default()),
            Err(vec![missing])
        );
        assert_eq!(table.check("counter", at, &config(9)), Err(vec![range]));
    }

    #[test]
    fn keeps_the_span_of_a_diagnostic_of_the_kind() {
        let mut config = config(1);
        let n = span(20, 24);
        config.attributes = Map::new(vec![Attribute {
            key: "n".into(),
            key_span: None,
            value: Value {
                kind: value::Kind::String("x".into()),
                span: n,
            },
        }])
        .expect("one key");
        let expected =
            Diagnostic::new(MISSING, n, "n is not an integer".into(), "Fix it".into());
        assert_eq!(
            table().check("counter", span(7, 15), &config),
            Err(vec![expected])
        );
    }

    #[test]
    fn shows_the_names_of_its_kinds() {
        let table = table().with("other", Counter);
        assert_eq!(format!("{table:?}"), r#"{"counter", "other"}"#);
    }

    /// A kind that names the counts `.0`.
    struct Counts(&'static [&'static str]);

    impl Kind for Counts {
        type Config = ();
        const SCHEMA: document::schema::Body = crate::testing::READS;

        fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
            Ok(())
        }

        fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
            Ok(Channels {
                counts: self.0.iter().map(|count| name(count)).collect(),
                ..Channels::default()
            })
        }

        fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), Error>> {
            std::future::ready(Ok(()))
        }
    }

    fn check_counts(
        counts: &'static [&'static str],
    ) -> Result<Channels, Vec<Diagnostic>> {
        Table::new().with("counts", Counts(counts)).check(
            "counts",
            None,
            &Document::default(),
        )
    }

    #[test]
    #[should_panic(expected = "the kind \"counts\" names the count `state`, a status \
                               channel of the supervisor")]
    fn panics_on_a_count_named_as_a_status_channel_of_the_supervisor() {
        drop(check_counts(&["state"]));
    }

    #[test]
    #[should_panic(expected = "the kind \"counts\" names the count `class`, a status \
                               channel of the supervisor")]
    fn panics_on_a_count_named_as_the_status_class() {
        drop(check_counts(&["class"]));
    }

    #[test]
    #[should_panic(expected = "the kind \"counts\" names the count `restarts`, a \
                               status channel of the supervisor")]
    fn panics_on_a_count_named_as_the_status_restarts() {
        drop(check_counts(&["restarts"]));
    }

    #[test]
    #[should_panic(expected = "the kind \"counts\" names the count `backoff`, a \
                               status channel of the supervisor")]
    fn panics_on_a_count_named_as_the_status_backoff() {
        drop(check_counts(&["backoff"]));
    }

    #[test]
    #[should_panic(expected = "the kind \"counts\" names the count `error`, a status \
                               channel of the supervisor")]
    fn panics_on_a_count_named_as_the_status_error() {
        drop(check_counts(&["error"]));
    }

    #[test]
    #[should_panic(expected = "the kind \"counts\" names the count `time`, a status \
                               channel of the supervisor")]
    fn panics_on_a_count_named_as_the_status_index() {
        drop(check_counts(&["time"]));
    }

    #[test]
    #[should_panic(expected = "the kind \"counts\" names the count `State`, a status \
                               channel of the supervisor")]
    fn panics_on_a_count_named_as_a_status_channel_in_another_case() {
        drop(check_counts(&["State"]));
    }

    #[test]
    #[should_panic(expected = "the kind \"counts\" names the count `TIME`, a status \
                               channel of the supervisor")]
    fn panics_on_a_count_named_as_the_status_index_in_another_case() {
        drop(check_counts(&["TIME"]));
    }

    #[test]
    #[should_panic(
        expected = "the kind \"counts\" names the count `x.frames`, which is \
                               not one segment"
    )]
    fn panics_on_a_count_of_more_than_one_segment() {
        drop(check_counts(&["x.frames"]));
    }

    #[test]
    #[should_panic(expected = "the kind \"counts\" names the count `Frames` twice")]
    fn panics_on_a_count_named_twice_in_another_case() {
        drop(check_counts(&["frames", "errors", "Frames"]));
    }

    #[test]
    fn checks_counts_with_names_of_their_own() {
        let channels = check_counts(&["frames", "restarts_seen"]).expect("checks");
        assert_eq!(channels.counts, [name("frames"), name("restarts_seen")]);
    }

    #[test]
    #[should_panic(expected = "the kind \"counter\" is in the table twice")]
    fn panics_on_a_kind_added_twice() {
        drop(table().with("counter", Counter));
    }

    #[test]
    fn discovers_through_its_kind() {
        let found =
            run(|_, _, _| async { table().discover("counter", &Token::new()).await });
        assert_eq!(found.expect("found"), [config(2)]);
    }

    #[test]
    fn returns_the_unknown_kind_from_discover_as_a_config_error() {
        let found =
            run(|_, _, _| async { table().discover("modbus", &Token::new()).await });
        let Err(Error::Config(diagnostics)) = found else {
            panic!("a config error: {found:?}");
        };
        assert_eq!(diagnostics, [unknown("modbus", "[\"counter\"]")]);
    }

    #[test]
    fn refuses_to_discover_with_a_kind_that_cannot_look() {
        let table = Table::new().with("counts", Counts(&[]));
        let found =
            run(|_, _, _| async move { table.discover("counts", &Token::new()).await });
        let Err(Error::Config(diagnostics)) = found else {
            panic!("a config error: {found:?}");
        };
        let expected = Diagnostic::new(
            NO_DISCOVER,
            None,
            "this connector kind cannot find its devices".into(),
            "Write the connector block by hand".into(),
        );
        assert_eq!(diagnostics, [expected]);
    }

    #[test]
    fn says_an_empty_table_has_no_kinds() {
        let result = Table::new().check("modbus", None, &config(1));
        let expected = Diagnostic::new(
            UNKNOWN_KIND,
            None,
            "this build has no connector kind \"modbus\"".into(),
            "Use a build that has connector kinds".into(),
        );
        assert_eq!(result, Err(vec![expected]));
    }

    #[test]
    fn runs_until_cancelled_with_its_context() {
        let (early, late, out, ctx_name, n) = run_on(|node, tasks| async move {
            let token = Token::new();
            let inputs =
                Rc::new(create_config(&node, tasks.clone(), Table::new(), "a").await);
            let ctx = Context::new(
                name("plant.counter"),
                3,
                token.clone(),
                tasks.clone(),
                status::Status::new(Vec::new(), tracing::Span::none()),
                None,
                inputs,
            );
            let clock = node.clock();
            let (ctx_name, n) = (ctx.name().clone(), *ctx.config());
            let out = Rc::new(RefCell::new(None));
            let slot = Rc::clone(&out);
            tasks.spawn(async move {
                *slot.borrow_mut() = Some(Kind::run(&Counter, ctx).await);
            });
            clock.sleep(ms(50)).await;
            let early = out.borrow().is_some();
            token.cancel();
            clock.sleep(ms(1)).await;
            let late = out.borrow_mut().take();
            (early, late.is_some(), late, ctx_name, n)
        });
        assert!(!early, "runs until cancelled");
        assert!(late, "returns after the cancel");
        out.expect("returned").expect("stops cleanly");
        assert_eq!((ctx_name, n), (name("plant.counter"), 3));
    }

    #[test]
    fn gives_a_new_random_source_on_each_call() {
        let (a, b) = run_on(|node, tasks| async move {
            let inputs =
                Rc::new(create_config(&node, tasks.clone(), Table::new(), "a").await);
            let status = status::Status::new(Vec::new(), tracing::Span::none());
            let ctx =
                Context::new(name("a"), (), Token::new(), tasks, status, None, inputs);
            (ctx.rng().next_u64(), ctx.rng().next_u64())
        });
        assert_ne!(a, b);
    }

    #[test]
    fn reads_a_secret_at_each_call_and_retries_one_the_store_has_not() {
        let (missing, read, changed) = run_on(|node, tasks| async move {
            let inputs =
                Rc::new(create_config(&node, tasks.clone(), Table::new(), "a").await);
            let status = status::Status::new(Vec::new(), tracing::Span::none());
            let ctx =
                Context::new(name("a"), (), Token::new(), tasks, status, None, inputs);
            let token = name("plant.influx_token");
            let source = secret::Source::Secret(token.clone());
            let get = async || {
                let value = ctx.secret(&source).await;
                value.map(|value| value.expose().to_vec())
            };
            let missing = get().await;
            testing::create_secret(&node.files(), &token, b"t0\n").await;
            let read = get().await;
            testing::create_secret(&node.files(), &token, b"t1").await;
            (missing, read, get().await)
        });
        let Err(Error::Retry(error)) = missing else {
            panic!("a missing secret is retried: {missing:?}");
        };
        assert_eq!(
            error.to_string(),
            "the secret secret(\"plant.influx_token\"): the file \
             /data/secrets/plant.influx_token is not there. Write the value to it, \
             unreadable by other users: chmod o-r /data/secrets/plant.influx_token"
        );
        let source = error.source().map(ToString::to_string);
        assert_eq!(
            source.as_deref(),
            Some(
                "the file /data/secrets/plant.influx_token is not there. Write the \
                 value to it, unreadable by other users: chmod o-r \
                 /data/secrets/plant.influx_token"
            )
        );
        assert_eq!(read.expect("the store has the value"), b"t0");
        assert_eq!(changed.expect("the store has the new value"), b"t1");
    }

    #[test]
    fn retries_a_variable_that_is_not_set_and_names_its_source() {
        let missing = run_on(|node, tasks| async move {
            let inputs =
                Rc::new(create_config(&node, tasks.clone(), Table::new(), "a").await);
            let status = status::Status::new(Vec::new(), tracing::Span::none());
            let ctx =
                Context::new(name("a"), (), Token::new(), tasks, status, None, inputs);
            let source = secret::Source::Env("INFLUX_TOKEN".into());
            ctx.secret(&source).await.map(drop)
        });
        let Err(Error::Retry(error)) = missing else {
            panic!("a missing variable is retried: {missing:?}");
        };
        assert_eq!(
            error.to_string(),
            "the secret env(\"INFLUX_TOKEN\"): the environment variable INFLUX_TOKEN \
             of the node is not set. Start the node with it set to the value"
        );
    }

    #[test]
    fn gives_a_secret_name_that_the_store_cannot_hold_as_a_config_error() {
        let bad = run_on(|node, tasks| async move {
            let inputs =
                Rc::new(create_config(&node, tasks.clone(), Table::new(), "a").await);
            let status = status::Status::new(Vec::new(), tracing::Span::none());
            let ctx =
                Context::new(name("a"), (), Token::new(), tasks, status, None, inputs);
            let source = secret::Source::Secret(name("plant.Token"));
            ctx.secret(&source).await.map(drop)
        });
        let Err(Error::Config(diagnostics)) = bad else {
            panic!("a bad name is a config error: {bad:?}");
        };
        let fix = "Rename the secret in lowercase letters, since the file store holds \
                   no name with a capital letter";
        let message =
            "the secret secret(\"plant.Token\") has a name that the store cannot hold";
        let expected =
            Diagnostic::new(BAD_SECRET_NAME, None, message.into(), fix.into());
        assert_eq!(diagnostics, [expected]);
    }

    #[test]
    fn shows_the_class_and_the_source() {
        let device = Error::Device("no reply from unit 4".into());
        assert_eq!(
            device.to_string(),
            "the device is in a bad state: no reply from unit 4"
        );
        let source = std::error::Error::source(&device).map(ToString::to_string);
        assert_eq!(source.as_deref(), Some("no reply from unit 4"));
        let retry = Error::Retry("timed out".into());
        assert_eq!(retry.to_string(), "an attempt failed: timed out");
        let source = std::error::Error::source(&retry).map(ToString::to_string);
        assert_eq!(source.as_deref(), Some("timed out"));
    }

    /// Calls the mapping directly: the hub of `hub::testing` has no region, so it never
    /// gives `Remote` or `Mesh`. #2376 holds the test through a run.
    #[test]
    fn retries_a_hub_error_that_the_home_or_the_mesh_can_clear() {
        let home = types::node::Key::from_u128(7);
        let stopped = || hub::mesh::Stopped::Dropped;
        let reader = |error| reader_error(error).to_string();
        let writer = |error| writer_error(error).to_string();
        assert_eq!(
            reader(hub::reader::Error::Remote { home }),
            format!(
                "an attempt failed: the home of the index of the reader is node \
                 {home}, and this reader opens only at the home"
            )
        );
        assert_eq!(
            writer(hub::writer::Error::Remote { home }),
            format!(
                "an attempt failed: the home of an index of the writer is node \
                 {home}, and a writer writes only at this node"
            )
        );
        let mesh =
            "an attempt failed: the mesh stopped: each mesh of the group dropped";
        assert_eq!(reader(hub::reader::Error::Mesh(stopped())), mesh);
        assert_eq!(writer(hub::writer::Error::Mesh(stopped())), mesh);
        assert_eq!(
            reader(hub::reader::Error::Unrecorded),
            "an attempt failed: the home had no room to record the position of the \
             named reader: open it again later"
        );
    }

    /// Calls the mapping directly: on the ring of the test hub, the entries that the
    /// opens append fill the ring before the records reach the limit, until a trim
    /// frees the ring (#406).
    #[test]
    fn retries_when_the_named_readers_fill_an_entry() {
        let full = reader_error(hub::reader::Error::Full);
        assert!(matches!(full, Error::Retry(_)), "{full:?}");
        assert_eq!(
            full.to_string(),
            "an attempt failed: no part of the named records of the index has room for \
             the reader: read the index from fewer connectors in complete mode, \
             set `mode = \"latest\"` on one, or wait for the hold of a removed reader \
             to end"
        );
    }

    /// Calls the mapping directly: the supervisor opens a reader only after the status
    /// channels, which wait for mesh time, so no run can give `Unsynced`.
    #[test]
    #[should_panic(
        expected = "invariant: a reader opened before mesh time: the node has no mesh \
                    time yet: open the named reader again later"
    )]
    fn panics_for_a_reader_that_has_no_mesh_time() {
        drop(reader_error(hub::reader::Error::Unsynced));
    }

    /// Calls the mapping directly, since a named reader opens only at its home. The
    /// arm of `Transport` has no test: `connector` cannot build a `transport` error.
    #[test]
    #[should_panic(
        expected = "invariant: a named reader away from its home gave: the pool had no \
                    block for the open: pool is full: asked for 8 bytes, 0 bytes free"
    )]
    fn panics_for_a_reader_error_away_from_its_home() {
        let pool = block::Error::Exhausted {
            requested: 8,
            available: 0,
        };
        drop(reader_error(hub::reader::Error::Pool(pool)));
    }

    #[test]
    #[should_panic(
        expected = "invariant: a named reader away from its home gave: the home \
                    refused the reader with code 17: the node is not the home of the \
                    index"
    )]
    fn panics_for_a_refusal_of_the_reader() {
        let refusal = wire::hub::Refusal::NotHome;
        drop(reader_error(hub::reader::Error::Refused(refusal)));
    }

    #[test]
    #[should_panic(
        expected = "invariant: a named reader away from its home gave: the reply of \
                    the home broke the hub protocol: the hub message is empty"
    )]
    fn panics_for_a_reply_that_breaks_the_hub_protocol() {
        drop(reader_error(hub::reader::Error::Message(
            wire::hub::Error::Empty,
        )));
    }

    #[test]
    #[should_panic(
        expected = "invariant: a writer of the hub gave: the node has no mesh time yet"
    )]
    fn panics_for_a_writer_error_that_the_hub_never_gives() {
        let unsynced = hub::home::writer::Error::Unsynced;
        drop(writer_error(hub::writer::Error::Home(unsynced)));
    }

    /// Calls the mapping directly, because a named reader opens only at its home. The
    /// arm has no test of `Stream`: `connector` cannot build a `transport` error.
    #[test]
    #[should_panic(
        expected = "invariant: a named reader away from its home gave: the home sent \
                    a frame past the credit of 8 bytes"
    )]
    fn panics_for_an_end_of_a_reader_away_from_its_home() {
        drop(Error::from(hub::reader::Ended::Credit { limit_bytes: 8 }));
    }

    #[test]
    #[should_panic(
        expected = "invariant: a named reader away from its home gave: the home \
                    ended the session with code 17: the node is not the home of the \
                    index"
    )]
    fn panics_for_a_refused_end_of_a_reader_away_from_its_home() {
        drop(Error::from(hub::reader::Ended::Refused(
            wire::hub::Refusal::NotHome,
        )));
    }

    #[test]
    #[should_panic(
        expected = "invariant: a named reader away from its home gave: a message \
                    from the home broke the hub protocol: the hub message is empty"
    )]
    fn panics_for_a_message_end_of_a_reader_away_from_its_home() {
        drop(Error::from(hub::reader::Ended::Message(
            wire::hub::Error::Empty,
        )));
    }

    #[test]
    #[should_panic(
        expected = "invariant: a named reader away from its home gave: a frame \
                    from the home is not valid: entry 2 is out of range for a key set \
                    of 1 entries"
    )]
    fn panics_for_a_frame_end_of_a_reader_away_from_its_home() {
        drop(Error::from(hub::reader::Ended::Frame(
            types::frame::Error::OutOfRange {
                entry: 2,
                entries: 1,
            },
        )));
    }

    #[test]
    #[should_panic(expected = "invariant: a kind labels no frame resend")]
    fn panics_for_a_write_of_a_resend_frame() {
        let resend = hub::home::Error::Resend;
        drop(Error::from(hub::writer::Failure::Home(resend)));
    }

    #[test]
    fn shows_every_diagnostic_of_a_config_error() {
        let config = Error::Config(vec![
            diagnostic(RANGE, "n is over 8"),
            diagnostic(MISSING, "no integer n"),
        ]);
        assert_eq!(
            config.to_string(),
            "n is over 8. Fix it; no integer n. Fix it"
        );
        assert!(std::error::Error::source(&config).is_none());
    }
}
