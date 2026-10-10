//! At the start of the next run, and at each 10 ms of the sim clock from the start of
//! a run that ends with a device or retry error of 1 MiB, or with a device error whose
//! text comes in two pieces of 1023 and 1 bytes, the process holds less than the cut
//! text twice over the same run in a twin sim, where it ends with an error of 1 byte.
//! The run has a task that lives 2 s more. In a second set of twins, a rival writer
//! keeps the pool full around the run, so the home applies its frames late. The twins
//! add their larger steps of memory at the same runs, so the steps cancel. This binary
//! has no test harness: the count covers each thread, and a harness allocates on its
//! own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::fmt;
use std::future::poll_fn;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::Relaxed;

use connector::cancel::Token;
use connector::kind::{Channels, Context, Error, Kind, Table};
use connector::supervisor::Supervisor;
use connector::testing;
use document::Document;
use document::diagnostic::Diagnostic;
use types::authority::Authority;
use types::channel;
use types::frame::{self, Form};
use types::name::Name;
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

const MIB: usize = 1 << 20;

/// The runs before the one that the test measures, so that the bytes a run adds
/// settle.
const SHORT: usize = 3;

/// The most bytes held at each sample while a count of runs had started, by that
/// count.
type Peaks = [AtomicUsize; SHORT + 3];

/// How run [`SHORT`] ends.
#[derive(Clone, Copy, Debug)]
enum Last {
    Short,
    Large,
    Pieces,
    Retry,
}

/// Whether a rival writer keeps the pool full from just after run [`SHORT`] - 1
/// starts to 2 s after run [`SHORT`] starts.
#[derive(Clone, Copy, Debug)]
enum Pool {
    Free,
    Full,
}

/// An error whose text comes in two pieces, of 1023 bytes then 1, so that the string
/// it gives grows past its length.
#[derive(Debug)]
struct Pieces(String);

impl fmt::Display for Pieces {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)?;
        f.write_str("b")
    }
}

impl std::error::Error for Pieces {}

/// A kind whose first [`SHORT`] runs end with a device error of 1 byte, whose next run
/// ends as `last` gives with a task that lives 2 s more, and whose last run ends `Ok`.
struct Large {
    last: Last,
    runs: Arc<AtomicUsize>,
    peaks: Arc<Peaks>,
}

impl Kind for Large {
    type Config = ();

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    fn discover(
        &self,
        _: &Token,
    ) -> impl Future<Output = Result<Vec<Document>, Error>> {
        std::future::ready(Ok(Vec::new()))
    }

    fn run(&self, ctx: Context<()>) -> impl Future<Output = Result<(), Error>> {
        let run = self.runs.fetch_add(1, Relaxed);
        self.peaks[run].fetch_max(ALLOCATOR.held(), Relaxed);
        if run == SHORT {
            let clock = ctx.clock().clone();
            ctx.tasks().spawn(async move {
                clock.sleep(Span::from_nanos(2_000_000_000)).await;
            });
        }
        let end = match (run, self.last) {
            (..SHORT, _) | (SHORT, Last::Short) => Err(Error::Device("a".into())),
            (SHORT, Last::Large) => Err(Error::Device("a".repeat(MIB).into())),
            (SHORT, Last::Retry) => Err(Error::Retry("a".repeat(MIB).into())),
            (SHORT, Last::Pieces) => {
                Err(Error::Device(Box::new(Pieces("a".repeat(1_023)))))
            }
            _ => Ok(()),
        };
        std::future::ready(end)
    }
}

/// Drafts frames of `writer` until the pool has no room for a frame of any size up to
/// 2 KiB. `writer` has `error`, whose text sets the size.
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

/// Keeps the pool full as [`Pool::Full`] states, with a writer of lower authority on
/// the status channels of `plant.large`.
async fn hog(hub: hub::Hub, clock: env::clock::Clock, runs: Arc<AtomicUsize>) {
    let channels = ["state", "class", "restarts", "backoff", "error"]
        .map(|c| {
            format!("plant.large.status.{c}")
                .parse()
                .expect("a valid name")
        })
        .into();
    let config = hub::writer::Config {
        subject: "plant.other".parse().expect("a valid name"),
        authority: Authority(1),
        lease: None,
        channels,
    };
    let writer = hub.writer(config).await.expect("the writer opens");
    while runs.load(Relaxed) < SHORT {
        clock.sleep(Span::from_nanos(1_000)).await;
    }
    clock.sleep(Span::from_nanos(1_000)).await;
    // The home frees blocks as it applies, so one fill does not keep the pool full.
    let mut held = fill(&writer);
    while runs.load(Relaxed) < SHORT + 1 {
        clock.sleep(Span::from_nanos(100_000)).await;
        held.extend(fill(&writer));
    }
    clock.sleep(Span::from_nanos(2_000_000_000)).await;
    drop(held);
}

/// The most bytes held at the samples from the start of run [`SHORT`] to the start of
/// the next, less those held before the sim, when that run ends as `last` gives.
fn held(last: Last, pool: Pool) -> usize {
    let before = ALLOCATOR.held();
    let (runs, peaks) = (Arc::new(AtomicUsize::new(0)), Arc::new(Peaks::default()));
    let kind = Large {
        last,
        runs: Arc::clone(&runs),
        peaks: Arc::clone(&peaks),
    };
    let seen = Arc::clone(&peaks);
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let result = sim.run_on(&node, move |node, tasks| async move {
        let clock = node.clock();
        let env = hub::testing::Env {
            files: node.files(),
            clock: node.clock(),
            wall: node.wall(),
            entropy: node.entropy(),
            tasks: tasks.clone(),
        };
        let kinds = Table::new().with("large", kind);
        let config = testing::create_config(env, node.net(), kinds).await;
        let connector: Name = "plant.large".parse().expect("a valid name");
        let status =
            testing::create_status(&connector, &[], channel::Key::from_u128(100));
        config
            .hub
            .set_definitions(status.iter().map(|(name, def)| (name, def)));
        if let Pool::Full = pool {
            tasks.spawn(hog(config.hub.clone(), clock.clone(), Arc::clone(&runs)));
        }
        let supervisor = Supervisor::new(config);
        let (token, config) = (Token::new(), Document::default());
        let mut run = pin!(supervisor.run("large", connector, &config, &token));
        let mut sample = pin!(async {
            loop {
                clock.sleep(Span::from_nanos(10_000_000)).await;
                seen[runs.load(Relaxed)].fetch_max(ALLOCATOR.held(), Relaxed);
            }
        });
        poll_fn(|cx| {
            assert!(sample.as_mut().poll(cx).is_pending(), "the sample loops");
            run.as_mut().poll(cx)
        })
        .await
    });
    result
        .expect("the run ends")
        .expect("the connector ends ok");
    peaks[SHORT + 1].load(Relaxed) - before
}

fn main() {
    // The first sim makes the allocations that a process makes once.
    held(Last::Short, Pool::Free);
    for pool in [Pool::Free, Pool::Full] {
        let short = held(Last::Short, pool);
        for last in [Last::Large, Last::Pieces, Last::Retry] {
            let held = held(last, pool);
            assert!(
                held < short + 2 * 1_024,
                "from run {SHORT} to the next, the sim holds {short} bytes after a \
                 short error, {held} after {last:?}, with the pool {pool:?}"
            );
        }
    }
}
