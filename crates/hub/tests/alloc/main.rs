//! A write and `next` make no heap allocation once the hub has taken a few frames: for
//! a complete reader when frames wait for it and when it waits for them, and for latest
//! readers that the write wakes, also two writes in one commit and readers replaced.
//! The commit task is not counted. The home's `woken` after a commit, which that task
//! calls, is counted on its own: it makes none once its keys are sized, while a
//! complete reader takes each frame, and when a complete reader falls behind again to
//! a depth it reached before. An open of a reader of many channels makes no
//! allocation for each channel. The names of a reader make no allocation at a change
//! to a key set no larger than one before. A client read makes as many allocations for
//! each frame it relays as a HUB WIRE session, and a client write of one or two groups
//! makes none, also at a member that forwards it to its home. A complete reader that
//! catches up from disk allocates only at each read. A write that the ring has no room
//! for makes none, also when the handoff of its writer finds no room. This binary has
//! no test harness: the count covers each thread, and a harness allocates on its own
//! thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[path = "../common/agent.rs"]
#[expect(dead_code, reason = "the binary uses only some of the helpers")]
mod agent;
#[path = "../common/mod.rs"]
mod common;
mod forward;
#[path = "../common/forwarded.rs"]
#[expect(dead_code, reason = "the binary uses only some of the helpers")]
mod forwarded;
#[path = "../common/net.rs"]
mod net;
#[path = "../common/node.rs"]
mod node;
#[path = "../common/peer.rs"]
mod peer;
#[path = "../common/region.rs"]
mod region;
mod relay;
#[path = "../common/serving.rs"]
mod serving;
#[path = "../common/sessions.rs"]
#[expect(dead_code, reason = "the binary uses only some of the helpers")]
mod sessions;
#[path = "../common/woken.rs"]
mod woken;
mod write;

use std::hint::black_box;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use common::{SETTLE, hub, name, unnamed};
use home::reader::Next;
use home::reader::complete::Charge;
use hub::Hub;
use hub::home::Outcome;
use hub::reader::{self, Mode, Reader};
use hub::writer::{self, Writer};
use types::authority::Authority;
use types::frame::{Draft, Form, Label, Path};
use types::time::Span;
use wire::hub::client;
use woken::Woken;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// Frames that warm the hub up: its vectors reach their size.
const WARM: i64 = 4;
/// Frames whose write and read are counted. More than the streak of `next`, so a
/// count covers its yield.
const COUNTED: i64 = 300;

/// Writes one sample to `time` and to `value`, with the allocations the write made.
fn write(writer: &mut Writer, stamp: i64) -> u64 {
    ALLOCATOR
        .count(|| common::write(writer, common::draft(writer, stamp)))
        .1
}

/// Takes the next frame of `reader`, which waits for it, with the allocations the
/// polls made. A first `Pending` is the yield of `next`.
fn read(reader: &mut Reader) -> u64 {
    let (taken, allocations) = ALLOCATOR.count(|| {
        let context = &mut Context::from_waker(Waker::noop());
        (0..2).any(|_| match pin!(reader.next()).poll(context) {
            Poll::Ready(received) => received.is_ok(),
            Poll::Pending => false,
        })
    });
    assert!(taken, "a committed frame waits for the reader");
    allocations
}

/// Takes the next frame of `reader`, as [`read`] does, and gives the name of each entry
/// of its view, with the allocations of both.
fn read_named(reader: &mut Reader) -> u64 {
    let (taken, allocations) = ALLOCATOR.count(|| {
        let context = &mut Context::from_waker(Waker::noop());
        (0..2).any(|_| match pin!(reader.next()).poll(context) {
            Poll::Ready(Ok(received)) => {
                for (entry, _) in received.view().iter() {
                    black_box(received.name(entry));
                }
                true
            }
            Poll::Ready(Err(_)) | Poll::Pending => false,
        })
    });
    assert!(taken, "a committed frame waits for the reader");
    allocations
}

/// Polls `reader`, which has no frame waiting, once, with the allocations the poll
/// made. A `Pending` ends the streak of `next`, so the poll never yields.
fn wait(reader: &mut Reader) -> u64 {
    let (waited, allocations) = ALLOCATOR.count(|| {
        let context = &mut Context::from_waker(Waker::noop());
        pin!(reader.next()).poll(context).is_pending()
    });
    assert!(waited, "no frame waits for the reader");
    allocations
}

/// Writes that each wake five latest readers, `latest` and four more it opens, from
/// `now`: two writes in one commit after each `WARM` writes that each have a commit.
async fn five_latest(
    hub: &Hub,
    node: &sim::node::Node,
    writer: &mut Writer,
    latest: Reader,
    mut now: i64,
) {
    let mut latests = vec![latest];
    for _ in 0..4 {
        let latest = hub.reader(unnamed(&["value"], Mode::Latest)).await;
        latests.push(latest.expect("opens"));
    }
    for latest in &mut latests[1..] {
        read(latest);
    }
    for n in 0..3 * (WARM + 2) {
        let waited: u64 = latests.iter_mut().map(wait).sum();
        let written = write(writer, now);
        now += 1;
        let taken: u64 = latests.iter_mut().map(read).sum();
        if n % (WARM + 2) <= WARM {
            node.clock().sleep(SETTLE).await;
        }
        if n >= WARM {
            assert_eq!((waited, written, taken), (0, 0, 0), "five latest {n}");
        }
    }
}

/// Writes that each wake twelve latest readers from `now`, then one more after each
/// reader is replaced by one that takes the newest frame at open.
async fn replaced(
    hub: &Hub,
    node: &sim::node::Node,
    writer: &mut Writer,
    mut now: i64,
) {
    let mut latests = Vec::new();
    for _ in 0..12 {
        let latest = hub.reader(unnamed(&["value"], Mode::Latest)).await;
        latests.push(latest.expect("opens"));
    }
    for latest in &mut latests {
        read(latest);
    }
    // Keep this wait: a commit wake here takes any key put at open, so the warm writes
    // size the keys to twelve, and a key put at open shows in the last write.
    node.clock().sleep(SETTLE).await;
    for n in 0..2 * WARM {
        let waited: u64 = latests.iter_mut().map(wait).sum();
        let written = write(writer, now);
        now += 1;
        let taken: u64 = latests.iter_mut().map(read).sum();
        node.clock().sleep(SETTLE).await;
        if n >= WARM {
            assert_eq!((waited, written, taken), (0, 0, 0), "twelve latest {n}");
        }
    }
    for latest in &mut latests {
        let replaced = hub.reader(unnamed(&["value"], Mode::Latest)).await;
        *latest = replaced.expect("opens");
        read(latest);
    }
    let waited: u64 = latests.iter_mut().map(wait).sum();
    let written = write(writer, now);
    let taken: u64 = latests.iter_mut().map(read).sum();
    assert_eq!((waited, written, taken), (0, 0, 0), "replaced");
}

/// Opens `config` on `hub`, with the allocations made while the open ran, not those of
/// the other tasks while it waited.
async fn open(
    hub: &Hub,
    node: &sim::node::Node,
    config: reader::Config,
) -> (Reader, u64) {
    let mut open = pin!(hub.reader(config));
    let mut context = Context::from_waker(Waker::noop());
    let mut allocations = 0;
    loop {
        let (poll, made) = ALLOCATOR.count(|| open.as_mut().poll(&mut context));
        allocations += made;
        if let Poll::Ready(reader) = poll {
            return (reader.expect("opens"), allocations);
        }
        node.clock().sleep(Span::MILLISECOND).await;
    }
}

/// An open of a reader of many channels makes fewer allocations than it has channels,
/// once the first open grew the state of its channels: it makes no allocation for each
/// name.
fn open_of_a_reader_of_many_channels() {
    const MANY: u64 = 1000;
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let more: Vec<_> = (0..MANY).map(|n| name(&format!("w.{n}"))).collect();
        let hub = common::hub_alone(&node, tasks, &more).await;
        let _first = open(&hub, &node, unnamed(&["w.**"], Mode::Latest)).await;
        let (_second, allocations) =
            open(&hub, &node, unnamed(&["w.**"], Mode::Latest)).await;
        assert!(
            allocations < MANY,
            "an open of {MANY} channels made {allocations} allocations"
        );
    })
    .expect("the run ends");
}

/// The `next` of a reader at each change between a large key set and a small one. The
/// first two frames get no name, and make no places. Then each frame gets a name for
/// each entry of its view: the places allocate at the first large key set, and then
/// never again.
fn next_at_each_change_of_key_set() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let more: Vec<_> = (0..64).map(|n| name(&format!("w.{n}"))).collect();
        let hub = common::hub_alone(&node, tasks, &more).await;
        let mut reader = hub
            .reader(unnamed(&["value"], Mode::Complete))
            .await
            .expect("opens");
        let large: Vec<_> = [name("time"), name("value")]
            .into_iter()
            .chain(more.iter().cloned())
            .collect();
        let small = vec![name("time"), name("value")];
        let mut counts = Vec::new();
        let mut now = 1_000_000;
        for n in 0..8 {
            let channels = if n % 2 == 0 {
                large.clone()
            } else {
                small.clone()
            };
            let config = writer::Config {
                subject: name("a"),
                authority: Authority(1),
                lease: None,
                channels,
            };
            let mut writer = hub.writer(config).await.expect("opens");
            now = writer.now().nanos().max(now + 1);
            write(&mut writer, now);
            node.clock().sleep(SETTLE).await;
            counts.push(if n < 2 {
                read(&mut reader)
            } else {
                read_named(&mut reader)
            });
            drop(writer);
            node.clock().sleep(SETTLE).await;
        }
        assert_eq!(
            counts,
            [4, 1, 5, 1, 4, 1, 4, 1],
            "allocations of each `next` and its names: {counts:?}"
        );
    })
    .expect("the run ends");
}

/// Writes a frame, waits for its commit, and calls `woken`. Returns the number of keys
/// it gave and the allocations it made.
async fn commit(woken: &mut Woken) -> (usize, u64) {
    woken.commit().await;
    let ((), allocations) = ALLOCATOR.count(|| woken.shard.woken(&mut woken.keys));
    (woken.keys.len(), allocations)
}

/// The home's `woken` with one complete reader that takes each frame: each call
/// after the first names the reader and allocates nothing.
fn woken_of_a_reader_that_takes_each_frame() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let mut woken = Woken::new(&node, tasks, 1).await;
        let mut calls = Vec::new();
        for n in 0..20 {
            calls.push(commit(&mut woken).await);
            assert!(
                matches!(woken.shard.take(woken.readers[0]), Next::Frame(_)),
                "frame {n} waits for the reader"
            );
        }
        assert_eq!(
            calls[1..],
            [(1, 0); 19],
            "(keys given, allocations) of each call after the first: {calls:?}"
        );
    })
    .expect("the run ends");
}

/// The home's `woken` with one complete reader that falls 64 frames behind, takes
/// each, then falls 64 frames behind again: no call of the second lag allocates.
fn woken_of_a_reader_that_falls_behind_again() {
    /// Deep enough that the waiting list of the reader grows more than once.
    const DEPTH: usize = 64;
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let mut woken = Woken::new(&node, tasks, 1).await;
        let mut lags = Vec::new();
        for lag in 0..2 {
            let mut calls = Vec::new();
            for _ in 0..DEPTH {
                calls.push(commit(&mut woken).await);
            }
            let reader = woken.readers[0];
            let taken = std::iter::from_fn(|| match woken.shard.take(reader) {
                Next::Frame(frame) => Some(frame),
                Next::Empty => None,
                Next::Disk(_) => panic!("lag {lag}: the reader missed"),
            })
            .count();
            assert_eq!(taken, DEPTH, "lag {lag}");
            lags.push(calls);
        }
        let grown = lags[0][1..]
            .iter()
            .filter(|&&(_, allocations)| allocations > 0);
        assert!(grown.count() > 1, "the first lag grows the list: {lags:?}");
        let mut expected = [(0, 0); DEPTH];
        expected[0] = (1, 0);
        assert_eq!(
            lags[1], expected,
            "(keys given, allocations) of each call of the second lag: {lags:?}"
        );
    })
    .expect("the run ends");
}

/// The home's `woken` with a reader that takes each frame and one whose credit covers
/// one frame at a time: once a frame of the second waits for credit, as many frames
/// wait for it as waited before, so the call allocates nothing.
fn woken_of_a_reader_whose_frame_waits_for_credit() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let mut woken = Woken::new(&node, tasks, 1).await;
        let slot = woken.set.entries()[woken.set.groups()[0]].slot;
        let limited = woken.shard.open_complete(slot, 1, Charge::Whole).key;
        let mut calls = Vec::new();
        let mut spent = 0;
        for n in 0..6 {
            if n < 5 {
                woken.shard.grant(limited, spent + 1);
            }
            calls.push(commit(&mut woken).await);
            assert!(
                matches!(woken.shard.take(woken.readers[0]), Next::Frame(_)),
                "frame {n} waits for the reader"
            );
            if n < 5 {
                let Next::Frame(frame) = woken.shard.take(limited.into()) else {
                    panic!("frame {n} waits for the limited reader");
                };
                spent += frame.charge();
            }
        }
        assert_eq!(
            calls[1..],
            [(2, 0), (2, 0), (2, 0), (2, 0), (1, 0)],
            "(keys given, allocations) of each call after the first: {calls:?}"
        );
    })
    .expect("the run ends");
}

/// Takes the next frame of `reader`, as [`read`] does, also when it waits for a read
/// from disk, with the allocations of its polls.
async fn take(reader: &mut Reader, node: &sim::node::Node) -> u64 {
    let mut next = pin!(reader.next());
    let mut context = Context::from_waker(Waker::noop());
    let mut allocations = 0;
    loop {
        let (poll, made) = ALLOCATOR.count(|| next.as_mut().poll(&mut context));
        allocations += made;
        if let Poll::Ready(received) = poll {
            assert!(received.is_ok(), "the reader takes a frame");
            return allocations;
        }
        node.clock().sleep(Span::MILLISECOND).await;
    }
}

/// A frame of `WIDE` samples from `stamp` on `time` and `value`, with values that do
/// not compress.
fn wide(writer: &Writer, stamp: i64) -> Draft {
    const WIDE: u32 = 1000;
    let set = writer.set();
    let bytes = usize::try_from(WIDE).expect("a count fits") * 8;
    let mut series: Vec<_> = (0..set.entries().len()).map(|n| (n, bytes)).collect();
    series.sort_unstable();
    let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
    for (entry, _) in series {
        let bytes = draft.series_mut(entry).expect("the series is present");
        for (n, sample) in bytes.as_chunks_mut::<8>().0.iter_mut().enumerate() {
            let n = stamp + i64::try_from(n).expect("a sample index fits");
            let scrambled = n.wrapping_mul(6_364_136_223_846_793_005);
            let value = if entry == 0 {
                n
            } else {
                scrambled ^ (scrambled >> 29)
            };
            *sample = value.to_le_bytes();
        }
    }
    draft.set_count(set.entries()[0].group, WIDE);
    draft
}

/// A complete reader that takes no frame until frames pass its window, then takes
/// each, twice. In the second catch-up, only a take that reads from disk allocates,
/// for its file calls, and each read gives 8 frames or more.
fn catch_up_from_disk() {
    const FRAMES: i64 = 201;
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let (hub, now) = hub(&node, tasks).await;
        let mut reader = hub
            .reader(unnamed(&["value"], Mode::Complete))
            .await
            .expect("opens");
        let config = writer::Config {
            subject: name("a"),
            authority: Authority(1),
            lease: None,
            channels: vec![name("time"), name("value")],
        };
        let mut writer = hub.writer(config).await.expect("opens");
        let mut laps = Vec::new();
        for lap in 0..2 {
            for n in 0..FRAMES {
                let draft = wide(&writer, now + (lap * FRAMES + n) * 1000);
                common::write(&mut writer, draft);
                if n >= FRAMES - 2 {
                    node.clock().sleep(SETTLE).await;
                }
            }
            let mut counts = Vec::new();
            for _ in 0..FRAMES {
                counts.push(take(&mut reader, &node).await);
            }
            laps.push(counts);
        }
        let reads: Vec<_> = (0..laps[1].len()).filter(|&n| laps[1][n] > 0).collect();
        assert!(!reads.is_empty(), "the reader reads from disk: {laps:?}");
        assert!(
            reads.windows(2).all(|pair| pair[1] - pair[0] >= 8),
            "allocations of each take: {laps:?}"
        );
    })
    .expect("the run ends");
}

fn main() {
    assert_eq!(
        ALLOCATOR.count(|| drop(Box::new(1_u8))).1,
        1,
        "the allocator counts"
    );
    for mode in [client::Mode::Latest, client::Mode::Complete] {
        relay::relays_a_frame_with_the_allocations_of_a_wire_session(mode);
    }
    for groups in [write::Groups::One, write::Groups::Two] {
        write::relays_a_frame_with_no_allocation(groups);
    }
    forward::forwards_a_frame_with_no_allocation();
    forward::forwards_a_full_window_with_no_allocation();
    catch_up_from_disk();
    woken_of_a_reader_that_takes_each_frame();
    woken_of_a_reader_that_falls_behind_again();
    woken_of_a_reader_whose_frame_waits_for_credit();
    open_of_a_reader_of_many_channels();
    next_at_each_change_of_key_set();
    lost_writes();
    write_and_next();
}

/// Writes one sample to `time` and to `value`, which the ring has no room for, with the
/// allocations the write made.
fn lose(writer: &mut Writer, stamp: i64) -> u64 {
    let (lost, allocations) = ALLOCATOR.count(|| {
        let draft = common::draft(writer, stamp);
        let written = writer.write(Label::Path(Path::Live), draft);
        matches!(written, Ok([Outcome::Lost { .. }]))
    });
    assert!(lost, "the ring has no room for the frame");
    allocations
}

/// A write that the ring has no room for, and one of a writer whose handoff finds no
/// room: neither allocates once warm.
fn lost_writes() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let (hub, mut stamp) = hub(&node, tasks).await;
        let config = |subject, authority| writer::Config {
            subject: name(subject),
            authority: Authority(authority),
            lease: None,
            channels: vec![name("value")],
        };
        let mut writer = hub.writer(config("a", 1)).await.expect("opens");
        'fill: loop {
            for _ in 0..64 {
                let draft = common::draft(&writer, stamp);
                stamp += 1;
                let written = writer.write(Label::Path(Path::Live), draft);
                if matches!(written, Ok([Outcome::Lost { .. }])) {
                    break 'fill;
                }
            }
            node.clock().sleep(SETTLE).await;
        }
        let counted = |writer: &mut Writer, stamp: &mut i64, name| {
            for n in 0..WARM + COUNTED {
                let lost = lose(writer, *stamp);
                *stamp += 1;
                if n >= WARM {
                    assert_eq!(lost, 0, "{name} write {n} allocated");
                }
            }
        };
        counted(&mut writer, &mut stamp, "lost");
        let mut taker = hub.writer(config("b", 2)).await.expect("opens");
        counted(&mut taker, &mut stamp, "handoff");
    })
    .expect("the run ends");
}

/// A write and `next` of a complete reader, when frames wait for it and when it
/// waits for them, and of latest readers that the write wakes: none allocates once
/// warm.
fn write_and_next() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let (hub, now) = hub(&node, tasks).await;
        let mut reader = hub
            .reader(unnamed(&["value"], Mode::Complete))
            .await
            .expect("opens");
        let config = writer::Config {
            subject: name("a"),
            authority: Authority(1),
            lease: None,
            channels: vec![name("time"), name("value")],
        };
        let mut writer = hub.writer(config).await.expect("opens");
        let mut latest = hub
            .reader(unnamed(&["value"], Mode::Latest))
            .await
            .expect("opens");
        for n in 0..WARM + COUNTED {
            let stamped = ALLOCATOR.count(|| writer.now()).1;
            let written = write(&mut writer, now + n);
            node.clock().sleep(SETTLE).await;
            let read = read(&mut reader);
            if n >= WARM {
                let counts = (stamped, written, read);
                assert_eq!(counts, (0, 0, 0), "frame {n} allocated");
            }
        }
        let now = now + WARM + COUNTED;
        for n in 0..WARM + COUNTED {
            let waited = wait(&mut reader);
            let written = write(&mut writer, now + n);
            node.clock().sleep(SETTLE).await;
            let read = read(&mut reader);
            if n >= WARM {
                assert_eq!((waited, written, read), (0, 0, 0), "frame {n} allocated");
            }
        }
        let now = now + WARM + COUNTED;
        read(&mut latest);
        for n in 0..WARM + COUNTED {
            let waited = wait(&mut latest);
            let written = write(&mut writer, now + n);
            let taken = read(&mut latest);
            node.clock().sleep(SETTLE).await;
            let read = read(&mut reader);
            if n >= WARM {
                assert_eq!((waited, written, taken, read), (0, 0, 0, 0), "latest {n}");
            }
        }
        let now = now + WARM + COUNTED;
        five_latest(&hub, &node, &mut writer, latest, now).await;
        replaced(&hub, &node, &mut writer, now + 3 * (WARM + 2)).await;
    })
    .expect("the run ends");
}
