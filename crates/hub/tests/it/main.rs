//! Tests of `hub` sessions through the public surface, on one shard of a simulated
//! node.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path as FilePath, PathBuf};
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use block::{Heap, Pool, Unique};
use env::clock::Clock;
use env::files::Operation;
use env::tasks::Tasks;
use hub::Hub;
use hub::home::{Outcome, Refusal};
use hub::reader::{self, Ended, Mode, Reader, Received};
use hub::writer::{self, Failure, Writer};
use spec::channel::{Data, Kind};
use spec::data_type::DataType;
use spec::definition::Definition;
use types::authority::Authority;
use types::channel::{self, Slot};
use types::frame::key_set::Interner;
use types::frame::{self, Form, Label, Path, Range, View};
use types::name::{Name, Selector};
use types::sample::{Scalar, Type};
use types::time::{Span, Stamp};

#[path = "../common/net.rs"]
mod net;

#[path = "../common/agent.rs"]
mod agent;
mod definitions;
mod link;
mod named;
#[path = "../common/node.rs"]
mod node;
mod read;
mod region;
mod remote;
mod serve;
#[path = "../common/sessions.rs"]
mod sessions;

use agent::name;
use net::NODE;

const DIR: &str = "shard-0";
const RING: &str = "shard-0/ring";
const AREA: u64 = 1 << 22;
const BODY_MAX: usize = 1 << 16;
/// The area and body max of a ring that takes a frame past the window of a reader.
const WIDE_AREA: u64 = 1 << 25;
const WIDE_BODY_MAX: usize = 1 << 22;
/// Samples per series of a frame whose charge is past the window of a reader.
const PAST_WINDOW: i64 = 140_000;
/// The pool reserves its budget once for each size class, 59 times here.
const POOL: usize = 1 << 22;
const COMMIT: Span = Span::from_nanos(10_000_000);
/// Past the commit of a write.
const SETTLE: Span = Span::from_nanos(20_000_000);
const LIMITS: home::order::Limits = home::order::Limits {
    earliest: Stamp::from_nanos(1),
    ahead: Span::from_nanos(1_000_000_000),
};
const LIVE: Label = Label::Path(Path::Live);
/// The credit that the hub gives a complete reader past the frames it took.
const WINDOW: u64 = 1 << 20;
const STAMP: Type = Type::Scalar(Scalar::Stamp);
const I64: Type = Type::Scalar(Scalar::I64);
/// Two indexes: `time` with `value` and `value-c`, and `time-b` with `value-b`. `time`
/// is at slot 0.
const CHANNELS: [(u128, &str, Type, u128); 5] = [
    (1, "time", STAMP, 1),
    (2, "value", I64, 1),
    (3, "time-b", STAMP, 3),
    (4, "value-b", I64, 3),
    (5, "value-c", I64, 1),
];
/// The key of `time` in [`CHANNELS`].
const TIME: channel::Key = channel::Key::from_u128(CHANNELS[0].0);
/// The key of `value` in [`CHANNELS`].
const VALUE: channel::Key = channel::Key::from_u128(CHANNELS[1].0);
/// The key of `time-b` in [`CHANNELS`].
const TIME_B: channel::Key = channel::Key::from_u128(CHANNELS[2].0);

/// What one test gets: a hub on one shard, with [`CHANNELS`] defined.
struct Test {
    node: sim::node::Node,
    pool: Rc<Pool>,
    clock: Clock,
    mesh: clock::Reader,
    tasks: Tasks,
    /// How many tasks of the hub have ended.
    ended: Rc<Cell<usize>>,
    /// Holds the hub's tasks while paused, and counts their polls.
    pause: Rc<Pause>,
    /// The node's mesh clock until [`Test::sync`] runs it.
    unsynced: Option<clock::Clock>,
    /// A commit of the home, taken before the hub had it. It holds the ring open.
    commit: home::Commit,
    /// The node's region, which the hub holds too.
    region: Option<hub::Region>,
    hub: Hub,
}

impl Test {
    /// A hub on a new ring of `node` with `layout`, a pool of `pool` bytes, and
    /// `region`, whose mesh clock does not run yet. The founding of `region` must hold
    /// [`channels`].
    async fn new(
        node: sim::node::Node,
        tasks: Tasks,
        layout: buffer::Layout,
        pool: usize,
        region: Option<hub::Region>,
    ) -> Self {
        let pool = u64::try_from(pool).expect("a usize fits in a u64");
        let config = block::Config::new(pool).expect("the budget fits");
        let pool = Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
        let (unsynced, mesh) = clock::Clock::new(node.clock());
        let mut interner = Interner::new();
        let config = buffer::Config {
            files: node.files(),
            dir: PathBuf::from(DIR),
            pool: Rc::clone(&pool),
            clock: node.clock(),
            tasks: tasks.clone(),
            entropy: node.entropy(),
            layout,
            commit: COMMIT,
        };
        let buffer = buffer::Buffer::open(config, interner.slots())
            .await
            .expect("opens");
        let home = home::Shard::open(home::Config {
            shard: 0,
            buffer,
            clock: mesh.clone(),
            limits: LIMITS,
        });
        let commit = home.committed();
        let ended = Rc::new(Cell::new(0));
        let pause = Rc::new(Pause::default());
        let hub = Hub::new(hub::Config {
            home,
            interner,
            tasks: Tasks::new(Counted {
                tasks: tasks.clone(),
                ended: Rc::clone(&ended),
                pause: Rc::clone(&pause),
            }),
            node: NODE,
            time: mesh.clone(),
            entropy: node.entropy(),
            region: region.as_ref().map(|region| hub::Region {
                mesh: region.mesh.clone(),
                transport: Rc::clone(&region.transport),
            }),
        });
        if region.is_none() {
            hub.set_definitions(&channels());
        }
        Self {
            clock: node.clock(),
            node,
            pool,
            mesh,
            tasks,
            ended,
            pause,
            unsynced: Some(unsynced),
            commit,
            region,
            hub,
        }
    }

    /// Runs the node's mesh clock and returns once the node has mesh time.
    async fn sync(&mut self) {
        let clock = self.unsynced.take().expect("the clock is not running");
        let wall = self.node.wall();
        self.tasks.spawn(async move { clock.run(wall).await });
        while self.mesh.now().mesh.is_none() {
            self.clock.sleep(Span::from_nanos(1)).await;
        }
    }

    /// Lets the hub's tasks settle, pauses them, runs `writes`, and sleeps through
    /// their commit. Then resumes the tasks, and lets the run pick whether main runs
    /// again before or after the commit task. Gives whether main ran first.
    async fn race(&self, writes: impl FnOnce()) -> bool {
        self.clock.sleep(SETTLE).await;
        self.pause.pause();
        writes();
        self.clock.sleep(SETTLE).await;
        let polls = self.pause.polls.get();
        assert_eq!(self.pause.resume(), 1, "only the commit task waits to run");
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                return Poll::Ready(());
            }
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        })
        .await;
        self.pause.polls.get() == polls
    }

    /// Mesh time now: the midpoint of the clock's interval.
    fn now(&self) -> i64 {
        let now = self.mesh.now().mesh.expect("the node has mesh time");
        now.earliest.nanos().midpoint(now.latest.nanos())
    }

    /// A writer of `subject` at authority 1 on `channels`.
    async fn writer(&self, subject: &str, channels: &[&str]) -> Writer {
        self.hub
            .writer(config(subject, channels))
            .await
            .expect("opens")
    }

    /// Sets [`CHANNELS`] and `more`, each `(key, name, data type, index)`, as the
    /// definitions: in a spec that the mesh puts in use, with a region.
    async fn define<'a>(
        &self,
        more: impl IntoIterator<Item = (u128, &'a str, DataType, u128)>,
    ) {
        let mut channels = channels();
        channels.extend(more.into_iter().map(|(key, channel, data_type, index)| {
            (name(channel), definition(key, data_type, index))
        }));
        match self.region {
            Some(_) => self.set_spec(channels).await,
            None => self.hub.set_definitions(&channels),
        }
    }

    async fn reader(&self, channels: &[&str], mode: Mode) -> reader::Reader {
        self.hub
            .reader(unnamed(channels, mode))
            .await
            .expect("opens")
    }

    /// How many blocks the pool gives, largest first, until it has no room.
    fn free(&self) -> usize {
        fill(&self.pool).len()
    }
}

/// Every block `pool` gives, largest first, until it has no room.
fn fill(pool: &Pool) -> Vec<Unique> {
    let mut blocks = Vec::new();
    let mut len = pool.largest();
    while len > 0 {
        while let Ok(block) = pool.alloc(len) {
            blocks.push(block);
        }
        len -= len.div_ceil(16);
    }
    blocks
}

/// Spawns on `tasks`, and counts each task that completes in `ended`. Polls no task
/// while `pause` holds them, and counts each poll in `pause`.
struct Counted {
    tasks: Tasks,
    ended: Rc<Cell<usize>>,
    pause: Rc<Pause>,
}

/// Holds back the polls of the hub's tasks, as an executor that runs other tasks
/// first does.
#[derive(Default)]
struct Pause {
    /// The waker of each task woken while paused, when paused.
    held: std::cell::RefCell<Option<Vec<Waker>>>,
    /// How many polls the hub's tasks have had.
    polls: Cell<usize>,
}

impl Pause {
    fn pause(&self) {
        *self.held.borrow_mut() = Some(Vec::new());
    }

    /// Wakes each task woken while paused. Gives how many tasks it woke.
    fn resume(&self) -> usize {
        let held = self.held.borrow_mut().take().unwrap_or_default();
        let mut tasks: Vec<Waker> = Vec::new();
        for waker in held {
            if !tasks.iter().any(|task| task.will_wake(&waker)) {
                tasks.push(waker);
            }
        }
        let woke = tasks.len();
        tasks.into_iter().for_each(Waker::wake);
        woke
    }

    /// Polls `task` and counts the poll, or, while paused, holds the waker of `cx`.
    fn poll<F: Future + ?Sized>(
        &self,
        cx: &mut Context<'_>,
        task: Pin<&mut F>,
    ) -> Poll<F::Output> {
        if let Some(held) = self.held.borrow_mut().as_mut() {
            held.push(cx.waker().clone());
            return Poll::Pending;
        }
        self.polls.set(self.polls.get() + 1);
        task.poll(cx)
    }
}

/// Runs `main` as [`run_on`] does, once for each value of `sim::Config::seed` below
/// `values`. Panics unless main ran first, as `main` gives from [`Test::race`], in
/// some runs and second in others.
fn races<F>(
    values: u64,
    ring: (u64, usize),
    main: impl Fn(u64, Test) -> F + Clone + Send + 'static,
) where
    F: Future<Output = bool> + 'static,
{
    let (first, second) = (0..values).fold((0, 0), |(first, second), seed| {
        let main = main.clone();
        if run_on(seed, ring, move |test| main(seed, test)) {
            (first + 1, second)
        } else {
            (first, second + 1)
        }
    });
    assert!(
        first > 0 && second > 0,
        "main ran first in {first} runs and second in {second}"
    );
}

impl env::tasks::Driver for Counted {
    fn spawn(&self, mut task: env::tasks::Task) {
        let (ended, pause) = (Rc::clone(&self.ended), Rc::clone(&self.pause));
        self.tasks.spawn(async move {
            std::future::poll_fn(|cx| pause.poll(cx, task.as_mut())).await;
            ended.set(ended.get() + 1);
        });
    }
}

/// Runs `main` on a hub of one shard of a simulated node, once the node has mesh
/// time.
fn run<F>(seed: u64, main: impl FnOnce(Test) -> F + Send + 'static)
where
    F: Future<Output = ()> + 'static,
{
    run_on(seed, (AREA, BODY_MAX), main);
}

/// Runs `main` as [`run`] does, on a ring of the area and body max in `ring`. Gives
/// what `main` gives.
fn run_on<T, F>(
    seed: u64,
    ring: (u64, usize),
    main: impl FnOnce(Test) -> F + Send + 'static,
) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
{
    unsynced_on(seed, ring, |mut test| async move {
        test.sync().await;
        main(test).await
    })
}

/// Runs `main` on a hub of one shard of a simulated node with no mesh time.
fn unsynced<F>(seed: u64, main: impl FnOnce(Test) -> F + Send + 'static)
where
    F: Future<Output = ()> + 'static,
{
    unsynced_on(seed, (AREA, BODY_MAX), main);
}

/// Runs `main` as [`unsynced`] does, on a ring of the area and body max in `ring`.
/// Gives what `main` gives.
fn unsynced_on<T, F>(
    seed: u64,
    (area, body_max): (u64, usize),
    main: impl FnOnce(Test) -> F + Send + 'static,
) -> T
where
    T: Send + 'static,
    F: Future<Output = T> + 'static,
{
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, move |node, tasks| async move {
        let layout = buffer::Layout::new(area, body_max).expect("a ring");
        main(Test::new(node, tasks, layout, POOL, None).await).await
    })
    .expect("the run ends")
}

/// An unnamed reader of `reader` on the channels named `channels`.
fn unnamed(channels: &[impl AsRef<str>], mode: Mode) -> reader::Config {
    reader::Config {
        select: Selector::new(channels.iter().map(AsRef::as_ref)).expect("a selector"),
        mode,
        subject: name("reader"),
        name: None,
        hold: Span::ZERO,
    }
}

fn config(subject: &str, channels: &[&str]) -> writer::Config {
    writer::Config {
        subject: name(subject),
        authority: Authority(1),
        lease: None,
        channels: channels.iter().map(|n| name(n)).collect(),
    }
}

/// The position of the channel `key` in the writer's key set.
fn entry(set: &types::frame::key_set::KeySet, key: u128) -> usize {
    let key = channel::Key::from_u128(key);
    set.entries()
        .iter()
        .position(|entry| entry.key == key)
        .expect("the key set holds the channel")
}

/// Writes `stamps` to `time` and `values` to `value`, one sample of each per pair.
fn write(writer: &mut Writer, stamps: &[i64], values: &[i64]) -> Vec<Outcome> {
    write_series(writer, &[(1, stamps), (2, values)])
}

/// Writes the samples of each channel by key, as [`draft`] makes them.
fn write_series(writer: &mut Writer, channels: &[(u128, &[i64])]) -> Vec<Outcome> {
    written(writer, channels).expect("the home takes it")
}

/// Writes as [`write_series`] does, and gives what the write gave.
fn written(
    writer: &mut Writer,
    channels: &[(u128, &[i64])],
) -> Result<Vec<Outcome>, Failure> {
    let draft = draft(writer, channels);
    writer.write(LIVE, draft).map(<[_]>::to_vec)
}

/// A frame of the samples of each channel by key. The count of each group is that of
/// its index's samples.
fn draft(writer: &Writer, channels: &[(u128, &[i64])]) -> frame::Draft {
    let set = writer.set();
    let mut series: Vec<_> = channels
        .iter()
        .map(|&(key, samples)| (entry(set, key), samples.len() * 8))
        .collect();
    series.sort_unstable();
    let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
    for &(key, samples) in channels {
        let entry = entry(set, key);
        let bytes = draft.series_mut(entry).expect("the series is present");
        for (bytes, sample) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(samples) {
            *bytes = sample.to_le_bytes();
        }
        if set.index(entry) == entry {
            let count = u32::try_from(samples.len()).expect("a short frame");
            draft.set_count(set.entries()[entry].group, count);
        }
    }
    draft
}

/// The entries of the view in `received`, as channel keys.
fn keys(received: &Received<'_>) -> Vec<u128> {
    let entries = received.set().entries();
    let present = received.view().iter().map(|(entry, _)| entries[entry].key);
    present.map(channel::Key::as_u128).collect()
}

/// The name of each channel of the view of `received`, in entry order.
fn names<'a>(received: &Received<'a>) -> Vec<&'a str> {
    let view = received.view();
    view.iter().map(|(entry, _)| received.name(entry)).collect()
}

/// Writes frame `n` of a run from `now`: 1000 samples per series, with values that
/// do not compress.
fn write_wide(writer: &mut Writer, now: i64, n: i64) {
    const SAMPLES: i64 = 1000;
    write_samples(writer, now + n * SAMPLES, SAMPLES);
}

/// Writes `samples` samples per series from `start`, with values that do not
/// compress.
fn write_samples(writer: &mut Writer, start: i64, samples: i64) {
    let stamps: Vec<_> = (start..start + samples).collect();
    write(writer, &stamps, &scrambled(&stamps));
}

/// A value for each of `stamps` that does not compress.
fn scrambled(stamps: &[i64]) -> Vec<i64> {
    stamps
        .iter()
        .map(|&s| {
            let x = s.wrapping_mul(6_364_136_223_846_793_005);
            x ^ (x >> 29)
        })
        .collect()
}

/// The samples of the channel `key` in `received`.
fn samples(received: &Received<'_>, key: u128) -> Vec<i64> {
    let entry = entry(received.set(), key);
    let group = received.set().entries()[entry].group;
    let count = received
        .view()
        .range(group)
        .expect("the group is present")
        .count;
    let count = usize::try_from(count).expect("a count");
    let (_, bytes) = received
        .view()
        .iter()
        .find(|&(present, _)| present == entry)
        .expect("the view holds the series");
    let data_type = received.set().entries()[entry].data_type;
    let mut out = Vec::new();
    codec::decode(data_type, count, bytes, &mut out).expect("decodes");
    let (chunks, _) = out.as_chunks::<8>();
    chunks
        .iter()
        .map(|chunk| i64::from_le_bytes(*chunk))
        .collect()
}

fn applied(seq: u64) -> Outcome {
    Outcome::Applied {
        slot: Slot::new(0),
        range: Range { seq, count: 1 },
    }
}

/// Polls `future` once, with a waker that does nothing.
fn poll_once<F: Future>(future: F) -> Poll<F::Output> {
    pin!(future).poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn gives_a_complete_reader_each_frame_in_seq_order_with_the_samples_written() {
    run(1, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        for n in 0..3 {
            let outcomes = write(&mut writer, &[now + n], &[n * 10]);
            assert_eq!(outcomes, [applied(n.cast_unsigned())]);
        }
        for n in 0_i64..3 {
            let received = reader.next().await.expect("a frame");
            let range = Range {
                seq: n.cast_unsigned(),
                count: 1,
            };
            assert_eq!(received.view().range(0), Some(range));
            assert_eq!(samples(&received, 1), [now + n]);
            assert_eq!(samples(&received, 2), [n * 10]);
        }
    });
}

/// [`CHANNELS`], as the hub defines them in [`Test::new`].
fn channels() -> BTreeMap<Name, Definition> {
    CHANNELS
        .into_iter()
        .map(|(key, channel, data_type, index)| {
            let data_type = DataType::Sample(data_type);
            (name(channel), definition(key, data_type, index))
        })
        .collect()
}

/// [`channels`] without the channels of `names`.
fn without(names: &[&str]) -> BTreeMap<Name, Definition> {
    let mut channels = channels();
    for removed in names {
        channels
            .remove(&name(removed))
            .expect("a channel of the test");
    }
    channels
}

#[test]
fn gives_the_entry_of_each_channel_of_the_config_in_its_order() {
    run(4, |test| async move {
        let names = ["value-c", "value-b", "time", "value-b", "value"];
        let writer = test.writer("a", &names).await;
        let keys: Vec<_> = writer
            .entries()
            .iter()
            .map(|&entry| writer.set().entries()[entry].key.as_u128())
            .collect();
        assert_eq!(keys, [5, 4, 1, 4, 2]);
        let time = writer.set().groups()[0];
        assert_eq!(writer.entries()[2], time, "the entry of the index");
    });
}

#[test]
fn opens_no_session_on_an_unknown_name() {
    run(2, |test| async move {
        let writer = test.hub.writer(config("a", &["value", "nope"])).await;
        let error = writer.expect_err("an error");
        assert_eq!(error, writer::Error::Unknown(name("nope")));
        assert_eq!(error.to_string(), "no channel is named nope");
        let reader = test.hub.reader(unnamed(&["nope"], Mode::Complete)).await;
        let error = reader.expect_err("an error");
        assert_eq!(error, reader::Error::Empty);
        assert_eq!(error.to_string(), "the selector matches no channel");
    });
}

#[test]
fn opens_no_session_on_two_indexes_and_names_the_least_channel_of_each() {
    run(3, |test| async move {
        let names = ["value-c", "value-b", "value"];
        let error = test
            .hub
            .reader(unnamed(&names, Mode::Latest))
            .await
            .expect_err("an error");
        let many = reader::Error::ManyIndexes {
            first: name("value"),
            other: name("value-b"),
        };
        assert_eq!(error, many);
        assert_eq!(
            error.to_string(),
            "the channels value and value-b are on different indexes: open a reader \
             per index"
        );
        let error = test.hub.reader(unnamed(&["**"], Mode::Latest)).await;
        let many = reader::Error::ManyIndexes {
            first: name("time"),
            other: name("time-b"),
        };
        assert_eq!(error.expect_err("an error"), many);
        let writer = test.hub.writer(config("a", &[])).await;
        let error = writer.expect_err("an error");
        assert_eq!(error, writer::Error::Empty);
        assert_eq!(error.to_string(), "a writer names at least one channel");
    });
}

#[test]
fn gives_a_reader_each_channel_that_its_selector_matches() {
    run(41, |test| async move {
        let select = ["**", "!time-b", "!value-b"];
        let mut all = test.reader(&select, Mode::Complete).await;
        let mut writer = test.writer("a", &["value", "value-c"]).await;
        let now = test.now();
        write_series(&mut writer, &[(1, &[now]), (2, &[7]), (5, &[9])]);
        let received = all.next().await.expect("a frame");
        assert_eq!(keys(&received), [1, 2, 5]);
        assert_eq!(samples(&received, 2), [7]);
        assert_eq!(samples(&received, 5), [9]);
    });
}

#[test]
fn gives_the_name_of_each_channel_of_the_view_with_the_index() {
    run(43, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value", "value-c"]).await;
        let now = test.now();
        write_series(&mut writer, &[(1, &[now]), (2, &[7]), (5, &[9])]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(names(&received), ["time", "value"]);
    });
}

#[test]
#[should_panic(expected = "the entry 2 is not a channel of the reader")]
fn panics_on_the_name_of_an_entry_outside_the_view() {
    run(44, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value", "value-c"]).await;
        let now = test.now();
        write_series(&mut writer, &[(1, &[now]), (2, &[7]), (5, &[9])]);
        let received = reader.next().await.expect("a frame");
        let _name = received.name(entry(received.set(), 5));
    });
}

#[test]
#[should_panic(expected = "the entry 3 is not a channel of the reader")]
fn panics_on_the_name_of_an_entry_past_the_set() {
    run(47, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value", "value-c"]).await;
        let now = test.now();
        write_series(&mut writer, &[(1, &[now]), (2, &[7]), (5, &[9])]);
        let received = reader.next().await.expect("a frame");
        let _name = received.name(received.set().entries().len());
    });
}

#[test]
#[should_panic(expected = "the entry 1 is not a channel of the reader")]
fn panics_on_the_name_of_an_entry_whose_place_a_key_set_before_held() {
    run(48, |test| async move {
        let mut reader = test.reader(&["value-c"], Mode::Complete).await;
        let mut first = test.writer("a", &["value", "value-c"]).await;
        let now = test.now();
        write_series(&mut first, &[(1, &[now]), (2, &[7]), (5, &[9])]);
        drop(first);
        let received = reader.next().await.expect("a frame");
        let held = entry(received.set(), 5);
        assert_eq!(names(&received), ["time", "value-c"]);
        let mut second = test.writer("b", &["value"]).await;
        write_series(&mut second, &[(1, &[now + 1]), (2, &[7])]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(names(&received), ["time"]);
        let _name = received.name(held);
    });
}

#[test]
fn gives_a_named_reader_the_name_of_each_channel_with_the_index() {
    run(45, |test| async move {
        let config = reader::Config {
            name: Some(name("named")),
            ..unnamed(&["value"], Mode::Complete)
        };
        let mut reader = test.hub.reader(config).await.expect("opens");
        let mut writer = test.writer("a", &["value", "value-c"]).await;
        let now = test.now();
        write_series(&mut writer, &[(1, &[now]), (2, &[7]), (5, &[9])]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(names(&received), ["time", "value"]);
    });
}

#[test]
fn gives_a_channel_defined_again_at_a_new_key_its_name_at_the_new_open() {
    run(46, |test| async move {
        let mut changed = channels();
        changed.insert(name("value"), definition(9, DataType::Sample(I64), 1));
        test.hub.set_definitions(&changed);
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        write_series(&mut writer, &[(1, &[now]), (9, &[7])]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(keys(&received), [1, 9]);
        assert_eq!(names(&received), ["time", "value"]);
    });
}

#[test]
fn leaves_a_channel_that_an_exclusion_matches_out_of_the_view() {
    run(42, |test| async move {
        let select = ["**", "!time-b", "!value-b", "!value-c"];
        let mut reader = test.reader(&select, Mode::Complete).await;
        let mut writer = test.writer("a", &["value", "value-c"]).await;
        let now = test.now();
        write_series(&mut writer, &[(1, &[now]), (2, &[7]), (5, &[9])]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(keys(&received), [1, 2]);
        assert_eq!(samples(&received, 2), [7]);
    });
}

#[test]
fn gives_a_latest_reader_a_frame_before_its_commit_and_a_complete_reader_after() {
    run(4, |test| async move {
        let mut complete = test.reader(&["time"], Mode::Complete).await;
        let mut latest = test.reader(&["value"], Mode::Latest).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        write(&mut writer, &[now], &[7]);
        assert!(poll_once(complete.next()).is_pending());
        let Poll::Ready(received) = poll_once(latest.next()) else {
            panic!("a latest reader gets the frame before its commit");
        };
        assert_eq!(samples(&received.expect("a frame"), 2), [7]);
        let received = complete.next().await.expect("a frame");
        assert_eq!(samples(&received, 1), [now]);
        let time = entry(received.set(), 1);
        let entries: Vec<_> = received.view().iter().map(|(entry, _)| entry).collect();
        assert_eq!(entries, [time], "the view holds only the reader's channels");
    });
}

#[test]
fn opens_a_writer_once_the_node_has_mesh_time_and_a_reader_before() {
    unsynced(5, |mut test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let hub = test.hub.clone();
        let mut opening = pin!(hub.writer(config("a", &["value"])));
        test.clock.sleep(Span::SECOND).await;
        assert!(poll_once(opening.as_mut()).is_pending());
        test.sync().await;
        let synced = test.clock.now();
        let mut writer = opening.await.expect("opens");
        let waited = test.clock.now() - synced;
        assert!(
            waited <= Span::SECOND,
            "opened {waited} after sync, more than 1 s"
        );
        let now = writer.now();
        assert_eq!(now.nanos(), test.now());
        assert_eq!(write(&mut writer, &[now.nanos()], &[1]), [applied(0)]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [1]);
    });
}

#[test]
fn gives_its_time_once_the_node_has_mesh_time() {
    unsynced(6, |mut test| async move {
        let hub = test.hub.clone();
        let mut waiting = pin!(hub.time());
        test.clock.sleep(Span::SECOND).await;
        assert!(poll_once(waiting.as_mut()).is_pending());
        test.sync().await;
        let synced = test.clock.now();
        let time = waiting.await;
        let waited = test.clock.now() - synced;
        assert!(
            waited <= Span::SECOND,
            "gave {waited} after sync, more than 1 s"
        );
        let writer = test.writer("a", &["value"]).await;
        assert_eq!(time.now(), writer.now());
        assert_eq!(time.now().nanos(), test.now());
    });
}

#[test]
fn the_time_of_a_writer_reads_mesh_time_also_after_the_writer_drops() {
    run(5, |test| async move {
        let writer = test.writer("a", &["value"]).await;
        let time = writer.time();
        test.clock.sleep(Span::SECOND).await;
        let last = writer.now();
        assert_eq!(time.now(), last);
        assert_eq!(last.nanos(), test.now());
        drop(writer);
        test.clock.sleep(Span::SECOND).await;
        let after = time.clone().now();
        assert!(after >= last, "{after} is before {last}");
        assert_eq!(after.nanos(), test.now());
    });
}

#[test]
fn opens_no_writer_with_a_lease_of_zero() {
    run(9, |test| async move {
        let config = writer::Config {
            lease: Some(Span::ZERO),
            ..config("a", &["value"])
        };
        let error = test.hub.writer(config).await.expect_err("a lease of zero");
        let lease = hub::home::writer::Error::Lease { span: Span::ZERO };
        assert_eq!(error, writer::Error::Home(lease));
        let want =
            format!("control lease must be longer than zero, got {}", Span::ZERO);
        assert_eq!(error.to_string(), want);
    });
}

/// A writer on an unknown channel fails before the node has mesh time.
#[test]
fn opens_no_writer_on_an_unknown_name_before_the_node_has_mesh_time() {
    unsynced(10, |test| async move {
        let Poll::Ready(opened) = poll_once(test.hub.writer(config("a", &["nothing"])))
        else {
            panic!("the open fails before mesh time");
        };
        let error = opened.expect_err("no channel is named nothing");
        assert_eq!(error, writer::Error::Unknown(name("nothing")));
    });
}

/// A writer that waits for mesh time finds a channel that a call removed meanwhile
/// unknown.
#[test]
fn opens_no_writer_on_a_channel_removed_while_it_waits_for_mesh_time() {
    unsynced(8, |mut test| async move {
        let hub = test.hub.clone();
        let mut opening = pin!(hub.writer(config("a", &["value"])));
        assert!(poll_once(opening.as_mut()).is_pending());
        hub.set_definitions(&without(&["value"]));
        test.sync().await;
        let error = opening.await.expect_err("value was removed");
        assert_eq!(error, writer::Error::Unknown(name("value")));
    });
}

/// A control lease short enough to run out in a test.
const LEASE: Span = Span::from_nanos(1_000_000);

/// The outcome of a write of `subject` on `value` after a power cut 2 s into a run of
/// `before`, which must not end, so that its writers stay open until the cut.
fn written_after_cut<F>(
    seed: u64,
    before: impl FnOnce(Test) -> F + Send + 'static,
    subject: &'static str,
) -> Vec<Outcome>
where
    F: Future<Output = ()> + 'static,
{
    let (mut sim, node) = create_sim(seed);
    cut_after(&mut sim, &node, Span::from_nanos(2_000_000_000), before);
    written_on(&mut sim, &node, subject)
}

/// A simulation with one node.
fn create_sim(seed: u64) -> (sim::Sim, sim::node::Node) {
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    (sim, node)
}

/// A test on a hub of `node`, once the node has mesh time.
async fn started(node: sim::node::Node, tasks: Tasks) -> Test {
    let layout = buffer::Layout::new(AREA, BODY_MAX).expect("a ring");
    let mut test = Test::new(node, tasks, layout, POOL, None).await;
    test.sync().await;
    test
}

/// Runs `before` on a hub of `node` for `span`, then cuts the power. `before` must not
/// end, so that its sessions stay open until the cut.
fn cut_after<F>(
    sim: &mut sim::Sim,
    node: &sim::node::Node,
    span: Span,
    before: impl FnOnce(Test) -> F + Send + 'static,
) where
    F: Future<Output = ()> + 'static,
{
    let config = env::shards::Config {
        name: "shard-0".into(),
        core: None,
    };
    let started = node.shards().start(config, {
        let node = node.clone();
        move |tasks| async move { before(started(node, tasks).await).await }
    });
    let _shard = started.expect("the shard starts");
    sim.run_for(span).expect("the run goes on");
    sim.crash(node, sim::Crash::Power);
}

/// The outcome of a write of `subject` on `value` in a new run of `node`.
fn written_on(
    sim: &mut sim::Sim,
    node: &sim::node::Node,
    subject: &'static str,
) -> Vec<Outcome> {
    sim.run_on(node, move |node, tasks| async move {
        let test = started(node, tasks).await;
        let mut writer = test.writer(subject, &["value"]).await;
        write(&mut writer, &[test.now()], &[2])
    })
    .expect("the run after the cut ends")
}

/// A writer of `subject` on `value` with a control lease of [`LEASE`].
fn leased(subject: &str) -> writer::Config {
    writer::Config {
        lease: Some(LEASE),
        ..config(subject, &["value"])
    }
}

/// The outcome of a write refused because a restored holder holds control.
fn reserved() -> Outcome {
    Outcome::Refused {
        slot: Slot::new(0),
        refusal: Refusal::Reserved,
    }
}

/// A holder that stops writing loses control at the end of its lease with no other
/// call, so no grace is reserved for it after a power cut.
#[test]
fn records_no_holder_when_a_lease_runs_out_with_no_call() {
    for seed in 0..32 {
        let after = written_after_cut(
            seed,
            |test| async move {
                let mut a = test.hub.writer(leased("subject-a")).await.expect("opens");
                assert_eq!(write(&mut a, &[test.now()], &[1]), [applied(0)]);
                std::future::pending::<()>().await;
            },
            "subject-b",
        );
        assert_eq!(after, [applied(1)], "seed {seed}");
    }
}

#[test]
fn records_the_handoff_to_a_waiting_writer_when_a_lease_runs_out_with_no_call() {
    for seed in 0..32 {
        let after = written_after_cut(
            seed,
            |test| async move {
                let mut a = test.hub.writer(leased("subject-a")).await.expect("opens");
                assert_eq!(write(&mut a, &[test.now()], &[1]), [applied(0)]);
                let _b = test.writer("subject-b", &["value"]).await;
                std::future::pending::<()>().await;
            },
            "subject-a",
        );
        assert_eq!(after, [reserved()], "seed {seed}");
    }
}

/// The close of a holder can start a lease, so it wakes the task that waits for the
/// end of each lease.
#[test]
fn records_the_handoff_at_the_end_of_a_lease_that_a_close_starts() {
    for seed in 0..32 {
        let after = written_after_cut(
            seed,
            |test| async move {
                let a = test.writer("subject-a", &["value"]).await;
                let _b = test.hub.writer(leased("subject-b")).await.expect("opens");
                let _c = test.writer("subject-c", &["value"]).await;
                test.clock.sleep(SETTLE).await;
                drop(a);
                std::future::pending::<()>().await;
            },
            "subject-b",
        );
        assert_eq!(after, [reserved()], "seed {seed}");
    }
}

/// The end of one lease starts the next, with no call between.
#[test]
fn records_the_handoff_at_the_end_of_a_lease_that_the_end_of_a_lease_starts() {
    for seed in 0..32 {
        let after = written_after_cut(
            seed,
            |test| async move {
                let _a = test.hub.writer(leased("subject-a")).await.expect("opens");
                let _b = test.hub.writer(leased("subject-b")).await.expect("opens");
                let _c = test.writer("subject-c", &["value"]).await;
                std::future::pending::<()>().await;
            },
            "subject-c",
        );
        assert_eq!(after, [applied(0)], "seed {seed}");
    }
}

/// A restored holder with no writer on its index loses control at the end of its
/// grace, which only the wake at the carry of the reader's open lets the gate task
/// see.
#[test]
fn records_the_drop_of_a_restored_holder_with_a_reader_and_no_writer() {
    for seed in 0..32 {
        let (mut sim, node) = create_sim(seed);
        cut_after(
            &mut sim,
            &node,
            Span::from_nanos(2_000_000_000),
            |test| async move {
                let mut a = test.writer("subject-a", &["value"]).await;
                assert_eq!(write(&mut a, &[test.now()], &[1]), [applied(0)]);
                std::future::pending::<()>().await;
            },
        );
        // Past the grace of 10 s from the open.
        let span = Span::from_nanos(12_000_000_000);
        cut_after(&mut sim, &node, span, |test| async move {
            let _reader = test.reader(&["value"], Mode::Complete).await;
            std::future::pending::<()>().await;
        });
        assert_eq!(
            written_on(&mut sim, &node, "subject-b"),
            [applied(1)],
            "seed {seed}"
        );
    }
}

/// A leased writer that opens at the instant a lease ends races the gate task. In each
/// order, the gate task records the end of its lease, which only the wake at the open
/// lets it see when the task runs first.
#[test]
fn records_the_end_of_the_lease_of_a_writer_that_opens_as_a_lease_ends() {
    for seed in 0..32 {
        let after = written_after_cut(
            seed,
            |test| async move {
                let mut a = test.hub.writer(leased("subject-a")).await.expect("opens");
                assert_eq!(write(&mut a, &[test.now()], &[1]), [applied(0)]);
                test.clock.sleep(LEASE).await;
                let _b = test.hub.writer(leased("subject-b")).await.expect("opens");
                std::future::pending::<()>().await;
            },
            "subject-a",
        );
        assert_eq!(after, [applied(1)], "seed {seed}");
    }
}

/// A holder that closes at the instant its lease ends races the gate task. In each
/// order, the leased waiter takes control at the end of the lease, and the timer of the
/// gate task records the end of the waiter's lease. No order needs the wake at the
/// close.
#[test]
fn records_the_end_of_the_lease_of_a_waiter_when_a_holder_closes_as_its_lease_ends() {
    for seed in 0..32 {
        let after = written_after_cut(
            seed,
            |test| async move {
                let mut a = test.hub.writer(leased("subject-a")).await.expect("opens");
                let _b = test.hub.writer(leased("subject-b")).await.expect("opens");
                assert_eq!(write(&mut a, &[test.now()], &[1]), [applied(0)]);
                test.clock.sleep(LEASE).await;
                drop(a);
                std::future::pending::<()>().await;
            },
            "subject-a",
        );
        assert_eq!(after, [applied(1)], "seed {seed}");
    }
}

#[test]
fn gives_control_to_a_waiting_writer_when_a_writer_drops() {
    run(6, |test| async move {
        let a = test.writer("a", &["value"]).await;
        let mut b = test.writer("b", &["value"]).await;
        let now = test.now();
        let waiting = Outcome::Refused {
            slot: Slot::new(0),
            refusal: Refusal::Waiting,
        };
        assert_eq!(write(&mut b, &[now], &[1]), [waiting]);
        drop(a);
        assert_eq!(write(&mut b, &[now + 1], &[2]), [applied(0)]);
    });
}

/// The commit task does not hold the home, so the home and its buffer end with the
/// hub and its sessions, and the task ends.
#[test]
fn drops_the_home_once_the_hub_and_each_session_drop() {
    run(23, |test| async move {
        let reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        write(&mut writer, &[test.now()], &[1]);
        let Test {
            pool,
            clock,
            commit,
            hub,
            ended,
            ..
        } = test;
        drop(writer);
        clock.sleep(SETTLE).await;
        assert_eq!(ended.get(), 0, "the hub holds the home");
        drop((hub, reader, commit));
        clock.sleep(SETTLE).await;
        assert_eq!(Rc::strong_count(&pool), 1, "only the test holds the pool");
        assert_eq!(ended.get(), 2, "the commit and gate tasks ended");
    });
}

/// Sessions that outlive the hub keep the commit task: it ends once the last drops.
/// The hub drops while the task waits, so the commit of the write after it must wake
/// the task.
#[test]
fn ends_the_commit_task_once_each_session_that_outlives_the_hub_drops() {
    run(23, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        let Test {
            pool,
            clock,
            commit,
            hub,
            ended,
            ..
        } = test;
        clock.sleep(SETTLE).await;
        drop((hub, commit));
        clock.sleep(SETTLE).await;
        assert_eq!(ended.get(), 0, "the sessions hold the home");
        assert_eq!(write(&mut writer, &[now], &[7]), [applied(0)]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [7]);
        drop(writer);
        clock.sleep(SETTLE).await;
        assert_eq!(ended.get(), 0, "the reader holds the home");
        drop(reader);
        clock.sleep(SETTLE).await;
        assert_eq!(ended.get(), 2, "the commit and gate tasks ended");
        assert_eq!(Rc::strong_count(&pool), 1, "only the test holds the pool");
    });
}

/// Writes during a commit do not wake the commit task, which the commit wakes.
#[test]
fn does_not_wake_the_commit_task_for_the_writes_during_a_commit() {
    run(23, |test| async move {
        let _reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        test.clock.sleep(SETTLE).await;
        let before = test.pause.polls.get();
        for value in 0..20 {
            write(&mut writer, &[test.now()], &[value]);
            test.clock.sleep(Span::from_nanos(1)).await;
        }
        assert_eq!(test.pause.polls.get() - before, 0, "no wake for a write");
        test.clock.sleep(SETTLE).await;
        assert_eq!(
            test.pause.polls.get() - before,
            1,
            "one wake, for the commit"
        );
    });
}

/// A group commit that ends before the first poll of the commit task still wakes a
/// complete reader, as the hub takes its first commit at the start.
#[test]
fn wakes_a_complete_reader_for_a_commit_before_the_first_poll_of_the_task() {
    unsynced(23, |mut test| async move {
        test.pause.pause();
        test.sync().await;
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        write(&mut writer, &[test.now()], &[5]);
        test.clock.sleep(SETTLE).await;
        test.pause.resume();
        test.clock.sleep(Span::from_nanos(1)).await;
        let Poll::Ready(received) = poll_once(reader.next()) else {
            panic!("the commit before the first poll woke no reader");
        };
        assert_eq!(samples(&received.expect("a frame"), 2), [5]);
    });
}

/// A carry can make the home's deadline earlier only when it is new, so only the first
/// open of a reader on an index polls the gate task.
#[test]
fn polls_the_gate_task_at_a_reader_open_only_when_its_index_is_new() {
    run(23, |test| async move {
        test.clock.sleep(SETTLE).await;
        let before = test.pause.polls.get();
        let first = test.reader(&["value"], Mode::Latest).await;
        test.clock.sleep(SETTLE).await;
        let new = test.pause.polls.get() - before;
        let second = test.reader(&["value"], Mode::Latest).await;
        test.clock.sleep(SETTLE).await;
        let carried = test.pause.polls.get() - before - new;
        assert_eq!((new, carried), (1, 0));
        drop((first, second));
    });
}

/// Writes that append nothing do not wake the commit task, as no commit follows them.
#[test]
fn does_not_wake_the_commit_task_for_writes_with_no_samples() {
    run(23, |test| async move {
        let mut writer = test.writer("a", &["value"]).await;
        write(&mut writer, &[test.now()], &[0]);
        test.clock.sleep(SETTLE).await;
        let empty = Outcome::Applied {
            slot: Slot::new(0),
            range: Range { seq: 1, count: 0 },
        };
        let before = test.pause.polls.get();
        for _ in 0..1000 {
            assert_eq!(write(&mut writer, &[], &[]), std::slice::from_ref(&empty));
            test.clock.sleep(Span::from_nanos(1)).await;
        }
        test.clock.sleep(SETTLE).await;
        assert_eq!(test.pause.polls.get() - before, 0);
    });
}

/// A commit task that waits for a commit when the hub and its sessions drop ends at
/// its next poll. The commit it waited for dropped with the state.
#[test]
fn ends_the_commit_task_in_its_commit_wait_once_the_hub_drops() {
    run(23, |test| async move {
        let reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        test.clock.sleep(SETTLE).await;
        write(&mut writer, &[test.now()], &[1]);
        let Test {
            pool,
            clock,
            commit,
            hub,
            ended,
            ..
        } = test;
        clock.sleep(Span::from_nanos(1)).await;
        drop((hub, reader, writer, commit));
        clock.sleep(Span::from_nanos(1)).await;
        assert_eq!(ended.get(), 2, "the tasks ended before the commit");
        clock.sleep(SETTLE).await;
        assert_eq!(Rc::strong_count(&pool), 1, "only the test holds the pool");
    });
}

/// Once the hub and each session drop at a commit, before or after the commit task
/// polls it, the hub holds no commit: a commit of the home that resolves holds the
/// ring open until it drops. The commit and gate tasks end.
#[test]
fn drops_the_commit_it_waits_for_with_the_hub() {
    races(64, (AREA, BODY_MAX), |seed, test| async move {
        let reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let first = test
            .race(|| {
                write(&mut writer, &[test.now()], &[1]);
            })
            .await;
        let Test {
            node,
            clock,
            ended,
            mut commit,
            hub,
            pause,
            ..
        } = test;
        pause.pause();
        drop((hub, reader, writer));
        assert_eq!((&mut commit).await, Ok(()));
        let open = || async {
            node.files()
                .open(FilePath::new(RING), env::files::Mode::Write)
                .await
                .map(drop)
        };
        let busy = env::files::Error::Busy {
            path: PathBuf::from(RING),
        };
        assert_eq!(open().await, Err(busy), "seed {seed}");
        drop(commit);
        assert_eq!(open().await, Ok(()), "seed {seed}");
        pause.resume();
        clock.sleep(Span::from_nanos(1)).await;
        assert_eq!(ended.get(), 2, "the commit and gate tasks ended");
        first
    });
}

/// After a commit wakes a complete reader, the next commit wakes it again.
#[test]
fn gives_a_complete_reader_the_frame_of_a_later_commit() {
    run(23, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        test.clock.sleep(SETTLE).await;
        write(&mut writer, &[test.now()], &[1]);
        assert_eq!(samples(&reader.next().await.expect("a frame"), 2), [1]);
        test.clock.sleep(SETTLE).await;
        write(&mut writer, &[test.now()], &[2]);
        assert_eq!(samples(&reader.next().await.expect("a frame"), 2), [2]);
    });
}

#[test]
fn frees_the_frames_of_a_reader_when_it_drops() {
    run(7, |test| async move {
        let mut taker = test.reader(&["value"], Mode::Complete).await;
        let lagger = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        for n in 0..4 {
            write(&mut writer, &[now + n], &[n]);
            taker.next().await.expect("a frame");
        }
        let held = test.free();
        drop(lagger);
        // The home keeps the newest frame for latest readers.
        assert_eq!(test.free(), held + 3);
    });
}

#[test]
fn lets_other_tasks_run_while_a_complete_reader_waits_for_a_write() {
    run(8, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        test.clock.sleep(Span::from_nanos(1_000_000_000)).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        write(&mut writer, &[now], &[3]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [3]);
    });
}

#[test]
fn gives_each_reader_the_error_of_a_failed_sync_on_each_later_call() {
    run(9, |test| async move {
        let mut complete = test.reader(&["value"], Mode::Complete).await;
        let mut latest = test.reader(&["value"], Mode::Latest).await;
        let mut writer = test.writer("a", &["value"]).await;
        test.node.fail_file(FilePath::new(RING), Operation::Sync);
        let now = test.now();
        write(&mut writer, &[now], &[1]);
        assert_eq!(samples(&latest.next().await.expect("a frame"), 2), [1]);
        let failed = Ended::Buffer(env::files::Error::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        });
        for _ in 0..2 {
            assert_eq!(complete.next().await.err(), Some(failed.clone()));
            assert_eq!(latest.next().await.err(), Some(failed.clone()));
        }
        test.clock.sleep(SETTLE).await;
        assert_eq!(test.ended.get(), 1, "the commit task ended with the buffer");
        assert_eq!(
            failed.to_string(),
            "the buffer of the shard failed: sync of shard-0/ring failed with OS error 5"
        );
    });
}

/// A reader that falls three windows behind gets its frames past the window from
/// disk, in order, and then the live ones.
#[test]
fn slow_reader_gets_every_frame_from_disk() {
    run(10, |test| async move {
        let mut taker = test.reader(&["value"], Mode::Complete).await;
        let mut lagger = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        let mut bytes = 0;
        for n in 0..400 {
            write_wide(&mut writer, now, n);
            bytes += charge(&taker.next().await.expect("a frame").view());
            if n < 2 {
                lagger.next().await.expect("a frame");
            }
        }
        assert!(bytes > 3 * WINDOW, "{bytes} bytes are past three windows");
        let caught = take_waiting(&test.clock, &mut lagger).await;
        let ranges: Vec<_> = caught.iter().map(|&(range, _)| range).collect();
        assert_eq!(ranges, wide(2, 398));
        write_wide(&mut writer, now, 400);
        let received = lagger.next().await.expect("a live frame");
        assert_eq!(received.view().range(0), wide(400, 1).first().copied());
    });
}

#[test]
fn gives_a_reader_on_some_channels_of_a_frame_those_and_their_index() {
    run(20, |test| async move {
        let mut one = test.reader(&["value-c"], Mode::Complete).await;
        let mut both = test.reader(&["value-c", "value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value", "value-c"]).await;
        let now = test.now();
        write_series(&mut writer, &[(1, &[now]), (2, &[7]), (5, &[9])]);
        let received = both.next().await.expect("a frame");
        assert_eq!(keys(&received), [1, 5, 2], "in the key set's order");
        assert_eq!(samples(&received, 1), [now]);
        assert_eq!(samples(&received, 2), [7]);
        assert_eq!(samples(&received, 5), [9]);
        let received = one.next().await.expect("a frame");
        assert_eq!(keys(&received), [1, 5]);
        assert_eq!(samples(&received, 1), [now]);
        assert_eq!(samples(&received, 5), [9]);
    });
}

#[test]
fn gives_a_reader_the_index_of_a_frame_without_its_channels() {
    run(21, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["time"]).await;
        let now = test.now();
        write_series(&mut writer, &[(1, &[now])]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(keys(&received), [1]);
        assert_eq!(samples(&received, 1), [now]);
        let range = Range { seq: 0, count: 1 };
        assert_eq!(received.view().range(0), Some(range));
    });
}

#[test]
fn releases_the_lent_frame_at_the_next_call() {
    run(22, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        write(&mut writer, &[now], &[0]);
        test.clock.sleep(SETTLE).await;
        reader.next().await.expect("a frame");
        write(&mut writer, &[now + 1], &[1]);
        let held = test.free();
        assert!(poll_once(reader.next()).is_pending());
        assert_eq!(test.free(), held + 1, "the lent frame is released");
    });
}

#[test]
fn keeps_a_complete_reader_that_called_next_before_a_commit_under_a_window() {
    run_on(1, (WIDE_AREA, WIDE_BODY_MAX), |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        write_samples(&mut writer, now, 80_000);
        let first = charge(&reader.next().await.expect("a frame").view());
        assert!(first < WINDOW, "{first} bytes are under the window");
        let a = {
            let next = reader.next();
            write_samples(&mut writer, now + 80_000, 50_000);
            write_samples(&mut writer, now + 130_000, 1);
            test.clock.sleep(SETTLE).await;
            charge(&next.await.expect("a frame").view())
        };
        assert!(
            a + 4096 < WINDOW,
            "{a} bytes and one sample are under the window"
        );
        let b = reader.next().await.map(|received| charge(&received.view()));
        assert_eq!(b.map(|b| b < 4096), Ok(true), "first {first}, a {a}");
    });
}

#[test]
fn keeps_a_complete_reader_that_takes_a_commit_of_more_than_a_window() {
    run_on(1, (WIDE_AREA, WIDE_BODY_MAX), |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        write_samples(&mut writer, now, 80_000);
        write_samples(&mut writer, now + 80_000, 80_000);
        test.clock.sleep(SETTLE).await;
        let first = charge(&reader.next().await.expect("a frame").view());
        let second = charge(&reader.next().await.expect("a frame").view());
        assert!(
            first + second > WINDOW,
            "{first} + {second} bytes pass a window"
        );
        write_samples(&mut writer, now + 160_000, 1);
        test.clock.sleep(SETTLE).await;
        let third = reader.next().await.map(|received| charge(&received.view()));
        assert_eq!(third.map(|third| third < 4096), Ok(true));
    });
}

#[test]
fn releases_the_lent_frame_of_a_latest_reader_at_the_next_call() {
    run(22, |test| async move {
        let mut latest = test.reader(&["value"], Mode::Latest).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        write(&mut writer, &[now], &[0]);
        latest.next().await.expect("a frame");
        let mut next = pin!(latest.next());
        write(&mut writer, &[now + 1], &[1]);
        test.clock.sleep(SETTLE).await;
        let held = test.free();
        assert!(poll_once(next.as_mut()).is_ready());
        assert_eq!(test.free(), held, "the reader held no older frame");
    });
}

#[test]
fn writes_and_reads_a_channel_of_a_variable_type() {
    run(19, |test| async move {
        test.define([(6, "text", DataType::Sample(Type::String), 1)])
            .await;
        let mut reader = test.reader(&["text"], Mode::Complete).await;
        let mut writer = test.writer("a", &["text"]).await;
        let now = test.now();
        let set = Arc::clone(writer.set());
        let (index, text) = (entry(&set, 1), entry(&set, 6));
        let form = codec::Variable::of(Type::String).expect("a variable type");
        let series = [(index, 8), (text, form.len(&["hi"]).expect("a short text"))];
        let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
        draft
            .series_mut(index)
            .expect("the index")
            .copy_from_slice(&now.to_le_bytes());
        form.write(&["hi"], draft.series_mut(text).expect("the text"));
        draft.set_count(0, 1);
        let written = writer
            .write(LIVE, draft)
            .expect("the home takes it")
            .to_vec();
        assert_eq!(written, [applied(0)]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 1), [now]);
        let (_, bytes) = received
            .view()
            .iter()
            .find(|&(present, _)| present == text)
            .expect("the view holds the text");
        let mut out = Vec::new();
        codec::decode(Type::String, 1, bytes, &mut out).expect("decodes");
        assert!(form.samples(1, &out).eq([b"hi"]));
    });
}

#[test]
fn refuses_a_string_sample_that_is_not_utf8() {
    run(19, |test| async move {
        test.define([(6, "text", DataType::Sample(Type::String), 1)])
            .await;
        let mut writer = test.writer("a", &["text"]).await;
        let now = test.now();
        let set = Arc::clone(writer.set());
        let (index, text) = (entry(&set, 1), entry(&set, 6));
        let form = codec::Variable::of(Type::String).expect("a variable type");
        let samples = [[0xff, 0xfe]];
        let series = [
            (index, 8),
            (text, form.len(&samples).expect("a short text")),
        ];
        let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
        draft
            .series_mut(index)
            .expect("the index")
            .copy_from_slice(&now.to_le_bytes());
        form.write(&samples, draft.series_mut(text).expect("the text"));
        draft.set_count(0, 1);
        let written = writer
            .write(LIVE, draft)
            .expect("the home takes it")
            .to_vec();
        let refusal = Refusal::Codec {
            channel: channel::Key::from_u128(6),
            error: codec::Error::Utf8 { sample: 0 },
        };
        assert_eq!(
            refusal.to_string(),
            "channel 00000000-0000-0000-0000-000000000006: sample 0 is not UTF-8"
        );
        let refused = Outcome::Refused {
            slot: Slot::new(0),
            refusal,
        };
        assert_eq!(written, [refused], "A9: a string is UTF-8");
    });
}

#[test]
fn opens_a_writer_on_each_channel_once_with_its_index() {
    run(12, |test| async move {
        let channels = ["value", "time", "value", "value-b"];
        let writer = test.writer("a", &channels).await;
        let entries = writer.set().entries();
        let keys: Vec<_> = entries.iter().map(|entry| entry.key.as_u128()).collect();
        assert_eq!(keys, [1, 3, 2, 4]);
        let groups: Vec<_> = entries.iter().map(|entry| entry.group).collect();
        assert_eq!(groups, [0, 1, 0, 1]);
    });
}

/// The channel `key` of the spec: an index when `index` is `key`, else a data channel
/// of `data_type` on `index`.
fn definition(key: u128, data_type: DataType, index: u128) -> Definition {
    let kind = if key == index {
        Kind::Index {
            error: None,
            control: None,
        }
    } else {
        let index = channel::Key::from_u128(index);
        Kind::Data(Data::new(index, None, data_type, None).expect("no unit"))
    };
    Definition::Channel(spec::channel::Channel {
        key: channel::Key::from_u128(key),
        kind,
    })
}

/// Sets [`CHANNELS`] and `more`, each `(key, name, index)` and of `I64`, as the
/// definitions, in one call. A `Vec`, not a map, so that a name may come twice.
fn define(more: Vec<(u128, &'static str, u128)>) {
    run(18, move |test| async move {
        let more = more.into_iter().map(|(key, channel, index)| {
            (name(channel), definition(key, DataType::Sample(I64), index))
        });
        let channels: Vec<_> = channels().into_iter().chain(more).collect();
        test.hub
            .set_definitions(channels.iter().map(|(name, channel)| (name, channel)));
    });
}

#[test]
#[should_panic(expected = "two channels have key 00000000-0000-0000-0000-000000000002")]
fn define_panics_on_a_key_twice() {
    define(vec![(2, "other", 1)]);
}

#[test]
#[should_panic(expected = "two channels are named value")]
fn define_panics_on_a_name_twice() {
    define(vec![(9, "value", 1)]);
}

#[test]
#[should_panic(
    expected = "the index 00000000-0000-0000-0000-000000000002 of channel other is not an index of the definitions"
)]
fn define_panics_on_an_index_that_is_a_data_channel() {
    define(vec![(9, "other", 2)]);
}

#[test]
#[should_panic(
    expected = "the index 00000000-0000-0000-0000-000000000008 of channel other is not an index of the definitions"
)]
fn define_panics_on_an_unknown_index() {
    define(vec![(9, "other", 8)]);
}

#[test]
#[should_panic(expected = "two channels are named other")]
fn define_panics_on_a_new_name_twice() {
    define(vec![(9, "other", 1), (10, "other", 1)]);
}

/// Name order puts a data channel before its index.
#[test]
fn defines_a_data_channel_before_its_index_in_one_call() {
    run(18, |test| async move {
        let (temp, time) = (name("plant.temp"), name("plant.time"));
        let channels = [
            (&temp, &definition(7, DataType::Sample(I64), 6)),
            (&time, &definition(6, DataType::Sample(STAMP), 6)),
        ];
        test.hub.set_definitions(channels);
        let writer = test.writer("a", &["plant.temp"]).await;
        let keys: Vec<_> = writer.set().entries().iter().map(|e| e.key).collect();
        assert_eq!(keys, [6, 7].map(channel::Key::from_u128));
    });
}

#[test]
fn defines_each_channel_and_no_other_kind_of_definition() {
    run(2, |test| async move {
        let select = types::name::Selector::new(["plant.**"]).expect("a selector");
        let policy = spec::time::Policy::new(select, spec::time::Peers::Voters);
        let (other, temp, time) =
            (name("plant.clock"), name("plant.temp"), name("plant.time"));
        test.hub.set_definitions([
            (&other, &Definition::Time(policy)),
            (&temp, &definition(7, DataType::Sample(I64), 6)),
            (&time, &definition(6, DataType::Sample(STAMP), 6)),
        ]);
        let writer = test.writer("a", &["plant.temp"]).await;
        let keys: Vec<_> = writer.set().entries().iter().map(|e| e.key).collect();
        assert_eq!(keys, [6, 7].map(channel::Key::from_u128));
        let writer = test.hub.writer(config("a", &["plant.clock"])).await;
        let error = writer.expect_err("an error");
        assert_eq!(error, writer::Error::Unknown(other));
    });
}

/// The quality, error, and control edges point at keys that the hub does not know.
#[test]
fn defines_channels_with_edges_it_does_not_read() {
    run(18, |test| async move {
        let unknown = || Some(channel::Key::from_u128(9));
        let time = spec::channel::Channel {
            key: channel::Key::from_u128(6),
            kind: Kind::Index {
                error: unknown(),
                control: unknown(),
            },
        };
        let index = channel::Key::from_u128(6);
        let data = Data::new(index, unknown(), DataType::Sample(I64), None);
        let temp = spec::channel::Channel {
            key: channel::Key::from_u128(7),
            kind: Kind::Data(data.expect("no unit")),
        };
        let (temp_name, time_name) = (name("plant.temp"), name("plant.time"));
        let (temp, time) = (Definition::Channel(temp), Definition::Channel(time));
        test.hub
            .set_definitions([(&temp_name, &temp), (&time_name, &time)]);
        let writer = test.writer("a", &["plant.temp"]).await;
        let keys: Vec<_> = writer.set().entries().iter().map(|e| e.key).collect();
        assert_eq!(keys, [6, 7].map(channel::Key::from_u128));
    });
}

#[test]
fn gives_a_writer_the_sample_type_of_each_data_channel() {
    run(18, |test| async move {
        test.define([
            (6, "text", DataType::Sample(Type::String), 1),
            (7, "quality", DataType::Quality, 1),
        ])
        .await;
        let writer = test.writer("a", &["text", "quality"]).await;
        let entries: Vec<_> = writer
            .set()
            .entries()
            .iter()
            .map(|e| (e.key.as_u128(), e.data_type))
            .collect();
        let quality = DataType::Quality.sample();
        assert_eq!(entries, [(1, STAMP), (6, Type::String), (7, quality)]);
        assert_eq!(quality, Type::Scalar(Scalar::U32));
    });
}

#[test]
fn gives_a_reader_the_key_set_of_each_frame() {
    run(13, |test| async move {
        let mut reader = test.reader(&["time"], Mode::Complete).await;
        let mut a = test.writer("a", &["value"]).await;
        let now = test.now();
        write(&mut a, &[now], &[1]);
        let set = Arc::clone(a.set());
        drop(a);
        let mut b = test.writer("b", &["time"]).await;
        let mut draft = b.draft(Form::Raw, &[(0, 8)]).expect("a frame");
        draft
            .series_mut(0)
            .expect("the index")
            .copy_from_slice(&(now + 1).to_le_bytes());
        draft.set_count(0, 1);
        b.write(LIVE, draft).expect("the home takes it");
        let received = reader.next().await.expect("a frame");
        assert!(Arc::ptr_eq(received.set(), &set));
        let received = reader.next().await.expect("a frame");
        assert!(Arc::ptr_eq(received.set(), b.set()));
        assert_eq!(samples(&received, 1), [now + 1]);
    });
}

/// The simulator has no task budget, so only the reader's own yield lets the other
/// task run. The simulator picks a ready task at random, so the frames span several
/// yields.
#[test]
fn yields_to_other_tasks_of_the_shard_while_frames_wait() {
    run(11, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        let frames = 1000;
        for n in 0..frames {
            write(&mut writer, &[now + n], &[n]);
        }
        reader.next().await.expect("a frame");
        let taken = Rc::new(Cell::new(1));
        let other = Rc::new(Cell::new(None));
        let (seen, ran) = (Rc::clone(&taken), Rc::clone(&other));
        test.tasks.spawn(async move { ran.set(Some(seen.get())) });
        for n in 1..frames {
            reader.next().await.expect("a frame");
            taken.set(n + 1);
        }
        let other = other.get().expect("the other task ran");
        assert!(other < frames, "the other task ran after {other} frames");
    });
}

/// A commit task that loops never yields, so the run does not end.
#[test]
fn waits_for_no_commit_in_a_loop_after_the_only_complete_reader_closes() {
    run(14, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        write(&mut writer, &[now], &[0]);
        reader.next().await.expect("a frame");
        drop(reader);
        for n in 1..4 {
            assert_eq!(
                write(&mut writer, &[now + n], &[n]),
                [applied(n.cast_unsigned())]
            );
            test.clock.sleep(SETTLE).await;
        }
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        write(&mut writer, &[now + 4], &[4]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(received.view().range(0), Some(Range { seq: 4, count: 1 }));
    });
}

/// A commit task that loops never yields, so the run does not end.
#[test]
fn waits_for_no_commit_in_a_loop_while_the_only_complete_reader_is_out_of_credit() {
    run(15, |test| async move {
        let mut lagger = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        let frames = 200;
        for n in 0..frames {
            write_wide(&mut writer, now, n);
            test.clock.sleep(SETTLE).await;
        }
        let taken = take_waiting(&test.clock, &mut lagger).await;
        let ranges: Vec<_> = taken.iter().map(|&(range, _)| range).collect();
        assert_eq!(ranges, wide(0, 200));
        let spent: u64 = taken.iter().map(|&(_, charge)| charge).sum();
        assert!(WINDOW <= spent, "{spent} bytes reach the window");
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        write_wide(&mut writer, now, frames);
        let received = reader.next().await.expect("a frame");
        let range = Range {
            seq: frames.cast_unsigned() * 1000,
            count: 1000,
        };
        assert_eq!(received.view().range(0), Some(range));
    });
}

/// A waker that notes that it was woken.
#[derive(Default)]
struct Flag(AtomicBool);

/// What a frame of one group with only the series of `view` charges, as a reader
/// that holds each series of the frame spends ([`frame::charge`]).
fn charge(view: &View<'_>) -> u64 {
    let lens = view.iter().map(|(_, series)| ((), series.len()));
    let (count, end) =
        frame::ends(lens).fold((0, 0), |(count, _), ((), end)| (count + 1, end));
    frame::charge(count, end)
}

/// Takes each frame of `reader` until it ends: their charges, and how it ended.
async fn take_all(reader: &mut Reader) -> (Vec<u64>, Ended) {
    let mut charges = Vec::new();
    loop {
        match reader.next().await {
            Ok(received) => charges.push(charge(&received.view())),
            Err(ended) => return (charges, ended),
        }
    }
}

/// Takes each frame of `reader` until none comes within [`SETTLE`] of `clock`: the
/// range of each, and its charge.
async fn take_waiting(clock: &Clock, reader: &mut Reader) -> Vec<(Range, u64)> {
    let mut taken = Vec::new();
    while let Some(next) = within(clock, SETTLE, reader.next()).await {
        let received = next.expect("a frame");
        let range = received.view().range(0).expect("the index is present");
        taken.push((range, charge(&received.view())));
    }
    taken
}

/// The ranges of `count` frames of 1000 samples from frame `first`.
fn wide(first: u64, count: u64) -> Vec<Range> {
    (first..first + count)
        .map(|n| Range {
            seq: n * 1000,
            count: 1000,
        })
        .collect()
}

/// Polls `next` once with a waker that sets a flag, and returns the flag.
fn poll_flagged<F: Future>(next: Pin<&mut F>) -> (Poll<F::Output>, Arc<Flag>) {
    let flag = Arc::new(Flag::default());
    let waker = Waker::from(Arc::clone(&flag));
    (next.poll(&mut Context::from_waker(&waker)), flag)
}

#[test]
fn gives_a_complete_reader_that_holds_a_frame_past_its_window_each_later_frame() {
    races(32, (WIDE_AREA, WIDE_BODY_MAX), |seed, test| async move {
        let commits = (seed % 2 + 1).cast_signed();
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        let first = test
            .race(|| write_samples(&mut writer, now, PAST_WINDOW))
            .await;
        let charge = charge(&reader.next().await.expect("a frame").view());
        assert!(charge > WINDOW, "{charge} bytes spend the window");
        // The reader holds the frame, so the next waits for credit.
        for n in 0..commits {
            write_samples(&mut writer, now + PAST_WINDOW + n, 1);
            test.clock.sleep(SETTLE).await;
        }
        // With two commits, the reader missed the frame of the first and reads it from
        // disk.
        let ranges = take_waiting(&test.clock, &mut reader).await;
        let seq = PAST_WINDOW.cast_unsigned();
        let expected: Vec<_> = (seq..seq + commits.cast_unsigned())
            .map(|seq| Range { seq, count: 1 })
            .collect();
        let ranges: Vec<_> = ranges.into_iter().map(|(range, _)| range).collect();
        assert_eq!(ranges, expected);
        first
    });
}

#[test]
fn gives_a_waiting_complete_reader_each_frame_of_a_commit_past_its_window() {
    races(32, (AREA, BODY_MAX), |_, test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        let first = test
            .race(|| {
                for n in 0..200 {
                    write_wide(&mut writer, now, n);
                }
            })
            .await;
        let mut spent = 0;
        for n in 0..200 {
            let received = reader.next().await.expect("a frame");
            let range = received.view().range(0).expect("the index is present");
            assert_eq!(
                range,
                Range {
                    seq: n * 1000,
                    count: 1000
                }
            );
            spent += charge(&received.view());
        }
        assert!(spent > WINDOW, "{spent} bytes pass the window");
        let (polled, _) = poll_flagged(pin!(reader.next()));
        assert!(polled.is_pending(), "no frame waits");
        first
    });
}

#[test]
fn gives_a_complete_reader_its_waiting_frames_then_the_missed_ones_from_disk() {
    races(32, (AREA, BODY_MAX), |_, test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        for n in 0..20 {
            write_wide(&mut writer, now, n);
        }
        test.clock.sleep(SETTLE).await;
        let frame = reader.next().await.expect("a frame");
        let range = frame.view().range(0).expect("the index is present");
        assert_eq!(
            range,
            Range {
                seq: 0,
                count: 1000
            }
        );
        for n in 20..200 {
            write_wide(&mut writer, now, n);
        }
        // The frames past the window go to disk at this commit.
        let first = test.race(|| write_wide(&mut writer, now, 200)).await;
        let taken = take_waiting(&test.clock, &mut reader).await;
        let ranges: Vec<_> = taken.into_iter().map(|(range, _)| range).collect();
        assert_eq!(ranges, wide(1, 200));
        first
    });
}

/// A reader that sleeps while a writer writes one frame each second, and a read holds
/// the first, gets each frame that the hub applied: frames of 128 samples pass the
/// window before they fill the ring, and come later from disk. Frames of 1 sample
/// fill the ring first. The frames lost for no room end nothing.
#[test]
fn gives_a_complete_reader_that_sleeps_past_its_window_or_its_ring_each_applied_frame()
{
    for samples in [128, 1] {
        run(21, move |test| async move {
            let mut reader = test.reader(&["value"], Mode::Complete).await;
            let mut writer = test.writer("a", &["value"]).await;
            let mut held = None;
            let mut outcomes = Vec::new();
            for _ in 0..1_100 {
                let stamps: Vec<_> = (test.now()..).take(samples).collect();
                outcomes.extend(write(&mut writer, &stamps, &scrambled(&stamps)));
                test.clock.sleep(Span::SECOND).await;
                if held.is_none() {
                    held = Some(hub::testing::hold(&test.hub, Slot::new(0)));
                }
            }
            let applied = outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Outcome::Applied { .. }))
                .count();
            let mut taken = 0;
            while let Some(received) =
                within(&test.clock, Span::SECOND, reader.next()).await
            {
                received.unwrap_or_else(|ended| {
                    panic!("{samples} samples a frame: {ended} after {taken} frames")
                });
                taken += 1;
            }
            assert_eq!(taken, applied, "{samples} samples a frame");
            assert!(applied < 1_100, "{applied} applied");
        });
    }
}

#[test]
fn gives_a_complete_reader_the_samples_lost_for_no_room_as_the_gap_of_the_next_frame() {
    run(110, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        write(&mut writer, &[test.now()], &[7]);
        test.clock.sleep(SETTLE).await;
        let mut blocks = fill(&test.pool);
        drop(blocks.pop());
        let draft = draft(&writer, &[(1, &[test.now(), test.now() + 1]), (2, &[0, 0])]);
        let outcomes = writer.write(LIVE, draft).expect("taken").to_vec();
        let [Outcome::Lost { range, .. }] = outcomes[..] else {
            panic!("{outcomes:?}");
        };
        let lost = range.seq..range.end().expect("an end");
        drop(blocks);
        test.clock.sleep(SETTLE).await;
        write(&mut writer, &[test.now()], &[8]);
        write(&mut writer, &[test.now() + 1], &[9]);
        test.clock.sleep(SETTLE).await;
        for (values, gap) in [(7, None), (8, Some(lost)), (9, None)] {
            let received = reader.next().await.expect("a frame");
            assert_eq!(samples(&received, 2), [values]);
            assert_eq!(received.gap(), gap, "the gap before {values}");
        }
    });
}

#[test]
fn gives_a_latest_reader_that_skips_frames_no_gap() {
    run(111, |test| async move {
        let mut latest = test.reader(&["value"], Mode::Latest).await;
        let mut writer = test.writer("a", &["value"]).await;
        write(&mut writer, &[test.now()], &[7]);
        test.clock.sleep(SETTLE).await;
        let received = latest.next().await.expect("a frame");
        assert_eq!((samples(&received, 2), received.gap()), (vec![7], None));
        write(&mut writer, &[test.now()], &[8]);
        write(&mut writer, &[test.now() + 1], &[9]);
        test.clock.sleep(SETTLE).await;
        let received = latest.next().await.expect("a frame");
        assert_eq!((samples(&received, 2), received.gap()), (vec![9], None));
    });
}

/// What `future` gives within `span` of `clock`, or `None`.
async fn within<F: Future>(clock: &Clock, span: Span, future: F) -> Option<F::Output> {
    let (mut future, mut sleep) = (pin!(future), pin!(clock.sleep(span)));
    std::future::poll_fn(|cx| {
        if let Poll::Ready(output) = future.as_mut().poll(cx) {
            return Poll::Ready(Some(output));
        }
        sleep.as_mut().poll(cx).map(|()| None)
    })
    .await
}

#[test]
fn ends_a_complete_reader_that_missed_a_frame_with_the_error_of_a_failed_sync() {
    run(20, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        for n in 0..201 {
            write_wide(&mut writer, now, n);
            // The frames past the window miss at the commit of frame 200.
            if n >= 199 {
                test.clock.sleep(SETTLE).await;
            }
        }
        let mut latest = test.reader(&["value"], Mode::Latest).await;
        test.node.fail_file(FilePath::new(RING), Operation::Sync);
        write_wide(&mut writer, now, 201);
        test.clock.sleep(SETTLE).await;
        let failed = Ended::Buffer(env::files::Error::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        });
        let (_, ended) = take_all(&mut latest).await;
        assert_eq!(ended, failed);
        let (charges, ended) = take_all(&mut reader).await;
        assert_eq!(ended, failed);
        assert!(
            charges.len() < 200,
            "{} frames before the read",
            charges.len()
        );
        assert_eq!(reader.next().await.err(), Some(failed));
    });
}

/// Writes 201 wide frames, so the frames past the window of `reader`, which takes
/// none, miss at the commit of frame 200 and wait on disk.
async fn write_past_the_window(test: &Test, writer: &mut Writer) {
    let now = test.now();
    for n in 0..201 {
        write_wide(writer, now, n);
        if n >= 199 {
            test.clock.sleep(SETTLE).await;
        }
    }
}

/// A read from disk that fails ends the complete reader with its error, on each later
/// call too. The shard goes on.
#[test]
fn ends_a_complete_reader_whose_read_from_disk_fails_with_its_error() {
    run(62, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        write_past_the_window(&test, &mut writer).await;
        test.node.fail_file(FilePath::new(RING), Operation::ReadAt);
        let (charges, ended) = take_all(&mut reader).await;
        let failed = Ended::Buffer(env::files::Error::Io {
            path: PathBuf::from(RING),
            operation: Operation::ReadAt,
            code: 5,
        });
        assert_eq!(ended, failed);
        assert!(charges.len() < 200, "{} frames", charges.len());
        assert_eq!(reader.next().await.err(), Some(failed));
        let mut latest = test.reader(&["value"], Mode::Latest).await;
        write_wide(&mut writer, test.now(), 0);
        latest.next().await.expect("a frame");
    });
}

/// A read from disk that finds no block in the pool ends the complete reader with
/// the pool's error, on each later call too.
#[test]
fn ends_a_complete_reader_whose_read_from_disk_finds_no_block_with_the_pool_error() {
    run(63, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        write_past_the_window(&test, &mut writer).await;
        let mut blocks = Vec::new();
        let ended = loop {
            let next = reader.next();
            blocks.extend(fill(&test.pool));
            if let Err(ended) = next.await {
                break ended;
            }
        };
        let exhausted = Ended::Pool(block::Error::Exhausted {
            requested: 4096,
            available: 0,
        });
        assert_eq!(ended, exhausted);
        drop(blocks);
        assert_eq!(reader.next().await.err(), Some(exhausted));
    });
}

/// A read from disk that gets its entry but finds no block for the frame of the
/// entry ends the complete reader with the pool's error, on each later call too.
#[test]
fn ends_a_complete_reader_whose_frame_from_disk_finds_no_block_with_the_pool_error() {
    run(64, |test| async move {
        // The bytes of one entry. The frame of the entry needs 8080 more.
        const ENTRY: usize = 8088;
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        write_past_the_window(&test, &mut writer).await;
        let flag = Arc::new(Flag::default());
        let waker = Waker::from(Arc::clone(&flag));
        let mut next = Box::pin(reader.next());
        loop {
            flag.0.store(false, Ordering::Relaxed);
            match next.as_mut().poll(&mut Context::from_waker(&waker)) {
                Poll::Ready(received) => {
                    received.expect("a frame");
                    drop(next);
                    next = Box::pin(reader.next());
                }
                Poll::Pending if flag.0.load(Ordering::Relaxed) => {}
                // No wake yet: the read from disk holds its table and waits on it.
                Poll::Pending => break,
            }
        }
        let room = test.pool.alloc(ENTRY).expect("a block");
        let blocks = fill(&test.pool);
        drop(room);
        let exhausted = Ended::Pool(block::Error::Exhausted {
            requested: 8080,
            available: 64,
        });
        assert_eq!(next.await.err().as_ref(), Some(&exhausted));
        drop(blocks);
        assert_eq!(reader.next().await.err(), Some(exhausted));
    });
}

impl Wake for Flag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[test]
fn wakes_a_latest_reader_that_waits_in_the_write() {
    run(17, |test| async move {
        let mut latest = test.reader(&["value"], Mode::Latest).await;
        let mut writer = test.writer("a", &["value"]).await;
        let flag = Arc::new(Flag::default());
        let waker = Waker::from(Arc::clone(&flag));
        let mut next = pin!(latest.next());
        let polled = next.as_mut().poll(&mut Context::from_waker(&waker));
        assert!(polled.is_pending());
        write(&mut writer, &[test.now()], &[1]);
        assert!(flag.0.load(Ordering::Relaxed), "the write wakes the reader");
    });
}

#[test]
fn gives_a_reader_the_error_of_a_failed_sync_of_a_handoff() {
    for closed in [false, true] {
        run(16, move |test| async move {
            let mut complete = test.reader(&["value"], Mode::Complete).await;
            let writer = if closed {
                let writer = test.writer("a", &["value"]).await;
                test.clock.sleep(SETTLE).await;
                test.node.fail_file(FilePath::new(RING), Operation::Sync);
                drop(writer);
                None
            } else {
                test.node.fail_file(FilePath::new(RING), Operation::Sync);
                Some(test.writer("a", &["value"]).await)
            };
            test.clock.sleep(SETTLE).await;
            let failed = Ended::Buffer(env::files::Error::Io {
                path: PathBuf::from(RING),
                operation: Operation::Sync,
                code: 5,
            });
            let next = poll_once(complete.next()).map(Result::err);
            assert_eq!(next, Poll::Ready(Some(failed)), "closed: {closed}");
            drop(writer);
        });
    }
}

/// The handoff at the end of a lease, with no call, reaches the commit task.
#[test]
fn gives_a_reader_the_error_of_a_failed_sync_of_the_handoff_at_the_end_of_a_lease() {
    run(16, |test| async move {
        let mut complete = test.reader(&["value"], Mode::Complete).await;
        let leased = writer::Config {
            lease: Some(Span::from_nanos(50_000_000)),
            ..config("subject-a", &["value"])
        };
        let writer = test.hub.writer(leased).await.expect("opens");
        test.clock.sleep(SETTLE).await;
        test.node.fail_file(FilePath::new(RING), Operation::Sync);
        test.clock.sleep(Span::from_nanos(60_000_000)).await;
        let failed = Ended::Buffer(env::files::Error::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        });
        let next = poll_once(complete.next()).map(Result::err);
        assert_eq!(next, Poll::Ready(Some(failed)));
        drop(writer);
    });
}

/// The handoff of a writer that a removal closes reaches the commit.
#[test]
fn gives_a_reader_the_error_of_a_failed_sync_of_the_handoff_of_a_removal() {
    run(16, |test| async move {
        let mut complete = test.reader(&["value-c"], Mode::Complete).await;
        let writer = test.writer("a", &["value"]).await;
        test.clock.sleep(SETTLE).await;
        test.node.fail_file(FilePath::new(RING), Operation::Sync);
        test.hub.set_definitions(&without(&["value"]));
        test.clock.sleep(SETTLE).await;
        let failed = Ended::Buffer(env::files::Error::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        });
        let next = poll_once(complete.next()).map(Result::err);
        assert_eq!(next, Poll::Ready(Some(failed)));
        drop(writer);
    });
}

#[test]
fn gives_a_reader_the_error_of_a_failed_sync_of_a_handoff_in_a_failed_write() {
    run(18, |test| async move {
        let mut complete = test.reader(&["value"], Mode::Complete).await;
        let blocks = fill(&test.pool);
        // The handoff finds no block, so it waits for the next write.
        let mut writer = test.writer("a", &["value"]).await;
        drop(blocks);
        test.clock.sleep(SETTLE).await;
        test.node.fail_file(FilePath::new(RING), Operation::Sync);
        let now = test.now();
        let stamps: Vec<i64> = (0..10_000).map(|s| now + s).collect();
        let values: Vec<i64> = stamps
            .iter()
            .map(|&s| {
                let x = s.wrapping_mul(6_364_136_223_846_793_005);
                x ^ (x >> 29)
            })
            .collect();
        let set = writer.set();
        let (time, value) = (entry(set, 1), entry(set, 2));
        let group = set.entries()[time].group;
        let series = [(time, stamps.len() * 8), (value, values.len() * 8)];
        let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
        for (entry, samples) in [(time, &stamps), (value, &values)] {
            let bytes = draft.series_mut(entry).expect("the series is present");
            for (bytes, sample) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(samples)
            {
                *bytes = sample.to_le_bytes();
            }
        }
        draft.set_count(group, 10_000);
        let written = writer.write(LIVE, draft).map(<[_]>::to_vec);
        assert_eq!(written, Err(Failure::Home(hub::home::Error::Large)));
        test.clock.sleep(SETTLE).await;
        let failed = Ended::Buffer(env::files::Error::Io {
            path: PathBuf::from(RING),
            operation: Operation::Sync,
            code: 5,
        });
        let next = poll_once(complete.next()).map(Result::err);
        assert_eq!(next, Poll::Ready(Some(failed)));
    });
}

#[test]
fn opens_a_reader_at_the_first_poll() {
    run(19, |test| async move {
        let mut writer = test.writer("a", &["value"]).await;
        let names = ["value"];
        let opening = test.hub.reader(unnamed(&names, Mode::Complete));
        write(&mut writer, &[test.now()], &[1]);
        let mut complete = opening.await.expect("opens");
        test.clock.sleep(SETTLE).await;
        assert!(poll_once(complete.next()).is_pending());
    });
}

#[test]
fn keeps_a_complete_reader_that_gave_back_each_frame_through_a_commit_under_a_window() {
    run_on(1, (WIDE_AREA, WIDE_BODY_MAX), |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        write_samples(&mut writer, now, 80_000);
        let first = charge(&reader.next().await.expect("a frame").view());
        assert!(first < WINDOW, "{first} bytes are under the window");
        let a = {
            let mut next = pin!(reader.next());
            assert!(poll_once(next.as_mut()).is_pending(), "no frame waits");
            write_samples(&mut writer, now + 80_000, 50_000);
            write_samples(&mut writer, now + 130_000, 1);
            test.clock.sleep(SETTLE).await;
            charge(&next.await.expect("a frame").view())
        };
        assert!(
            a + 4096 < WINDOW,
            "{a} bytes and one sample are under the window"
        );
        let b = reader.next().await.map(|received| charge(&received.view()));
        assert_eq!(b.map(|b| b < 4096), Ok(true), "first {first}, a {a}");
    });
}

/// Polls `reader.next()` once with `waker`, and says whether it gave a frame.
fn poll_next(reader: &mut Reader, waker: &Waker) -> bool {
    let mut next = pin!(reader.next());
    match next.as_mut().poll(&mut Context::from_waker(waker)) {
        Poll::Ready(received) => {
            received.expect("a frame");
            true
        }
        Poll::Pending => false,
    }
}

#[test]
fn starts_a_new_streak_after_a_wait() {
    run(31, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        write(&mut writer, &[now], &[0]);
        test.clock.sleep(SETTLE).await;
        let waker = Waker::noop();
        assert!(poll_next(&mut reader, waker), "the first frame waits");
        assert!(!poll_next(&mut reader, waker), "no frame waits");
        for n in 1..=128 {
            write(&mut writer, &[now + n], &[n]);
        }
        test.clock.sleep(SETTLE).await;
        let given = (0..129)
            .take_while(|_| poll_next(&mut reader, waker))
            .count();
        assert_eq!(given, 128, "a wait ends the streak");
    });
}

#[test]
fn keeps_no_waker_of_a_dropped_reader() {
    run(32, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let flag = Arc::new(Flag::default());
        let waker = Waker::from(Arc::clone(&flag));
        assert!(!poll_next(&mut reader, &waker), "no frame waits");
        drop(waker);
        drop(reader);
        assert_eq!(Arc::strong_count(&flag), 1, "the hub keeps the waker");
    });
}

/// A local complete reader spends the whole frame, not only its channels: it misses
/// live frames that fit its window by its own channels, and reads them from disk.
#[test]
fn charges_a_local_complete_reader_on_some_channels_for_the_whole_frame() {
    const SAMPLES: i64 = 1000;
    run(16, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value", "value-c"]).await;
        let now = test.now();
        let frames = 100;
        for n in 0..frames {
            let stamps: Vec<_> = (now + n * SAMPLES..now + (n + 1) * SAMPLES).collect();
            let values: Vec<_> = stamps
                .iter()
                .map(|&s| {
                    let x = s.wrapping_mul(6_364_136_223_846_793_005);
                    x ^ (x >> 29)
                })
                .collect();
            write_series(&mut writer, &[(1, &stamps), (2, &values), (5, &values)]);
            test.clock.sleep(SETTLE).await;
        }
        let mut live = 0;
        while let Poll::Ready(next) = poll_once(reader.next()) {
            next.expect("a frame");
            live += 1;
        }
        assert!(live < frames, "{live} live frames, then a read from disk");
        let taken = take_waiting(&test.clock, &mut reader).await;
        let ranges: Vec<_> = taken.into_iter().map(|(range, _)| range.seq).collect();
        let seqs: Vec<_> = (live..frames).map(|n| n.cast_unsigned() * 1000).collect();
        assert_eq!(ranges, seqs);
    });
}
