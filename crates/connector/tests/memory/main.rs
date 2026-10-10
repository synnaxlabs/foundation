//! After a device error of 1 MiB, the run and the wait after it hold no more than 32
//! KiB over what they hold after a short one. A run adds about 16 KiB more every few
//! runs, so the test cannot see a smaller excess; the unit tests of `status` pin the
//! capacity of the text. This binary has no test harness: the count covers each
//! thread, and a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

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

/// The runs that end with a short error, so that the bytes a run adds settle.
const SHORT: usize = 3;

/// The most bytes held while a count of runs had started, by that count.
type Peaks = [AtomicUsize; SHORT + 3];

/// A kind whose first [`SHORT`] runs end with a device error of 1 byte, whose next run
/// ends with one of 1 MiB, and whose last run ends `Ok`.
struct Large {
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
        let end = match run {
            ..SHORT => Err(Error::Device("a".into())),
            SHORT => Err(Error::Device("a".repeat(MIB).into())),
            _ => Ok(()),
        };
        std::future::ready(end)
    }
}

fn main() {
    let (runs, peaks) = (Arc::new(AtomicUsize::new(0)), Arc::new(Peaks::default()));
    let kind = Large {
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
        let run = pin!(supervisor.run("large", connector, &config, &token));
        let sample = pin!(async {
            loop {
                clock.sleep(Span::from_nanos(10_000_000)).await;
                seen[runs.load(Relaxed)].fetch_max(ALLOCATOR.held(), Relaxed);
            }
        });
        let (mut run, mut sample) = (run, sample);
        poll_fn(|cx| {
            assert!(sample.as_mut().poll(cx).is_pending(), "the sample loops");
            run.as_mut().poll(cx)
        })
        .await
    });
    result
        .expect("the run ends")
        .expect("the connector ends ok");
    let at = |runs: usize| peaks[runs].load(Relaxed);
    let (short, long) = (at(SHORT), at(SHORT + 1));
    assert!(
        long <= short + 32 * 1_024,
        "a run and its wait hold {short} bytes after a short error, {long} after one \
         of 1 MiB"
    );
}
