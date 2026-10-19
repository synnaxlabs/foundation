//! Polling a registered wait or race allocates nothing, and so do a tick from the third
//! call on, a stamped request, and a set of a status count. This binary has no test
//! harness: the count covers each thread, and a harness allocates on its own thread at
//! any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::cell::Cell;
use std::future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;
use std::task::{Context, Wake, Waker};

use connector::cancel::Token;
use connector::kind::Table;
use connector::pace::Timer;
use connector::stamp::Stamps;
use connector::testing::{self, Rig};
use types::channel;
use types::name::Name;
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
    check_stamps();
    status::check(status::Shows::Nothing);
    status::check(status::Shows::Error);
    status::check(status::Shows::Note);
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

/// Checks that 64 polls of a stamped request that is ready allocate nothing, after
/// one warm-up request.
fn check_stamps() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let result = sim.run_on(&node, |node, tasks| async move {
        let rig = Rig::open(env(&node, tasks), node.net(), Table::new()).await;
        let connector: Name = "plant.stamp".parse().expect("a valid name");
        let first = channel::Key::from_u128(100);
        rig.define(testing::create_status(&connector, &[], first));
        let state = "plant.stamp.status.state".parse().expect("a valid name");
        let writer = rig.writer(connector, vec![state]).await;
        let mut stamps = Stamps::default();
        stamps.request(&writer, future::ready(())).await;
        let mut cx = Context::from_waker(Waker::noop());
        let ((), allocations) = ALLOCATOR.count(|| {
            for _ in 0..64 {
                let request = pin!(stamps.request(&writer, future::ready(())));
                assert!(request.poll(&mut cx).is_ready(), "a ready request");
            }
        });
        allocations
    });
    assert_eq!(
        result.expect("the run ends"),
        0,
        "a stamped request allocates nothing"
    );
}

/// The inputs of a hub on a new shard of `node`.
fn env(node: &sim::node::Node, tasks: env::tasks::Tasks) -> hub::testing::Env {
    hub::testing::Env {
        files: node.files(),
        clock: node.clock(),
        wall: node.wall(),
        entropy: node.entropy(),
        tasks,
    }
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

mod status {
    use std::cell::Cell;
    use std::future;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::Ordering::Relaxed;
    use std::sync::atomic::{AtomicBool, AtomicU64};

    use connector::cancel::Token;
    use connector::kind::{Channels, Context, Error, Kind, Table};
    use connector::testing::Rig;
    use document::Document;
    use document::diagnostic::Diagnostic;
    use types::name::Name;
    use types::time::Span;

    use super::ALLOCATOR;

    /// A kind that sets its count `samples` 64 times each 2.1 s, so each first set
    /// finds the status write due, and counts the allocations of the sets from the
    /// third time on. It holds `steady` from the third time on until it ends its last
    /// sleep, and counts in `slept` what its sleeps allocate meanwhile. With
    /// `Shows::Error` or `Shows::Note`, it shows an error or a note before its first
    /// set, so each status write after the start of the run and before its end holds
    /// it.
    struct Sets {
        allocations: Arc<AtomicU64>,
        steady: Arc<AtomicBool>,
        slept: Arc<AtomicU64>,
        shows: Shows,
    }

    /// What a run of [`Sets`] shows before it sets its count.
    #[derive(Clone, Copy, Debug)]
    pub(super) enum Shows {
        Nothing,
        /// An error, so the status write compares its text at each write.
        Error,
        /// A note, so the status write compares it at each write.
        Note,
    }

    impl Kind for Sets {
        type Config = ();
        const SCHEMA: document::schema::Body = connector::testing::READS;

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

        async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
            let count = ctx.count("samples");
            match self.shows {
                Shows::Nothing => {}
                Shows::Error => ctx.error(&"refused"),
                Shows::Note => ctx.note(&"polls"),
            }
            let allocations = Cell::new(0);
            // The first wakes of the flush grow the simulator's lists.
            for round in 0..6_u64 {
                let ((), n) = ALLOCATOR.count(|| {
                    for i in 0..64 {
                        count.set(round * 64 + i);
                    }
                });
                if round >= 2 {
                    allocations.set(allocations.get() + n);
                    self.steady.store(true, Relaxed);
                }
                let (sleep, n) = ALLOCATOR
                    .count(|| ctx.clock().sleep(Span::from_nanos(2_100_000_000)));
                if self.steady.load(Relaxed) {
                    self.slept.fetch_add(n, Relaxed);
                }
                let mut sleep = pin!(sleep);
                future::poll_fn(|cx| {
                    let (poll, n) = ALLOCATOR.count(|| sleep.as_mut().poll(cx));
                    if self.steady.load(Relaxed) {
                        self.slept.fetch_add(n, Relaxed);
                    }
                    poll
                })
                .await;
            }
            self.steady.store(false, Relaxed);
            self.allocations.store(allocations.get(), Relaxed);
            // The end of the run writes the status, after the polls that count.
            ctx.clock().sleep(Span::from_nanos(1)).await;
            Ok(())
        }
    }

    fn samples() -> Name {
        "samples".parse().expect("a valid name")
    }

    /// Checks that a set of a status count allocates nothing, also the set that wakes
    /// the status writer, and that so does each poll of the supervisor while the kind
    /// sets its count, with the write of the status, but for the kind's sleeps. With
    /// `Shows::Error` or `Shows::Note`, the run shows an error or a note, so a clone
    /// of its text at each write fails the check.
    pub(super) fn check(shows: Shows) {
        let allocations = Arc::new(AtomicU64::new(1));
        let steady = Arc::new(AtomicBool::new(false));
        let slept = Arc::new(AtomicU64::new(0));
        let kind = Sets {
            allocations: Arc::clone(&allocations),
            steady: Arc::clone(&steady),
            slept: Arc::clone(&slept),
            shows,
        };
        let polled = Arc::new(AtomicU64::new(0));
        let (counted, into) = (Arc::clone(&steady), Arc::clone(&polled));
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let result = sim.run_on(&node, |node, tasks| async move {
            let kinds = Table::new().with("sets", kind);
            let rig = Rig::open(super::env(&node, tasks), node.net(), kinds).await;
            let connector: Name = "plant.sets".parse().expect("a valid name");
            let (token, config) = (Token::new(), Document::default());
            let mut run = pin!(
                rig.open_connector("sets", connector, &config, &token)
                    .await
                    .run()
            );
            future::poll_fn(|cx| {
                if !counted.load(Relaxed) {
                    return run.as_mut().poll(cx);
                }
                let (poll, n) = ALLOCATOR.count(|| run.as_mut().poll(cx));
                into.fetch_add(n, Relaxed);
                poll
            })
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
        assert!(slept.load(Relaxed) > 0, "the sleeps of the kind count");
        assert_eq!(
            polled.load(Relaxed),
            slept.load(Relaxed),
            "a poll of the supervisor allocates only in the kind's sleeps, shows: \
             {shows:?}"
        );
    }
}
