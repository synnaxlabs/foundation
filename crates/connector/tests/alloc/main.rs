//! Polling a registered wait or race allocates nothing, and so do a tick from the third
//! call on and a set of a status count. This binary has no test harness: the count
//! covers each thread, and a harness allocates on its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::cell::Cell;
use std::future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::task::{Context, Wake, Waker};

use connector::cancel::Token;
use connector::pace::Timer;
use types::time::Rate;

/// A waker with a reference count, as a task has. It counts its wakes.
#[derive(Default)]
struct Tally(AtomicU64);

impl Wake for Tally {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );

    let token = Token::new();
    let wakers = [
        Waker::from(Arc::new(Tally::default())),
        Waker::from(Arc::new(Tally::default())),
    ];
    check(pin!(token.wait()), &token, &wakers, "wait");
    check(
        pin!(token.race(future::pending::<()>())),
        &token,
        &wakers,
        "race",
    );
    check_ticks();
    #[cfg(feature = "sim")]
    status::check();
}

/// Checks that 64 ticks after two warm-up ticks allocate nothing in their polls. The
/// count covers only the polls of `tick`, not the simulator around them.
fn check_ticks() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let clock = node.clock();
    let config = env::shards::Config {
        name: "shard-0".into(),
        core: Some(0),
    };
    let handle = node
        .shards()
        .start(config, move |_| async move {
            let token = Token::new();
            let rate = Rate::new(1_000, 1).expect("1 kHz is a rate");
            let mut timer = Timer::new(&clock, rate);
            // Tick 0 never waits, and the simulated clock grows its timer list on the
            // first wait.
            for _ in 0..2 {
                assert!(timer.tick(&token).await.is_some(), "a live token");
            }
            let allocations = Cell::new(0);
            for _ in 0..64 {
                let mut tick = pin!(timer.tick(&token));
                let tick = future::poll_fn(|cx| {
                    let (poll, count) = ALLOCATOR.count(|| tick.as_mut().poll(cx));
                    allocations.set(allocations.get() + count);
                    poll
                })
                .await;
                assert!(tick.is_some(), "a live token");
            }
            assert_eq!(
                allocations.get(),
                0,
                "a tick from the third call on allocates nothing"
            );
        })
        .expect("the shard starts");
    sim.run().expect("the run ends");
    handle.join().expect("the shard ends");
}

/// Polls `f` once to register it, then checks that 64 more polls with alternating
/// wakers allocate nothing.
fn check<F: Future>(mut f: Pin<&mut F>, token: &Token, wakers: &[Waker], name: &str) {
    let mut cx = Context::from_waker(&wakers[0]);
    assert!(f.as_mut().poll(&mut cx).is_pending(), "{name}: live token");
    let ((), allocations) = ALLOCATOR.count(|| {
        for waker in wakers.iter().cycle().take(64) {
            let mut cx = Context::from_waker(waker);
            assert!(f.as_mut().poll(&mut cx).is_pending(), "{name}: live token");
        }
        assert!(!token.cancelled(), "{name}: the token is live");
    });
    assert_eq!(
        allocations, 0,
        "{name}: a registered poll allocates nothing"
    );
}

#[cfg(feature = "sim")]
mod status {
    use std::cell::Cell;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
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

    use super::ALLOCATOR;

    /// A kind that sets its count `samples` 64 times in each second, and counts the
    /// allocations of the sets from the third second on.
    struct Sets(Arc<AtomicU64>);

    impl Kind for Sets {
        type Config = ();

        fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
            Ok(())
        }

        fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
            let counts = vec![samples()];
            Ok(Channels {
                counts,
                ..Channels::default()
            })
        }

        fn discover(
            &self,
            _: &Token,
        ) -> impl Future<Output = Result<Vec<Document>, Error>> {
            std::future::ready(Ok(Vec::new()))
        }

        async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
            let count = ctx.count("samples");
            let allocations = Cell::new(0);
            // The first wakes of the flush grow the simulator's lists.
            for second in 0..6_u64 {
                let ((), n) = ALLOCATOR.count(|| {
                    for i in 0..64 {
                        count.set(second * 64 + i);
                    }
                });
                if second >= 2 {
                    allocations.set(allocations.get() + n);
                }
                ctx.clock().sleep(Span::from_nanos(1_100_000_000)).await;
            }
            self.0.store(allocations.get(), Relaxed);
            Ok(())
        }
    }

    fn samples() -> Name {
        "samples".parse().expect("a valid name")
    }

    /// Checks that a set of a status count allocates nothing, also the set that wakes
    /// the status writer.
    pub(super) fn check() {
        let allocations = Arc::new(AtomicU64::new(1));
        let kind = Sets(Arc::clone(&allocations));
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
            let kinds = Table::new().with("sets", kind);
            let config = testing::create_config(env, node.net(), kinds).await;
            let connector: Name = "plant.sets".parse().expect("a valid name");
            let counts = [samples()];
            let first = channel::Key::from_u128(100);
            let status = testing::create_status(&connector, &counts, first);
            config
                .hub
                .set_definitions(status.iter().map(|(name, def)| (name, def)));
            Supervisor::new(config)
                .run("sets", connector, &Document::default(), &Token::new())
                .await
        });
        result
            .expect("the run ends")
            .expect("the connector ends ok");
        assert_eq!(
            allocations.load(Relaxed),
            0,
            "a set of a status count allocates nothing"
        );
    }
}
