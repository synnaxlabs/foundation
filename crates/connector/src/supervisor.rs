//! Runs connectors and restarts them after errors.

use std::cell::{Cell, RefCell};
use std::future::poll_fn;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Poll, Waker};

use document::Document;
use env::clock::Clock;
use env::entropy::Entropy;
use env::net::Net;
use env::tasks::{Driver, Task, Tasks};
use types::hash::Map;
use types::name::Name;
use types::time::Span;

use crate::kind::{self, Context, Error};
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
}

/// Runs connectors of the kinds in a table, one `run` call at a time per connector.
/// It is not `Send`: each shard makes its own.
#[derive(Debug)]
pub struct Supervisor {
    config: Rc<Config>,
    /// The tasks of each connector whose `run` future dropped before they ended.
    left: RefCell<Map<Name, Rc<Live>>>,
}

impl Supervisor {
    /// Makes a supervisor for one shard.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config: Rc::new(config),
            left: RefCell::default(),
        }
    }

    /// Runs one connector: parses its config, starts `run`, and restarts it with
    /// backoff after any error but `Config`. The waits start again from the first
    /// after a run that lasted at least a minute. Never starts a run before the last
    /// one returned or dropped and each task it spawned through [`Context::tasks`]
    /// ended, or after `cancel` is cancelled.
    ///
    /// Returns `Ok` when `run` returns `Ok`, or when `cancel` is cancelled and the
    /// run returned, in each case once the run's tasks ended. A drop of the future
    /// cancels the run, and the next `run` of `name` on this supervisor waits for its
    /// tasks. The future is not `Send`: call it on a shard.
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
        let Config {
            kinds,
            clock,
            entropy,
            tasks,
            ..
        } = &*self.config;
        let left = self.left.borrow_mut().remove(&name).unwrap_or_default();
        let mut held = Left {
            name: name.clone(),
            live: left,
            map: &self.left,
        };
        held.live.ended().await;
        let mut backoff = retry::Backoff::new(clock, entropy.rng(), RESTART);
        while !cancel.cancelled() {
            let token = Ended(cancel.child());
            let live = Rc::new(Live::default());
            held.live = Rc::clone(&live);
            let count = Count {
                tasks: tasks.clone(),
                live: Rc::clone(&live),
            };
            let ctx = Context::new(
                name.clone(),
                (),
                token.0.clone(),
                Tasks::new(count),
                Rc::clone(&self.config),
            );
            let start = clock.now();
            let end = kinds.run(kind, config, ctx).map_err(Error::Config)?.await;
            let lasted = clock.now() - start;
            drop(token);
            // This wait reaches the connector's status as `state` 3 in #1731.
            live.ended().await;
            match end {
                Ok(()) => return Ok(()),
                Err(error @ Error::Config(_)) => return Err(error),
                // These reach the connector's status in #420.
                Err(Error::Device(_) | Error::Retry(_)) => {}
            }
            if lasted >= HEALTHY {
                backoff.reset();
            }
            backoff.wait(cancel).await;
        }
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

/// Keeps the tasks of a connector's last run for its next `run` when the future of
/// `run` drops before they ended.
struct Left<'a> {
    name: Name,
    live: Rc<Live>,
    map: &'a RefCell<Map<Name, Rc<Live>>>,
}

impl Drop for Left<'_> {
    fn drop(&mut self) {
        if self.live.n.get() > 0 {
            let live = Rc::clone(&self.live);
            self.map.borrow_mut().insert(self.name.clone(), live);
        }
    }
}

/// Spawns a run's tasks on the shard and counts those that have not ended.
struct Count {
    tasks: Tasks,
    live: Rc<Live>,
}

impl Driver for Count {
    fn spawn(&self, task: Task) {
        self.live.n.set(self.live.n.get() + 1);
        let held = Held(Rc::clone(&self.live));
        self.tasks.spawn(async move {
            let _held = held;
            task.await;
        });
    }
}

/// How many tasks of one run have not ended, and who waits for none.
#[derive(Debug, Default)]
struct Live {
    n: Cell<usize>,
    waiter: RefCell<Option<Waker>>,
}

impl Live {
    /// Returns when no task of the run is left.
    async fn ended(&self) {
        poll_fn(|cx| {
            if self.n.get() == 0 {
                return Poll::Ready(());
            }
            self.waiter.replace(Some(cx.waker().clone()));
            Poll::Pending
        })
        .await;
    }
}

/// Counts one task until the task ends or the shard drops it.
struct Held(Rc<Live>);

impl Drop for Held {
    fn drop(&mut self) {
        let n = self.0.n.get() - 1;
        self.0.n.set(n);
        if n == 0
            && let Some(waker) = self.0.waiter.take()
        {
            waker.wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::IoSlice;
    use std::net::SocketAddr;
    use std::pin::pin;
    use std::sync::Mutex;

    use env::net::tcp;

    use document::diagnostic::{Code, Diagnostic};
    use document::value::{self, Value};
    use document::{Attribute, Map};
    use types::time::Monotonic;

    use super::*;
    use crate::cancel::Token;
    use crate::common::{inputs, run_on};
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
        /// Returns a retry error at once.
        Retry,
        /// Cancels its own token, then returns a device error.
        Abort,
        /// Waits for the cancel, then the span, then returns a device error.
        Linger(Span),
        /// Spawns a task that ends the span after the cancel, then returns a device
        /// error at once.
        Hold(Span),
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
                    let (token, clock) = (ctx.cancel().clone(), clock.clone());
                    ctx.tasks().spawn(async move {
                        token.wait().await;
                        clock.sleep(span).await;
                    });
                    Err(Error::Device("left a task".into()))
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
            let supervisor = Supervisor::new(inputs(&node, tasks.clone(), kinds));
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
        unsent_bytes_max: 1 << 10,
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
                let supervisor = Supervisor::new(inputs(&node, tasks.clone(), kinds));
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
            let supervisor = Supervisor::new(inputs(&node, tasks.clone(), kinds));
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
    fn stops_the_tasks_of_a_run_when_its_future_drops() {
        let live = run_on(|node, tasks| async move {
            let kind = Spawner::default();
            let live = Arc::clone(&kind.live);
            let kinds = Table::new().with("spawner", kind);
            let supervisor = Supervisor::new(inputs(&node, tasks, kinds));
            let token = Token::new();
            let name = "plant.spawner".parse().expect("a valid name");
            let config = config();
            let mut run = Box::pin(supervisor.run("spawner", name, &config, &token));
            let clock = node.clock();
            let mut later = pin!(clock.sleep(ms(5_000)));
            poll_fn(|cx| {
                assert!(run.as_mut().poll(cx).is_pending(), "it runs until dropped");
                later.as_mut().poll(cx)
            })
            .await;
            let before = *live.lock().expect("no panic");
            drop(run);
            clock.sleep(ms(1)).await;
            (before, *live.lock().expect("no panic"))
        });
        assert_eq!(live, (1, 0), "the second run's task outlives the drop");
    }

    #[test]
    fn waits_for_the_tasks_of_a_dropped_run_before_a_new_run_of_its_connector() {
        let (seen, kept) = run_on(|node, tasks| async move {
            let kind = Spawner {
                linger: ms(2_000),
                ..Spawner::default()
            };
            let seen = Arc::clone(&kind.seen);
            let kinds = Table::new().with("spawner", kind);
            let supervisor = Supervisor::new(inputs(&node, tasks.clone(), kinds));
            let config = config();
            let clock = node.clock();
            let token = Token::new();
            let name: Name = "plant.spawner".parse().expect("a valid name");
            let mut run =
                Box::pin(supervisor.run("spawner", name.clone(), &config, &token));
            let mut later = pin!(clock.sleep(ms(5_000)));
            poll_fn(|cx| {
                assert!(run.as_mut().poll(cx).is_pending(), "it runs until dropped");
                later.as_mut().poll(cx)
            })
            .await;
            drop(run);
            let again = Token::new();
            let (canceller, sleeper) = (again.clone(), clock.clone());
            tasks.spawn(async move {
                sleeper.sleep(ms(5_000)).await;
                canceller.cancel();
            });
            supervisor
                .run("spawner", name, &config, &again)
                .await
                .expect("ok after the cancel");
            let kept = supervisor.left.borrow().len();
            (seen.lock().expect("no panic").clone(), kept)
        });
        assert_eq!(seen, [0, 0, 0], "the dropped run's task still runs");
        assert_eq!(kept, 0, "a run whose tasks ended is kept");
    }
}
