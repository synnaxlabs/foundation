//! The cost of a stamp, on a sim node. Run with `cargo bench -p connector --bench
//! stamp`.
//!
//! - `now`: one read of mesh time by `Writer::now`.
//! - `request`: `poll::Writer::request` of a request that is ready at once, so the
//!   line is its two reads of mesh time and the rule of the stamp.
//! - `before`: `Fit::before` of a fit that bounds the clock, one value later in device
//!   time each call.
//! - `between`: `Fit::between` of that fit, one read 1 us later in device time each
//!   call.
//! - `old`: `Fit::before` of a fit whose newest reading is 1 s later in device time,
//!   one value later in device time each call, up to that reading.
//! - `mesh`: `Fit::mesh` of that fit.
//! - `sample`: `poll::Writer::sample` of an interval 1 us later each call.
//!
//! Each figure is ps per call, so a change under 1 ns shows, with the count of the
//! allocations of the calls.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::future;
use std::hint::black_box;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use connector::cancel::Token;
use connector::kind::{self, Channels, Error, Kind, Table};
use connector::poll::Writer;
use connector::stamp::{Fit, Fitted};
use connector::testing::{self, Rig};
use document::Document;
use document::diagnostic::Diagnostic;
use spec::channel::{Channel, Data, Kind as ChannelKind};
use spec::data_type::DataType;
use spec::definition::Definition;
use types::channel;
use types::name::Name;
use types::sample::{Scalar, Type};
use types::time::{Interval, Monotonic, Span, Stamp};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

const CALLS: u64 = 10_000;
const WARMUP: usize = 10;
const ROUNDS: usize = 100;

fn main() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let lines = sim.run_on(&node, |node, tasks| async move {
        let env = hub::testing::Env {
            files: node.files(),
            clock: node.clock(),
            wall: node.wall(),
            entropy: node.entropy(),
            tasks,
        };
        let stamped = Arc::new(Mutex::new(Vec::new()));
        let kind = Stamped(Arc::clone(&stamped));
        let rig = Rig::open(env, node.net(), Table::new().with("stamped", kind)).await;
        rig.define(definitions());
        let connector: Name = "plant.stamp".parse().expect("a valid name");
        let writer = rig.writer(connector.clone(), vec![value()]).await;
        let now = time("now", || {
            black_box(writer.now());
        });
        drop(writer);
        let (token, config) = (Token::new(), Document::default());
        let opened = rig
            .open_connector("stamped", connector, &config, &token)
            .await;
        opened.run().await.expect("the connector ends ok");
        let [request, sample] = stamped
            .lock()
            .expect("no panic")
            .drain(..)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap_or_else(|_| panic!("the run times two lines"));
        [now, request, before(), between(), old(), mesh(), sample]
    });
    print(&lines.expect("the run ends"));
}

/// The channel that the [`Stamped`] kind writes.
fn value() -> Name {
    "plant.stamp.value".parse().expect("a valid name")
}

/// An index `plant.stamp.time` and its `f64` channel [`value`].
fn definitions() -> Vec<(Name, Definition)> {
    let index = channel::Key::from_u128(100);
    let kind = ChannelKind::Index {
        error: None,
        control: None,
    };
    let time = Definition::Channel(Channel { key: index, kind });
    let data = Data::new(
        index,
        None,
        DataType::Sample(Type::Scalar(Scalar::F64)),
        None,
    );
    let kind = ChannelKind::Data(data.expect("no unit"));
    let key = channel::Key::from_u128(101);
    let data = Definition::Channel(Channel { key, kind });
    let time_name = "plant.stamp.time".parse().expect("a valid name");
    vec![(time_name, time), (value(), data)]
}

/// A kind that opens a [`Writer`] of [`value`], and puts its lines `request` and
/// `sample` in the list that it holds.
struct Stamped(Arc<Mutex<Vec<Line>>>);

impl Kind for Stamped {
    type Config = ();
    const SCHEMA: document::schema::Body = testing::READS;

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels {
            writes: vec![value()],
            counts: connector::poll::COUNTS
                .map(|count| count.parse().expect("a name"))
                .to_vec(),
            ..Channels::default()
        })
    }

    async fn run(&self, ctx: kind::Context<()>) -> Result<(), Error> {
        let writer = Writer::open(&ctx, vec![value()]).await?;
        let mut writer = writer.expect("no cancel");
        let request = request(&mut writer);
        let sample = sample(&mut writer);
        self.0.lock().expect("no panic").extend([request, sample]);
        Ok(())
    }
}

/// The line `request`.
fn request(writer: &mut Writer) -> Line {
    let mut cx = Context::from_waker(Waker::noop());
    time("request", || {
        let request = pin!(writer.request(future::ready(black_box(7_u8))));
        let Poll::Ready(stamped) = request.poll(&mut cx) else {
            panic!("the request is ready")
        };
        black_box(stamped);
    })
}

/// A fit of a clock 500 ns behind mesh time, from one read at device time 0.
fn fit() -> Fit {
    let mut fit = Fit::default();
    let fitted =
        fit.between(Monotonic(0), Stamp::from_nanos(400), Stamp::from_nanos(500));
    assert_eq!(fitted, Fitted::Kept, "one reading fits");
    fit
}

/// The line `before`.
fn before() -> Line {
    let mut fit = fit();
    let mut device = 0;
    time("before", || {
        device += 1_000;
        let arrived = Stamp::from_nanos(i64::try_from(device).expect("small") + 500);
        let fitted = black_box(fit.before(Monotonic(device), black_box(arrived)));
        assert_eq!(fitted, Fitted::Kept, "a true reading fits");
    })
}

/// The line `between`.
fn between() -> Line {
    let mut fit = fit();
    let mut device = 0;
    time("between", || {
        device += 1_000;
        let sent = Stamp::from_nanos(i64::try_from(device).expect("small") + 400);
        let answered = sent + Span::from_nanos(100);
        let fitted =
            black_box(fit.between(Monotonic(device), sent, black_box(answered)));
        assert_eq!(fitted, Fitted::Kept, "a true reading fits");
    })
}

/// The line `old`.
fn old() -> Line {
    const NEWEST: u64 = 1_000_000_000;
    let mut fit = fit();
    let newest = Stamp::from_nanos(i64::try_from(NEWEST).expect("small") + 500);
    assert_eq!(
        fit.before(Monotonic(NEWEST), newest),
        Fitted::Kept,
        "a true reading fits"
    );
    let mut device = 0;
    time("old", || {
        device = (device + 1_000) % NEWEST;
        let arrived = Stamp::from_nanos(i64::try_from(device).expect("small") + 500);
        let fitted = black_box(fit.before(Monotonic(device), black_box(arrived)));
        assert_eq!(fitted, Fitted::Kept, "a true reading fits");
    })
}

/// The line `mesh`.
fn mesh() -> Line {
    let fit = fit();
    let mut device = 0;
    time("mesh", || {
        device += 1_000;
        black_box(fit.mesh(black_box(Monotonic(device))));
    })
}

/// The line `sample`.
fn sample(writer: &mut Writer) -> Line {
    let mut earliest = writer.time().now();
    time("sample", || {
        earliest = earliest + Span::MICROSECOND;
        let interval = Interval {
            earliest,
            latest: earliest + Span::from_nanos(200),
        };
        black_box(writer.sample(black_box(interval), earliest));
    })
}

/// The ps per call of each timed round, and the allocations of all of them.
struct Line {
    name: &'static str,
    picos: Vec<u64>,
    allocations: u64,
}

/// Times `ROUNDS` rounds of `CALLS` calls of `call`, after `WARMUP` rounds.
#[expect(clippy::disallowed_methods, reason = "a benchmark reads a real clock")]
fn time(name: &'static str, mut call: impl FnMut()) -> Line {
    let mut line = Line {
        name,
        picos: Vec::new(),
        allocations: 0,
    };
    for round in 0..WARMUP + ROUNDS {
        let start = Instant::now();
        let ((), allocations) = ALLOCATOR.count(|| {
            for _ in 0..CALLS {
                call();
            }
        });
        let nanos = Instant::now().duration_since(start).as_nanos();
        if round >= WARMUP {
            let picos =
                u64::try_from(nanos * 1000).expect("a round takes under 2^64 ps");
            line.picos.push(picos / CALLS);
            line.allocations += allocations;
        }
    }
    line
}

/// Prints p10, p50, and p90 of the ps per call of each line, and its allocations.
#[expect(clippy::print_stdout, reason = "a benchmark prints its results")]
fn print(lines: &[Line]) {
    println!("ps per call over {ROUNDS} rounds of {CALLS} calls");
    println!(
        "{:<10} {:>7} {:>7} {:>7} {:>7}",
        "line", "p10", "p50", "p90", "allocs"
    );
    for line in lines {
        let mut picos = line.picos.clone();
        picos.sort_unstable();
        let at = |percent: usize| picos[picos.len() * percent / 100];
        println!(
            "{:<10} {:>7} {:>7} {:>7} {:>7}",
            line.name,
            at(10),
            at(50),
            at(90),
            line.allocations
        );
    }
}
