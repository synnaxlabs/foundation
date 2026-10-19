//! Each behavior of a [`Reader`]: the open, the frames, and each error.

use std::collections::BTreeMap;
use std::future::poll_fn;
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;

use env::clock::Clock;
use env::tasks::Tasks;
use hub::home::Outcome;
use hub::writer::{self, Writer};
use hub::{Hub, serve};
use program::{Error, Reader, Session};
use spec::channel::{Channel, Data, Kind};
use spec::data_type::DataType;
use spec::definition::Definition;
use transport::Code;
use transport::stream::Incoming;
use types::authority::Authority;
use types::channel;
use types::frame::{Form, Frame, Label, Path, Range};
use types::name::Selector;
use types::sample::{Scalar, Type};
use types::time::Span;
use wire::header::MALFORMED;
use wire::hub::Reply;
use wire::hub::client::{Mode, OPEN_BYTES_MAX, Open, Place, Refusal};

use crate::agent::{AGENT, SUBJECT, name};
use crate::connect::{ample, connect, connect_with};
use crate::hand::{accept_read, by_hand, opened, reply, send, send_frame, start};
use crate::node::env;
use crate::session::{admit, raw, within};
use crate::sessions::{self, Got, Home, QUIET, closed_with, rules};

const STAMP: Type = Type::Scalar(Scalar::Stamp);
const I64: Type = Type::Scalar(Scalar::I64);

/// How long after the home accepts the session its first write comes, so the program
/// has opened its read by then.
const START: Span = Span::from_nanos(100_000_000);

/// The time between two writes: two commits of the home.
const APART: Span = Span::from_nanos(20_000_000);

fn key(key: u128) -> channel::Key {
    channel::Key::from_u128(key)
}

fn select(channels: &[&str]) -> Selector {
    Selector::new(channels.iter().copied()).expect("a selector")
}

fn place(at: u128, data_type: Type, channel: &str) -> Place {
    Place {
        key: key(at),
        data_type,
        name: name(channel),
    }
}

/// The key of each entry of the key set of `reader`.
fn keys(reader: &Reader) -> Vec<channel::Key> {
    reader
        .set()
        .entries()
        .iter()
        .map(|entry| entry.key)
        .collect()
}

/// Each series of `frame`, a frame of `reader`, as its entry and its samples decoded
/// as the type of its place.
fn samples(reader: &Reader, frame: &Frame) -> Vec<(usize, Vec<i64>)> {
    let count = frame.range(0).expect("the index group").count;
    let count = usize::try_from(count).expect("fits");
    frame
        .iter()
        .map(|(entry, bytes)| {
            let mut out = Vec::new();
            let data_type = reader.places()[entry].data_type;
            codec::decode(data_type, count, bytes, &mut out).expect("decodes");
            let (chunks, _) = out.as_chunks::<8>();
            let samples = chunks.iter().map(|chunk| i64::from_le_bytes(*chunk));
            (entry, samples.collect())
        })
        .collect()
}

/// `time` with key 1, and `value` with key 2, an `i64` channel on `time`.
fn definitions() -> BTreeMap<types::name::Name, Definition> {
    let time = Channel {
        key: key(1),
        kind: Kind::Index {
            error: None,
            control: None,
        },
    };
    let data = Data::new(time.key, None, DataType::Sample(I64), None).expect("no unit");
    let value = Channel {
        key: key(2),
        kind: Kind::Data(data),
    };
    BTreeMap::from([
        (name("time"), Definition::Channel(time)),
        (name("value"), Definition::Channel(value)),
    ])
}

/// What the node gives for a read that the program dropped.
fn dropped() -> Result<Got, serve::Error> {
    Err(serve::Error::Stream(transport::Error::Reset {
        code: Code(0),
    }))
}

/// What the node gives for the hello of a session that the program closed.
fn closed() -> Result<Got, serve::Error> {
    Err(serve::Error::Stream(closed_with(0)))
}

/// Runs `program` with a session of [`SUBJECT`] on a hub with `time` and `value`,
/// where a writer writes each of `values` to `value` in a frame of its own, [`APART`]
/// apart from [`START`] on, at stamps that count up by 1 from the open of the hub.
/// The program gets the clock of its node. Gives what the home saw.
fn with_writes<P>(
    seed: u64,
    values: &'static [i64],
    program: impl FnOnce(Session, Clock) -> P + Send + 'static,
) -> Home
where
    P: Future<Output = ()> + 'static,
{
    let open = async |node: &sim::node::Node, tasks: &Tasks| {
        let (hub, now) = hub::testing::open(env(node, tasks.clone())).await;
        hub.set_definitions(&definitions());
        let now = now.nanos();
        (hub.clone(), (hub, now, node.clock(), tasks.clone()))
    };
    let ready = async move |(hub, now, clock, tasks): &(Hub, i64, Clock, Tasks)| {
        let config = writer::Config {
            subject: name("a"),
            authority: Authority(1),
            lease: None,
            channels: vec![name("value")],
        };
        let mut writer = hub.writer(config).await.expect("opens");
        let (clock, now) = (clock.clone(), *now);
        tasks.spawn(async move {
            clock.sleep(START).await;
            for (stamp, &value) in (now..).zip(values) {
                let draft = frame(&writer, stamp, value);
                let outcomes = writer
                    .write(Label::Path(Path::Live), draft)
                    .expect("the home takes it");
                assert!(
                    matches!(outcomes, [Outcome::Applied { .. }]),
                    "{outcomes:?}"
                );
                clock.sleep(APART).await;
            }
        });
    };
    sessions::serve_session(
        seed,
        sim::link::Config::default(),
        Some(rules()),
        open,
        ready,
        |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            program(session, node.clock()).await;
            node.clock().sleep(QUIET).await;
        },
    )
}

/// A frame of one sample of `writer`: `stamp` on `time`, and `value` on `value`.
fn frame(writer: &Writer, stamp: i64, value: i64) -> types::frame::Draft {
    let entries = writer.set().entries();
    let entry = |at| {
        let at = key(at);
        entries
            .iter()
            .position(|entry| entry.key == at)
            .expect("held")
    };
    let (time, data) = (entry(1), entry(2));
    let mut series = [(time, 8), (data, 8)];
    series.sort_unstable();
    let mut draft = writer.draft(Form::Raw, &series).expect("a frame");
    for (entry, sample) in [(time, stamp), (data, value)] {
        let bytes = draft.series_mut(entry).expect("present");
        bytes.copy_from_slice(&sample.to_le_bytes());
    }
    draft.set_count(entries[time].group, 1);
    draft
}

/// The places of a read are the channels that its selector matches, in name order,
/// then its index, and the entries of each frame are its places.
#[test]
fn gives_each_frame_of_a_complete_read_in_order() {
    let home = with_writes(400, &[10, 20], |session, clock| async move {
        let mut reader = session
            .reader(select(&["value"]), Mode::Complete)
            .await
            .expect("opens");
        assert_eq!(
            reader.places(),
            [place(2, I64, "value"), place(1, STAMP, "time")]
        );
        assert_eq!(keys(&reader), [key(2), key(1)]);
        let mut stamps = Vec::new();
        for value in [10, 20] {
            let frame = reader.next().await.expect("a frame");
            assert_eq!(frame.path(), Path::Live);
            let got = samples(&reader, &frame);
            assert_eq!(got[0], (0, vec![value]));
            assert_eq!(got[1].0, 1);
            stamps.extend(got[1].1.iter().copied());
        }
        assert_eq!(stamps[1], stamps[0] + 1, "{stamps:?}");
        drop(reader);
        clock.sleep(QUIET).await;
    });
    assert_eq!(home.served, [dropped(), closed()]);
}

#[test]
fn gives_a_latest_read_its_index_once_when_its_selector_matches_it() {
    let home = with_writes(401, &[10], |session, clock| async move {
        let mut reader = session
            .reader(select(&["value", "time"]), Mode::Latest)
            .await
            .expect("opens");
        assert_eq!(
            reader.places(),
            [place(1, STAMP, "time"), place(2, I64, "value")]
        );
        assert_eq!(keys(&reader), [key(1), key(2)]);
        let frame = reader.next().await.expect("a frame");
        assert_eq!(samples(&reader, &frame)[1], (1, vec![10]));
        drop(reader);
        clock.sleep(QUIET).await;
    });
    assert_eq!(home.served, [dropped(), closed()]);
}

#[test]
fn refuses_a_read_whose_selector_matches_no_channel() {
    with_writes(402, &[], |session, _| async move {
        let opened = session.reader(select(&["nope"]), Mode::Complete).await;
        assert_eq!(opened.map(drop), Err(Error::Refused(Refusal::Unknown)));
    });
}

/// The node sees no stream for an open over the cap: it serves only the hello.
#[test]
fn refuses_an_open_over_the_cap_with_nothing_sent() {
    let names: Vec<String> = (0..300)
        .map(|n| format!("n{n:03}{}", "x".repeat(240)))
        .collect();
    let select = Selector::new(names.iter().map(String::as_str)).expect("a selector");
    let open = Open {
        mode: Mode::Complete,
        select: select.clone(),
    };
    let length = open.encoded_len();
    assert!(u64::try_from(length).expect("fits") > OPEN_BYTES_MAX);
    let home = with_writes(403, &[], move |session, _| async move {
        let opened = session.reader(select, Mode::Complete).await;
        assert_eq!(opened.map(drop), Err(Error::Open { length }));
    });
    assert_eq!(home.served, [closed()]);
}

/// An open of exactly the cap reaches the node, which finds no channel.
#[test]
fn sends_an_open_at_the_cap() {
    let names: Vec<String> = (0..255)
        .map(|n| format!("n{n:03}{}", "x".repeat(251)))
        .collect();
    let select = Selector::new(names.iter().map(String::as_str)).expect("a selector");
    let open = Open {
        mode: Mode::Complete,
        select: select.clone(),
    };
    assert_eq!(u64::try_from(open.encoded_len()), Ok(OPEN_BYTES_MAX));
    with_writes(405, &[], move |session, _| async move {
        let opened = session.reader(select, Mode::Complete).await;
        assert_eq!(opened.map(drop), Err(Error::Refused(Refusal::Unknown)));
    });
}

/// A read takes no turn, so a request runs while a reader is open.
#[test]
fn runs_a_request_beside_an_open_reader() {
    let home = with_writes(404, &[10], |session, clock| async move {
        let mut reader = session
            .reader(select(&["value"]), Mode::Complete)
            .await
            .expect("opens");
        assert_eq!(session.request(b"ab").await, Ok(b"ba".to_vec()));
        let frame = reader.next().await.expect("a frame");
        assert_eq!(samples(&reader, &frame)[0], (0, vec![10]));
        drop(reader);
        clock.sleep(QUIET).await;
    });
    assert_eq!(
        home.served[0],
        Ok(Got::Request(name(SUBJECT), b"ab".to_vec()))
    );
    assert_eq!(home.served[1..], [dropped(), closed()]);
}

/// The places of a channel on `time` on each side of `time`.
fn around() -> [Place; 3] {
    [
        place(10, I64, "a"),
        place(1, STAMP, "time"),
        place(11, I64, "b"),
    ]
}

/// A frame of each place, with its body cut across two messages, then a frame of the
/// index alone: each series lands at the entry of its place.
#[test]
fn gives_each_series_at_the_entry_of_its_place() {
    let body: Vec<u8> = (0..24).collect();
    let sent = body.clone();
    by_hand(
        410,
        move |session, node| async move {
            let mut read = opened(&session, &around(), 1).await;
            let range = Range { seq: 5, count: 1 };
            let run = [(0, 8), (1, 16), (2, 24)];
            send_frame(&mut read, range, &run, &sent, &[10, 14]).await;
            let range = Range { seq: 6, count: 1 };
            send_frame(&mut read, range, &[(1, 8)], &sent[..8], &[8]).await;
            node.clock().sleep(QUIET).await;
        },
        move |session, _| {
            Box::pin(async move {
                let mut reader = session
                    .reader(select(&["a", "b"]), Mode::Complete)
                    .await
                    .expect("opens");
                assert_eq!(reader.places(), around());
                assert_eq!(keys(&reader), [key(10), key(1), key(11)]);
                assert_eq!(reader.index(), 1);
                let frame = reader.next().await.expect("a frame");
                assert_eq!(frame.range(0), Some(Range { seq: 5, count: 1 }));
                assert_eq!(frame.form(), Form::Encoded);
                let series: Vec<_> = frame.iter().collect();
                assert_eq!(
                    series,
                    [(0, &body[..8]), (1, &body[8..16]), (2, &body[16..])]
                );
                let frame = reader.next().await.expect("a frame");
                assert_eq!(frame.range(0), Some(Range { seq: 6, count: 1 }));
                assert_eq!(frame.iter().collect::<Vec<_>>(), [(1, &body[..8])]);
            })
        },
    );
}

/// The index place that the node names is the index of the key set, also when it
/// comes first.
#[test]
fn takes_the_index_at_the_place_that_the_node_names() {
    let places = [
        place(1, STAMP, "time"),
        place(10, I64, "a"),
        place(11, I64, "b"),
    ];
    let sent = places.clone();
    let body: Vec<u8> = (0..8).collect();
    let stamps = body.clone();
    by_hand(
        411,
        move |session, node| async move {
            let mut read = opened(&session, &sent, 0).await;
            let range = Range { seq: 5, count: 1 };
            send_frame(&mut read, range, &[(0, 8)], &stamps, &[8]).await;
            node.clock().sleep(QUIET).await;
        },
        move |session, _| {
            Box::pin(async move {
                let mut reader = session
                    .reader(select(&["a", "b"]), Mode::Complete)
                    .await
                    .expect("opens");
                assert_eq!(reader.places(), places);
                assert_eq!((reader.index(), reader.set().groups()), (0, &[0][..]));
                let frame = reader.next().await.expect("a frame");
                assert_eq!(frame.iter().collect::<Vec<_>>(), [(0, &body[..])]);
            })
        },
    );
}

/// A call of `next` that drops while the draft of a frame waits for the pool leaves
/// the frame to the next call.
#[test]
fn gives_a_frame_whose_draft_waited_at_a_call_that_dropped() {
    const LEN: usize = 40_000;
    let body: Vec<u8> = (0..=250).cycle().take(LEN).collect();
    let sent = body.clone();
    raw(
        418,
        move |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let mut read = opened(&session, &around(), 1).await;
            let end = u32::try_from(LEN).expect("fits");
            for seq in [0, 1] {
                let range = Range { seq, count: 1 };
                send_frame(&mut read, range, &[(1, end)], &sent, &[1000; 40]).await;
            }
            node.clock().sleep(QUIET).await;
        },
        move |node, tasks, at| async move {
            let config = block::Config::new(
                u64::try_from(block::footprint(1 << 16)).expect("a usize fits a u64"),
            )
            .expect("the budget fits");
            let pool = block::Pool::new(
                config.clone(),
                block::Heap::new(config.reservation()),
            );
            let session = connect_with(&node, tasks, at, AGENT, ample(), Rc::new(pool))
                .await
                .expect("connects");
            let mut reader = session
                .reader(select(&["a", "b"]), Mode::Complete)
                .await
                .expect("opens");
            let first = reader.next().await.expect("a frame");
            node.clock().sleep(APART).await;
            {
                let mut next = pin!(reader.next());
                let waits =
                    poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx).is_pending()));
                assert!(waits.await, "the draft waits for the pool");
            }
            drop(first);
            let second = reader.next().await.expect("a frame");
            assert_eq!(second.range(0), Some(Range { seq: 1, count: 1 }));
            assert_eq!(second.iter().collect::<Vec<_>>(), [(1, &body[..])]);
            node.clock().sleep(QUIET).await;
        },
    );
}

/// A program that held frames until the pool of frames was full, then dropped all but
/// one, holds 1 frame of 64 kB of a 2 MiB pool. The next frame, of 200 kB, comes: the
/// free blocks of the small size give their pages back.
#[test]
fn gives_a_large_frame_while_the_program_holds_one_small_frame() {
    const SMALL: usize = 64_000;
    const LARGE: usize = 200_000;
    const BATCH: u64 = 40;
    let body: Vec<u8> = (0..=250).cycle().take(LARGE).collect();
    raw(
        901,
        move |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let mut read = opened(&session, &around(), 1).await;
            let end = u32::try_from(SMALL).expect("fits");
            for seq in 0..BATCH {
                let range = Range { seq, count: 1 };
                send_frame(&mut read, range, &[(1, end)], &body[..SMALL], &[16_000; 4])
                    .await;
            }
            let range = Range {
                seq: BATCH,
                count: 1,
            };
            let end = u32::try_from(LARGE).expect("fits");
            send_frame(&mut read, range, &[(1, end)], &body, &[50_000; 4]).await;
            node.clock()
                .sleep(Span::from_nanos(60 * Span::SECOND.nanos()))
                .await;
            drop(session);
        },
        |node, tasks, at| async move {
            let frames = block::Config::new(1 << 21).expect("the budget fits");
            let frames = Rc::new(block::Pool::heap(frames));
            let session = connect_with(&node, tasks, at, AGENT, ample(), frames)
                .await
                .expect("connects");
            let mut reader = session
                .reader(select(&["a", "b"]), Mode::Complete)
                .await
                .expect("opens");
            let clock = node.clock();
            let mut held = Vec::new();
            while let Some(got) = within(&clock, Span::SECOND, reader.next()).await {
                held.push(got.expect("a frame"));
            }
            let count = u64::try_from(held.len()).expect("fits");
            assert!(count < BATCH, "the pool of frames filled: {count}");
            let last = held.pop().expect("a frame");
            drop(held);
            for seq in count..BATCH {
                let frame = reader.next().await.expect("a frame");
                assert_eq!(frame.range(0), Some(Range { seq, count: 1 }));
            }
            let quiet = Span::from_nanos(10 * Span::SECOND.nanos());
            let large = within(&clock, quiet, reader.next()).await;
            let large = large.map(|got| got.map(|frame| frame.range(0)));
            let range = Range {
                seq: BATCH,
                count: 1,
            };
            assert_eq!(
                large,
                Some(Ok(Some(range))),
                "the program holds one frame of {SMALL} bytes"
            );
            drop(last);
        },
    );
}

/// Kind 3, which `Behind` once took, ends the reader with the error of an unknown kind,
/// stops the stream as `MALFORMED`, and each later call gives it again.
#[test]
fn refuses_the_retired_kind_of_behind_at_each_call() {
    by_hand(
        411,
        |session, node| async move {
            let mut read = opened(&session, &around(), 1).await;
            send(read.sender.as_mut().expect("two-way"), &[3]).await;
            assert_eq!(stopped(&mut read).await, Code(MALFORMED));
            node.clock().sleep(QUIET).await;
        },
        |session, node| {
            Box::pin(async move {
                let mut reader = session
                    .reader(select(&["a", "b"]), Mode::Complete)
                    .await
                    .expect("opens");
                let retired = Error::Reply(wire::hub::Error::Kind { kind: 3 });
                assert_eq!(reader.next().await.map(drop), Err(retired.clone()));
                assert_eq!(reader.next().await.map(drop), Err(retired));
                node.clock().sleep(QUIET).await;
            })
        },
    );
}

/// A stream that the node finishes before the last place gives the error of the
/// places, and one that it finishes before `Opened` the error of the replies.
#[test]
fn refuses_a_read_finished_before_it_opened() {
    let cases = [
        (412, 2, Error::Message(wire::hub::client::Error::Finished)),
        (413, 3, Error::Reply(wire::hub::Error::Finished)),
    ];
    for (seed, sent, error) in cases {
        by_hand(
            seed,
            move |session, node| async move {
                let mut read = accept_read(&session).await;
                start(&mut read, &around(), 1, sent).await;
                read.sender
                    .as_mut()
                    .expect("two-way")
                    .finish()
                    .expect("finishes");
                node.clock().sleep(QUIET).await;
            },
            move |session, _| {
                Box::pin(async move {
                    let opened =
                        session.reader(select(&["a", "b"]), Mode::Complete).await;
                    assert_eq!(opened.map(drop), Err(error));
                })
            },
        );
    }
}

/// Places that name one channel twice, also apart, or that `wire` refuses, are not
/// valid: the program stops the read as `MALFORMED`.
#[test]
fn stops_a_read_whose_places_are_not_valid() {
    let twice = vec![place(1, STAMP, "time"), place(1, STAMP, "again")];
    let apart = vec![
        place(10, I64, "a"),
        place(1, STAMP, "time"),
        place(10, I64, "again"),
    ];
    let unstamped = vec![place(10, I64, "a"), place(1, I64, "time")];
    let repeated =
        |n| Error::Message(wire::hub::client::Error::Repeated { key: key(n) });
    let cases = [
        (414, twice, 0, repeated(1)),
        (418, apart, 1, repeated(10)),
        (
            419,
            unstamped,
            1,
            Error::Message(wire::hub::client::Error::Unstamped { sample: I64 }),
        ),
    ];
    for (seed, places, index, error) in cases {
        by_hand(
            seed,
            move |session, node| async move {
                let mut read = accept_read(&session).await;
                start(&mut read, &places, index, places.len()).await;
                assert_eq!(stopped(&mut read).await, Code(MALFORMED));
                node.clock().sleep(QUIET).await;
            },
            move |session, node| {
                Box::pin(async move {
                    let opened = session.reader(select(&["a"]), Mode::Complete).await;
                    assert_eq!(opened.map(drop), Err(error));
                    node.clock().sleep(QUIET).await;
                })
            },
        );
    }
}

/// The code with which the program reset the stream of `read`.
async fn stopped(read: &mut Incoming) -> Code {
    match read.receiver.recv().await {
        Err(transport::Error::Reset { code }) => code,
        got => panic!("a reset, not {:?}", got.map(|got| got.map(|b| b.to_vec()))),
    }
}

/// A read that the node refuses, as it refuses one signed with another key, gives
/// [`Refusal::Refused`].
#[test]
fn gives_the_refusal_of_a_read_that_the_node_refuses() {
    by_hand(
        420,
        |session, node| async move {
            let mut read = accept_read(&session).await;
            let code = Code(Refusal::Refused.code());
            read.receiver.stop(code);
            read.sender.take().expect("two-way").reset(code);
            node.clock().sleep(QUIET).await;
        },
        |session, node| {
            Box::pin(async move {
                let opened = session.reader(select(&["a"]), Mode::Complete).await;
                assert_eq!(opened.map(drop), Err(Error::Refused(Refusal::Refused)));
                node.clock().sleep(QUIET).await;
            })
        },
    );
}

/// What the node does wrong after `Opened`.
#[derive(Clone, Copy, Debug)]
enum Fault {
    /// A frame that lacks its index.
    Lacks,
    /// A second `Opened`.
    Reopen,
    /// A body message longer than the body.
    Long,
    /// A finish.
    Finish,
    /// A finish in a body.
    FinishInBody,
}

/// Each reply that `wire` refuses, a frame that `types` refuses, and a stream that
/// ends early end the reader with their error, which each later call gives again,
/// and stop the stream as `MALFORMED`.
#[test]
fn stops_a_read_at_a_frame_or_a_reply_that_is_not_valid() {
    let lacks = types::frame::Error::IndexAbsent { entry: 0, index: 1 };
    let cases = [
        (415, Fault::Lacks, Error::Frame(lacks)),
        (
            416,
            Fault::Reopen,
            Error::Reply(wire::hub::Error::Reopen { kind: 1 }),
        ),
        (
            421,
            Fault::Long,
            Error::Reply(wire::hub::Error::Body { len: 16, remain: 8 }),
        ),
        (422, Fault::Finish, Error::Reply(wire::hub::Error::Finished)),
        (
            423,
            Fault::FinishInBody,
            Error::Reply(wire::hub::Error::Unfinished { remain: 4 }),
        ),
    ];
    for (seed, fault, error) in cases {
        by_hand(
            seed,
            move |session, node| async move {
                let mut read = opened(&session, &around(), 1).await;
                let range = Range { seq: 0, count: 1 };
                match fault {
                    Fault::Lacks => {
                        send_frame(&mut read, range, &[(0, 8)], &[0; 8], &[8]).await;
                    }
                    Fault::Reopen => {
                        reply(
                            read.sender.as_mut().expect("two-way"),
                            Reply::Opened { live: 0 },
                        )
                        .await;
                    }
                    Fault::Long => {
                        send_frame(&mut read, range, &[(1, 8)], &[0; 16], &[16]).await;
                    }
                    Fault::Finish | Fault::FinishInBody => {
                        if let Fault::FinishInBody = fault {
                            send_frame(&mut read, range, &[(1, 8)], &[0; 4], &[4])
                                .await;
                        }
                        let sender = read.sender.as_mut().expect("two-way");
                        sender.finish().expect("finishes");
                    }
                }
                assert_eq!(stopped(&mut read).await, Code(MALFORMED));
                node.clock().sleep(QUIET).await;
            },
            move |session, node| {
                Box::pin(async move {
                    let mut reader = session
                        .reader(select(&["a", "b"]), Mode::Complete)
                        .await
                        .expect("opens");
                    assert_eq!(reader.next().await.map(drop), Err(error.clone()));
                    assert_eq!(reader.next().await.map(drop), Err(error));
                    node.clock().sleep(QUIET).await;
                })
            },
        );
    }
}

/// An open after a challenge that `wire` refuses ended the renewal gives that error,
/// not the error of the close.
#[test]
fn gives_the_error_of_the_renewal_to_a_later_open() {
    raw(
        424,
        |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let sender = hello.sender.as_mut().expect("two-way");
            send(sender, &[0xff]).await;
            drop(session.closed().await);
        },
        |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            node.clock().sleep(APART).await;
            let opened = session.reader(select(&["a"]), Mode::Complete).await;
            let ended = Error::Message(wire::hub::client::Error::Kind { kind: 0xff });
            assert_eq!(opened.map(drop), Err(ended));
        },
    );
}

/// A reader open when a challenge that `wire` refuses ends the renewal gives that
/// error, not the error of the close.
#[test]
fn gives_the_error_of_the_renewal_to_an_open_reader() {
    raw(
        417,
        |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let _read = opened(&session, &around(), 1).await;
            node.clock().sleep(APART).await;
            let sender = hello.sender.as_mut().expect("two-way");
            send(sender, &[0xff]).await;
            drop(session.closed().await);
        },
        |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            let mut reader = session
                .reader(select(&["a", "b"]), Mode::Complete)
                .await
                .expect("opens");
            let ended = Err(Error::Message(wire::hub::client::Error::Kind {
                kind: 0xff,
            }));
            assert_eq!(reader.next().await.map(drop), ended);
            assert_eq!(reader.next().await.map(drop), ended);
        },
    );
}

/// A reader keeps its session open after the last clone of the session drops, and the
/// session closes when the reader drops.
#[test]
fn keeps_the_session_open_while_a_reader_lives() {
    let home = with_writes(402, &[10, 20], |session, clock| async move {
        let mut reader = session
            .reader(select(&["value"]), Mode::Complete)
            .await
            .expect("opens");
        drop(session);
        for value in [10, 20] {
            let frame = reader.next().await.expect("a frame");
            assert_eq!(samples(&reader, &frame)[0], (0, vec![value]));
        }
        drop(reader);
        clock.sleep(QUIET).await;
    });
    assert_eq!(home.served, [closed(), closed()]);
}

/// A frame that no block of `Config::frames` holds ends the reader with
/// `Error::Pool`, and the program drops the stream: the node broke no rule.
#[test]
fn ends_a_read_whose_frame_no_block_of_its_pool_holds() {
    const LEN: usize = 4096;
    raw(
        427,
        |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let mut read = opened(&session, &around(), 1).await;
            let end = u32::try_from(LEN).expect("fits");
            let range = Range { seq: 0, count: 1 };
            send_frame(&mut read, range, &[(1, end)], &[0; LEN], &[LEN]).await;
            assert_eq!(
                read.receiver
                    .recv()
                    .await
                    .map(|got| got.map(|block| block.to_vec())),
                Err(transport::Error::Reset { code: Code(0) })
            );
            node.clock().sleep(QUIET).await;
        },
        move |node, tasks, at| async move {
            let config = block::Config::new(
                u64::try_from(block::footprint(1 << 10)).expect("a usize fits a u64"),
            )
            .expect("the budget fits");
            let frames = block::Pool::new(
                config.clone(),
                block::Heap::new(config.reservation()),
            );
            let session =
                connect_with(&node, tasks, at, AGENT, ample(), Rc::new(frames))
                    .await
                    .expect("connects");
            let mut reader = session
                .reader(select(&["a", "b"]), Mode::Complete)
                .await
                .expect("opens");
            let got = reader.next().await.map(drop);
            let pool = block::Error::TooLarge {
                requested: 4136,
                largest: 1024,
            };
            assert_eq!(got, Err(Error::Pool(pool)));
            node.clock().sleep(QUIET).await;
        },
    );
}
