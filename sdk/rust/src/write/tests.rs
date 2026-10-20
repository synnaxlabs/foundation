// These tests serve the calls of a writer on a thread of their own, in place of the
// task of the client's thread, whose `program::Writer` needs a node. The tests of
// `crates/node/tests/it/sdk.rs` run the task against a node process.

use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};

use types::channel;
use types::time::Stamp;

use super::*;

const WAITING: Outcome = Outcome::Refused(Refusal::Waiting);

/// The [`Writer::bytes_max`] of each writer of these tests.
const BYTES_MAX: usize = 128;

/// Runs `serve` on a thread of its own, in place of the client's thread.
#[expect(
    clippy::disallowed_methods,
    reason = "a thread stands in for the client's thread"
)]
fn spawn<T: Send + 'static>(
    serve: impl FnOnce() -> T + Send + 'static,
) -> JoinHandle<T> {
    thread::spawn(serve)
}

/// The next open on `opened`, which is the open of a writer.
fn received(opened: &mut mpsc::UnboundedReceiver<crate::Open>) -> Open {
    let Some(crate::Open::Write(open)) = opened.blocking_recv() else {
        panic!("the open of a writer");
    };
    open
}

/// The place of the node of the channel of key `at`, of `scalar` samples, named `name`,
/// whose index is place `index` of the node.
fn place(at: u128, scalar: Scalar, name: &str, index: usize) -> program::Place {
    program::Place {
        key: channel::Key::from_u128(at),
        data_type: Type::Scalar(scalar),
        name: name.parse().unwrap(),
        index,
    }
}

/// `time`, an index at place 0, then `count` (`i64`) and `level` (`f64`) of it, then
/// `clock`, an index at place 3, and `heat` (`i64`) of it.
fn places() -> Vec<program::Place> {
    vec![
        place(1, Scalar::Stamp, "time", 0),
        place(2, Scalar::I64, "count", 0),
        place(3, Scalar::F64, "level", 0),
        place(4, Scalar::Stamp, "clock", 3),
        place(5, Scalar::I64, "heat", 3),
    ]
}

/// The place of the writer of `place` of `places()`, whose index is place `index`.
fn at(place: usize, index: usize) -> Place {
    let program::Place {
        key,
        data_type,
        name,
        ..
    } = places().swap_remove(place);
    Place {
        key,
        data_type,
        name,
        index,
    }
}

/// The frame and time of each write that a writer sent, until its first close.
type Sent = Vec<(Frame, Time)>;

/// What a write gives: the place of the index and the outcome of each group.
type Given<'a> = Result<&'a [(usize, Outcome)], Error>;

/// A writer of `count` and `level`, so places 0, 1, and 2 are `count`, `level`, and
/// `time`, whose calls a thread serves: it answers each group of each write with
/// `outcome` and the close with `Ok`, and gives each write that it served.
fn writer(outcome: Outcome) -> (Writer, JoinHandle<Sent>) {
    writer_of(&["count", "level"], move |_| outcome)
}

/// A writer of `channels`, served as [`writer`] serves it, with the outcome of the
/// group of each place of the node of an index from `outcome`.
fn writer_of(
    channels: &[&str],
    outcome: impl Fn(usize) -> Outcome + Send + 'static,
) -> (Writer, JoinHandle<Sent>) {
    let (opens, mut opened) = mpsc::unbounded_channel();
    let served = spawn(move || {
        let Open {
            opened: answer,
            mut calls,
            written,
            ..
        } = received(&mut opened);
        answer.send(Ok((places(), BYTES_MAX))).unwrap();
        let mut sent = Vec::new();
        while let Some(call) = calls.blocking_recv() {
            match call {
                Call::Write { mut frame, time } => {
                    let kept = Frame {
                        parts: frame.parts.clone(),
                        series: frame.series.clone(),
                        bytes: frame.bytes.clone(),
                        outcomes: Vec::new(),
                    };
                    sent.push((kept, time));
                    answered(&mut frame, &outcome);
                    written.send((Ok(()), frame)).unwrap();
                }
                Call::Close(finished) => {
                    finished.send(Ok(())).unwrap();
                    break;
                }
            }
        }
        sent
    });
    let channels = channels.iter().map(|name| name.parse().unwrap()).collect();
    (
        Writer::open(&opens, &Arc::default(), Config::new(channels)).unwrap(),
        served,
    )
}

/// Puts the outcome of each group of `frame` in it, as `program::Writer::write` gives
/// them, but in the reverse order of the places of the node, which the writer sorts.
fn answered(frame: &mut Frame, outcome: impl Fn(usize) -> Outcome) {
    let entries = places();
    let mut groups: Vec<_> = (frame.series.iter())
        .map(|&(node, ..)| entries[node].index)
        .collect();
    groups.sort_unstable_by(|a, b| b.cmp(a));
    groups.dedup();
    frame.outcomes = groups
        .into_iter()
        .map(|group| (group, outcome(group)))
        .collect();
}

/// A writer of `channels` whose open gives `places()`.
fn opened(channels: &[&str]) -> Writer {
    let (opens, mut opened) = mpsc::unbounded_channel();
    let served = spawn(move || {
        let open = received(&mut opened);
        open.opened.send(Ok((places(), BYTES_MAX))).unwrap();
        open
    });
    let channels = channels.iter().map(|name| name.parse().unwrap()).collect();
    let writer = Writer::open(&opens, &Arc::default(), Config::new(channels)).unwrap();
    drop(served.join().unwrap());
    writer
}

/// The bytes of each series of `frame`, a sorted frame.
fn series(frame: &Frame) -> Vec<(usize, Vec<u8>)> {
    let each = frame.each().map(|(place, bytes)| (place, bytes.to_vec()));
    each.collect()
}

/// A series of `count` samples of `bytes` at `place`.
fn of(place: usize, count: u32, bytes: &[u8]) -> Series<'_> {
    Series {
        place,
        count,
        bytes,
    }
}

#[test]
fn a_config_of_new_has_authority_0_and_no_lease() {
    let config = Config::new(vec!["count".parse().unwrap()]);
    assert_eq!(config.authority, Authority(0));
    assert_eq!(config.lease, None);
    assert_eq!(config.channels, ["count".parse().unwrap()]);
}

#[test]
fn a_writer_moves_between_threads() {
    fn sent<T: Send>() {}
    sent::<Writer>();
}

#[test]
fn an_open_sends_its_config_and_gives_its_channels_in_its_order_then_the_index() {
    let (opens, mut opened) = mpsc::unbounded_channel();
    let lease = Span::from_nanos(5);
    let served = spawn(move || {
        let open = received(&mut opened);
        open.opened.send(Ok((places(), BYTES_MAX))).unwrap();
        open.config
    });
    let config = Config {
        channels: vec!["level".parse().unwrap(), "count".parse().unwrap()],
        authority: Authority(7),
        lease: Some(lease),
    };
    let writer = Writer::open(&opens, &Arc::default(), config.clone()).unwrap();
    assert_eq!(writer.places(), [at(2, 2), at(1, 2), at(0, 2)]);
    let sent = served.join().unwrap();
    assert_eq!(
        sent,
        program::write::Config {
            authority: Authority(7),
            lease: Some(lease),
            channels: config.channels,
        }
    );
}

#[test]
fn an_open_gives_the_index_of_each_group_it_does_not_name_in_the_order_of_its_first_channel()
 {
    let writer = opened(&["heat", "level", "count"]);
    let places = [at(4, 3), at(2, 4), at(1, 4), at(3, 3), at(0, 4)];
    assert_eq!(writer.places(), places);
    let writer = opened(&["count", "clock", "heat"]);
    assert_eq!(writer.places(), [at(1, 3), at(3, 1), at(4, 1), at(0, 3)]);
}

#[test]
fn a_writer_that_names_the_index_gives_it_at_its_place() {
    let writer = opened(&["count", "time"]);
    assert_eq!(writer.places(), [at(1, 1), at(0, 1)]);
}

#[test]
fn a_writer_that_names_a_channel_twice_gives_it_at_each_place() {
    let writer = opened(&["time", "count", "time", "count"]);
    assert_eq!(writer.places(), [at(0, 0), at(1, 0), at(0, 0), at(1, 0)]);
}

#[test]
fn an_open_gives_the_error_of_the_node() {
    let (opens, mut opened) = mpsc::unbounded_channel();
    let served = spawn(move || {
        let open = received(&mut opened);
        let refused = Error::Stream(program::Error::Unanswered);
        open.opened.send(Err(refused)).unwrap();
    });
    let config = Config::new(vec!["count".parse().unwrap()]);
    let opened = Writer::open(&opens, &Arc::default(), config).map(drop);
    assert_eq!(opened, Err(Error::Stream(program::Error::Unanswered)));
    served.join().unwrap();
}

/// The flag of a client that closed or dropped, and the error of a call whose task
/// ended, then of a flag of a client that did not.
const LOST: [(bool, Error); 2] = [(true, Error::Closed), (false, Error::Panicked)];

#[test]
fn an_open_whose_thread_ended_gives_closed_after_a_close_else_panicked() {
    for (closed, error) in LOST {
        let closed = Arc::new(AtomicBool::new(closed));
        let config = Config::new(vec!["count".parse().unwrap()]);
        let (opens, opened) = mpsc::unbounded_channel();
        drop(opened);
        let open = Writer::open(&opens, &closed, config.clone()).map(drop);
        assert_eq!(open, Err(error.clone()));
        let (opens, mut opened) = mpsc::unbounded_channel();
        let served = spawn(move || drop(opened.blocking_recv()));
        let open = Writer::open(&opens, &closed, config).map(drop);
        assert_eq!(open, Err(error));
        served.join().unwrap();
    }
}

#[test]
fn a_write_sends_one_sample_of_each_value_and_a_stamp_of_0_in_place_order() {
    let (mut writer, served) = writer(WAITING);
    let values = [(1, Value::F64(1.5)), (0, Value::I64(-2))];
    assert_eq!(writer.write(&values), Ok(&[(2, WAITING)][..]));
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    let [(frame, time)] = sent.as_slice() else {
        panic!("{} writes", sent.len());
    };
    assert_eq!(*time, Time::Node);
    assert_eq!(frame.series, [(0, 1, 8), (1, 1, 8), (2, 1, 8)]);
    assert_eq!(
        series(frame),
        [
            (0, vec![0; 8]),
            (1, (-2_i64).to_le_bytes().to_vec()),
            (2, 1.5_f64.to_le_bytes().to_vec()),
        ]
    );
}

#[test]
fn a_write_with_a_value_of_the_index_sends_it_as_the_stamp_of_the_frame() {
    let (mut writer, served) = writer(WAITING);
    let values = [(0, Value::I64(3)), (2, Value::Stamp(Stamp::from_nanos(9)))];
    assert_eq!(writer.write(&values), Ok(&[(2, WAITING)][..]));
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    assert_eq!(sent[0].1, Time::Frame(Path::Live));
    assert_eq!(sent[0].0.series, [(0, 1, 8), (1, 1, 8)]);
    assert_eq!(
        series(&sent[0].0),
        [
            (0, 9_i64.to_le_bytes().to_vec()),
            (1, 3_i64.to_le_bytes().to_vec())
        ]
    );
}

/// The outcome of the group of `time` (node place 0) and of `clock` (node place 3).
fn by_group(group: usize) -> Outcome {
    match group {
        0 => Outcome::Lost {
            range: frame::Range { seq: 4, count: 1 },
        },
        _ => WAITING,
    }
}

#[test]
fn a_write_of_two_groups_with_no_stamp_sends_a_stamp_of_0_for_each_group() {
    // Places: heat 0, count 1, then clock 2 and time 3.
    let (mut writer, served) = writer_of(&["heat", "count"], by_group);
    let values = [(1, Value::I64(7)), (0, Value::I64(8))];
    let given: Given<'_> = Ok(&[(2, WAITING), (3, by_group(0))]);
    assert_eq!(writer.write(&values), given);
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    let (frame, time) = &sent[0];
    assert_eq!(*time, Time::Node);
    assert_eq!(frame.series, [(0, 1, 8), (1, 1, 8), (3, 1, 8), (4, 1, 8)]);
    assert_eq!(
        series(frame),
        [
            (0, vec![0; 8]),
            (1, 7_i64.to_le_bytes().to_vec()),
            (3, vec![0; 8]),
            (4, 8_i64.to_le_bytes().to_vec()),
        ]
    );
}

#[test]
fn a_write_that_stamps_each_group_sends_its_stamps() {
    let (mut writer, served) = writer_of(&["heat", "count"], by_group);
    let stamp = |nanos| Value::Stamp(Stamp::from_nanos(nanos));
    let values = [(3, stamp(5)), (0, Value::I64(8)), (2, stamp(6))];
    let given: Given<'_> = Ok(&[(2, WAITING), (3, by_group(0))]);
    assert_eq!(writer.write(&values), given);
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    let (frame, time) = &sent[0];
    assert_eq!(*time, Time::Frame(Path::Live));
    assert_eq!(frame.series, [(0, 1, 8), (3, 1, 8), (4, 1, 8)]);
    assert_eq!(series(frame)[0], (0, 5_i64.to_le_bytes().to_vec()));
    assert_eq!(series(frame)[1], (3, 6_i64.to_le_bytes().to_vec()));
}

#[test]
fn a_write_that_stamps_some_groups_gives_index_absent_in_its_places_and_sends_nothing()
{
    let (mut writer, served) = writer_of(&["heat", "count", "level"], by_group);
    let stamp = Value::Stamp(Stamp::from_nanos(5));
    let values = [(2, Value::F64(1.0)), (0, Value::I64(8)), (3, stamp)];
    assert_eq!(writer.write(&values), absent(2, 4));
    let values = [(0, Value::I64(8)), (2, Value::F64(1.0)), (4, stamp)];
    assert_eq!(writer.write(&values), absent(0, 3));
    let values = [(0, Value::I64(8)), (3, stamp), (2, Value::F64(1.0))];
    let (count, level) = (1_i64.to_le_bytes(), 2.5_f64.to_le_bytes());
    let only = [of(2, 1, &level), of(1, 1, &count), of(0, 1, &count)];
    let stamps = 5_i64.to_le_bytes();
    let stamped = [of(1, 1, &count), of(4, 1, &stamps), of(0, 1, &count)];
    assert_eq!(writer.write_series(&only), absent(0, 3));
    assert_eq!(writer.write_series(&stamped), absent(0, 3));
    let given: Given<'_> = Ok(&[(3, WAITING), (4, by_group(0))]);
    let values = [values[0], values[1], values[2], (4, stamp)];
    assert_eq!(writer.write(&values), given);
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(served.join().unwrap().len(), 1);
}

fn absent(entry: usize, index: usize) -> Given<'static> {
    layout(frame::Error::IndexAbsent { entry, index })
}

#[test]
fn a_write_of_series_of_two_groups_sends_the_count_of_each() {
    let (mut writer, served) = writer_of(&["heat", "count"], by_group);
    let (two, three) = ([0; 16], [1; 24]);
    let both = [
        of(3, 3, &three),
        of(0, 2, &two),
        of(1, 3, &three),
        of(2, 2, &two),
    ];
    let given: Given<'_> = Ok(&[(2, WAITING), (3, by_group(0))]);
    assert_eq!(writer.write_series(&both), given);
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    let series = [(0, 3, 24), (1, 3, 24), (3, 2, 16), (4, 2, 16)];
    assert_eq!(sent[0].0.series, series);
}

#[test]
fn a_write_of_series_of_one_group_with_two_counts_gives_count_in_its_places() {
    // Places: heat 0, count 1, level 2, then clock 3 and time 4.
    let (mut writer, served) = writer_of(&["heat", "count", "level"], by_group);
    let count = |place, count, other, other_count| {
        Err(Error::Stream(program::Error::Count {
            place,
            count,
            other,
            other_count,
        }))
    };
    let (one, two) = ([0; 8], [0; 16]);
    let uneven = [
        of(4, 2, &two),
        of(2, 1, &one),
        of(1, 2, &two),
        of(0, 1, &one),
        of(3, 1, &one),
    ];
    assert_eq!(writer.write_series(&uneven), count(1, 2, 2, 1));
    let both = [
        of(4, 1, &one),
        of(2, 1, &one),
        of(1, 2, &two),
        of(0, 2, &two),
        of(3, 1, &one),
    ];
    assert_eq!(writer.write_series(&both), count(0, 2, 3, 1));
    let even = [
        of(4, 1, &one),
        of(1, 1, &one),
        of(0, 2, &two),
        of(3, 2, &two),
    ];
    let given: Given<'_> = Ok(&[(3, WAITING), (4, by_group(0))]);
    assert_eq!(writer.write_series(&even), given);
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(served.join().unwrap().len(), 1);
}

#[test]
fn a_write_where_a_group_ended_gives_partial_and_the_writer_goes_on() {
    let ended = |group| match group {
        0 => Outcome::Ended(End::Removed),
        _ => Outcome::Taken {
            range: frame::Range { seq: 0, count: 1 },
        },
    };
    // Places: count 0, heat 1, level 2, then time 3 and clock 4.
    let (mut writer, served) = writer_of(&["count", "heat", "level"], ended);
    let names = |names: &[&str]| -> Vec<Name> {
        names.iter().map(|name| name.parse().unwrap()).collect()
    };
    let partial: Given<'_> = Err(Error::Partial {
        written: names(&["heat", "clock"]),
        unwritten: names(&["count", "level", "time"]),
        outcomes: vec![(3, ended(0)), (4, ended(3))],
    });
    let values = [(2, Value::F64(1.0)), (1, Value::I64(2)), (0, Value::I64(3))];
    assert_eq!(writer.write(&values), partial);
    let given: Given<'_> = Ok(&[(4, ended(3))]);
    assert_eq!(writer.write(&[(1, Value::I64(4))]), given);
    let partial: Given<'_> = Err(Error::Partial {
        written: Vec::new(),
        unwritten: names(&["count", "time"]),
        outcomes: vec![(3, ended(0))],
    });
    assert_eq!(writer.write(&[(0, Value::I64(5))]), partial);
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(served.join().unwrap().len(), 3);
}

#[test]
fn a_write_of_a_value_of_another_type_than_its_place_sends_nothing() {
    let (mut writer, served) = writer(WAITING);
    let cases = [
        ((0, Value::F64(1.0)), Scalar::I64),
        ((2, Value::I64(1)), Scalar::Stamp),
    ];
    for ((place, value), held) in cases {
        let refused = writer.write(&[(1, Value::F64(2.0)), (place, value)]);
        let error = Error::Value {
            place,
            scalar: value.scalar(),
            data_type: Type::Scalar(held),
        };
        assert_eq!(refused, Err(error));
    }
    assert_eq!(writer.write(&[(0, Value::I64(4))]), Ok(&[(2, WAITING)][..]));
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(series(&sent[0].0)[1], (1, 4_i64.to_le_bytes().to_vec()));
}

#[test]
fn a_call_of_a_place_past_the_places_gives_out_of_range_and_sends_nothing() {
    let (mut writer, served) = writer(WAITING);
    let past = Err(Error::Stream(program::Error::Layout(
        frame::Error::OutOfRange {
            entry: 3,
            entries: 3,
        },
    )));
    assert_eq!(writer.write(&[(0, Value::I64(1)), (3, Value::U8(1))]), past);
    let series_of = [of(0, 1, &[0; 8]), of(3, 1, &[1])];
    assert_eq!(writer.write_series(&series_of), past);
    assert_eq!(writer.write(&[(0, Value::I64(2))]), Ok(&[(2, WAITING)][..]));
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(series(&sent[0].0)[1], (1, 2_i64.to_le_bytes().to_vec()));
}

fn layout(error: frame::Error) -> Given<'static> {
    Err(Error::Stream(program::Error::Layout(error)))
}

#[test]
fn a_call_of_two_series_of_one_channel_gives_unordered_in_its_places_and_sends_nothing()
{
    let (mut writer, served) = writer(WAITING);
    let twice = [
        (1, Value::F64(1.0)),
        (0, Value::I64(1)),
        (1, Value::F64(2.0)),
    ];
    let unordered = |entry, last| layout(frame::Error::Unordered { entry, last });
    assert_eq!(writer.write(&twice), unordered(1, 1));
    let (stamp, count) = (1_i64.to_le_bytes(), 2_i64.to_le_bytes());
    let series_of = [of(0, 1, &count), of(2, 1, &stamp), of(0, 1, &count)];
    assert_eq!(writer.write_series(&series_of), unordered(0, 0));
    assert_eq!(writer.write(&[(0, Value::I64(4))]), Ok(&[(2, WAITING)][..]));
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(served.join().unwrap().len(), 1);
    let (mut writer, served) = writer_of(&["count", "count"], |_| WAITING);
    let both = [(1, Value::I64(1)), (0, Value::I64(2))];
    assert_eq!(writer.write(&both), unordered(1, 0));
    assert_eq!(writer.write(&[(1, Value::I64(3))]), Ok(&[(2, WAITING)][..]));
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(series(&sent[0].0)[1], (1, 3_i64.to_le_bytes().to_vec()));
}

#[test]
fn a_write_of_series_with_none_of_the_index_gives_index_absent_and_sends_nothing() {
    let (mut writer, served) = writer(WAITING);
    let (level, count) = (1.5_f64.to_le_bytes(), 2_i64.to_le_bytes());
    let series_of = [of(1, 1, &level), of(0, 1, &count)];
    assert_eq!(writer.write_series(&series_of), absent(0, 2));
    assert_eq!(writer.write_series(&[of(1, 1, &level)]), absent(1, 2));
    let stamp = 1_i64.to_le_bytes();
    assert_eq!(
        writer.write_series(&[of(1, 1, &level), of(2, 1, &stamp)]),
        Ok(&[(2, WAITING)][..])
    );
    assert_eq!(writer.write_series(&[]), Ok(&[][..]));
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].0.series, [(0, 1, 8), (2, 1, 8)]);
    assert_eq!(sent[1].0.series, []);
}

#[test]
fn a_write_of_no_values_sends_a_frame_of_no_series() {
    let (mut writer, served) = writer(WAITING);
    assert_eq!(writer.write(&[]), Ok(&[][..]));
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    assert_eq!(sent[0].0.series, []);
    assert_eq!(sent[0].1, Time::Node);
}

#[test]
fn a_write_of_series_sends_each_series_in_place_order_with_its_stamps() {
    let (mut writer, served) = writer(WAITING);
    let stamps = [1_i64, 2].map(i64::to_le_bytes).concat();
    let counts = [5_i64, 6].map(i64::to_le_bytes).concat();
    let series_of = [of(2, 2, &stamps), of(0, 2, &counts)];
    assert_eq!(writer.write_series(&series_of), Ok(&[(2, WAITING)][..]));
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    let (frame, time) = &sent[0];
    assert_eq!(*time, Time::Frame(Path::Live));
    assert_eq!(frame.series, [(0, 2, 16), (1, 2, 16)]);
    assert_eq!(series(frame), [(0, stamps), (1, counts)]);
}

#[test]
fn a_write_of_series_over_bytes_max_gives_too_large_and_copies_nothing() {
    let (mut writer, served) = writer(WAITING);
    assert_eq!(writer.bytes_max(), BYTES_MAX);
    let room = writer.frame.bytes.capacity();
    let (stamps, counts) = ([0; 64], [0; 65]);
    let large = block::Error::TooLarge {
        requested: 129,
        largest: BYTES_MAX,
    };
    assert_eq!(
        writer.write_series(&[of(2, 2, &stamps), of(0, 2, &counts)]),
        Err(Error::Stream(program::Error::Pool(large)))
    );
    assert_eq!(writer.frame.bytes.capacity(), room);
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(served.join().unwrap().len(), 0);
}

#[test]
fn a_write_gives_the_error_of_the_node() {
    let (opens, mut opened) = mpsc::unbounded_channel();
    let served = spawn(move || {
        let open = received(&mut opened);
        open.opened.send(Ok((places(), BYTES_MAX))).unwrap();
        let mut calls = open.calls;
        let Some(Call::Write { frame, .. }) = calls.blocking_recv() else {
            panic!("a call other than a write");
        };
        let failed = Error::Stream(program::Error::Unanswered);
        open.written.send((Err(failed.clone()), frame)).unwrap();
        let Some(Call::Close(finished)) = calls.blocking_recv() else {
            panic!("a call other than a close");
        };
        finished.send(Err(failed)).unwrap();
    });
    let config = Config::new(vec!["count".parse().unwrap()]);
    let mut writer = Writer::open(&opens, &Arc::default(), config).unwrap();
    let failed = Err(Error::Stream(program::Error::Unanswered));
    assert_eq!(writer.write(&[(0, Value::I64(1))]), failed);
    assert_eq!(writer.close(), failed.map(drop));
    served.join().unwrap();
}

#[test]
fn a_close_gives_unstored_at_the_place_of_the_index_of_each_group() {
    let (opens, mut opened) = mpsc::unbounded_channel();
    let served = spawn(move || {
        let mut open = received(&mut opened);
        open.opened.send(Ok((places(), BYTES_MAX))).unwrap();
        let Some(Call::Close(finished)) = open.calls.blocking_recv() else {
            panic!("a call other than a close");
        };
        let groups = vec![(0, End::Removed), (3, End::Failed)];
        let unstored = Error::Stream(program::Error::Unstored(groups));
        finished.send(Err(unstored)).unwrap();
    });
    let channels = ["heat", "count"].map(|name| name.parse().unwrap()).to_vec();
    let writer = Writer::open(&opens, &Arc::default(), Config::new(channels)).unwrap();
    let groups = vec![(2, End::Failed), (3, End::Removed)];
    let unstored = Err(Error::Stream(program::Error::Unstored(groups)));
    assert_eq!(writer.close(), unstored);
    served.join().unwrap();
}

#[test]
fn each_call_whose_task_ended_gives_closed_after_a_close_else_panicked() {
    for (closed, error) in LOST {
        let (opens, mut opened) = mpsc::unbounded_channel();
        let served = spawn(move || {
            let open = received(&mut opened);
            open.opened.send(Ok((places(), BYTES_MAX))).unwrap();
            let mut calls = open.calls;
            drop(calls.blocking_recv());
            drop(open.written);
            calls
        });
        let config = Config::new(vec!["count".parse().unwrap()]);
        let closed = Arc::new(AtomicBool::new(closed));
        let mut writer = Writer::open(&opens, &closed, config).unwrap();
        let lost = Err(error.clone());
        assert_eq!(writer.write(&[(0, Value::I64(1))]), lost);
        drop(served.join().unwrap());
        assert_eq!(writer.write(&[(0, Value::I64(1))]), lost);
        assert_eq!(writer.write_series(&[]), lost);
        assert_eq!(writer.close(), Err(error));
    }
}

/// The task drops `written` before `calls`, as at the close of the client, with the
/// call still in `calls`.
#[test]
fn a_call_after_an_answer_that_did_not_come_gives_its_error_again() {
    for (closed, error) in LOST {
        let (opens, mut opened) = mpsc::unbounded_channel();
        let (again, called_again) = std_mpsc::channel::<()>();
        let served = spawn(move || {
            let open = received(&mut opened);
            open.opened.send(Ok((places(), BYTES_MAX))).unwrap();
            while open.calls.is_empty() {
                thread::yield_now();
            }
            drop(open.written);
            #[expect(
                clippy::disallowed_methods,
                reason = "the thread waits for the test"
            )]
            called_again.recv().unwrap();
            drop(open.calls);
        });
        let config = Config::new(vec!["count".parse().unwrap()]);
        let closed = Arc::new(AtomicBool::new(closed));
        let mut writer = Writer::open(&opens, &closed, config).unwrap();
        let lost = Err(error.clone());
        assert_eq!(writer.write(&[(0, Value::I64(1))]), lost);
        assert_eq!(writer.write(&[(0, Value::I64(1))]), lost);
        assert_eq!(writer.write_series(&[]), lost);
        assert_eq!(writer.close(), Err(error));
        again.send(()).unwrap();
        served.join().unwrap();
    }
}

#[test]
fn a_close_whose_task_ended_before_its_answer_gives_closed_after_a_close() {
    for (closed, error) in LOST {
        let (opens, mut opened) = mpsc::unbounded_channel();
        let served = spawn(move || {
            let open = received(&mut opened);
            open.opened.send(Ok((places(), BYTES_MAX))).unwrap();
            let mut calls = open.calls;
            drop(calls.blocking_recv());
        });
        let config = Config::new(vec!["count".parse().unwrap()]);
        let closed = Arc::new(AtomicBool::new(closed));
        let writer = Writer::open(&opens, &closed, config).unwrap();
        assert_eq!(writer.close(), Err(error));
        served.join().unwrap();
    }
}

/// A client whose thread answers the open of a writer with `places()`, then runs
/// `after` with the calls of the writer.
fn client(after: impl FnOnce(mpsc::Receiver<Call>) + Send + 'static) -> crate::Client {
    let started = crate::Client::start(|_, outcome, ended, mut opens| async move {
        outcome.send(Ok(())).unwrap();
        let crate::Open::Write(open) = opens.recv().await.unwrap() else {
            panic!("the client opens a writer");
        };
        open.opened.send(Ok((places(), BYTES_MAX))).unwrap();
        after(open.calls);
        drop(ended.await);
    });
    started.expect("a client")
}

#[test]
fn each_call_of_a_writer_after_a_close_of_its_client_gives_closed() {
    let client = client(drop);
    let config = Config::new(vec!["count".parse().unwrap()]);
    let mut writer = client.open_writer(config).unwrap();
    assert_eq!(client.close(), Ok(()));
    assert_eq!(writer.write(&[(0, Value::I64(1))]), Err(Error::Closed));
    assert_eq!(writer.close(), Err(Error::Closed));
}

#[test]
fn each_call_of_a_writer_after_a_drop_of_its_client_gives_closed() {
    let client = client(drop);
    let config = Config::new(vec!["count".parse().unwrap()]);
    let mut writer = client.open_writer(config).unwrap();
    drop(client);
    assert_eq!(writer.write(&[(0, Value::I64(1))]), Err(Error::Closed));
}

#[test]
fn each_call_of_a_writer_whose_client_thread_panicked_gives_panicked() {
    let client = client(|_| panic!("the session failed"));
    let config = Config::new(vec!["count".parse().unwrap()]);
    let mut writer = client.open_writer(config).unwrap();
    assert_eq!(writer.write(&[(0, Value::I64(1))]), Err(Error::Panicked));
    assert_eq!(writer.write_series(&[]), Err(Error::Panicked));
    assert_eq!(writer.close(), Err(Error::Panicked));
    assert_eq!(client.close(), Err(Error::Panicked));
}
