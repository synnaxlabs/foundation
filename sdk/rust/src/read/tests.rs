// These tests serve the calls of a reader on a thread of their own, in place of the
// task of the client's thread, whose `program::Reader` needs a node. The tests of
// `crates/node/tests/it/sdk/read.rs` run the task against a node process.

use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};

use codec::Encoder;
use types::channel;
use types::frame::key_set::{Group, Interner};
use types::frame::{Draft, Form, Path};
use types::sample::{Scalar, Type};

use super::*;

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

/// The next open on `opened`, which is the open of a reader.
fn received(opened: &mut mpsc::UnboundedReceiver<crate::Open>) -> Open {
    let Some(crate::Open::Read(open)) = opened.blocking_recv() else {
        panic!("the open of a reader");
    };
    open
}

/// The place of the channel of key `at`, of `scalar` samples, named `name`.
fn place(at: u128, scalar: Scalar, name: &str) -> Place {
    Place {
        key: channel::Key::from_u128(at),
        data_type: Type::Scalar(scalar),
        name: name.parse().unwrap(),
    }
}

/// `count` (`i64`), then `time`, its index.
fn places() -> Vec<Place> {
    vec![
        place(2, Scalar::I64, "count"),
        place(1, Scalar::Stamp, "time"),
    ]
}

fn config() -> Config {
    Config::new(vec!["count".parse().unwrap()])
}

/// A frame of `count` samples whose index is place 1, with a series of each of
/// `series`: its place, its bytes, and its lost seqs.
fn frame(count: u32, series: &[(usize, &[u8], Range<u64>)]) -> Frame {
    let mut frame = Frame::new();
    frame.count = count;
    frame.index = 1;
    for (place, bytes, lost) in series {
        if frame.series.len() <= *place {
            frame.series.resize_with(place + 1, Series::default);
        }
        frame.series[*place] = Series {
            held: true,
            bytes: bytes.to_vec(),
            lost: lost.clone(),
        };
    }
    frame.firsts = (0..frame.series.len()).collect();
    frame
}

/// A reader of `places()` whose calls a thread serves with `answer`, which takes the
/// count of each call and the frame of the call, and gives the outcome. The thread
/// gives the count of the calls that it served.
fn reader(
    mut answer: impl FnMut(usize, &mut Frame) -> Result<(), Error> + Send + 'static,
) -> (Reader, JoinHandle<usize>) {
    let (opens, mut opened) = mpsc::unbounded_channel();
    let served = spawn(move || {
        let Open {
            opened: answer_open,
            mut calls,
            answer: given,
            ..
        } = received(&mut opened);
        answer_open.send(Ok(places())).unwrap();
        let mut served = 0;
        while let Some(mut frame) = calls.blocking_recv() {
            let outcome = answer(served, &mut frame);
            served += 1;
            given.send((outcome, frame)).unwrap();
        }
        served
    });
    (
        Reader::open(&opens, &Arc::default(), config()).unwrap(),
        served,
    )
}

#[test]
fn a_config_of_new_is_complete() {
    let channels = vec!["count".parse().unwrap()];
    let config = Config::new(channels.clone());
    assert_eq!(
        config,
        Config {
            channels,
            mode: Mode::Complete
        }
    );
}

#[test]
fn a_reader_moves_between_threads() {
    fn sent<T: Send>() {}
    sent::<Reader>();
}

#[test]
fn an_open_sends_its_config_and_gives_the_places_of_the_node() {
    let (opens, mut opened) = mpsc::unbounded_channel();
    let served = spawn(move || {
        let open = received(&mut opened);
        open.opened.send(Ok(places())).unwrap();
        open.config
    });
    let config = Config {
        channels: vec!["count".parse().unwrap(), "level".parse().unwrap()],
        mode: Mode::Latest,
    };
    let reader = Reader::open(&opens, &Arc::default(), config.clone()).unwrap();
    assert_eq!(reader.places(), places());
    assert_eq!(served.join().unwrap(), config);
}

#[test]
fn an_open_gives_the_error_of_the_thread() {
    let (opens, mut opened) = mpsc::unbounded_channel();
    let served = spawn(move || {
        let open = received(&mut opened);
        let refused = Error::Stream(program::Error::Unanswered);
        open.opened.send(Err(refused)).unwrap();
    });
    let opened = Reader::open(&opens, &Arc::default(), config()).map(drop);
    assert_eq!(opened, Err(Error::Stream(program::Error::Unanswered)));
    served.join().unwrap();
}

#[test]
fn an_open_of_no_channel_gives_unnamed_and_sends_nothing() {
    let (opens, mut opened) = mpsc::unbounded_channel();
    let none = Config::new(Vec::new());
    let opened_none = Reader::open(&opens, &Arc::default(), none).map(drop);
    assert_eq!(opened_none, Err(Error::Stream(program::Error::Unnamed)));
    assert_eq!(
        Error::Stream(program::Error::Unnamed).to_string(),
        "the stream names no channel"
    );
    opened.try_recv().unwrap_err();
}

/// The flag of a client that closed or dropped, and the error of a call whose task
/// ended, then of a flag of a client that did not.
const LOST: [(bool, Error); 2] = [(true, Error::Closed), (false, Error::Panicked)];

#[test]
fn an_open_whose_thread_ended_gives_closed_after_a_close_else_panicked() {
    for (closed, error) in LOST {
        let closed = Arc::new(AtomicBool::new(closed));
        let (opens, opened) = mpsc::unbounded_channel();
        drop(opened);
        let open = Reader::open(&opens, &closed, config()).map(drop);
        assert_eq!(open, Err(error.clone()));
        let (opens, mut opened) = mpsc::unbounded_channel();
        let served = spawn(move || drop(opened.blocking_recv()));
        assert_eq!(
            Reader::open(&opens, &closed, config()).map(drop),
            Err(error)
        );
        served.join().unwrap();
    }
}

#[test]
fn a_call_gives_the_frame_of_the_thread_and_the_next_call_sends_it_back() {
    let (mut reader, served) = reader(|call, frame| {
        assert_eq!(frame.count, u32::try_from(call).unwrap());
        *frame = self::frame(frame.count + 1, &[(0, &[7; 8], 2..4)]);
        Ok(())
    });
    let first = reader.next().unwrap();
    assert_eq!((first.count(), first.index()), (1, 1));
    assert_eq!((first.series(0), first.lost(0)), (Some(&[7; 8][..]), 2..4));
    assert_eq!(reader.next().unwrap().count(), 2);
    drop(reader);
    assert_eq!(served.join().unwrap(), 2);
}

#[test]
fn an_error_ends_the_reader_and_each_later_call_gives_it_with_nothing_sent() {
    let failed = Error::Stream(program::Error::Unanswered);
    let error = failed.clone();
    let (mut reader, served) = reader(move |_, _| Err(error.clone()));
    assert_eq!(reader.next().map(drop), Err(failed.clone()));
    assert_eq!(reader.next().map(drop), Err(failed));
    drop(reader);
    assert_eq!(served.join().unwrap(), 1);
}

#[test]
fn each_call_of_a_reader_whose_task_ended_gives_closed_after_a_close_else_panicked() {
    for (closed, error) in LOST {
        let (opens, mut opened) = mpsc::unbounded_channel();
        let served = spawn(move || {
            let Open {
                opened: answer,
                mut calls,
                ..
            } = received(&mut opened);
            answer.send(Ok(places())).unwrap();
            drop(calls.blocking_recv());
        });
        let closed = Arc::new(AtomicBool::new(closed));
        let mut reader = Reader::open(&opens, &closed, config()).unwrap();
        assert_eq!(reader.next().map(drop), Err(error.clone()));
        served.join().unwrap();
        assert_eq!(reader.next().map(drop), Err(error));
    }
}

/// The task drops `answer` before `calls`, as at the close of the client, with the call
/// still in `calls`.
#[test]
fn a_call_after_an_answer_that_did_not_come_gives_its_error_again() {
    for (closed, error) in LOST {
        let (opens, mut opened) = mpsc::unbounded_channel();
        let (again, called_again) = std_mpsc::channel::<()>();
        let served = spawn(move || {
            let Open {
                opened: open_answer,
                calls,
                answer,
                ..
            } = received(&mut opened);
            open_answer.send(Ok(places())).unwrap();
            while calls.is_empty() {
                thread::yield_now();
            }
            drop(answer);
            #[expect(
                clippy::disallowed_methods,
                reason = "the thread waits for the test"
            )]
            called_again.recv().unwrap();
            drop(calls);
        });
        let closed = Arc::new(AtomicBool::new(closed));
        let mut reader = Reader::open(&opens, &closed, config()).unwrap();
        assert_eq!(reader.next().map(drop), Err(error.clone()));
        assert_eq!(reader.next().map(drop), Err(error));
        again.send(()).unwrap();
        served.join().unwrap();
    }
}

#[test]
fn a_frame_gives_only_its_series() {
    let stamps = [5_i64, 6].map(i64::to_le_bytes).concat();
    let mut frame = frame(2, &[(1, &stamps, 0..0), (0, &[0; 16], 3..9)]);
    assert_eq!(frame.series(1), Some(stamps.as_slice()));
    assert_eq!(
        (frame.lost(0), frame.lost(1), frame.lost(2)),
        (3..9, 0..0, 0..0)
    );
    frame.series[0].held = false;
    assert_eq!((frame.series(0), frame.lost(0)), (None, 0..0));
}

/// A frame of a reader of `time`, entry 0, and `count`, entry 1, as the node gives
/// it: each of `series` is an entry and its raw samples, which it encodes, or its
/// bytes as they are when `raw` is false.
fn read(samples: u32, series: &[(usize, &[u8], bool)]) -> types::frame::Frame {
    let key = channel::Key::from_u128;
    let group = Group {
        index: key(1),
        data: &[(key(2), Type::Scalar(Scalar::I64))],
    };
    let set = Interner::new().intern(&[group]);
    let types = [Scalar::Stamp, Scalar::I64].map(Type::Scalar);
    let bytes: Vec<(usize, Vec<u8>)> = series
        .iter()
        .map(|&(entry, values, raw)| {
            if !raw {
                return (entry, values.to_vec());
            }
            let mut out = vec![0; codec::max_len(types[entry], values.len())];
            let count = usize::try_from(samples).unwrap();
            let len = Encoder::new(types[entry]).encode(count, values, &mut out);
            out.truncate(len.unwrap());
            (entry, out)
        })
        .collect();
    let lens: Vec<(usize, usize)> =
        bytes.iter().map(|(at, b)| (*at, b.len())).collect();
    let config = block::Config::new(1 << 16).unwrap();
    let memory = block::Heap::new(config.reservation());
    let pool = block::Pool::new(config, memory);
    let mut draft = Draft::new(&pool, &set, Form::Encoded, &lens).unwrap();
    for (entry, series) in &bytes {
        draft.series_mut(*entry).unwrap().copy_from_slice(series);
    }
    draft.set_count(0, samples);
    draft.freeze(Path::Live)
}

/// `time` and `count`, the entries of a frame of `read`.
fn read_places() -> [Place; 2] {
    [
        place(1, Scalar::Stamp, "time"),
        place(2, Scalar::I64, "count"),
    ]
}

/// The targets of `entries`, the first place of each entry, in a reader whose places
/// have the series of `firsts`, and whose index is the place of entry 0.
fn targets(entries: &[Option<usize>], firsts: &[usize]) -> Targets {
    Targets {
        index: entries[0].unwrap(),
        entries: entries.to_vec(),
        firsts: firsts.to_vec(),
    }
}

#[test]
fn a_decode_gives_each_series_raw_at_each_of_its_places_with_its_lost_seqs() {
    let stamps = [5_i64, 6].map(i64::to_le_bytes).concat();
    let counts = [-1_i64, 9].map(i64::to_le_bytes).concat();
    let lost = |entry| if entry == 1 { 4..6 } else { 0..0 };
    let targets = targets(&[Some(3), Some(4)], &[0, 1, 2, 3, 4, 4]);
    let mut frame = frame(9, &[(0, &[1; 3], 1..2), (1, &[], 0..0), (2, &[], 0..0)]);
    let both = read(2, &[(0, &stamps, true), (1, &counts, true)]);
    let places = read_places();
    assert_eq!(decoded(&places, &targets, lost, &both, &mut frame), Ok(()));
    assert_eq!((frame.count(), frame.index()), (2, 3));
    assert_eq!((frame.series(0), frame.lost(0)), (None, 0..0));
    assert_eq!((frame.series(3), frame.lost(3)), (Some(&stamps[..]), 0..0));
    for place in [4, 5] {
        assert_eq!(
            (frame.series(place), frame.lost(place)),
            (Some(&counts[..]), 4..6)
        );
    }
    let index = read(1, &[(0, &stamps[..8], true)]);
    assert_eq!(decoded(&places, &targets, lost, &index, &mut frame), Ok(()));
    let held = (frame.count(), frame.series(4), frame.series(5));
    assert_eq!(held, (1, None, None));
}

#[test]
fn a_series_that_does_not_decode_gives_decode_with_its_place_unless_it_has_none() {
    let stamps = 5_i64.to_le_bytes();
    let bad = [0xff; 8];
    let mut out = Vec::new();
    let i64 = Type::Scalar(Scalar::I64);
    let cause = codec::decode(i64, 1, &bad, &mut out).unwrap_err();
    let broken = read(1, &[(0, &stamps, true), (1, &bad, false)]);
    let places = read_places();
    let mut frame = Frame::new();
    let decode = decoded(
        &places,
        &targets(&[Some(2), Some(3)], &[0, 1, 2, 3]),
        |_| 0..0,
        &broken,
        &mut frame,
    );
    assert_eq!(decode, Err(Error::Decode { place: 3, cause }));
    let unplaced = targets(&[Some(2), None], &[0, 1, 2, 3]);
    let skipped = decoded(&places, &unplaced, |_| 0..0, &broken, &mut frame);
    assert_eq!((skipped, frame.series(3)), (Ok(()), None));
}

/// The names of `names`.
fn names(names: &[&str]) -> Vec<Name> {
    names.iter().map(|name| name.parse().unwrap()).collect()
}

/// The names, and the layout of a reader of `plant.a`, `plant.b`, and `plant.time`.
type Laid<'a> = (&'a [&'a str], Option<Layout>);

#[test]
fn a_layout_puts_each_name_at_its_place_then_the_index_unless_a_name_names_it() {
    let a = place(2, Scalar::I64, "plant.a");
    let b = place(3, Scalar::I64, "plant.b");
    let plant = place(1, Scalar::Stamp, "plant.time");
    let entries = [a.clone(), b.clone(), plant.clone()];
    let targets = |index, entries: &[Option<usize>], firsts: &[usize]| Targets {
        index,
        entries: entries.to_vec(),
        firsts: firsts.to_vec(),
    };
    let cases: [Laid<'_>; 4] = [
        (
            &["plant.b", "plant.a"],
            Some(Layout {
                places: vec![b.clone(), a.clone(), plant.clone()],
                targets: targets(2, &[Some(1), Some(0), Some(2)], &[0, 1, 2]),
            }),
        ),
        (
            &["plant.a", "plant.a"],
            Some(Layout {
                places: vec![a.clone(), a.clone(), plant.clone()],
                targets: targets(2, &[Some(0), None, Some(2)], &[0, 0, 2]),
            }),
        ),
        (
            &["plant.time", "plant.a"],
            Some(Layout {
                places: vec![plant.clone(), a.clone()],
                targets: targets(0, &[Some(1), None, Some(0)], &[0, 1]),
            }),
        ),
        (&["plant.a", "cell.c"], None),
    ];
    for (named, layout) in cases {
        assert_eq!(
            super::layout(&names(named), &entries, 2),
            layout,
            "{named:?}"
        );
    }
}
