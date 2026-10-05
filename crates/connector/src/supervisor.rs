//! Runs connectors and restarts them after errors.

use std::sync::Arc;

use document::Document;
use env::clock::Clock;
use env::entropy::Entropy;
use types::name::Name;
use types::time::Span;

use crate::kind::{self, Context, Error};
use crate::{cancel, retry};

/// The waits between restarts.
const RESTART: retry::Config = retry::Config {
    first: Span::SECOND,
    cap: Span::MINUTE,
};

/// Runs connectors of the kinds in a table, one `run` call at a time per connector.
#[derive(Debug)]
pub struct Supervisor {
    kinds: Arc<kind::Table>,
    clock: Clock,
    entropy: Entropy,
}

impl Supervisor {
    /// Makes a supervisor for the kinds in `kinds`.
    #[must_use]
    pub fn new(kinds: Arc<kind::Table>, clock: Clock, entropy: Entropy) -> Self {
        Self {
            kinds,
            clock,
            entropy,
        }
    }

    /// Runs one connector: parses its config, starts `run`, and restarts it with
    /// backoff after any error but `Config`. The waits start again from the first
    /// after a run that lasted at least a minute. Never starts a run before the last
    /// one returned.
    ///
    /// Returns `Ok` when `run` returns `Ok`, or when `cancel` is cancelled and the
    /// run returned. The future is not `Send`: call it on a shard.
    ///
    /// # Errors
    ///
    /// [`Error::Config`], without a restart, when the kind is unknown, the config
    /// does not parse, or `run` returns it.
    pub async fn run(
        &self,
        kind: &str,
        name: Name,
        config: &Document,
        cancel: &cancel::Token,
    ) -> Result<(), Error> {
        let mut backoff = retry::Backoff::new(&self.clock, self.entropy.rng(), RESTART);
        loop {
            let ctx = Context::new(
                name.clone(),
                (),
                cancel.clone(),
                self.clock.clone(),
                self.entropy.clone(),
            );
            let start = self.clock.now();
            match self
                .kinds
                .run(kind, config, ctx)
                .map_err(Error::Config)?
                .await
            {
                Ok(()) => return Ok(()),
                Err(error @ Error::Config(_)) => return Err(error),
                Err(Error::Device(_) | Error::Retry(_)) => {}
            }
            if self.clock.now() - start >= RESTART.cap {
                backoff.reset();
            }
            if !backoff.wait(cancel).await {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use document::diagnostic::{Code, Diagnostic};
    use document::value::{self, Value};
    use document::{Attribute, Map};
    use types::time::Monotonic;

    use super::*;
    use crate::cancel::Token;
    use crate::common::run;
    use crate::kind::{Channels, Kind, Table};

    const BAD: Code = Code::new("test.bad");

    /// What one run of [`Script`] does.
    #[derive(Clone, Copy, Debug)]
    enum Step {
        /// Returns `Ok` at once.
        Done,
        /// Returns a device error after the span.
        Device(Span),
        /// Returns a config error at once.
        Config,
        /// Waits for the cancel, then the span, then returns a device error.
        Linger(Span),
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
        type Config = ();

        fn parse(&self, config: &Document) -> Result<(), Vec<Diagnostic>> {
            match config.attributes.get("bad") {
                Some(_) => Err(vec![bad()]),
                None => Ok(()),
            }
        }

        fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
            Ok(Channels::default())
        }

        fn discover(
            &self,
            _: &cancel::Token,
        ) -> impl Future<Output = Result<Vec<Document>, Error>> {
            std::future::ready(Ok(Vec::new()))
        }

        async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
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
            let out = match step {
                Some(Step::Done) => Ok(()),
                Some(Step::Device(span)) => {
                    clock.sleep(span).await;
                    Err(Error::Device("no reply".into()))
                }
                Some(Step::Config) => Err(Error::Config(vec![bad()])),
                Some(Step::Linger(span)) => {
                    ctx.cancel().wait().await;
                    clock.sleep(span).await;
                    Err(Error::Device("stopped late".into()))
                }
                None => {
                    ctx.cancel().wait().await;
                    Ok(())
                }
            };
            let mut runs = self.runs.lock().expect("no panic under the lock");
            if let Some(run) = runs.get_mut(index) {
                run.1 = Some(clock.now());
            }
            out
        }
    }

    fn bad() -> Diagnostic {
        Diagnostic::new(BAD, None, "the config is bad".into(), "Fix it".into())
    }

    fn between(from: Span, to: Span) -> Span {
        Span::from_nanos(to.nanos() - from.nanos())
    }

    fn ms(n: i64) -> Span {
        Span::from_nanos(n * 1_000_000)
    }

    /// The outcome of [`supervise`]: the result, when it returned, and each run's
    /// start and end, all from the start.
    struct Outcome {
        result: Result<(), Error>,
        returned: Span,
        runs: Vec<(Span, Option<Span>)>,
    }

    /// Supervises one connector of [`Script`] with `steps` and `config`, and
    /// cancels it after `cancel`, if given.
    fn supervise(
        kind: &'static str,
        steps: Vec<Step>,
        config: Document,
        cancel: Option<Span>,
    ) -> Outcome {
        run(move |clock, tasks, entropy| async move {
            let script = Script {
                steps: Mutex::new(steps.into()),
                ..Script::default()
            };
            let runs = Arc::clone(&script.runs);
            let kinds = Arc::new(Table::new().with("script", script));
            let supervisor = Supervisor::new(kinds, clock.clone(), entropy);
            let token = Token::new();
            if let Some(after) = cancel {
                let canceller = token.clone();
                let sleeper = clock.clone();
                tasks.spawn(async move {
                    sleeper.sleep(after).await;
                    canceller.cancel();
                });
            }
            let start = clock.now();
            let name = "plant.script".parse().expect("a valid name");
            let result = supervisor.run(kind, name, &config, &token).await;
            let returned = clock.now() - start;
            let runs = runs.lock().expect("no panic under the lock");
            let runs = runs
                .iter()
                .map(|(from, to)| (*from - start, to.map(|to| to - start)))
                .collect();
            Outcome {
                result,
                returned,
                runs,
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
    fn runs_once_and_returns_when_run_returns_ok() {
        let out = supervise("script", vec![Step::Done], config(), None);
        out.result.expect("ok");
        assert_eq!(out.runs, [(Span::ZERO, Some(Span::ZERO))]);
    }

    #[test]
    fn restarts_after_a_device_error_within_the_backoff() {
        let steps = vec![
            Step::Device(ms(10)),
            Step::Device(ms(10)),
            Step::Device(ms(10)),
            Step::Done,
        ];
        let out = supervise("script", steps, config(), None);
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
    fn returns_a_config_error_from_run_without_a_restart() {
        let steps = vec![Step::Config, Step::Done];
        let out = supervise("script", steps, config(), None);
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
        let out = supervise("script", vec![Step::Done], config, None);
        assert_eq!(config_errors(out.result), [bad()]);
        assert!(out.runs.is_empty());
    }

    #[test]
    fn returns_an_unknown_kind_as_a_config_error() {
        let out = supervise("modbus", vec![Step::Done], config(), None);
        let codes: Vec<_> = config_errors(out.result).iter().map(|d| d.code).collect();
        assert_eq!(codes, [Code::new("connector.unknown-kind")]);
        assert!(out.runs.is_empty());
    }

    #[test]
    fn waits_for_the_run_to_return_after_a_cancel() {
        let steps = vec![Step::Linger(ms(30))];
        let out = supervise("script", steps, config(), Some(ms(50)));
        out.result.expect("ok after a cancel");
        assert_eq!(out.runs, [(Span::ZERO, Some(ms(80)))]);
        assert_eq!(out.returned, ms(80), "after the run returned, never before");
    }

    #[test]
    fn returns_at_once_when_cancelled_during_the_backoff() {
        let steps = vec![Step::Device(Span::ZERO)];
        let out = supervise("script", steps, config(), Some(Span::from_nanos(1)));
        out.result.expect("ok after a cancel");
        assert_eq!(out.runs, [(Span::ZERO, Some(Span::ZERO))]);
        assert_eq!(out.returned, Span::from_nanos(1), "at the cancel");
    }

    #[test]
    fn starts_the_backoff_again_after_a_long_run() {
        let mut steps = vec![Step::Device(Span::ZERO); 8];
        steps.push(Step::Device(Span::MINUTE));
        steps.extend([Step::Device(Span::ZERO); 3]);
        steps.push(Step::Done);
        let out = supervise("script", steps, config(), None);
        out.result.expect("ok");
        let after = out.runs.get(9..).expect("13 runs");
        let first = out.runs.get(8).and_then(|run| run.1).expect("run 8 ended");
        let last = after.last().expect("a last run").0;
        // Four waits from the first again: at most 1 + 2 + 4 + 8 s. Without the
        // reset, each would be up to a minute.
        let gap = between(first, last);
        assert!(gap <= ms(15_000), "{gap:?}");
    }
}
