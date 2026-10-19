//! The cost of one poll of a kind's run under `Opened::run`, on a sim node. Run
//! with `cargo bench -p connector --bench run`.
//!
//! The kind's run wakes itself and is pending once between its steps, so each step is
//! one poll of the supervisor's task. Each round times `STEPS` steps:
//!
//! - `poll`: a step with no work.
//! - `poll + set`: a step that sets a status count.
//! - `set`: a set of a status count, with no poll.
//! - `add`: an add to a status count, with no poll.
//! - `poll + set (closed)`: as `poll + set`, after a status channel was removed, so
//!   the status writer writes no more.
//! - `clear`: a clear of the shown error, with no poll, after a clear.
//! - `show`: a show of the same error, with no poll, after it was shown.
//! - `note`: a note of the same text, with no poll, after it was noted.
//!
//! Each figure is ns per step to the ps, with the count of the allocations of the
//! steps. Judge `poll` by its p50. A line panics when its last status state is not
//! `stopped`, or `running` for the closed line, whose writer wrote no state after the
//! removal.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{self, Poll};
use std::time::Instant;

use connector::cancel::Token;
use connector::kind::{Channels, Context, Error, Kind, Table};
use connector::testing::Rig;
use document::Document;
use document::diagnostic::Diagnostic;
use hub::reader::Mode;
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

fn main() {
    let lines = [
        Step::Poll,
        Step::PollSet,
        Step::Set,
        Step::Add,
        Step::Closed,
        Step::Clear,
        Step::Show,
        Step::Note,
    ]
    .map(run);
    print(&lines);
}

/// What a step of the kind does.
#[derive(Clone, Copy)]
enum Step {
    Poll,
    PollSet,
    Set,
    Add,
    Closed,
    Clear,
    Show,
    Note,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Self::Poll => "poll",
            Self::PollSet => "poll + set",
            Self::Set => "set",
            Self::Add => "add",
            Self::Closed => "poll + set (closed)",
            Self::Clear => "clear",
            Self::Show => "show",
            Self::Note => "note",
        }
    }
}

/// The ps per step of each timed round, and the allocations of all of them.
struct Line {
    name: &'static str,
    picos: Vec<u64>,
    allocations: u64,
}

/// A kind whose run times `ROUNDS` rounds of `STEPS` steps.
struct Steps {
    step: Step,
    line: Arc<Mutex<Line>>,
}

impl Kind for Steps {
    type Config = ();
    const SCHEMA: document::schema::Body = connector::testing::READS;

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels {
            counts: vec![samples()],
            ..Channels::default()
        })
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
                let set = || match self.step {
                    Step::Poll => {}
                    Step::Clear => ctx.clear_error(),
                    Step::Show => ctx.error(&"refused"),
                    Step::Note => ctx.note(&"steady"),
                    Step::Add => count.add(std::hint::black_box(1)),
                    _ => count.set(std::hint::black_box(i)),
                };
                allocations += ALLOCATOR.count(set).1;
                if !matches!(
                    self.step,
                    Step::Set | Step::Add | Step::Clear | Step::Show | Step::Note
                ) {
                    Yield(false).await;
                }
            }
            let nanos = Instant::now().duration_since(start).as_nanos();
            if round >= WARMUP {
                let mut line = self.line.lock().expect("no panic under the lock");
                let picos =
                    u64::try_from(nanos * 1000).expect("a round takes under 2^64 ps");
                line.picos.push(picos / STEPS);
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
        picos: Vec::new(),
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
        let rig = Rc::new(Rig::open(env, node.net(), kinds).await);
        let connector: Name = "plant.steps".parse().expect("a valid name");
        if matches!(step, Step::Closed) {
            let (rig, clock) = (Rc::clone(&rig), node.clock());
            let removed = format!("{connector}.status.samples");
            let removed: Name = removed.parse().expect("a valid name");
            tasks.spawn(async move {
                clock.sleep(REMOVED).await;
                rig.remove(&[removed]);
            });
        }
        let (hub, state) = (rig.hub().clone(), format!("{connector}.status.state"));
        let (token, document) = (Token::new(), Document::default());
        let opened = rig
            .open_connector("steps", connector, &document, &token)
            .await;
        let mut reader = open_state(&hub, &state).await;
        let result = opened.run().await;
        node.clock().sleep(Span::SECOND).await;
        let last = last_state(&mut reader, &state).await;
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

/// Opens a latest reader of the status channel `state`. It spawns no task, so the
/// timed rounds poll only the supervisor.
async fn open_state(hub: &hub::Hub, state: &str) -> hub::reader::Reader {
    let open = hub::reader::Config {
        select: Selector::new([state]).expect("a selector"),
        mode: Mode::Latest,
        subject: "bench".parse().expect("a valid name"),
        name: None,
        hold: Span::ZERO,
    };
    hub.reader(open).await.expect("the reader opens")
}

/// The last sample of the status channel `state` in the newest status frame that
/// `reader` holds.
async fn last_state(reader: &mut hub::reader::Reader, state: &str) -> Option<u8> {
    let received = reader.next().await.expect("a status frame");
    let entries = received.set().entries();
    let at = (0..entries.len()).position(|entry| received.name(entry) == state);
    let at = at.expect("the set holds `state`");
    let range = received.view().range(entries[at].group);
    let count = range.expect("the group is present").count;
    let count = usize::try_from(count).expect("a count");
    let (_, bytes) = received
        .view()
        .iter()
        .find(|&(present, _)| present == at)
        .expect("the view holds `state`");
    let mut samples = Vec::new();
    codec::decode(entries[at].data_type, count, bytes, &mut samples).expect("decodes");
    samples.last().copied()
}

/// Prints p10, p50, and p90 of the ns per step of each line, and its allocations.
#[expect(clippy::print_stdout, reason = "a benchmark prints its results")]
fn print(lines: &[Line]) {
    println!("ns per step over {ROUNDS} rounds of {STEPS} steps");
    println!(
        "{:<20} {:>9} {:>9} {:>9} {:>7}",
        "line", "p10", "p50", "p90", "allocs"
    );
    for line in lines {
        let mut picos = line.picos.clone();
        picos.sort_unstable();
        let at = |percent: usize| {
            let p = picos[picos.len() * percent / 100];
            format!("{}.{:03}", p / 1000, p % 1000)
        };
        println!(
            "{:<20} {:>9} {:>9} {:>9} {:>7}",
            line.name,
            at(10),
            at(50),
            at(90),
            line.allocations
        );
    }
}
