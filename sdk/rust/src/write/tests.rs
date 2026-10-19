// These tests serve the calls of a writer on a thread of their own, in place of the
// task of the client's thread, whose `program::Writer` needs a node. The tests of
// `crates/node/tests/it/sdk.rs` run the task against a node process.

use std::thread::{self, JoinHandle};

use types::channel;
use types::time::Stamp;

use super::*;

const WAITING: Outcome = Outcome::Refused(Refusal::Waiting);

/// The [`Writer::bytes_max`] of each writer of these tests.
const BYTES_MAX: usize = 32;

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

fn place(at: u128, scalar: Scalar, name: &str) -> Place {
    Place {
        key: channel::Key::from_u128(at),
        data_type: Type::Scalar(scalar),
        name: name.parse().unwrap(),
    }
}

/// `time`, the index at place 0, then `count` (`i64`) and `level` (`f64`).
fn places() -> Vec<Place> {
    vec![
        place(1, Scalar::Stamp, "time"),
        place(2, Scalar::I64, "count"),
        place(3, Scalar::F64, "level"),
    ]
}

/// The frame and time of each write that a writer sent, until its first close.
type Sent = Vec<(Frame, Time)>;

/// A writer of `count` and `level`, so places 0, 1, and 2 are `count`, `level`, and
/// `time`, whose calls a thread serves: it answers each write with `outcome` and the
/// close with `Ok`, and gives each write that it served.
fn writer(outcome: Outcome) -> (Writer, JoinHandle<Sent>) {
    writer_of(&["count", "level"], outcome)
}

/// A writer of `channels`, served as [`writer`] serves it.
fn writer_of(channels: &[&str], outcome: Outcome) -> (Writer, JoinHandle<Sent>) {
    let (opens, mut opened) = mpsc::unbounded_channel::<Open>();
    let served = spawn(move || {
        let Open {
            opened: answer,
            mut calls,
            written,
            ..
        } = opened.blocking_recv().unwrap();
        answer.send(Ok((places(), 0, BYTES_MAX))).unwrap();
        let mut sent = Vec::new();
        while let Some(call) = calls.blocking_recv() {
            match call {
                Call::Write { frame, time } => {
                    let kept = Frame {
                        count: frame.count,
                        parts: frame.parts.clone(),
                        series: frame.series.clone(),
                        bytes: frame.bytes.clone(),
                    };
                    sent.push((kept, time));
                    written.send((Ok(outcome), frame)).unwrap();
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

/// A writer of `channels` whose open gives `places()` with the index at node place 0.
fn opened(channels: &[&str]) -> Writer {
    let (opens, mut opened) = mpsc::unbounded_channel::<Open>();
    let served = spawn(move || {
        let open = opened.blocking_recv().unwrap();
        open.opened.send(Ok((places(), 0, BYTES_MAX))).unwrap();
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
    let (opens, mut opened) = mpsc::unbounded_channel::<Open>();
    let lease = Span::from_nanos(5);
    let served = spawn(move || {
        let open = opened.blocking_recv().unwrap();
        open.opened.send(Ok((places(), 0, BYTES_MAX))).unwrap();
        open.config
    });
    let config = Config {
        channels: vec!["level".parse().unwrap(), "count".parse().unwrap()],
        authority: Authority(7),
        lease: Some(lease),
    };
    let writer = Writer::open(&opens, &Arc::default(), config.clone()).unwrap();
    let node = places();
    let ordered = [node[2].clone(), node[1].clone(), node[0].clone()];
    assert_eq!(writer.places(), ordered);
    assert_eq!(writer.index(), 2);
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
fn a_writer_that_names_the_index_gives_it_at_its_place() {
    let writer = opened(&["count", "time"]);
    assert_eq!(writer.places(), [places()[1].clone(), places()[0].clone()]);
    assert_eq!(writer.index(), 1);
}

#[test]
fn a_writer_that_names_a_channel_twice_gives_it_at_each_place() {
    let writer = opened(&["time", "count", "time", "count"]);
    let [time, count, _] = places().try_into().unwrap();
    assert_eq!(writer.places(), [time.clone(), count.clone(), time, count]);
    assert_eq!(writer.index(), 0);
}

#[test]
fn an_open_gives_the_error_of_the_node() {
    let (opens, mut opened) = mpsc::unbounded_channel::<Open>();
    let served = spawn(move || {
        let open = opened.blocking_recv().unwrap();
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
        let (opens, opened) = mpsc::unbounded_channel::<Open>();
        drop(opened);
        let open = Writer::open(&opens, &closed, config.clone()).map(drop);
        assert_eq!(open, Err(error.clone()));
        let (opens, mut opened) = mpsc::unbounded_channel::<Open>();
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
    assert_eq!(writer.write(&values), Ok(WAITING));
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    let [(frame, time)] = sent.as_slice() else {
        panic!("{} writes", sent.len());
    };
    assert_eq!(*time, Time::Node);
    assert_eq!(frame.count, 1);
    assert_eq!(frame.series, [(0, 8), (1, 8), (2, 8)]);
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
    assert_eq!(writer.write(&values), Ok(WAITING));
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    assert_eq!(sent[0].1, Time::Frame(Path::Live));
    assert_eq!(sent[0].0.series, [(0, 8), (1, 8)]);
    assert_eq!(
        series(&sent[0].0),
        [
            (0, 9_i64.to_le_bytes().to_vec()),
            (1, 3_i64.to_le_bytes().to_vec())
        ]
    );
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
    assert_eq!(writer.write(&[(0, Value::I64(4))]), Ok(WAITING));
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
    assert_eq!(writer.write_series(1, &[(0, &[0; 8]), (3, &[1])]), past);
    assert_eq!(writer.write(&[(0, Value::I64(2))]), Ok(WAITING));
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(series(&sent[0].0)[1], (1, 2_i64.to_le_bytes().to_vec()));
}

fn layout(error: frame::Error) -> Result<Outcome, Error> {
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
    let series_of = [(0, count.as_slice()), (2, &stamp), (0, &count)];
    assert_eq!(writer.write_series(1, &series_of), unordered(0, 0));
    assert_eq!(writer.write(&[(0, Value::I64(4))]), Ok(WAITING));
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(served.join().unwrap().len(), 1);
    let (mut writer, served) = writer_of(&["count", "count"], WAITING);
    let both = [(1, Value::I64(1)), (0, Value::I64(2))];
    assert_eq!(writer.write(&both), unordered(1, 0));
    assert_eq!(writer.write(&[(1, Value::I64(3))]), Ok(WAITING));
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(series(&sent[0].0)[1], (1, 3_i64.to_le_bytes().to_vec()));
}

#[test]
fn a_write_of_series_with_none_of_the_index_gives_index_absent_and_sends_nothing() {
    let (mut writer, served) = writer(WAITING);
    let (level, count) = (1.5_f64.to_le_bytes(), 2_i64.to_le_bytes());
    let absent = layout(frame::Error::IndexAbsent { entry: 0, index: 2 });
    let series_of = [(1, level.as_slice()), (0, &count)];
    assert_eq!(writer.write_series(1, &series_of), absent);
    let absent = layout(frame::Error::IndexAbsent { entry: 1, index: 2 });
    assert_eq!(writer.write_series(1, &[(1, &level)]), absent);
    let stamp = 1_i64.to_le_bytes();
    assert_eq!(
        writer.write_series(1, &[(1, &level), (2, &stamp)]),
        Ok(WAITING)
    );
    assert_eq!(writer.write_series(1, &[]), Ok(WAITING));
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].0.series, [(0, 8), (2, 8)]);
    assert_eq!(sent[1].0.series, []);
}

#[test]
fn a_write_of_no_values_sends_a_frame_of_no_series() {
    let (mut writer, served) = writer(WAITING);
    assert_eq!(writer.write(&[]), Ok(WAITING));
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
    let series_of = [(2, stamps.as_slice()), (0, counts.as_slice())];
    assert_eq!(writer.write_series(2, &series_of), Ok(WAITING));
    assert_eq!(writer.close(), Ok(()));
    let sent = served.join().unwrap();
    let (frame, time) = &sent[0];
    assert_eq!(*time, Time::Frame(Path::Live));
    assert_eq!(frame.count, 2);
    assert_eq!(frame.series, [(0, 16), (1, 16)]);
    assert_eq!(series(frame), [(0, stamps), (1, counts)]);
}

#[test]
fn a_write_of_series_over_bytes_max_gives_too_large_and_copies_nothing() {
    let (mut writer, served) = writer(WAITING);
    assert_eq!(writer.bytes_max(), BYTES_MAX);
    let room = writer.frame.bytes.capacity();
    let (stamps, counts) = ([0; 16], [0; 17]);
    let large = block::Error::TooLarge {
        requested: 33,
        largest: BYTES_MAX,
    };
    assert_eq!(
        writer.write_series(2, &[(2, &stamps), (0, &counts)]),
        Err(Error::Stream(program::Error::Pool(large)))
    );
    assert_eq!(writer.frame.bytes.capacity(), room);
    assert_eq!(writer.close(), Ok(()));
    assert_eq!(served.join().unwrap().len(), 0);
}

#[test]
fn a_write_gives_the_error_of_the_node() {
    let (opens, mut opened) = mpsc::unbounded_channel::<Open>();
    let served = spawn(move || {
        let open = opened.blocking_recv().unwrap();
        open.opened.send(Ok((places(), 0, BYTES_MAX))).unwrap();
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
fn each_call_whose_task_ended_gives_closed_after_a_close_else_panicked() {
    for (closed, error) in LOST {
        let (opens, mut opened) = mpsc::unbounded_channel::<Open>();
        let served = spawn(move || {
            let open = opened.blocking_recv().unwrap();
            open.opened.send(Ok((places(), 0, BYTES_MAX))).unwrap();
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
        assert_eq!(writer.write_series(1, &[]), lost);
        assert_eq!(writer.close(), Err(error));
    }
}

#[test]
fn a_close_whose_task_ended_before_its_answer_gives_closed_after_a_close() {
    for (closed, error) in LOST {
        let (opens, mut opened) = mpsc::unbounded_channel::<Open>();
        let served = spawn(move || {
            let open = opened.blocking_recv().unwrap();
            open.opened.send(Ok((places(), 0, BYTES_MAX))).unwrap();
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
        let open = opens.recv().await.unwrap();
        open.opened.send(Ok((places(), 0, BYTES_MAX))).unwrap();
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
    assert_eq!(writer.write_series(1, &[]), Err(Error::Panicked));
    assert_eq!(writer.close(), Err(Error::Panicked));
    assert_eq!(client.close(), Err(Error::Panicked));
}
