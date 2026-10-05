//! The contract every connector kind implements, and the table of kinds.

use std::collections::BTreeMap;
use std::fmt;
use std::pin::Pin;

use document::Document;
use document::diagnostic::{Code, Diagnostic};
use env::clock::Clock;
use env::rng::Rng;
use types::name::Name;

use crate::cancel;

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
    /// The channels it reads from the device.
    pub reads: Vec<Name>,
    /// The channels it writes to the device.
    pub writes: Vec<Name>,
}

/// Why a run or a discovery stopped. One handler for each variant.
#[derive(Debug)]
pub enum Error {
    /// The config cannot work. The supervisor stops; a spec change starts it again.
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
                diagnostics.iter().try_for_each(|d| write!(f, ": {d}"))
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

/// One run's capabilities.
pub struct Context<C> {
    name: Name,
    config: C,
    cancel: cancel::Token,
    clock: Clock,
    rng: Rng,
}

impl<C> Context<C> {
    #[cfg_attr(not(test), expect(dead_code, reason = "the supervisor calls it"))]
    pub(crate) fn new(
        name: Name,
        config: C,
        cancel: cancel::Token,
        clock: Clock,
        rng: Rng,
    ) -> Self {
        Self {
            name,
            config,
            cancel,
            clock,
            rng,
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

    /// Cancelled when the run must stop.
    #[must_use]
    pub fn cancel(&self) -> &cancel::Token {
        &self.cancel
    }

    /// The node's clock.
    #[must_use]
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// A random source that simulation replays.
    pub fn rng(&mut self) -> &mut Rng {
        &mut self.rng
    }
}

impl<C: fmt::Debug> fmt::Debug for Context<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("name", &self.name)
            .field("config", &self.config)
            .field("cancel", &self.cancel)
            .finish_non_exhaustive()
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
    /// The diagnostics of the kind's `parse` or `check`. An unknown kind gives one
    /// diagnostic, `connector.unknown-kind`, since the name comes from a file.
    pub fn check(
        &self,
        kind: &str,
        config: &Document,
    ) -> Result<Channels, Vec<Diagnostic>> {
        self.get(kind)?.check(config)
    }

    /// Finds connectors of `kind` that this node can run.
    ///
    /// # Errors
    ///
    /// The kind's error, or [`Error::Config`] with `connector.unknown-kind`.
    pub async fn discover(
        &self,
        kind: &str,
        cancel: &cancel::Token,
    ) -> Result<Vec<Document>, Error> {
        self.get(kind)
            .map_err(Error::Config)?
            .discover(cancel)
            .await
    }

    fn get(&self, kind: &str) -> Result<&dyn Erased, Vec<Diagnostic>> {
        let erased = self.kinds.get(kind).ok_or_else(|| {
            let names: Vec<_> = self.kinds.keys().copied().collect();
            vec![Diagnostic::new(
                UNKNOWN_KIND,
                None,
                format!("this build has no connector kind {kind:?}"),
                format!("Use one of {names:?}"),
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

/// A [`Kind`] with its config type erased, so one table holds every kind.
trait Erased: Send + Sync {
    fn check(&self, config: &Document) -> Result<Channels, Vec<Diagnostic>>;

    fn discover<'a>(
        &'a self,
        cancel: &'a cancel::Token,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Document>, Error>> + 'a>>;
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
}

#[cfg(test)]
mod tests {
    use document::value::{self, Value};
    use document::{Attribute, Map};

    use super::*;
    use crate::cancel::Token;
    use crate::common::run;

    const MISSING: Code = Code::new("test.missing");
    const RANGE: Code = Code::new("test.range");

    fn name(text: &str) -> Name {
        text.parse().expect("a valid name")
    }

    /// A kind whose config is one integer attribute `n`. A device takes `n` up to 8.
    struct Counter;

    impl Kind for Counter {
        type Config = i64;

        fn parse(&self, config: &Document) -> Result<i64, Vec<Diagnostic>> {
            match config.attributes.get("n").map(|a| &a.value.kind) {
                Some(value::Kind::Integer(n)) => i64::try_from(*n)
                    .map_err(|_| vec![diagnostic(RANGE, "n is over 8")]),
                _ => Err(vec![diagnostic(MISSING, "no integer n")]),
            }
        }

        fn check(&self, n: &i64) -> Result<Channels, Vec<Diagnostic>> {
            if *n > 8 {
                return Err(vec![diagnostic(RANGE, "n is over 8")]);
            }
            Ok(Channels {
                reads: (0..*n).map(|i| name(&format!("counter.c{i}"))).collect(),
                writes: Vec::new(),
            })
        }

        async fn discover(
            &self,
            cancel: &cancel::Token,
        ) -> Result<Vec<Document>, Error> {
            if cancel.cancelled() {
                return Err(Error::Retry("cancelled".into()));
            }
            Ok(vec![config(2)])
        }

        async fn run(&self, ctx: Context<i64>) -> Result<(), Error> {
            ctx.cancel().wait().await;
            Ok(())
        }
    }

    fn diagnostic(code: Code, message: &str) -> Diagnostic {
        Diagnostic::new(code, None, message.into(), "Fix it".into())
    }

    fn config(n: i64) -> Document {
        let n = Attribute {
            key: "n".into(),
            key_span: None,
            value: Value {
                kind: value::Kind::Integer(n.into()),
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
        let channels = table().check("counter", &config(2));
        let reads = vec![name("counter.c0"), name("counter.c1")];
        assert_eq!(
            channels,
            Ok(Channels {
                reads,
                writes: Vec::new()
            })
        );
    }

    #[test]
    fn returns_the_diagnostics_of_parse() {
        let result = table().check("counter", &Document::default());
        assert_eq!(result, Err(vec![diagnostic(MISSING, "no integer n")]));
    }

    #[test]
    fn returns_the_diagnostics_of_check() {
        let result = table().check("counter", &config(9));
        assert_eq!(result, Err(vec![diagnostic(RANGE, "n is over 8")]));
    }

    #[test]
    fn names_the_known_kinds_for_an_unknown_kind() {
        let table = table().with("other", Counter);
        let expected = Diagnostic::new(
            UNKNOWN_KIND,
            None,
            "this build has no connector kind \"modbus\"".into(),
            "Use one of [\"counter\", \"other\"]".into(),
        );
        assert_eq!(table.check("modbus", &config(1)), Err(vec![expected]));
    }

    #[test]
    #[should_panic(expected = "the kind \"counter\" is in the table twice")]
    fn panics_on_a_kind_added_twice() {
        let _ = table().with("counter", Counter);
    }

    #[test]
    fn discovers_through_its_kind() {
        let found =
            run(|_, _| async { table().discover("counter", &Token::new()).await });
        assert_eq!(found.expect("found"), [config(2)]);
    }

    #[test]
    fn returns_the_unknown_kind_from_discover_as_a_config_error() {
        let found =
            run(|_, _| async { table().discover("modbus", &Token::new()).await });
        let Err(Error::Config(diagnostics)) = found else {
            panic!("a config error: {found:?}");
        };
        let codes: Vec<_> = diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, [UNKNOWN_KIND]);
    }

    #[test]
    fn runs_until_cancelled_with_its_context() {
        let (out, kind_name, n) = run(|clock, _| async move {
            let token = Token::new();
            let ctx = Context::new(
                name("plant.counter"),
                3,
                token.clone(),
                clock,
                Rng::from_seed(1),
            );
            let kind_name = ctx.name().clone();
            let n = *ctx.config();
            token.cancel();
            (Counter.run(ctx).await, kind_name, n)
        });
        assert!(out.is_ok());
        assert_eq!((kind_name, n), (name("plant.counter"), 3));
    }

    #[test]
    fn shows_the_class_and_the_source() {
        let device = Error::Device("no reply from unit 4".into());
        assert_eq!(
            device.to_string(),
            "the device is in a bad state: no reply from unit 4"
        );
        let config = Error::Config(vec![diagnostic(RANGE, "n is over 8")]);
        assert_eq!(
            config.to_string(),
            "the config cannot work: n is over 8. Fix it"
        );
    }
}
