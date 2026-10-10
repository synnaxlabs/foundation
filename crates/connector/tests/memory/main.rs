//! After a device error of 1 MiB, a run adds no more than 32 KiB over what it adds
//! after a short one. A run adds about 16 KiB more every few runs, so the test cannot
//! see a smaller excess; the unit tests of `status` pin the capacity of the text.
//! This binary has no test harness: the count covers each thread, and a harness
//! allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

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

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

const MIB: usize = 1 << 20;

/// The runs that end with a short error, so that the bytes a run adds settle.
const SHORT: usize = 3;

/// A kind whose first [`SHORT`] runs end with a device error of 1 byte, whose next run
/// ends with one of 1 MiB, and whose last run ends `Ok`. Each run records the bytes
/// held at its start.
struct Large {
    runs: AtomicUsize,
    held: Arc<[AtomicUsize; SHORT + 2]>,
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
        self.held[run].store(ALLOCATOR.held(), Relaxed);
        let end = match run {
            ..SHORT => Err(Error::Device("a".into())),
            SHORT => Err(Error::Device("a".repeat(MIB).into())),
            _ => Ok(()),
        };
        std::future::ready(end)
    }
}

fn main() {
    let held = Arc::new(std::array::from_fn(|_| AtomicUsize::new(0)));
    let kind = Large {
        runs: AtomicUsize::new(0),
        held: Arc::clone(&held),
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
        supervisor.run("large", connector, &config, &token).await
    });
    result
        .expect("the run ends")
        .expect("the connector ends ok");
    let at = |run: usize| held[run].load(Relaxed);
    let short = at(SHORT).saturating_sub(at(SHORT - 1));
    let long = at(SHORT + 1).saturating_sub(at(SHORT));
    assert!(
        long <= short + 32 * 1_024,
        "a run adds {short} bytes after a short error, {long} after one of 1 MiB"
    );
}
