//! The cost of one poll of a kind's run under `Supervisor::run`, on a sim node. Run with
//! `cargo bench -p connector --bench run`.
//!
//! The kind's run wakes itself and is pending once between its steps, so each step is
//! one poll of the supervisor's task. Each round times `STEPS` steps:
//!
//! - `poll`: a step with no work.
//!
//! The base of the comparison for #2173, which adds two lines with status counts.
//!
//! Each figure is ns per step, with the count of the allocations of the steps. Judge
//! `poll` by its p50.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{self, Poll};
use std::time::Instant;

use connector::cancel::Token;
use connector::kind::{Channels, Context, Error, Kind, Table};
use connector::supervisor::Supervisor;
use connector::testing;
use document::Document;
use document::diagnostic::Diagnostic;
use types::name::Name;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const STEPS: u64 = 10_000;
const WARMUP: usize = 10;
const ROUNDS: usize = 100;

fn main() {
    let lines = [Step::Poll].map(run);
    print(&lines);
}

/// What a step of the kind does.
#[derive(Clone, Copy)]
enum Step {
    Poll,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Self::Poll => "poll",
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
    async fn run(&self, _: Context<()>) -> Result<(), Error> {
        for round in 0..WARMUP + ROUNDS {
            let (start, mut allocations) = (Instant::now(), 0);
            for i in 0..STEPS {
                let set = || {
                    if !matches!(self.step, Step::Poll) {
                        std::hint::black_box(i);
                    }
                };
                allocations += ALLOCATOR.count(set).1;
                Yield(false).await;
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
    let result = sim.run_on(&node, |node, tasks| async move {
        let env = hub::testing::Env {
            files: node.files(),
            clock: node.clock(),
            wall: node.wall(),
            entropy: node.entropy(),
            tasks,
        };
        let kinds = Table::new().with("steps", kind);
        let (config, _) = testing::create_config(env, node.net(), kinds).await;
        let connector: Name = "plant.steps".parse().expect("a valid name");
        let (token, document) = (Token::new(), Document::default());
        Supervisor::new(config)
            .run("steps", connector, &document, &token)
            .await
    });
    result
        .expect("the run ends")
        .expect("the connector ends ok");
    Arc::into_inner(line)
        .expect("the kind dropped")
        .into_inner()
        .expect("no panic under the lock")
}

/// Prints p10, p50, and p90 of the ns per step of each line, and its allocations.
#[expect(clippy::print_stdout, reason = "a benchmark prints its results")]
fn print(lines: &[Line]) {
    println!("ns per step over {ROUNDS} rounds of {STEPS} steps");
    println!(
        "{:<12} {:>7} {:>7} {:>7} {:>7}",
        "line", "p10", "p50", "p90", "allocs"
    );
    for line in lines {
        let mut nanos = line.nanos.clone();
        nanos.sort_unstable();
        let at = |percent: usize| nanos[nanos.len() * percent / 100];
        println!(
            "{:<12} {:>7} {:>7} {:>7} {:>7}",
            line.name,
            at(10),
            at(50),
            at(90),
            line.allocations
        );
    }
}
