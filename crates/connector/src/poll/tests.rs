use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use document::Document;
use document::diagnostic::Diagnostic;
use env::clock::Clock;
use hub::Hub;
use hub::home::Outcome;
use hub::reader::{Mode, Received};
use spec::channel::{Channel, Data, Kind as ChannelKind};
use spec::data_type::DataType;
use spec::definition::Definition;
use types::authority::Authority;
use types::channel;
use types::frame::{Draft, Form, Label, Path, key_set};
use types::name::{Name, Selector};
use types::sample::Value::{F64, U16};
use types::sample::{Scalar, Type, Value};
use types::time::{Span, Stamp};

use super::Writer;
use crate::common::{env, run_on};
use crate::kind::{Channels, Context, Error, Kind, Table};
use crate::testing::Rig;
use env::tasks::Tasks;

/// The time between two writes of [`Polls`].
const TICK: Span = Span::from_nanos(100_000_000);

/// Of the channels of [`definitions`].
const CHANNELS: [&str; 3] = ["plant.a", "plant.c", "plant.b"];

fn name(text: &str) -> Name {
    text.parse().expect("a valid name")
}

/// Two indexes, `plant.time` (key 1) and `plant.clock` (key 4), and the data
/// channels of each: `plant.a` (key 2, `f64`) and `plant.b` (key 3, `i32`) of the
/// first, and `plant.c` (key 5, `u16`) and `plant.s` (key 6, `string`) of the second.
fn definitions() -> Vec<(Name, Definition)> {
    let index = |key| {
        let kind = ChannelKind::Index {
            error: None,
            control: None,
        };
        let key = channel::Key::from_u128(key);
        Definition::Channel(Channel { key, kind })
    };
    let data = |key, index, sample| {
        let index = channel::Key::from_u128(index);
        let data = Data::new(index, None, DataType::Sample(sample), None);
        let kind = ChannelKind::Data(data.expect("no unit"));
        let key = channel::Key::from_u128(key);
        Definition::Channel(Channel { key, kind })
    };
    vec![
        (name("plant.time"), index(1)),
        (name("plant.a"), data(2, 1, Type::Scalar(Scalar::F64))),
        (name("plant.b"), data(3, 1, Type::Scalar(Scalar::I32))),
        (name("plant.clock"), index(4)),
        (name("plant.c"), data(5, 4, Type::Scalar(Scalar::U16))),
        (name("plant.s"), data(6, 4, Type::String)),
    ]
}

/// A kind that opens a [`Writer`] of `channels`, keeps its types, and writes each of
/// `writes` one [`TICK`] after the last, until the cancel.
struct Polls {
    channels: [&'static str; 3],
    writes: Vec<Vec<Option<Value>>>,
    types: Arc<Mutex<Vec<Type>>>,
}

impl Kind for Polls {
    type Config = ();
    const SCHEMA: document::schema::Body = crate::testing::READS;

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels {
            writes: self.channels.map(name).to_vec(),
            counts: vec![name("in"), name("lost")],
            ..Channels::default()
        })
    }

    async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
        let channels = self.channels.map(name).to_vec();
        let Some(mut writer) = Writer::open(&ctx, channels).await? else {
            return Ok(());
        };
        *self.types.lock().expect("no panic") = writer.types().collect();
        for values in &self.writes {
            if ctx.cancel().race(ctx.clock().sleep(TICK)).await.is_none() {
                return Ok(());
            }
            let ((), stamp) = writer.request(std::future::ready(())).await;
            writer.write(values.iter().copied(), stamp)?;
        }
        ctx.cancel().wait().await;
        Ok(())
    }
}

/// The samples of one frame, by key: each index as `i64` ns and each value widened to
/// 8 bytes.
type Frame = BTreeMap<u128, u64>;

/// What a run of [`Polls`] recorded.
#[derive(Debug)]
struct Record {
    result: Result<(), Error>,
    /// The frames of `plant.time`, `plant.a`, and `plant.b`.
    time: Vec<Frame>,
    /// The frames of `plant.clock` and `plant.c`.
    clock: Vec<Frame>,
    types: Vec<Type>,
    /// The counts `in` and `lost` at the end.
    counts: (u64, u64),
}

/// Runs [`Polls`] of [`CHANNELS`] with `writes`, after `script` on the hub, the clock,
/// and the tasks of the node. Records each frame of the channels until the run ends,
/// 1 s after the counts `in` and `lost` are `counts`.
fn run<F: Future<Output = ()> + 'static>(
    writes: Vec<[Option<Value>; 3]>,
    counts: (u64, u64),
    script: impl FnOnce(Hub, Clock, Tasks) -> F + Send + 'static,
) -> Record {
    let writes = writes.into_iter().map(Vec::from).collect();
    run_of(CHANNELS, writes, counts, script)
}

/// As [`run`], with [`Polls`] of `channels`.
fn run_of<F: Future<Output = ()> + 'static>(
    channels: [&'static str; 3],
    writes: Vec<Vec<Option<Value>>>,
    counts: (u64, u64),
    script: impl FnOnce(Hub, Clock, Tasks) -> F + Send + 'static,
) -> Record {
    let types = Arc::new(Mutex::new(Vec::new()));
    let kind = Polls {
        channels,
        writes,
        types: Arc::clone(&types),
    };
    let mut record = run_on(move |node, tasks| async move {
        let kinds = Table::new().with("writes", kind);
        let rig = Rig::open(env(&node, tasks.clone()), node.net(), kinds).await;
        rig.define(definitions());
        let time =
            record(rig.hub(), &tasks, ["plant.time", "plant.a", "plant.b"]).await;
        let clock = record(rig.hub(), &tasks, ["plant.clock", "plant.c"]).await;
        script(rig.hub().clone(), node.clock(), tasks.clone()).await;
        let config = Document::default();
        let mut connector = rig.supervise("writes", name("plant.writes"), config).await;
        let reading = connector
            .until(Span::from_nanos(10_000_000_000), |reading| {
                (reading.counts[&name("in")], reading.counts[&name("lost")]) == counts
            })
            .await;
        let counts = (reading.counts[&name("in")], reading.counts[&name("lost")]);
        node.clock().sleep(Span::SECOND).await;
        Record {
            result: connector.stop().await,
            time: time.take(),
            clock: clock.take(),
            types: Vec::new(),
            counts,
        }
    });
    record.types = types.lock().expect("no panic").clone();
    record
}

/// Records each frame of the channels `select` of one index.
async fn record<const N: usize>(
    hub: &Hub,
    tasks: &Tasks,
    select: [&str; N],
) -> Rc<RefCell<Vec<Frame>>> {
    let config = hub::reader::Config {
        select: Selector::new(select).expect("a selector"),
        mode: Mode::Complete,
        subject: name("plant.test"),
        name: None,
        hold: Span::ZERO,
    };
    let mut reader = hub.reader(config).await.expect("the reader opens");
    let frames = Rc::new(RefCell::new(Vec::new()));
    let into = Rc::clone(&frames);
    tasks.spawn(async move {
        while let Ok(received) = reader.next().await {
            into.borrow_mut().push(decode(&received));
        }
    });
    frames
}

fn decode(received: &Received<'_>) -> Frame {
    let (set, view) = (received.set(), received.view());
    let mut frame = Frame::new();
    for (entry, bytes) in view.iter() {
        let entry = &set.entries()[entry];
        let range = view.range(entry.group).expect("the group is present");
        assert_eq!(range.count, 1, "one sample of each series");
        let mut raw = Vec::new();
        codec::decode(entry.data_type, 1, bytes, &mut raw).expect("decodes");
        let mut sample = [0; 8];
        sample[..raw.len()].copy_from_slice(&raw);
        frame.insert(entry.key.as_u128(), u64::from_le_bytes(sample));
    }
    frame
}

/// The values `a`, `c`, and `b` of one write, each `None` where `kept` is false.
fn values(a: f64, c: u16, b: i32, kept: [bool; 3]) -> [Option<Value>; 3] {
    let all = [Value::F64(a), Value::U16(c), Value::I32(b)];
    let mut values = [None; 3];
    for ((value, all), kept) in values.iter_mut().zip(all).zip(kept) {
        *value = kept.then_some(all);
    }
    values
}

/// The frames of `values` stamped `stamp`: of `plant.time` with `a` and `b`, and of
/// `plant.clock` with `c`, each `None` when it keeps no value.
fn frames(values: [Option<Value>; 3], stamp: i64) -> (Option<Frame>, Option<Frame>) {
    let widen = |value: Value| {
        let mut sample = [0; 8];
        value.write(&mut sample[..value.scalar().width()]);
        u64::from_le_bytes(sample)
    };
    let [a, c, b] = values.map(|value| value.map(widen));
    let frame = |series: &[(u128, Option<u64>)]| {
        let frame: Frame = (series.iter())
            .filter_map(|&(key, sample)| Some((key, sample?)))
            .collect();
        (!frame.is_empty()).then(|| {
            let mut frame = frame;
            let index = if frame.contains_key(&5) { 4 } else { 1 };
            frame.insert(index, stamp.cast_unsigned());
            frame
        })
    };
    (frame(&[(2, a), (3, b)]), frame(&[(5, c)]))
}

/// The stamp of the index `key` in `frame`.
fn stamp(frame: &Frame, key: u128) -> i64 {
    frame[&key].cast_signed()
}

#[test]
fn writes_each_value_and_the_stamp_of_the_request_in_each_index() {
    let all = [true; 3];
    let writes = vec![values(1.5, 7, -3, all), values(2.5, 8, -4, all)];
    let record = run(writes.clone(), (6, 0), |_, _, _| async {});
    assert_eq!(record.result.as_ref().map_err(ToString::to_string), Ok(&()));
    let scalar = |scalar| Type::Scalar(scalar);
    assert_eq!(
        record.types,
        [
            scalar(Scalar::F64),
            scalar(Scalar::U16),
            scalar(Scalar::I32)
        ]
    );
    assert_eq!(record.time.len(), 2, "{:?}", record.time);
    assert_eq!(record.clock.len(), 2, "{:?}", record.clock);
    let first = stamp(&record.time[0], 1);
    assert_eq!(stamp(&record.time[1], 1) - first, TICK.nanos());
    for (at, values) in writes.into_iter().enumerate() {
        let (time, clock) = frames(values, stamp(&record.time[at], 1));
        assert_eq!(Some(&record.time[at]), time.as_ref());
        assert_eq!(Some(&record.clock[at]), clock.as_ref());
    }
    assert_eq!(record.counts, (6, 0));
}

#[test]
fn leaves_out_each_none_and_writes_no_frame_that_keeps_none() {
    let writes = vec![
        values(1.5, 7, -3, [true, true, false]),
        values(2.5, 8, -4, [false, true, false]),
        values(3.5, 9, -5, [false; 3]),
        values(4.5, 10, -6, [false, false, true]),
        values(5.5, 11, -7, [true; 3]),
    ];
    let record = run(writes.clone(), (2 + 1 + 1 + 3, 0), |_, _, _| async {});
    assert_eq!(record.result.as_ref().map_err(ToString::to_string), Ok(&()));
    let start = stamp(&record.clock[0], 4);
    let (mut time, mut clock) = (Vec::new(), Vec::new());
    for (at, values) in (0..).zip(writes) {
        let (a, c) = frames(values, start + at * TICK.nanos());
        time.extend(a);
        clock.extend(c);
    }
    assert_eq!(record.time, time);
    assert_eq!(record.clock, clock);
    assert_eq!(record.counts, (7, 0));
}

#[test]
fn writes_a_group_refused_as_backwards_again_after_the_stamp_of_the_refusal() {
    let all = [true; 3];
    let writes = vec![values(1.5, 7, -3, all), values(2.5, 8, -4, all)];
    let ahead = Arc::new(Mutex::new(0));
    let into = Arc::clone(&ahead);
    let record = run(writes.clone(), (6, 0), |hub, _, _| async move {
        *into.lock().expect("no panic") =
            write_ahead(&hub, "plant.a", F64(0.0), Span::SECOND).await;
    });
    assert_eq!(record.result.as_ref().map_err(ToString::to_string), Ok(&()));
    let ahead = *ahead.lock().expect("no panic");
    let first = stamp(&record.clock[0], 4);
    assert!(first < ahead, "{first} < {ahead}");
    let (time, clock): (Vec<_>, Vec<_>) = [
        frames(writes[0], ahead + 1).0.map(|time| (time, first)),
        frames(writes[1], ahead + 2).0.map(|time| (time, ahead + 2)),
    ]
    .into_iter()
    .flatten()
    .unzip();
    let mut want = vec![Frame::from([(1, ahead.cast_unsigned()), (2, 0)])];
    want.extend(time);
    assert_eq!(record.time, want);
    let want: Vec<_> = (writes.iter().zip(clock))
        .filter_map(|(&values, stamp)| frames(values, stamp).1)
        .collect();
    assert_eq!(record.clock, want);
    assert_eq!(record.counts, (6, 0));
}

/// `plant.clock` is the second group of the frame, so a write again of each series
/// sends `plant.time` with no sample, which the home refuses.
#[test]
fn writes_again_only_the_series_of_the_group_refused_as_backwards() {
    let all = [true; 3];
    let writes = vec![values(1.5, 7, -3, all), values(2.5, 8, -4, all)];
    let ahead = Arc::new(Mutex::new(0));
    let into = Arc::clone(&ahead);
    let record = run(writes.clone(), (6, 0), |hub, _, _| async move {
        *into.lock().expect("no panic") =
            write_ahead(&hub, "plant.c", U16(0), Span::SECOND).await;
    });
    assert_eq!(record.result.as_ref().map_err(ToString::to_string), Ok(&()));
    let ahead = *ahead.lock().expect("no panic");
    let first = stamp(&record.time[0], 1);
    assert!(first < ahead, "{first} < {ahead}");
    let want: Vec<_> = [(writes[0], first), (writes[1], ahead + 2)]
        .into_iter()
        .filter_map(|(values, stamp)| frames(values, stamp).0)
        .collect();
    assert_eq!(record.time, want);
    let mut want = vec![Frame::from([(4, ahead.cast_unsigned()), (5, 0)])];
    want.extend(
        [(writes[0], ahead + 1), (writes[1], ahead + 2)]
            .into_iter()
            .filter_map(|(values, stamp)| frames(values, stamp).1),
    );
    assert_eq!(record.clock, want);
    assert_eq!(record.counts, (6, 0));
}

/// The home refuses both groups as `Backwards`, the first with the later stamp, so
/// the write again of both is after that stamp.
#[test]
fn writes_groups_refused_as_backwards_again_after_the_latest_stamp() {
    let all = [true; 3];
    let writes = vec![values(1.5, 7, -3, all), values(2.5, 8, -4, all)];
    let ahead = Arc::new(Mutex::new((0, 0)));
    let into = Arc::clone(&ahead);
    let record = run(writes.clone(), (6, 0), |hub, _, _| async move {
        let a = write_ahead(&hub, "plant.a", F64(0.0), Span::SECOND).await;
        let half = Span::from_nanos(500_000_000);
        let c = write_ahead(&hub, "plant.c", U16(0), half).await;
        *into.lock().expect("no panic") = (a, c);
    });
    assert_eq!(record.result.as_ref().map_err(ToString::to_string), Ok(&()));
    let (a, c) = *ahead.lock().expect("no panic");
    assert!(c < a, "{c} < {a}");
    let (mut time, mut clock) = (
        vec![Frame::from([(1, a.cast_unsigned()), (2, 0)])],
        vec![Frame::from([(4, c.cast_unsigned()), (5, 0)])],
    );
    for (at, values) in (1..).zip(writes) {
        let (a, c) = frames(values, a + at);
        time.extend(a);
        clock.extend(c);
    }
    assert_eq!(record.time, time);
    assert_eq!(record.clock, clock);
    assert_eq!(record.counts, (6, 0));
}

/// Writes `value` to `channel` as `plant.rival`, `ahead` after the mesh time, and gives
/// that stamp.
async fn write_ahead(hub: &Hub, channel: &str, value: Value, ahead: Span) -> i64 {
    let mut rival = rival(hub, Authority::ABSOLUTE, &[channel]).await;
    let set = Arc::clone(rival.set());
    let data = rival.entries()[0];
    let width =
        |entry: &key_set::Entry| entry.data_type.width().expect("a fixed width");
    let series: Vec<_> = set.entries().iter().map(width).enumerate().collect();
    let mut draft = rival.draft(Form::Raw, &series).expect("a frame");
    let time = rival.now() + ahead;
    for (entry, bytes) in draft.iter_mut() {
        if entry == data {
            value.write(bytes);
        } else {
            Value::Stamp(time).write(bytes);
        }
    }
    draft.set_count(set.entries()[data].group, 1);
    let outcomes = rival
        .write(Label::Path(Path::Live), draft)
        .expect("a write");
    assert!(
        matches!(outcomes, [Outcome::Applied { .. }]),
        "{outcomes:?}"
    );
    time.nanos()
}

/// Opens a writer of `channels` at `authority` as `plant.rival`.
async fn rival(
    hub: &Hub,
    authority: Authority,
    channels: &[&str],
) -> hub::writer::Writer {
    let config = hub::writer::Config {
        subject: name("plant.rival"),
        authority,
        lease: None,
        channels: channels.iter().map(|&channel| name(channel)).collect(),
    };
    hub.writer(config).await.expect("opens")
}

/// The pool has a block for the draft of no write until the third, after the open of
/// the writer.
#[test]
fn loses_the_samples_of_a_frame_for_which_the_pool_has_no_block() {
    let all = [true; 3];
    let writes = vec![
        values(1.5, 7, -3, all),
        values(2.5, 8, -4, all),
        values(3.5, 9, -5, all),
    ];
    let record = run(writes.clone(), (3, 6), |hub, clock, tasks| async move {
        let rival = rival(&hub, Authority(1), &CHANNELS).await;
        tasks.spawn(async move {
            clock.sleep(Span::from_nanos(50_000_000)).await;
            let held = fill(&rival);
            clock.sleep(Span::from_nanos(200_000_000)).await;
            drop(held);
            drop(rival);
        });
    });
    assert_eq!(record.result.as_ref().map_err(ToString::to_string), Ok(&()));
    let (time, clock) = frames(writes[2], stamp(&record.time[0], 1));
    assert_eq!(record.time, Vec::from_iter(time));
    assert_eq!(record.clock, Vec::from_iter(clock));
    assert_eq!(record.counts, (3, 6));
}

/// The pool has one block free for the draft of each write, and none for the home,
/// which loses each group of the first two writes.
#[test]
fn loses_the_samples_of_a_frame_that_the_home_loses() {
    let all = [true; 3];
    let writes = vec![
        values(1.5, 7, -3, all),
        values(2.5, 8, -4, all),
        values(3.5, 9, -5, all),
    ];
    let record = run(writes.clone(), (3, 6), |hub, clock, tasks| async move {
        let rival = rival(&hub, Authority(1), &["plant.a"]).await;
        let mut held = fill(&rival);
        drop(held.pop());
        tasks.spawn(async move {
            clock.sleep(Span::from_nanos(250_000_000)).await;
            drop(held);
            drop(rival);
        });
    });
    assert_eq!(record.result.as_ref().map_err(ToString::to_string), Ok(&()));
    let (time, clock) = frames(writes[2], stamp(&record.time[0], 1));
    assert_eq!(record.time, Vec::from_iter(time));
    assert_eq!(record.clock, Vec::from_iter(clock));
    assert_eq!(record.counts, (3, 6));
}

/// Drafts frames of `writer` until the pool has no block, and gives them.
fn fill(writer: &hub::writer::Writer) -> Vec<Draft> {
    let mut held = Vec::new();
    for len in [1 << 16, 1 << 12, 1 << 8, 8] {
        let series: Vec<_> = (0..writer.set().entries().len())
            .map(|entry| (entry, len))
            .collect();
        while let Ok(draft) = writer.draft(Form::Raw, &series) {
            held.push(draft);
        }
    }
    held
}

#[test]
fn loses_the_samples_of_a_group_that_another_writer_controls() {
    let all = [true; 3];
    let writes = vec![values(1.5, 7, -3, all), values(2.5, 8, -4, all)];
    let record = run(writes.clone(), (2, 4), |hub, clock, tasks| async move {
        let rival = rival(&hub, Authority::ABSOLUTE, &["plant.a"]).await;
        tasks.spawn(async move {
            clock.sleep(Span::from_nanos(10_000_000_000)).await;
            drop(rival);
        });
    });
    assert_eq!(record.result.as_ref().map_err(ToString::to_string), Ok(&()));
    assert_eq!(record.time, []);
    let start = stamp(&record.clock[0], 4);
    let clock: Vec<_> = (0..)
        .zip(writes)
        .filter_map(|(at, values)| frames(values, start + at * TICK.nanos()).1)
        .collect();
    assert_eq!(record.clock, clock);
    assert_eq!(record.counts, (2, 4));
}

/// The diagnostics of `record`, which ends with [`Error::Config`].
fn diagnostics(record: &Record) -> Vec<(String, String, String)> {
    let Err(Error::Config(diagnostics)) = &record.result else {
        panic!("a config error, not {:?}", record.result)
    };
    (diagnostics.iter())
        .map(|diagnostic| {
            let code = diagnostic.code.to_string();
            (code, diagnostic.message.clone(), diagnostic.fix.clone())
        })
        .collect()
}

#[test]
fn refuses_a_channel_that_is_an_index() {
    let channels = ["plant.time", "plant.a", "plant.b"];
    let stamp = Value::Stamp(Stamp::from_nanos(1));
    let writes = vec![vec![Some(stamp), Some(F64(1.5)), Some(Value::I32(-3))]];
    let record = run_of(channels, writes, (0, 0), |_, _, _| async {});
    let message = "the channel `plant.time` is an index, which holds the stamp of each \
                   request";
    let want = (
        "connector.index",
        message,
        "Write the value to a data channel",
    );
    let want =
        [want].map(|(code, message, fix)| (code.into(), message.into(), fix.into()));
    assert_eq!(diagnostics(&record), want);
    assert_eq!((record.time, record.counts), (Vec::new(), (0, 0)));
}

#[test]
fn refuses_each_channel_that_is_an_index_in_order() {
    let channels = ["plant.time", "plant.a", "plant.clock"];
    let stamp = Value::Stamp(Stamp::from_nanos(1));
    let writes = vec![vec![Some(stamp), Some(F64(1.5)), Some(stamp)]];
    let record = run_of(channels, writes, (0, 0), |_, _, _| async {});
    let want = ["plant.time", "plant.clock"].map(|name| {
        let message = format!(
            "the channel `{name}` is an index, which holds the stamp of each request"
        );
        (
            "connector.index".into(),
            message,
            "Write the value to a data channel".into(),
        )
    });
    assert_eq!(diagnostics(&record), want);
    assert_eq!((record.time, record.counts), (Vec::new(), (0, 0)));
}

#[test]
#[should_panic(
    expected = "invariant: the parse of the kind refuses the channel `plant.a` again"
)]
fn panics_at_a_channel_named_again() {
    let channels = ["plant.a", "plant.a", "plant.b"];
    let writes = vec![vec![Some(F64(1.5)), None, Some(Value::I32(-3))]];
    run_of(channels, writes, (0, 0), |_, _, _| async {});
}

/// `plant.a` is an `f64`, so an `i64` of the same width panics.
#[test]
#[should_panic(
    expected = "invariant: the value of channel 0 is i64, and its samples are f64"
)]
fn panics_at_a_value_of_another_type() {
    let writes = vec![[Some(Value::I64(1)), Some(U16(7)), Some(Value::I32(-3))]];
    run(writes, (0, 0), |_, _, _| async {});
}

/// `plant.s` is a `string`, so the `u16` value of its place panics.
#[test]
#[should_panic(
    expected = "invariant: the value of channel 1 is u16, and its samples are not \
                scalars"
)]
fn panics_at_a_value_of_a_channel_of_no_fixed_width() {
    let channels = ["plant.a", "plant.s", "plant.b"];
    let writes = vec![Vec::from(values(1.5, 7, -3, [true; 3]))];
    run_of(channels, writes, (0, 0), |_, _, _| async {});
}

/// Calls the private `value` with a `None` that `Kept::sort` never keeps, so no
/// write reaches it. It is the only kill of a `value` that gives a value for a left
/// out channel in place of the panic.
#[test]
#[should_panic(
    expected = "invariant: a frame keeps no value of channel 1, which is `None`"
)]
fn panics_with_the_channel_whose_value_is_left_out() {
    super::value(&[Some(Value::F64(1.0)), None], 1, Some(Scalar::F64));
}

/// One value more than the channels.
#[test]
#[should_panic(expected = "invariant: a kind gives more values than its 3 channels")]
fn panics_at_values_of_another_length() {
    let mut values = Vec::from(values(1.5, 7, -3, [true; 3]));
    values.push(Some(U16(8)));
    run_of(CHANNELS, vec![values], (0, 0), |_, _, _| async {});
}

/// One value less than the channels.
#[test]
#[should_panic(expected = "invariant: a kind gives 2 values for 3 channels")]
fn panics_at_fewer_values_than_channels() {
    let mut values = Vec::from(values(1.5, 7, -3, [true; 3]));
    values.pop();
    run_of(CHANNELS, vec![values], (0, 0), |_, _, _| async {});
}

/// One value less than the channels, after a frame that keeps each value, whose slot
/// of the last channel is still full.
#[test]
#[should_panic(expected = "invariant: a kind gives 2 values for 3 channels")]
fn panics_at_fewer_values_after_a_frame_that_keeps_each() {
    let all = Vec::from(values(1.5, 7, -3, [true; 3]));
    let mut short = Vec::from(values(2.5, 8, -4, [true; 3]));
    short.pop();
    run_of(CHANNELS, vec![all, short], (0, 0), |_, _, _| async {});
}
