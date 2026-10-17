//! Runs connectors and restarts them after errors.

use std::fmt;
use std::future::poll_fn;
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::Poll;

use document::Document;
use env::clock::Clock;
use env::entropy::Entropy;
use env::net::Net;
use env::tasks::{Group, Tasks};
use types::name::Name;
use types::time::Span;

use crate::kind::{self, Context, Error};
use crate::{browse, cancel, http, reader, retry, status};

/// The waits between restarts.
const RESTART: retry::Config = retry::Config {
    first: Span::SECOND,
    cap: Span::MINUTE,
};

/// A run at least this long starts the waits again from the first.
const HEALTHY: Span = Span::MINUTE;

/// The inputs of a supervisor on one shard.
#[derive(Debug)]
pub struct Config {
    /// The kinds this binary has.
    pub kinds: Arc<kind::Table>,
    /// The node's clock.
    pub clock: Clock,
    /// The source of each run's randomness.
    pub entropy: Entropy,
    /// The network that kinds connect through.
    pub net: Net,
    /// The roots that an `https` connect of a kind trusts.
    pub roots: http::Roots,
    /// The shard's tasks.
    pub tasks: Tasks,
    /// The shard's hub. Each session a kind opens acts as its connector.
    pub hub: hub::Hub,
    /// The sources that [`kind::Context::secret`] reads.
    pub secrets: secret::Resolver,
}

/// Runs connectors of the kinds in a table, one `open` and its `run` at a time per
/// connector. It is not `Send`: each shard makes its own. Its clones share one config.
#[derive(Clone, Debug)]
pub struct Supervisor(Rc<Config>);

impl Supervisor {
    /// Makes a supervisor for one shard.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self(Rc::new(config))
    }

    /// The kinds of the connectors that the supervisor runs.
    #[must_use]
    pub fn kinds(&self) -> &kind::Table {
        &self.0.kinds
    }

    /// The sources that [`kind::Context::secret`] reads for the connectors of the
    /// supervisor.
    #[must_use]
    pub fn secrets(&self) -> &secret::Resolver {
        &self.0.secrets
    }

    /// Browses the device of the connector block `config` of `kind` as `query`
    /// asks, with the network, clock, randomness, and secrets of the supervisor. It
    /// connects apart from any run of the connector and keeps no state after it
    /// returns. A drop of the future ends the browse. The future is not `Send`: call
    /// it on a shard.
    ///
    /// # Errors
    ///
    /// The errors of [`Kind::browse`](kind::Kind::browse), and [`Error::Config`] with
    /// `connector.unknown-kind`.
    pub async fn browse(
        &self,
        kind: &str,
        config: &Document,
        query: &browse::Query,
    ) -> Result<browse::Found, Error> {
        let ctx = browse::Context::new(config, &self.0);
        self.0.kinds.browse(kind, &ctx, query).await
    }

    /// Opens the status channels of the connector `name` once the node has mesh
    /// time, then, when the kind's `check` gives reader settings, its reader under
    /// `name`. Resolves once the status channels and the reader opened, or once the
    /// first run cannot start: the kind is unknown, the config does not parse or
    /// check, the mesh stopped, the reader open failed, or `cancel` is cancelled. The
    /// reader open waits while the mesh names no home for the index that it selects.
    /// It runs no code of the kind's `run`, so the connector writes no frame before
    /// [`Opened::run`]. Dropped, the [`Opened`] closes the reader and writes no status.
    /// The future is not `Send`: call it on a shard.
    ///
    /// # Panics
    ///
    /// When the status channels of `name` do not open for a reason other than a
    /// stopped mesh: their index has its home on another node, or `node` did not
    /// define them and did not cancel the call. When a status name of `name` is
    /// longer than [`Name::MAX_BYTES`], which the plan refuses. When the hub gives a
    /// reader error that this open cannot get.
    pub async fn open<'a>(
        &'a self,
        kind: &'a str,
        name: Name,
        config: &'a Document,
        cancel: &'a cancel::Token,
    ) -> Opened<'a> {
        let call = Call {
            supervisor: self,
            kind,
            name,
            config,
            cancel,
        };
        let start = call.start().await;
        Opened { call, start }
    }
}

/// A connector that [`Supervisor::open`] opened, whose first run has not started.
#[must_use = "an Opened runs nothing until Opened::run"]
pub struct Opened<'a> {
    call: Call<'a>,
    start: Start,
}

impl fmt::Debug for Opened<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Opened")
            .field("call", &self.call)
            .finish_non_exhaustive()
    }
}

impl Opened<'_> {
    /// Runs the connector, and restarts it with backoff after any error but `Config`.
    /// Before each run but the first, after the backoff and before the start status,
    /// opens the reader again under the connector's name, when `check` gave reader
    /// settings. That open waits while the mesh names no home for the index that it
    /// selects. A reader open that fails ends that run with its error. The waits start
    /// again from the first after a run that lasted at least a minute. Never starts a
    /// run before the last one returned and each task it spawned through
    /// [`Context::tasks`] ended, or after `cancel` is cancelled. Writes the connector's
    /// status channels as the connector: the whole status at each start and change of
    /// state, and a change of counts alone at most once each second. Each change of
    /// state, a start too, waits until the home applied the state before it, `cancel`
    /// is cancelled, or 10 s passed since the oldest change of state that the home did
    /// not apply.
    ///
    /// Returns `Ok` when `run` returns `Ok`, when the mesh stopped before the status
    /// channels opened, or when `cancel` is cancelled: at once while the channels are
    /// not open, with no new run while a reader open waits (the open of
    /// [`Supervisor::open`] too), and otherwise once the run returned. It returns, with
    /// `Ok` or an error, only once each task of its last run ended, and the home
    /// applied its last status frame, `cancel` is cancelled, or 10 s passed since the
    /// write of its last state. So however late the home applies status frames, once
    /// its last run returned `Ok` or a config error, it returns at most 30 s after that
    /// run returned, or 10 s after the last task of that run ended when that is later.
    /// Neither wait holds once the home refused a status frame because the status
    /// channels were removed or its disk failed: after it, the call writes no more
    /// status. A drop of the future cancels the run and does not wait for its tasks: to
    /// wait, cancel `cancel` and await the future. The future is not `Send`: call it on
    /// a shard.
    ///
    /// # Errors
    ///
    /// [`Error::Config`], without a restart, when the kind is unknown, the config
    /// does not parse or check, a reader open gives it, or `run` returns it.
    ///
    /// # Panics
    ///
    /// When the home refuses a status frame for a cause that only a defect of
    /// `connector` gives, or a status frame is larger than the largest block of the
    /// shard's pool. When the hub gives a reader error that this open cannot get.
    pub async fn run(self) -> Result<(), Error> {
        let Self { call, start } = self;
        match start {
            Start::Done(end) => end,
            Start::Ready(ready) => {
                let Ready {
                    writer,
                    status,
                    settings,
                    reader,
                } = *ready;
                let restarts =
                    call.restarts(&writer, &status, settings.as_ref(), reader);
                writer.during(restarts).await
            }
        }
    }
}

/// The arguments of one call of [`Supervisor::open`].
#[derive(Debug)]
struct Call<'a> {
    supervisor: &'a Supervisor,
    kind: &'a str,
    name: Name,
    config: &'a Document,
    cancel: &'a cancel::Token,
}

/// What [`Supervisor::open`] gives [`Opened::run`].
enum Start {
    /// No run starts, and the call ends with this.
    Done(Result<(), Error>),
    Ready(Box<Ready>),
}

/// The status of a connector whose status channels opened, and its first reader.
struct Ready {
    writer: status::Writer,
    status: status::Status,
    settings: Option<reader::Settings>,
    reader: Opening,
}

/// The reader open of one run.
#[expect(
    clippy::large_enum_variant,
    reason = "one for each run, which the run loop unpacks at once"
)]
enum Opening {
    /// `cancel` is cancelled, so no run starts.
    Cancelled,
    /// The run ends with this error.
    Failed(Error),
    /// The reader, or `None` for a kind that reads no channel.
    Opened(Option<hub::reader::Reader>),
}

impl Call<'_> {
    /// The status writer and the first reader of [`Supervisor::open`], or the end of a
    /// call that starts no run.
    async fn start(&self) -> Start {
        let Self {
            supervisor,
            kind,
            name,
            config,
            cancel,
        } = self;
        let Config {
            kinds, clock, hub, ..
        } = &*supervisor.0;
        let channels = match kinds.check(kind, None, config) {
            Ok(channels) => channels,
            Err(diagnostics) => return Start::Done(Err(Error::Config(diagnostics))),
        };
        let mut open = pin!(status::Writer::open(
            hub,
            name,
            channels.counts,
            clock.clone(),
            (*cancel).clone()
        ));
        let mut cancelled = pin!(cancel.wait());
        // An open that is ready at once wins, so a cancel before the call still
        // writes `state` 2.
        let opened = poll_fn(|cx| match open.as_mut().poll(cx) {
            Poll::Ready(opened) => Poll::Ready(Some(opened)),
            Poll::Pending => cancelled.as_mut().poll(cx).map(|()| None),
        });
        let (writer, status) = match opened.await {
            None | Some(Err(hub::writer::Error::Mesh(_))) => {
                return Start::Done(Ok(()));
            }
            // A cancelled call may find its status channels removed.
            Some(Err(hub::writer::Error::Unknown(_))) if cancel.cancelled() => {
                return Start::Done(Ok(()));
            }
            Some(Ok(opened)) => opened,
            Some(Err(error)) => {
                panic!(
                    "the status channels of the connector {name} do not open: {error}"
                )
            }
        };
        let settings = channels.reader;
        // After the status channels, which wait for mesh time, so the open never gives
        // `Unsynced`.
        let reader = self.reader(settings.as_ref()).await;
        Start::Ready(Box::new(Ready {
            writer,
            status,
            settings,
            reader,
        }))
    }

    /// Opens the reader of `settings` under the connector's name.
    async fn reader(&self, settings: Option<&reader::Settings>) -> Opening {
        let Some(settings) = settings else {
            return Opening::Opened(None);
        };
        let config = hub::reader::Config {
            select: settings.select.clone(),
            mode: settings.mode,
            subject: self.name.clone(),
            name: Some(self.name.clone()),
            hold: settings.hold,
        };
        match self.cancel.race(self.supervisor.0.hub.reader(config)).await {
            None => Opening::Cancelled,
            Some(Ok(reader)) => Opening::Opened(Some(reader)),
            Some(Err(error)) => Opening::Failed(kind::reader_error(error)),
        }
    }

    /// Runs the connector and restarts it, and writes its status through `writer`
    /// at each change of state. `first` is the reader of the first run.
    async fn restarts(
        &self,
        writer: &status::Writer,
        status: &status::Status,
        settings: Option<&reader::Settings>,
        first: Opening,
    ) -> Result<(), Error> {
        let Self {
            supervisor,
            kind,
            config,
            cancel,
            ..
        } = self;
        let Config {
            kinds,
            clock,
            entropy,
            ..
        } = &*supervisor.0;
        let mut backoff = retry::Backoff::new(clock, entropy.rng(), RESTART);
        let mut first = Some(first);
        loop {
            // Before the start status, so the reader of each run sees that status.
            let opening = match first.take() {
                Some(opening) => opening,
                None => self.reader(settings).await,
            };
            let reader = match opening {
                Opening::Cancelled => break,
                Opening::Failed(error) => Err(error),
                Opening::Opened(reader) => Ok(reader),
            };
            if !writer.start().await {
                break;
            }
            let token = Cancels(cancel.child());
            let group = Group::new(self.supervisor.0.tasks.clone());
            let start = clock.now();
            let end = match reader {
                Ok(reader) => {
                    let ctx = self.context(reader, &token.0, &group, status);
                    kinds.run(kind, config, ctx).map_err(Error::Config)?.await
                }
                Err(error) => Err(error),
            };
            let lasted = clock.now() - start;
            drop(token);
            let ended = writer.end(&end);
            // A device error can be large, and nothing after this needs it.
            let last = matches!(end, Ok(()) | Err(Error::Config(_))).then_some(end);
            ended.await;
            group.ended().await;
            if let Some(end) = last {
                writer.stop().await;
                return end;
            }
            if cancel.cancelled() {
                break;
            }
            if lasted >= HEALTHY {
                backoff.reset();
            }
            let next = writer.wait(backoff.next()).await;
            cancel.race(clock.sleep_until(next)).await;
        }
        writer.stop().await;
        Ok(())
    }

    /// The context of one run, cancelled by `token`, whose tasks `group` counts.
    fn context(
        &self,
        reader: Option<hub::reader::Reader>,
        token: &cancel::Token,
        group: &Group,
        status: &status::Status,
    ) -> Context<()> {
        let inputs = &self.supervisor.0;
        Context::new(
            self.name.clone(),
            (),
            token.clone(),
            group.tasks().clone(),
            status.share(),
            reader,
            Rc::clone(inputs),
        )
    }
}

/// A run's token, cancelled when the run returned or its future dropped.
struct Cancels(cancel::Token);

impl Drop for Cancels {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::io::IoSlice;
    use std::net::SocketAddr;
    use std::num::NonZeroUsize;
    use std::pin::pin;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Waker;

    use env::files::Absolute;
    use env::net::tcp;

    use document::diagnostic::{Code, Diagnostic};
    use document::value::{self, Value};
    use document::{Attribute, Map};
    use types::time::Monotonic;

    use super::*;
    use crate::cancel::Token;
    use crate::common::{
        STATUS, create_config, create_status, env, run_in, run_on, run_with,
    };
    use crate::kind::{Channels, Kind, Table};
    use crate::testing;
    use hub::home::Refusal;
    use hub::reader::{Mode, Received};
    use spec::channel::{Channel, Data};
    use spec::data_type::DataType;
    use spec::definition::Definition;
    use types::authority::Authority;
    use types::channel;
    use types::frame::{self, Form, Label, Path};
    use types::name::Selector;
    use types::sample::{Scalar, Type};

    const BAD: Code = Code::new("test.bad");
    /// The least count of sim seeds of a test whose outcome the seed decides, through
    /// the order of tasks ready at one instant or the draws of `entropy`.
    const SEEDS: u64 = 64;

    /// Runs `check` at each of [`SEEDS`] seeds. Panics unless it gives `true` at some
    /// seeds and `false` at others, so that each outcome occurs.
    fn each_seed(outcome: &str, mut check: impl FnMut(u64) -> bool) {
        let n: u64 = (0..SEEDS).map(|seed| u64::from(check(seed))).sum();
        assert!(0 < n && n < SEEDS, "{n} of {SEEDS} seeds {outcome}");
    }

    /// What one run of [`Script`] does.
    #[derive(Clone, Debug)]
    enum Step {
        /// Returns `Ok` at once.
        Done,
        /// Returns a device error after the span.
        Device(Span),
        /// Returns a config error at once.
        Config,
        /// Returns a config error of two diagnostics at once.
        Configs,
        /// Returns a retry error at once.
        Retry,
        /// Returns a device error at once, whose text is `a`, then 600 `é`.
        Long,
        /// Returns a device error at once, whose text is 1100 `a`.
        Wide,
        /// Cancels its own token, then returns a device error.
        Abort,
        /// Waits for the cancel, then the span, then returns a device error.
        Linger(Span),
        /// Spawns a task that ends the span after the cancel, then returns a device
        /// error at once.
        Hold(Span),
        /// Spawns a task that ends the span after the cancel, then returns a config
        /// error at once.
        Refuse(Span),
        /// Returns the end of a reader with `?`.
        Ended(hub::reader::Ended),
        /// Returns a failed write with `?`.
        Failed(hub::writer::Failure),
        /// Returns a failed draft of a frame with `?`.
        Draft(frame::Error),
        /// Shows the error `refused`, waits the span, shows it again, waits the span,
        /// clears it twice, waits the span, then returns `Ok`.
        Show(Span),
        /// Shows the error `refused`, then returns a device error at once.
        ShowEnd,
        /// Shows an error of 1,100 bytes, waits the span, then returns `Ok`.
        ShowWide(Span),
        /// Clears the error, waits the span, then returns `Ok`.
        Clear(Span),
        /// Lends its context to a task that shows the error `late`, or clears the
        /// error when `cleared`, at the cancel of the run. Then returns `Ok` at once,
        /// or a device error when `failed`.
        Late { failed: bool, cleared: bool },
        /// Shows the note `polls` and the error `refused`, waits the span, shows the
        /// note again, waits the span, then returns `Ok`.
        Note(Span),
        /// Shows a note of 1,111 bytes with a 2-byte char at bytes 510 and 511, waits
        /// the span, then returns `Ok`.
        NoteWide(Span),
        /// Lends its context to a task that shows the note `late` at the cancel of the
        /// run, then returns `Ok` at once.
        NoteLate,
        /// Shows a note whose text shows the error `refused` as it formats, waits the
        /// span, then returns `Ok`.
        NoteNested(Span),
        /// Shows a note whose text is the debug of its context, waits the span, then
        /// returns `Ok`.
        NoteDebug(Span),
    }

    /// A note that shows the error `refused` on its context as it formats.
    struct Nested<'a>(&'a Context<bool>);

    impl fmt::Display for Nested<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.error(&"refused");
            f.write_str("polls")
        }
    }

    /// When each run started and ended.
    type Runs = Arc<Mutex<Vec<(Monotonic, Option<Monotonic>)>>>;

    /// A kind that runs the next step of its script on each run and records when
    /// each run started and ended. With no step left, it waits for the cancel.
    #[derive(Default)]
    struct Script {
        steps: Mutex<VecDeque<Step>>,
        runs: Runs,
    }

    impl Kind for Script {
        /// The config has the attribute `worse`, which `check` refuses.
        type Config = bool;

        fn parse(&self, config: &Document) -> Result<bool, Vec<Diagnostic>> {
            match config.attributes.get("bad") {
                Some(_) => Err(vec![bad()]),
                None => Ok(config.attributes.get("worse").is_some()),
            }
        }

        fn check(&self, refused: &bool) -> Result<Channels, Vec<Diagnostic>> {
            match refused {
                true => Err(vec![worse()]),
                false => Ok(Channels::default()),
            }
        }

        async fn run(&self, ctx: Context<bool>) -> Result<(), Error> {
            let ctx = Rc::new(ctx);
            assert_eq!(ctx.name().as_str(), "plant.script");
            let clock = ctx.clock();
            let index = {
                let mut runs = self.runs.lock().expect("no panic under the lock");
                runs.push((clock.now(), None));
                runs.len() - 1
            };
            let step = self
                .steps
                .lock()
                .expect("no panic under the lock")
                .pop_front();
            let out = play(&ctx, step).await;
            let mut runs = self.runs.lock().expect("no panic under the lock");
            if let Some(run) = runs.get_mut(index) {
                run.1 = Some(clock.now());
            }
            out
        }
    }

    /// Runs `step` of a `Script`, or waits for the cancel when it is `None`.
    async fn play(lent: &Rc<Context<bool>>, step: Option<Step>) -> Result<(), Error> {
        let ctx: &Context<bool> = lent;
        let clock = ctx.clock();
        match step {
            Some(Step::Done) => Ok(()),
            Some(Step::Device(span)) => {
                clock.sleep(span).await;
                Err(Error::Device("no reply".into()))
            }
            Some(Step::Config) => Err(Error::Config(vec![bad()])),
            Some(Step::Configs) => Err(Error::Config(vec![bad(), worse()])),
            Some(Step::Retry) => Err(Error::Retry("busy".into())),
            Some(Step::Long) => Err(Error::Device(long().into())),
            Some(Step::Wide) => Err(Error::Device("a".repeat(1_100).into())),
            Some(Step::Abort) => {
                ctx.cancel().cancel();
                Err(Error::Device("stopped its parts".into()))
            }
            Some(Step::Linger(span)) => {
                ctx.cancel().wait().await;
                clock.sleep(span).await;
                Err(Error::Device("stopped late".into()))
            }
            Some(Step::Hold(span)) => {
                hold(ctx, span);
                Err(Error::Device("left a task".into()))
            }
            Some(Step::Refuse(span)) => {
                hold(ctx, span);
                Err(Error::Config(vec![bad()]))
            }
            Some(Step::Ended(ended)) => returned(ended),
            Some(Step::Failed(failure)) => returned(failure),
            Some(Step::Draft(error)) => returned(error),
            Some(Step::Show(span)) => {
                ctx.error(&"refused");
                clock.sleep(span).await;
                ctx.error(&"refused");
                clock.sleep(span).await;
                ctx.clear_error();
                ctx.clear_error();
                clock.sleep(span).await;
                Ok(())
            }
            Some(Step::ShowEnd) => {
                ctx.error(&"refused");
                Err(Error::Device("no reply".into()))
            }
            Some(Step::ShowWide(span)) => {
                ctx.error(&"a".repeat(1_100));
                clock.sleep(span).await;
                Ok(())
            }
            Some(Step::Clear(span)) => {
                ctx.clear_error();
                clock.sleep(span).await;
                Ok(())
            }
            Some(Step::Late { failed, cleared }) => show_late(lent, failed, cleared),
            Some(step) => note(lent, step).await,
            None => {
                ctx.cancel().wait().await;
                Ok(())
            }
        }
    }

    /// Runs a step of a note: [`Step::Note`], [`Step::NoteWide`], [`Step::NoteLate`],
    /// [`Step::NoteNested`], or [`Step::NoteDebug`].
    async fn note(lent: &Rc<Context<bool>>, step: Step) -> Result<(), Error> {
        let clock = lent.clock();
        match step {
            Step::Note(span) => {
                lent.note(&"polls");
                lent.error(&"refused");
                clock.sleep(span).await;
                lent.note(&"polls");
                clock.sleep(span).await;
            }
            Step::NoteWide(span) => {
                lent.note(&format!("{}é{}", "a".repeat(509), "a".repeat(600)));
                clock.sleep(span).await;
            }
            Step::NoteLate => {
                let late = Rc::clone(lent);
                lent.tasks().spawn(async move {
                    late.cancel().wait().await;
                    late.note(&"late");
                });
            }
            Step::NoteNested(span) => {
                lent.note(&Nested(lent));
                clock.sleep(span).await;
            }
            Step::NoteDebug(span) => {
                lent.note(&format_args!("{lent:?}"));
                clock.sleep(span).await;
            }
            _ => unreachable!("a step of a note"),
        }
        Ok(())
    }

    /// Runs [`Step::Late`].
    fn show_late(
        lent: &Rc<Context<bool>>,
        failed: bool,
        cleared: bool,
    ) -> Result<(), Error> {
        let late = Rc::clone(lent);
        lent.tasks().spawn(async move {
            late.cancel().wait().await;
            if cleared {
                late.clear_error();
            } else {
                late.error(&"late");
            }
        });
        if failed {
            return Err(Error::Device("no reply".into()));
        }
        Ok(())
    }

    /// Returns `end` with `?`, as a kind does.
    fn returned<E>(end: E) -> Result<(), Error>
    where
        Error: From<E>,
    {
        Err(end)?
    }

    /// Spawns a task of the run that ends `span` after the run's cancel.
    fn hold<C>(ctx: &Context<C>, span: Span) {
        let (token, clock) = (ctx.cancel().clone(), ctx.clock().clone());
        ctx.tasks().spawn(async move {
            token.wait().await;
            clock.sleep(span).await;
        });
    }

    /// `a`, then 600 `é`: 1201 bytes.
    fn long() -> String {
        format!("a{}", "é".repeat(600))
    }

    fn bad() -> Diagnostic {
        Diagnostic::new(BAD, None, "the config is bad".into(), "Fix it".into())
    }

    fn worse() -> Diagnostic {
        Diagnostic::new(BAD, None, "the rate is bad".into(), "Fix it too".into())
    }

    fn between(from: Span, to: Span) -> Span {
        Span::from_nanos(to.nanos() - from.nanos())
    }

    fn ms(n: i64) -> Span {
        Span::from_nanos(n * 1_000_000)
    }

    /// The outcome of [`supervise`]: the result, when it returned, each run's start
    /// and end, all from the start, and each status frame.
    struct Outcome {
        result: Result<(), Error>,
        returned: Span,
        runs: Vec<(Span, Option<Span>)>,
        statuses: Vec<Written>,
    }

    /// One status frame: its stamp from the first, the sample of each status channel
    /// but the index, `error`, and `note`, in the order of [`status::channels`], the
    /// text of `error`, and the text of `note`.
    type Written = (Span, Vec<i64>, String, String);

    /// Opens a writer of `channels` as `subject` at `authority`, with no lease.
    async fn open_writer(
        hub: &hub::Hub,
        subject: &str,
        authority: Authority,
        channels: Vec<Name>,
    ) -> hub::writer::Writer {
        let config = hub::writer::Config {
            subject: name(subject),
            authority,
            lease: None,
            channels,
        };
        hub.writer(config).await.expect("opens")
    }

    /// Writes one status frame of `plant.script` from another writer, stamped `lead`
    /// after the hub's time, with an empty text in `error` and `note`, and 9 in each
    /// other sample.
    async fn write_ahead(hub: &hub::Hub, lead: Span) {
        let channels = status_names("plant.script", &[]);
        let mut other =
            open_writer(hub, "plant.other", Authority::ABSOLUTE, channels).await;
        let entries = other.set().entries();
        let text = codec::Variable::of(Type::String).expect("a variable type");
        let series: Vec<_> = (entries.iter().enumerate())
            .map(|(i, entry)| {
                let width = entry.data_type.width();
                (
                    i,
                    width.unwrap_or_else(|| text.len(&[""]).expect("a length")),
                )
            })
            .collect();
        let mut draft = other.draft(Form::Raw, &series).expect("a frame");
        let stamp = other.now().nanos() + lead.nanos();
        for (i, entry) in entries.iter().enumerate() {
            let bytes = draft.series_mut(i).expect("a series");
            if entry.data_type == Type::String {
                text.write(&[""], bytes);
                continue;
            }
            let value = if entry.key == STATUS { stamp } else { 9 };
            let len = bytes.len();
            bytes.copy_from_slice(&value.to_le_bytes()[..len]);
        }
        draft.set_count(entries[0].group, 1);
        let outcomes = other.write(Label::Path(Path::Live), draft);
        let applied = matches!(outcomes, Ok([hub::home::Outcome::Applied { .. }]));
        assert!(applied, "the frame ahead applies: {outcomes:?}");
    }

    /// Reads the status frames of `connector` with `counts`, whose status channels
    /// have keys from [`STATUS`] on, into the vector it gives, in a task on `tasks`.
    /// Panics on a frame that lacks one of [`status::channels`].
    async fn read_status(
        hub: &hub::Hub,
        connector: &str,
        counts: &[Name],
        tasks: &env::tasks::Tasks,
    ) -> Rc<RefCell<Vec<Written>>> {
        let connector = connector.parse().expect("a valid name");
        let names = status::channels(&connector, counts).expect("names");
        let (_, channels) = names;
        let key = |last: &str| {
            let at = channels
                .iter()
                .position(|(name, _)| name.as_str().ends_with(last));
            STATUS.as_u128() + 1 + u128::try_from(at.expect(last)).expect("few")
        };
        let (error, note) = (key(".error"), key(".note"));
        let names: Vec<_> = channels.into_iter().map(|(name, _)| name).collect();
        let keys = 1..=u128::try_from(names.len()).expect("a few channels");
        let keys =
            keys.filter(move |&key| ![error, note].contains(&(STATUS.as_u128() + key)));
        let open = hub::reader::Config {
            select: Selector::new(names.iter().map(Name::as_str)).expect("a selector"),
            mode: Mode::Complete,
            subject: name("test"),
            name: None,
            hold: Span::ZERO,
        };
        let mut reader = hub.reader(open).await.expect("the reader opens");
        let statuses = Rc::new(RefCell::new(Vec::new()));
        let into = Rc::clone(&statuses);
        tasks.spawn(async move {
            let mut first = None;
            while let Ok(received) = reader.next().await {
                let held: Vec<_> = (received.view().iter())
                    .map(|(entry, _)| received.name(entry))
                    .collect();
                let lacks = names.iter().find(|name| !held.contains(&name.as_str()));
                assert_eq!(lacks, None, "a status frame holds each status channel");
                let stamps = series(&received, STATUS.as_u128());
                let samples: Vec<_> = keys
                    .clone()
                    .map(|key| series(&received, STATUS.as_u128() + key))
                    .collect();
                let mut notes = texts(&received, note).into_iter();
                let mut texts = texts(&received, error).into_iter();
                let mut into = into.borrow_mut();
                for (i, stamp) in stamps.into_iter().enumerate() {
                    let first = *first.get_or_insert(stamp);
                    let at = Span::from_nanos(stamp - first);
                    let samples = samples.iter().map(|series| series[i]).collect();
                    let (text, note) = (texts.next(), notes.next());
                    into.push((
                        at,
                        samples,
                        text.expect("a text"),
                        note.expect("a note"),
                    ));
                }
            }
        });
        statuses
    }

    /// When [`supervise`] cancels its connector.
    enum Cancel {
        Never,
        /// Before the call.
        Now,
        After(Span),
    }

    /// Supervises one connector of [`Script`] with `steps` and `config` in a sim of
    /// `seed`, and cancels it at `cancel`.
    fn supervise(
        seed: u64,
        kind: &'static str,
        steps: Vec<Step>,
        config: Document,
        cancel: Cancel,
    ) -> Outcome {
        run_in(seed, move |node, tasks| async move {
            let script = Script {
                steps: Mutex::new(steps.into()),
                ..Script::default()
            };
            let runs = Arc::clone(&script.runs);
            let kinds = Table::new().with("script", script);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.script").await;
            let statuses = read_status(&inputs.hub, "plant.script", &[], &tasks).await;
            let supervisor = Supervisor::new(inputs);
            let clock = node.clock();
            let token = Token::new();
            match cancel {
                Cancel::Never => {}
                Cancel::Now => token.cancel(),
                Cancel::After(after) => {
                    let canceller = token.clone();
                    let sleeper = clock.clone();
                    tasks.spawn(async move {
                        sleeper.sleep(after).await;
                        canceller.cancel();
                    });
                }
            }
            let start = clock.now();
            let name = "plant.script".parse().expect("a valid name");
            let result = supervisor
                .open(kind, name, &config, &token)
                .await
                .run()
                .await;
            let returned = clock.now() - start;
            let runs = runs
                .lock()
                .expect("no panic under the lock")
                .iter()
                .map(|(from, to)| (*from - start, to.map(|to| to - start)))
                .collect();
            clock.sleep(Span::SECOND).await;
            let statuses = statuses.borrow().clone();
            Outcome {
                result,
                returned,
                runs,
                statuses,
            }
        })
    }

    fn config() -> Document {
        Document::default()
    }

    fn config_errors(result: Result<(), Error>) -> Vec<Diagnostic> {
        match result {
            Err(Error::Config(diagnostics)) => diagnostics,
            other => panic!("a config error: {other:?}"),
        }
    }

    #[test]
    fn gives_the_kinds_of_its_config() {
        let kinds = run_on(|node, tasks| async move {
            let kinds = Table::new().with("script", Script::default());
            let inputs = create_config(&node, tasks, kinds, "plant.script").await;
            format!("{:?}", Supervisor::new(inputs).kinds())
        });
        assert_eq!(kinds, "{\"script\"}");
    }

    #[test]
    fn runs_once_and_returns_when_run_returns_ok() {
        let out = supervise(0, "script", vec![Step::Done], config(), Cancel::Never);
        out.result.expect("ok");
        assert_eq!(out.runs, [(Span::ZERO, Some(Span::ZERO))]);
    }

    /// The `(state, class, restarts)` of each of `statuses`.
    fn states(statuses: &[Written]) -> Vec<(i64, i64, i64)> {
        let state = |samples: &[i64]| {
            let [state, class, restarts, ..] = samples[..] else {
                panic!("the samples of the supervisor's channels: {samples:?}");
            };
            (state, class, restarts)
        };
        statuses
            .iter()
            .map(|(_, samples, ..)| state(samples))
            .collect()
    }

    #[test]
    fn writes_the_status_of_each_start_and_end_after_a_device_error() {
        let steps = vec![Step::Device(ms(10))];
        let out = supervise(0, "script", steps, config(), Cancel::After(ms(5_000)));
        let [(_, Some(ended)), (again, _)] = out.runs[..] else {
            panic!("two runs, the first ended: {:?}", out.runs);
        };
        let ns = |span: Span| Span::from_nanos(span.nanos() + 1);
        let want = [
            (Span::ZERO, (0, 0, 0)),
            (ended, (3, 2, 0)),
            (ns(ended), (1, 2, 0)),
            (again, (0, 2, 1)),
            (ms(5_000), (3, 0, 1)),
            (ns(ms(5_000)), (2, 0, 1)),
        ];
        let at = out.statuses.iter().map(|(at, ..)| *at);
        let got: Vec<_> = at.zip(states(&out.statuses)).collect();
        assert_eq!(got, want);
        out.result.expect("ok after a cancel");
    }

    /// The `(backoff, error)` of each of `statuses`.
    fn errors(statuses: &[Written]) -> Vec<(Span, &str)> {
        statuses
            .iter()
            .map(|(_, samples, text, _)| {
                let [.., backoff] = samples[..4] else {
                    panic!("the samples of the supervisor's channels: {samples:?}");
                };
                (Span::from_nanos(backoff), text.as_str())
            })
            .collect()
    }

    #[test]
    fn writes_the_error_text_and_the_backoff_after_a_device_error() {
        let steps = vec![Step::Device(ms(10))];
        let out = supervise(0, "script", steps, config(), Cancel::After(ms(5_000)));
        let [(_, Some(ended)), (again, _)] = out.runs[..] else {
            panic!("two runs, the first ended: {:?}", out.runs);
        };
        assert!(again > ended, "a wait after the end: {:?}", out.runs);
        let waited = out.statuses[2].0;
        assert!(waited > ended, "1 ns after `state` 3: {:?}", out.statuses);
        let want = [
            (Span::ZERO, ""),
            (Span::ZERO, "no reply"),
            (between(waited, again), "no reply"),
            (Span::ZERO, "no reply"),
            (Span::ZERO, ""),
            (Span::ZERO, ""),
        ];
        assert_eq!(errors(&out.statuses), want);
    }

    #[test]
    fn writes_the_error_that_a_run_shows_and_its_clear() {
        let steps = vec![Step::Show(ms(2_000))];
        let out = supervise(0, "script", steps, config(), Cancel::Never);
        let got: Vec<_> = (out.statuses.iter())
            .zip(states(&out.statuses))
            .map(|((at, _, text, _), state)| (*at, state, text.as_str()))
            .collect();
        let want = [
            (Span::ZERO, (0, 0, 0), ""),
            (ms(1_000), (0, 3, 0), "refused"),
            (ms(4_000), (0, 0, 0), ""),
            (ms(6_000), (3, 0, 0), ""),
            (Span::from_nanos(ms(6_000).nanos() + 1), (2, 0, 0), ""),
        ];
        assert_eq!(got, want);
        out.result.expect("ok");
    }

    #[test]
    fn writes_the_note_that_a_run_shows_beside_its_error_until_the_run_ends() {
        let steps = vec![Step::Note(ms(2_000))];
        let out = supervise(0, "script", steps, config(), Cancel::Never);
        let got: Vec<_> = (out.statuses.iter())
            .zip(states(&out.statuses))
            .map(|((at, _, text, note), state)| {
                (*at, state, text.as_str(), note.as_str())
            })
            .collect();
        let want = [
            (Span::ZERO, (0, 0, 0), "", ""),
            (ms(1_000), (0, 3, 0), "refused", "polls"),
            (ms(4_000), (3, 0, 0), "", ""),
            (Span::from_nanos(ms(4_000).nanos() + 1), (2, 0, 0), "", ""),
        ];
        assert_eq!(got, want);
        out.result.expect("ok");
    }

    #[test]
    fn cuts_a_long_note_that_a_run_shows_at_a_char_boundary() {
        let steps = vec![Step::NoteWide(ms(2_000))];
        let out = supervise(0, "script", steps, config(), Cancel::Never);
        let notes: Vec<_> = out.statuses.iter().map(|(.., note)| note.len()).collect();
        assert_eq!(notes, [0, 1_022, 0, 0]);
        assert_eq!(
            out.statuses[1].3,
            format!("{}…{}", "a".repeat(509), "a".repeat(510))
        );
    }

    #[test]
    fn shows_a_note_whose_text_shows_an_error_as_it_formats() {
        let steps = vec![Step::NoteNested(ms(2_000))];
        let out = supervise(0, "script", steps, config(), Cancel::Never);
        let got: Vec<_> = (out.statuses.iter())
            .map(|(_, _, text, note)| (text.as_str(), note.as_str()))
            .collect();
        assert_eq!(got[1], ("refused", "polls"));
        out.result.expect("ok");
    }

    #[test]
    fn shows_a_note_whose_text_debugs_its_context() {
        let steps = vec![Step::NoteDebug(ms(2_000))];
        let out = supervise(0, "script", steps, config(), Cancel::Never);
        assert!(
            out.statuses[1].3.starts_with("Context {"),
            "{:?}",
            out.statuses
        );
        out.result.expect("ok");
    }

    #[test]
    fn writes_no_note_that_a_task_shows_after_the_run_returned() {
        let out = supervise(
            0,
            "script",
            vec![Step::NoteLate],
            config(),
            Cancel::After(ms(500)),
        );
        out.result.expect("ok after a cancel");
        let got: Vec<_> = (out.statuses.iter())
            .zip(states(&out.statuses))
            .map(|((.., note), state)| (state, note.as_str()))
            .collect();
        assert_eq!(got, [((0, 0, 0), ""), ((3, 0, 0), ""), ((2, 0, 0), "")]);
    }

    /// The seed draws the first wait, so the restart comes before the cancel at
    /// 500 ms or not.
    #[test]
    fn writes_the_end_of_a_run_in_place_of_the_error_it_showed() {
        each_seed("restart before the cancel", |seed| {
            let steps = vec![Step::ShowEnd];
            let cancel = Cancel::After(ms(500));
            let out = supervise(seed, "script", steps, config(), cancel);
            let errors = errors(&out.statuses);
            let texts: Vec<_> = errors.iter().map(|(_, t)| *t).collect();
            let classes: Vec<_> = states(&out.statuses).iter().map(|s| s.1).collect();
            let wait = errors.get(2).map(|(backoff, _)| *backoff);
            let ended = ["", "no reply", "no reply", "no reply"];
            if wait.is_some_and(|wait| wait < ms(500)) {
                assert_eq!(out.runs.len(), 2, "seed {seed}: a restart: {:?}", out.runs);
                assert_eq!(texts.len(), 6, "seed {seed}: {texts:?}");
                assert_eq!(texts[..4], ended, "seed {seed}");
                assert_eq!(texts[4..], ["", ""], "seed {seed}: the cancel clears it");
                assert_eq!(classes, [0, 2, 2, 2, 0, 0], "seed {seed}");
                true
            } else {
                assert_eq!(
                    out.runs.len(),
                    1,
                    "seed {seed}: no restart: {:?}",
                    out.runs
                );
                assert_eq!(texts, ended, "seed {seed}");
                assert_eq!(classes, [0, 2, 2, 2], "seed {seed}");
                false
            }
        });
    }

    #[test]
    fn writes_the_error_that_a_restart_shows_again() {
        let steps = vec![Step::ShowEnd, Step::Show(ms(2_000))];
        let out = supervise(0, "script", steps, config(), Cancel::Never);
        let texts: Vec<_> = errors(&out.statuses).into_iter().map(|(_, t)| t).collect();
        let want = [
            "", "no reply", "no reply", "no reply", "refused", "", "", "",
        ];
        assert_eq!(texts, want);
    }

    #[test]
    fn cuts_the_middle_of_a_long_error_that_a_run_shows() {
        let steps = vec![Step::ShowWide(ms(2_000))];
        let out = supervise(0, "script", steps, config(), Cancel::Never);
        let got: Vec<_> = (states(&out.statuses).into_iter())
            .zip(&out.statuses)
            .map(|(state, (_, _, text, _))| (state, text.as_str()))
            .collect();
        let cut = format!("{0}…{0}", "a".repeat(510));
        assert_eq!(got[1], ((0, 3, 0), cut.as_str()));
    }

    #[test]
    fn writes_a_clear_status_when_a_restart_clears_the_end_before_it() {
        let steps = vec![Step::ShowEnd, Step::Clear(ms(2_000))];
        let out = supervise(0, "script", steps, config(), Cancel::Never);
        let got: Vec<_> = (states(&out.statuses).into_iter())
            .zip(&out.statuses)
            .map(|(state, (at, _, text, _))| (*at, state, text.as_str()))
            .collect();
        let restart = got[3].0;
        let cleared = Span::from_nanos(restart.nanos() + ms(1_000).nanos());
        let want = [(restart, (0, 2, 1), "no reply"), (cleared, (0, 0, 1), "")];
        assert_eq!(got[3..5], want);
    }

    /// The state, the class, and the text of each status that a run of `late`, a
    /// [`Step::Late`], gives, cancelled at `cancel`.
    fn supervise_late(late: Step, cancel: Span) -> Vec<((i64, i64, i64), String)> {
        let out = supervise(0, "script", vec![late], config(), Cancel::After(cancel));
        out.result.expect("ok after a cancel");
        (out.statuses.iter())
            .zip(states(&out.statuses))
            .map(|((_, _, text, _), state)| (state, text.clone()))
            .collect()
    }

    #[test]
    fn keeps_the_ok_end_when_a_task_shows_an_error_after_it() {
        let want = [
            ((0, 0, 0), String::new()),
            ((3, 0, 0), String::new()),
            ((2, 0, 0), String::new()),
        ];
        let late = Step::Late {
            failed: false,
            cleared: false,
        };
        assert_eq!(supervise_late(late, ms(500)), want);
    }

    #[test]
    fn keeps_the_device_end_when_a_task_shows_an_error_after_it() {
        let want = [
            ((0, 0, 0), String::new()),
            ((3, 2, 0), "no reply".to_owned()),
            ((1, 2, 0), "no reply".to_owned()),
            ((0, 2, 1), "no reply".to_owned()),
        ];
        let late = Step::Late {
            failed: true,
            cleared: false,
        };
        assert_eq!(supervise_late(late, ms(5_000))[..4], want);
    }

    #[test]
    fn keeps_the_device_end_when_a_task_clears_the_error_after_it() {
        let want = [
            ((0, 0, 0), String::new()),
            ((3, 2, 0), "no reply".to_owned()),
            ((1, 2, 0), "no reply".to_owned()),
            ((0, 2, 1), "no reply".to_owned()),
        ];
        let late = Step::Late {
            failed: true,
            cleared: true,
        };
        assert_eq!(supervise_late(late, ms(5_000))[..4], want);
    }

    #[test]
    fn writes_no_backoff_when_the_next_try_passed_before_the_write() {
        let (statuses, runs) =
            refuse_the_last_wait(0, vec![Step::Hold(ms(2_000))], Span::ZERO);
        let [_, again] = runs[..] else {
            panic!("two runs: {runs:?}");
        };
        let (at, samples, ..) = &statuses[2];
        assert_eq!(samples[..2], [1, 2], "the wait: {statuses:?}");
        assert_eq!(*at, ms(3_000), "written again 1 s after the refusal at 2 s");
        assert_eq!(again, ms(3_000), "the run waits for the write");
        assert_eq!(samples[3], 0, "the next try passed: {statuses:?}");
    }

    #[test]
    fn writes_the_text_of_a_retry_error() {
        let out = supervise(
            0,
            "script",
            vec![Step::Retry, Step::Done],
            config(),
            Cancel::Never,
        );
        let texts: Vec<_> = errors(&out.statuses).into_iter().map(|(_, t)| t).collect();
        assert_eq!(texts, ["", "busy", "busy", "busy", "", ""]);
    }

    /// A kind that reads its secret source at the start of its run, then returns.
    struct Locked(secret::Source);

    impl Kind for Locked {
        type Config = ();

        fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
            Ok(())
        }

        fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
            Ok(Channels::default())
        }

        async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
            let value = ctx.secret(&self.0).await?;
            assert_eq!(value.expose(), b"t");
            Ok(())
        }
    }

    #[test]
    fn heals_a_run_when_the_missing_secret_is_written() {
        let (result, statuses) = run_on(|node, tasks| async move {
            let kinds = Table::new().with(
                "locked",
                Locked(secret::Source::Secret(name("plant.token"))),
            );
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.locked").await;
            let statuses = read_status(&inputs.hub, "plant.locked", &[], &tasks).await;
            let (shown, files, clock) =
                (Rc::clone(&statuses), node.files(), node.clock());
            tasks.spawn(async move {
                while shown.borrow().len() < 2 {
                    clock.sleep(ms(10)).await;
                }
                testing::create_secret(&files, &name("plant.token"), b"t").await;
            });
            let token = Token::new();
            let result = Supervisor::new(inputs)
                .open("locked", name("plant.locked"), &config(), &token)
                .await
                .run()
                .await;
            node.clock().sleep(Span::SECOND).await;
            (result, statuses.borrow().clone())
        });
        result.expect("the second run reads the secret");
        let texts: Vec<_> = errors(&statuses).into_iter().map(|(_, t)| t).collect();
        let missing = "the secret secret(\"plant.token\"): the file \
                       /data/secrets/plant.token is not there. Write the value to it, \
                       unreadable by other users: chmod o-r /data/secrets/plant.token";
        assert_eq!(texts, ["", missing, missing, missing, "", ""]);
    }

    #[test]
    fn heals_a_run_when_a_secret_file_too_large_is_cut() {
        let path = "/run/secrets/plant.token";
        let (result, statuses) = run_on(move |node, tasks| async move {
            let source = secret::Source::File(Absolute::new(path).expect("absolute"));
            let kinds = Table::new().with("locked", Locked(source));
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.locked").await;
            let statuses = read_status(&inputs.hub, "plant.locked", &[], &tasks).await;
            let paths = node.paths();
            let large = vec![b't'; (1 << 16) + 1];
            let path = std::path::Path::new(path);
            node.make_dir(path.parent().expect("a directory"));
            paths.write(path, &large).expect("writes it");
            let (shown, clock) = (Rc::clone(&statuses), node.clock());
            tasks.spawn(async move {
                while shown.borrow().len() < 2 {
                    clock.sleep(ms(10)).await;
                }
                paths
                    .write(std::path::Path::new(path), b"t")
                    .expect("writes it");
            });
            let token = Token::new();
            let result = Supervisor::new(inputs)
                .open("locked", name("plant.locked"), &config(), &token)
                .await
                .run()
                .await;
            node.clock().sleep(Span::SECOND).await;
            (result, statuses.borrow().clone())
        });
        result.expect("the second run reads the secret");
        let texts: Vec<_> = errors(&statuses).into_iter().map(|(_, t)| t).collect();
        let large = "the secret file(\"/run/secrets/plant.token\"): the file \
                     /run/secrets/plant.token is 65537 bytes, more than 65536. Make it \
                     at most 65536 bytes";
        assert_eq!(texts, ["", large, large, large, "", ""]);
    }

    #[test]
    fn ends_a_run_whose_secret_name_the_store_cannot_hold_without_a_restart() {
        let (result, statuses) = run_on(|node, tasks| async move {
            let kinds = Table::new().with(
                "locked",
                Locked(secret::Source::Secret(name("plant.Token"))),
            );
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.locked").await;
            let statuses = read_status(&inputs.hub, "plant.locked", &[], &tasks).await;
            let token = Token::new();
            let result = Supervisor::new(inputs)
                .open("locked", name("plant.locked"), &config(), &token)
                .await
                .run()
                .await;
            node.clock().sleep(ms(10_000)).await;
            (result, statuses.borrow().clone())
        });
        let message =
            "the secret secret(\"plant.Token\") has a name that the store cannot hold";
        let fix = "Rename the secret in lowercase letters, since the file store holds \
                   no name with a capital letter";
        let code = Code::new("connector.bad-secret-name");
        let bad = Diagnostic::new(code, None, message.into(), fix.into());
        assert_eq!(config_errors(result), [bad]);
        assert_eq!(states(&statuses), [(0, 0, 0), (3, 1, 0), (2, 1, 0)]);
        let texts: Vec<_> = errors(&statuses).into_iter().map(|(_, t)| t).collect();
        let shown = format!("{message}. {fix}");
        assert_eq!(texts, ["", &shown, &shown]);
    }

    #[test]
    fn writes_the_text_of_a_config_error() {
        let out = supervise(0, "script", vec![Step::Config], config(), Cancel::Never);
        let text = bad().to_string();
        let want = [(Span::ZERO, ""), (Span::ZERO, &*text), (Span::ZERO, &*text)];
        assert_eq!(errors(&out.statuses), want);
    }

    #[test]
    fn joins_the_texts_of_the_diagnostics_of_a_config_error() {
        let out = supervise(0, "script", vec![Step::Configs], config(), Cancel::Never);
        let text = format!("{}; {}", bad(), worse());
        assert_eq!(out.statuses[1].2, text);
    }

    #[test]
    fn writes_no_backoff_when_a_cancel_ends_the_wait() {
        let steps = vec![Step::Device(Span::ZERO)];
        let out = supervise(
            0,
            "script",
            steps,
            config(),
            Cancel::After(Span::from_nanos(1)),
        );
        let states: Vec<_> = out.statuses.iter().map(|(_, s, ..)| s[0]).collect();
        assert_eq!(states, [0, 3, 1, 2], "a cancel in the wait");
        assert!(out.statuses[2].1[3] > 0, "a wait: {:?}", out.statuses);
        assert_eq!(errors(&out.statuses)[3], (Span::ZERO, "no reply"));
    }

    #[test]
    fn cuts_the_middle_of_a_long_error_text() {
        let out = supervise(
            0,
            "script",
            vec![Step::Wide],
            config(),
            Cancel::After(ms(5_000)),
        );
        assert_eq!(out.statuses[1].2, format!("{0}…{0}", "a".repeat(510)));
    }

    /// The 255th `é` takes bytes 510 and 511, across the bound of 510 bytes of the
    /// head.
    #[test]
    fn cuts_a_long_error_text_at_a_char_boundary() {
        let out = supervise(
            0,
            "script",
            vec![Step::Long],
            config(),
            Cancel::After(ms(5_000)),
        );
        let cut = format!("a{}…{}", "é".repeat(254), "é".repeat(255));
        assert_eq!(out.statuses[1].2, cut);
    }

    /// Runs `steps` and fills the pool from 1.5 s after the start of the last run
    /// for 1 s, so the home refuses the frame of the wait after it. The last step
    /// holds a task for 2 s. The wall steps forward by `wall` 2.001 s after the start
    /// of the last run. Runs in a sim of `seed`. Returns the status and the start of
    /// each run.
    fn refuse_the_last_wait(
        seed: u64,
        steps: Vec<Step>,
        wall: Span,
    ) -> (Vec<Written>, Vec<Span>) {
        let count = steps.len();
        run_in(seed, move |node, tasks| async move {
            let script = Script {
                steps: Mutex::new(steps.into()),
                ..Script::default()
            };
            let runs = Arc::clone(&script.runs);
            let kinds = Table::new().with("script", script);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.script").await;
            let statuses = read_status(&inputs.hub, "plant.script", &[], &tasks).await;
            let channels =
                ["state", "error"].map(|c| name(&format!("plant.script.status.{c}")));
            let hog =
                open_writer(&inputs.hub, "plant.other", Authority(1), channels.into())
                    .await;
            let (token, clock) = (Token::new(), node.clock());
            let (canceller, sleeper) = (token.clone(), clock.clone());
            let (filled, node) = (Arc::clone(&runs), node.clone());
            tasks.spawn(async move {
                while filled.lock().expect("no panic under the lock").len() < count {
                    sleeper.sleep(ms(10)).await;
                }
                sleeper.sleep(ms(1_500)).await;
                let held = fill(&hog);
                sleeper.sleep(ms(501)).await;
                node.step_wall(wall);
                sleeper.sleep(ms(499)).await;
                drop(held);
                sleeper.sleep(ms(5_000)).await;
                canceller.cancel();
            });
            let (supervisor, start) = (Supervisor::new(inputs), clock.now());
            let name = name("plant.script");
            let result = supervisor
                .open("script", name, &config(), &token)
                .await
                .run()
                .await;
            result.expect("ok after a cancel");
            clock.sleep(Span::SECOND).await;
            let runs: Vec<_> = (runs.lock().expect("no panic under the lock").iter())
                .map(|(from, _)| *from - start)
                .collect();
            (statuses.borrow().clone(), runs)
        })
    }

    #[test]
    fn writes_the_backoff_that_is_left_when_the_home_refused_the_wait() {
        each_seed("wait longer than 1 s", |seed| {
            let steps = vec![Step::Device(Span::ZERO), Step::Hold(ms(2_000))];
            let (statuses, runs) = refuse_the_last_wait(seed, steps, Span::ZERO);
            let [_, second, third] = runs[..] else {
                panic!("seed {seed}: three runs: {runs:?}");
            };
            let Some((at, samples, ..)) = statuses.get(5) else {
                panic!("seed {seed}: a sixth status: {statuses:?}");
            };
            assert_eq!(
                samples[..2],
                [1, 2],
                "seed {seed}: the second wait: {statuses:?}"
            );
            let again = Span::from_nanos(second.nanos() + ms(3_000).nanos());
            assert_eq!(
                *at, again,
                "seed {seed}: written again 1 s after the refusal at the end"
            );
            let left = Span::from_nanos(samples[3]);
            assert_eq!(
                left,
                between(*at, third),
                "seed {seed}: the wait from the write to the run: {statuses:?}"
            );
            left > Span::ZERO
        });
    }

    /// The hub reads the wall up to 1 s late, so the step can show after the write of
    /// the backoff, at a few seeds. A backoff of zero shows no slew. The test needs
    /// 128 seeds of 512 that show both, and one with a backoff whose write does not
    /// show the step.
    #[test]
    fn writes_the_backoff_that_is_left_after_a_wall_step_in_the_wait() {
        let steps = vec![Step::Device(Span::ZERO), Step::Hold(ms(2_000))];
        let wall = ms(10_000);
        let (seeds, mut slewed, mut late) = (8 * SEEDS, 0, 0);
        for seed in 0..seeds {
            let (statuses, _) = refuse_the_last_wait(seed, steps.clone(), wall);
            let (before, ..) = &statuses[4];
            let (at, samples, ..) = &statuses[5];
            assert_eq!(
                samples[..2],
                [1, 2],
                "seed {seed}: the second wait: {statuses:?}"
            );
            let (run, again, ..) = &statuses[6];
            assert_eq!(again[0], 0, "seed {seed}: the next try: {statuses:?}");
            let (backoff, wait) = (samples[3], between(*at, *run).nanos());
            if backoff == 0 {
                assert_eq!(wait, 1, "seed {seed}: the next ns: {statuses:?}");
                continue;
            }
            if between(*before, *at) < wall {
                late += 1;
                continue;
            }
            // `backoff` is the wait by the node's clock. After the step, the hub's
            // time slews toward the wall at the 500 ppm of MESH SLEW, 1 ns at each
            // 2 us tick. The wait holds `backoff / 2_000` ticks or one more, and a
            // status write takes at least the ns after the one before it.
            let gain = wait - backoff - backoff / 2_000;
            assert!(
                gain == 0 || gain == 1,
                "seed {seed}: the wait of {wait} ns gains {gain} ns over the backoff \
                 of {backoff} ns and its slew, not 0 or 1: {statuses:?}"
            );
            slewed += 1;
        }
        assert!(
            slewed >= 128 && late >= 1,
            "of {seeds} seeds, {slewed} show the step and a slew, and {late} show the \
             step after the write"
        );
    }

    #[test]
    fn writes_the_class_of_a_retry_error() {
        let out = supervise(
            0,
            "script",
            vec![Step::Retry, Step::Done],
            config(),
            Cancel::Never,
        );
        let want = [
            (0, 0, 0),
            (3, 3, 0),
            (1, 3, 0),
            (0, 3, 1),
            (3, 0, 1),
            (2, 0, 1),
        ];
        assert_eq!(states(&out.statuses), want);
    }

    /// A kind returns each end of a session with `?`, and the status gives its class
    /// and its text.
    #[test]
    fn writes_the_class_of_each_end_that_a_kind_returns() {
        use hub::home::Error as Home;
        use hub::reader::Ended;
        use hub::writer::Failure;
        let key = channel::Key::from_u128(2);
        let full = || env::files::Error::Full {
            path: "buffer".into(),
        };
        let ended = |ended: Ended| (ended.to_string(), Step::Ended(ended));
        let failed = |failure: Failure| (failure.to_string(), Step::Failed(failure));
        let pool = |error: block::Error| {
            let error = frame::Error::Pool(error);
            (error.to_string(), Step::Draft(error))
        };
        let retries = [
            ended(Ended::Removed(key)),
            ended(Ended::Replaced),
            ended(Ended::Buffer(full())),
            failed(Failure::Removed(key)),
            failed(Failure::Home(Home::Full)),
            failed(Failure::Home(Home::Disk(full()))),
            pool(block::Error::Exhausted {
                requested: 16,
                available: 8,
            }),
            pool(block::Error::Refused { requested: 16 }),
        ];
        let retry = [
            (0, 0, 0),
            (3, 3, 0),
            (1, 3, 0),
            (0, 3, 1),
            (3, 0, 1),
            (2, 0, 1),
        ];
        for (text, step) in retries {
            let steps = vec![step, Step::Done];
            let out = supervise(0, "script", steps, config(), Cancel::Never);
            assert_eq!(states(&out.statuses), retry, "{text}");
            assert_eq!(errors(&out.statuses)[1], (Span::ZERO, text.as_str()));
        }
        let (_, large) = failed(Failure::Home(Home::Large));
        let out = supervise(0, "script", vec![large], config(), Cancel::Never);
        assert_eq!(states(&out.statuses), [(0, 0, 0), (3, 1, 0), (2, 1, 0)]);
        let text = "the frame is too large for one write: split it. Give the connector \
                    fewer channels";
        assert_eq!(errors(&out.statuses)[1], (Span::ZERO, text));
        let want = unplaced(
            "connector.large-frame",
            "the frame is too large for one write: split it",
            "Give the connector fewer channels",
        );
        assert_eq!(out.result.map_err(diagnostics), Err(Ok(want)));
        let (_, large) = pool(block::Error::TooLarge {
            requested: 16,
            largest: 8,
        });
        let out = supervise(0, "script", vec![large], config(), Cancel::Never);
        assert_eq!(states(&out.statuses), [(0, 0, 0), (3, 1, 0), (2, 1, 0)]);
        let text = "no block for the frame: block of 16 bytes is above the largest \
                    block of 8 bytes. Give the connector fewer channels";
        assert_eq!(errors(&out.statuses)[1], (Span::ZERO, text));
        let want = unplaced(
            "connector.large-frame",
            "no block for the frame: block of 16 bytes is above the largest block of 8 \
             bytes",
            "Give the connector fewer channels",
        );
        assert_eq!(out.result.map_err(diagnostics), Err(Ok(want)));
    }

    /// A draft of series that do not follow the key set is a defect of the kind.
    #[test]
    #[should_panic(
        expected = "invariant: the series follow the key set: entry 3 is out of range"
    )]
    fn panics_when_a_kind_returns_series_that_do_not_follow_the_key_set() {
        let error = frame::Error::OutOfRange {
            entry: 3,
            entries: 2,
        };
        supervise(
            0,
            "script",
            vec![Step::Draft(error)],
            config(),
            Cancel::Never,
        );
    }

    #[test]
    #[should_panic(expected = "invariant: the series follow the key set: entry 1 \
                               follows entry 2: entries must increase")]
    fn panics_when_a_kind_returns_series_out_of_order() {
        let error = frame::Error::Unordered { entry: 1, last: 2 };
        supervise(
            0,
            "script",
            vec![Step::Draft(error)],
            config(),
            Cancel::Never,
        );
    }

    #[test]
    #[should_panic(expected = "invariant: the series follow the key set: entry 2 is \
                               present but its index, entry 0, is absent")]
    fn panics_when_a_kind_returns_a_series_with_no_index() {
        let error = frame::Error::IndexAbsent { entry: 2, index: 0 };
        supervise(
            0,
            "script",
            vec![Step::Draft(error)],
            config(),
            Cancel::Never,
        );
    }

    /// No draft gives an end of another node.
    #[test]
    #[should_panic(expected = "invariant: a draft gives no end of another node: the \
                               ends do not fit the series")]
    fn panics_when_a_kind_returns_an_end_of_another_node() {
        let error = frame::Error::End(frame::BadEnd::Before { end: 4, start: 8 });
        supervise(
            0,
            "script",
            vec![Step::Draft(error)],
            config(),
            Cancel::Never,
        );
    }

    #[test]
    fn writes_stopped_after_a_config_error_from_run() {
        let out = supervise(0, "script", vec![Step::Config], config(), Cancel::Never);
        assert_eq!(states(&out.statuses), [(0, 0, 0), (3, 1, 0), (2, 1, 0)]);
    }

    #[test]
    fn writes_stopped_after_a_run_returns_ok() {
        let out = supervise(0, "script", vec![Step::Done], config(), Cancel::Never);
        assert_eq!(states(&out.statuses), [(0, 0, 0), (3, 0, 0), (2, 0, 0)]);
    }

    #[test]
    fn writes_only_stopped_when_cancelled_before_the_call() {
        let out = supervise(0, "script", vec![Step::Done], config(), Cancel::Now);
        assert_eq!(states(&out.statuses), [(2, 0, 0)]);
    }

    #[test]
    fn writes_stopped_when_cancelled_during_the_backoff() {
        let steps = vec![Step::Device(Span::ZERO)];
        let out = supervise(
            0,
            "script",
            steps,
            config(),
            Cancel::After(Span::from_nanos(1)),
        );
        let want = [(0, 0, 0), (3, 2, 0), (1, 2, 0), (2, 2, 0)];
        assert_eq!(states(&out.statuses), want);
    }

    #[test]
    fn writes_no_waiting_after_a_cancel() {
        let steps = vec![Step::Linger(ms(30))];
        let out = supervise(0, "script", steps, config(), Cancel::After(ms(50)));
        let want = [(0, 0, 0), (3, 2, 0), (2, 2, 0)];
        assert_eq!(states(&out.statuses), want);
    }

    #[test]
    fn writes_ending_until_the_tasks_of_a_run_end() {
        let steps = vec![Step::Hold(ms(2_000)), Step::Done];
        let out = supervise(0, "script", steps, config(), Cancel::Never);
        let at = |i: usize| out.statuses[i].0;
        let want = [
            (0, 0, 0),
            (3, 2, 0),
            (1, 2, 0),
            (0, 2, 1),
            (3, 0, 1),
            (2, 0, 1),
        ];
        assert_eq!(states(&out.statuses), want);
        assert_eq!((at(1), at(2)), (Span::from_nanos(1), ms(2_000)));
    }

    #[test]
    fn writes_a_status_that_the_home_refused_again_a_second_later() {
        let statuses = run_on(|node, tasks| async move {
            let script = Script {
                steps: Mutex::new([Step::Hold(ms(2_000))].into()),
                ..Script::default()
            };
            let kinds = Table::new().with("script", script);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.script").await;
            let statuses = read_status(&inputs.hub, "plant.script", &[], &tasks).await;
            let channels = vec![name("plant.script.status.state")];
            let holder =
                open_writer(&inputs.hub, "plant.other", Authority::ABSOLUTE, channels)
                    .await;
            let (token, clock) = (Token::new(), node.clock());
            let (canceller, sleeper) = (token.clone(), clock.clone());
            tasks.spawn(async move {
                sleeper.sleep(ms(500)).await;
                drop(holder);
                sleeper.sleep(ms(4_500)).await;
                canceller.cancel();
            });
            let supervisor = Supervisor::new(inputs);
            let name = name("plant.script");
            let result = supervisor
                .open("script", name, &config(), &token)
                .await
                .run()
                .await;
            result.expect("ok after a cancel");
            clock.sleep(Span::SECOND).await;
            statuses.borrow().clone()
        });
        let at = |i: usize| statuses[i].0;
        let want = [
            (0, 0, 0),
            (3, 2, 0),
            (1, 2, 0),
            (0, 2, 1),
            (3, 0, 1),
            (2, 0, 1),
        ];
        assert_eq!(states(&statuses), want, "the frames of 0 s were refused");
        assert_eq!(
            (at(1), at(2)),
            (Span::from_nanos(1), Span::SECOND),
            "`state` 0 written again at 1 s, `state` 3 after it, and `state` 1 when \
             the tasks ended at 2 s"
        );
    }

    /// The status frames of one connector of [`Script`] that plays `steps`, whose
    /// frames another writer refuses until `held`, in a sim of `seed`, and when the
    /// call returned.
    fn refused_until(
        seed: u64,
        steps: Vec<Step>,
        held: Span,
        cancel: Option<Span>,
    ) -> (Vec<Written>, Span) {
        run_in(seed, move |node, tasks| async move {
            let script = Script {
                steps: Mutex::new(steps.into()),
                ..Script::default()
            };
            let kinds = Table::new().with("script", script);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.script").await;
            let statuses = read_status(&inputs.hub, "plant.script", &[], &tasks).await;
            let channels = vec![name("plant.script.status.state")];
            let holder =
                open_writer(&inputs.hub, "plant.other", Authority::ABSOLUTE, channels)
                    .await;
            let (token, clock) = (Token::new(), node.clock());
            let (canceller, sleeper) = (token.clone(), clock.clone());
            tasks.spawn(async move {
                sleeper.sleep(held).await;
                drop(holder);
            });
            if let Some(cancel) = cancel {
                let sleeper = clock.clone();
                tasks.spawn(async move {
                    sleeper.sleep(cancel).await;
                    canceller.cancel();
                });
            }
            let (supervisor, start) = (Supervisor::new(inputs), clock.now());
            let name = name("plant.script");
            let result = supervisor
                .open("script", name, &config(), &token)
                .await
                .run()
                .await;
            result.expect("ok");
            let returned = clock.now() - start;
            clock.sleep(ms(5_000)).await;
            let statuses = statuses.borrow().clone();
            (statuses, returned)
        })
    }

    #[test]
    fn writes_a_refused_last_frame_again_before_the_call_returns() {
        let (statuses, returned) = refused_until(0, vec![Step::Done], ms(500), None);
        assert_eq!(
            states(&statuses),
            [(0, 0, 0), (3, 0, 0), (2, 0, 0)],
            "the frame of `state` 0 written again at 1 s, then each state after it"
        );
        assert_eq!(
            returned,
            Span::SECOND,
            "returns once the frame of 1 s applied"
        );
    }

    #[test]
    fn writes_a_last_frame_refused_twice_until_the_home_applies_it() {
        let (statuses, returned) = refused_until(0, vec![Step::Done], ms(1_500), None);
        assert_eq!(
            states(&statuses),
            [(0, 0, 0), (3, 0, 0), (2, 0, 0)],
            "the frame of `state` 0 refused at 0 s and 1 s"
        );
        assert_eq!(returned, ms(2_000), "returns once the frame of 2 s applied");
    }

    #[test]
    fn writes_a_frame_refused_as_ahead_again_at_the_next_flush() {
        let (statuses, returned) = run_on(|node, tasks| async move {
            let script = Script {
                steps: Mutex::new([Step::Done].into()),
                ..Script::default()
            };
            let kinds = Table::new().with("script", script);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.script").await;
            let statuses = read_status(&inputs.hub, "plant.script", &[], &tasks).await;
            write_ahead(&inputs.hub, Span::SECOND).await;
            let (clock, token) = (node.clock(), Token::new());
            let (supervisor, start) = (Supervisor::new(inputs), clock.now());
            let result = supervisor
                .open("script", name("plant.script"), &config(), &token)
                .await
                .run()
                .await;
            result.expect("ok");
            let returned = clock.now() - start;
            clock.sleep(ms(5_000)).await;
            let statuses = statuses.borrow().clone();
            (statuses, returned)
        });
        // The other frame is at 1 s, and the frames of 0 s are written again after it,
        // where each is ahead of the mesh time until the flush of 1 s.
        let want = [
            (Span::ZERO, vec![9, 9, 9, 9], String::new(), String::new()),
            (
                Span::from_nanos(2),
                vec![0, 0, 0, 0],
                String::new(),
                String::new(),
            ),
            (
                Span::from_nanos(3),
                vec![3, 0, 0, 0],
                String::new(),
                String::new(),
            ),
            (
                Span::from_nanos(4),
                vec![2, 0, 0, 0],
                String::new(),
                String::new(),
            ),
        ];
        assert_eq!(statuses, want);
        assert_eq!(returned, Span::SECOND);
    }

    #[test]
    fn returns_after_a_bound_on_each_outage_while_the_home_refuses_each_frame() {
        let (statuses, returned) = refused_until(0, vec![Step::Done], ms(60_000), None);
        assert_eq!(
            states(&statuses),
            [],
            "the call returned before the home applied"
        );
        assert_eq!(
            returned,
            ms(20_000),
            "10 s from `state` 0, then `state` 3 and `state` 2 at once, and 10 s for \
             the last frame"
        );
    }

    #[test]
    fn restarts_after_a_bound_on_each_outage_while_the_home_refuses_each_frame() {
        let steps = vec![Step::Retry, Step::Done];
        let (statuses, returned) = refused_until(0, steps, ms(15_000), None);
        assert_eq!(
            states(&statuses),
            [(2, 0, 1)],
            "the first frame that the home applies holds the restart"
        );
        assert!(
            returned > ms(15_000) && returned <= ms(16_000),
            "returns at the first write after the home applies: {returned:?}"
        );
    }

    /// The other writer closes at 5 s, as the frame is written again. The seed
    /// picks which of the two runs first, so the home applies the frame of 5 s or
    /// the one of 6 s.
    #[test]
    fn writes_a_last_frame_refused_for_5_s_until_the_home_applies_it() {
        each_seed("close the writer before the write at 5 s", |seed| {
            let steps = vec![Step::Done];
            let (statuses, returned) = refused_until(seed, steps, ms(5_000), None);
            assert_eq!(
                states(&statuses),
                [(0, 0, 0), (3, 0, 0), (2, 0, 0)],
                "seed {seed}: each state after the one that the home refused waits \
                 for it"
            );
            match returned {
                at if at == ms(5_000) => true,
                at if at == ms(6_000) => false,
                at => panic!("seed {seed}: returns at {at:?}, not at 5 s or 6 s"),
            }
        });
    }

    #[test]
    fn writes_a_frame_refused_for_9_5_s_before_the_next_state() {
        let (statuses, returned) = refused_until(0, vec![Step::Done], ms(9_500), None);
        assert_eq!(
            states(&statuses),
            [(0, 0, 0), (3, 0, 0), (2, 0, 0)],
            "the write at 10 s comes before the bound"
        );
        assert_eq!(returned, ms(10_000));
    }

    #[test]
    fn returns_at_a_cancel_while_the_last_frame_waits() {
        let (statuses, returned) =
            refused_until(0, vec![Step::Done], ms(2_500), Some(ms(500)));
        assert_eq!(states(&statuses), [], "no frame after the cancel");
        assert_eq!(returned, ms(500));
    }

    #[test]
    fn writes_the_last_status_after_the_tasks_of_a_run_while_the_pool_is_full() {
        let (statuses, returned) = run_on(|node, tasks| async move {
            let script = Script {
                steps: Mutex::new([Step::Refuse(ms(1_000))].into()),
                ..Script::default()
            };
            let kinds = Table::new().with("script", script);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.script").await;
            let statuses = read_status(&inputs.hub, "plant.script", &[], &tasks).await;
            let channels = vec![name("plant.script.status.state")];
            let hog =
                open_writer(&inputs.hub, "plant.script", Authority(1), channels).await;
            let clock = node.clock();
            let sleeper = clock.clone();
            tasks.spawn(async move {
                sleeper.sleep(ms(500)).await;
                let held = fill(&hog);
                sleeper.sleep(ms(2_000)).await;
                drop(held);
            });
            let (supervisor, start) = (Supervisor::new(inputs), clock.now());
            let name = name("plant.script");
            let result = supervisor
                .open("script", name, &config(), &Token::new())
                .await
                .run()
                .await;
            assert_eq!(config_errors(result), [bad()]);
            let returned = clock.now() - start;
            clock.sleep(ms(5_000)).await;
            let statuses = statuses.borrow().clone();
            (statuses, returned)
        });
        let last = statuses.last().map(|(at, samples, ..)| (*at, samples[0]));
        assert_eq!(last, Some((ms(3_000), 2)), "{statuses:?}");
        assert_eq!(returned, ms(3_000), "returns once the home applied state 2");
    }

    /// After the removal at 500 ms, the end of the first run at 900 ms gets `Removed`
    /// and closes the writer, while its flush sleeps until 1 s. The flush ends at
    /// that wake, which also polls a restart that the seed draws before 1 s. Its
    /// timers wake only when due.
    #[test]
    fn wakes_a_restart_once_more_after_a_change_of_state_closed_the_sleeping_flush() {
        each_seed("restart before the wake of the flush", |seed| {
            let polls = Arc::new(Mutex::new(Vec::new()));
            let runs = Arc::new(Mutex::new(Vec::new()));
            let (counted, started) = (Arc::clone(&polls), Arc::clone(&runs));
            let run = move |ctx: Context<()>| {
                let polls = Arc::clone(&counted);
                let mut runs = started.lock().expect("no panic under the lock");
                runs.push(ctx.clock().now());
                let first = runs.len() == 1;
                async move {
                    if first {
                        ctx.clock().sleep(ms(100)).await;
                        ctx.count("samples").set(1);
                        ctx.clock().sleep(ms(800)).await;
                        return Err(Error::Retry("busy".into()));
                    }
                    ctx.count("samples").set(2);
                    let mut sleep = pin!(ctx.clock().sleep(ms(3_000)));
                    poll_fn(|cx| {
                        let now = ctx.clock().now();
                        polls.lock().expect("no panic under the lock").push(now);
                        sleep.as_mut().poll(cx)
                    })
                    .await;
                    Ok(())
                }
            };
            timed_due(seed, run, |scene| async move {
                scene.node.clock().sleep(ms(500)).await;
                scene.remove("samples");
            });
            let runs = runs.lock().expect("no panic under the lock").clone();
            let [first, second] = runs[..] else {
                panic!("seed {seed}: one restart: {runs:?}");
            };
            let polls: Vec<_> = (polls.lock().expect("no panic under the lock").iter())
                .map(|at| *at - first)
                .collect();
            let (start, woke) = (second - first, ms(1_000));
            let end = Span::from_nanos(start.nanos() + ms(3_000).nanos());
            if start < woke {
                assert_eq!(
                    polls,
                    [start, woke, end],
                    "seed {seed}: a poll at the start, at the wake of the flush, and \
                     at the end"
                );
                return true;
            }
            assert_eq!(
                polls,
                [start, end],
                "seed {seed}: a poll to start the sleep, and one at its end"
            );
            false
        });
    }

    /// It states one wake at the deadline, so its timers wake only when due.
    #[test]
    fn wakes_nothing_after_a_close_in_the_poll_that_staged_a_count() {
        let polls = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let (counted, started) = (Arc::clone(&polls), Arc::clone(&runs));
        let run = move |ctx: Context<()>| {
            let polls = Arc::clone(&counted);
            let first = started.fetch_add(1, Ordering::Relaxed) == 0;
            async move {
                if first {
                    ctx.clock().sleep(ms(900)).await;
                    ctx.count("samples").set(1);
                    return Err(Error::Retry("busy".into()));
                }
                ctx.count("samples").set(2);
                let mut sleep = pin!(ctx.clock().sleep(ms(3_000)));
                poll_fn(|cx| {
                    polls.fetch_add(1, Ordering::Relaxed);
                    sleep.as_mut().poll(cx)
                })
                .await;
                Ok(())
            }
        };
        timed_due(0, run, |scene| async move {
            scene.node.clock().sleep(ms(500)).await;
            scene.remove("samples");
        });
        assert_eq!(runs.load(Ordering::Relaxed), 2, "one restart");
        let polls = polls.load(Ordering::Relaxed);
        assert_eq!(polls, 2, "a poll to start the sleep, and one at its end");
    }

    #[test]
    fn returns_at_once_when_the_pool_fills_after_a_removed_status_channel() {
        let run = |ctx: Context<()>| async move {
            ctx.count("samples").set(1);
            ctx.clock().sleep(ms(1_500)).await;
            Ok(())
        };
        let returned = timed(run, |scene| async move {
            let clock = scene.node.clock();
            clock.sleep(ms(100)).await;
            let channels = status_names("plant.tally", &["samples"]);
            let hog = rival(&scene.hub, channels).await;
            clock.sleep(ms(400)).await;
            scene.remove("samples");
            clock.sleep(ms(700)).await;
            let held = fill(&hog);
            clock.sleep(ms(20_000)).await;
            drop(held);
        });
        assert_eq!(returned, ms(1_500), "no change of state waits after 1 s");
    }

    #[test]
    fn returns_at_once_when_the_pool_fills_after_a_failed_disk() {
        let run = |ctx: Context<()>| async move {
            ctx.count("samples").set(1);
            ctx.clock().sleep(ms(1_100)).await;
            ctx.count("samples").set(2);
            ctx.clock().sleep(ms(1_400)).await;
            Ok(())
        };
        let returned = timed(run, |scene| async move {
            let clock = scene.node.clock();
            clock.sleep(ms(100)).await;
            let channels = status_names("plant.tally", &["samples"]);
            let hog = rival(&scene.hub, channels).await;
            clock.sleep(ms(400)).await;
            scene
                .node
                .fail_file(RING.as_ref(), env::files::Operation::Sync);
            clock.sleep(ms(1_700)).await;
            let held = fill(&hog);
            clock.sleep(ms(20_000)).await;
            drop(held);
        });
        assert_eq!(returned, ms(2_500), "no change of state waits after 2 s");
    }

    /// A kind with `N` counts, whose run returns `Ok` at once.
    struct Wide;

    impl Wide {
        // 16 bytes a count.
        const N: usize = 500_000;

        fn counts() -> Vec<Name> {
            (0..Self::N).map(|i| name(&format!("c{i}"))).collect()
        }
    }

    impl Kind for Wide {
        type Config = ();

        fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
            Ok(())
        }

        fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
            Ok(Channels {
                counts: Self::counts(),
                ..Channels::default()
            })
        }

        fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), Error>> {
            std::future::ready(Ok(()))
        }
    }

    /// Matches only the start: the sizes come from `types` and `home`.
    #[test]
    #[should_panic(expected = "invariant: a status frame fits the largest block")]
    fn panics_at_the_start_of_a_status_larger_than_the_largest_block() {
        run_on(|node, tasks| async move {
            let kinds = Table::new().with("wide", Wide);
            let inputs = create_config(&node, tasks.clone(), kinds, "plant.wide").await;
            let connector = name("plant.wide");
            let status = testing::create_status(&connector, &Wide::counts(), STATUS);
            inputs
                .hub
                .set_definitions(status.iter().map(|(name, def)| (name, def)));
            let result = Supervisor::new(inputs)
                .open("wide", connector, &config(), &Token::new())
                .await
                .run()
                .await;
            result.expect("the run returns ok");
        });
    }

    #[test]
    fn wakes_nothing_at_a_count_set_after_its_status_channels_are_removed() {
        let polls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&polls);
        let run = move |ctx: Context<()>| {
            let polls = Arc::clone(&counted);
            async move {
                ctx.count("samples").set(1);
                ctx.clock().sleep(ms(1_500)).await;
                ctx.count("samples").set(2);
                let mut sleep = pin!(ctx.clock().sleep(Span::SECOND));
                poll_fn(|cx| {
                    polls.fetch_add(1, Ordering::Relaxed);
                    sleep.as_mut().poll(cx)
                })
                .await;
                Ok(())
            }
        };
        timed(run, |scene| async move {
            scene.node.clock().sleep(ms(500)).await;
            scene.remove("samples");
        });
        let polls = polls.load(Ordering::Relaxed);
        assert_eq!(polls, 2, "a poll to start the sleep, and one at its end");
    }

    /// It states one wake at the deadline, so its timers wake only when due.
    #[test]
    fn wakes_nothing_at_a_count_set_after_a_change_of_state_closed_the_writer() {
        let polls = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let (counted, started) = (Arc::clone(&polls), Arc::clone(&runs));
        let run = move |ctx: Context<()>| {
            let polls = Arc::clone(&counted);
            let first = started.fetch_add(1, Ordering::Relaxed) == 0;
            async move {
                if first {
                    ctx.count("samples").set(1);
                    ctx.clock().sleep(ms(2_000)).await;
                    return Err(Error::Retry("busy".into()));
                }
                ctx.count("samples").set(2);
                let mut sleep = pin!(ctx.clock().sleep(ms(3_000)));
                poll_fn(|cx| {
                    polls.fetch_add(1, Ordering::Relaxed);
                    sleep.as_mut().poll(cx)
                })
                .await;
                Ok(())
            }
        };
        timed_due(0, run, |scene| async move {
            scene.node.clock().sleep(ms(1_500)).await;
            scene.remove("samples");
        });
        assert_eq!(runs.load(Ordering::Relaxed), 2, "one restart");
        let polls = polls.load(Ordering::Relaxed);
        assert_eq!(polls, 2, "a poll to start the sleep, and one at its end");
    }

    #[test]
    fn waits_for_a_full_pool_after_its_status_channels_are_removed() {
        assert_eq!(
            removed_with_a_full_pool(ms(5_100)),
            ms(6_500),
            "state 3 waits until the pool frees"
        );
    }

    #[test]
    fn returns_after_a_bound_on_a_full_pool_after_its_status_channels_are_removed() {
        assert_eq!(
            removed_with_a_full_pool(ms(40_100)),
            ms(21_500),
            "10 s for state 3, then 10 s for the last frame"
        );
    }

    /// Fills the pool at 0.5 s, removes a status channel, and frees the pool `held`
    /// later, while a run ends at 1.5 s. Gives the length of the call.
    fn removed_with_a_full_pool(held: Span) -> Span {
        let run = |ctx: Context<()>| async move {
            ctx.clock().sleep(ms(1_500)).await;
            Ok(())
        };
        timed(run, move |scene| async move {
            let hog = rival(&scene.hub, tally_statuses(&["state", "error"])).await;
            scene.node.clock().sleep(ms(500)).await;
            let pool = fill(&hog);
            scene.remove("samples");
            // Off the status writes at 1.5 s plus each whole second, where the order of
            // the two tasks is random.
            scene.node.clock().sleep(held).await;
            drop(pool);
        })
    }

    /// The pool frees at 3 s and so ends the first outage. A refusal at 8 s then waits
    /// a full `SETTLE`, and the last frame its own.
    #[test]
    fn waits_a_full_bound_for_a_refusal_after_a_frame_applied() {
        let runs = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Mutex::new(None));
        let started = Arc::clone(&start);
        let run = move |ctx: Context<()>| {
            let first = runs.fetch_add(1, Ordering::Relaxed) == 0;
            let start = Arc::clone(&started);
            async move {
                let clock = ctx.clock();
                if first {
                    *start.lock().expect("not poisoned") = Some(clock.now());
                    clock.sleep(ms(1_000)).await;
                    return Err(Error::Retry("busy".into()));
                }
                let origin = start.lock().expect("not poisoned").expect("set");
                clock.sleep_until(origin + ms(8_000)).await;
                Ok(())
            }
        };
        let returned = timed(run, |scene| async move {
            let clock = scene.node.clock();
            let origin = clock.now();
            let hog = rival(&scene.hub, tally_statuses(&["state", "error"])).await;
            clock.sleep_until(origin + ms(500)).await;
            let held = fill(&hog);
            clock.sleep_until(origin + ms(3_000)).await;
            drop(held);
            clock.sleep_until(origin + ms(7_000)).await;
            let held = fill(&hog);
            clock.sleep(ms(60_000)).await;
            drop(held);
        });
        assert_eq!(
            returned,
            ms(28_000),
            "10 s from 8 s, then 10 s for the last frame"
        );
    }

    #[test]
    fn waits_for_a_full_pool_after_its_disk_failed() {
        let run = |ctx: Context<()>| async move {
            ctx.count("samples").set(1);
            ctx.clock().sleep(ms(1_500)).await;
            Ok(())
        };
        let returned = timed(run, |scene| async move {
            let clock = scene.node.clock();
            clock.sleep(ms(100)).await;
            let mut hog = rival(&scene.hub, tally_statuses(&["state", "error"])).await;
            clock.sleep(ms(400)).await;
            scene
                .node
                .fail_file(RING.as_ref(), env::files::Operation::Sync);
            clock.sleep(ms(700)).await;
            let mut held = fill(&hog);
            clock.sleep(ms(200)).await;
            let one = held.pop().expect("one draft");
            let written = hog.write(Label::Path(Path::Live), one);
            assert!(
                matches!(
                    written,
                    Err(hub::writer::Failure::Home(hub::home::Error::Disk(_)))
                ),
                "the shard failed on disk at 1.4 s: {written:?}"
            );
            held.extend(fill(&hog));
            clock.sleep(ms(4_200)).await;
            drop(held);
        });
        assert_eq!(returned, ms(6_500), "state 3 waits until the pool frees");
    }

    #[test]
    fn writes_no_status_for_a_config_that_does_not_parse() {
        let out = supervise(0, "modbus", vec![Step::Done], config(), Cancel::Never);
        assert!(out.statuses.is_empty(), "{:?}", out.statuses);
    }

    #[test]
    fn restarts_after_a_device_error_within_the_backoff() {
        let steps = vec![
            Step::Device(ms(10)),
            Step::Device(ms(10)),
            Step::Device(ms(10)),
            Step::Done,
        ];
        let out = supervise(0, "script", steps, config(), Cancel::Never);
        out.result.expect("ok");
        assert_eq!(out.runs.len(), 4);
        let ceilings = [Span::SECOND, ms(2000), ms(4000)];
        for (pair, ceiling) in out.runs.windows(2).zip(ceilings) {
            let [(_, Some(end)), (next, _)] = pair else {
                panic!("each run ended: {pair:?}");
            };
            let gap = between(*end, *next);
            assert!(gap >= Span::ZERO && gap <= ceiling, "{gap:?} > {ceiling:?}");
        }
    }

    #[test]
    fn restarts_after_a_retry_error() {
        let out = supervise(
            0,
            "script",
            vec![Step::Retry, Step::Done],
            config(),
            Cancel::Never,
        );
        out.result.expect("ok");
        assert_eq!(out.runs.len(), 2);
    }

    #[test]
    fn restarts_after_a_run_cancels_its_own_token() {
        let out = supervise(
            0,
            "script",
            vec![Step::Abort, Step::Done],
            config(),
            Cancel::Never,
        );
        out.result.expect("ok");
        assert_eq!(out.runs.len(), 2, "the run's token is not the caller's");
    }

    #[test]
    fn grows_the_waits_after_short_runs() {
        let mut steps = vec![Step::Device(Span::ZERO); 12];
        steps.push(Step::Done);
        let out = supervise(0, "script", steps, config(), Cancel::Never);
        out.result.expect("ok");
        let last = out.runs.last().expect("13 runs").0;
        // With no growth, twelve waits of at most 1 s each.
        assert!(last > ms(12_000), "{last:?}");
    }

    #[test]
    fn returns_a_config_error_from_run_without_a_restart() {
        let steps = vec![Step::Config, Step::Done];
        let out = supervise(0, "script", steps, config(), Cancel::Never);
        assert_eq!(config_errors(out.result), [bad()]);
        assert_eq!(out.runs.len(), 1);
    }

    #[test]
    fn returns_a_parse_error_without_a_run() {
        let n = Attribute {
            key: "bad".into(),
            key_span: None,
            value: Value {
                kind: value::Kind::Bool(true),
                span: None,
            },
        };
        let config = Document {
            attributes: Map::new(vec![n]).expect("one key"),
            blocks: Vec::new(),
        };
        let out = supervise(0, "script", vec![Step::Done], config, Cancel::Never);
        assert_eq!(config_errors(out.result), [bad()]);
        assert!(out.runs.is_empty());
    }

    #[test]
    fn returns_a_check_error_without_a_run_or_a_status() {
        let config = Document {
            attributes: Map::new(vec![Attribute {
                key: "worse".into(),
                key_span: None,
                value: Value {
                    kind: value::Kind::Bool(true),
                    span: None,
                },
            }])
            .expect("one key"),
            blocks: Vec::new(),
        };
        let out = supervise(0, "script", vec![Step::Done], config, Cancel::Never);
        assert_eq!(config_errors(out.result), [worse()]);
        assert!(out.runs.is_empty());
        assert!(out.statuses.is_empty(), "{:?}", out.statuses);
    }

    #[test]
    fn returns_an_unknown_kind_as_a_config_error() {
        let out = supervise(0, "modbus", vec![Step::Done], config(), Cancel::Never);
        let codes: Vec<_> = config_errors(out.result).iter().map(|d| d.code).collect();
        assert_eq!(codes, [Code::new("connector.unknown-kind")]);
        assert!(out.runs.is_empty());
    }

    #[test]
    fn waits_for_the_run_to_return_after_a_cancel() {
        let steps = vec![Step::Linger(ms(30))];
        let out = supervise(0, "script", steps, config(), Cancel::After(ms(50)));
        out.result.expect("ok after a cancel");
        assert_eq!(out.runs, [(Span::ZERO, Some(ms(80)))]);
        assert_eq!(out.returned, ms(80), "after the run returned, never before");
    }

    #[test]
    fn returns_at_once_when_cancelled_during_the_backoff() {
        let steps = vec![Step::Device(Span::ZERO)];
        let out = supervise(
            0,
            "script",
            steps,
            config(),
            Cancel::After(Span::from_nanos(1)),
        );
        out.result.expect("ok after a cancel");
        assert_eq!(out.runs, [(Span::ZERO, Some(Span::ZERO))]);
        assert_eq!(out.returned, Span::from_nanos(1), "at the cancel");
    }

    #[test]
    fn starts_no_run_when_cancelled_before_the_call() {
        let out = supervise(0, "script", vec![Step::Done], config(), Cancel::Now);
        out.result.expect("ok after a cancel");
        assert!(out.runs.is_empty(), "a run started after the cancel");
    }

    #[test]
    fn starts_the_backoff_again_after_a_long_run() {
        let mut steps = vec![Step::Device(Span::ZERO); 8];
        steps.push(Step::Device(Span::MINUTE));
        steps.extend(std::iter::repeat_n(Step::Device(Span::ZERO), 3));
        steps.push(Step::Done);
        let out = supervise(0, "script", steps, config(), Cancel::Never);
        out.result.expect("ok");
        let after = out.runs.get(9..).expect("13 runs");
        let first = out.runs.get(8).and_then(|run| run.1).expect("run 8 ended");
        let last = after.last().expect("a last run").0;
        // Four waits from the first again: at most 1 + 2 + 4 + 8 s. Without the
        // reset, each would be up to a minute.
        let gap = between(first, last);
        assert!(gap <= ms(15_000), "{gap:?}");
    }

    #[test]
    fn counts_no_wait_for_the_tasks_toward_a_long_run() {
        let mut steps = vec![Step::Device(Span::ZERO); 8];
        steps.push(Step::Hold(ms(61_000)));
        steps.extend(std::iter::repeat_n(Step::Device(Span::ZERO), 3));
        steps.push(Step::Done);
        let out = supervise(0, "script", steps, config(), Cancel::Never);
        out.result.expect("ok");
        let held = out.runs.get(8).and_then(|run| run.1).expect("run 8 ended");
        let last = out.runs.last().expect("13 runs").0;
        // Each of the four waits after run 8 has a cap of a minute. A reset would
        // make them at most 1 + 2 + 4 + 8 s after the 61 s wait for the task.
        let gap = between(held, last);
        assert!(gap > ms(61_000 + 15_000), "{gap:?}");
    }

    const OPTIONS: tcp::Options = tcp::Options {
        send_buffer_bytes: 1 << 12,
        recv_buffer_bytes: 1 << 12,
        unsent_bytes_max: NonZeroUsize::new(1 << 10).unwrap(),
        delayed: false,
    };

    /// A kind that connects to `remote` through its context and reads the stream in
    /// a task that it spawns through its context. At the cancel, the task closes the
    /// stream and the run returns.
    struct Dial {
        remote: SocketAddr,
        read: Arc<Mutex<Vec<u8>>>,
    }

    impl Kind for Dial {
        type Config = ();

        fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
            Ok(())
        }

        fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
            Ok(Channels::default())
        }

        async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
            let config = tcp::Config {
                remote: self.remote,
                options: OPTIONS,
            };
            let mut tcp = ctx.net().connect(&config).await.expect("it listens");
            let read = Arc::clone(&self.read);
            let token = ctx.cancel().clone();
            ctx.tasks().spawn(async move {
                let mut buffer = [0; 16];
                while let Some(n) = token
                    .race(poll_fn(|cx| tcp.poll_read(cx, &mut buffer)))
                    .await
                {
                    let n = n.expect("the read works");
                    let Some(bytes) = buffer.get(..n).filter(|_| n > 0) else {
                        break;
                    };
                    read.lock()
                        .expect("no panic under the lock")
                        .extend_from_slice(bytes);
                }
                poll_fn(|cx| tcp.poll_close(cx)).await.expect("it closes");
            });
            ctx.cancel().wait().await;
            Ok(())
        }
    }

    #[test]
    fn gives_a_kind_the_network_and_the_tasks_of_its_shard() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let (client, server) = (
            sim.node(sim::node::Config::default()),
            sim.node(sim::node::Config::default()),
        );
        let remote = SocketAddr::new(server.addresses()[0], 4840);
        let listen = tcp::Listen {
            local: remote,
            backlog: 1,
            options: OPTIONS,
        };
        let mut listener = server.net().listen(&listen).expect("the port is free");
        let shard = env::shards::Config {
            name: "server".into(),
            core: Some(0),
        };
        let server = server.shards().start(shard, move |_| async move {
            let mut tcp = poll_fn(|cx| listener.poll_accept(cx))
                .await
                .expect("a stream comes");
            let hello = [IoSlice::new(b"hello")];
            let n = poll_fn(|cx| tcp.poll_write(cx, &hello))
                .await
                .expect("the write works");
            assert_eq!(n, 5);
            let mut buffer = [0; 1];
            let n = poll_fn(|cx| tcp.poll_read(cx, &mut buffer))
                .await
                .expect("the client closes");
            assert_eq!(n, 0, "the client sends nothing");
        });
        let _server = server.expect("the shard starts");
        let read = Arc::new(Mutex::new(Vec::new()));
        let dial = Dial {
            remote,
            read: Arc::clone(&read),
        };
        let result = sim
            .run_on(&client, move |node, tasks| async move {
                let kinds = Table::new().with("dial", dial);
                let inputs = create_config(&node, tasks.clone(), kinds, "plant.dial");
                let supervisor = Supervisor::new(inputs.await);
                let token = Token::new();
                let canceller = token.clone();
                let clock = node.clock();
                tasks.spawn(async move {
                    clock.sleep(Span::SECOND).await;
                    canceller.cancel();
                });
                let name = "plant.dial".parse().expect("a valid name");
                supervisor
                    .open("dial", name, &config(), &token)
                    .await
                    .run()
                    .await
            })
            .expect("the run ends");
        result.expect("ok after the cancel");
        assert_eq!(*read.lock().expect("no panic under the lock"), b"hello");
    }

    /// A kind whose run spawns one task through its context. The task holds the
    /// device until `linger` after the run's cancel. The first run fails with a
    /// device error; each run records how many tasks of earlier runs still hold the
    /// device at its start.
    #[derive(Default)]
    struct Spawner {
        linger: Span,
        live: Arc<Mutex<u32>>,
        seen: Arc<Mutex<Vec<u32>>>,
    }

    impl Kind for Spawner {
        type Config = ();

        fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
            Ok(())
        }

        fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
            Ok(Channels::default())
        }

        async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
            let n = *self.live.lock().expect("no panic");
            let first = {
                let mut seen = self.seen.lock().expect("no panic");
                seen.push(n);
                seen.len() == 1
            };
            *self.live.lock().expect("no panic") += 1;
            let live = Arc::clone(&self.live);
            let token = ctx.cancel().clone();
            let (clock, linger) = (ctx.clock().clone(), self.linger);
            ctx.tasks().spawn(async move {
                token.wait().await;
                clock.sleep(linger).await;
                *live.lock().expect("no panic") -= 1;
            });
            if first {
                return Err(Error::Device("no reply".into()));
            }
            ctx.cancel().wait().await;
            Ok(())
        }
    }

    /// What [`supervise_spawner`] saw: what each run of [`Spawner`] saw at its
    /// start, when `run` returned, and how many tasks still ran then.
    struct Spawned {
        seen: Vec<u32>,
        returned: Span,
        live: u32,
    }

    /// Supervises one connector of [`Spawner`] with `linger`, and cancels it at 5 s.
    fn supervise_spawner(linger: Span) -> Spawned {
        run_on(move |node, tasks| async move {
            let kind = Spawner {
                linger,
                ..Spawner::default()
            };
            let (seen, live) = (Arc::clone(&kind.seen), Arc::clone(&kind.live));
            let kinds = Table::new().with("spawner", kind);
            let supervisor = Supervisor::new(
                create_config(&node, tasks.clone(), kinds, "plant.spawner").await,
            );
            let token = Token::new();
            let canceller = token.clone();
            let clock = node.clock();
            let sleeper = clock.clone();
            tasks.spawn(async move {
                sleeper.sleep(ms(5_000)).await;
                canceller.cancel();
            });
            let start = clock.now();
            let name = "plant.spawner".parse().expect("a valid name");
            supervisor
                .open("spawner", name, &config(), &token)
                .await
                .run()
                .await
                .expect("ok after the cancel");
            let seen = seen.lock().expect("no panic").clone();
            let live = *live.lock().expect("no panic");
            Spawned {
                seen,
                returned: clock.now() - start,
                live,
            }
        })
    }

    #[test]
    fn stops_the_tasks_of_a_run_before_the_next_run() {
        let out = supervise_spawner(Span::ZERO);
        assert_eq!(out.seen, [0, 0], "the first run's task still runs");
    }

    #[test]
    fn waits_for_the_tasks_of_a_run_before_the_next_run() {
        let out = supervise_spawner(ms(2_000));
        assert_eq!(out.seen, [0, 0], "the first run's task still runs");
    }

    #[test]
    fn waits_for_the_tasks_of_a_run_before_it_returns() {
        let out = supervise_spawner(ms(2_000));
        assert_eq!((out.returned, out.live), (ms(7_000), 0));
    }

    #[test]
    fn waits_for_the_tasks_of_a_run_before_it_returns_a_config_error() {
        let out = supervise(
            0,
            "script",
            vec![Step::Refuse(ms(2_000))],
            config(),
            Cancel::Never,
        );
        assert_eq!(config_errors(out.result), [bad()]);
        assert_eq!(out.returned, ms(2_000));
    }

    /// Polls `run` until 5 s pass, checks that it still runs, and drops it. Gives
    /// what `before` reads just before the drop.
    async fn drop_at_5_s<T>(
        run: impl Future,
        clock: &Clock,
        before: impl FnOnce() -> T,
    ) -> T {
        let mut run = pin!(run);
        let mut later = pin!(clock.sleep(ms(5_000)));
        poll_fn(|cx| {
            assert!(run.as_mut().poll(cx).is_pending(), "it runs until dropped");
            later.as_mut().poll(cx)
        })
        .await;
        before()
    }

    #[test]
    fn stops_the_tasks_of_a_run_when_its_future_drops() {
        let live = run_on(|node, tasks| async move {
            let kind = Spawner::default();
            let live = Arc::clone(&kind.live);
            let kinds = Table::new().with("spawner", kind);
            let supervisor = Supervisor::new(
                create_config(&node, tasks, kinds, "plant.spawner").await,
            );
            let token = Token::new();
            let name = "plant.spawner".parse().expect("a valid name");
            let config = config();
            let clock = node.clock();
            let opened = supervisor.open("spawner", name, &config, &token).await;
            let run = Box::pin(opened.run());
            let before =
                drop_at_5_s(run, &clock, || *live.lock().expect("no panic")).await;
            clock.sleep(ms(1)).await;
            (before, *live.lock().expect("no panic"))
        });
        assert_eq!(live, (1, 0), "the second run's task outlives the drop");
    }

    #[test]
    fn starts_a_new_call_with_no_wait_for_the_tasks_of_a_dropped_call() {
        let seen = run_on(|node, tasks| async move {
            let kind = Spawner {
                linger: ms(2_000),
                ..Spawner::default()
            };
            let seen = Arc::clone(&kind.seen);
            let kinds = Table::new().with("spawner", kind);
            let supervisor = Supervisor::new(
                create_config(&node, tasks.clone(), kinds, "plant.spawner").await,
            );
            let (token, config) = (Token::new(), config());
            let name: Name = "plant.spawner".parse().expect("a valid name");
            let clock = node.clock();
            let opened = supervisor
                .open("spawner", name.clone(), &config, &token)
                .await;
            let run = Box::pin(opened.run());
            drop_at_5_s(run, &clock, || ()).await;
            let again = Token::new();
            let (canceller, sleeper) = (again.clone(), clock.clone());
            tasks.spawn(async move {
                sleeper.sleep(ms(1)).await;
                canceller.cancel();
            });
            supervisor
                .open("spawner", name, &config, &again)
                .await
                .run()
                .await
                .expect("ok after the cancel");
            seen.lock().expect("no panic").clone()
        });
        assert_eq!(
            seen,
            [0, 0, 1],
            "the new call waited for the task of the dropped call"
        );
    }

    /// A kind with the count `samples`. Each run sets the count `count` to each of 1
    /// to `n`, `gap` apart, then returns `Ok`.
    struct Tally {
        count: &'static str,
        n: u64,
        gap: Span,
    }

    impl Tally {
        /// Sets `count` once, at once.
        fn new(count: &'static str) -> Self {
            Self {
                count,
                n: 1,
                gap: Span::ZERO,
            }
        }
    }

    impl Kind for Tally {
        type Config = ();

        fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
            Ok(())
        }

        fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
            Ok(Channels {
                counts: vec![name("samples")],
                ..Channels::default()
            })
        }

        async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
            let count = ctx.count(self.count);
            for i in 1..=self.n {
                ctx.clock().sleep(self.gap).await;
                count.set(i);
            }
            Ok(())
        }
    }

    /// A kind with the counts `counts` that reads with `reader`, whose run is the
    /// function it holds.
    struct Closure<F> {
        counts: Vec<Name>,
        reader: Option<reader::Settings>,
        run: F,
    }

    /// A kind with the count `samples`, whose run is `run`.
    fn counted<F>(run: F) -> Closure<F> {
        Closure {
            counts: vec![name("samples")],
            reader: None,
            run,
        }
    }

    impl<F, R> Kind for Closure<F>
    where
        F: Fn(Context<()>) -> R + Send + Sync + 'static,
        R: Future<Output = Result<(), Error>>,
    {
        type Config = ();

        fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
            Ok(())
        }

        fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
            Ok(Channels {
                counts: self.counts.clone(),
                reader: self.reader.clone(),
                ..Channels::default()
            })
        }

        fn run(&self, ctx: Context<()>) -> impl Future<Output = Result<(), Error>> {
            (self.run)(ctx)
        }
    }

    /// A kind with the count `samples`. Its run sets it to 1 at 500 ms and returns
    /// `Ok` at 600 ms. A task of the run sets it to `value`, if given, at 800 ms and
    /// ends at 2 s.
    fn late(value: Option<u64>) -> impl Kind<Config = ()> {
        counted(move |ctx: Context<()>| async move {
            let (late, clock) = (ctx.count("samples"), ctx.clock().clone());
            ctx.tasks().spawn(async move {
                clock.sleep(ms(800)).await;
                if let Some(value) = value {
                    late.set(value);
                }
                clock.sleep(ms(1_200)).await;
            });
            ctx.clock().sleep(ms(500)).await;
            ctx.count("samples").set(1);
            ctx.clock().sleep(ms(100)).await;
            Ok(())
        })
    }

    #[test]
    fn writes_a_change_of_counts_alone_a_second_after_a_write_of_the_supervisor() {
        let statuses = tally(late(Some(2)));
        let got: Vec<_> = statuses
            .iter()
            .map(|(at, samples, ..)| (*at, samples[0], samples[4]))
            .collect();
        let want = [
            (Span::ZERO, 0, 0),
            (ms(600), 3, 1),
            (ms(1_600), 3, 2),
            (ms(2_000), 2, 2),
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn writes_no_count_that_a_write_of_the_supervisor_wrote() {
        let statuses = tally(late(None));
        let got: Vec<_> = statuses
            .iter()
            .map(|(at, samples, ..)| (*at, samples[0], samples[4]))
            .collect();
        let want = [(Span::ZERO, 0, 0), (ms(600), 3, 1), (ms(2_000), 2, 1)];
        assert_eq!(got, want);
    }

    #[test]
    fn keeps_a_count_across_the_runs_of_one_call() {
        let runs = Arc::new(AtomicUsize::new(0));
        let statuses = tally(counted(move |ctx: Context<()>| {
            let first = runs.fetch_add(1, Ordering::Relaxed) == 0;
            async move {
                if first {
                    ctx.count("samples").add(2);
                    return Err(Error::Retry("busy".into()));
                }
                ctx.count("samples").add(3);
                Ok(())
            }
        }));
        let last = statuses
            .last()
            .map(|(_, samples, ..)| (samples[2], samples[4]));
        assert_eq!(last, Some((1, 5)), "one restart, then 2 + 3: {statuses:?}");
    }

    #[test]
    fn adds_to_the_value_that_a_set_gave() {
        let statuses = tally(counted(|ctx: Context<()>| async move {
            ctx.count("samples").set(3);
            ctx.count("samples").add(4);
            Ok(())
        }));
        let last = statuses.last().map(|(_, samples, ..)| samples[4]);
        assert_eq!(last, Some(7), "{statuses:?}");
    }

    #[test]
    fn gives_an_add_in_the_next_flush() {
        let statuses = tally(counted(|ctx: Context<()>| async move {
            let clock = ctx.clock().clone();
            ctx.count("samples").set(3);
            clock.sleep(ms(1_500)).await;
            ctx.count("samples").add(4);
            clock.sleep(ms(3_000)).await;
            Ok(())
        }));
        let first = statuses.iter().find(|(_, samples, ..)| samples[4] == 7);
        assert_eq!(first.map(|&(at, ..)| at), Some(ms(2_000)), "{statuses:?}");
    }

    #[test]
    fn adds_to_each_count_alone() {
        let kind = Closure {
            counts: vec![name("samples"), name("errors")],
            reader: None,
            run: |ctx: Context<()>| async move {
                ctx.count("samples").set(3);
                ctx.count("errors").add(1);
                ctx.count("samples").add(4);
                Ok(())
            },
        };
        let (statuses, _) = tally_until(kind, &["samples", "errors"], None);
        let last = statuses
            .last()
            .map(|(_, counts, ..)| (counts[4], counts[5]));
        assert_eq!(last, Some((7, 1)), "samples, then errors: {statuses:?}");
    }

    #[test]
    fn saturates_an_add_at_the_largest_count() {
        let statuses = tally(counted(|ctx: Context<()>| async move {
            ctx.count("samples").set(u64::MAX - 1);
            ctx.count("samples").add(2);
            Ok(())
        }));
        let last = statuses.last().map(|(_, samples, ..)| samples[4]);
        assert_eq!(last, Some(-1), "u64::MAX, read as i64: {statuses:?}");
    }

    /// A kind with the count `samples`. Its run spawns a task that sets the count to
    /// each of 1 to `n`, `gap` apart, and returns `Ok` once that task ended.
    fn relay(n: u64, gap: Span) -> impl Kind<Config = ()> {
        counted(move |ctx: Context<()>| async move {
            let (count, clock) = (ctx.count("samples"), ctx.clock().clone());
            let done = Rc::new((Cell::new(false), Cell::new(None::<Waker>)));
            let signal = Rc::clone(&done);
            ctx.tasks().spawn(async move {
                for i in 1..=n {
                    clock.sleep(gap).await;
                    count.set(i);
                }
                signal.0.set(true);
                if let Some(waker) = signal.1.take() {
                    waker.wake();
                }
            });
            poll_fn(|cx| {
                if done.0.get() {
                    return Poll::Ready(());
                }
                done.1.set(Some(cx.waker().clone()));
                Poll::Pending
            })
            .await;
            Ok(())
        })
    }

    #[test]
    fn gives_13_frames_for_10_000_samples_set_from_a_task_past_10_s() {
        // Off the whole seconds, so no set is at the time of a flush. The last set is
        // just after 10 s, so the flush at 10 s writes a frame of counts.
        let statuses = tally(relay(10_000, Span::from_nanos(1_000_001)));
        assert_eq!(statuses.len(), 13, "{statuses:?}");
        assert_eq!(
            statuses.last().map(|(_, samples, ..)| samples[4]),
            Some(10_000)
        );
        assert_eq!(states(&statuses)[11..], [(3, 0, 0), (2, 0, 0)]);
    }

    /// A kind with the count `samples`. Its run takes frames of the shape of a
    /// status frame from the shard's pool until it has no room, sets the count to 7,
    /// gives the frames back at 1.5 s, and returns `Ok` at 3 s. When `full`, it takes
    /// the room that came back again at 900 ms, so the pool is full at the flush at
    /// 1 s. Else the home has no room to take that flush's frame, and loses it.
    fn hog(full: bool) -> impl Kind<Config = ()> {
        counted(move |ctx: Context<()>| async move {
            let channels = status_names("plant.tally", &["samples"]);
            let writer = ctx.writer(channels, Authority(1), None).await;
            let writer = writer.expect("the writer opens");
            let mut held = fill(&writer);
            ctx.count("samples").set(7);
            ctx.clock().sleep(ms(900)).await;
            if full {
                held.extend(fill(&writer));
            }
            ctx.clock().sleep(ms(600)).await;
            drop(held);
            ctx.clock().sleep(ms(1_500)).await;
            Ok(())
        })
    }

    #[test]
    fn writes_a_count_again_when_the_pool_has_no_room_for_its_frame() {
        assert_eq!(hogged(true), HOGGED);
    }

    #[test]
    fn writes_a_count_again_when_the_home_loses_its_frame() {
        assert_eq!(hogged(false), HOGGED);
    }

    /// The status of `hog`: the count of the flush at 1 s is written at 2 s.
    const HOGGED: [(Span, i64, i64); 4] = [
        (Span::ZERO, 0, 0),
        (Span::from_nanos(2_000_000_000), 0, 7),
        (Span::from_nanos(3_000_000_000), 3, 7),
        (Span::from_nanos(3_000_000_001), 2, 7),
    ];

    /// A kind with the count `samples`. Its run takes frames from the shard's pool
    /// until it has no room, sets the count to 7, fills the pool again at 900 ms, and
    /// at 1 s, after the flush gets no block for its frame, gives the frames back and
    /// returns `Ok`.
    fn hog_to_flush() -> impl Kind<Config = ()> {
        counted(|ctx: Context<()>| async move {
            let channels = status_names("plant.tally", &["samples"]);
            let writer = ctx.writer(channels, Authority(1), None).await;
            let writer = writer.expect("the writer opens");
            let mut held = fill(&writer);
            ctx.count("samples").set(7);
            ctx.clock().sleep(ms(900)).await;
            held.extend(fill(&writer));
            ctx.clock().sleep(ms(100)).await;
            // The flush at 1 s runs after this task wakes at 1 s, so yield once.
            let mut flushed = false;
            poll_fn(|cx| {
                if flushed {
                    return Poll::Ready(());
                }
                flushed = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            })
            .await;
            drop(held);
            Ok(())
        })
    }

    #[test]
    fn stamps_a_frame_after_the_stamp_of_a_frame_with_no_block() {
        let want = [
            (Span::ZERO, 0, 0),
            (Span::from_nanos(1_000_000_001), 3, 7),
            (Span::from_nanos(1_000_000_002), 2, 7),
        ];
        assert_eq!(hogged_by(hog_to_flush()), want);
    }

    /// Takes frames of one sample for each channel of `writer` from the shard's pool
    /// until the pool has no room for a frame of any size up to 2 KiB, so that no
    /// status frame and no copy of it in the home gets a block. `writer` has `error`
    /// and `note`, whose texts set the size, so this holds only while the two texts
    /// of a test are short.
    fn fill(writer: &hub::writer::Writer) -> Vec<frame::Draft> {
        let mut held = Vec::new();
        for text in (0..=2_048).rev().step_by(16) {
            let series: Vec<_> = (writer.set().entries().iter().enumerate())
                .map(|(i, entry)| (i, entry.data_type.width().unwrap_or(4 + text)))
                .collect();
            let error = loop {
                match writer.draft(Form::Raw, &series) {
                    Ok(draft) => held.push(draft),
                    Err(error) => break error,
                }
            };
            assert!(
                matches!(error, frame::Error::Pool(block::Error::Exhausted { .. })),
                "{error}"
            );
        }
        held
    }

    /// The ring file of the test shard, which `fail_file` fails.
    const RING: &str = "shard-0/ring";

    /// What the scenario of [`timed`] gets.
    struct Scene {
        hub: hub::Hub,
        node: sim::node::Node,
        /// The status definitions of `plant.tally`.
        status: Vec<(Name, Definition)>,
    }

    impl Scene {
        /// Removes the status channel `plant.tally.status.<suffix>`.
        fn remove(&self, suffix: &str) {
            let removed = name(&format!("plant.tally.status.{suffix}"));
            let kept = self.status.iter().filter(|(name, _)| *name != removed);
            self.hub
                .set_definitions(kept.map(|(name, def)| (name, def)));
        }
    }

    /// Runs `run` as the kind of `plant.tally`, with the count `samples`, while
    /// `scenario` runs in a task of the node. Returns the length of the call, which
    /// must return `Ok`.
    fn timed<F, R, S, T>(run: F, scenario: S) -> Span
    where
        F: Fn(Context<()>) -> R + Send + Sync + 'static,
        R: Future<Output = Result<(), Error>>,
        S: FnOnce(Scene) -> T + Send + 'static,
        T: Future<Output = ()> + 'static,
    {
        timed_on(0, sim::node::Config::default(), run, scenario)
    }

    /// [`timed`] on a node whose timers wake only when due, so a test can count the
    /// polls of a sleep, in a sim of `seed`.
    fn timed_due<F, R, S, T>(seed: u64, run: F, scenario: S) -> Span
    where
        F: Fn(Context<()>) -> R + Send + Sync + 'static,
        R: Future<Output = Result<(), Error>>,
        S: FnOnce(Scene) -> T + Send + 'static,
        T: Future<Output = ()> + 'static,
    {
        let node = sim::node::Config {
            arm_max: None,
            ..sim::node::Config::default()
        };
        timed_on(seed, node, run, scenario)
    }

    /// [`timed`] on a node of `node`, in a sim of `seed`.
    fn timed_on<F, R, S, T>(
        seed: u64,
        node: sim::node::Config,
        run: F,
        scenario: S,
    ) -> Span
    where
        F: Fn(Context<()>) -> R + Send + Sync + 'static,
        R: Future<Output = Result<(), Error>>,
        S: FnOnce(Scene) -> T + Send + 'static,
        T: Future<Output = ()> + 'static,
    {
        run_with(seed, node, |node, tasks| async move {
            let kinds = Table::new().with("tally", counted(run));
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.tally").await;
            let (connector, counts) = (name("plant.tally"), [name("samples")]);
            let status = testing::create_status(&connector, &counts, STATUS);
            inputs
                .hub
                .set_definitions(status.iter().map(|(name, def)| (name, def)));
            let hub = inputs.hub.clone();
            tasks.spawn(scenario(Scene {
                hub,
                node: node.clone(),
                status,
            }));
            let (clock, start) = (node.clock(), node.clock().now());
            let result = Supervisor::new(inputs)
                .open("tally", connector, &config(), &Token::new())
                .await
                .run()
                .await;
            result.expect("the run returns ok");
            clock.now() - start
        })
    }

    /// A writer of the subject `plant.other` with authority 1 on `channels`.
    async fn rival(hub: &hub::Hub, channels: Vec<Name>) -> hub::writer::Writer {
        open_writer(hub, "plant.other", Authority(1), channels).await
    }

    /// The time, `state`, and count of each status frame of `hog(full)`.
    fn hogged(full: bool) -> Vec<(Span, i64, i64)> {
        hogged_by(hog(full))
    }

    /// The time, `state`, and count of each status frame of `kind`.
    fn hogged_by(kind: impl Kind + 'static) -> Vec<(Span, i64, i64)> {
        let statuses = tally(kind);
        statuses
            .iter()
            .map(|(at, samples, ..)| (*at, samples[0], samples[4]))
            .collect()
    }

    /// A kind with the count `samples`. Its run spawns a task that takes frames from
    /// the shard's pool until it has no room and gives them back at 500 ms, and
    /// returns `Ok` at 200 ms.
    fn outlive() -> impl Kind<Config = ()> {
        counted(|ctx: Context<()>| async move {
            let channels = status_names("plant.tally", &["samples"]);
            let writer = ctx.writer(channels, Authority(1), None).await;
            let writer = writer.expect("the writer opens");
            let clock = ctx.clock().clone();
            ctx.tasks().spawn(async move {
                let held = fill(&writer);
                clock.sleep(ms(500)).await;
                drop(held);
            });
            ctx.clock().sleep(ms(200)).await;
            Ok(())
        })
    }

    #[test]
    fn returns_at_a_cancel_while_a_change_of_state_waits() {
        let (statuses, returned) = tally_until(outlive(), &["samples"], Some(ms(700)));
        let states: Vec<_> = statuses.iter().map(|(at, s, ..)| (*at, s[0])).collect();
        let want = [(Span::ZERO, 0), (ms(700), 2)];
        assert_eq!(
            states, want,
            "`state` 2 at the cancel, in place of `state` 3"
        );
        assert_eq!(returned, ms(700));
    }

    #[test]
    fn writes_state_3_when_the_pool_is_full_at_the_end_of_a_run() {
        let statuses = tally(outlive());
        let states: Vec<_> = statuses.iter().map(|(at, s, ..)| (*at, s[0])).collect();
        let want = [
            (Span::ZERO, 0),
            (ms(1_200), 3),
            (Span::from_nanos(1_200_000_001), 2),
        ];
        assert_eq!(
            states, want,
            "`state` 3 written again 1 s after the run returned"
        );
    }

    #[test]
    fn starts_no_run_after_a_cancel_while_the_start_waits() {
        let runs = run_on(|node, tasks| async move {
            let script = Script {
                steps: Mutex::new([Step::Hold(ms(1_000))].into()),
                ..Script::default()
            };
            let runs = Arc::clone(&script.runs);
            let kinds = Table::new().with("script", script);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.script").await;
            let hub = inputs.hub.clone();
            let (token, clock) = (Token::new(), node.clock());
            let (canceller, sleeper) = (token.clone(), clock.clone());
            tasks.spawn(async move {
                sleeper.sleep(ms(500)).await;
                let channels = status_names("plant.script", &[]);
                let hog =
                    open_writer(&hub, "plant.other", Authority(1), channels).await;
                let held = fill(&hog);
                sleeper.sleep(ms(2_000)).await;
                canceller.cancel();
                sleeper.sleep(ms(5_000)).await;
                drop(held);
            });
            let (supervisor, start) = (Supervisor::new(inputs), clock.now());
            let name = name("plant.script");
            let result = supervisor
                .open("script", name, &config(), &token)
                .await
                .run()
                .await;
            result.expect("ok after a cancel");
            let runs: Vec<_> = runs
                .lock()
                .expect("no panic under the lock")
                .iter()
                .map(|(from, _)| *from - start)
                .collect();
            runs
        });
        assert_eq!(
            runs,
            [Span::ZERO],
            "no run starts after the cancel at 2.5 s"
        );
    }

    /// The status frames of one connector of `kind`, `plant.tally`, with the count
    /// `samples`.
    fn tally(kind: impl Kind + 'static) -> Vec<Written> {
        tally_until(kind, &["samples"], None).0
    }

    /// The status frames of one connector of `kind`, as [`tally`] gives, with the
    /// counts `counts`, whose call is cancelled at `cancel`, and when the call
    /// returned.
    fn tally_until(
        kind: impl Kind + 'static,
        counts: &'static [&'static str],
        cancel: Option<Span>,
    ) -> (Vec<Written>, Span) {
        run_on(move |node, tasks| async move {
            let kinds = Table::new().with("tally", kind);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.tally").await;
            let connector = name("plant.tally");
            let counts: Vec<_> = counts.iter().map(|count| name(count)).collect();
            let status = testing::create_status(&connector, &counts, STATUS);
            inputs
                .hub
                .set_definitions(status.iter().map(|(name, def)| (name, def)));
            let statuses =
                read_status(&inputs.hub, "plant.tally", &counts, &tasks).await;
            let (token, clock) = (Token::new(), node.clock());
            if let Some(cancel) = cancel {
                let (canceller, sleeper) = (token.clone(), clock.clone());
                tasks.spawn(async move {
                    sleeper.sleep(cancel).await;
                    canceller.cancel();
                });
            }
            let start = clock.now();
            let result = Supervisor::new(inputs)
                .open("tally", connector, &config(), &token)
                .await
                .run()
                .await;
            result.expect("the run returns ok");
            let returned = clock.now() - start;
            clock.sleep(Span::SECOND).await;
            let statuses = statuses.borrow().clone();
            (statuses, returned)
        })
    }

    #[test]
    fn writes_a_change_of_counts_alone_at_most_once_each_second() {
        // Off the whole seconds, so no set is at the time of a flush.
        let gap = Span::from_nanos(999_999);
        let statuses = tally(Tally {
            count: "samples",
            n: 10_000,
            gap,
        });
        let end = Span::from_nanos(gap.nanos() * 10_000);
        let mut want = vec![(Span::ZERO, 0)];
        want.extend((1..=9).map(|s| (ms(s * 1_000), s * 1_000)));
        want.extend([(end, 10_000), (Span::from_nanos(end.nanos() + 1), 10_000)]);
        let got = statuses.iter().map(|(at, samples, ..)| (*at, samples[4]));
        assert_eq!(got.collect::<Vec<_>>(), want);
        let states = states(&statuses);
        assert_eq!(states[..10], [(0, 0, 0); 10]);
        assert_eq!(states[10..], [(3, 0, 0), (2, 0, 0)]);
    }

    #[test]
    fn writes_the_start_of_a_call_at_the_instant_of_the_end_before_it_at_once() {
        let statuses = run_on(|node, tasks| async move {
            let script = Script {
                steps: Mutex::new([Step::Done, Step::Linger(Span::ZERO)].into()),
                ..Script::default()
            };
            let kinds = Table::new().with("script", script);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.script").await;
            let statuses = read_status(&inputs.hub, "plant.script", &[], &tasks).await;
            let supervisor = Supervisor::new(inputs);
            let (name, clock) = (name("plant.script"), node.clock());
            let token = Token::new();
            let first = supervisor
                .open("script", name.clone(), &config(), &token)
                .await
                .run()
                .await;
            first.expect("the first call returns ok");
            let (canceller, sleeper) = (token.clone(), clock.clone());
            tasks.spawn(async move {
                sleeper.sleep(ms(5_000)).await;
                canceller.cancel();
            });
            let second = supervisor
                .open("script", name, &config(), &token)
                .await
                .run()
                .await;
            second.expect("the second call returns ok");
            clock.sleep(Span::SECOND).await;
            statuses.borrow().clone()
        });
        let got: Vec<_> = statuses.iter().map(|(at, s, ..)| (*at, s[0])).collect();
        let ns = Span::from_nanos;
        let first = [(ns(0), 0), (ns(1), 3), (ns(2), 2)];
        let second = [(ns(3), 0), (ms(5_000), 3), (ns(5_000_000_001), 2)];
        assert_eq!(got, [first, second].concat());
    }

    #[test]
    fn writes_the_count_of_a_set_at_the_time_of_a_flush() {
        let statuses = tally(Tally {
            count: "samples",
            n: 10_000,
            gap: ms(1),
        });
        let mut want = vec![(Span::ZERO, 0)];
        want.extend((1..=9).map(|s| (ms(s * 1_000), s * 1_000)));
        want.extend([
            (ms(10_000), 10_000),
            (Span::from_nanos(10_000_000_001), 10_000),
        ]);
        let got = statuses.iter().map(|(at, samples, ..)| (*at, samples[4]));
        assert_eq!(got.collect::<Vec<_>>(), want);
        assert_eq!(states(&statuses)[10..], [(3, 0, 0), (2, 0, 0)]);
    }

    #[test]
    fn writes_no_status_once_a_status_channel_is_removed() {
        let statuses = run_on(|node, tasks| async move {
            let gap = ms(300);
            let kind = Tally {
                count: "samples",
                n: 10,
                gap,
            };
            let kinds = Table::new().with("tally", kind);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.tally").await;
            let (connector, counts) = (name("plant.tally"), [name("samples")]);
            let status = testing::create_status(&connector, &counts, STATUS);
            inputs
                .hub
                .set_definitions(status.iter().map(|(name, def)| (name, def)));
            let statuses = read_status(&inputs.hub, "plant.tally", &[], &tasks).await;
            let (hub, clock) = (inputs.hub.clone(), node.clock());
            tasks.spawn(async move {
                clock.sleep(ms(1_500)).await;
                let samples = name("plant.tally.status.samples");
                let kept = status.iter().filter(|(name, _)| *name != samples);
                hub.set_definitions(kept.map(|(name, def)| (name, def)));
            });
            let result = Supervisor::new(inputs)
                .open("tally", connector, &config(), &Token::new())
                .await
                .run()
                .await;
            result.expect("the run returns ok");
            node.clock().sleep(Span::SECOND).await;
            statuses.borrow().clone()
        });
        let at: Vec<_> = statuses.iter().map(|(at, ..)| *at).collect();
        assert_eq!(at, [Span::ZERO, Span::SECOND], "nothing after 1.5 s");
        assert_eq!(states(&statuses), [(0, 0, 0); 2]);
    }

    #[test]
    fn writes_no_status_after_a_failed_commit() {
        let (statuses, returned) = run_on(|node, tasks| async move {
            let kind = Tally {
                count: "samples",
                n: 10,
                gap: ms(300),
            };
            let kinds = Table::new().with("tally", kind);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.tally").await;
            let (connector, counts) = (name("plant.tally"), [name("samples")]);
            let status = testing::create_status(&connector, &counts, STATUS);
            inputs
                .hub
                .set_definitions(status.iter().map(|(name, def)| (name, def)));
            let statuses = read_status(&inputs.hub, "plant.tally", &[], &tasks).await;
            let (failer, clock) = (node.clone(), node.clock());
            tasks.spawn(async move {
                clock.sleep(ms(1_500)).await;
                failer.fail_file(RING.as_ref(), env::files::Operation::Sync);
            });
            let (clock, start) = (node.clock(), node.clock().now());
            let result = Supervisor::new(inputs)
                .open("tally", connector, &config(), &Token::new())
                .await
                .run()
                .await;
            result.expect("the run returns ok");
            let returned = clock.now() - start;
            clock.sleep(Span::SECOND).await;
            let statuses = statuses.borrow().clone();
            (statuses, returned)
        });
        let at: Vec<_> = statuses.iter().map(|(at, ..)| *at).collect();
        assert_eq!(at, [Span::ZERO, Span::SECOND], "nothing after 1.5 s");
        assert_eq!(returned, ms(3_000));
    }

    #[test]
    #[should_panic(expected = "the kind did not name the count `other` in its check")]
    fn panics_on_a_count_that_the_kind_did_not_name() {
        tally(Tally::new("other"));
    }

    #[test]
    fn returns_at_a_cancel_while_the_status_channels_wait_to_open() {
        let (returned, runs, _) =
            unsynced(0, Span::from_nanos(30_000_000_000), Span::SECOND, false);
        assert_eq!(returned, Span::SECOND, "the call returns at the cancel");
        assert_eq!(runs, []);
    }

    /// The caller removes the status channels and cancels as the mesh time arrives.
    /// The order in which the tasks wake, which the seed picks, decides whether a run
    /// starts at that instant. Each order returns at the cancel.
    #[test]
    fn returns_at_a_cancel_with_a_removal_as_the_mesh_time_arrives() {
        each_seed("start a run", |seed| {
            let (returned, runs, statuses) =
                unsynced(seed, Span::SECOND, Span::SECOND, true);
            assert_eq!(returned, Span::SECOND, "seed {seed}: returns at the cancel");
            assert_eq!(statuses, [], "seed {seed}: no status channel");
            match runs[..] {
                [] => false,
                [at] if at == Span::SECOND => true,
                _ => panic!("seed {seed}: no run, or one run at the cancel: {runs:?}"),
            }
        });
    }

    #[test]
    fn returns_at_a_cancel_before_the_call_on_status_channels_that_are_gone() {
        let runs = run_on(move |node, tasks| async move {
            let script = Script::default();
            let runs = Arc::clone(&script.runs);
            let supervisor = Supervisor::new(Config {
                kinds: Arc::new(Table::new().with("script", script)),
                clock: node.clock(),
                entropy: node.entropy(),
                net: node.net(),
                roots: http::Roots::default(),
                tasks: tasks.clone(),
                hub: hub::testing::open(env(&node, tasks.clone())).await.0,
                secrets: testing::create_secrets(node.files()),
            });
            let token = Token::new();
            token.cancel();
            let name = name("plant.script");
            let result = supervisor
                .open("script", name, &config(), &token)
                .await
                .run()
                .await;
            result.expect("the call returns ok");
            runs.lock().expect("no panic under the lock").len()
        });
        assert_eq!(runs, 0);
    }

    #[test]
    fn starts_the_first_run_once_the_node_has_mesh_time() {
        let two = Span::from_nanos(2_000_000_000);
        let three = Span::from_nanos(3_000_000_000);
        let (returned, runs, statuses) = unsynced(0, two, three, false);
        assert_eq!(runs, [two]);
        assert_eq!(returned, Span::from_nanos(3_000_000_000));
        assert_eq!(states(&statuses), [(0, 0, 0), (3, 0, 0), (2, 0, 0)]);
    }

    /// Runs a `Script` connector with no step on a node whose mesh time starts
    /// `delay` after the call, in a sim of `seed`, and cancels it at `cancel`, after
    /// it removes the status channels when `removed`. Checks that the call returns
    /// `Ok`, and gives when it returned and when each run started, and the status
    /// frames.
    fn unsynced(
        seed: u64,
        delay: Span,
        cancel: Span,
        removed: bool,
    ) -> (Span, Vec<Span>, Vec<Written>) {
        run_in(seed, move |node, tasks| async move {
            let hub =
                hub::testing::open_unsynced(env(&node, tasks.clone()), delay).await;
            let status = create_status("plant.script");
            hub.set_definitions(status.iter().map(|(name, def)| (name, def)));
            let statuses = read_status(&hub, "plant.script", &[], &tasks).await;
            let script = Script::default();
            let runs = Arc::clone(&script.runs);
            let clock = node.clock();
            let inputs = Config {
                kinds: Arc::new(Table::new().with("script", script)),
                clock: clock.clone(),
                entropy: node.entropy(),
                net: node.net(),
                roots: http::Roots::default(),
                tasks: tasks.clone(),
                hub,
                secrets: testing::create_secrets(node.files()),
            };
            let token = Token::new();
            let (canceller, sleeper) = (token.clone(), clock.clone());
            let definer = inputs.hub.clone();
            tasks.spawn(async move {
                sleeper.sleep(cancel).await;
                if removed {
                    definer.set_definitions(std::iter::empty());
                }
                canceller.cancel();
            });
            let start = clock.now();
            let name = name("plant.script");
            let supervisor = Supervisor::new(inputs);
            let result = supervisor
                .open("script", name, &config(), &token)
                .await
                .run()
                .await;
            result.expect("the call returns ok");
            let returned = clock.now() - start;
            let runs: Vec<_> = {
                let runs = runs.lock().expect("no panic under the lock");
                runs.iter().map(|(from, _)| *from - start).collect()
            };
            clock.sleep(Span::SECOND).await;
            let statuses = statuses.borrow().clone();
            (returned, runs, statuses)
        })
    }

    #[test]
    #[should_panic(
        expected = "the status channels of the connector plant.script do not \
                               open: no channel is named plant.script.status.time"
    )]
    fn panics_when_the_status_channels_are_not_defined() {
        run_on(|node, tasks| async move {
            let kinds = Table::new().with("script", Script::default());
            let inputs = create_config(&node, tasks, kinds, "plant.other").await;
            let name = name("plant.script");
            let supervisor = Supervisor::new(inputs);
            drop(
                supervisor
                    .open("script", name, &config(), &Token::new())
                    .await
                    .run()
                    .await,
            );
        });
    }

    #[test]
    #[should_panic(expected = "invariant: the plan refused the name of connector \
                               `plant.ccc")]
    fn panics_on_a_connector_name_that_makes_a_status_name_too_long() {
        run_on(|node, tasks| async move {
            let kinds = Table::new().with("script", Script::default());
            let inputs = create_config(&node, tasks, kinds, "plant.other").await;
            let name = name(&format!("plant.{}", "c".repeat(239)));
            let supervisor = Supervisor::new(inputs);
            drop(
                supervisor
                    .open("script", name, &config(), &Token::new())
                    .await
                    .run()
                    .await,
            );
        });
    }

    /// A kind that writes one sample of `plant.value` for each of `values`, `gap`
    /// apart, through a writer of its context at `authority` with `lease`, stamped
    /// with mesh time. It keeps the refusal of each write in `refusals`, or `None`
    /// when the home applied it.
    struct Write {
        values: Vec<i64>,
        authority: Authority,
        lease: Option<Span>,
        gap: Span,
        refusals: Arc<Mutex<Vec<Option<Refusal>>>>,
    }

    impl Write {
        /// One write of each of `values`, with no gap, at authority 1 and no lease.
        fn new(values: Vec<i64>) -> Self {
            Self {
                values,
                authority: Authority(1),
                lease: None,
                gap: Span::ZERO,
                refusals: Arc::default(),
            }
        }
    }

    impl Kind for Write {
        type Config = ();

        fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
            Ok(())
        }

        fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
            Ok(Channels::default())
        }

        async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
            let channels = vec![name("plant.value")];
            let mut writer = ctx
                .writer(channels, self.authority, self.lease)
                .await
                .expect("the writer opens");
            let mut last = None;
            for (i, sample) in self.values.iter().enumerate() {
                if i > 0 {
                    ctx.clock().sleep(self.gap).await;
                }
                let now = writer.now().nanos();
                let stamp = last.map_or(now, |last: i64| now.max(last + 1));
                last = Some(stamp);
                let outcomes = write_value(&mut writer, stamp, *sample);
                let refusal = match &outcomes[..] {
                    [hub::home::Outcome::Applied { .. }] => None,
                    [hub::home::Outcome::Refused { refusal, .. }] => {
                        Some(refusal.clone())
                    }
                    _ => panic!("one group applied or refused: {outcomes:?}"),
                };
                self.refusals.lock().expect("no panic").push(refusal);
            }
            Ok(())
        }
    }

    fn name(text: &str) -> Name {
        text.parse().expect("a valid name")
    }

    /// The status channels `plant.tally.status.<suffix>`, one for each of `suffixes`.
    fn tally_statuses(suffixes: &[&str]) -> Vec<Name> {
        (suffixes.iter())
            .map(|suffix| name(&format!("plant.tally.status.{suffix}")))
            .collect()
    }

    /// The names of the status channels of `connector` with `counts`, without their
    /// index.
    fn status_names(connector: &str, counts: &[&str]) -> Vec<Name> {
        let counts: Vec<_> = counts.iter().map(|count| name(count)).collect();
        let (_, channels) = status::channels(&name(connector), &counts).expect("names");
        channels.into_iter().map(|(channel, _)| channel).collect()
    }

    /// Writes `sample` at `stamp` to `plant.value` through `writer`, which writes
    /// `plant.value` and its index, and gives the outcomes.
    fn write_value(
        writer: &mut hub::writer::Writer,
        stamp: i64,
        sample: i64,
    ) -> Vec<hub::home::Outcome> {
        let draft = draft_value(writer, stamp, sample, 2);
        let outcomes = writer.write(Label::Path(Path::Live), draft);
        outcomes.expect("the home takes it").to_vec()
    }

    /// A frame of `sample` at `stamp` for `writer`, which writes `plant.value` at
    /// key `key` and its index.
    fn draft_value(
        writer: &hub::writer::Writer,
        stamp: i64,
        sample: i64,
        key: u128,
    ) -> frame::Draft {
        let entries = writer.set().entries();
        let entry = |key| {
            let key = channel::Key::from_u128(key);
            entries.iter().position(|entry| entry.key == key)
        };
        let (time, value) = (entry(1).expect("time"), entry(key).expect("value"));
        let group = entries[time].group;
        let mut series = [(time, 8), (value, 8)];
        series.sort_unstable();
        let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
        let time = draft.series_mut(time).expect("the index series");
        time.copy_from_slice(&stamp.to_le_bytes());
        let value = draft.series_mut(value).expect("the value series");
        value.copy_from_slice(&sample.to_le_bytes());
        draft.set_count(group, 1);
        draft
    }

    /// Defines `plant.time` (key 1), `plant.value` (key `key`, `i64`), and the status
    /// channels of `plant.write` on `hub`.
    fn define(hub: &hub::Hub, key: u128) {
        let time = Channel {
            key: channel::Key::from_u128(1),
            kind: spec::channel::Kind::Index {
                error: None,
                control: None,
            },
        };
        let i64 = DataType::Sample(Type::Scalar(Scalar::I64));
        let data = Data::new(time.key, None, i64, None).expect("no unit");
        let value = Channel {
            key: channel::Key::from_u128(key),
            kind: spec::channel::Kind::Data(data),
        };
        let (time, value) = (Definition::Channel(time), Definition::Channel(value));
        let mut definitions = create_status("plant.write");
        definitions.extend([(name("plant.time"), time), (name("plant.value"), value)]);
        hub.set_definitions(definitions.iter().map(|(name, def)| (name, def)));
    }

    /// The `i64` samples of the channel of key 2 in `received`.
    fn values(received: &Received<'_>) -> Vec<i64> {
        series(received, 2)
    }

    /// The texts of the `String` series of `key` in `received`.
    fn texts(received: &Received<'_>, key: u128) -> Vec<String> {
        let (data_type, count, bytes) = encoded(received, key);
        let mut raw = Vec::new();
        codec::decode(data_type, count, bytes, &mut raw).expect("decodes");
        let variable = codec::Variable::of(data_type).expect("a variable type");
        variable
            .samples(count, &raw)
            .map(|sample| std::str::from_utf8(sample).expect("UTF-8").to_owned())
            .collect()
    }

    /// The type, the count, and the encoded bytes of the series of `key` in
    /// `received`.
    fn encoded<'a>(received: &'a Received<'_>, key: u128) -> (Type, usize, &'a [u8]) {
        let set = received.set();
        let key = channel::Key::from_u128(key);
        let entry = set.entries().iter().position(|entry| entry.key == key);
        let entry = entry.expect("the set holds the channel");
        let range = received.view().range(set.entries()[entry].group);
        let count = range.expect("the group is present").count;
        let count = usize::try_from(count).expect("a count");
        let (_, bytes) = received
            .view()
            .iter()
            .find(|&(present, _)| present == entry)
            .expect("the view holds the series");
        (set.entries()[entry].data_type, count, bytes)
    }

    /// The samples of the channel of `key` in `received`, each widened to `i64`.
    fn series(received: &Received<'_>, key: u128) -> Vec<i64> {
        let (data_type, count, bytes) = encoded(received, key);
        let width = data_type.width().expect("a fixed width");
        let mut out = Vec::new();
        codec::decode(data_type, count, bytes, &mut out).expect("decodes");
        out.chunks(width)
            .map(|chunk| {
                let mut sample = [0; 8];
                sample[..width].copy_from_slice(chunk);
                i64::from_le_bytes(sample)
            })
            .collect()
    }

    /// Runs a `Script` connector whose one run gives a device error after 10 ms, after
    /// a status frame `lead` ahead, in a sim of `seed`. Gives the status, each stamp
    /// from the first frame, and the start of the second run, from the start of the
    /// call.
    fn after_a_frame_ahead(seed: u64, lead: Span) -> (Vec<Written>, Span) {
        run_in(seed, move |node, tasks| async move {
            let script = Script {
                steps: Mutex::new([Step::Device(ms(10))].into()),
                ..Script::default()
            };
            let runs = Arc::clone(&script.runs);
            let kinds = Table::new().with("script", script);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.script").await;
            let statuses = read_status(&inputs.hub, "plant.script", &[], &tasks).await;
            write_ahead(&inputs.hub, lead).await;
            let (clock, token) = (node.clock(), Token::new());
            let (canceller, sleeper) = (token.clone(), clock.clone());
            tasks.spawn(async move {
                sleeper.sleep(ms(5_000)).await;
                canceller.cancel();
            });
            let (supervisor, start) = (Supervisor::new(inputs), clock.now());
            let result = supervisor
                .open("script", name("plant.script"), &config(), &token)
                .await
                .run()
                .await;
            result.expect("ok");
            clock.sleep(Span::SECOND).await;
            let runs = runs.lock().expect("no panic under the lock").clone();
            let [_, (again, _)] = runs[..] else {
                panic!("seed {seed}: two runs: {runs:?}");
            };
            (statuses.borrow().clone(), again - start)
        })
    }

    /// The seed draws the first wait, so the next try comes before the frame's time
    /// or after it.
    #[test]
    fn writes_a_backoff_that_ends_at_the_next_try_after_a_frame_ahead() {
        let lead = ms(500);
        each_seed("try again after the frame's time", |seed| {
            let (statuses, again) = after_a_frame_ahead(seed, lead);
            let (at, samples, ..) = statuses
                .iter()
                .find(|(_, samples, ..)| samples[0] == 1)
                .unwrap_or_else(|| {
                    panic!("seed {seed}: a frame of state 1: {statuses:?}")
                });
            // `statuses` gives each stamp from the first frame: the one `lead`
            // ahead.
            let run = Span::from_nanos(again.nanos() - lead.nanos());
            if samples[3] == 0 {
                assert!(
                    run <= *at,
                    "seed {seed}: no backoff, so the next try at {run:?} passed by \
                     the frame's time: {statuses:?}"
                );
                return false;
            }
            let next = Span::from_nanos(at.nanos() + samples[3]);
            assert_eq!(
                next, run,
                "seed {seed}: the frame's time plus `backoff` is the next try: \
                 {statuses:?}"
            );
            true
        });
    }

    #[test]
    fn gives_a_kind_a_writer_whose_samples_a_hub_reader_gets_in_order() {
        let got = run_on(|node, tasks| async move {
            let mut inputs =
                create_config(&node, tasks, Table::new(), "plant.write").await;
            define(&inputs.hub, 2);
            let write = Write::new(vec![30, 10, 20]);
            let refusals = Arc::clone(&write.refusals);
            inputs.kinds = Arc::new(Table::new().with("write", write));
            let open = hub::reader::Config {
                select: Selector::new(["plant.value"]).expect("a selector"),
                mode: Mode::Complete,
                subject: name("test"),
                name: None,
                hold: Span::ZERO,
            };
            let mut reader = inputs.hub.reader(open).await.expect("the reader opens");
            let supervisor = Supervisor::new(inputs);
            let result = supervisor
                .open("write", name("plant.write"), &config(), &Token::new())
                .await
                .run()
                .await;
            result.expect("the run returns ok");
            assert_eq!(*refusals.lock().expect("no panic"), [None, None, None]);
            let mut got = Vec::new();
            for _ in 0..3 {
                got.extend(values(&reader.next().await.expect("a frame")));
            }
            got
        });
        assert_eq!(got, [30, 10, 20]);
    }

    /// The reader settings of `plant.value`, complete, with a hold of `hold`.
    fn settings(hold: Span) -> reader::Settings {
        reader::Settings {
            select: Selector::new(["plant.value"]).expect("a selector"),
            mode: Mode::Complete,
            hold,
        }
    }

    /// A kind with no count that reads with `settings`, whose run is `run`.
    fn reading<F>(settings: reader::Settings, run: F) -> Closure<F> {
        Closure {
            counts: Vec::new(),
            reader: Some(settings),
            run,
        }
    }

    /// What [`read_through`] gives.
    struct Read<T> {
        /// What the kind put in `got`.
        got: Vec<T>,
        end: Result<(), Error>,
        statuses: Vec<Written>,
    }

    /// Runs `kind` once as `plant.write`, reading with `settings` when it is some,
    /// while a task runs `beside` with the hub and the clock.
    fn read_through<F, R, T, B, S>(
        seed: u64,
        settings: Option<reader::Settings>,
        kind: F,
        beside: B,
    ) -> Read<T>
    where
        F: Fn(Context<()>, Arc<Mutex<Vec<T>>>) -> R + Send + Sync + 'static,
        R: Future<Output = Result<(), Error>> + 'static,
        T: Clone + Send + 'static,
        B: FnOnce(hub::Hub, Clock) -> S + Send + 'static,
        S: Future<Output = ()> + 'static,
    {
        run_in(seed, move |node, tasks| async move {
            let got = Arc::new(Mutex::new(Vec::new()));
            let into = Arc::clone(&got);
            let kind = Closure {
                counts: Vec::new(),
                reader: settings,
                run: move |ctx| kind(ctx, Arc::clone(&into)),
            };
            let kinds = Table::new().with("read", kind);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.write").await;
            define(&inputs.hub, 2);
            let statuses = read_status(&inputs.hub, "plant.write", &[], &tasks).await;
            tasks.spawn(beside(inputs.hub.clone(), node.clock()));
            let supervisor = Supervisor::new(inputs);
            let end = supervisor
                .open("read", name("plant.write"), &config(), &Token::new())
                .await
                .run()
                .await;
            node.clock().sleep(Span::SECOND).await;
            let got = got.lock().expect("no panic").clone();
            let statuses = statuses.borrow().clone();
            Read { got, end, statuses }
        })
    }

    /// Writes each `(at, value)` of `samples` at `at` from the call, through a hub
    /// writer as `plant.other`.
    async fn write_at(hub: hub::Hub, clock: Clock, samples: Vec<(Span, i64)>) {
        let channels = vec![name("plant.value")];
        let mut writer = open_writer(&hub, "plant.other", Authority(1), channels).await;
        let start = clock.now();
        for (at, value) in samples {
            clock.sleep_until(start + at).await;
            let stamp = writer.now().nanos();
            let outcomes = write_value(&mut writer, stamp, value);
            assert!(
                matches!(outcomes[..], [hub::home::Outcome::Applied { .. }]),
                "{outcomes:?}"
            );
        }
    }

    #[test]
    fn opens_the_reader_before_the_first_run() {
        let (before, got) = run_on(|node, tasks| async move {
            let (runs, got) =
                (Arc::new(AtomicUsize::new(0)), Arc::new(Mutex::new(None)));
            let (counter, into) = (Arc::clone(&runs), Arc::clone(&got));
            let kind = reading(settings(Span::ZERO), move |mut ctx: Context<()>| {
                counter.fetch_add(1, Ordering::Relaxed);
                let into = Arc::clone(&into);
                async move {
                    let mut reader = ctx.reader().expect("a reader");
                    let received = reader.next().await.expect("a frame");
                    *into.lock().expect("no panic") = Some(values(&received));
                    Ok(())
                }
            });
            let kinds = Table::new().with("read", kind);
            let inputs = create_config(&node, tasks, kinds, "plant.write").await;
            define(&inputs.hub, 2);
            let hub = inputs.hub.clone();
            let supervisor = Supervisor::new(inputs);
            let token = Token::new();
            let config = config();
            let opened = supervisor
                .open("read", name("plant.write"), &config, &token)
                .await;
            let before = runs.load(Ordering::Relaxed);
            let channels = vec![name("plant.value")];
            let mut writer =
                open_writer(&hub, "plant.other", Authority(1), channels).await;
            let stamp = writer.now().nanos();
            write_value(&mut writer, stamp, 5);
            opened.run().await.expect("ok");
            (before, got.lock().expect("no panic").take())
        });
        assert_eq!((before, got), (0, Some(vec![5])));
    }

    #[test]
    fn gives_a_kind_a_reader_that_gets_the_frames_of_a_hub_writer_in_order() {
        let writes = vec![(ms(100), 30), (ms(200), 10), (ms(300), 20)];
        let read = read_through(
            0,
            Some(settings(Span::ZERO)),
            |mut ctx, got| async move {
                let mut reader = ctx.reader().expect("a reader");
                for _ in 0..3 {
                    let received = reader.next().await.expect("a frame");
                    got.lock().expect("no panic").extend(values(&received));
                }
                Ok(())
            },
            |hub, clock| write_at(hub, clock, writes),
        );
        assert_eq!(read.got, [30, 10, 20]);
    }

    #[test]
    fn gives_the_reader_once_and_none_to_a_kind_that_reads_no_channel() {
        type Got = Arc<Mutex<Vec<(bool, bool)>>>;
        let take = |mut ctx: Context<()>, got: Got| async move {
            let first = ctx.reader().is_some();
            let second = ctx.reader().is_some();
            got.lock().expect("no panic").push((first, second));
            Ok(())
        };
        let reads = read_through(0, Some(settings(Span::ZERO)), take, |_, _| async {});
        let none = read_through(0, None, take, |_, _| async {});
        assert_eq!(
            (reads.got, none.got),
            (vec![(true, false)], vec![(false, false)])
        );
    }

    #[test]
    fn opens_a_new_reader_before_each_restart() {
        let writes = vec![(ms(100), 1), (ms(5_000), 2)];
        let read = read_through(
            0,
            Some(settings(Span::ZERO)),
            |mut ctx, got| async move {
                let mut reader = ctx.reader().expect("a reader");
                let received = reader.next().await.expect("a frame");
                let mut got = got.lock().expect("no panic");
                got.extend(values(&received));
                if got.len() == 1 {
                    return Err(Error::Retry("again".into()));
                }
                Ok(())
            },
            |hub, clock| write_at(hub, clock, writes),
        );
        read.end.expect("ok");
        assert_eq!(read.got, [1, 2]);
    }

    #[test]
    fn gives_a_kind_a_reader_that_an_open_of_the_connector_name_takes_over() {
        let read = read_through(
            0,
            Some(settings(Span::ZERO)),
            |mut ctx, got| async move {
                let mut reader = ctx.reader().expect("a reader");
                let ended = reader.next().await.expect_err("the session ends");
                got.lock().expect("no panic").push(ended);
                Ok(())
            },
            |hub, clock| async move {
                clock.sleep(ms(100)).await;
                let open = hub::reader::Config {
                    select: Selector::new(["plant.value"]).expect("a selector"),
                    mode: Mode::Complete,
                    subject: name("plant.write"),
                    name: Some(name("plant.write")),
                    hold: Span::ZERO,
                };
                let reader = hub.reader(open).await.expect("the reader opens");
                clock.sleep(Span::SECOND).await;
                drop(reader);
            },
        );
        assert_eq!(read.got, [hub::reader::Ended::Replaced]);
    }

    #[test]
    fn gives_a_kind_a_reader_in_the_mode_of_its_settings() {
        let writes = vec![(ms(100), 1), (ms(101), 2), (ms(102), 3)];
        let latest = reader::Settings {
            mode: Mode::Latest,
            ..settings(Span::ZERO)
        };
        let read = read_through(
            0,
            Some(latest),
            |mut ctx, got| async move {
                let mut reader = ctx.reader().expect("a reader");
                ctx.clock().sleep(ms(200)).await;
                let received = reader.next().await.expect("a frame");
                got.lock().expect("no panic").extend(values(&received));
                Ok(())
            },
            |hub, clock| write_at(hub, clock, writes),
        );
        assert_eq!(read.got, [3]);
    }

    /// The first frame of the reader of each of two calls of `plant.write` reading
    /// with `hold`, while a writer writes 7 at 100 ms and 8 at 300 ms. The second
    /// call opens 100 ms after the first one closed its reader.
    fn hold_twice(hold: Span) -> Option<Result<Vec<i64>, hub::reader::Ended>> {
        run_on(move |node, tasks| async move {
            let got = Arc::new(Mutex::new(Vec::new()));
            let into = Arc::clone(&got);
            let kind = reading(settings(hold), move |mut ctx: Context<()>| {
                let into = Arc::clone(&into);
                async move {
                    let mut reader = ctx.reader().expect("a reader");
                    let next = reader.next().await.map(|received| values(&received));
                    drop(reader);
                    into.lock()
                        .expect("no panic")
                        .push((ctx.clock().now(), next));
                    Ok(())
                }
            });
            let kinds = Table::new().with("read", kind);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.write").await;
            define(&inputs.hub, 2);
            let writes = vec![(ms(100), 7), (ms(300), 8)];
            tasks.spawn(write_at(inputs.hub.clone(), node.clock(), writes));
            let supervisor = Supervisor::new(inputs);
            let token = Token::new();
            for _ in 0..2 {
                let closed = got.lock().expect("no panic").first().map(|(at, _)| *at);
                if let Some(closed) = closed {
                    node.clock().sleep_until(closed + ms(100)).await;
                }
                let config = config();
                let opened = supervisor
                    .open("read", name("plant.write"), &config, &token)
                    .await;
                opened.run().await.expect("ok");
            }
            let got = got.lock().expect("no panic");
            got.get(1).map(|(_, next)| next.clone())
        })
    }

    #[test]
    fn holds_the_reader_of_a_kind_for_the_hold_of_its_settings() {
        for (hold, want) in [
            (Span::SECOND, Ok(vec![7])),
            (Span::from_nanos(ms(100).nanos() + 1), Ok(vec![7])),
            (ms(100), Ok(vec![8])),
            (ms(99), Ok(vec![8])),
            (Span::ZERO, Ok(vec![8])),
        ] {
            assert_eq!(hold_twice(hold), Some(want), "hold {hold:?}");
        }
    }

    /// The reader of each restart keeps the hold of its settings: a run within the hold
    /// opens at the held position, which no run acked, and reads its frame from disk.
    #[test]
    fn opens_the_reader_of_a_restart_with_the_hold_of_its_settings() {
        let hour = Span::from_nanos(Span::MINUTE.nanos() * 60);
        let read = read_through(
            0,
            Some(settings(hour)),
            |mut ctx, got| async move {
                let mut reader = ctx.reader().expect("a reader");
                let next = reader.next().await.map(|received| values(&received));
                let runs = {
                    let mut got = got.lock().expect("no panic");
                    got.push(next);
                    got.len()
                };
                if runs < 3 {
                    return Err(Error::Retry("again".into()));
                }
                Ok(())
            },
            |hub, clock| {
                let samples = (1..=600).map(|i| (ms(100 * i), i));
                write_at(hub, clock, samples.collect())
            },
        );
        read.end.expect("ok");
        assert_eq!(read.got, [Ok(vec![1]), Ok(vec![1]), Ok(vec![1])]);
    }

    /// A run whose held reader reads from disk while the pool has no block gets
    /// `Retry` from the end of its reader, and the next run reads the frame. The
    /// read waits for the commit of the records of the open, which frees a block, so
    /// the run fills the pool again after it.
    #[test]
    fn retries_a_read_from_disk_while_the_pool_is_full() {
        let hour = Span::from_nanos(Span::MINUTE.nanos() * 60);
        let read = read_through(
            0,
            Some(settings(hour)),
            |mut ctx, got| async move {
                let runs = got.lock().expect("no panic").len();
                let mut reader = ctx.reader().expect("a reader");
                let held = if runs == 1 {
                    let channels = status_names("plant.write", &[]);
                    let writer = ctx.writer(channels, Authority(1), None).await;
                    let writer = writer.expect("the writer opens");
                    let mut drafts = fill(&writer);
                    ctx.clock().sleep(Span::SECOND).await;
                    drafts.extend(fill(&writer));
                    Some((drafts, writer))
                } else {
                    None
                };
                let next = reader.next().await.map(|received| values(&received));
                drop(held);
                got.lock().expect("no panic").push(next.clone());
                next?;
                if runs == 0 {
                    return Err(Error::Retry("again".into()));
                }
                Ok(())
            },
            |hub, clock| write_at(hub, clock, vec![(ms(100), 1)]),
        );
        read.end.expect("ok");
        assert!(
            matches!(
                read.got[..],
                [
                    Ok(_),
                    Err(hub::reader::Ended::Pool(block::Error::Exhausted { .. })),
                    Ok(_)
                ]
            ),
            "{:?}",
            read.got
        );
        assert_eq!(read.got[2], Ok(vec![1]));
    }

    /// The first run acks the frame at 100 ms. The frame at 900 ms finds no room, so
    /// the held position of the next run is at a lost seq: it reads the frame at
    /// 1.5 s, with the lost seq as its gap. The pool fills before the tick of the
    /// records at 1 s, so the order of the two does not decide the run.
    #[test]
    fn opens_a_held_reader_at_a_lost_frame_at_the_frame_after_it() {
        let hour = Span::from_nanos(Span::MINUTE.nanos() * 60);
        let read = read_through(
            0,
            Some(settings(hour)),
            |mut ctx, got| async move {
                let runs = got.lock().expect("no panic").len();
                let mut reader = ctx.reader().expect("a reader");
                let next = match reader.next().await {
                    Ok(received) => {
                        let position = received.position();
                        let read = (values(&received), received.gap().is_some());
                        reader.ack(position);
                        Ok(read)
                    }
                    Err(ended) => Err(ended),
                };
                got.lock().expect("no panic").push(next.clone());
                if runs == 0 {
                    ctx.clock().sleep(ms(2_000)).await;
                }
                if runs == 1 {
                    return Ok(());
                }
                next?;
                Err(Error::Retry("again".into()))
            },
            |hub, clock| async move {
                let channels = vec![name("plant.value")];
                let mut writer =
                    open_writer(&hub, "plant.other", Authority(1), channels).await;
                let start = clock.now();
                clock.sleep_until(start + ms(100)).await;
                let stamp = writer.now().nanos();
                let outcomes = write_value(&mut writer, stamp, 1);
                assert!(
                    matches!(outcomes[..], [hub::home::Outcome::Applied { .. }]),
                    "{outcomes:?}"
                );
                clock.sleep_until(start + ms(900)).await;
                let mut held = fill(&writer);
                drop(held.pop());
                let stamp = writer.now().nanos();
                let outcomes = write_value(&mut writer, stamp, 2);
                assert!(
                    matches!(outcomes[..], [hub::home::Outcome::Lost { .. }]),
                    "{outcomes:?}"
                );
                drop(held);
                clock.sleep_until(start + ms(1_500)).await;
                let stamp = writer.now().nanos();
                write_value(&mut writer, stamp, 3);
            },
        );
        read.end.expect("ok");
        assert_eq!(read.got, [Ok((vec![1], false)), Ok((vec![3], true))]);
    }

    /// A held reader whose next seq the home lost for no room reads the frame after
    /// it in the same run, with the lost seq as its gap.
    #[test]
    fn reads_the_frame_after_one_lost_for_no_room_with_its_gap_in_the_same_run() {
        let hour = Span::from_nanos(Span::MINUTE.nanos() * 60);
        let read = read_through(
            0,
            Some(settings(hour)),
            |mut ctx, got| async move {
                let mut reader = ctx.reader().expect("a reader");
                for _ in 0..2 {
                    let received = reader.next().await.expect("a frame");
                    let read = (values(&received), received.gap());
                    got.lock().expect("no panic").push(read);
                }
                Ok(())
            },
            |hub, clock| async move {
                let channels = vec![name("plant.value")];
                let mut writer =
                    open_writer(&hub, "plant.other", Authority(1), channels).await;
                for value in 1..=3 {
                    clock.sleep(ms(100)).await;
                    let draft = draft_value(&writer, writer.now().nanos(), value, 2);
                    let held = if value == 2 {
                        fill(&writer)
                    } else {
                        Vec::new()
                    };
                    let outcomes = writer.write(Label::Path(Path::Live), draft);
                    let outcomes = outcomes.expect("the home takes it").to_vec();
                    let want = frame::Range { seq: 1, count: 1 };
                    match (value, &outcomes[..]) {
                        (2, [hub::home::Outcome::Lost { range, .. }]) => {
                            assert_eq!(*range, want);
                        }
                        (1 | 3, [hub::home::Outcome::Applied { .. }]) => {}
                        _ => panic!("{value}: {outcomes:?}"),
                    }
                    drop(held);
                }
            },
        );
        read.end.expect("ok");
        assert_eq!(read.got, [(vec![1], None), (vec![3], Some(1..2))]);
    }

    /// The reader of a restart opens before the start status, as the first reader
    /// does, so each run reads `state` 0 of its own start. A run that gets no frame in
    /// 10 s records `None`.
    #[test]
    fn the_reader_of_each_run_sees_the_start_of_that_run() {
        let settings = reader::Settings {
            select: Selector::new(["plant.write.status.state"]).expect("a selector"),
            ..settings(Span::ZERO)
        };
        let read = read_through(
            0,
            Some(settings),
            |mut ctx, got| async move {
                let mut reader = ctx.reader().expect("a reader");
                let clock = ctx.clock().clone();
                let mut next = pin!(reader.next());
                let mut late =
                    pin!(clock.sleep(Span::from_nanos(Span::SECOND.nanos() * 10)));
                let next = poll_fn(|cx| {
                    if let Poll::Ready(next) = next.as_mut().poll(cx) {
                        let next = next.map(|got| series(&got, STATUS.as_u128() + 1));
                        return Poll::Ready(Some(next));
                    }
                    late.as_mut().poll(cx).map(|()| None)
                })
                .await;
                let runs = {
                    let mut got = got.lock().expect("no panic");
                    got.push(next);
                    got.len()
                };
                if runs < 3 {
                    return Err(Error::Retry("again".into()));
                }
                Ok(())
            },
            |_, _| async {},
        );
        read.end.expect("ok");
        let start = Some(Ok(vec![0]));
        assert_eq!(read.got, [start.clone(), start.clone(), start]);
    }

    /// The reader of a restart opens after the backoff, so the first frame of each
    /// restart was written at or after its start. Each frame holds the time of its
    /// write by the node's clock.
    #[test]
    fn the_reader_of_a_restart_opens_after_the_backoff() {
        let read = read_through(
            0,
            Some(settings(Span::ZERO)),
            |mut ctx, got| async move {
                let start = ctx.clock().now();
                let mut reader = ctx.reader().expect("a reader");
                let next = reader.next().await.map(|received| values(&received));
                let runs = {
                    let mut got = got.lock().expect("no panic");
                    got.push((start, next));
                    got.len()
                };
                if runs < 4 {
                    return Err(Error::Retry("again".into()));
                }
                Ok(())
            },
            |hub, clock| async move {
                let channels = vec![name("plant.value")];
                let mut writer =
                    open_writer(&hub, "plant.other", Authority(1), channels).await;
                for _ in 0..1_000 {
                    clock.sleep(ms(10)).await;
                    let stamp = writer.now().nanos();
                    let at = i64::try_from(clock.now().0).expect("fits");
                    write_value(&mut writer, stamp, at);
                }
            },
        );
        read.end.expect("ok");
        assert_eq!(read.got.len(), 4);
        for (start, next) in &read.got[1..] {
            let written = next.as_ref().expect("a frame")[0];
            let start = i64::try_from(start.0).expect("fits");
            assert!(written >= start, "{:?}", read.got);
        }
    }

    /// The diagnostics of `error` when it is [`Error::Config`], else its text.
    fn diagnostics(error: Error) -> Result<Vec<Diagnostic>, String> {
        match error {
            Error::Config(diagnostics) => Ok(diagnostics),
            error => Err(error.to_string()),
        }
    }

    /// One diagnostic with no span.
    fn unplaced(code: &'static str, message: &str, fix: &str) -> Vec<Diagnostic> {
        vec![Diagnostic::new(
            Code::new(code),
            None,
            message.into(),
            fix.into(),
        )]
    }

    /// The first run's reader ends when a spec change removes the channels it reads,
    /// and the run returns `Retry`. The reader open of the restart gives `Config`.
    #[test]
    fn ends_with_a_config_error_when_the_reader_of_a_restart_matches_nothing() {
        let read = read_through(
            0,
            Some(settings(Span::ZERO)),
            |mut ctx, got| async move {
                let mut reader = ctx.reader().expect("a reader");
                let ended = reader.next().await.expect_err("the session ends");
                got.lock().expect("no panic").push(ended);
                Err(Error::Retry("again".into()))
            },
            |hub, clock| async move {
                clock.sleep(ms(100)).await;
                let definitions = create_status("plant.write");
                hub.set_definitions(definitions.iter().map(|(name, def)| (name, def)));
            },
        );
        assert_eq!(read.got.len(), 1, "{:?}", read.got);
        let want = unplaced(
            "connector.no-channel",
            "the selector matches no channel",
            "Select a channel that the spec defines",
        );
        assert_eq!(read.end.map_err(diagnostics), Err(Ok(want)));
        assert_eq!(
            states(&read.statuses),
            [
                (0, 0, 0),
                (3, 3, 0),
                (1, 3, 0),
                (0, 3, 1),
                (3, 1, 1),
                (2, 1, 1)
            ],
            "{:?}",
            read.statuses
        );
    }

    /// One spec change removes `plant.value` and defines it again at key 3. The
    /// reader ends, the run returns the end with `?`, which is `Retry`, and the reader
    /// of the restart reads key 3.
    #[test]
    fn reads_a_channel_that_a_spec_change_defined_again_with_a_new_key() {
        for seed in 0..SEEDS {
            let read = read_through(
                seed,
                Some(settings(Span::ZERO)),
                |mut ctx, got| async move {
                    let mut reader = ctx.reader().expect("a reader");
                    let next = reader.next().await;
                    let read = next.as_ref().map(|received| series(received, 3));
                    got.lock()
                        .expect("no panic")
                        .push(read.map_err(Clone::clone));
                    let _received = next?;
                    Ok(())
                },
                |hub, clock| async move {
                    clock.sleep(ms(100)).await;
                    define(&hub, 3);
                    clock.sleep(ms(1_500)).await;
                    let channels = vec![name("plant.value")];
                    let mut writer =
                        open_writer(&hub, "plant.other", Authority(1), channels).await;
                    let draft = draft_value(&writer, writer.now().nanos(), 7, 3);
                    let outcomes = writer.write(Label::Path(Path::Live), draft);
                    outcomes.expect("the home takes it");
                },
            );
            assert!(read.end.is_ok(), "seed {seed}: {:?}", read.end);
            let removed = hub::reader::Ended::Removed(channel::Key::from_u128(2));
            let text = removed.to_string();
            assert_eq!(read.got, [Err(removed), Ok(vec![7])], "seed {seed}");
            assert_eq!(
                states(&read.statuses),
                [
                    (0, 0, 0),
                    (3, 3, 0),
                    (1, 3, 0),
                    (0, 3, 1),
                    (3, 0, 1),
                    (2, 0, 1)
                ],
                "seed {seed}: {:?}",
                read.statuses
            );
            let wait = errors(&read.statuses)[2].0;
            assert!(
                wait <= RESTART.first,
                "seed {seed}: the first wait of `RESTART` is at most 1 s: {wait:?}"
            );
            assert_eq!(
                wait,
                between(read.statuses[2].0, read.statuses[3].0),
                "seed {seed}: the wait from its write to the restart: {:?}",
                read.statuses
            );
            assert_eq!(
                errors(&read.statuses),
                [
                    (Span::ZERO, ""),
                    (Span::ZERO, text.as_str()),
                    (wait, text.as_str()),
                    (Span::ZERO, text.as_str()),
                    (Span::ZERO, ""),
                    (Span::ZERO, "")
                ],
                "seed {seed}"
            );
        }
    }

    /// One spec change removes `plant.value` and defines it again at key 3. The
    /// write of the run gives `Removed`, the run returns it with `?`, which is
    /// `Retry`, and the writer of the restart writes key 3. Lost: the frame of the
    /// refused write, and each sample until the restart, after the first wait of the
    /// backoff.
    #[test]
    fn writes_a_channel_that_a_spec_change_defined_again_with_a_new_key() {
        for seed in 0..SEEDS {
            let read = read_through(
                seed,
                None,
                |ctx, got| async move {
                    let channels = vec![name("plant.value")];
                    let writer = ctx.writer(channels, Authority(1), None).await;
                    let mut writer = writer.expect("opens");
                    let first = got.lock().expect("no panic").is_empty();
                    let (key, sample) = if first { (2, 5) } else { (3, 7) };
                    if first {
                        ctx.clock().sleep(ms(200)).await;
                    }
                    let draft = draft_value(&writer, writer.now().nanos(), sample, key);
                    let written = writer.write(Label::Path(Path::Live), draft);
                    let applied = |outcomes: &[_]| {
                        matches!(outcomes, [hub::home::Outcome::Applied { .. }])
                    };
                    let got_written = written.as_deref().map(applied);
                    got.lock()
                        .expect("no panic")
                        .push(got_written.map_err(Clone::clone));
                    let _outcomes = written?;
                    Ok(())
                },
                |hub, clock| async move {
                    clock.sleep(ms(100)).await;
                    define(&hub, 3);
                },
            );
            assert!(read.end.is_ok(), "seed {seed}: {:?}", read.end);
            let removed = hub::writer::Failure::Removed(channel::Key::from_u128(2));
            let text = removed.to_string();
            assert_eq!(read.got, [Err(removed), Ok(true)], "seed {seed}");
            assert_eq!(
                states(&read.statuses),
                [
                    (0, 0, 0),
                    (3, 3, 0),
                    (1, 3, 0),
                    (0, 3, 1),
                    (3, 0, 1),
                    (2, 0, 1)
                ],
                "seed {seed}"
            );
            let wait = errors(&read.statuses)[2].0;
            assert!(
                wait <= RESTART.first,
                "seed {seed}: the first wait of `RESTART` is at most 1 s: {wait:?}"
            );
            assert_eq!(
                wait,
                between(read.statuses[2].0, read.statuses[3].0),
                "seed {seed}: the wait from its write to the restart: {:?}",
                read.statuses
            );
            assert_eq!(
                errors(&read.statuses),
                [
                    (Span::ZERO, ""),
                    (Span::ZERO, text.as_str()),
                    (wait, text.as_str()),
                    (Span::ZERO, text.as_str()),
                    (Span::ZERO, ""),
                    (Span::ZERO, "")
                ],
                "seed {seed}"
            );
        }
    }

    #[test]
    fn ends_with_a_config_error_for_a_reader_that_only_a_spec_change_fixes() {
        for (select, want) in [
            (
                "plant.missing",
                unplaced(
                    "connector.no-channel",
                    "the selector matches no channel",
                    "Select a channel that the spec defines",
                ),
            ),
            (
                "plant.**",
                unplaced(
                    "connector.many-indexes",
                    "the channels plant.time and plant.write.status.backoff are on \
                     different indexes: open a reader per index",
                    "Select channels of one index",
                ),
            ),
        ] {
            let settings = reader::Settings {
                select: Selector::new([select]).expect("a selector"),
                ..settings(Span::ZERO)
            };
            let read = read_through(
                0,
                Some(settings),
                |_, got: Arc<Mutex<Vec<()>>>| async move {
                    got.lock().expect("no panic").push(());
                    Ok(())
                },
                |_, _| async {},
            );
            let text = want[0].to_string();
            let end = read.end.map_err(diagnostics);
            assert_eq!(end, Err(Ok(want)), "select {select}");
            assert_eq!(read.got, [], "no run: select {select}");
            let classes = states(&read.statuses);
            assert_eq!(classes, [(0, 0, 0), (3, 1, 0), (2, 1, 0)], "{select}");
            let texts: Vec<_> =
                read.statuses.iter().map(|(_, _, text, _)| text).collect();
            assert_eq!(texts, ["", &text, &text], "select {select}");
        }
    }

    #[test]
    fn writes_only_stopped_for_a_reading_kind_cancelled_before_the_call() {
        let (end, statuses) = run_on(|node, tasks| async move {
            let kind = reading(settings(Span::ZERO), |_| async {
                panic!("no run starts after the cancel")
            });
            let kinds = Table::new().with("read", kind);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.write").await;
            define(&inputs.hub, 2);
            let statuses = read_status(&inputs.hub, "plant.write", &[], &tasks).await;
            let supervisor = Supervisor::new(inputs);
            let token = Token::new();
            token.cancel();
            let config = config();
            let opened = supervisor
                .open("read", name("plant.write"), &config, &token)
                .await;
            let end = opened.run().await;
            node.clock().sleep(Span::SECOND).await;
            (end, statuses.borrow().clone())
        });
        end.expect("ok");
        assert_eq!(states(&statuses), [(2, 0, 0)]);
    }

    #[test]
    fn writes_no_status_when_an_opened_connector_drops() {
        let (runs, statuses, debug) = run_on(|node, tasks| async move {
            let runs = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&runs);
            let kind = reading(settings(Span::ZERO), move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
                async { Ok(()) }
            });
            let kinds = Table::new().with("read", kind);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.write").await;
            define(&inputs.hub, 2);
            let statuses = read_status(&inputs.hub, "plant.write", &[], &tasks).await;
            let supervisor = Supervisor::new(inputs);
            let token = Token::new();
            let config = config();
            let opened = supervisor
                .open("read", name("plant.write"), &config, &token)
                .await;
            let debug = format!("{opened:?}");
            drop(opened);
            node.clock().sleep(Span::SECOND).await;
            (
                runs.load(Ordering::Relaxed),
                statuses.borrow().clone(),
                debug,
            )
        });
        assert_eq!((runs, statuses), (0, Vec::new()));
        assert!(debug.starts_with("Opened { call: Call { "), "{debug}");
        assert!(debug.contains("kind: \"read\""), "{debug}");
        assert!(debug.ends_with(", .. }"), "{debug}");
    }

    #[test]
    fn gives_a_config_error_for_a_writer_that_only_a_spec_change_fixes() {
        for (channels, lease, want) in [
            (
                vec!["plant.missing"],
                None,
                unplaced(
                    "connector.unknown-channel",
                    "no channel is named plant.missing",
                    "Define the channel in the spec",
                ),
            ),
            (
                Vec::new(),
                None,
                unplaced(
                    "connector.no-channel",
                    "a writer names at least one channel",
                    "Give the connector a channel to write",
                ),
            ),
            (
                vec!["plant.value"],
                Some(Span::ZERO),
                unplaced(
                    "connector.lease",
                    "control lease must be longer than zero, got 0s",
                    "Give a lease longer than zero, or none",
                ),
            ),
        ] {
            let channels: Vec<Name> = channels.into_iter().map(name).collect();
            let open = channels.clone();
            let read = read_through(
                0,
                None,
                move |ctx, got| {
                    let open = open.clone();
                    async move {
                        let writer = ctx.writer(open, Authority(1), lease).await;
                        let error = writer.expect_err("an error");
                        got.lock().expect("no panic").push(diagnostics(error));
                        Ok(())
                    }
                },
                |_, _| async {},
            );
            assert_eq!(read.got, [Ok(want)], "channels {channels:?}");
        }
    }

    /// Runs `write` as `plant.write` on a new shard, while a writer as
    /// `plant.other` at `holder` holds `plant.value` when it is some. Gives the
    /// refusal of each write, or `None` for one the home applied.
    fn refusals(holder: Option<Authority>, write: Write) -> Vec<Option<Refusal>> {
        run_on(move |node, tasks| async move {
            let mut inputs =
                create_config(&node, tasks, Table::new(), "plant.write").await;
            define(&inputs.hub, 2);
            let refusals = Arc::clone(&write.refusals);
            inputs.kinds = Arc::new(Table::new().with("write", write));
            let mut other = None;
            if let Some(authority) = holder {
                let channels = vec![name("plant.value")];
                let hub = &inputs.hub;
                other =
                    Some(open_writer(hub, "plant.other", authority, channels).await);
            }
            let supervisor = Supervisor::new(inputs);
            let result = supervisor
                .open("write", name("plant.write"), &config(), &Token::new())
                .await
                .run()
                .await;
            result.expect("the run returns ok");
            drop(other);
            refusals.lock().expect("no panic").clone()
        })
    }

    /// Writes 9 at the mesh time to `plant.value` as `subject` at authority 1.
    async fn write_as(hub: &hub::Hub, subject: &str) -> Vec<hub::home::Outcome> {
        let channels = vec![name("plant.value")];
        let mut writer = open_writer(hub, subject, Authority(1), channels).await;
        let now = writer.now().nanos();
        write_value(&mut writer, now, 9)
    }

    /// The writer of a kind holds control as its connector: after a power cut, until
    /// the grace ends, the home keeps control for the connector's name.
    #[test]
    fn holds_control_for_the_connector_across_a_power_cut() {
        let write = Write {
            gap: Span::HOUR,
            ..Write::new(vec![7, 8])
        };
        let refusals = Arc::clone(&write.refusals);
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let shard = env::shards::Config {
            name: "shard-0".into(),
            core: None,
        };
        let started = node.shards().start(shard, {
            let node = node.clone();
            move |tasks| async move {
                let mut inputs =
                    create_config(&node, tasks, Table::new(), "plant.write").await;
                define(&inputs.hub, 2);
                inputs.kinds = Arc::new(Table::new().with("write", write));
                let supervisor = Supervisor::new(inputs);
                let (name, token) = (name("plant.write"), Token::new());
                let result = supervisor
                    .open("write", name, &config(), &token)
                    .await
                    .run()
                    .await;
                result.expect("the run returns ok");
            }
        });
        let _shard = started.expect("the shard starts");
        // The kind's writer stays open: a closed one records a handoff with no holder.
        sim.run_for(Span::SECOND).expect("the run goes on");
        sim.crash(&node, sim::Crash::Power);
        let (other, own) = sim
            .run_on(&node, |node, tasks| async move {
                let (hub, _) = hub::testing::open(env(&node, tasks)).await;
                define(&hub, 2);
                let other = write_as(&hub, "plant.other").await;
                (other, write_as(&hub, "plant.write").await)
            })
            .expect("the run after the cut ends");

        assert_eq!(*refusals.lock().expect("no panic"), [None]);
        // The restart gives slots in the order of the ring's entries, and the status
        // index's come first.
        let slot = channel::Slot::new(1);
        let refusal = Refusal::Reserved;
        assert_eq!(other, [hub::home::Outcome::Refused { slot, refusal }]);
        // Seq 1: the frame of the kind is on disk.
        let range = frame::Range { seq: 1, count: 1 };
        assert_eq!(own, [hub::home::Outcome::Applied { slot, range }]);
    }

    #[test]
    fn gives_a_kind_control_only_over_a_holder_of_lower_authority() {
        for (kind, holder, want) in [
            (1, 0, None),
            (1, 1, Some(Refusal::Waiting)),
            (2, 1, None),
            (2, 2, Some(Refusal::Waiting)),
            (255, 254, None),
            (255, 255, Some(Refusal::Waiting)),
        ] {
            let write = Write {
                authority: Authority(kind),
                ..Write::new(vec![7])
            };
            let refusals = refusals(Some(Authority(holder)), write);
            assert_eq!(refusals, [want], "kind {kind}, holder {holder}");
        }
    }

    /// The refusals of two writes of a kind with `lease`, `gap` apart.
    fn two_writes(lease: Option<Span>, gap: Span) -> Vec<Option<Refusal>> {
        let write = Write {
            lease,
            gap,
            ..Write::new(vec![7, 8])
        };
        refusals(None, write)
    }

    #[test]
    fn applies_the_writes_of_a_kind_with_no_lease_a_day_apart() {
        assert_eq!(two_writes(None, Span::DAY), [None, None]);
    }

    #[test]
    fn refuses_the_write_of_a_kind_once_its_lease_runs_out() {
        for nanos in [
            Span::SECOND,
            Span::from_nanos(3 * Span::SECOND.nanos()),
            Span::DAY,
        ]
        .map(Span::nanos)
        {
            let lease = Some(Span::from_nanos(nanos));
            let just_before = Span::from_nanos(nanos - 1);
            let at = Span::from_nanos(nanos);
            assert_eq!(
                two_writes(lease, just_before),
                [None, None],
                "a lease of {nanos} ns, the second write 1 ns before its end"
            );
            assert_eq!(
                two_writes(lease, at),
                [None, Some(Refusal::Expired)],
                "a lease of {nanos} ns, the second write at its end"
            );
        }
    }

    /// A kind whose browse reads the mesh time and gives no entries.
    struct Mesh;

    impl Kind for Mesh {
        type Config = ();

        fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
            Ok(())
        }

        fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
            Ok(Channels::default())
        }

        fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), Error>> {
            std::future::ready(Ok(()))
        }

        async fn browse(
            &self,
            ctx: &browse::Context<'_>,
            _: &browse::Query,
        ) -> Result<browse::Found, Error> {
            ctx.time().await?;
            Ok(browse::Found {
                entries: Vec::new(),
                values: Vec::new(),
                end: browse::End::Done,
            })
        }
    }

    /// What a browse of [`Mesh`] gives on a node whose mesh time starts `delay` after
    /// the call, and when it returned.
    fn browse_unsynced(delay: Span) -> (Result<browse::Found, Error>, Span) {
        run_on(move |node, tasks| async move {
            let clock = node.clock();
            let inputs = Config {
                kinds: Arc::new(Table::new().with("mesh", Mesh)),
                clock: clock.clone(),
                entropy: node.entropy(),
                net: node.net(),
                roots: http::Roots::default(),
                tasks: tasks.clone(),
                hub: hub::testing::open_unsynced(env(&node, tasks), delay).await,
                secrets: testing::create_secrets(node.files()),
            };
            let start = clock.now();
            let query = browse::Query {
                start: browse::Path::root(),
                scope: browse::Scope::Level(None),
                values: browse::Values::Skipped,
            };
            let found = Supervisor::new(inputs)
                .browse("mesh", &config(), &query)
                .await;
            (found, clock.now() - start)
        })
    }

    #[test]
    fn a_browse_reads_the_mesh_time_once_the_node_has_it() {
        let (found, returned) = browse_unsynced(Span::from_nanos(4_000_000_000));
        assert_eq!(found.expect("found").end, browse::End::Done);
        assert_eq!(returned, Span::from_nanos(4_000_000_000));
    }

    #[test]
    fn a_browse_gives_a_retry_when_the_node_has_no_mesh_time_at_the_bound() {
        let (found, returned) = browse_unsynced(Span::from_nanos(6_000_000_000));
        let Err(Error::Retry(error)) = found else {
            panic!("not a retry: {found:?}");
        };
        assert_eq!(error.to_string(), "the node has no mesh time after 5s");
        assert_eq!(returned, browse::WAIT);
    }

    #[test]
    fn a_browse_reads_the_mesh_time_that_starts_at_the_bound() {
        let (found, returned) = browse_unsynced(browse::WAIT);
        assert_eq!(found.expect("found").end, browse::End::Done);
        assert_eq!(returned, browse::WAIT);
    }
}
