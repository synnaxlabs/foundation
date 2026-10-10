//! A run that ends with a device error of 1 MiB, or with one whose text comes in two
//! pieces of 1023 and 1 bytes, and the wait after it, hold no more than 2.25 KiB over
//! the same run in a twin sim, where it ends with an error of 1 byte: room for the cut
//! text twice, plus 256 bytes. The twins add their larger steps of memory at the same
//! runs, so the steps cancel. This binary has no test harness: the count covers each
//! thread, and a harness allocates on its own thread at any time.

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
use types::channel;
use types::name::Name;
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

const MIB: usize = 1 << 20;

/// The runs before the one that the test measures, so that the bytes a run adds
/// settle.
const SHORT: usize = 3;

/// The most bytes held while a count of runs had started, by that count.
type Peaks = [AtomicUsize; SHORT + 3];

/// How run [`SHORT`] ends.
#[derive(Clone, Copy, Debug)]
enum Last {
    Short,
    Large,
    Pieces,
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
/// ends as `last` gives, and whose last run ends `Ok`.
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

    fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), Error>> {
        let run = self.runs.fetch_add(1, Relaxed);
        self.peaks[run].fetch_max(ALLOCATOR.held(), Relaxed);
        let end = match (run, self.last) {
            (..SHORT, _) | (SHORT, Last::Short) => Err(Error::Device("a".into())),
            (SHORT, Last::Large) => Err(Error::Device("a".repeat(MIB).into())),
            (SHORT, Last::Pieces) => {
                Err(Error::Device(Box::new(Pieces("a".repeat(1_023)))))
            }
            _ => Ok(()),
        };
        std::future::ready(end)
    }
}

/// The most bytes held over run [`SHORT`] and the wait after it, less those held
/// before the sim, when that run ends as `last` gives.
fn held(last: Last) -> usize {
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
    let result = sim.run_on(&node, |node, tasks| async move {
        let clock = node.clock();
        let env = hub::testing::Env {
            files: node.files(),
            clock: node.clock(),
            wall: node.wall(),
            entropy: node.entropy(),
            tasks,
        };
        let kinds = Table::new().with("large", kind);
        let config = testing::create_config(env, node.net(), kinds).await;
        let connector: Name = "plant.large".parse().expect("a valid name");
        let status =
            testing::create_status(&connector, &[], channel::Key::from_u128(100));
        config
            .hub
            .set_definitions(status.iter().map(|(name, def)| (name, def)));
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
    held(Last::Short);
    let short = held(Last::Short);
    for last in [Last::Large, Last::Pieces] {
        let held = held(last);
        assert!(
            held <= short + 2 * 1_024 + 256,
            "run {SHORT} and its wait hold {short} bytes after a short error, {held} \
             after {last:?}"
        );
    }
}
