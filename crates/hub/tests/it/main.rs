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
use hub::writer::{self, Writer};
use spec::channel::{Data, Kind};
use spec::data_type::DataType;
use spec::definition::Definition;
use types::authority::Authority;
use types::channel::{self, Slot};
use types::frame::key_set::Interner;
use types::frame::{self, Form, Label, Path, Range, View};
use types::name::Name;
use types::sample::{Scalar, Type};
use types::time::{Span, Stamp};

mod serve;

const DIR: &str = "shard-0";
const RING: &str = "shard-0/ring";
const AREA: u64 = 1 << 22;
const BODY_MAX: usize = 1 << 16;
/// The area and body max of a ring that takes a frame past the window of a reader.
const WIDE_AREA: u64 = 1 << 25;
const WIDE_BODY_MAX: usize = 1 << 22;
/// Samples per series of a frame whose charge is past the window of a reader.
const PAST_WINDOW: i64 = 140_000;
/// The pool reserves its budget once for each size class, 59 times here. With 8 tests
/// at once, a budget of 8 MiB (63 classes) went over the 4 GiB cap of a test process
/// in CI.
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

/// What one test gets: a hub on one shard, with [`CHANNELS`] defined.
struct Test {
    node: sim::node::Node,
    pool: Rc<Pool>,
    clock: Clock,
    mesh: clock::Reader,
    tasks: Tasks,
    /// How many tasks of the hub have ended.
    ended: Rc<Cell<usize>>,
    /// How many polls the hub's tasks have had.
    polls: Rc<Cell<usize>>,
    /// While set, the hub's tasks are not polled.
    paused: Rc<Pause>,
    /// The node's mesh clock until [`Test::sync`] runs it.
    unsynced: Option<clock::Clock>,
    /// A commit of the home, taken before the hub had it. It holds the ring open.
    commit: home::Commit,
    hub: Hub,
}

impl Test {
    /// A hub on a new ring of `node` with `layout`, whose mesh clock does not run yet.
    async fn new(node: sim::node::Node, tasks: Tasks, layout: buffer::Layout) -> Self {
        let config = block::Config { budget: POOL };
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
        let home = home::Shard::new(home::Config {
            shard: 0,
            buffer,
            clock: mesh.clone(),
            limits: LIMITS,
        });
        let commit = home.committed();
        let (ended, polls) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
        let paused = Rc::new(Pause::default());
        let hub = Hub::new(hub::Config {
            home,
            interner,
            tasks: Tasks::new(Counted {
                tasks: tasks.clone(),
                ended: Rc::clone(&ended),
                polls: Rc::clone(&polls),
                paused: Rc::clone(&paused),
            }),
        });
        let channels: BTreeMap<_, _> = CHANNELS
            .into_iter()
            .map(|(key, channel, data_type, index)| {
                let data_type = DataType::Sample(data_type);
                (name(channel), definition(key, data_type, index))
            })
            .collect();
        hub.define(&channels);
        Self {
            clock: node.clock(),
            node,
            pool,
            mesh,
            tasks,
            ended,
            polls,
            paused,
            unsynced: Some(unsynced),
            commit,
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

    async fn reader(&self, channels: &[&str], mode: Mode) -> reader::Reader {
        let names: Vec<_> = channels.iter().map(|n| name(n)).collect();
        self.hub.reader(&names, mode).await.expect("opens")
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

/// Spawns on `tasks`, and counts each poll in `polls` and each task that completes in
/// `ended`. Polls no task while `paused` is set.
struct Counted {
    tasks: Tasks,
    ended: Rc<Cell<usize>>,
    polls: Rc<Cell<usize>>,
    paused: Rc<Pause>,
}

/// Holds back the polls of the hub's tasks, as an executor that runs other tasks
/// first does.
#[derive(Default)]
struct Pause {
    /// The waker of each task woken while paused, when paused.
    held: std::cell::RefCell<Option<Vec<Waker>>>,
}

impl Pause {
    fn pause(&self) {
        *self.held.borrow_mut() = Some(Vec::new());
    }

    /// Wakes each task woken while paused.
    fn resume(&self) {
        let held = self.held.borrow_mut().take().unwrap_or_default();
        held.into_iter().for_each(Waker::wake);
    }

    /// Whether the task of `cx` waits, which holds its waker.
    fn holds(&self, cx: &Context<'_>) -> bool {
        let mut held = self.held.borrow_mut();
        held.as_mut()
            .map(|held| held.push(cx.waker().clone()))
            .is_some()
    }
}

impl env::tasks::Driver for Counted {
    fn spawn(&self, mut task: env::tasks::Task) {
        let (ended, polls) = (Rc::clone(&self.ended), Rc::clone(&self.polls));
        let paused = Rc::clone(&self.paused);
        self.tasks.spawn(async move {
            std::future::poll_fn(|cx| {
                if paused.holds(cx) {
                    return Poll::Pending;
                }
                polls.set(polls.get() + 1);
                task.as_mut().poll(cx)
            })
            .await;
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

/// Runs `main` as [`run`] does, on a ring of the area and body max in `ring`.
fn run_on<F>(
    seed: u64,
    ring: (u64, usize),
    main: impl FnOnce(Test) -> F + Send + 'static,
) where
    F: Future<Output = ()> + 'static,
{
    unsynced_on(seed, ring, |mut test| async move {
        test.sync().await;
        main(test).await;
    });
}

/// Runs `main` on a hub of one shard of a simulated node with no mesh time.
fn unsynced<F>(seed: u64, main: impl FnOnce(Test) -> F + Send + 'static)
where
    F: Future<Output = ()> + 'static,
{
    unsynced_on(seed, (AREA, BODY_MAX), main);
}

/// Runs `main` as [`unsynced`] does, on a ring of the area and body max in `ring`.
fn unsynced_on<F>(
    seed: u64,
    (area, body_max): (u64, usize),
    main: impl FnOnce(Test) -> F + Send + 'static,
) where
    F: Future<Output = ()> + 'static,
{
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, move |node, tasks| async move {
        let layout = buffer::Layout::new(area, body_max).expect("a ring");
        main(Test::new(node, tasks, layout).await).await;
    })
    .expect("the run ends");
}

fn name(name: &str) -> Name {
    name.parse().expect("a valid name")
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

/// Writes the samples of each channel by key, in one group: the first is its index.
fn write_series(writer: &mut Writer, channels: &[(u128, &[i64])]) -> Vec<Outcome> {
    let set = writer.set();
    let entries: Vec<_> = channels.iter().map(|&(key, _)| entry(set, key)).collect();
    let group = set.entries()[entries[0]].group;
    let mut series: Vec<_> = (entries.iter().zip(channels))
        .map(|(&entry, (_, samples))| (entry, samples.len() * 8))
        .collect();
    series.sort_unstable();
    let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
    for (&entry, (_, samples)) in entries.iter().zip(channels) {
        let bytes = draft.series_mut(entry).expect("the series is present");
        for (bytes, sample) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(*samples) {
            *bytes = sample.to_le_bytes();
        }
    }
    let count = u32::try_from(channels[0].1.len()).expect("a short frame");
    draft.set_count(group, count);
    writer
        .write(LIVE, draft)
        .expect("the home takes it")
        .to_vec()
}

/// The entries of the view in `received`, as channel keys.
fn keys(received: &Received<'_>) -> Vec<u128> {
    let entries = received.set.entries();
    let present = received.view.iter().map(|(entry, _)| entries[entry].key);
    present.map(channel::Key::as_u128).collect()
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
    let entry = entry(received.set, key);
    let group = received.set.entries()[entry].group;
    let count = received
        .view
        .range(group)
        .expect("the group is present")
        .count;
    let count = usize::try_from(count).expect("a count");
    let (_, bytes) = received
        .view
        .iter()
        .find(|&(present, _)| present == entry)
        .expect("the view holds the series");
    let data_type = received.set.entries()[entry].data_type;
    let mut out = vec![0; count * 8];
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
            assert_eq!(received.view.range(0), Some(range));
            assert_eq!(samples(&received, 1), [now + n]);
            assert_eq!(samples(&received, 2), [n * 10]);
        }
    });
}

#[test]
fn opens_no_session_on_an_unknown_name() {
    run(2, |test| async move {
        let writer = test.hub.writer(config("a", &["value", "nope"])).await;
        let error = writer.expect_err("an error");
        assert_eq!(error, writer::Error::Unknown(name("nope")));
        assert_eq!(error.to_string(), "no channel is named nope");
        let reader = test.hub.reader(&[name("nope")], Mode::Complete).await;
        let error = reader.expect_err("an error");
        assert_eq!(error, reader::Error::Unknown(name("nope")));
        assert_eq!(error.to_string(), "no channel is named nope");
    });
}

#[test]
fn opens_no_session_on_two_indexes_or_on_no_channel() {
    run(3, |test| async move {
        let names = [name("value"), name("value-b")];
        let error = test
            .hub
            .reader(&names, Mode::Latest)
            .await
            .expect_err("an error");
        assert_eq!(error, reader::Error::ManyIndexes);
        assert_eq!(
            error.to_string(),
            "the channels are on more than one index: open a reader per index"
        );
        let error = test
            .hub
            .reader(&[], Mode::Latest)
            .await
            .expect_err("an error");
        assert_eq!(error, reader::Error::Empty);
        assert_eq!(error.to_string(), "a reader names at least one channel");
        let writer = test.hub.writer(config("a", &[])).await;
        let error = writer.expect_err("an error");
        assert_eq!(error, writer::Error::Empty);
        assert_eq!(error.to_string(), "a writer names at least one channel");
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
        let time = entry(received.set, 1);
        let entries: Vec<_> = received.view.iter().map(|(entry, _)| entry).collect();
        assert_eq!(entries, [time], "the view holds only the reader's channels");
    });
}

#[test]
fn opens_a_writer_once_the_node_has_mesh_time_and_a_reader_before() {
    unsynced(5, |mut test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let error = test
            .hub
            .writer(config("a", &["value"]))
            .await
            .expect_err("an error");
        let unsynced = hub::home::writer::Error::Unsynced;
        assert_eq!(error, writer::Error::Home(unsynced));
        assert_eq!(error.to_string(), "the node has no mesh time yet");
        test.sync().await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        assert_eq!(write(&mut writer, &[now], &[1]), [applied(0)]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [1]);
    });
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
        assert_eq!(ended.get(), 1, "the commit task ended");
    });
}

/// Sessions that outlive the hub keep the commit task: it ends once the last drops.
/// The hub drops while the task sleeps, so the write after it must wake the task.
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
        assert_eq!(ended.get(), 1, "the commit task ended");
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
        let before = test.polls.get();
        for value in 0..20 {
            write(&mut writer, &[test.now()], &[value]);
            test.clock.sleep(Span::from_nanos(1)).await;
        }
        assert_eq!(
            test.polls.get() - before,
            1,
            "one wake, for the first write"
        );
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
        assert_eq!(ended.get(), 1, "the commit task ended before the commit");
        clock.sleep(SETTLE).await;
        assert_eq!(Rc::strong_count(&pool), 1, "only the test holds the pool");
    });
}

/// Once the hub and each session drop while the commit task waits for a commit, the
/// hub holds no commit: a commit of the home that resolves holds the ring open until
/// it drops.
#[test]
fn drops_the_commit_it_waits_for_with_the_hub() {
    for seed in 0..64 {
        run(seed, move |test| async move {
            let reader = test.reader(&["value"], Mode::Complete).await;
            let mut writer = test.writer("a", &["value"]).await;
            test.clock.sleep(SETTLE).await;
            write(&mut writer, &[test.now()], &[1]);
            test.clock.sleep(Span::from_nanos(1)).await;
            let Test {
                node,
                mut commit,
                hub,
                paused,
                ..
            } = test;
            paused.pause();
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
            paused.resume();
        });
    }
}

/// The commit task that went back to sleep after a commit wakes for the next write.
#[test]
fn wakes_the_commit_task_for_a_write_after_a_commit() {
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

#[test]
fn gives_a_complete_reader_frames_past_its_window_only_as_it_takes_them() {
    run(10, |test| async move {
        let mut taker = test.reader(&["value"], Mode::Complete).await;
        let mut lagger = test.reader(&["value"], Mode::Complete).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        let (mut bytes, mut spent, mut first) = (0, 0, 0);
        for n in 0..400 {
            write_wide(&mut writer, now, n);
            bytes += charge(&taker.next().await.expect("a frame").view);
            // The call that takes the second frame grants credit for the first.
            if n < 2 {
                let charge = charge(&lagger.next().await.expect("a frame").view);
                if n == 0 {
                    first = charge;
                }
                spent += charge;
            }
        }
        assert!(bytes > 3 * WINDOW, "{bytes} bytes are past three windows");
        let (charges, ended) = take_all(&mut lagger).await;
        assert_behind(&ended);
        let last = charges.last().copied().unwrap_or(0);
        spent += charges.iter().sum::<u64>();
        let limit = first + WINDOW;
        assert!(
            spent - last < limit && limit <= spent,
            "{spent} bytes end at the first frame past {limit}"
        );
    });
}

#[test]
fn gives_a_reader_on_some_channels_of_a_frame_those_and_their_index() {
    run(20, |test| async move {
        let mut both = test.reader(&["value-c", "value"], Mode::Complete).await;
        let mut one = test.reader(&["value-c"], Mode::Complete).await;
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
        assert_eq!(received.view.range(0), Some(range));
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
        let first = charge(&reader.next().await.expect("a frame").view);
        assert!(first < WINDOW, "{first} bytes are under the window");
        let a = {
            let next = reader.next();
            write_samples(&mut writer, now + 80_000, 50_000);
            write_samples(&mut writer, now + 130_000, 1);
            test.clock.sleep(SETTLE).await;
            charge(&next.await.expect("a frame").view)
        };
        assert!(
            a + 4096 < WINDOW,
            "{a} bytes and one sample are under the window"
        );
        let b = reader.next().await.map(|received| charge(&received.view));
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
        let first = charge(&reader.next().await.expect("a frame").view);
        let second = charge(&reader.next().await.expect("a frame").view);
        assert!(
            first + second > WINDOW,
            "{first} + {second} bytes pass a window"
        );
        write_samples(&mut writer, now + 160_000, 1);
        test.clock.sleep(SETTLE).await;
        let third = reader.next().await.map(|received| charge(&received.view));
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
        let text = definition(6, DataType::Sample(Type::String), 1);
        test.hub.define([(&name("text"), &text)]);
        let mut reader = test.reader(&["text"], Mode::Complete).await;
        let mut writer = test.writer("a", &["text"]).await;
        let now = test.now();
        let set = Arc::clone(writer.set());
        let (index, text) = (entry(&set, 1), entry(&set, 6));
        let raw = [&2_u32.to_le_bytes()[..], b"hi"].concat();
        let series = [(index, 8), (text, raw.len())];
        let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
        draft
            .series_mut(index)
            .expect("the index")
            .copy_from_slice(&now.to_le_bytes());
        draft
            .series_mut(text)
            .expect("the text")
            .copy_from_slice(&raw);
        draft.set_count(0, 1);
        let written = writer
            .write(LIVE, draft)
            .expect("the home takes it")
            .to_vec();
        assert_eq!(written, [applied(0)]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 1), [now]);
        let (_, bytes) = received
            .view
            .iter()
            .find(|&(present, _)| present == text)
            .expect("the view holds the text");
        let mut out = vec![0; raw.len()];
        codec::decode(Type::String, 1, bytes, &mut out).expect("decodes");
        assert_eq!(out, raw);
    });
}

#[test]
fn refuses_a_string_sample_that_is_not_utf8() {
    run(19, |test| async move {
        let text = definition(6, DataType::Sample(Type::String), 1);
        test.hub.define([(&name("text"), &text)]);
        let mut writer = test.writer("a", &["text"]).await;
        let now = test.now();
        let set = Arc::clone(writer.set());
        let (index, text) = (entry(&set, 1), entry(&set, 6));
        let raw = [&2_u32.to_le_bytes()[..], &[0xff, 0xfe]].concat();
        let series = [(index, 8), (text, raw.len())];
        let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
        draft
            .series_mut(index)
            .expect("the index")
            .copy_from_slice(&now.to_le_bytes());
        draft
            .series_mut(text)
            .expect("the text")
            .copy_from_slice(&raw);
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

/// Defines `channels`, each `(key, name, index)` and of `I64`, in one call to a new
/// hub. A `Vec`, not a map, so that a name may come twice.
fn define(channels: Vec<(u128, &'static str, u128)>) {
    run(18, move |test| async move {
        let channels: Vec<_> = channels
            .into_iter()
            .map(|(key, channel, index)| {
                let data_type = DataType::Sample(I64);
                (name(channel), definition(key, data_type, index))
            })
            .collect();
        test.hub
            .define(channels.iter().map(|(name, channel)| (name, channel)));
    });
}

#[test]
#[should_panic(
    expected = "a channel with key 00000000-0000-0000-0000-000000000002 or name other is known already"
)]
fn define_panics_on_a_known_key() {
    define(vec![(2, "other", 1)]);
}

#[test]
#[should_panic(
    expected = "a channel with key 00000000-0000-0000-0000-000000000009 or name value is known already"
)]
fn define_panics_on_a_known_name() {
    define(vec![(9, "value", 1)]);
}

#[test]
#[should_panic(
    expected = "the index 00000000-0000-0000-0000-000000000002 of channel other is not a known index"
)]
fn define_panics_on_an_index_that_is_a_data_channel() {
    define(vec![(9, "other", 2)]);
}

#[test]
#[should_panic(
    expected = "the index 00000000-0000-0000-0000-000000000008 of channel other is not a known index"
)]
fn define_panics_on_an_unknown_index() {
    define(vec![(9, "other", 8)]);
}

#[test]
#[should_panic(
    expected = "a channel with key 00000000-0000-0000-0000-00000000000a or name other is known already"
)]
fn define_panics_on_a_name_twice_in_one_call() {
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
        test.hub.define(channels);
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
        test.hub.define([
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
        test.hub.define([(&temp_name, &temp), (&time_name, &time)]);
        let writer = test.writer("a", &["plant.temp"]).await;
        let keys: Vec<_> = writer.set().entries().iter().map(|e| e.key).collect();
        assert_eq!(keys, [6, 7].map(channel::Key::from_u128));
    });
}

#[test]
fn gives_a_writer_the_sample_type_of_each_data_channel() {
    run(18, |test| async move {
        let types = [
            (6, "text", DataType::Sample(Type::String)),
            (7, "quality", DataType::Quality),
        ];
        let channels: BTreeMap<_, _> = types
            .into_iter()
            .map(|(key, channel, data_type)| {
                (name(channel), definition(key, data_type, 1))
            })
            .collect();
        test.hub.define(&channels);
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
        assert!(Arc::ptr_eq(received.set, &set));
        let received = reader.next().await.expect("a frame");
        assert!(Arc::ptr_eq(received.set, b.set()));
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
        assert_eq!(received.view.range(0), Some(Range { seq: 4, count: 1 }));
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
        let (charges, ended) = take_all(&mut lagger).await;
        assert_behind(&ended);
        let spent: u64 = charges.iter().sum();
        assert!(WINDOW <= spent, "{spent} bytes reach the window");
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        write_wide(&mut writer, now, frames);
        let received = reader.next().await.expect("a frame");
        let range = Range {
            seq: frames.cast_unsigned() * 1000,
            count: 1000,
        };
        assert_eq!(received.view.range(0), Some(range));
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
            Ok(received) => charges.push(charge(&received.view)),
            Err(ended) => return (charges, ended),
        }
    }
}

fn assert_behind(ended: &Ended) {
    assert_eq!(*ended, Ended::Behind);
    assert_eq!(
        ended.to_string(),
        "the reader missed a frame and gets no later one: open a new reader"
    );
}

/// Polls `next` once with a waker that sets a flag, and returns the flag.
fn poll_flagged<F: Future>(next: Pin<&mut F>) -> (Poll<F::Output>, Arc<Flag>) {
    let flag = Arc::new(Flag::default());
    let waker = Waker::from(Arc::clone(&flag));
    (next.poll(&mut Context::from_waker(&waker)), flag)
}

#[test]
fn ends_a_complete_reader_that_holds_a_frame_past_its_window_at_the_next_commit() {
    for (seed, commits) in (0..32).zip([1, 2].into_iter().cycle()) {
        run_on(seed, (WIDE_AREA, WIDE_BODY_MAX), move |test| async move {
            let mut reader = test.reader(&["value"], Mode::Complete).await;
            let mut writer = test.writer("a", &["value"]).await;
            let now = test.now();
            write_samples(&mut writer, now, PAST_WINDOW);
            let charge = charge(&reader.next().await.expect("a frame").view);
            assert!(charge > WINDOW, "{charge} bytes spend the window");
            // The reader holds the frame, so the next waits for credit.
            for n in 0..commits {
                write_samples(&mut writer, now + PAST_WINDOW + n, 1);
                test.clock.sleep(SETTLE).await;
            }
            let next = reader.next().await;
            if commits == 1 {
                let range = next.expect("the call gives credit").view.range(0);
                let seq = PAST_WINDOW.cast_unsigned();
                assert_eq!(range, Some(Range { seq, count: 1 }));
            } else {
                assert_behind(&next.expect_err("the reader missed a frame"));
            }
        });
    }
}

#[test]
fn gives_a_waiting_complete_reader_each_frame_of_a_commit_past_its_window() {
    for seed in 0..32 {
        run(seed, |test| async move {
            let mut reader = test.reader(&["value"], Mode::Complete).await;
            let mut writer = test.writer("a", &["value"]).await;
            let now = test.now();
            let flag = {
                let (polled, flag) = poll_flagged(pin!(reader.next()));
                assert!(polled.is_pending(), "no frame waits");
                flag
            };
            for n in 0..200 {
                write_wide(&mut writer, now, n);
            }
            test.clock.sleep(SETTLE).await;
            assert!(
                flag.0.load(Ordering::Relaxed),
                "the commit wakes the reader"
            );
            let mut spent = 0;
            for n in 0..200 {
                let received = reader.next().await.expect("a frame");
                let range = received.view.range(0).expect("the index is present");
                assert_eq!(
                    range,
                    Range {
                        seq: n * 1000,
                        count: 1000
                    }
                );
                spent += charge(&received.view);
            }
            assert!(spent > WINDOW, "{spent} bytes pass the window");
            let (polled, _) = poll_flagged(pin!(reader.next()));
            assert!(polled.is_pending(), "no frame waits");
        });
    }
}

#[test]
fn ends_a_complete_reader_after_its_waiting_frames_when_it_misses_a_frame() {
    for seed in 0..32 {
        run(seed, |test| async move {
            let mut reader = test.reader(&["value"], Mode::Complete).await;
            let mut writer = test.writer("a", &["value"]).await;
            let now = test.now();
            for n in 0..20 {
                write_wide(&mut writer, now, n);
            }
            test.clock.sleep(SETTLE).await;
            for n in 20..200 {
                write_wide(&mut writer, now, n);
            }
            test.clock.sleep(SETTLE).await;
            // The frames past the window miss at this commit.
            write_wide(&mut writer, now, 200);
            test.clock.sleep(SETTLE).await;
            let (charges, ended) = take_all(&mut reader).await;
            assert_behind(&ended);
            assert!(
                20 < charges.len() && charges.len() < 200,
                "{}",
                charges.len()
            );
        });
    }
}

#[test]
fn ends_a_complete_reader_that_missed_a_frame_as_behind_after_a_failed_sync() {
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
        let (_, ended) = take_all(&mut latest).await;
        assert!(matches!(ended, Ended::Buffer(_)), "the sync failed");
        let (_, ended) = take_all(&mut reader).await;
        assert_behind(&ended);
        assert_behind(&reader.next().await.expect_err("the reader ended"));
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
        assert_eq!(written, Err(hub::home::Error::Large));
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
        let names = [name("value")];
        let opening = test.hub.reader(&names, Mode::Complete);
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
        let first = charge(&reader.next().await.expect("a frame").view);
        assert!(first < WINDOW, "{first} bytes are under the window");
        let a = {
            let mut next = pin!(reader.next());
            assert!(poll_once(next.as_mut()).is_pending(), "no frame waits");
            write_samples(&mut writer, now + 80_000, 50_000);
            write_samples(&mut writer, now + 130_000, 1);
            test.clock.sleep(SETTLE).await;
            charge(&next.await.expect("a frame").view)
        };
        assert!(
            a + 4096 < WINDOW,
            "{a} bytes and one sample are under the window"
        );
        let b = reader.next().await.map(|received| charge(&received.view));
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

/// A local complete reader spends the whole frame, not only its channels.
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
        let mut got = 0;
        loop {
            match poll_once(reader.next()) {
                Poll::Ready(Ok(_)) => got += 1,
                Poll::Ready(Err(ended)) => {
                    assert_behind(&ended);
                    assert!(got < frames, "{got} frames before the end");
                    break;
                }
                Poll::Pending => panic!("the reader took all {got} frames and waits"),
            }
        }
    });
}
