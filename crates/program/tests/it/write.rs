//! The writes of a session against the serve of a real hub.

use std::collections::BTreeMap;
use std::future::poll_fn;
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;

use env::clock::Clock;
use env::tasks::Tasks;
use program::write::{Config, Outcome, Refusal as Refused, Time};
use program::{Error, Reader, Session, Writer};
use spec::channel::{Channel, Data, Kind};
use spec::data_type::DataType;
use spec::definition::Definition;
use transport::Code;
use transport::stream::Incoming;
use types::authority::Authority;
use types::channel;
use types::frame::{Draft, Form, Frame, Path, Range};
use types::name::Selector;
use types::sample::{Scalar, Type};
use types::time::Span;
use wire::header::MALFORMED;
use wire::hub::client::write::Write;
use wire::hub::client::{Mode, OPEN_BYTES_MAX, Place, Refusal};
use wire::hub::write::Reply;

use crate::agent::{AGENT, name};
use crate::connect::{ample, connect, connect_receiving, connect_with};
use crate::hand::{accept_stream, by_hand, send, start};
use crate::net::own_pool;
use crate::node::env;
use crate::session::{admit, fill, raw, raw_sized, within};
use crate::sessions::{self, Home, QUIET, rules};

const STAMP: Type = Type::Scalar(Scalar::Stamp);
const I64: Type = Type::Scalar(Scalar::I64);
/// Longer than a renewal takes to end.
const APART: Span = Span::from_nanos(20_000_000);

fn key(key: u128) -> channel::Key {
    channel::Key::from_u128(key)
}

fn place(at: u128, data_type: Type, channel: &str) -> Place {
    Place {
        key: key(at),
        data_type,
        name: name(channel),
    }
}

/// `time` with key 1, and `value` and `extra` with keys 2 and 3, `i64` channels on
/// `time`.
fn definitions() -> BTreeMap<types::name::Name, Definition> {
    let time = Channel {
        key: key(1),
        kind: Kind::Index {
            error: None,
            control: None,
        },
    };
    let data = |at| Channel {
        key: key(at),
        kind: Kind::Data(
            Data::new(time.key, None, DataType::Sample(I64), None).expect("no unit"),
        ),
    };
    BTreeMap::from([
        (name("time"), Definition::Channel(time.clone())),
        (name("value"), Definition::Channel(data(2))),
        (name("extra"), Definition::Channel(data(3))),
    ])
}

/// Runs `program` with a session of `SUBJECT` on a hub with the channels of
/// [`definitions`]. The program gets the clock of its node. Gives what the home saw.
fn on_hub<P>(
    seed: u64,
    program: impl FnOnce(Session, Clock) -> P + Send + 'static,
) -> Home
where
    P: Future<Output = ()> + 'static,
{
    let open = async |node: &sim::node::Node, tasks: &Tasks| {
        let (hub, _) = hub::testing::open(env(node, tasks.clone())).await;
        hub.set_definitions(&definitions());
        (hub, ())
    };
    sessions::serve_session(
        seed,
        sim::link::Config::default(),
        Some(rules()),
        open,
        async |()| {},
        |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            program(session, node.clock()).await;
            node.clock().sleep(QUIET).await;
        },
    )
}

fn config(channels: &[&str], authority: u8) -> Config {
    Config {
        authority: Authority(authority),
        lease: None,
        channels: channels.iter().map(|channel| name(channel)).collect(),
    }
}

async fn reader(session: &Session, channels: &[&str]) -> Reader {
    let select = Selector::new(channels.iter().copied()).expect("a selector");
    session.reader(select, Mode::Complete).await.expect("opens")
}

/// The entry of the place of channel `at` of `places`.
fn entry(places: &[Place], at: u128) -> usize {
    places
        .iter()
        .position(|place| place.key == key(at))
        .expect("a place")
}

/// The result of a draft of one sample of the index and the first place of `writer`.
async fn redraft(writer: &mut Writer) -> Result<(), Error> {
    writer
        .draft(Form::Raw, 1, &[(0, 8), (1, 8)])
        .await
        .map(drop)
}

/// Fills the draft of `writer`: a raw frame with `count` samples a series, `stamp` on
/// `time`, then each of `values` on the channel of its key. A sample `n` of a series is
/// its first value plus `n`.
async fn frame<'a>(
    writer: &'a mut Writer,
    count: usize,
    stamp: i64,
    values: &[(u128, i64)],
) -> &'a mut Draft {
    let mut series: Vec<(usize, i64)> = [(1, stamp)]
        .iter()
        .chain(values)
        .map(|&(at, first)| (entry(writer.places(), at), first))
        .collect();
    series.sort_unstable();
    let lens: Vec<(usize, usize)> =
        series.iter().map(|&(at, _)| (at, 8 * count)).collect();
    let count = u32::try_from(count).expect("fits");
    let draft = writer
        .draft(Form::Raw, count, &lens)
        .await
        .expect("a frame");
    for (at, first) in series {
        let bytes = draft.series_mut(at).expect("present");
        let (samples, _) = bytes.as_chunks_mut::<8>();
        for (n, sample) in samples.iter_mut().enumerate() {
            let n = i64::try_from(n).expect("fits");
            *sample = (first + n).to_le_bytes();
        }
    }
    draft
}

/// Each series of `frame`, a frame of `reader`, as its key and its samples.
fn samples(reader: &Reader, frame: &Frame) -> Vec<(channel::Key, Vec<i64>)> {
    let count = frame.range(0).expect("the index group").count;
    let count = usize::try_from(count).expect("fits");
    frame
        .iter()
        .map(|(entry, bytes)| {
            let mut out = Vec::new();
            let place = &reader.places()[entry];
            codec::decode(place.data_type, count, bytes, &mut out).expect("decodes");
            let (chunks, _) = out.as_chunks::<8>();
            (
                place.key,
                chunks
                    .iter()
                    .map(|chunk| i64::from_le_bytes(*chunk))
                    .collect(),
            )
        })
        .collect()
}

/// The stamp of the one sample of `frame`, a frame of `reader`.
fn stamp_of(reader: &Reader, frame: &Frame) -> i64 {
    let samples = samples(reader, frame);
    let (_, stamps) = samples
        .iter()
        .find(|(at, _)| *at == key(1))
        .expect("the index");
    stamps[0]
}

/// The places of a writer of `series` places: `time`, then `i64` channels.
fn wide(series: usize) -> Vec<Place> {
    [place(1, STAMP, "time")]
        .into_iter()
        .chain(
            (2..)
                .take(series - 1)
                .map(|at| place(at, I64, &format!("c{at:05}"))),
        )
        .collect()
}

/// Accepts the next write of `session` and opens it with `places`.
async fn opened_with(session: &transport::Session, places: &[Place]) -> Incoming {
    let mut write = accept_stream(session, |first| {
        Write::decode(first).expect("a write").body()
    })
    .await;
    start(&mut write, places, 0, places.len()).await;
    write
}

/// Sends `outcome` of `frame` on `write`.
async fn answer(write: &mut Incoming, frame: u64, outcome: Outcome) {
    reply(write, Reply::Outcome { frame, outcome }).await;
}

async fn reply(write: &mut Incoming, reply: Reply) {
    let mut bytes = vec![0; reply.encoded_len()];
    reply.encode(&mut bytes);
    send(write.sender.as_mut().expect("two-way"), &bytes).await;
}

fn taken(seq: u64, count: u32) -> Outcome {
    Outcome::Taken {
        range: Range { seq, count },
    }
}

/// The node stamps a frame of `Time::Node`, a frame of `Time::Frame` holds its own
/// stamps, and a reader on the home gets both in order.
#[test]
fn writes_each_frame_to_a_reader_on_the_home_in_order() {
    on_hub(700, |session, _| async move {
        let mut reader = reader(&session, &["value"]).await;
        let mut writer = session.writer(config(&["value"], 1)).await.expect("opens");
        assert_eq!(
            writer.places(),
            [place(1, STAMP, "time"), place(2, I64, "value")]
        );
        assert_eq!(writer.index(), 0);
        frame(&mut writer, 1, 0, &[(2, 10)]).await;
        assert_eq!(writer.write(Time::Node).await, Ok(taken(0, 1)));
        let first = reader.next().await.expect("a frame");
        let stamp = stamp_of(&reader, &first);
        assert_eq!(samples(&reader, &first)[0], (key(2), vec![10]));
        frame(&mut writer, 2, stamp + 5, &[(2, 20)]).await;
        let outcome = writer.write(Time::Frame(Path::Live)).await;
        assert_eq!(outcome, Ok(taken(1, 2)));
        let second = reader.next().await.expect("a frame");
        assert_eq!(
            samples(&reader, &second),
            [(key(2), vec![20, 21]), (key(1), vec![stamp + 5, stamp + 6])]
        );
    });
}

/// `index` gives the place that the home gives for the index, also when it is not
/// the first place.
#[test]
fn gives_the_place_of_the_index() {
    by_hand(
        767,
        |session, node| async move {
            let mut write = accept_stream(&session, |first| {
                Write::decode(first).expect("a write").body()
            })
            .await;
            let places = [place(2, I64, "value"), place(1, STAMP, "time")];
            start(&mut write, &places, 1, places.len()).await;
            node.clock().sleep(QUIET).await;
        },
        |session, node| {
            Box::pin(async move {
                let writer =
                    session.writer(config(&["value"], 1)).await.expect("opens");
                assert_eq!(writer.index(), 1);
                node.clock().sleep(QUIET).await;
            })
        },
    );
}

/// A frame whose series fill fewer places than the writer has puts each series at its
/// own place.
#[test]
fn writes_a_frame_that_fills_fewer_places_than_the_writer_has() {
    on_hub(701, |session, _| async move {
        let mut reader = reader(&session, &["extra", "value"]).await;
        let mut writer = session
            .writer(config(&["value", "extra"], 1))
            .await
            .expect("opens");
        frame(&mut writer, 1, 0, &[(3, 30)]).await;
        assert_eq!(writer.write(Time::Node).await, Ok(taken(0, 1)));
        let got = reader.next().await.expect("a frame");
        let got = samples(&reader, &got);
        let keys: Vec<channel::Key> = got.iter().map(|(at, _)| *at).collect();
        assert!(keys.contains(&key(1)) && keys.contains(&key(3)), "{got:?}");
        assert!(!keys.contains(&key(2)), "{got:?}");
        assert!(got.contains(&(key(3), vec![30])), "{got:?}");
    });
}

/// A frame of a body over the most bytes of one message goes in more than one, and
/// the reader gets it whole.
#[test]
fn writes_a_body_over_one_message_whole() {
    const COUNT: usize = 20_000;
    on_hub(702, |session, _| async move {
        let mut reader = reader(&session, &["value"]).await;
        let mut writer = session.writer(config(&["value"], 1)).await.expect("opens");
        frame(&mut writer, COUNT, 1_000, &[(2, 7)]).await;
        let count = u32::try_from(COUNT).expect("fits");
        let outcome = writer.write(Time::Frame(Path::Live)).await;
        assert_eq!(outcome, Ok(taken(0, count)));
        let got = reader.next().await.expect("a frame");
        let got = samples(&reader, &got);
        let values: Vec<i64> = (7..).take(COUNT).collect();
        let stamps: Vec<i64> = (1_000..).take(COUNT).collect();
        assert_eq!(got, [(key(2), values), (key(1), stamps)]);
    });
}

/// A frame of `Time::Frame` whose stamp is not after the last stamp that the node
/// wrote is refused, and the writer goes on.
#[test]
fn refuses_a_frame_whose_stamp_is_not_after_the_last_stamp_of_the_node() {
    on_hub(703, |session, _| async move {
        let mut reader = reader(&session, &["value"]).await;
        let mut writer = session.writer(config(&["value"], 1)).await.expect("opens");
        frame(&mut writer, 1, 0, &[(2, 10)]).await;
        assert_eq!(writer.write(Time::Node).await, Ok(taken(0, 1)));
        let first = reader.next().await.expect("a frame");
        let stamp = stamp_of(&reader, &first);
        frame(&mut writer, 1, stamp, &[(2, 11)]).await;
        let outcome = writer.write(Time::Frame(Path::Live)).await;
        assert_eq!(outcome, Ok(Outcome::Refused(Refused::Order)));
        frame(&mut writer, 1, 0, &[(2, 12)]).await;
        assert_eq!(writer.write(Time::Node).await, Ok(taken(1, 1)));
        let next = reader.next().await.expect("a frame");
        let later = stamp_of(&reader, &next);
        assert!(later > stamp, "{later} after {stamp}");
    });
}

/// A second writer on a channel whose control the first holds gets `Waiting`.
#[test]
fn gives_waiting_to_a_second_writer_while_the_first_holds_control() {
    on_hub(704, |session, _| async move {
        let mut first = session.writer(config(&["value"], 2)).await.expect("opens");
        let mut second = session.writer(config(&["value"], 1)).await.expect("opens");
        frame(&mut first, 1, 0, &[(2, 1)]).await;
        assert_eq!(first.write(Time::Node).await, Ok(taken(0, 1)));
        frame(&mut second, 1, 0, &[(2, 2)]).await;
        let outcome = second.write(Time::Node).await;
        assert_eq!(outcome, Ok(Outcome::Refused(Refused::Waiting)));
    });
}

/// `stored` gives at once with no taken frame, and waits for the commit of a taken
/// frame.
#[test]
fn gives_stored_once_the_commit_covers_the_last_taken_frame() {
    on_hub(705, |session, clock| async move {
        let mut writer = session.writer(config(&["value"], 1)).await.expect("opens");
        {
            let mut stored = pin!(writer.stored());
            let now = poll_fn(|cx| Poll::Ready(stored.as_mut().poll(cx)));
            assert_eq!(now.await, Poll::Ready(Ok(())));
        }
        frame(&mut writer, 1, 0, &[(2, 1)]).await;
        assert_eq!(writer.write(Time::Node).await, Ok(taken(0, 1)));
        let before = clock.now();
        assert_eq!(writer.stored().await, Ok(()));
        assert!(clock.now() > before, "stored waits for the commit");
        let mut stored = pin!(writer.stored());
        let now = poll_fn(|cx| Poll::Ready(stored.as_mut().poll(cx)));
        assert_eq!(now.await, Poll::Ready(Ok(())));
    });
}

/// A frame that the node cannot take as it is gives `Unsent` with nothing sent, and
/// the writer goes on.
#[test]
fn gives_unsent_for_a_frame_of_no_sample_or_a_bad_frame_to_stamp() {
    on_hub(706, |session, _| async move {
        let mut writer = session.writer(config(&["value"], 1)).await.expect("opens");
        frame(&mut writer, 1, 0, &[(2, 1)]).await.set_count(0, 0);
        let got = writer.write(Time::Frame(Path::Live)).await;
        assert_eq!(
            got,
            Err(Error::Unsent {
                count: 0,
                form: Form::Raw
            })
        );
        assert_eq!(
            got.unwrap_err().to_string(),
            "the frame holds no sample, so it was not sent"
        );
        frame(&mut writer, 2, 0, &[(2, 1)]).await;
        let got = writer.write(Time::Node).await;
        assert_eq!(
            got,
            Err(Error::Unsent {
                count: 2,
                form: Form::Raw
            })
        );
        assert_eq!(
            got.unwrap_err().to_string(),
            "a frame that the node stamps holds 1 sample in the raw form, not 2 in the \
             Raw form, so it was not sent"
        );
        let places = writer.places().len();
        let series: Vec<(usize, usize)> = (0..places).map(|at| (at, 8)).collect();
        writer
            .draft(Form::Encoded, 1, &series)
            .await
            .expect("a frame");
        let got = writer.write(Time::Node).await;
        assert_eq!(
            got,
            Err(Error::Unsent {
                count: 1,
                form: Form::Encoded
            })
        );
        frame(&mut writer, 1, 0, &[(2, 1)]).await;
        assert_eq!(writer.write(Time::Node).await, Ok(taken(0, 1)));
    });
}

/// A writer of no channel, or of an open over the cap, sends nothing.
#[test]
fn refuses_a_writer_of_no_channel_or_over_the_cap_with_nothing_sent() {
    let names: Vec<String> = (0..300)
        .map(|n| format!("n{n:03}{}", "x".repeat(240)))
        .collect();
    let channels: Vec<_> = names.iter().map(|n| name(n)).collect();
    let open = wire::hub::client::write::Open {
        authority: Authority(1),
        lease: None,
        channels: channels.clone(),
    };
    let length = open.encoded_len();
    assert!(u64::try_from(length).expect("fits") > OPEN_BYTES_MAX);
    let home = on_hub(707, move |session, _| async move {
        let got = session.writer(config(&[], 1)).await;
        let error = got.map(drop).unwrap_err();
        assert_eq!(error, Error::Unnamed);
        assert_eq!(error.to_string(), "the stream names no channel");
        let over = Config {
            authority: Authority(1),
            lease: None,
            channels,
        };
        let got = session.writer(over).await;
        assert_eq!(got.map(drop), Err(Error::Open { length }));
    });
    assert_eq!(home.served.len(), 1, "only the hello: {:?}", home.served);
}

/// A write that the node refuses, as it refuses one signed with another key, gives
/// [`Refusal::Refused`].
#[test]
fn gives_the_refusal_of_a_write_that_the_node_refuses() {
    by_hand(
        708,
        |session, node| async move {
            let mut write = accept_stream(&session, |first| {
                Write::decode(first).expect("a write").body()
            })
            .await;
            let code = Code(Refusal::Refused.code());
            write.receiver.stop(code);
            write.sender.take().expect("two-way").reset(code);
            node.clock().sleep(QUIET).await;
        },
        |session, node| {
            Box::pin(async move {
                let got = session.writer(config(&["value"], 1)).await;
                let error = got.map(drop).unwrap_err();
                assert_eq!(error, Error::Refused(Refusal::Refused));
                assert_eq!(error.to_string(), Refusal::Refused.to_string());
                node.clock().sleep(QUIET).await;
            })
        },
    );
}

/// A call dropped after it sent its frame loses its outcome, and the next call gives
/// its own.
#[test]
fn gives_the_next_outcome_after_a_call_that_dropped_after_its_send() {
    on_hub(709, |session, _| async move {
        let mut writer = session.writer(config(&["value"], 1)).await.expect("opens");
        {
            frame(&mut writer, 1, 0, &[(2, 1)]).await;
            let mut write = pin!(writer.write(Time::Node));
            let waits = poll_fn(|cx| Poll::Ready(write.as_mut().poll(cx).is_pending()));
            assert!(waits.await, "the write waits for its outcome");
        }
        frame(&mut writer, 1, 0, &[(2, 2)]).await;
        assert_eq!(writer.write(Time::Node).await, Ok(taken(1, 1)));
    });
}

/// A `stored` after a call that dropped after its send waits for the commit of that
/// frame.
#[test]
fn waits_in_stored_for_the_frame_of_a_call_that_dropped_after_its_send() {
    on_hub(717, |session, _| async move {
        let mut writer = session.writer(config(&["value"], 1)).await.expect("opens");
        {
            frame(&mut writer, 1, 0, &[(2, 1)]).await;
            let mut write = pin!(writer.write(Time::Node));
            let waits = poll_fn(|cx| Poll::Ready(write.as_mut().poll(cx).is_pending()));
            assert!(waits.await, "the write waits for its outcome");
        }
        {
            let mut stored = pin!(writer.stored());
            let waits =
                poll_fn(|cx| Poll::Ready(stored.as_mut().poll(cx).is_pending()));
            assert!(waits.await, "stored waits for the outcome and the commit");
            assert_eq!(stored.await, Ok(()));
        }
        frame(&mut writer, 1, 0, &[(2, 2)]).await;
        assert_eq!(writer.write(Time::Node).await, Ok(taken(1, 1)));
    });
}

/// An open of exactly the cap reaches the node, which finds no channel.
#[test]
fn sends_an_open_at_the_cap() {
    let open = |names: &[String]| wire::hub::client::write::Open {
        authority: Authority(1),
        lease: None,
        channels: names.iter().map(|n| name(n)).collect(),
    };
    let mut names: Vec<String> = (0..256)
        .map(|n| format!("n{n:03}{}", "x".repeat(251)))
        .collect();
    let over =
        open(&names).encoded_len() - usize::try_from(OPEN_BYTES_MAX).expect("fits");
    names[0].truncate(255 - over);
    let open = open(&names);
    assert_eq!(u64::try_from(open.encoded_len()), Ok(OPEN_BYTES_MAX));
    on_hub(718, move |session, _| async move {
        let config = Config {
            authority: open.authority,
            lease: open.lease,
            channels: open.channels,
        };
        let got = session.writer(config).await.map(drop);
        assert_eq!(got, Err(Error::Refused(Refusal::Unknown)));
    });
}

/// A frame whose ends are over the most bytes of one message sends them in more than
/// one, each but the last of the most whole ends that a body message holds, and the
/// node takes them in order.
#[test]
fn sends_ends_over_one_message_as_whole_ends() {
    const SERIES: usize = 1 << 15;
    let places = wide(SERIES);
    by_hand(
        719,
        move |session, node| async move {
            let mut write = opened_with(&session, &places).await;
            let mut next = async || {
                let message = write.receiver.recv().await.expect("receives");
                message.expect("a message of the frame").to_vec()
            };
            next().await;
            let mut ends = Vec::new();
            let mut runs = Vec::new();
            while ends.len() < SERIES * 8 {
                let message = next().await;
                ends.extend_from_slice(&message);
                runs.push(message.len());
            }
            assert!(runs.len() > 1, "the ends go in {} messages", runs.len());
            let want: Vec<u8> = (1..=SERIES)
                .flat_map(|at| {
                    let at = u32::try_from(at).expect("fits");
                    [(at - 1).to_le_bytes(), (8 * at).to_le_bytes()].concat()
                })
                .collect();
            assert_eq!(ends, want);
            let most = next().await.len();
            let mut body = most;
            while body < SERIES * 8 {
                body += next().await.len();
            }
            let (last, full) = runs.split_last().expect("ends");
            assert!(
                full.iter().all(|&run| run == most / 8 * 8),
                "{runs:?} of {most}"
            );
            assert_eq!(last % 8, 0, "whole ends");
            answer(&mut write, 1, taken(0, 1)).await;
            node.clock().sleep(QUIET).await;
        },
        |session, node| {
            Box::pin(async move {
                let mut writer =
                    session.writer(config(&["c00002"], 1)).await.expect("opens");
                let series: Vec<(usize, usize)> =
                    (0..SERIES).map(|at| (at, 8)).collect();
                writer.draft(Form::Raw, 1, &series).await.expect("a frame");
                let outcome = writer.write(Time::Frame(Path::Live)).await;
                assert_eq!(outcome, Ok(taken(0, 1)));
                node.clock().sleep(QUIET).await;
            })
        },
    );
}

/// A call dropped while it sent its frame ends the writer, and the home gets no part
/// of the cut frame or of the next.
#[test]
fn ends_the_writer_after_a_call_that_dropped_while_it_sent() {
    const COUNT: usize = 1 << 17;
    on_hub(710, |session, clock| async move {
        let mut reader = reader(&session, &["value"]).await;
        let mut writer = session.writer(config(&["value"], 1)).await.expect("opens");
        {
            frame(&mut writer, COUNT, 1_000, &[(2, 1)]).await;
            let mut write = pin!(writer.write(Time::Frame(Path::Live)));
            let waits = poll_fn(|cx| Poll::Ready(write.as_mut().poll(cx).is_pending()));
            assert!(waits.await, "the write waits for room");
        }
        let reset = Err(Error::Transport(transport::Error::Reset { code: Code(0) }));
        assert_eq!(redraft(&mut writer).await, reset);
        assert_eq!(redraft(&mut writer).await, reset);
        assert_eq!(writer.stored().await, reset);
        let got = within(&clock, QUIET, reader.next()).await;
        assert!(got.is_none(), "{got:?}");
    });
}

/// Accepts the next write of `session` and opens it with `time` and `value`.
async fn opened(session: &transport::Session) -> Incoming {
    opened_with(session, &[place(1, STAMP, "time"), place(2, I64, "value")]).await
}

/// Takes the head, the ends, and the body of a frame of 2 series.
async fn take_frame(write: &mut Incoming) {
    for _ in ["head", "ends", "body"] {
        let message = write.receiver.recv().await.expect("receives");
        message.expect("a message of the frame");
    }
}

/// The code that the program reset `write` with.
async fn reset(write: &mut Incoming) -> Code {
    match write.receiver.recv().await {
        Err(transport::Error::Reset { code }) => code,
        got => panic!("a reset, not {:?}", got.map(|got| got.map(|b| b.to_vec()))),
    }
}

/// A reply of a kind that no reply has, or a finish where a reply must come, ends the
/// writer with the error of the replies at each later call, and stops the stream as
/// `MALFORMED`.
#[test]
fn ends_the_writer_at_a_reply_that_wire_refuses() {
    let cases = [
        (711, Some(9), wire::hub::Error::Kind { kind: 9 }),
        (712, None, wire::hub::Error::Finished),
    ];
    for (seed, kind, error) in cases {
        by_hand(
            seed,
            move |session, node| async move {
                let mut write = opened(&session).await;
                take_frame(&mut write).await;
                let sender = write.sender.as_mut().expect("two-way");
                match kind {
                    Some(kind) => send(sender, &[kind]).await,
                    None => sender.finish().expect("finishes"),
                }
                assert_eq!(reset(&mut write).await, Code(MALFORMED));
                node.clock().sleep(QUIET).await;
            },
            move |session, node| {
                Box::pin(async move {
                    let mut writer =
                        session.writer(config(&["value"], 1)).await.expect("opens");
                    let ended = Err(Error::Reply(error));
                    frame(&mut writer, 1, 0, &[(2, 1)]).await;
                    assert_eq!(writer.write(Time::Node).await, ended);
                    let ended = ended.map(drop);
                    assert_eq!(redraft(&mut writer).await, ended);
                    assert_eq!(writer.stored().await, ended);
                    node.clock().sleep(QUIET).await;
                })
            },
        );
    }
}

/// Each writer sends its own draft: the draft of one writer never goes to the channels
/// of another of the same types.
#[test]
fn sends_the_draft_of_each_writer_to_its_own_channels() {
    on_hub(713, |session, clock| async move {
        let mut reader = reader(&session, &["extra", "value"]).await;
        let mut value = session.writer(config(&["value"], 1)).await.expect("opens");
        let mut extra = session.writer(config(&["extra"], 1)).await.expect("opens");
        frame(&mut value, 1, 1_000, &[(2, 20)]).await;
        frame(&mut extra, 1, 2_000, &[(3, 30)]).await;
        assert_eq!(value.write(Time::Frame(Path::Live)).await, Ok(taken(0, 1)));
        drop(value);
        clock.sleep(QUIET).await;
        assert_eq!(extra.write(Time::Frame(Path::Live)).await, Ok(taken(1, 1)));
        for (at, other, sample) in [(2, 3, 20), (3, 2, 30)] {
            let got = reader.next().await.expect("a frame");
            let got = samples(&reader, &got);
            assert!(got.contains(&(key(at), vec![sample])), "{got:?}");
            assert!(
                !got.iter().any(|(key_of, _)| *key_of == key(other)),
                "{got:?}"
            );
        }
    });
}

/// A write with no draft is a bug of the program.
#[test]
#[should_panic(expected = "a write sends the draft of a call of `Writer::draft`")]
fn panics_at_a_write_with_no_draft() {
    on_hub(714, |session, _| async move {
        let mut writer = session.writer(config(&["value"], 1)).await.expect("opens");
        frame(&mut writer, 1, 0, &[(2, 1)]).await;
        assert_eq!(writer.write(Time::Node).await, Ok(taken(0, 1)));
        drop(writer.write(Time::Node).await);
    });
}

/// A write that gives `Unsent` drops the draft, so the next write with no new draft
/// panics.
#[test]
#[should_panic(expected = "a write sends the draft of a call of `Writer::draft`")]
fn panics_at_a_write_after_an_unsent_with_no_new_draft() {
    on_hub(720, |session, _| async move {
        let mut writer = session.writer(config(&["value"], 1)).await.expect("opens");
        frame(&mut writer, 1, 0, &[(2, 1)]).await.set_count(0, 0);
        let unsent = Err(Error::Unsent {
            count: 0,
            form: Form::Raw,
        });
        assert_eq!(writer.write(Time::Node).await, unsent);
        drop(writer.write(Time::Node).await);
    });
}

/// A draft replaces the draft before it, and a write dropped before its first poll
/// leaves the draft held for the next write.
#[test]
fn sends_the_last_draft_after_a_write_dropped_before_its_first_poll() {
    on_hub(721, |session, _| async move {
        let mut reader = reader(&session, &["value"]).await;
        let mut writer = session.writer(config(&["value"], 1)).await.expect("opens");
        frame(&mut writer, 1, 0, &[(2, 1)]).await;
        frame(&mut writer, 1, 0, &[(2, 5)]).await;
        drop(writer.write(Time::Node));
        assert_eq!(writer.write(Time::Node).await, Ok(taken(0, 1)));
        let got = reader.next().await.expect("a frame");
        assert_eq!(samples(&reader, &got)[0], (key(2), vec![5]));
    });
}

/// A `stored` dropped before it gave leaves the writer whole: the next `stored` waits
/// for the same commit, and the next write gives its own outcome.
#[test]
fn keeps_the_writer_whole_after_a_dropped_stored() {
    on_hub(715, |session, _| async move {
        let mut writer = session.writer(config(&["value"], 1)).await.expect("opens");
        frame(&mut writer, 1, 0, &[(2, 1)]).await;
        assert_eq!(writer.write(Time::Node).await, Ok(taken(0, 1)));
        {
            let mut stored = pin!(writer.stored());
            let waits =
                poll_fn(|cx| Poll::Ready(stored.as_mut().poll(cx).is_pending()));
            assert!(waits.await, "stored waits for the commit");
        }
        assert_eq!(writer.stored().await, Ok(()));
        frame(&mut writer, 1, 0, &[(2, 2)]).await;
        assert_eq!(writer.write(Time::Node).await, Ok(taken(1, 1)));
    });
}

/// A writer whose head and ends of a frame no block of `Config::pool` holds fails at
/// the open with `Error::Pool`, and the program drops the stream. A writer of one
/// channel fewer opens.
#[test]
fn refuses_a_writer_whose_frame_lead_no_block_of_its_pool_holds() {
    const CHANNELS: usize = 126;
    let names: Vec<String> = (0..CHANNELS).map(|at| format!("c{at:03}")).collect();
    let places: Vec<Place> = [place(1, STAMP, "time")]
        .into_iter()
        .chain(names.iter().zip(2..).map(|(n, at)| place(at, I64, n)))
        .collect();
    raw(
        716,
        move |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let _fits = opened_with(&session, &places[..CHANNELS]).await;
            let mut write = opened_with(&session, &places).await;
            assert_eq!(reset(&mut write).await, Code(0));
            node.clock().sleep(QUIET).await;
        },
        move |node, tasks, at| async move {
            let config = block::Config::new(
                u64::try_from(block::footprint(1 << 10)).expect("a usize fits a u64"),
            )
            .expect("the budget fits");
            let pool = block::Pool::new(
                config.clone(),
                block::Heap::new(config.reservation()),
            );
            let session = connect_with(&node, tasks, at, AGENT, Rc::new(pool), ample())
                .await
                .expect("connects");
            let config = |count| Config {
                authority: Authority(1),
                lease: None,
                channels: names[..count].iter().map(|n| name(n)).collect(),
            };
            let fits = session.writer(config(CHANNELS - 1)).await.expect("opens");
            assert_eq!(fits.places().len(), CHANNELS);
            let got = session.writer(config(CHANNELS)).await.map(drop);
            let pool = block::Error::TooLarge {
                requested: 11 + 8 * (CHANNELS + 1),
                largest: 1024,
            };
            assert_eq!(got, Err(Error::Pool(pool)));
            node.clock().sleep(QUIET).await;
        },
    );
}

/// A node whose largest message is not a multiple of 8 gets each run of ends but the
/// last as the most whole ends under that size, and the body in messages of that size.
#[test]
fn sends_whole_ends_to_a_node_whose_message_is_not_a_multiple_of_8() {
    const SERIES: usize = 400;
    let places = wide(SERIES);
    raw_sized(
        722,
        (1500, 1 << 20),
        move |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let mut write = opened_with(&session, &places).await;
            let mut lens = Vec::new();
            while lens.iter().sum::<usize>() < 11 + 2 * 8 * SERIES {
                let message = write.receiver.recv().await.expect("receives");
                lens.push(message.expect("a message of the frame").len());
            }
            assert_eq!(lens, [11, 1496, 1496, 208, 1500, 1500, 200]);
            answer(&mut write, 1, taken(0, 1)).await;
            node.clock().sleep(QUIET).await;
        },
        |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            let mut writer =
                session.writer(config(&["c00002"], 1)).await.expect("opens");
            let series: Vec<(usize, usize)> = (0..SERIES).map(|at| (at, 8)).collect();
            writer.draft(Form::Raw, 1, &series).await.expect("a frame");
            let outcome = writer.write(Time::Frame(Path::Live)).await;
            assert_eq!(outcome, Ok(taken(0, 1)));
            node.clock().sleep(QUIET).await;
        },
    );
}

/// A call dropped in the run of ends ends the writer with a reset of `Code(0)`, and the
/// node gets the head and whole messages of ends before the reset.
#[test]
fn ends_the_writer_after_a_call_dropped_in_its_run_of_ends() {
    const SERIES: usize = 2000;
    let places = wide(SERIES);
    raw_sized(
        723,
        (1500, 3000),
        move |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let mut write = opened_with(&session, &places).await;
            let mut lens = Vec::new();
            let code = loop {
                match write.receiver.recv().await {
                    Ok(Some(message)) => lens.push(message.len()),
                    Err(transport::Error::Reset { code }) => break code,
                    got => panic!("{:?} after {lens:?}", got.map(|got| got.is_some())),
                }
            };
            assert_eq!(code, Code(0));
            let (head, ends) = lens.split_first().expect("the head");
            assert_eq!(*head, 11);
            assert!(!ends.is_empty(), "{lens:?}");
            assert!(ends.len() < 8 * SERIES / 1496, "{lens:?}");
            assert!(ends.iter().all(|&len| len == 1496), "{lens:?}");
            node.clock().sleep(QUIET).await;
        },
        |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            let mut writer =
                session.writer(config(&["c00002"], 1)).await.expect("opens");
            let series: Vec<(usize, usize)> = (0..SERIES).map(|at| (at, 8)).collect();
            writer.draft(Form::Raw, 1, &series).await.expect("a frame");
            {
                let mut write = pin!(writer.write(Time::Frame(Path::Live)));
                let waits =
                    poll_fn(|cx| Poll::Ready(write.as_mut().poll(cx).is_pending()));
                assert!(waits.await, "the write waits for room for the ends");
                node.clock()
                    .sleep(Span::from_nanos(QUIET.nanos() / 2))
                    .await;
            }
            let reset =
                Err(Error::Transport(transport::Error::Reset { code: Code(0) }));
            assert_eq!(writer.draft(Form::Raw, 1, &series).await.map(drop), reset);
            node.clock().sleep(QUIET).await;
        },
    );
}

/// A call dropped between two whole messages of its frame leaves the stream open, and
/// `finish` resets it with `Code(0)`: the node never gets a finish inside the frame.
#[test]
fn resets_at_the_finish_after_a_call_dropped_between_messages_of_its_frame() {
    const SERIES: usize = 2000;
    let places = wide(SERIES);
    // The window holds the head and two whole messages of ends, and no byte more.
    raw_sized(
        751,
        (1500, 3008),
        move |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let mut write = opened_with(&session, &places).await;
            node.clock().sleep(QUIET).await;
            let mut lens = Vec::new();
            let code = loop {
                match write.receiver.recv().await {
                    Ok(Some(message)) => lens.push(message.len()),
                    Err(transport::Error::Reset { code }) => break code,
                    got => panic!("{:?} after {lens:?}", got.map(|got| got.is_some())),
                }
            };
            assert_eq!(code, Code(0), "after {lens:?}");
            node.clock().sleep(QUIET).await;
        },
        |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            let mut writer =
                session.writer(config(&["c00002"], 1)).await.expect("opens");
            let series: Vec<(usize, usize)> = (0..SERIES).map(|at| (at, 8)).collect();
            writer.draft(Form::Raw, 1, &series).await.expect("a frame");
            {
                let mut write = pin!(writer.write(Time::Frame(Path::Live)));
                let waits =
                    poll_fn(|cx| Poll::Ready(write.as_mut().poll(cx).is_pending()));
                assert!(waits.await, "the write waits for room for the ends");
            }
            let reset =
                Err(Error::Transport(transport::Error::Reset { code: Code(0) }));
            assert_eq!(writer.finish().await, reset);
            node.clock().sleep(QUIET).await;
        },
    );
}

/// A draft drops the draft that the writer holds before it takes a block, so a pool
/// of frames that holds one draft gives a second, and the write sends it.
#[test]
fn replaces_a_held_draft_in_a_frames_pool_of_one_frame() {
    const COUNT: usize = 200;
    raw(
        724,
        |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let mut write = opened(&session).await;
            let mut body = Vec::new();
            for _ in ["head", "ends", "body"] {
                let message = write.receiver.recv().await.expect("receives");
                body = message.expect("a message of the frame").to_vec();
            }
            let at = 8 * COUNT;
            assert_eq!(body[at..at + 8], 5_i64.to_le_bytes());
            answer(&mut write, 1, taken(0, 200)).await;
            node.clock().sleep(QUIET).await;
        },
        |node, tasks, at| async move {
            let budget = block::Config::new(
                u64::try_from(block::footprint(1 << 12)).expect("a usize fits a u64"),
            )
            .expect("the budget fits");
            let frames = block::Pool::new(
                budget.clone(),
                block::Heap::new(budget.reservation()),
            );
            let session =
                connect_with(&node, tasks, at, AGENT, ample(), Rc::new(frames))
                    .await
                    .expect("connects");
            let mut writer =
                session.writer(config(&["value"], 1)).await.expect("opens");
            frame(&mut writer, COUNT, 1_000, &[(2, 1)]).await;
            let clock = node.clock();
            let replaced = within(&clock, QUIET, async {
                frame(&mut writer, COUNT, 1_000, &[(2, 5)]).await;
            });
            assert_eq!(replaced.await, Some(()), "the held draft gives its block");
            let outcome = writer.write(Time::Frame(Path::Live)).await;
            assert_eq!(outcome, Ok(taken(0, 200)));
            node.clock().sleep(QUIET).await;
        },
    );
}

/// A writer opened after a challenge that `wire` refuses ended the renewal gives that
/// error, not the error of the close.
#[test]
fn gives_the_error_of_the_renewal_to_a_later_writer() {
    raw(
        725,
        |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            send(hello.sender.as_mut().expect("two-way"), &[0xff]).await;
            drop(session.closed().await);
        },
        |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            node.clock().sleep(APART).await;
            let got = session.writer(config(&["value"], 1)).await.map(drop);
            let ended = Error::Message(wire::hub::client::Error::Kind { kind: 0xff });
            assert_eq!(got, Err(ended));
        },
    );
}

/// A writer that waits for an outcome when a challenge that `wire` refuses ends the
/// renewal gives that error at each call, a draft too, not the error of the close.
#[test]
fn gives_the_error_of_the_renewal_to_an_open_writer() {
    raw(
        726,
        |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let mut write = opened(&session).await;
            take_frame(&mut write).await;
            send(hello.sender.as_mut().expect("two-way"), &[0xff]).await;
            drop(session.closed().await);
        },
        |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            let mut writer =
                session.writer(config(&["value"], 1)).await.expect("opens");
            let ended = Err(Error::Message(wire::hub::client::Error::Kind {
                kind: 0xff,
            }));
            frame(&mut writer, 1, 0, &[(2, 1)]).await;
            assert_eq!(writer.write(Time::Node).await, ended);
            let ended = ended.map(drop);
            assert_eq!(redraft(&mut writer).await, ended);
            assert_eq!(writer.stored().await, ended);
            assert_eq!(writer.finish().await, ended);
        },
    );
}

/// Places that name one channel twice, a stream that the node finishes before the
/// last place, or places with no place of a channel of the open give the error of the
/// places, and the program stops the write as `MALFORMED`.
#[test]
fn stops_a_write_whose_places_are_not_valid() {
    let twice = vec![place(1, STAMP, "time"), place(1, STAMP, "again")];
    let whole = vec![place(1, STAMP, "time"), place(2, I64, "value")];
    let other = vec![place(1, STAMP, "time"), place(2, I64, "other")];
    let message = Error::Message;
    let cases = [
        (
            727,
            twice,
            2,
            message(wire::hub::client::Error::Repeated { key: key(1) }),
        ),
        (728, whole, 1, message(wire::hub::client::Error::Finished)),
        (729, other, 2, Error::Unplaced("value".parse().unwrap())),
    ];
    assert_eq!(
        cases[2].3.to_string(),
        "the node gave no place for the channel value of the open"
    );
    for (seed, places, sent, error) in cases {
        by_hand(
            seed,
            move |session, node| async move {
                let mut write = accept_stream(&session, |first| {
                    Write::decode(first).expect("a write").body()
                })
                .await;
                start(&mut write, &places, 0, sent).await;
                if sent < places.len() {
                    let sender = write.sender.as_mut().expect("two-way");
                    sender.finish().expect("finishes");
                }
                assert_eq!(reset(&mut write).await, Code(MALFORMED));
                node.clock().sleep(QUIET).await;
            },
            move |session, node| {
                Box::pin(async move {
                    let got = session.writer(config(&["value"], 1)).await.map(drop);
                    assert_eq!(got, Err(error));
                    node.clock().sleep(QUIET).await;
                })
            },
        );
    }
}

/// An outcome of a frame past those sent ends the writer with `Unasked`, and the
/// program stops the stream as `MALFORMED`.
#[test]
fn ends_the_writer_at_an_outcome_past_the_frames_sent() {
    by_hand(
        740,
        |session, node| async move {
            let mut write = opened(&session).await;
            take_frame(&mut write).await;
            answer(&mut write, 1, taken(0, 1)).await;
            answer(&mut write, 2, Outcome::Refused(Refused::Order)).await;
            reply(&mut write, Reply::Stored { frames: 1 }).await;
            let sender = write.sender.as_mut().expect("two-way");
            sender.finish().expect("finishes");
            assert_eq!(reset(&mut write).await, Code(MALFORMED));
            node.clock().sleep(QUIET).await;
        },
        |session, node| {
            Box::pin(async move {
                let mut writer =
                    session.writer(config(&["value"], 1)).await.expect("opens");
                frame(&mut writer, 1, 0, &[(2, 1)]).await;
                assert_eq!(writer.write(Time::Node).await, Ok(taken(0, 1)));
                let unasked = wire::hub::Error::Unasked { frame: 2, sent: 1 };
                let ended = Err(Error::Reply(unasked));
                assert_eq!(writer.stored().await, ended);
                assert_eq!(redraft(&mut writer).await, ended);
                node.clock().sleep(QUIET).await;
            })
        },
    );
}

/// A `Stored` past the outcomes given ends the writer with the error of `Stored`, so
/// `stored` never gives for a frame that no commit covers.
#[test]
fn ends_the_writer_at_a_stored_past_the_outcomes() {
    by_hand(
        741,
        |session, node| async move {
            let mut write = opened(&session).await;
            reply(&mut write, Reply::Stored { frames: 9 }).await;
            // The reset can come before the frame does.
            let mut messages = 0;
            let code = loop {
                match write.receiver.recv().await {
                    Ok(Some(_)) => {
                        messages += 1;
                        if messages == 3 {
                            answer(&mut write, 1, taken(0, 1)).await;
                        }
                    }
                    Err(transport::Error::Reset { code }) => break code,
                    got => panic!("{:?}", got.map(|got| got.is_some())),
                }
            };
            assert_eq!(code, Code(MALFORMED));
            node.clock().sleep(QUIET).await;
        },
        |session, node| {
            Box::pin(async move {
                let mut writer =
                    session.writer(config(&["value"], 1)).await.expect("opens");
                frame(&mut writer, 1, 0, &[(2, 1)]).await;
                let error = wire::hub::Error::Stored {
                    frames: 9,
                    bound: 0,
                };
                let ended = Err(Error::Reply(error));
                assert_eq!(writer.write(Time::Node).await, ended);
                assert_eq!(writer.stored().await, ended.map(drop));
                node.clock().sleep(QUIET).await;
            })
        },
    );
}

/// `finish` ends control: a second writer that got `Waiting` takes control after it.
#[test]
fn ends_control_at_the_finish() {
    on_hub(742, |session, _| async move {
        let mut first = session.writer(config(&["value"], 2)).await.expect("opens");
        let mut second = session.writer(config(&["value"], 1)).await.expect("opens");
        frame(&mut first, 1, 0, &[(2, 1)]).await;
        assert_eq!(first.write(Time::Node).await, Ok(taken(0, 1)));
        frame(&mut second, 1, 0, &[(2, 2)]).await;
        let outcome = second.write(Time::Node).await;
        assert_eq!(outcome, Ok(Outcome::Refused(Refused::Waiting)));
        assert_eq!(first.finish().await, Ok(()));
        frame(&mut second, 1, 0, &[(2, 3)]).await;
        assert_eq!(second.write(Time::Node).await, Ok(taken(1, 1)));
    });
}

/// `finish` drops the held draft, sends nothing more, and waits for the `Stored` of
/// the last taken frame and the finish of the node.
#[test]
fn waits_in_finish_for_the_stored_of_each_taken_frame() {
    by_hand(
        743,
        |session, node| async move {
            let mut write = opened(&session).await;
            take_frame(&mut write).await;
            answer(&mut write, 1, taken(0, 1)).await;
            take_frame(&mut write).await;
            answer(&mut write, 2, Outcome::Refused(Refused::Order)).await;
            let got = write
                .receiver
                .recv()
                .await
                .map(|got| got.map(|b| b.to_vec()));
            assert_eq!(got, Ok(None), "the held draft is not sent");
            node.clock().sleep(QUIET).await;
            reply(&mut write, Reply::Stored { frames: 1 }).await;
            node.clock().sleep(QUIET).await;
            let sender = write.sender.as_mut().expect("two-way");
            sender.finish().expect("finishes");
            node.clock().sleep(QUIET).await;
        },
        |session, node| {
            Box::pin(async move {
                let clock = node.clock();
                let mut writer =
                    session.writer(config(&["value"], 1)).await.expect("opens");
                frame(&mut writer, 1, 0, &[(2, 1)]).await;
                assert_eq!(writer.write(Time::Node).await, Ok(taken(0, 1)));
                frame(&mut writer, 1, 0, &[(2, 2)]).await;
                let outcome = writer.write(Time::Node).await;
                assert_eq!(outcome, Ok(Outcome::Refused(Refused::Order)));
                frame(&mut writer, 1, 0, &[(2, 3)]).await;
                let before = clock.now();
                assert_eq!(writer.finish().await, Ok(()));
                let quiet = Span::from_nanos(2 * QUIET.nanos());
                assert!(clock.now() >= before + quiet, "finish waits for the node");
                clock.sleep(QUIET).await;
            })
        },
    );
}

/// A node that finishes the stream before the `Stored` of each taken frame makes
/// `finish` give `Finished`.
#[test]
fn gives_finished_to_a_finish_with_a_taken_frame_not_stored() {
    by_hand(
        744,
        |session, node| async move {
            let mut write = opened(&session).await;
            take_frame(&mut write).await;
            answer(&mut write, 1, taken(0, 1)).await;
            let got = write.receiver.recv().await.map(|got| got.is_some());
            assert_eq!(got, Ok(false));
            let sender = write.sender.as_mut().expect("two-way");
            sender.finish().expect("finishes");
            node.clock().sleep(QUIET).await;
        },
        |session, node| {
            Box::pin(async move {
                let mut writer =
                    session.writer(config(&["value"], 1)).await.expect("opens");
                frame(&mut writer, 1, 0, &[(2, 1)]).await;
                assert_eq!(writer.write(Time::Node).await, Ok(taken(0, 1)));
                let got = writer.finish().await;
                assert_eq!(got, Err(Error::Reply(wire::hub::Error::Finished)));
                node.clock().sleep(QUIET).await;
            })
        },
    );
}

/// A draft whose series break a rule of the layout gives `Layout` with no block
/// taken, a draft of no series gives `Unsent` at its write, a draft over the largest
/// block gives `Pool`, and the writer goes on.
#[test]
fn gives_layout_for_a_draft_whose_series_are_not_valid() {
    on_hub(745, |session, _| async move {
        let mut writer = session.writer(config(&["value"], 1)).await.expect("opens");
        let got = writer
            .draft(Form::Raw, 1, &[(1, 8), (0, 8)])
            .await
            .map(drop);
        let unordered = types::frame::Error::Unordered { entry: 0, last: 1 };
        assert_eq!(got, Err(Error::Layout(unordered)));
        assert_eq!(
            got.unwrap_err().to_string(),
            "the series of the draft are not valid: entry 0 follows entry 1: entries \
             must increase"
        );
        let got = writer
            .draft(Form::Raw, 1, &[(0, 8), (2, 8)])
            .await
            .map(drop);
        let out = types::frame::Error::OutOfRange {
            entry: 2,
            entries: 2,
        };
        assert_eq!(got, Err(Error::Layout(out)));
        let largest = ample().largest();
        let got = writer.draft(Form::Raw, 1, &[(0, largest)]).await.map(drop);
        let Err(Error::Pool(block::Error::TooLarge {
            requested,
            largest: most,
        })) = got
        else {
            panic!("{got:?}");
        };
        assert_eq!(most, largest);
        assert!(requested > largest, "{requested}");
        writer.draft(Form::Raw, 1, &[]).await.expect("a frame");
        let got = writer.write(Time::Node).await;
        let unsent = Error::Unsent {
            count: 0,
            form: Form::Raw,
        };
        assert_eq!(got, Err(unsent));
        frame(&mut writer, 1, 0, &[(2, 1)]).await;
        assert_eq!(writer.write(Time::Node).await, Ok(taken(0, 1)));
    });
}

/// A draft waits while the pool of frames is full, and takes the block that another
/// writer's draft gives back.
#[test]
fn waits_in_draft_for_a_block_of_a_full_pool_of_frames() {
    raw(
        746,
        |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let _first = opened(&session).await;
            let mut second = opened(&session).await;
            take_frame(&mut second).await;
            answer(&mut second, 1, taken(0, 1)).await;
            node.clock().sleep(QUIET).await;
            node.clock().sleep(QUIET).await;
        },
        |node, tasks, at| async move {
            let clock = node.clock();
            let budget = block::Config::new(
                u64::try_from(block::footprint(1 << 12)).expect("a usize fits a u64"),
            )
            .expect("the budget fits");
            let frames = block::Pool::new(
                budget.clone(),
                block::Heap::new(budget.reservation()),
            );
            let session =
                connect_with(&node, tasks, at, AGENT, ample(), Rc::new(frames))
                    .await
                    .expect("connects");
            let mut first = session.writer(config(&["value"], 1)).await.expect("opens");
            let mut second =
                session.writer(config(&["value"], 1)).await.expect("opens");
            // A frame of 3840 bytes of series takes the one block of 4096.
            frame(&mut first, 240, 1_000, &[(2, 1)]).await;
            let waited = within(&clock, QUIET, async {
                frame(&mut second, 1, 0, &[(2, 1)]).await;
            });
            assert_eq!(waited.await, None, "the draft waits for a block");
            drop(first);
            let drafted = within(&clock, QUIET, async {
                frame(&mut second, 1, 0, &[(2, 1)]).await;
            });
            assert_eq!(drafted.await, Some(()), "the draft takes the block");
            assert_eq!(second.write(Time::Node).await, Ok(taken(0, 1)));
            clock.sleep(QUIET).await;
        },
    );
}

/// When an extra outcome of a frame comes, before the next write or while its frame is
/// sent, the write ends the writer with `Order`, and the extra outcome is never the
/// outcome of the frame.
#[test]
fn ends_the_writer_at_an_extra_outcome_before_or_during_the_next_write() {
    for (seed, during) in [(747, false), (750, true)] {
        by_hand(
            seed,
            move |session, node| async move {
                let mut write = opened(&session).await;
                take_frame(&mut write).await;
                answer(&mut write, 1, taken(0, 1)).await;
                if during {
                    let head = write.receiver.recv().await.expect("receives");
                    head.expect("the head of the frame");
                }
                answer(&mut write, 1, Outcome::Refused(Refused::Order)).await;
                assert_eq!(reset_in_frame(&mut write).await, Code(MALFORMED));
                node.clock().sleep(QUIET).await;
            },
            move |session, node| {
                Box::pin(async move {
                    let mut writer =
                        session.writer(config(&["value"], 1)).await.expect("opens");
                    frame(&mut writer, 1, 0, &[(2, 1)]).await;
                    assert_eq!(writer.write(Time::Node).await, Ok(taken(0, 1)));
                    if !during {
                        node.clock().sleep(QUIET).await;
                    }
                    frame(&mut writer, 1, 0, &[(2, 2)]).await;
                    let order = wire::hub::Error::Order { frame: 1, next: 2 };
                    let ended = Err(Error::Reply(order));
                    assert_eq!(writer.write(Time::Node).await, ended, "{during}");
                    node.clock().sleep(QUIET).await;
                })
            },
        );
    }
}

/// Takes the messages of a frame of `write` until the program resets the stream, and
/// gives the code of the reset. A reset can drop messages of the frame that the
/// program sent.
async fn reset_in_frame(write: &mut Incoming) -> Code {
    loop {
        match write.receiver.recv().await {
            Ok(Some(_)) => {}
            Err(transport::Error::Reset { code }) => return code,
            got => panic!("a reset, not {:?}", got.map(|got| got.map(|b| b.to_vec()))),
        }
    }
}

/// An extra outcome that the transport of the program holds back while its receive
/// pool has no block is read after the next frame is sent, and ends the writer with
/// `Order`.
#[test]
fn ends_the_writer_at_an_extra_outcome_held_back_by_a_full_pool() {
    raw(
        752,
        |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let mut write = opened(&session).await;
            take_frame(&mut write).await;
            answer(&mut write, 1, taken(0, 1)).await;
            node.clock().sleep(QUIET).await;
            answer(&mut write, 1, Outcome::Refused(Refused::Order)).await;
            assert_eq!(reset_in_frame(&mut write).await, Code(MALFORMED));
            node.clock().sleep(QUIET).await;
        },
        |node, tasks, at| async move {
            let clock = node.clock();
            let received = own_pool();
            let session = connect_receiving(
                &node,
                tasks.clone(),
                at,
                AGENT,
                ample(),
                ample(),
                Rc::clone(&received),
            )
            .await
            .expect("connects");
            let mut writer =
                session.writer(config(&["value"], 1)).await.expect("opens");
            frame(&mut writer, 1, 0, &[(2, 1)]).await;
            assert_eq!(writer.write(Time::Node).await, Ok(taken(0, 1)));
            let held = fill(&received);
            frame(&mut writer, 1, 0, &[(2, 2)]).await;
            clock.sleep(Span::from_nanos(QUIET.nanos() * 3 / 2)).await;
            let freed = clock.now() + QUIET;
            let free = clock.clone();
            tasks.spawn(async move {
                free.sleep(QUIET).await;
                drop(held);
            });
            let order = wire::hub::Error::Order { frame: 1, next: 2 };
            assert_eq!(writer.write(Time::Node).await, Err(Error::Reply(order)));
            assert!(clock.now() >= freed, "the outcome waits for the pool");
            clock.sleep(QUIET).await;
        },
    );
}

/// `finish` drops a held draft before it waits, so the draft of another writer takes
/// its block of a pool of frames of one block.
#[test]
fn frees_the_block_of_a_held_draft_while_finish_waits() {
    raw(
        748,
        |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let mut first = opened(&session).await;
            let mut second = opened(&session).await;
            let got = first.receiver.recv().await.map(|got| got.is_some());
            assert_eq!(got, Ok(false));
            take_frame(&mut second).await;
            answer(&mut second, 1, taken(0, 1)).await;
            let sender = first.sender.as_mut().expect("two-way");
            sender.finish().expect("finishes");
            node.clock().sleep(QUIET).await;
        },
        |node, tasks, at| async move {
            let clock = node.clock();
            let budget = block::Config::new(
                u64::try_from(block::footprint(1 << 12)).expect("a usize fits a u64"),
            )
            .expect("the budget fits");
            let frames = block::Pool::new(
                budget.clone(),
                block::Heap::new(budget.reservation()),
            );
            let session =
                connect_with(&node, tasks, at, AGENT, ample(), Rc::new(frames))
                    .await
                    .expect("connects");
            let mut first = session.writer(config(&["value"], 1)).await.expect("opens");
            let mut second =
                session.writer(config(&["value"], 1)).await.expect("opens");
            frame(&mut first, 240, 1_000, &[(2, 1)]).await;
            let mut finish = pin!(first.finish());
            let waits =
                poll_fn(|cx| Poll::Ready(finish.as_mut().poll(cx).is_pending()));
            assert!(waits.await, "the finish waits for the node");
            let drafted = within(&clock, QUIET, async {
                frame(&mut second, 1, 0, &[(2, 1)]).await;
            });
            assert_eq!(drafted.await, Some(()), "the draft takes the block");
            assert_eq!(second.write(Time::Node).await, Ok(taken(0, 1)));
            assert_eq!(finish.await, Ok(()));
            clock.sleep(QUIET).await;
        },
    );
}

/// A reply that `wire` refuses while `finish` waits gives its error, not `Finished`.
#[test]
fn ends_the_finish_at_a_reply_that_wire_refuses() {
    by_hand(
        749,
        |session, node| async move {
            let mut write = opened(&session).await;
            take_frame(&mut write).await;
            answer(&mut write, 1, taken(0, 1)).await;
            let got = write.receiver.recv().await.map(|got| got.is_some());
            assert_eq!(got, Ok(false));
            reply(&mut write, Reply::Stored { frames: 9 }).await;
            let sender = write.sender.as_mut().expect("two-way");
            sender.finish().expect("finishes");
            node.clock().sleep(QUIET).await;
        },
        |session, node| {
            Box::pin(async move {
                let mut writer =
                    session.writer(config(&["value"], 1)).await.expect("opens");
                frame(&mut writer, 1, 0, &[(2, 1)]).await;
                assert_eq!(writer.write(Time::Node).await, Ok(taken(0, 1)));
                let error = wire::hub::Error::Stored {
                    frames: 9,
                    bound: 1,
                };
                assert_eq!(writer.finish().await, Err(Error::Reply(error)));
                node.clock().sleep(QUIET).await;
            })
        },
    );
}
