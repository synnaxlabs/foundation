//! The rounds and the table of the bench.

use std::hint::black_box;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use env::tasks::{Group, Tasks};
use program::write::{Config, Outcome, Time};
use sdk::Client;
use types::authority::Authority;
use types::frame::Form;
use types::name::{Name, Selector};
use types::sample::Value;
use wire::hub::client::Mode;

use crate::rig::{POOL, Rig};

/// The mode of each reader. A complete reader gets each frame after its commit, one
/// record of the ring for each read, and the node never trims the ring (#160).
const MODE: Mode = Mode::Latest;

/// Calls before the timed rounds.
const WARMUP: usize = 500;

/// The spin of the caller before each call, which is longer than the time for the
/// task to park.
const GAP: Duration = Duration::from_micros(100);

/// The spin of the caller after a write of [`Timed::Held`], which is longer than the
/// time for its frame to reach the client's thread.
const HELD: Duration = Duration::from_millis(1);

/// What a call of a line times.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Timed {
    /// A write.
    Write,
    /// A write, then the read of its frame.
    Read,
    /// A write, then the read of its frame by a reader that names a channel twice.
    Twice,
    /// The read of the frame of a write, after [`HELD`], when the client's thread holds
    /// the frame.
    Held,
    /// [`Timed::Held`] with `next_within`.
    HeldWithin,
}

/// Runs the bench and prints its table.
pub(crate) fn run() {
    let (rounds, calls) = (5, 1_000);
    let legs = legs(rounds, calls);
    let program_line =
        |read| alone(|node, subject| program(node, &subject, rounds, calls, read));
    let sdk_line = |calls, timed| {
        alone(|node, subject| spans(node, subject, rounds, calls, timed))
    };
    let program = program_line(false);
    let sdk = sdk_line(calls, Timed::Write);
    let program_read = program_line(true);
    let sdk_read = sdk_line(calls, Timed::Read);
    let twice = sdk_line(calls, Timed::Twice);
    let held = sdk_line(calls / 2, Timed::Held);
    let held_within = sdk_line(calls / 2, Timed::HeldWithin);
    println!(
        "{:<20} {:>6} {:>9} {:>9}",
        "line", "round", "p50 µs", "p99 µs"
    );
    for (name, spans) in [
        ("leg to task", legs.0),
        ("leg to caller", legs.1),
        ("sdk", sdk),
        ("program", program),
        ("sdk read", sdk_read),
        ("program read", program_read),
        ("sdk read twice", twice),
        ("sdk next held", held),
        ("sdk next_within held", held_within),
    ] {
        for (round, mut spans) in spans.into_iter().enumerate() {
            spans.sort_unstable();
            let at = |percent: usize| micros(spans[spans.len() * percent / 100]);
            println!("{name:<20} {round:>6} {:>9.1} {:>9.1}", at(50), at(99));
        }
    }
}

/// The spans of `line` against a node of its own, with four times the disk budget of
/// [`Rig::new`]: 32 MiB for each core.
fn alone<T>(line: impl FnOnce(&sdk::Node, sdk::Subject) -> T) -> T {
    let mut rig = Rig::new();
    rig.disk *= 4;
    rig.keep(POOL, rig.disk);
    let (mut rig, node, subject) = crate::plant::planted_on(rig);
    let spans = line(&node, subject);
    rig.stop();
    spans
}

/// The spans of each of `rounds` rounds of `calls` writes of 2 values through an
/// `sdk::Writer` at `node`, timed as `timed`, with the read of each frame through a
/// reader of the same channels and client when `timed` reads.
fn spans(
    node: &sdk::Node,
    subject: sdk::Subject,
    rounds: usize,
    calls: usize,
    timed: Timed,
) -> Vec<Vec<Duration>> {
    let client = Client::connect(node, subject).expect("the client connects");
    let config = sdk::write::Config::new(names());
    let mut writer = client.open_writer(config).expect("the writer opens");
    let mut reader = (timed != Timed::Write).then(|| {
        let mut channels = names();
        if timed == Timed::Twice {
            channels.push(channels[0].clone());
        }
        let config = sdk::read::Config {
            channels,
            mode: MODE,
        };
        client.open_reader(config).expect("the reader opens")
    });
    let places = reader.as_ref().map_or(0, |reader| reader.places().len());
    let values = [(0, Value::I64(1)), (1, Value::I64(2))];
    let sdk = rounded(rounds, calls, || {
        spin(GAP);
        let mut start = Instant::now();
        let written = writer.write(&values);
        // Before the read: no frame comes for a write that is not taken.
        assert!(
            matches!(written, Ok(sdk::write::Outcome::Taken { .. })),
            "{written:?}"
        );
        if matches!(timed, Timed::Held | Timed::HeldWithin) {
            spin(HELD);
            start = Instant::now();
        }
        if let Some(reader) = &mut reader {
            let frame = if timed == Timed::HeldWithin {
                let frame = reader.next_within(HELD).expect("the reader reads");
                frame.expect("the client's thread holds the frame")
            } else {
                reader.next().expect("the reader reads")
            };
            assert_eq!(frame.count(), 1, "the frame of the write");
            for place in 0..places {
                black_box(frame.series(place));
                black_box(frame.lost(place));
            }
        }
        start.elapsed()
    });
    drop(reader);
    assert_eq!(writer.close(), Ok(()), "the writer closes");
    assert_eq!(client.close(), Ok(()), "the client closes");
    sdk
}

/// The spans of each of `rounds` rounds of `calls` calls of `call`, after
/// [`WARMUP`] calls.
fn rounded(
    rounds: usize,
    calls: usize,
    mut call: impl FnMut() -> Duration,
) -> Vec<Vec<Duration>> {
    for _ in 0..WARMUP {
        call();
    }
    let round = |_| (0..calls).map(|_| call()).collect();
    (0..rounds).map(round).collect()
}

/// The spans of each leg between this thread and a task on an `os` shard, in
/// `rounds` rounds of `calls` round trips.
fn legs(rounds: usize, calls: usize) -> (Vec<Vec<Duration>>, Vec<Vec<Duration>>) {
    let (to_task, mut taken) = tokio::sync::mpsc::channel::<Instant>(1);
    let (to_caller, answers) = mpsc::sync_channel::<(Duration, Instant)>(1);
    let main = move |tasks: Tasks| async move {
        let group = Group::new(tasks);
        while let Some(sent) = taken.recv().await {
            let span = sent.elapsed();
            to_caller
                .try_send((span, Instant::now()))
                .expect("the caller waits for each answer");
        }
        group.ended().await;
    };
    let handle = started("task", main);
    let mut back = Vec::new();
    let mut trip = || {
        spin(GAP);
        to_task
            .try_send(Instant::now())
            .expect("the task took the call before");
        let (span, sent) = answers.recv().expect("the task answers");
        back.push(sent.elapsed());
        span
    };
    let forth = rounded(rounds, calls, &mut trip);
    drop(to_task);
    handle.join().expect("the task ends");
    let back = back[WARMUP..].chunks(calls).map(<[_]>::to_vec).collect();
    (forth, back)
}

/// The spans of each of `rounds` rounds of `calls` writes of 2 values through a
/// `program::Writer` at `node`, on an `os` shard of its own, each with the read of its
/// frame through a `program::Reader` of the same session when `read`.
fn program(
    node: &sdk::Node,
    subject: &sdk::Subject,
    rounds: usize,
    calls: usize,
    read: bool,
) -> Vec<Vec<Duration>> {
    let (node, subject) = (node.clone(), subject.clone());
    let (sender, spans) = mpsc::channel();
    let main = move |tasks: Tasks| async move {
        let group = Group::new(tasks);
        let tasks = group.tasks().clone();
        let session = crate::plant::session(node, subject, tasks, 64 << 10).await;
        let config = Config {
            authority: Authority(0),
            lease: None,
            channels: names(),
        };
        let mut writer = session.writer(config).await.expect("the writer opens");
        let layout: Vec<_> = (0..writer.places().len()).map(|p| (p, 8)).collect();
        let mut reader = None;
        if read {
            let select = Selector::new(["plant.a", "plant.b"]).expect("a selector");
            let opened = session.reader(select, MODE).await;
            reader = Some(opened.expect("the reader opens"));
        }
        let mut all = Vec::with_capacity(WARMUP + rounds * calls);
        for _ in 0..WARMUP + rounds * calls {
            let start = Instant::now();
            write(&mut writer, &layout).await;
            if let Some(reader) = &mut reader {
                reader.next().await.expect("the reader reads");
            }
            all.push(start.elapsed());
        }
        drop(reader);
        writer.finish().await.expect("the writer finishes");
        drop(session);
        group.ended().await;
        sender.send(all).expect("the bench waits for the spans");
    };
    let handle = started("program", main);
    let all = spans.recv().expect("the program thread sends its spans");
    handle.join().expect("the program thread ends");
    all[WARMUP..].chunks(calls).map(<[_]>::to_vec).collect()
}

/// Writes a frame of 1 sample of each place of `layout`, a place and 8 bytes for each
/// place of `writer`, with the node's stamp.
async fn write(writer: &mut program::Writer, layout: &[(usize, usize)]) {
    let index = writer.index();
    let draft = writer
        .draft(Form::Raw, 1, layout)
        .await
        .expect("the frame fits the pool");
    for (place, _) in layout.iter().filter(|(place, _)| *place != index) {
        draft
            .series_mut(*place)
            .expect("the layout holds it")
            .fill(1);
    }
    let written = writer.write(Time::Node).await;
    assert!(matches!(written, Ok(Outcome::Taken { .. })), "{written:?}");
}

/// The channels that each writer writes.
fn names() -> Vec<Name> {
    ["plant.a", "plant.b"]
        .map(|name| name.parse().expect("a name"))
        .into()
}

/// The thread `name` on a shard of `os`, which runs `main`.
fn started<M, F>(name: &str, main: M) -> env::thread::Handle
where
    M: FnOnce(Tasks) -> F + Send + 'static,
    F: Future<Output = ()> + 'static,
{
    let config = env::shards::Config {
        name: name.into(),
        core: None,
    };
    let log = tracing::dispatcher::get_default(Clone::clone);
    let shards = os::shards().expect("the shards start").logged(log);
    shards.start(config, main).expect("the thread starts")
}

/// Spins for `span`.
fn spin(span: Duration) {
    let start = Instant::now();
    while start.elapsed() < span {
        std::hint::spin_loop();
    }
}

fn micros(span: Duration) -> f64 {
    span.as_secs_f64() * 1e6
}
