//! Runs connectors and restarts them after errors.

use std::cell::Cell;
use std::future::poll_fn;
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Poll, Waker};

use document::Document;
use env::clock::Clock;
use env::entropy::Entropy;
use env::net::Net;
use env::tasks::{Driver, Task, Tasks};
use types::name::Name;
use types::time::Span;

use crate::kind::{self, Context, Error};
use crate::status;
use crate::{cancel, retry};

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
    /// The shard's tasks.
    pub tasks: Tasks,
    /// The shard's hub. Each session a kind opens acts as its connector.
    pub hub: hub::Hub,
}

/// Runs connectors of the kinds in a table, one `run` call at a time per connector.
/// It is not `Send`: each shard makes its own.
#[derive(Debug)]
pub struct Supervisor(Rc<Config>);

impl Supervisor {
    /// Makes a supervisor for one shard.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self(Rc::new(config))
    }

    /// Runs one connector: parses its config, starts `run`, and restarts it with
    /// backoff after any error but `Config`. The waits start again from the first
    /// after a run that lasted at least a minute. Within one call, never starts a run
    /// before the last one returned and each task it spawned through [`Context::tasks`]
    /// ended, or after `cancel` is cancelled. Writes the connector's status channels
    /// as the connector: the whole status at each start and change of state, and a
    /// change of counts alone at most once each second. Each change of state, a start
    /// too, waits until the home applied the state before it or `cancel` is cancelled.
    ///
    /// The status channels open once the node has mesh time, and no run starts before.
    /// Returns `Ok` when `run` returns `Ok`, when the mesh stopped before the status
    /// channels opened, or when `cancel` is cancelled: at once while the channels are
    /// not open, and otherwise once the run returned. It returns, with `Ok` or an
    /// error, only once each task of its last run ended, and the home applied its last
    /// status frame or `cancel` is cancelled. Neither wait holds once the home refused
    /// a status frame because the status channels were removed or its disk failed:
    /// after it, the call writes no more status. A drop of the future cancels the run
    /// and does not wait for its tasks: to wait, cancel `cancel` and await the future.
    /// The future is not `Send`: call it on a shard.
    ///
    /// # Errors
    ///
    /// [`Error::Config`], without a restart, when the kind is unknown, the config
    /// does not parse, or `run` returns it.
    ///
    /// # Panics
    ///
    /// When the status channels of `name` do not open for a reason other than a
    /// stopped mesh: `node` did not define them, or homed them on another node. When
    /// a status name of `name` is longer than [`Name::MAX_BYTES`], which the plan
    /// refuses. When the home refuses a status frame for a cause that only a defect of
    /// `connector` gives. Or when a status frame is larger than the largest block of
    /// the shard's pool.
    pub async fn run(
        &self,
        kind: &str,
        name: Name,
        config: &Document,
        cancel: &cancel::Token,
    ) -> Result<(), Error> {
        let Config {
            kinds, clock, hub, ..
        } = &*self.0;
        let counts = kinds
            .check(kind, None, config)
            .map_err(Error::Config)?
            .counts;
        let mut open = pin!(status::Writer::open(
            hub,
            &name,
            counts,
            clock.clone(),
            cancel.clone()
        ));
        let mut cancelled = pin!(cancel.wait());
        // An open that is ready at once wins, so a cancel before the call still
        // writes `state` 2.
        let opened = poll_fn(|cx| match open.as_mut().poll(cx) {
            Poll::Ready(opened) => Poll::Ready(Some(opened)),
            Poll::Pending => cancelled.as_mut().poll(cx).map(|()| None),
        });
        let (writer, status) = match opened.await {
            None | Some(Err(hub::writer::Error::Mesh(_))) => return Ok(()),
            Some(Ok(opened)) => opened,
            Some(Err(error)) => {
                panic!(
                    "the status channels of the connector {name} do not open: {error}"
                )
            }
        };
        let runs = self.runs(kind, &name, config, cancel, &writer, &status);
        writer.during(runs).await
    }

    /// Runs the connector and restarts it, as [`Self::run`] says, and writes its
    /// status through `writer` at each change of state.
    async fn runs(
        &self,
        kind: &str,
        name: &Name,
        config: &Document,
        cancel: &cancel::Token,
        writer: &status::Writer,
        status: &status::Status,
    ) -> Result<(), Error> {
        let Config {
            kinds,
            clock,
            entropy,
            tasks,
            ..
        } = &*self.0;
        let mut backoff = retry::Backoff::new(clock, entropy.rng(), RESTART);
        while writer.start().await {
            let token = Ended(cancel.child());
            let live = Rc::new(Live::default());
            let count = Count {
                tasks: tasks.clone(),
                live: Rc::clone(&live),
            };
            let ctx = Context::new(
                name.clone(),
                (),
                token.0.clone(),
                Tasks::new(count),
                status.share(),
                Rc::clone(&self.0),
            );
            let start = clock.now();
            let end = kinds.run(kind, config, ctx).map_err(Error::Config)?.await;
            let lasted = clock.now() - start;
            drop(token);
            writer.end(&end).await;
            live.ended().await;
            match end {
                Ok(()) | Err(Error::Config(_)) => {
                    writer.stop().await;
                    return end;
                }
                // The status gives their class. #420 adds their text.
                Err(Error::Device(_) | Error::Retry(_)) => {}
            }
            if cancel.cancelled() {
                break;
            }
            writer.wait().await;
            if lasted >= HEALTHY {
                backoff.reset();
            }
            backoff.wait(cancel).await;
        }
        writer.stop().await;
        Ok(())
    }
}

/// A run's token, cancelled when the run returned or its future dropped.
struct Ended(cancel::Token);

impl Drop for Ended {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Spawns a run's tasks on the shard and counts those that have not ended.
struct Count {
    tasks: Tasks,
    live: Rc<Live>,
}

impl Driver for Count {
    fn spawn(&self, task: Task) {
        self.live.n.set(self.live.n.get().strict_add(1));
        let held = Held(Rc::clone(&self.live));
        self.tasks.spawn(async move {
            let _held = held;
            task.await;
        });
    }
}

/// How many tasks of one run have not ended, and who waits for none.
#[derive(Default)]
struct Live {
    n: Cell<usize>,
    waiter: Cell<Option<Waker>>,
}

impl Live {
    /// Returns when no task of the run is left.
    async fn ended(&self) {
        poll_fn(|cx| {
            if self.n.get() == 0 {
                return Poll::Ready(());
            }
            self.waiter.set(Some(cx.waker().clone()));
            Poll::Pending
        })
        .await;
    }
}

/// Counts one task until the task ends or the shard drops it.
struct Held(Rc<Live>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.n.set(self.0.n.get().strict_sub(1));
        if let Some(waker) = self.0.waiter.take() {
            waker.wake();
        }
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io::IoSlice;
    use std::net::SocketAddr;
    use std::num::NonZeroUsize;
    use std::pin::pin;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use env::net::tcp;

    use document::diagnostic::{Code, Diagnostic};
    use document::value::{self, Value};
    use document::{Attribute, Map};
    use types::time::Monotonic;

    use super::*;
    use crate::cancel::Token;
    use crate::common::{STATUS, create_config, create_status, env, run_on};
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
    use types::sample::{Scalar, Type};

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
        /// Returns a retry error at once.
        Retry,
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
                Some(Step::Retry) => Err(Error::Retry("busy".into())),
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
                    hold(&ctx, span);
                    Err(Error::Device("left a task".into()))
                }
                Some(Step::Refuse(span)) => {
                    hold(&ctx, span);
                    Err(Error::Config(vec![bad()]))
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

    /// Spawns a task of the run that ends `span` after the run's cancel.
    fn hold(ctx: &Context<()>, span: Span) {
        let (token, clock) = (ctx.cancel().clone(), ctx.clock().clone());
        ctx.tasks().spawn(async move {
            token.wait().await;
            clock.sleep(span).await;
        });
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

    /// The outcome of [`supervise`]: the result, when it returned, each run's start
    /// and end, all from the start, and each status frame.
    struct Outcome {
        result: Result<(), Error>,
        returned: Span,
        runs: Vec<(Span, Option<Span>)>,
        statuses: Vec<Written>,
    }

    /// One status frame: its stamp from the first, and the sample of each status
    /// channel but the index, in the order of [`status::channels`].
    type Written = (Span, Vec<i64>);

    /// Reads the status frames of `connector` with `counts`, whose status channels
    /// have keys from [`STATUS`] on, into the vector it gives, in a task on `tasks`.
    async fn read_status(
        hub: &hub::Hub,
        connector: &str,
        counts: &[Name],
        tasks: &env::tasks::Tasks,
    ) -> Rc<RefCell<Vec<Written>>> {
        let connector = connector.parse().expect("a valid name");
        let names = status::channels(&connector, counts).expect("names");
        let (_, channels) = names;
        let names: Vec<_> = channels.into_iter().map(|(name, _)| name).collect();
        let keys = 1..=u128::try_from(names.len()).expect("a few channels");
        let reader = hub.reader(&names, Mode::Complete).await;
        let mut reader = reader.expect("the reader opens");
        let statuses = Rc::new(RefCell::new(Vec::new()));
        let into = Rc::clone(&statuses);
        tasks.spawn(async move {
            let mut first = None;
            while let Ok(received) = reader.next().await {
                let stamps = series(&received, STATUS.as_u128());
                let samples: Vec<_> = keys
                    .clone()
                    .map(|key| series(&received, STATUS.as_u128() + key))
                    .collect();
                let mut into = into.borrow_mut();
                for (i, stamp) in stamps.into_iter().enumerate() {
                    let first = *first.get_or_insert(stamp);
                    let at = Span::from_nanos(stamp - first);
                    into.push((at, samples.iter().map(|series| series[i]).collect()));
                }
            }
        });
        statuses
    }

    /// Supervises one connector of [`Script`] with `steps` and `config`, and
    /// cancels it after `cancel`, if given. A zero `cancel` cancels before the call.
    fn supervise(
        kind: &'static str,
        steps: Vec<Step>,
        config: Document,
        cancel: Option<Span>,
    ) -> Outcome {
        run_on(move |node, tasks| async move {
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
            if cancel == Some(Span::ZERO) {
                token.cancel();
            } else if let Some(after) = cancel {
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
    fn runs_once_and_returns_when_run_returns_ok() {
        let out = supervise("script", vec![Step::Done], config(), None);
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
        statuses.iter().map(|(_, samples)| state(samples)).collect()
    }

    #[test]
    fn writes_the_status_of_each_start_and_end_after_a_device_error() {
        let steps = vec![Step::Device(ms(10))];
        let out = supervise("script", steps, config(), Some(ms(5_000)));
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
        let at = out.statuses.iter().map(|(at, _)| *at);
        let got: Vec<_> = at.zip(states(&out.statuses)).collect();
        assert_eq!(got, want);
        out.result.expect("ok after a cancel");
    }

    #[test]
    fn writes_the_class_of_a_retry_error() {
        let out = supervise("script", vec![Step::Retry, Step::Done], config(), None);
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

    #[test]
    fn writes_stopped_after_a_config_error_from_run() {
        let out = supervise("script", vec![Step::Config], config(), None);
        assert_eq!(states(&out.statuses), [(0, 0, 0), (3, 1, 0), (2, 1, 0)]);
    }

    #[test]
    fn writes_stopped_after_a_run_returns_ok() {
        let out = supervise("script", vec![Step::Done], config(), None);
        assert_eq!(states(&out.statuses), [(0, 0, 0), (3, 0, 0), (2, 0, 0)]);
    }

    #[test]
    fn writes_only_stopped_when_cancelled_before_the_call() {
        let out = supervise("script", vec![Step::Done], config(), Some(Span::ZERO));
        assert_eq!(states(&out.statuses), [(2, 0, 0)]);
    }

    #[test]
    fn writes_stopped_when_cancelled_during_the_backoff() {
        let steps = vec![Step::Device(Span::ZERO)];
        let out = supervise("script", steps, config(), Some(Span::from_nanos(1)));
        let want = [(0, 0, 0), (3, 2, 0), (1, 2, 0), (2, 2, 0)];
        assert_eq!(states(&out.statuses), want);
    }

    #[test]
    fn writes_no_waiting_after_a_cancel() {
        let steps = vec![Step::Linger(ms(30))];
        let out = supervise("script", steps, config(), Some(ms(50)));
        let want = [(0, 0, 0), (3, 2, 0), (2, 2, 0)];
        assert_eq!(states(&out.statuses), want);
    }

    #[test]
    fn writes_ending_until_the_tasks_of_a_run_end() {
        let steps = vec![Step::Hold(ms(2_000)), Step::Done];
        let out = supervise("script", steps, config(), None);
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
            let holder = hub::writer::Config {
                subject: name("plant.other"),
                authority: Authority::ABSOLUTE,
                lease: None,
                channels: vec![name("plant.script.status.state")],
            };
            let holder = inputs.hub.writer(holder).await.expect("opens");
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
            let result = supervisor.run("script", name, &config(), &token).await;
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

    /// The status frames of one connector of [`Script`] that returns `Ok` at once,
    /// whose frames another writer refuses until `held`, and when the call returned.
    fn refused_until(held: Span, cancel: Option<Span>) -> (Vec<Written>, Span) {
        run_on(move |node, tasks| async move {
            let script = Script {
                steps: Mutex::new([Step::Done].into()),
                ..Script::default()
            };
            let kinds = Table::new().with("script", script);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.script").await;
            let statuses = read_status(&inputs.hub, "plant.script", &[], &tasks).await;
            let holder = hub::writer::Config {
                subject: name("plant.other"),
                authority: Authority::ABSOLUTE,
                lease: None,
                channels: vec![name("plant.script.status.state")],
            };
            let holder = inputs.hub.writer(holder).await.expect("opens");
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
            let result = supervisor.run("script", name, &config(), &token).await;
            result.expect("ok");
            let returned = clock.now() - start;
            clock.sleep(ms(5_000)).await;
            let statuses = statuses.borrow().clone();
            (statuses, returned)
        })
    }

    #[test]
    fn writes_a_refused_last_frame_again_before_the_call_returns() {
        let (statuses, returned) = refused_until(ms(500), None);
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
        let (statuses, returned) = refused_until(ms(1_500), None);
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
            let channels = ["state", "class", "restarts"];
            let other = hub::writer::Config {
                subject: name("plant.other"),
                authority: Authority::ABSOLUTE,
                lease: None,
                channels: channels
                    .map(|c| name(&format!("plant.script.status.{c}")))
                    .into(),
            };
            let mut other = inputs.hub.writer(other).await.expect("opens");
            let entries = other.set().entries();
            let series: Vec<_> = (entries.iter().enumerate())
                .map(|(i, entry)| (i, entry.data_type.width().expect("one width")))
                .collect();
            let mut draft = other.draft(Form::Raw, &series).expect("a frame");
            let stamp = other.now().nanos() + Span::SECOND.nanos();
            for (i, entry) in entries.iter().enumerate() {
                let bytes = draft.series_mut(i).expect("a series");
                let value = if entry.key == STATUS { stamp } else { 9 };
                let len = bytes.len();
                bytes.copy_from_slice(&value.to_le_bytes()[..len]);
            }
            draft.set_count(entries[0].group, 1);
            let outcomes = other.write(Label::Path(Path::Live), draft);
            let applied = matches!(outcomes, Ok([hub::home::Outcome::Applied { .. }]));
            assert!(applied, "the frame 1 s ahead applies: {outcomes:?}");
            drop(other);
            let (clock, token) = (node.clock(), Token::new());
            let (supervisor, start) = (Supervisor::new(inputs), clock.now());
            let result = supervisor
                .run("script", name("plant.script"), &config(), &token)
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
            (Span::ZERO, vec![9, 9, 9]),
            (Span::from_nanos(2), vec![0, 0, 0]),
            (Span::from_nanos(3), vec![3, 0, 0]),
            (Span::from_nanos(4), vec![2, 0, 0]),
        ];
        assert_eq!(statuses, want);
        assert_eq!(returned, Span::SECOND);
    }

    #[test]
    fn returns_at_a_cancel_while_the_last_frame_waits() {
        let (statuses, returned) = refused_until(ms(2_500), Some(ms(500)));
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
            let hog = hub::writer::Config {
                subject: name("plant.script"),
                authority: Authority(1),
                lease: None,
                channels: vec![name("plant.script.status.state")],
            };
            let hog = inputs.hub.writer(hog).await.expect("opens");
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
                .run("script", name, &config(), &Token::new())
                .await;
            assert_eq!(config_errors(result), [bad()]);
            let returned = clock.now() - start;
            clock.sleep(ms(5_000)).await;
            let statuses = statuses.borrow().clone();
            (statuses, returned)
        });
        let last = statuses.last().map(|(at, samples)| (*at, samples[0]));
        assert_eq!(last, Some((ms(3_000), 2)), "{statuses:?}");
        assert_eq!(returned, ms(3_000), "returns once the home applied state 2");
    }

    #[test]
    fn returns_at_once_when_the_pool_fills_after_a_removed_status_channel() {
        let returned = run_on(|node, tasks| async move {
            let kind = Counted(|ctx: Context<()>| async move {
                ctx.count("samples").set(1);
                ctx.clock().sleep(ms(1_500)).await;
                Ok(())
            });
            let kinds = Table::new().with("tally", kind);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.tally").await;
            let (connector, counts) = (name("plant.tally"), [name("samples")]);
            let status = testing::create_status(&connector, &counts, STATUS);
            inputs
                .hub
                .set_definitions(status.iter().map(|(name, def)| (name, def)));
            let (hub, clock) = (inputs.hub.clone(), node.clock());
            tasks.spawn(async move {
                clock.sleep(ms(100)).await;
                let hog = rival(&hub, &["state", "class", "restarts", "samples"]).await;
                clock.sleep(ms(400)).await;
                let samples = name("plant.tally.status.samples");
                let kept = status.iter().filter(|(name, _)| *name != samples);
                hub.set_definitions(kept.map(|(name, def)| (name, def)));
                clock.sleep(ms(700)).await;
                let held = fill(&hog);
                clock.sleep(ms(20_000)).await;
                drop(held);
            });
            let (clock, start) = (node.clock(), node.clock().now());
            let result = Supervisor::new(inputs)
                .run("tally", connector, &config(), &Token::new())
                .await;
            result.expect("the run returns ok");
            clock.now() - start
        });
        assert_eq!(returned, ms(1_500), "no change of state waits after 1 s");
    }

    #[test]
    fn returns_at_once_when_the_pool_fills_after_a_failed_disk() {
        let returned = run_on(|node, tasks| async move {
            let kind = Counted(|ctx: Context<()>| async move {
                ctx.count("samples").set(1);
                ctx.clock().sleep(ms(1_100)).await;
                ctx.count("samples").set(2);
                ctx.clock().sleep(ms(1_400)).await;
                Ok(())
            });
            let kinds = Table::new().with("tally", kind);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.tally").await;
            let (connector, counts) = (name("plant.tally"), [name("samples")]);
            let status = testing::create_status(&connector, &counts, STATUS);
            inputs
                .hub
                .set_definitions(status.iter().map(|(name, def)| (name, def)));
            let (hub, clock, failer) = (inputs.hub.clone(), node.clock(), node.clone());
            tasks.spawn(async move {
                clock.sleep(ms(100)).await;
                let hog = rival(&hub, &["state", "class", "restarts", "samples"]).await;
                clock.sleep(ms(400)).await;
                failer.fail_file(RING.as_ref(), env::files::Operation::Sync);
                clock.sleep(ms(1_700)).await;
                let held = fill(&hog);
                clock.sleep(ms(20_000)).await;
                drop(held);
            });
            let (clock, start) = (node.clock(), node.clock().now());
            let result = Supervisor::new(inputs)
                .run("tally", connector, &config(), &Token::new())
                .await;
            result.expect("the run returns ok");
            clock.now() - start
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
                .run("wide", connector, &config(), &Token::new())
                .await;
            result.expect("the run returns ok");
        });
    }

    #[test]
    fn wakes_nothing_at_a_count_set_after_its_status_channels_are_removed() {
        let polls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&polls);
        run_on(|node, tasks| async move {
            let kind = Counted(move |ctx: Context<()>| {
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
            });
            let kinds = Table::new().with("tally", kind);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.tally").await;
            let (connector, counts) = (name("plant.tally"), [name("samples")]);
            let status = testing::create_status(&connector, &counts, STATUS);
            inputs
                .hub
                .set_definitions(status.iter().map(|(name, def)| (name, def)));
            let (hub, clock) = (inputs.hub.clone(), node.clock());
            tasks.spawn(async move {
                clock.sleep(ms(500)).await;
                let samples = name("plant.tally.status.samples");
                let kept = status.iter().filter(|(name, _)| *name != samples);
                hub.set_definitions(kept.map(|(name, def)| (name, def)));
            });
            let result = Supervisor::new(inputs)
                .run("tally", connector, &config(), &Token::new())
                .await;
            result.expect("the run returns ok");
        });
        let polls = polls.load(Ordering::Relaxed);
        assert_eq!(polls, 2, "a poll to start the sleep, and one at its end");
    }

    #[test]
    fn wakes_nothing_at_a_count_set_after_a_change_of_state_closed_the_writer() {
        let polls = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let (counted, started) = (Arc::clone(&polls), Arc::clone(&runs));
        run_on(|node, tasks| async move {
            let kind = Counted(move |ctx: Context<()>| {
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
            });
            let kinds = Table::new().with("tally", kind);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.tally").await;
            let (connector, counts) = (name("plant.tally"), [name("samples")]);
            let status = testing::create_status(&connector, &counts, STATUS);
            inputs
                .hub
                .set_definitions(status.iter().map(|(name, def)| (name, def)));
            let (hub, clock) = (inputs.hub.clone(), node.clock());
            tasks.spawn(async move {
                clock.sleep(ms(1_500)).await;
                let samples = name("plant.tally.status.samples");
                let kept = status.iter().filter(|(name, _)| *name != samples);
                hub.set_definitions(kept.map(|(name, def)| (name, def)));
            });
            let result = Supervisor::new(inputs)
                .run("tally", connector, &config(), &Token::new())
                .await;
            result.expect("the run returns ok");
        });
        assert_eq!(runs.load(Ordering::Relaxed), 2, "one restart");
        let polls = polls.load(Ordering::Relaxed);
        assert_eq!(polls, 2, "a poll to start the sleep, and one at its end");
    }

    #[test]
    fn waits_for_a_full_pool_after_its_status_channels_are_removed() {
        let returned = run_on(|node, tasks| async move {
            let kind = Counted(|ctx: Context<()>| async move {
                ctx.clock().sleep(ms(1_500)).await;
                Ok(())
            });
            let kinds = Table::new().with("tally", kind);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.tally").await;
            let (connector, counts) = (name("plant.tally"), [name("samples")]);
            let status = testing::create_status(&connector, &counts, STATUS);
            inputs
                .hub
                .set_definitions(status.iter().map(|(name, def)| (name, def)));
            let (hub, clock) = (inputs.hub.clone(), node.clock());
            tasks.spawn(async move {
                let hog = rival(&hub, &["state"]).await;
                clock.sleep(ms(500)).await;
                let held = fill(&hog);
                let samples = name("plant.tally.status.samples");
                let kept = status.iter().filter(|(name, _)| *name != samples);
                hub.set_definitions(kept.map(|(name, def)| (name, def)));
                clock.sleep(ms(20_000)).await;
                drop(held);
            });
            let (clock, start) = (node.clock(), node.clock().now());
            let result = Supervisor::new(inputs)
                .run("tally", connector, &config(), &Token::new())
                .await;
            result.expect("the run returns ok");
            clock.now() - start
        });
        assert_eq!(returned, ms(21_500), "state 3 waits until the pool frees");
    }

    #[test]
    fn waits_for_a_full_pool_after_its_disk_failed() {
        let returned = run_on(|node, tasks| async move {
            let kind = Counted(|ctx: Context<()>| async move {
                ctx.count("samples").set(1);
                ctx.clock().sleep(ms(1_500)).await;
                Ok(())
            });
            let kinds = Table::new().with("tally", kind);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.tally").await;
            let (connector, counts) = (name("plant.tally"), [name("samples")]);
            let status = testing::create_status(&connector, &counts, STATUS);
            inputs
                .hub
                .set_definitions(status.iter().map(|(name, def)| (name, def)));
            let (hub, clock, failer) = (inputs.hub.clone(), node.clock(), node.clone());
            tasks.spawn(async move {
                clock.sleep(ms(100)).await;
                let mut hog = rival(&hub, &["state"]).await;
                clock.sleep(ms(400)).await;
                failer.fail_file(RING.as_ref(), env::files::Operation::Sync);
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
                clock.sleep(ms(19_800)).await;
                drop(held);
            });
            let (clock, start) = (node.clock(), node.clock().now());
            let result = Supervisor::new(inputs)
                .run("tally", connector, &config(), &Token::new())
                .await;
            result.expect("the run returns ok");
            clock.now() - start
        });
        assert_eq!(returned, ms(21_500), "state 3 waits until the pool frees");
    }

    #[test]
    fn writes_no_status_for_a_config_that_does_not_parse() {
        let out = supervise("modbus", vec![Step::Done], config(), None);
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
    fn restarts_after_a_retry_error() {
        let out = supervise("script", vec![Step::Retry, Step::Done], config(), None);
        out.result.expect("ok");
        assert_eq!(out.runs.len(), 2);
    }

    #[test]
    fn restarts_after_a_run_cancels_its_own_token() {
        let out = supervise("script", vec![Step::Abort, Step::Done], config(), None);
        out.result.expect("ok");
        assert_eq!(out.runs.len(), 2, "the run's token is not the caller's");
    }

    #[test]
    fn grows_the_waits_after_short_runs() {
        let mut steps = vec![Step::Device(Span::ZERO); 12];
        steps.push(Step::Done);
        let out = supervise("script", steps, config(), None);
        out.result.expect("ok");
        let last = out.runs.last().expect("13 runs").0;
        // With no growth, twelve waits of at most 1 s each.
        assert!(last > ms(12_000), "{last:?}");
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
    fn starts_no_run_when_cancelled_before_the_call() {
        let out = supervise("script", vec![Step::Done], config(), Some(Span::ZERO));
        out.result.expect("ok after a cancel");
        assert!(out.runs.is_empty(), "a run started after the cancel");
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

    #[test]
    fn counts_no_wait_for_the_tasks_toward_a_long_run() {
        let mut steps = vec![Step::Device(Span::ZERO); 8];
        steps.push(Step::Hold(ms(61_000)));
        steps.extend([Step::Device(Span::ZERO); 3]);
        steps.push(Step::Done);
        let out = supervise("script", steps, config(), None);
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

        fn discover(
            &self,
            _: &cancel::Token,
        ) -> impl Future<Output = Result<Vec<Document>, Error>> {
            std::future::ready(Ok(Vec::new()))
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
                supervisor.run("dial", name, &config(), &token).await
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

        fn discover(
            &self,
            _: &cancel::Token,
        ) -> impl Future<Output = Result<Vec<Document>, Error>> {
            std::future::ready(Ok(Vec::new()))
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
                .run("spawner", name, &config(), &token)
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
        let out = supervise("script", vec![Step::Refuse(ms(2_000))], config(), None);
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
            let run = supervisor.run("spawner", name, &config, &token);
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
            let run = supervisor.run("spawner", name.clone(), &config, &token);
            drop_at_5_s(run, &clock, || ()).await;
            let again = Token::new();
            let (canceller, sleeper) = (again.clone(), clock.clone());
            tasks.spawn(async move {
                sleeper.sleep(ms(1)).await;
                canceller.cancel();
            });
            supervisor
                .run("spawner", name, &config, &again)
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

        fn discover(
            &self,
            _: &cancel::Token,
        ) -> impl Future<Output = Result<Vec<Document>, Error>> {
            std::future::ready(Ok(Vec::new()))
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

    /// A kind with the count `samples`, whose run is the function it holds.
    struct Counted<F>(F);

    impl<F, R> Kind for Counted<F>
    where
        F: Fn(Context<()>) -> R + Send + Sync + 'static,
        R: Future<Output = Result<(), Error>>,
    {
        type Config = ();

        fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
            Ok(())
        }

        fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
            Tally::check(&Tally::new("samples"), &())
        }

        fn discover(
            &self,
            _: &cancel::Token,
        ) -> impl Future<Output = Result<Vec<Document>, Error>> {
            std::future::ready(Ok(Vec::new()))
        }

        fn run(&self, ctx: Context<()>) -> impl Future<Output = Result<(), Error>> {
            (self.0)(ctx)
        }
    }

    /// A kind with the count `samples`. Its run sets it to 1 at 500 ms and returns
    /// `Ok` at 600 ms. A task of the run sets it to `value`, if given, at 800 ms and
    /// ends at 2 s.
    fn late(value: Option<u64>) -> impl Kind<Config = ()> {
        Counted(move |ctx: Context<()>| async move {
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
            .map(|(at, samples)| (*at, samples[0], samples[3]))
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
            .map(|(at, samples)| (*at, samples[0], samples[3]))
            .collect();
        let want = [(Span::ZERO, 0, 0), (ms(600), 3, 1), (ms(2_000), 2, 1)];
        assert_eq!(got, want);
    }

    /// A kind with the count `samples`. Its run spawns a task that sets the count to
    /// each of 1 to `n`, `gap` apart, and returns `Ok` once that task ended.
    fn relay(n: u64, gap: Span) -> impl Kind<Config = ()> {
        Counted(move |ctx: Context<()>| async move {
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
    fn gives_13_frames_for_10_000_samples_over_10_s_set_from_a_task() {
        let statuses = tally(relay(10_000, ms(1)));
        assert_eq!(statuses.len(), 13, "{statuses:?}");
        assert_eq!(statuses.last().map(|(_, samples)| samples[3]), Some(10_000));
        assert_eq!(states(&statuses)[11..], [(3, 0, 0), (2, 0, 0)]);
    }

    /// A kind with the count `samples`. Its run takes frames of the shape of a
    /// status frame from the shard's pool until it has no room, sets the count to 7,
    /// gives the frames back at 1.5 s, and returns `Ok` at 3 s. When `full`, it takes
    /// the room that came back again at 900 ms, so the pool is full at the flush at
    /// 1 s. Else the home has no room to take that flush's frame, and loses it.
    fn hog(full: bool) -> impl Kind<Config = ()> {
        Counted(move |ctx: Context<()>| async move {
            let channels = ["state", "class", "restarts", "samples"]
                .map(|c| name(&format!("plant.tally.status.{c}")))
                .into();
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

    /// Takes frames of one sample for each channel of `writer` from the shard's pool
    /// until the pool has no room.
    fn fill(writer: &hub::writer::Writer) -> Vec<frame::Draft> {
        let series: Vec<_> = (writer.set().entries().iter().enumerate())
            .map(|(i, entry)| (i, entry.data_type.width().expect("one width")))
            .collect();
        let mut held = Vec::new();
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
        held
    }

    /// The ring file of the test shard, which `fail_file` fails.
    const RING: &str = "shard-0/ring";

    /// A writer of the subject `plant.other` with authority 1 on the status channels
    /// of `plant.tally` that `suffixes` name.
    async fn rival(hub: &hub::Hub, suffixes: &[&str]) -> hub::writer::Writer {
        let channels = (suffixes.iter())
            .map(|suffix| name(&format!("plant.tally.status.{suffix}")))
            .collect();
        let config = hub::writer::Config {
            subject: name("plant.other"),
            authority: Authority(1),
            lease: None,
            channels,
        };
        hub.writer(config).await.expect("opens")
    }

    /// The time, `state`, and count of each status frame of `hog(full)`.
    fn hogged(full: bool) -> Vec<(Span, i64, i64)> {
        let statuses = tally(hog(full));
        statuses
            .iter()
            .map(|(at, samples)| (*at, samples[0], samples[3]))
            .collect()
    }

    /// A kind with the count `samples`. Its run spawns a task that takes frames from
    /// the shard's pool until it has no room and gives them back at 500 ms, and
    /// returns `Ok` at 200 ms.
    fn outlive() -> impl Kind<Config = ()> {
        Counted(|ctx: Context<()>| async move {
            let channels = ["state", "class", "restarts", "samples"]
                .map(|c| name(&format!("plant.tally.status.{c}")))
                .into();
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
        let (statuses, returned) = tally_until(outlive(), Some(ms(700)));
        let states: Vec<_> = statuses.iter().map(|(at, s)| (*at, s[0])).collect();
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
        let states: Vec<_> = statuses.iter().map(|(at, s)| (*at, s[0])).collect();
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
                let channels = ["state", "class", "restarts"]
                    .map(|c| name(&format!("plant.script.status.{c}")))
                    .into();
                let hog = hub::writer::Config {
                    subject: name("plant.other"),
                    authority: Authority(1),
                    lease: None,
                    channels,
                };
                let hog = hub.writer(hog).await.expect("opens");
                let held = fill(&hog);
                sleeper.sleep(ms(2_000)).await;
                canceller.cancel();
                sleeper.sleep(ms(5_000)).await;
                drop(held);
            });
            let (supervisor, start) = (Supervisor::new(inputs), clock.now());
            let name = name("plant.script");
            let result = supervisor.run("script", name, &config(), &token).await;
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
        tally_until(kind, None).0
    }

    /// The status frames of one connector of `kind`, as [`tally`] gives, whose call
    /// is cancelled at `cancel`, and when the call returned.
    fn tally_until(
        kind: impl Kind + 'static,
        cancel: Option<Span>,
    ) -> (Vec<Written>, Span) {
        run_on(move |node, tasks| async move {
            let kinds = Table::new().with("tally", kind);
            let inputs =
                create_config(&node, tasks.clone(), kinds, "plant.tally").await;
            let (connector, counts) = (name("plant.tally"), [name("samples")]);
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
                .run("tally", connector, &config(), &token)
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
        let got = statuses.iter().map(|(at, samples)| (*at, samples[3]));
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
                .run("script", name.clone(), &config(), &token)
                .await;
            first.expect("the first call returns ok");
            let (canceller, sleeper) = (token.clone(), clock.clone());
            tasks.spawn(async move {
                sleeper.sleep(ms(5_000)).await;
                canceller.cancel();
            });
            let second = supervisor.run("script", name, &config(), &token).await;
            second.expect("the second call returns ok");
            clock.sleep(Span::SECOND).await;
            statuses.borrow().clone()
        });
        let got: Vec<_> = statuses.iter().map(|(at, s)| (*at, s[0])).collect();
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
        let got = statuses.iter().map(|(at, samples)| (*at, samples[3]));
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
                .run("tally", connector, &config(), &Token::new())
                .await;
            result.expect("the run returns ok");
            node.clock().sleep(Span::SECOND).await;
            statuses.borrow().clone()
        });
        let at: Vec<_> = statuses.iter().map(|(at, _)| *at).collect();
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
                .run("tally", connector, &config(), &Token::new())
                .await;
            result.expect("the run returns ok");
            let returned = clock.now() - start;
            clock.sleep(Span::SECOND).await;
            let statuses = statuses.borrow().clone();
            (statuses, returned)
        });
        let at: Vec<_> = statuses.iter().map(|(at, _)| *at).collect();
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
            unsynced(Span::from_nanos(30_000_000_000), Span::SECOND);
        assert_eq!(returned, Span::SECOND, "the call returns at the cancel");
        assert_eq!(runs, []);
    }

    #[test]
    fn starts_the_first_run_once_the_node_has_mesh_time() {
        let two = Span::from_nanos(2_000_000_000);
        let (returned, runs, statuses) = unsynced(two, Span::from_nanos(3_000_000_000));
        assert_eq!(runs, [two]);
        assert_eq!(returned, Span::from_nanos(3_000_000_000));
        assert_eq!(states(&statuses), [(0, 0, 0), (3, 0, 0), (2, 0, 0)]);
    }

    /// Runs a `Script` connector with no step on a node whose mesh time starts
    /// `delay` after the call, and cancels it at `cancel`. Checks that the call returns
    /// `Ok`, and gives when it returned and when each run started, and the status
    /// frames.
    fn unsynced(delay: Span, cancel: Span) -> (Span, Vec<Span>, Vec<Written>) {
        run_on(move |node, tasks| async move {
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
                tasks: tasks.clone(),
                hub,
            };
            let token = Token::new();
            let (canceller, sleeper) = (token.clone(), clock.clone());
            tasks.spawn(async move {
                sleeper.sleep(cancel).await;
                canceller.cancel();
            });
            let start = clock.now();
            let name = name("plant.script");
            let supervisor = Supervisor::new(inputs);
            let result = supervisor.run("script", name, &config(), &token).await;
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
                    .run("script", name, &config(), &Token::new())
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
                    .run("script", name, &config(), &Token::new())
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

        fn discover(
            &self,
            _: &cancel::Token,
        ) -> impl Future<Output = Result<Vec<Document>, Error>> {
            std::future::ready(Ok(Vec::new()))
        }

        async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
            let channels = vec![name("plant.value")];
            let mut writer = ctx
                .writer(channels, self.authority, self.lease)
                .await
                .expect("the writer opens");
            let entries = writer.set().entries();
            let entry = |key| {
                let key = channel::Key::from_u128(key);
                entries.iter().position(|entry| entry.key == key)
            };
            let (time, value) = (entry(1).expect("time"), entry(2).expect("value"));
            let group = entries[time].group;
            let mut last = None;
            for (i, sample) in self.values.iter().enumerate() {
                if i > 0 {
                    ctx.clock().sleep(self.gap).await;
                }
                let now = writer.now().nanos();
                let stamp = last.map_or(now, |last: i64| now.max(last + 1));
                last = Some(stamp);
                let mut series = [(time, 8), (value, 8)];
                series.sort_unstable();
                let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
                let time = draft.series_mut(time).expect("the index series");
                time.copy_from_slice(&stamp.to_le_bytes());
                let value = draft.series_mut(value).expect("the value series");
                value.copy_from_slice(&sample.to_le_bytes());
                draft.set_count(group, 1);
                let outcomes = writer
                    .write(Label::Path(Path::Live), draft)
                    .expect("the home takes it");
                let refusal = match outcomes {
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

    /// Defines `plant.time` (key 1), `plant.value` (key 2, `i64`), and the status
    /// channels of `plant.write` on `hub`.
    fn define(hub: &hub::Hub) {
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
            key: channel::Key::from_u128(2),
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

    /// The samples of the channel of `key` in `received`, each widened to `i64`.
    fn series(received: &Received<'_>, key: u128) -> Vec<i64> {
        let set = received.set;
        let key = channel::Key::from_u128(key);
        let entry = set.entries().iter().position(|entry| entry.key == key);
        let entry = entry.expect("the set holds the channel");
        let range = received.view.range(set.entries()[entry].group);
        let count = range.expect("the group is present").count;
        let count = usize::try_from(count).expect("a count");
        let (_, bytes) = received
            .view
            .iter()
            .find(|&(present, _)| present == entry)
            .expect("the view holds the series");
        let data_type = set.entries()[entry].data_type;
        let width = data_type.width().expect("a fixed width");
        let mut out = vec![0; count * width];
        codec::decode(data_type, count, bytes, &mut out).expect("decodes");
        out.chunks(width)
            .map(|chunk| {
                let mut sample = [0; 8];
                sample[..width].copy_from_slice(chunk);
                i64::from_le_bytes(sample)
            })
            .collect()
    }

    #[test]
    fn gives_a_kind_a_writer_whose_samples_a_hub_reader_gets_in_order() {
        let got = run_on(|node, tasks| async move {
            let mut inputs =
                create_config(&node, tasks, Table::new(), "plant.write").await;
            define(&inputs.hub);
            let write = Write::new(vec![30, 10, 20]);
            let refusals = Arc::clone(&write.refusals);
            inputs.kinds = Arc::new(Table::new().with("write", write));
            let mut reader = inputs
                .hub
                .reader(&[name("plant.value")], Mode::Complete)
                .await
                .expect("the reader opens");
            let supervisor = Supervisor::new(inputs);
            let result = supervisor
                .run("write", name("plant.write"), &config(), &Token::new())
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

    /// Runs `write` as `plant.write` on a new shard, while a writer as
    /// `plant.other` at `holder` holds `plant.value` when it is some. Gives the
    /// refusal of each write, or `None` for one the home applied.
    fn refusals(holder: Option<Authority>, write: Write) -> Vec<Option<Refusal>> {
        run_on(move |node, tasks| async move {
            let mut inputs =
                create_config(&node, tasks, Table::new(), "plant.write").await;
            define(&inputs.hub);
            let refusals = Arc::clone(&write.refusals);
            inputs.kinds = Arc::new(Table::new().with("write", write));
            let mut other = None;
            if let Some(authority) = holder {
                let config = hub::writer::Config {
                    subject: name("plant.other"),
                    authority,
                    lease: None,
                    channels: vec![name("plant.value")],
                };
                other = Some(inputs.hub.writer(config).await.expect("opens"));
            }
            let supervisor = Supervisor::new(inputs);
            let result = supervisor
                .run("write", name("plant.write"), &config(), &Token::new())
                .await;
            result.expect("the run returns ok");
            drop(other);
            refusals.lock().expect("no panic").clone()
        })
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
}
