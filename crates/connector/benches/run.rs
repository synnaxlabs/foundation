//! The cost of one poll of a kind's run under `Supervisor::run`, on a sim node. Run
//! with `cargo bench -p connector --bench run`.
//!
//! The kind's run wakes itself and is pending once between its steps, so each step is
//! one poll of the supervisor's task. Each round times `STEPS` steps:
//!
//! - `poll`: a step with no work.
//! - `poll + set`: a step that sets a status count.
//! - `set`: a set of a status count, with no poll.
//! - `poll + set (closed)`: as `poll + set`, after a status channel was removed, so
//!   the status writer writes no more.
//!
//! Each figure is ns per step, with the count of the allocations of the steps. Judge
//! `poll` by its p50. A line panics when its last status state is not `stopped`, or
//! `running` for the closed line, whose writer wrote no state after the removal.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{self, Poll};
use std::time::Instant;

use connector::cancel::Token;
use connector::kind::{Channels, Context, Error, Kind, Table};
use connector::supervisor::Supervisor;
use connector::testing;
use document::Document;
use document::diagnostic::Diagnostic;
use env::tasks::Tasks;
use hub::reader::Mode;
use types::channel;
use types::name::{Name, Selector};
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const STEPS: u64 = 10_000;
const WARMUP: usize = 10;
const ROUNDS: usize = 100;
/// When the status channel `samples` is removed.
const REMOVED: Span = Span::from_nanos(500_000_000);
/// When a `Closed` kind starts its rounds, after the writer saw the removal.
const CLOSED: Span = Span::from_nanos(3_000_000_000);
/// The first key of the status channels.
const STATUS: channel::Key = channel::Key::from_u128(100);
/// The key of the status channel `state`.
const STATE: channel::Key = channel::Key::from_u128(101);

fn main() {
    let lines = [Step::Poll, Step::PollSet, Step::Set, Step::Closed].map(run);
    print(&lines);
}

/// What a step of the kind does.
#[derive(Clone, Copy)]
enum Step {
    Poll,
    PollSet,
    Set,
    Closed,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Self::Poll => "poll",
            Self::PollSet => "poll + set",
            Self::Set => "set",
            Self::Closed => "poll + set (closed)",
        }
    }
}

/// The ns per step of each timed round, and the allocations of all of them.
struct Line {
    name: &'static str,
    nanos: Vec<u64>,
    allocations: u64,
}

/// A kind whose run times `ROUNDS` rounds of `STEPS` steps.
struct Steps {
    step: Step,
    line: Arc<Mutex<Line>>,
}

impl Kind for Steps {
    type Config = ();

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels {
            counts: vec![samples()],
            ..Channels::default()
        })
    }

    fn discover(
        &self,
        _: &Token,
    ) -> impl Future<Output = Result<Vec<Document>, Error>> {
        std::future::ready(Ok(Vec::new()))
    }

    #[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
    async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
        let count = ctx.count("samples");
        if matches!(self.step, Step::Closed) {
            count.set(1);
            ctx.clock().sleep(CLOSED).await;
        }
        for round in 0..WARMUP + ROUNDS {
            let (start, mut allocations) = (Instant::now(), 0);
            for i in 0..STEPS {
                let set = || {
                    if !matches!(self.step, Step::Poll) {
                        count.set(std::hint::black_box(i));
                    }
                };
                allocations += ALLOCATOR.count(set).1;
                if !matches!(self.step, Step::Set) {
                    Yield(false).await;
                }
            }
            let nanos = Instant::now().duration_since(start).as_nanos();
            if round >= WARMUP {
                let mut line = self.line.lock().expect("no panic under the lock");
                let nanos = u64::try_from(nanos).expect("a round takes under 2^64 ns");
                line.nanos.push(nanos / STEPS);
                line.allocations += allocations;
            }
        }
        Ok(())
    }
}

/// Pending once, after a wake of its own task.
struct Yield(bool);

impl Future for Yield {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

fn samples() -> Name {
    "samples".parse().expect("a valid name")
}

/// Runs one supervisor of a kind that does `step` on a sim node.
fn run(step: Step) -> Line {
    let line = Arc::new(Mutex::new(Line {
        name: step.name(),
        nanos: Vec::new(),
        allocations: 0,
    }));
    let kind = Steps {
        step,
        line: Arc::clone(&line),
    };
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let result = sim.run_on(&node, move |node, tasks| async move {
        let env = hub::testing::Env {
            files: node.files(),
            clock: node.clock(),
            wall: node.wall(),
            entropy: node.entropy(),
            tasks: tasks.clone(),
        };
        let kinds = Table::new().with("steps", kind);
        let config = testing::create_config(env, node.net(), kinds).await;
        let connector: Name = "plant.steps".parse().expect("a valid name");
        let status = testing::create_status(&connector, &[samples()], STATUS);
        config
            .hub
            .set_definitions(status.iter().map(|(name, def)| (name, def)));
        if matches!(step, Step::Closed) {
            let (hub, clock) = (config.hub.clone(), node.clock());
            let removed = format!("{connector}.status.samples");
            let removed: Name = removed.parse().expect("a valid name");
            tasks.spawn(async move {
                clock.sleep(REMOVED).await;
                let kept = status.iter().filter(|(name, _)| *name != removed);
                hub.set_definitions(kept.map(|(name, def)| (name, def)));
            });
        }
        let written = read_states(&config.hub, &connector, &tasks).await;
        let (token, document) = (Token::new(), Document::default());
        let result = Supervisor::new(config)
            .run("steps", connector, &document, &token)
            .await;
        node.clock().sleep(Span::SECOND).await;
        let last = written.borrow().last().copied();
        (result, last)
    });
    let (result, last) = result.expect("the run ends");
    result.expect("the connector ends ok");
    let (running, stopped) = (0_u8, 2);
    let state = if matches!(step, Step::Closed) {
        running
    } else {
        stopped
    };
    assert_eq!(
        last,
        Some(state),
        "the last status state of `{}`",
        step.name()
    );
    Arc::into_inner(line)
        .expect("the kind dropped")
        .into_inner()
        .expect("no panic under the lock")
}

/// Reads the `state` sample of each status frame of `connector` into the vector it
/// gives, in a task on `tasks`.
async fn read_states(
    hub: &hub::Hub,
    connector: &Name,
    tasks: &Tasks,
) -> Rc<RefCell<Vec<u8>>> {
    let state = format!("{connector}.status.state");
    let open = hub::reader::Config {
        select: Selector::new([state.as_str()]).expect("a selector"),
        mode: Mode::Complete,
        subject: "bench".parse().expect("a valid name"),
        name: None,
        hold: Span::ZERO,
    };
    let mut reader = hub.reader(open).await.expect("the reader opens");
    let states = Rc::new(RefCell::new(Vec::new()));
    let into = Rc::clone(&states);
    tasks.spawn(async move {
        while let Ok(received) = reader.next().await {
            let entries = received.set.entries();
            let at = entries.iter().position(|entry| entry.key == STATE);
            let at = at.expect("the set holds `state`");
            let range = received.view.range(entries[at].group);
            let count = range.expect("the group is present").count;
            let count = usize::try_from(count).expect("a count");
            let (_, bytes) = (received.view.iter())
                .find(|&(present, _)| present == at)
                .expect("the view holds `state`");
            let mut samples = vec![0; count];
            codec::decode(entries[at].data_type, count, bytes, &mut samples)
                .expect("decodes");
            into.borrow_mut().extend(samples);
        }
    });
    states
}

/// Prints p10, p50, and p90 of the ns per step of each line, and its allocations.
#[expect(clippy::print_stdout, reason = "a benchmark prints its results")]
fn print(lines: &[Line]) {
    println!("ns per step over {ROUNDS} rounds of {STEPS} steps");
    println!(
        "{:<20} {:>7} {:>7} {:>7} {:>7}",
        "line", "p10", "p50", "p90", "allocs"
    );
    for line in lines {
        let mut nanos = line.nanos.clone();
        nanos.sort_unstable();
        let at = |percent: usize| nanos[nanos.len() * percent / 100];
        println!(
            "{:<20} {:>7} {:>7} {:>7} {:>7}",
            line.name,
            at(10),
            at(50),
            at(90),
            line.allocations
        );
    }
}
