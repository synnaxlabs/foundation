//! The contract every connector kind implements, and the table of kinds.

use std::collections::BTreeMap;
use std::fmt;
use std::pin::Pin;
use std::rc::Rc;

use document::diagnostic::{Code, Diagnostic};
use document::{Document, Span};
use env::clock::Clock;
use env::net::Net;
use env::rng::Rng;
use env::tasks::Tasks;
use types::authority::Authority;
use types::name::Name;
use types::time;

use crate::{cancel, status, supervisor};

/// The noun of the document that [`Kind::parse`] gets, for the text of a diagnostic.
pub const NOUN: &str = "the connector";

/// One connector kind: it owns its config's fields and how it runs. `node` builds one
/// value per kind for the life of the process; every connector of the kind shares it,
/// so it holds registries, dialers, and vendor libraries.
pub trait Kind: Send + Sync + 'static {
    /// The decoded config of one connector.
    type Config;

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

    /// Finds connectors this node can run, as config documents.
    fn discover(
        &self,
        cancel: &cancel::Token,
    ) -> impl Future<Output = Result<Vec<Document>, Error>>;

    /// Runs one connector until `ctx.cancel()` is cancelled or an error leaves it.
    fn run(
        &self,
        ctx: Context<Self::Config>,
    ) -> impl Future<Output = Result<(), Error>>;
}

/// What a checked connector reads and writes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Channels {
    /// The channels it reads from the mesh: commands for the device, or samples it
    /// sends out.
    pub reads: Vec<Name>,
    /// The channels it writes to the mesh: samples from the device.
    pub writes: Vec<Name>,
    /// The last segment of each count channel of the kind,
    /// `<connector>.status.<count>`.
    pub counts: Vec<Name>,
}

/// Why a run or a discovery stopped. One handler for each variant.
#[derive(Debug)]
pub enum Error {
    /// The config cannot work. The supervisor stops; a spec change that changes the
    /// connector starts it again.
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
                write!(f, "the config cannot work")?;
                let mut separator = ": ";
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
        inputs: Rc<supervisor::Config>,
    ) -> Self {
        Self {
            name,
            config,
            cancel,
            tasks,
            status,
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
    /// is the connector's name.
    ///
    /// # Errors
    ///
    /// As [`hub::Hub::writer`].
    pub async fn writer(
        &self,
        channels: Vec<Name>,
        authority: Authority,
        lease: Option<time::Span>,
    ) -> Result<hub::writer::Writer, hub::writer::Error> {
        let config = hub::writer::Config {
            subject: self.name.clone(),
            authority,
            lease,
            channels,
        };
        self.inputs.hub.writer(config).await
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

    /// Connects streams and datagrams.
    #[must_use]
    pub fn net(&self) -> &Net {
        &self.inputs.net
    }

    /// Runs the kind's own tasks on its shard. Each task must end when
    /// [`Context::cancel`] is cancelled. The next run of the same `Supervisor::run`
    /// call starts only after each of them ends.
    #[must_use]
    pub fn tasks(&self) -> &Tasks {
        &self.tasks
    }
}

/// The kinds this binary has, by name. `node` builds it once.
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

    /// Finds connectors of `kind` that this node can run.
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

/// One run of a kind.
pub(crate) type Run<'a> = Pin<Box<dyn Future<Output = Result<(), Error>> + 'a>>;

/// A [`Kind`] with its config type erased, so one table holds every kind.
trait Erased: Send + Sync {
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
}

impl<K: Kind> Erased for K {
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
                reads: Vec::new(),
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
                reads: Vec::new(),
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

        fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
            Ok(())
        }

        fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
            Ok(Channels {
                counts: self.0.iter().map(|count| name(count)).collect(),
                ..Channels::default()
            })
        }

        fn discover(
            &self,
            _: &cancel::Token,
        ) -> impl Future<Output = Result<Vec<Document>, Error>> {
            std::future::ready(Ok(Vec::new()))
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
                status::Status::new(Vec::new()),
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
            let status = status::Status::new(Vec::new());
            let ctx = Context::new(name("a"), (), Token::new(), tasks, status, inputs);
            (ctx.rng().next_u64(), ctx.rng().next_u64())
        });
        assert_ne!(a, b);
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

    #[test]
    fn shows_every_diagnostic_of_a_config_error() {
        let config = Error::Config(vec![
            diagnostic(RANGE, "n is over 8"),
            diagnostic(MISSING, "no integer n"),
        ]);
        assert_eq!(
            config.to_string(),
            "the config cannot work: n is over 8. Fix it; no integer n. Fix it"
        );
        assert!(std::error::Error::source(&config).is_none());
    }
}
