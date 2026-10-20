//! The write streams of a client session that `Link::serve` serves, over a real
//! transport from a program to a simulated node.

use std::cell::Cell;
use std::path::Path as FilePath;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use access::proof::{Error as Refusal, Kind};
use block::Unique;
use env::files::Operation;
use hub::{serve, writer};
use spec::data_type::DataType;
use transport::Class;
use types::authority::Authority;
use types::channel;
use types::frame::{Form, Path};
use types::time::Span;
use wire::header::MALFORMED;
use wire::hub::client::write::Open;
use wire::hub::client::{BODY_BYTES_MAX, Mode, Place, Places, REFUSED};
use wire::hub::write::{End, Head, Outcome, Reply, Time};
use wire::hub::{BUSY, NOT_HOME, UNKNOWN};

use super::link::{as_agent, serve_session_after};
use super::read::{finish, read_session};
use super::serve::decoded;
use super::{
    AREA, BODY_MAX, I64, NODE, POOL, RING, STAMP, TIME, TIME_B, Test, channels, region,
    without,
};
use crate::agent::{AGENT, Agent, SUBJECT, Stream, name, reset_with};
use crate::net::{HOME, own_pool, transport};
use crate::sessions::{Got, Kept, OTHER, recorded, rules, run_program, serve_each};

/// When the home of a test changes what the write meets, after the program opened it.
const START: Span = Span::from_nanos(100_000_000);

/// The place of `channel`, whose group's index is at place `index`.
fn place(
    key: u128,
    data_type: types::sample::Type,
    channel: &str,
    index: usize,
) -> Place {
    Place {
        key: channel::Key::from_u128(key),
        data_type,
        name: name(channel),
        index,
    }
}

/// The head of a frame of `series` series on the live path.
fn live(series: u32) -> Head {
    Head {
        time: Time::Frame(Path::Live),
        form: Form::Raw,
        series,
    }
}

/// The head of a frame of `series` series that the node stamps.
fn untimed(series: u32) -> Head {
    Head {
        time: Time::Node,
        form: Form::Raw,
        series,
    }
}

/// The ends and the body of a frame of `series`, each a place and its samples.
fn frame(series: &[(u32, &[i64])]) -> (Vec<(u32, u32)>, Vec<u8>) {
    let mut body = Vec::new();
    let ends = series
        .iter()
        .map(|&(place, samples)| {
            body.extend(samples.iter().flat_map(|sample| sample.to_le_bytes()));
            (place, u32::try_from(body.len()).expect("a body fits a u32"))
        })
        .collect();
    (ends, body)
}

/// Sends a live frame of `stamps` at place 0 and `values` at place 1.
async fn send(stream: &mut Stream, stamps: &[i64], values: &[i64]) {
    let count = u32::try_from(stamps.len()).expect("a count");
    let (ends, body) = frame(&[(0, stamps), (1, values)]);
    stream.frame(live(2), &[count], &ends, &body).await;
}

/// Sends a frame of `value` at place 1 that the node stamps.
async fn send_untimed(stream: &mut Stream, value: i64) {
    let (ends, body) = frame(&[(0, &[0]), (1, &[value])]);
    stream.frame(untimed(2), &[1], &ends, &body).await;
}

/// Finishes `stream`, and gives each reply that the node sent before it finished.
async fn end(stream: &mut Stream) -> Vec<Reply> {
    stream.sender.finish().expect("finishes");
    let mut replies = Vec::new();
    while let Some(message) = stream.recv().await.expect("receives") {
        replies.push(Reply::decode(&message).expect("a reply"));
    }
    replies
}

fn stored(frames: u64) -> Reply {
    Reply::Stored { group: 0, frames }
}

fn taken(frame: u64, seq: u64, count: u32) -> Reply {
    Reply::Outcome {
        frame,
        group: 0,
        outcome: Outcome::Taken {
            range: types::frame::Range { seq, count },
        },
    }
}

fn refused(frame: u64, refusal: wire::hub::write::Refusal) -> Reply {
    Reply::Outcome {
        frame,
        group: 0,
        outcome: Outcome::Refused(refusal),
    }
}

/// Opens a write of `value`, and checks its places: `time` at place 0 and `value` at
/// place 1.
async fn write(agent: &Agent) -> Stream {
    let mut stream = agent.write(&["value"]).await;
    let start = stream.started().await;
    assert_eq!(
        start.places,
        [place(1, STAMP, "time", 0), place(2, I64, "value", 0)]
    );
    stream
}

/// Opens a complete read of `value`, whose places are `value`, then `time`.
async fn read(agent: &Agent) -> (Stream, wire::hub::Reader) {
    read_of(agent, "value").await
}

/// Opens a complete read of `channel`, an `I64` channel, whose places are `channel`,
/// then its index.
async fn read_of(agent: &Agent, channel: &str) -> (Stream, wire::hub::Reader) {
    let mut stream = agent.read(Mode::Complete, &[channel]).await;
    let (places, run, reader) = stream.opened().await;
    assert_eq!(places, Places { count: 2 });
    let indexes: Vec<_> = run.iter().map(|place| place.index).collect();
    assert_eq!(indexes, [1, 1]);
    (stream, reader)
}

/// The values and the stamps of the next frame of a read of `value`.
async fn next(stream: &mut Stream, reader: &mut wire::hub::Reader) -> (i64, i64) {
    let got = super::serve::got(&mut stream.receiver, reader).await;
    let series = decoded(&got, &[I64, STAMP]);
    (series[0][0], series[1][0])
}

/// Each frame of a write is taken in order, a read at the home gets it, and `Stored`
/// comes after its commit with the count of frames.
#[test]
fn takes_each_frame_of_a_write_in_order_and_stores_it() {
    let home = read_session(
        400,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let (mut reading, mut reader) = read(&agent).await;
            let mut stream = write(&agent).await;
            for (seq, (stamp, value)) in (0..).zip([(1, 10), (2, 20)]) {
                send(&mut stream, &[stamp], &[value]).await;
                assert_eq!(stream.reply().await, taken(seq + 1, seq, 1));
                assert_eq!(stream.reply().await, stored(seq + 1));
                assert_eq!(next(&mut reading, &mut reader).await, (value, stamp));
            }
            stream.sender.finish().expect("finishes");
            assert_eq!(stream.recv().await, Ok(None));
            reading.sender.finish().expect("finishes");
            assert_eq!(reading.recv().await, Ok(None));
            finish(&mut agent).await;
        },
    );
    assert_eq!(
        home.served,
        [Ok(Got::Ended), Ok(Got::Ended), Ok(Got::Ended)]
    );
}

/// Each path has its own seqs, and `Stored` counts the frames of both paths.
#[test]
fn counts_the_frames_of_each_path_in_stored() {
    let home = read_session(
        440,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut stream = write(&agent).await;
            send(&mut stream, &[100], &[1]).await;
            let stamps: Vec<i64> = (1..=32).collect();
            let (ends, body) = frame(&[(0, &stamps), (1, &stamps)]);
            let backfill = Head {
                time: Time::Frame(Path::Backfill),
                ..live(2)
            };
            stream.frame(backfill, &[32], &ends, &body).await;
            send(&mut stream, &[101], &[3]).await;
            let replies = [taken(1, 0, 1), taken(2, 0, 32), taken(3, 1, 1)];
            let stored = [stored(1), stored(3)];
            assert_eq!(end(&mut stream).await, [&replies[..], &stored].concat());
            finish(&mut agent).await;
        },
    );
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A program that finishes its stream right after a frame gets the frame's `Stored`
/// before the finish.
#[test]
fn stores_each_taken_frame_before_it_finishes_the_stream() {
    let home = read_session(
        401,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut stream = write(&agent).await;
            send(&mut stream, &[1, 2, 3], &[10, 20, 30]).await;
            stream.sender.finish().expect("finishes");
            assert_eq!(stream.reply().await, taken(1, 0, 3));
            assert_eq!(stream.reply().await, stored(1));
            assert_eq!(stream.recv().await, Ok(None));
            finish(&mut agent).await;
        },
    );
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A write is served on a stream of each class, as the class sets only the priority
/// of its replies.
#[test]
fn serves_a_write_on_a_stream_of_each_class() {
    let home = read_session(
        416,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let classes = [Class::Command, Class::Latest, Class::CatchUp];
            for (seq, class) in (0..).zip(classes) {
                let mut stream = agent.write_as(class, &["value"]).await;
                stream.started().await;
                let stamp = i64::try_from(seq).expect("a stamp") + 1;
                send(&mut stream, &[stamp], &[10]).await;
                assert_eq!(stream.reply().await, taken(1, seq, 1), "{class:?}");
                assert_eq!(end(&mut stream).await, [stored(1)], "{class:?}");
            }
            finish(&mut agent).await;
        },
    );
    assert_eq!(
        home.served,
        [
            Ok(Got::Ended),
            Ok(Got::Ended),
            Ok(Got::Ended),
            Ok(Got::Ended)
        ]
    );
}

/// The node stamps a frame of `Time::Node` with mesh time, and a frame at the same
/// mesh time 1 ns after the stamp before it. A frame after mesh time moves on gets
/// mesh time again.
#[test]
fn stamps_an_untimed_frame_with_mesh_time_and_after_the_stamp_before_it() {
    let home = read_session(
        402,
        async |_| {},
        |mut agent| async move {
            let challenge = agent.admit().await;
            let (mut reading, mut reader) = read(&agent).await;
            let mut stream = write(&agent).await;
            send_untimed(&mut stream, 10).await;
            send_untimed(&mut stream, 20).await;
            assert_eq!(stream.reply().await, taken(1, 0, 1));
            assert_eq!(stream.reply().await, taken(2, 1, 1));
            let (value, first) = next(&mut reading, &mut reader).await;
            assert_eq!(value, 10);
            assert!(
                first >= challenge.now.earliest.nanos()
                    && first < challenge.now.latest.nanos() + Span::SECOND.nanos(),
                "the stamp {first} is not mesh time after {:?}",
                challenge.now
            );
            assert_eq!(next(&mut reading, &mut reader).await, (20, first + 1));
            agent.sleep(Span::MILLISECOND).await;
            send_untimed(&mut stream, 30).await;
            assert_eq!(stream.reply().await, stored(1));
            assert_eq!(stream.reply().await, stored(2));
            assert_eq!(stream.reply().await, taken(3, 2, 1));
            let (value, third) = next(&mut reading, &mut reader).await;
            assert_eq!(value, 30);
            assert!(
                third >= first + Span::MILLISECOND.nanos(),
                "the stamp {third} is not mesh time 1 ms after {first}"
            );
            assert_eq!(end(&mut stream).await, [stored(3)]);
            reading.sender.finish().expect("finishes");
            assert_eq!(reading.recv().await, Ok(None));
            finish(&mut agent).await;
        },
    );
    assert_eq!(
        home.served,
        [Ok(Got::Ended), Ok(Got::Ended), Ok(Got::Ended)]
    );
}

/// A frame of `Time::Node` behind a later stamp of another writer, and a timed frame
/// before the last stamp of the node, are refused, and the stream takes the next
/// frame.
#[test]
fn refuses_a_frame_out_of_order_and_takes_the_next() {
    let ready = async |test: &Test| {
        let mut writer = test.writer("a", &["value"]).await;
        let later = test.now() + Span::from_nanos(500_000_000).nanos();
        super::write(&mut writer, &[later], &[1]);
    };
    let home = read_session(403, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        send_untimed(&mut stream, 10).await;
        assert_eq!(
            stream.reply().await,
            refused(1, wire::hub::write::Refusal::Order)
        );
        agent.sleep(Span::from_nanos(600_000_000)).await;
        send_untimed(&mut stream, 20).await;
        assert_eq!(stream.reply().await, taken(2, 1, 1));
        send(&mut stream, &[5], &[30]).await;
        assert_eq!(
            stream.reply().await,
            refused(3, wire::hub::write::Refusal::Order)
        );
        send_untimed(&mut stream, 40).await;
        assert_eq!(stream.reply().await, taken(4, 2, 1));
        assert_eq!(end(&mut stream).await, [stored(2), stored(4)]);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A frame of `Time::Node` behind a timed frame of its own stream is refused as
/// `Order`: the node stamps after the last stamp that it gave, not after the stamps of
/// timed frames.
#[test]
fn refuses_an_untimed_frame_behind_a_timed_frame_of_its_stream() {
    let home = read_session(
        436,
        async |_| {},
        |mut agent| async move {
            let challenge = agent.admit().await;
            let mut stream = write(&agent).await;
            let later = challenge.now.latest.nanos() + START.nanos();
            send(&mut stream, &[later], &[10]).await;
            assert_eq!(stream.reply().await, taken(1, 0, 1));
            send_untimed(&mut stream, 20).await;
            let order = Outcome::Refused(wire::hub::write::Refusal::Order);
            assert_eq!(outcome(&mut stream).await, order);
            end(&mut stream).await;
            finish(&mut agent).await;
        },
    );
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A frame of `Time::Node` that the home loses for no room in the ring spends its
/// stamp, so the next one at the same mesh time gets 1 ns after it and is not refused
/// as `Order`.
#[test]
fn stamps_an_untimed_frame_after_a_lost_untimed_frame() {
    let home = read_session(
        420,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut stream = write(&agent).await;
            for n in 1.. {
                let stamps: Vec<i64> = (n * 1000..(n + 1) * 1000).collect();
                send(&mut stream, &stamps, &noise(&stamps)).await;
                match outcome(&mut stream).await {
                    Outcome::Taken { .. } => {}
                    Outcome::Lost { .. } => break,
                    other @ (Outcome::Refused(_) | Outcome::Ended(_)) => {
                        panic!("frame {n}: {other:?}")
                    }
                }
            }
            send_untimed(&mut stream, 10).await;
            send_untimed(&mut stream, 20).await;
            let got = [outcome(&mut stream).await, outcome(&mut stream).await];
            let [
                Outcome::Lost { range: first },
                Outcome::Lost { range: second },
            ] = got
            else {
                panic!("{got:?}");
            };
            assert_eq!(second.seq, first.seq + 1);
            end(&mut stream).await;
            finish(&mut agent).await;
        },
    );
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A frame of `Time::Node` that the home refuses spends no stamp: the next one at the
/// same mesh time gets 1 ns after the stamp of the frame before the refused one.
#[test]
fn spends_no_stamp_on_a_refused_untimed_frame() {
    let home = read_session(
        434,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let (mut reading, mut reader) = read(&agent).await;
            let mut stream = write(&agent).await;
            send_untimed(&mut stream, 10).await;
            let (ends, body) = frame(&[(0, &[0]), (1, &[20, 20])]);
            stream.frame(untimed(2), &[1], &ends, &body).await;
            send_untimed(&mut stream, 30).await;
            assert_eq!(stream.reply().await, taken(1, 0, 1));
            assert_eq!(
                stream.reply().await,
                refused(2, wire::hub::write::Refusal::Codec)
            );
            assert_eq!(stream.reply().await, taken(3, 1, 1));
            let (value, first) = next(&mut reading, &mut reader).await;
            assert_eq!(value, 10);
            assert_eq!(next(&mut reading, &mut reader).await, (30, first + 1));
            end(&mut stream).await;
            reading.sender.finish().expect("finishes");
            assert_eq!(reading.recv().await, Ok(None));
            finish(&mut agent).await;
        },
    );
    assert_eq!(
        home.served,
        [Ok(Got::Ended), Ok(Got::Ended), Ok(Got::Ended)]
    );
}

/// A frame whose series do not hold its count is refused as `Codec`: one of
/// `Time::Node` whose index is not one stamp, which the node does not stamp, and a
/// timed one. The stream takes the next frame.
#[test]
fn refuses_a_frame_whose_series_do_not_hold_its_count_as_codec() {
    let home = read_session(
        421,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut stream = write(&agent).await;
            let codec = |frame| refused(frame, wire::hub::write::Refusal::Codec);
            for (n, stamps) in (1..).zip([&[][..], &[0, 0]]) {
                let (ends, body) = frame(&[(0, stamps), (1, &[10])]);
                stream.frame(untimed(2), &[1], &ends, &body).await;
                assert_eq!(stream.reply().await, codec(n), "{stamps:?}");
            }
            let (ends, body) = frame(&[(0, &[1, 2]), (1, &[10])]);
            stream.frame(live(2), &[1], &ends, &body).await;
            assert_eq!(stream.reply().await, codec(3));
            send_untimed(&mut stream, 20).await;
            assert_eq!(stream.reply().await, taken(4, 0, 1));
            assert_eq!(end(&mut stream).await, [stored(4)]);
            finish(&mut agent).await;
        },
    );
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A write whose index another writer controls is refused at each frame as `Waiting`,
/// also a frame of `Time::Node` whose index is not one stamp.
#[test]
fn refuses_a_frame_while_another_writer_holds_control() {
    let ready = async |test: &Test| {
        let writer = test.writer("a", &["value"]).await;
        let clock = test.clock.clone();
        test.tasks.spawn(async move {
            clock
                .sleep(Span::from_nanos(10 * Span::SECOND.nanos()))
                .await;
            drop(writer);
        });
    };
    let home = read_session(404, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        send(&mut stream, &[1], &[10]).await;
        let waiting = |frame| refused(frame, wire::hub::write::Refusal::Waiting);
        assert_eq!(stream.reply().await, waiting(1));
        let (ends, body) = frame(&[(0, &[]), (1, &[10])]);
        stream.frame(untimed(2), &[1], &ends, &body).await;
        assert_eq!(stream.reply().await, waiting(2));
        assert_eq!(end(&mut stream).await, []);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A write whose index a holder restored after a power cut controls is refused as
/// `Reserved`.
#[test]
fn refuses_a_frame_while_a_restored_holder_holds_control_as_reserved() {
    let before = |sim: &mut sim::Sim, node: &sim::node::Node| {
        let span = Span::from_nanos(2 * Span::SECOND.nanos());
        super::cut_after(sim, node, span, |test| async move {
            let mut writer = test.writer("a", &["value"]).await;
            super::write(&mut writer, &[test.now()], &[1]);
            std::future::pending::<()>().await;
        });
    };
    let home = serve_session_after(494, before, |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        send(&mut stream, &[1], &[10]).await;
        let reserved = refused(1, wire::hub::write::Refusal::Reserved);
        assert_eq!(stream.reply().await, reserved);
        assert_eq!(end(&mut stream).await, []);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A frame after the control lease of its write ran out is refused as `Expired`.
#[test]
fn refuses_a_frame_after_the_lease_of_its_write_ran_out_as_expired() {
    let home = read_session(
        441,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let open = Open {
                authority: Authority(1),
                lease: Some(START),
                channels: vec![name("value")],
            };
            let mut stream = agent.open().await;
            stream.write(&open, Kind::Write, &AGENT).await;
            stream.started().await;
            send(&mut stream, &[1], &[10]).await;
            assert_eq!(stream.reply().await, taken(1, 0, 1));
            assert_eq!(stream.reply().await, stored(1));
            agent.sleep(Span::from_nanos(START.nanos() * 2)).await;
            send(&mut stream, &[2], &[20]).await;
            assert_eq!(
                stream.reply().await,
                refused(2, wire::hub::write::Refusal::Expired)
            );
            assert_eq!(end(&mut stream).await, []);
            finish(&mut agent).await;
        },
    );
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// Every block of `pool` from the largest down to blocks of `least` bytes, but the last
/// one, so the pool has room for small blocks only.
fn hold(pool: &block::Pool, least: usize) -> Vec<Unique> {
    let mut blocks = Vec::new();
    let mut len = pool.largest();
    while len >= least {
        while let Ok(block) = pool.alloc(len) {
            blocks.push(block);
        }
        len -= len.div_ceil(16);
    }
    blocks.pop();
    blocks
}

/// A frame whose draft finds no block is refused as `Full`, its body drops, and the
/// stream takes the next frame once the pool has room.
#[test]
fn refuses_a_frame_that_finds_no_block_and_takes_it_after_a_free() {
    let ready = async |test: &Test| {
        let blocks = hold(&test.pool, 1 << 14);
        let clock = test.clock.clone();
        test.tasks.spawn(async move {
            clock.sleep(START).await;
            drop(blocks);
        });
    };
    let stamps: Vec<i64> = (1..=3000).collect();
    let home = read_session(405, ready, move |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        send(&mut stream, &stamps, &stamps).await;
        assert_eq!(
            stream.reply().await,
            refused(1, wire::hub::write::Refusal::Full)
        );
        agent.sleep(START).await;
        send(&mut stream, &stamps, &stamps).await;
        assert_eq!(stream.reply().await, taken(2, 0, 3000));
        assert_eq!(end(&mut stream).await, [stored(2)]);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// Fills the pool of `test` at [`START`], and frees it [`START`] later.
async fn fill_for_a_while(test: &Test) {
    let (clock, pool) = (test.clock.clone(), Rc::clone(&test.pool));
    test.tasks.spawn(async move {
        clock.sleep(START).await;
        let blocks = super::fill(&pool);
        clock.sleep(START).await;
        drop(blocks);
    });
}

/// Fills the pool of `test` when a complete reader gets the first frame of `value`, at
/// its commit, and frees it [`START`] later. The run picks the order of the tasks that
/// the commit wakes: in the runs of the tests, the reader runs before the write.
async fn fill_at_commit(test: &Test) {
    let names = ["value"];
    let config = super::unnamed(&names, hub::reader::Mode::Complete);
    let mut reader = test.hub.reader(config).await.expect("opens");
    let (clock, pool) = (test.clock.clone(), Rc::clone(&test.pool));
    test.tasks.spawn(async move {
        let frame = reader.next().await.expect("a frame");
        let blocks = super::fill(&pool);
        clock.sleep(START).await;
        drop((frame, blocks));
    });
}

/// A frame that finds no block for its outcome stops the stream as `BUSY` before the
/// home takes it: the next write takes its first frame at seq 0.
#[test]
fn stops_a_write_whose_frame_finds_no_block_for_its_outcome_as_busy() {
    let home = read_session(424, fill_for_a_while, |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        agent.sleep(Span::from_nanos(START.nanos() * 3 / 2)).await;
        send(&mut stream, &[1], &[10]).await;
        assert_eq!(stream.recv().await, reset_with(BUSY));
        agent.sleep(START).await;
        let mut stream = write(&agent).await;
        send(&mut stream, &[2], &[20]).await;
        assert_eq!(stream.reply().await, taken(1, 0, 1));
        assert_eq!(end(&mut stream).await, [stored(1)]);
        finish(&mut agent).await;
    });
    let busy = serve::Error::Pool(block::Error::Exhausted {
        requested: Outcome::LEN_MAX,
        available: 64,
    });
    assert_eq!(home.served, [Err(busy), Ok(Got::Ended), Ok(Got::Ended)]);
}

/// Fills `pool` but for one free block of each of `sizes`, and gives the blocks that
/// fill it. The pool then has no pages to give back: each of those sizes has a block in
/// use, and a free block of under two pages gives back none.
fn spare(pool: &block::Pool, sizes: &[usize]) -> Vec<Unique> {
    let (mut held, mut spared) = (Vec::new(), Vec::new());
    for &len in sizes {
        held.push(pool.alloc(len).expect("a block"));
        spared.push(pool.alloc(len).expect("a block"));
    }
    held.extend(super::fill(pool));
    held
}

/// Fills the pool of a test at [`START`] as [`spare`] does, and frees it [`START`]
/// later.
fn spare_for_a_while(sizes: Vec<usize>) -> impl AsyncFnOnce(&Test) + Send + 'static {
    async move |test: &Test| {
        let (clock, pool) = (test.clock.clone(), Rc::clone(&test.pool));
        test.tasks.spawn(async move {
            clock.sleep(START).await;
            let held = spare(&pool, &sizes);
            clock.sleep(START).await;
            drop(held);
        });
    }
}

/// Fills the pool of a test as [`spare`] does when a latest reader gets the first frame
/// of `value`, before its commit, and frees it [`START`] later.
fn spare_at_take(sizes: Vec<usize>) -> impl AsyncFnOnce(&Test) + Send + 'static {
    async move |test: &Test| {
        let names = ["value"];
        let config = super::unnamed(&names, hub::reader::Mode::Latest);
        let mut reader = test.hub.reader(config).await.expect("opens");
        let (clock, pool) = (test.clock.clone(), Rc::clone(&test.pool));
        test.tasks.spawn(async move {
            let frame = reader.next().await.expect("a frame");
            let held = spare(&pool, &sizes);
            clock.sleep(START).await;
            drop((frame, held));
        });
    }
}

/// A frame whose draft finds a block but whose outcome finds none stops the stream
/// as `BUSY` before the home takes it: the next write takes its first frame at seq 0.
#[test]
fn stops_a_write_whose_outcome_finds_no_block_after_its_draft_as_busy() {
    let stamps: Vec<i64> = (1..=32).collect();
    // One block of each size but 64 bytes, the smallest.
    let sizes =
        (65..=2048).filter(|&len| block::footprint(len) != block::footprint(len - 1));
    let ready = spare_for_a_while(sizes.collect());
    let home = read_session(438, ready, move |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        agent.sleep(Span::from_nanos(START.nanos() * 3 / 2)).await;
        send(&mut stream, &stamps, &stamps).await;
        assert_eq!(stream.recv().await, reset_with(BUSY));
        agent.sleep(START).await;
        let mut stream = write(&agent).await;
        send(&mut stream, &[100], &[20]).await;
        assert_eq!(stream.reply().await, taken(1, 0, 1));
        assert_eq!(end(&mut stream).await, [stored(1)]);
        finish(&mut agent).await;
    });
    let [
        Err(serve::Error::Pool(block::Error::Exhausted {
            requested: Outcome::LEN_MAX,
            ..
        })),
        ..,
    ] = home.served[..]
    else {
        panic!("{:?}", home.served);
    };
    assert_eq!(home.served[1..], [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A frame whose `Stored` finds no block is refused as `Full`, and the stream goes on:
/// the `Stored` that is due comes at its commit, and the stream takes the next frame
/// once the pool has room, whose `Stored` counts the refused frame.
#[test]
fn refuses_a_frame_whose_stored_finds_no_block_as_full() {
    let stamps: Vec<i64> = (2..=33).collect();
    // The blocks of the outcome and of the draft of 32 samples, 560 bytes.
    let ready = spare_at_take(vec![Outcome::LEN_MAX, 560]);
    let home = read_session(442, ready, move |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        send(&mut stream, &[1], &[10]).await;
        assert_eq!(stream.reply().await, taken(1, 0, 1));
        send(&mut stream, &stamps, &stamps).await;
        let full = refused(2, wire::hub::write::Refusal::Full);
        assert_eq!(stream.reply().await, full);
        assert_eq!(stream.reply().await, stored(1));
        agent.sleep(START).await;
        send(&mut stream, &[100], &[100]).await;
        assert_eq!(stream.reply().await, taken(3, 1, 1));
        assert_eq!(end(&mut stream).await, [stored(3)]);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A stream that holds the block of its next `Stored` takes no other: a frame after the
/// pool fills reaches the home, which loses it for no room.
#[test]
fn takes_no_second_block_for_the_next_stored() {
    let stamps: Vec<i64> = (3..=34).collect();
    // The blocks of the outcome and of the draft of 32 samples, 560 bytes.
    let ready = spare_at_take(vec![Outcome::LEN_MAX, 560]);
    let home = read_session(445, ready, move |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        send(&mut stream, &[1], &[10]).await;
        send(&mut stream, &[2], &[20]).await;
        assert_eq!(stream.reply().await, taken(1, 0, 1));
        assert_eq!(stream.reply().await, taken(2, 1, 1));
        send(&mut stream, &stamps, &stamps).await;
        let lost = Reply::Outcome {
            frame: 3,
            group: 0,
            outcome: Outcome::Lost {
                range: types::frame::Range { seq: 2, count: 32 },
            },
        };
        assert_eq!(end(&mut stream).await, [lost, stored(1), stored(2)]);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A refused frame leaves the stream no block when no taken frame waits for a `Stored`:
/// a frame after the pool fills finds no block for its `Stored`, and is refused as
/// `Full`.
#[test]
fn holds_no_block_for_a_stored_after_a_refused_frame() {
    let stamps: Vec<i64> = (1..=32).collect();
    // The blocks of the outcome and of the draft of 32 samples, 560 bytes.
    let ready = spare_for_a_while(vec![Outcome::LEN_MAX, 560]);
    let home = read_session(444, ready, move |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        let (ends, body) = frame(&[(0, &[]), (1, &[10])]);
        stream.frame(untimed(2), &[1], &ends, &body).await;
        let codec = refused(1, wire::hub::write::Refusal::Codec);
        assert_eq!(stream.reply().await, codec);
        agent.sleep(Span::from_nanos(START.nanos() * 3 / 2)).await;
        send(&mut stream, &stamps, &stamps).await;
        let full = refused(2, wire::hub::write::Refusal::Full);
        assert_eq!(stream.reply().await, full);
        agent.sleep(START).await;
        send(&mut stream, &[100], &[100]).await;
        assert_eq!(stream.reply().await, taken(3, 0, 1));
        assert_eq!(end(&mut stream).await, [stored(3)]);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A backfill frame whose entry finds no block at the home is refused as `Full`, and
/// the stream takes it once the pool has room.
#[test]
fn refuses_a_backfill_frame_that_the_home_finds_no_block_for_as_full() {
    let stamps: Vec<i64> = (1..=32).collect();
    // The blocks of the outcome, of the `Stored`, and of the draft of 32 samples, 560
    // bytes.
    let len = stored(0).encoded_len();
    let ready = spare_for_a_while(vec![Outcome::LEN_MAX, len, 560]);
    let home = read_session(439, ready, move |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        send(&mut stream, &[100], &[100]).await;
        assert_eq!(stream.reply().await, taken(1, 0, 1));
        assert_eq!(stream.reply().await, stored(1));
        agent.sleep(Span::from_nanos(START.nanos() * 3 / 2)).await;
        let (ends, body) = frame(&[(0, &stamps), (1, &stamps)]);
        let backfill = Head {
            time: Time::Frame(Path::Backfill),
            ..live(2)
        };
        stream.frame(backfill, &[32], &ends, &body).await;
        assert_eq!(
            stream.reply().await,
            refused(2, wire::hub::write::Refusal::Full)
        );
        agent.sleep(START).await;
        stream.frame(backfill, &[32], &ends, &body).await;
        assert_eq!(stream.reply().await, taken(3, 0, 32));
        assert_eq!(end(&mut stream).await, [stored(3)]);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A `Stored` comes when its commit ends, while the pool is full, with no next frame
/// and no finish, so a program that waits for it ends its wait. The next `Stored`
/// covers the next frame.
#[test]
fn sends_a_stored_at_its_commit_while_the_pool_is_full() {
    let home = read_session(433, fill_at_commit, |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        send(&mut stream, &[1], &[10]).await;
        assert_eq!(stream.reply().await, taken(1, 0, 1));
        let full = Span::from_nanos(START.nanos() / 2);
        let got = super::within(&agent.node.clock(), full, stream.reply()).await;
        assert_eq!(got, Some(stored(1)));
        agent.sleep(Span::from_nanos(START.nanos() * 3 / 2)).await;
        send(&mut stream, &[2], &[20]).await;
        assert_eq!(stream.reply().await, taken(2, 1, 1));
        assert_eq!(stream.reply().await, stored(2));
        assert_eq!(end(&mut stream).await, []);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A commit that ends while the body of a frame comes leaves that frame the block of
/// its `Stored`: the home takes each frame, and the last `Stored` covers them all.
#[test]
fn keeps_the_stored_block_of_a_frame_whose_body_comes_after_a_commit() {
    let home = read_session(
        443,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut stream = write(&agent).await;
            send(&mut stream, &[1], &[10]).await;
            send(&mut stream, &[2], &[20]).await;
            let mut head = [0; Head::LEN];
            live(2).encode(&mut head);
            stream.send(&head).await;
            let (ends, body) = frame(&[(0, &[3]), (1, &[30])]);
            let mut out = vec![0; ends.len() * wire::hub::ends::LEN];
            wire::hub::ends::encode(ends.iter().copied(), &mut out);
            stream.send(&out).await;
            stream.send(&1_u32.to_le_bytes()).await;
            agent.sleep(START).await;
            stream.send(&body).await;
            let replies = end(&mut stream).await;
            assert_eq!(
                replies,
                [
                    taken(1, 0, 1),
                    taken(2, 1, 1),
                    stored(1),
                    stored(2),
                    taken(3, 2, 1),
                    stored(3)
                ]
            );
            finish(&mut agent).await;
        },
    );
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// At the finish, the last `Stored` comes when its commit ends, while the pool is full.
#[test]
fn sends_the_last_stored_at_the_finish_while_the_pool_is_full() {
    let home = read_session(426, fill_at_commit, |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        send(&mut stream, &[1], &[10]).await;
        let sent = agent.node.clock().now();
        assert_eq!(end(&mut stream).await, [taken(1, 0, 1), stored(1)]);
        let waited = agent.node.clock().now() - sent;
        assert!(waited < Span::from_nanos(START.nanos() / 2), "{waited}");
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// Control ends when the program finishes its stream: a writer of lower authority then
/// gets control, and the home spends a seq on its frame, which it loses for no room in
/// the full pool.
#[test]
fn ends_control_when_the_program_finishes_the_stream() {
    let ready = async |test: &Test| {
        let mut reader = test.reader(&["value"], hub::reader::Mode::Complete).await;
        let config = writer::Config {
            authority: Authority(0),
            ..super::config("b", &["value"])
        };
        let mut writer = test.hub.writer(config).await.expect("opens");
        let draft = super::draft(&writer, &[(1, &[2]), (2, &[20])]);
        let (clock, pool) = (test.clock.clone(), Rc::clone(&test.pool));
        test.tasks.spawn(async move {
            let frame = reader.next().await.expect("a frame");
            let blocks = super::fill(&pool);
            clock.sleep(Span::from_nanos(START.nanos() / 4)).await;
            let written = writer.write(super::LIVE, draft).expect("writes");
            assert!(
                matches!(written, [hub::home::Outcome::Lost { range, .. }] if range.seq == 1),
                "{written:?}"
            );
            clock.sleep(START).await;
            drop((frame, blocks));
        });
    };
    let home = read_session(426, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        send(&mut stream, &[1], &[10]).await;
        assert_eq!(end(&mut stream).await, [taken(1, 0, 1), stored(1)]);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// Values of `stamps` that do not compress, so a stored frame is as large as its body.
fn noise(stamps: &[i64]) -> Vec<i64> {
    stamps
        .iter()
        .map(|&stamp| {
            let x = stamp.wrapping_mul(6_364_136_223_846_793_005);
            x ^ (x >> 29)
        })
        .collect()
}

/// The next outcome of a write, past each `Stored`.
async fn outcome(stream: &mut Stream) -> Outcome {
    loop {
        if let Reply::Outcome { outcome, .. } = stream.reply().await {
            return outcome;
        }
    }
}

/// A frame over the body of one write is refused as `Large`, and the stream takes the
/// next frame.
#[test]
fn refuses_a_frame_too_large_for_one_write() {
    let stamps: Vec<i64> = (1..=10_000).collect();
    let values = noise(&stamps);
    let home = read_session(
        406,
        async |_| {},
        move |mut agent| async move {
            agent.admit().await;
            let mut stream = write(&agent).await;
            send(&mut stream, &stamps, &values).await;
            assert_eq!(
                stream.reply().await,
                refused(1, wire::hub::write::Refusal::Large)
            );
            send(&mut stream, &[1], &[10]).await;
            assert_eq!(stream.reply().await, taken(2, 0, 1));
            assert_eq!(end(&mut stream).await, [stored(2)]);
            finish(&mut agent).await;
        },
    );
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

#[test]
fn stops_a_write_whose_channels_are_unknown() {
    let home = read_session(
        407,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut unknown = agent.write(&["nope"]).await;
            assert_eq!(unknown.recv().await, reset_with(UNKNOWN));
            finish(&mut agent).await;
        },
    );
    let unknown = serve::Error::Writer(Box::new(writer::Error::Unknown(name("nope"))));
    assert_eq!(home.served, [Err(unknown.clone()), Ok(Got::Ended)]);
    assert_eq!(
        unknown.to_string(),
        "the writer of a write did not open: no channel is named nope"
    );
}

#[test]
fn refuses_a_write_signed_with_another_key_or_as_a_read() {
    let home = read_session(
        408,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let open = Open {
                authority: Authority(1),
                lease: None,
                channels: vec![name("value")],
            };
            for (kind, key) in [(Kind::Write, &OTHER), (Kind::Read, &AGENT)] {
                let mut stream = agent.open().await;
                stream.write(&open, kind, key).await;
                assert_eq!(stream.recv().await, reset_with(REFUSED));
            }
            finish(&mut agent).await;
        },
    );
    let signature = || Err(serve::Error::Access(Box::new(Refusal::Signature)));
    assert_eq!(home.served, [signature(), signature(), Ok(Got::Ended)]);
}

#[test]
fn stops_a_write_whose_lease_is_zero_as_malformed() {
    let home = read_session(
        409,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let open = Open {
                authority: Authority(1),
                lease: Some(Span::ZERO),
                channels: vec![name("value")],
            };
            let mut stream = agent.open().await;
            stream.write(&open, Kind::Write, &AGENT).await;
            assert_eq!(stream.recv().await, reset_with(MALFORMED));
            finish(&mut agent).await;
        },
    );
    let lease = home::writer::Error::Lease { span: Span::ZERO };
    let lease = serve::Error::Writer(Box::new(writer::Error::Home(lease)));
    assert_eq!(home.served, [Err(lease), Ok(Got::Ended)]);
}

/// A write holds 20 bytes for each place of its writers in the share of its subject
/// from the home of each index until it ends. The open of `value` is 8 bytes and its
/// names 21, so they fit where its ends do not.
#[test]
fn stops_a_write_whose_ends_pass_the_share_as_busy() {
    let home = read_session(
        410,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let held = agent.unfinished(BODY_BYTES_MAX - 60, &[]).await;
            agent.sleep(Span::MILLISECOND).await;
            let mut busy = agent.write(&["value"]).await;
            assert_eq!(busy.recv().await, reset_with(BUSY));
            drop(held);
            agent.sleep(Span::MILLISECOND).await;
            let mut stream = write(&agent).await;
            let held = agent.unfinished(BODY_BYTES_MAX - 40, &[]).await;
            agent.sleep(Span::MILLISECOND).await;
            send(&mut stream, &[1], &[10]).await;
            assert_eq!(stream.reply().await, taken(1, 0, 1));
            drop(held);
            finish(&mut agent).await;
        },
    );
    let share = serve::Error::Share {
        subject: name(SUBJECT),
        length: 40,
        held: BODY_BYTES_MAX - 39,
    };
    assert_eq!(home.served[0], Err(share.clone()));
    assert_eq!(
        share.to_string(),
        "40 bytes of a body or of the ends, counts, or names of a write do not fit \
         under the share of 16777216 bytes of subject ops.agent: its open requests, \
         reads, and writes hold 16777177 bytes"
    );
}

/// A write holds its decoded names in the share of its subject, 16 bytes and the
/// length of each, from the decode of its open until it sends its places. The names of
/// `value` are 21 bytes.
#[test]
fn stops_a_write_whose_names_pass_the_share_as_busy() {
    let home = read_session(
        417,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let held = agent.unfinished(BODY_BYTES_MAX - 20, &[]).await;
            agent.sleep(Span::MILLISECOND).await;
            let mut busy = agent.write(&["value"]).await;
            assert_eq!(busy.recv().await, reset_with(BUSY));
            drop(held);
            agent.sleep(Span::MILLISECOND).await;
            let held = agent.unfinished(BODY_BYTES_MAX - 61, &[]).await;
            agent.sleep(Span::MILLISECOND).await;
            let mut stream = write(&agent).await;
            send(&mut stream, &[1], &[10]).await;
            assert_eq!(stream.reply().await, taken(1, 0, 1));
            drop(held);
            finish(&mut agent).await;
        },
    );
    let share = serve::Error::Share {
        subject: name(SUBJECT),
        length: 21,
        held: BODY_BYTES_MAX - 20,
    };
    assert_eq!(home.served[0], Err(share));
}

#[test]
fn ends_the_group_of_a_write_whose_channel_is_removed() {
    let ready = async |test: &Test| {
        let (clock, hub) = (test.clock.clone(), test.hub.clone());
        test.tasks.spawn(async move {
            clock.sleep(START).await;
            hub.set_definitions(&without(&["value"]));
        });
    };
    let home = read_session(411, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        agent.sleep(START).await;
        send(&mut stream, &[1], &[10]).await;
        assert_eq!(stream.reply().await, ended(0, End::Removed));
        assert_eq!(stream.reply().await, ended_at(1, 0, End::Removed));
        send(&mut stream, &[2], &[20]).await;
        assert_eq!(end(&mut stream).await, [ended_at(2, 0, End::Removed)]);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A frame whose body message passes its body, and a frame whose ends name a place
/// that the write does not have, stop the stream as `MALFORMED`.
#[test]
fn stops_a_write_whose_frame_breaks_a_rule_as_malformed() {
    let home = read_session(
        415,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut over = write(&agent).await;
            let (ends, mut body) = frame(&[(0, &[1]), (1, &[10])]);
            body.extend([0; 8]);
            over.frame(live(2), &[1], &ends, &body).await;
            assert_eq!(over.recv().await, reset_with(MALFORMED));
            let mut outside = write(&agent).await;
            let (ends, body) = frame(&[(0, &[1]), (2, &[10])]);
            outside.frame(live(2), &[1], &ends, &body).await;
            assert_eq!(outside.recv().await, reset_with(MALFORMED));
            finish(&mut agent).await;
        },
    );
    let over = wire::hub::Error::Body {
        len: 24,
        remain: 16,
    };
    let outside = types::frame::Error::OutOfRange {
        entry: 2,
        entries: 2,
    };
    assert_eq!(
        home.served,
        [
            Err(serve::Error::Message(over)),
            Err(serve::Error::Frame(outside.clone())),
            Ok(Got::Ended)
        ]
    );
    assert_eq!(
        serve::Error::Frame(outside).to_string(),
        "a frame of a write is not valid: entry 2 is out of range for a key set of 2 \
         entries"
    );
}

/// A program that finishes its stream inside a frame stops it as `MALFORMED`.
#[test]
fn stops_a_write_that_ends_inside_a_frame_as_malformed() {
    let home = read_session(
        412,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut stream = write(&agent).await;
            let mut head = [0; Head::LEN];
            live(2).encode(&mut head);
            stream.send(&head).await;
            stream.sender.finish().expect("finishes");
            assert_eq!(stream.recv().await, reset_with(MALFORMED));
            finish(&mut agent).await;
        },
    );
    let cut = wire::hub::Error::Cut {
        received: Head::LEN,
        len: None,
    };
    let cut = serve::Error::Message(cut);
    assert_eq!(home.served, [Err(cut.clone()), Ok(Got::Ended)]);
    assert_eq!(
        cut.to_string(),
        "a hub message is not valid: the stream ended inside a frame: 7 bytes, before \
         the end of its ends"
    );
}

/// A failed sync of the buffer ends the group of a write whose frame waits for its
/// commit as `Failed`, and the stream goes on.
#[test]
fn ends_the_group_whose_commit_failed_as_failed() {
    let ready = async |test: &Test| {
        let (clock, node) = (test.clock.clone(), test.node.clone());
        test.tasks.spawn(async move {
            clock.sleep(START).await;
            node.fail_file(FilePath::new(RING), Operation::Sync);
        });
    };
    let home = read_session(413, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        agent.sleep(START).await;
        send(&mut stream, &[1], &[10]).await;
        assert_eq!(stream.reply().await, taken(1, 0, 1));
        assert_eq!(stream.reply().await, ended(0, End::Failed));
        send(&mut stream, &[2], &[20]).await;
        assert_eq!(end(&mut stream).await, [ended_at(2, 0, End::Failed)]);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A failed sync of the buffer after the program finishes its stream ends the group
/// as `Failed`, with no `Stored` for the frame that waits for that commit, and the
/// stream finishes.
#[test]
fn ends_the_group_whose_last_commit_failed_as_failed() {
    let ready = async |test: &Test| {
        let (clock, node) = (test.clock.clone(), test.node.clone());
        test.tasks.spawn(async move {
            clock.sleep(START).await;
            node.fail_file(FilePath::new(RING), Operation::Sync);
        });
    };
    let home = read_session(422, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        agent.sleep(START).await;
        send(&mut stream, &[1], &[10]).await;
        let replies = [taken(1, 0, 1), ended(0, End::Failed)];
        assert_eq!(end(&mut stream).await, replies);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

fn taken_in(frame: u64, group: u32, seq: u64, count: u32) -> Reply {
    Reply::Outcome {
        frame,
        group,
        outcome: Outcome::Taken {
            range: types::frame::Range { seq, count },
        },
    }
}

fn ended(group: u32, end: End) -> Reply {
    Reply::Ended { group, end }
}

fn ended_at(frame: u64, group: u32, end: End) -> Reply {
    Reply::Outcome {
        frame,
        group,
        outcome: Outcome::Ended(end),
    }
}

/// Opens a write of `value` and `value-b`, two groups, and checks its places: the
/// indexes `time` and `time-b` at places 0 and 1, which the node numbers first, then
/// `value` and `value-b`. Open it before a read, which numbers its channels.
async fn write_two(agent: &Agent) -> Stream {
    let mut stream = agent.write(&["value", "value-b"]).await;
    let start = stream.started().await;
    assert_eq!(
        start.places,
        [
            place(1, STAMP, "time", 0),
            place(3, STAMP, "time-b", 1),
            place(2, I64, "value", 0),
            place(4, I64, "value-b", 1),
        ]
    );
    stream
}

/// Sends a live frame of `value` at `stamp` and `value-b` at `stamp_b`, each of one
/// sample.
async fn send_two(
    stream: &mut Stream,
    (stamp, value): (i64, i64),
    (stamp_b, value_b): (i64, i64),
) {
    let (ends, body) = frame(&[
        (0, &[stamp]),
        (1, &[stamp_b]),
        (2, &[value]),
        (3, &[value_b]),
    ]);
    stream.frame(live(4), &[1, 1], &ends, &body).await;
}

/// A write on two indexes gives each group of a frame its outcome, in group order, and
/// each group its own seqs and `Stored`. A read of each channel gets its frames.
#[test]
fn serves_a_write_on_two_indexes_with_the_outcome_and_stored_of_each_group() {
    let home = read_session(
        446,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut stream = write_two(&agent).await;
            let (mut reading, mut reader) = read(&agent).await;
            let (mut reading_b, mut reader_b) = read_of(&agent, "value-b").await;
            send_two(&mut stream, (1, 10), (5, 50)).await;
            assert_eq!(stream.reply().await, taken_in(1, 0, 0, 1));
            assert_eq!(stream.reply().await, taken_in(1, 1, 0, 1));
            assert_eq!(
                stream.reply().await,
                Reply::Stored {
                    group: 0,
                    frames: 1
                }
            );
            assert_eq!(
                stream.reply().await,
                Reply::Stored {
                    group: 1,
                    frames: 1
                }
            );
            let (ends, body) = frame(&[(1, &[6]), (3, &[60])]);
            stream.frame(live(2), &[1], &ends, &body).await;
            assert_eq!(stream.reply().await, taken_in(2, 1, 1, 1));
            let stored = Reply::Stored {
                group: 1,
                frames: 2,
            };
            assert_eq!(end(&mut stream).await, [stored]);
            assert_eq!(next(&mut reading, &mut reader).await, (10, 1));
            assert_eq!(next(&mut reading_b, &mut reader_b).await, (50, 5));
            assert_eq!(next(&mut reading_b, &mut reader_b).await, (60, 6));
            for reading in [&mut reading, &mut reading_b] {
                reading.sender.finish().expect("finishes");
                assert_eq!(reading.recv().await, Ok(None));
            }
            finish(&mut agent).await;
        },
    );
    assert_eq!(
        home.served,
        [
            Ok(Got::Ended),
            Ok(Got::Ended),
            Ok(Got::Ended),
            Ok(Got::Ended)
        ]
    );
}

/// The node stamps each group of a frame of `Time::Node` on its own: with no stamp
/// before them, both indexes get the same mesh time.
#[test]
fn stamps_each_group_of_an_untimed_frame_on_its_own() {
    let home = read_session(
        447,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut stream = write_two(&agent).await;
            let (mut reading, mut reader) = read(&agent).await;
            let (mut reading_b, mut reader_b) = read_of(&agent, "value-b").await;
            let (ends, body) = frame(&[(0, &[0]), (1, &[0]), (2, &[10]), (3, &[20])]);
            stream.frame(untimed(4), &[1, 1], &ends, &body).await;
            assert_eq!(stream.reply().await, taken_in(1, 0, 0, 1));
            assert_eq!(stream.reply().await, taken_in(1, 1, 0, 1));
            let (value, stamp) = next(&mut reading, &mut reader).await;
            let (value_b, stamp_b) = next(&mut reading_b, &mut reader_b).await;
            assert_eq!((value, value_b), (10, 20));
            assert_eq!(stamp, stamp_b);
            let stored = |group| Reply::Stored { group, frames: 1 };
            assert_eq!(end(&mut stream).await, [stored(0), stored(1)]);
            for reading in [&mut reading, &mut reading_b] {
                reading.sender.finish().expect("finishes");
                assert_eq!(reading.recv().await, Ok(None));
            }
            finish(&mut agent).await;
        },
    );
    assert_eq!(
        home.served,
        [
            Ok(Got::Ended),
            Ok(Got::Ended),
            Ok(Got::Ended),
            Ok(Got::Ended)
        ]
    );
}

/// A group of a frame whose copy finds no block is refused as `Full`, and the other
/// group of the frame reaches its home, which loses it for no room. Each group takes
/// the next frame once the pool has room.
#[test]
fn refuses_the_group_of_a_frame_whose_copy_finds_no_block_as_full() {
    let stamps: Vec<i64> = (1..=32).collect();
    // The blocks of the two outcomes and the two `Stored`, then of the frame of 624
    // bytes and of the copy of its first group, 80 bytes. The copy of the second group,
    // 560 bytes, finds none.
    let sizes = [Outcome::LEN_MAX; 4].into_iter().chain([624, 80]).collect();
    let ready = spare_for_a_while(sizes);
    let home = read_session(450, ready, move |mut agent| async move {
        agent.admit().await;
        let mut stream = write_two(&agent).await;
        agent.sleep(Span::from_nanos(START.nanos() * 3 / 2)).await;
        let (ends, body) =
            frame(&[(0, &[1, 2]), (1, &stamps), (2, &[10, 20]), (3, &stamps)]);
        stream.frame(live(4), &[2, 32], &ends, &body).await;
        let lost = Reply::Outcome {
            frame: 1,
            group: 0,
            outcome: Outcome::Lost {
                range: types::frame::Range { seq: 0, count: 2 },
            },
        };
        assert_eq!(stream.reply().await, lost);
        let full = Reply::Outcome {
            frame: 1,
            group: 1,
            outcome: Outcome::Refused(wire::hub::write::Refusal::Full),
        };
        assert_eq!(stream.reply().await, full);
        agent.sleep(START).await;
        send_two(&mut stream, (100, 30), (100, 40)).await;
        assert_eq!(stream.reply().await, taken_in(2, 0, 2, 1));
        assert_eq!(stream.reply().await, taken_in(2, 1, 0, 1));
        let stored_b = Reply::Stored {
            group: 1,
            frames: 2,
        };
        assert_eq!(end(&mut stream).await, [stored(2), stored_b]);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A removed channel ends only its group: the other group and the stream go on, each
/// later frame of the ended group gets the same end, and the stream finishes with no
/// error.
#[test]
fn ends_one_group_whose_channel_is_removed_and_writes_the_other() {
    let ready = async |test: &Test| {
        let (clock, hub) = (test.clock.clone(), test.hub.clone());
        test.tasks.spawn(async move {
            clock.sleep(START).await;
            hub.set_definitions(&without(&["value-b"]));
        });
    };
    let home = read_session(448, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = write_two(&agent).await;
        agent.sleep(START).await;
        send_two(&mut stream, (1, 10), (1, 10)).await;
        assert_eq!(stream.reply().await, taken_in(1, 0, 0, 1));
        assert_eq!(stream.reply().await, ended(1, End::Removed));
        assert_eq!(stream.reply().await, ended_at(1, 1, End::Removed));
        assert_eq!(stream.reply().await, stored(1));
        send_two(&mut stream, (2, 20), (2, 20)).await;
        let replies = [
            taken_in(2, 0, 1, 1),
            ended_at(2, 1, End::Removed),
            stored(2),
        ];
        assert_eq!(end(&mut stream).await, replies);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A failed commit ends only the groups whose taken frames wait for it: a group with
/// no taken frame goes on.
#[test]
fn ends_only_the_groups_whose_commit_failed() {
    let ready = async |test: &Test| {
        let (clock, node) = (test.clock.clone(), test.node.clone());
        test.tasks.spawn(async move {
            clock.sleep(START).await;
            node.fail_file(FilePath::new(RING), Operation::Sync);
        });
    };
    let home = read_session(449, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = write_two(&agent).await;
        agent.sleep(START).await;
        let (ends, body) = frame(&[(0, &[1]), (2, &[10])]);
        stream.frame(live(2), &[1], &ends, &body).await;
        let replies = [taken_in(1, 0, 0, 1), ended(0, End::Failed)];
        assert_eq!(end(&mut stream).await, replies);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// The home of `TIME` is another node and that of `TIME_B` is this node: it serves
/// one session of a program and records what each stream gave into `kept`.
async fn split_home(node: sim::node::Node, tasks: env::tasks::Tasks, kept: Kept) {
    in_region(node, tasks, kept, async |test| {
        test.set_home(TIME, region::OTHER).await;
    })
    .await;
}

/// `TIME` has no home and that of `TIME_B` is this node: it serves one session of a
/// program, records what each stream gave into `kept`, moves `value` to `TIME_B` at
/// [`START`], and makes this node the home of `TIME` [`START`] later.
async fn moves_at_start(node: sim::node::Node, tasks: env::tasks::Tasks, kept: Kept) {
    in_region(node, tasks, kept, async |test| {
        test.clock.sleep(START).await;
        let mut moved = channels();
        let definition = super::definition(2, DataType::Sample(I64), 3);
        moved.insert(name("value"), definition);
        test.set_spec(moved).await;
        test.clock.sleep(START).await;
        test.set_home(TIME, NODE).await;
    })
    .await;
}

/// A hub in a region where the home of `TIME_B` is this node: it serves one session of
/// a program and records what each stream gave into `kept`, and runs `then` while the
/// session is open.
async fn in_region(
    node: sim::node::Node,
    tasks: env::tasks::Tasks,
    kept: Kept,
    then: impl AsyncFnOnce(&Test),
) {
    let transport = Rc::new(transport(&node, &tasks, &own_pool(), HOME, 1 << 16));
    let region =
        region::open(&node, &tasks, Rc::clone(&transport), Vec::new(), channels())
            .await;
    let layout = buffer::Layout::new(AREA, BODY_MAX).expect("a ring");
    let mut test = Test::new(
        node.clone(),
        NODE,
        tasks.clone(),
        layout,
        POOL,
        Some(region),
    )
    .await;
    test.sync().await;
    test.set_home(TIME_B, NODE).await;
    test.hub.set_rules(rules());
    let session = transport.accept().await.expect("a session");
    let link = test.hub.link(session.clone());
    let accepted = Rc::new(Cell::new(0));
    tasks.spawn(serve_each(
        &session,
        &link,
        &tasks,
        &node.clock(),
        &kept,
        &accepted,
    ));
    then(&test).await;
    session.closed().await;
    recorded(&node, &kept, accepted.get()).await;
}

/// A write on two indexes, where the home of one index is another node, stops with
/// `NOT_HOME`.
#[test]
fn stops_a_write_on_two_indexes_whose_one_home_is_another_node_as_not_home() {
    let served = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&served);
    let home = move |node, tasks| split_home(node, tasks, kept);
    run_program(423, home, |node, tasks, at| {
        as_agent(node, tasks, at, |mut agent| async move {
            agent.admit().await;
            let mut stream = agent.write(&["value", "value-b"]).await;
            assert_eq!(stream.recv().await, reset_with(NOT_HOME));
            finish(&mut agent).await;
        })
    });
    let remote = writer::Error::Remote {
        home: region::OTHER,
    };
    let served = std::mem::take(&mut *served.lock().expect("not poisoned"));
    assert_eq!(
        served,
        [Err(serve::Error::Writer(Box::new(remote))), Ok(Got::Ended)]
    );
}

/// A node serves a write only of an index whose home it is: a write whose home is
/// another node stops with `NOT_HOME`, and takes no control there.
#[test]
fn stops_a_write_whose_home_is_another_node_as_not_home() {
    let served = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&served);
    let home = move |node, tasks| split_home(node, tasks, kept);
    run_program(414, home, |node, tasks, at| {
        as_agent(node, tasks, at, |mut agent| async move {
            agent.admit().await;
            let mut stream = agent.write(&["value"]).await;
            assert_eq!(stream.recv().await, reset_with(NOT_HOME));
            finish(&mut agent).await;
        })
    });
    let remote = writer::Error::Remote {
        home: region::OTHER,
    };
    let served = std::mem::take(&mut *served.lock().expect("not poisoned"));
    assert_eq!(
        served,
        [Err(serve::Error::Writer(Box::new(remote))), Ok(Got::Ended)]
    );
}

/// A write whose channel moves to a second index while it waits for the home of its
/// index opens on both indexes once each has its home.
#[test]
fn opens_a_write_whose_channel_moves_to_a_second_index_during_its_home_wait() {
    let served = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&served);
    let home = move |node, tasks| moves_at_start(node, tasks, kept);
    run_program(437, home, |node, tasks, at| {
        as_agent(node, tasks, at, |mut agent| async move {
            agent.admit().await;
            let mut stream = agent.write(&["time", "value"]).await;
            let start = stream.started().await;
            assert_eq!(
                start.places,
                [
                    place(1, STAMP, "time", 0),
                    place(3, STAMP, "time-b", 1),
                    place(2, I64, "value", 1),
                ]
            );
            let (ends, body) = frame(&[(0, &[1]), (1, &[5]), (2, &[50])]);
            stream.frame(live(3), &[1, 1], &ends, &body).await;
            assert_eq!(stream.reply().await, taken_in(1, 0, 0, 1));
            assert_eq!(stream.reply().await, taken_in(1, 1, 0, 1));
            let mut replies = end(&mut stream).await;
            replies.sort_by_key(|reply| format!("{reply:?}"));
            let stored = |group| Reply::Stored { group, frames: 1 };
            assert_eq!(replies, [stored(0), stored(1)]);
            finish(&mut agent).await;
        })
    });
    let served = std::mem::take(&mut *served.lock().expect("not poisoned"));
    assert_eq!(served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A write on two indexes holds the ends of its four places, 80 bytes, not of its two
/// channels and one index.
#[test]
fn holds_the_ends_of_each_place_of_a_write_on_two_indexes() {
    let home = read_session(
        452,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut stream = write_two(&agent).await;
            agent.sleep(Span::MILLISECOND).await;
            let held = agent.unfinished(BODY_BYTES_MAX - 79, &[]).await;
            agent.sleep(Span::MILLISECOND).await;
            drop(held);
            agent.sleep(Span::MILLISECOND).await;
            assert_eq!(end(&mut stream).await, []);
            finish(&mut agent).await;
        },
    );
    let share = serve::Error::Share {
        subject: name(SUBJECT),
        length: BODY_BYTES_MAX - 79,
        held: 80,
    };
    assert!(
        home.served.contains(&Err(share)),
        "served: {:?}",
        home.served
    );
}

/// A frame of a group that ended, which finds no block for its draft, gets the end of
/// the group, not `Full`.
#[test]
fn refuses_a_frame_of_an_ended_group_with_its_end() {
    let ready = async |test: &Test| {
        let (clock, hub) = (test.clock.clone(), test.hub.clone());
        test.tasks.spawn(async move {
            clock.sleep(Span::from_nanos(START.nanos() / 2)).await;
            hub.set_definitions(&without(&["value"]));
        });
        let sizes = vec![Outcome::LEN_MAX, Outcome::LEN_MAX, 64];
        spare_for_a_while(sizes)(test).await;
    };
    let stamps: Vec<i64> = (1..=32).collect();
    let home = read_session(491, ready, move |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        agent.sleep(Span::from_nanos(START.nanos() * 5 / 4)).await;
        send(&mut stream, &[1], &[10]).await;
        assert_eq!(stream.reply().await, ended(0, End::Removed));
        assert_eq!(stream.reply().await, ended_at(1, 0, End::Removed));
        send(&mut stream, &stamps, &stamps).await;
        assert_eq!(stream.reply().await, ended_at(2, 0, End::Removed));
        assert_eq!(end(&mut stream).await, []);
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A group whose commit failed gives back the block of its next `Stored`: while the
/// stream idles, the pool has as much room as after the stream ends.
#[test]
fn holds_no_block_for_a_stored_after_its_group_ends() {
    let ready = async |test: &Test| {
        let config = super::unnamed(&["value"], hub::reader::Mode::Latest);
        let mut reader = test.hub.reader(config).await.expect("opens");
        let (clock, node, pool) =
            (test.clock.clone(), test.node.clone(), Rc::clone(&test.pool));
        test.tasks.spawn(async move {
            clock.sleep(START).await;
            node.fail_file(FilePath::new(RING), Operation::Sync);
            reader.next().await.expect("a frame");
            reader.next().await.expect_err("a failed sync");
            let room = || {
                let blocks = super::fill(&pool);
                (
                    blocks.len(),
                    blocks.iter().map(|block| block.len()).sum::<usize>(),
                )
            };
            clock.sleep(Span::from_nanos(START.nanos() / 2)).await;
            let idle = room();
            clock.sleep(START).await;
            assert_eq!(idle, room());
        });
    };
    let home = read_session(496, ready, |mut agent| async move {
        agent.admit().await;
        // The read of `value` numbers `time` and `value` first.
        let mut stream = agent.write(&["value", "value-b"]).await;
        stream.started().await;
        agent.sleep(START).await;
        for (stamp, value) in [(1, 10), (2, 20)] {
            let (ends, body) = frame(&[(0, &[stamp]), (1, &[value])]);
            stream.frame(live(2), &[1], &ends, &body).await;
        }
        assert_eq!(stream.reply().await, taken_in(1, 0, 0, 1));
        assert_eq!(stream.reply().await, taken_in(2, 0, 1, 1));
        assert_eq!(stream.reply().await, ended(0, End::Failed));
        agent.sleep(START).await;
        assert_eq!(end(&mut stream).await, []);
        agent.sleep(START).await;
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// The `Stored` of a commit that is due when its group ends never comes.
#[test]
fn drops_the_stored_of_a_commit_due_when_the_group_ends() {
    let ready = async |test: &Test| {
        let config = super::unnamed(&["value"], hub::reader::Mode::Latest);
        let mut reader = test.hub.reader(config).await.expect("opens");
        let hub = test.hub.clone();
        test.tasks.spawn(async move {
            let _frame = reader.next().await.expect("a frame");
            hub.set_definitions(&without(&["value"]));
        });
    };
    let home = read_session(492, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = write(&agent).await;
        send(&mut stream, &[1], &[10]).await;
        assert_eq!(stream.reply().await, taken(1, 0, 1));
        send(&mut stream, &[2], &[20]).await;
        let replies = end(&mut stream).await;
        assert_eq!(
            replies,
            [ended(0, End::Removed), ended_at(2, 0, End::Removed)]
        );
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// The `Stored` replies of a commit come in the order in which the groups took their
/// first frame since the commit before, not in group order.
#[test]
fn sends_the_stored_of_each_group_in_the_order_the_groups_took() {
    let home = read_session(
        495,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut stream = write_two(&agent).await;
            send_two(&mut stream, (1, 10), (5, 50)).await;
            let (ends, body) = frame(&[(1, &[6]), (3, &[60])]);
            stream.frame(live(2), &[1], &ends, &body).await;
            let (ends, body) = frame(&[(0, &[2]), (2, &[20])]);
            stream.frame(live(2), &[1], &ends, &body).await;
            let stored = |group, frames| Reply::Stored { group, frames };
            let replies = [
                taken_in(1, 0, 0, 1),
                taken_in(1, 1, 0, 1),
                taken_in(2, 1, 1, 1),
                taken_in(3, 0, 1, 1),
                stored(0, 1),
                stored(1, 1),
                stored(1, 2),
                stored(0, 3),
            ];
            assert_eq!(end(&mut stream).await, replies);
            finish(&mut agent).await;
        },
    );
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// The writers of a write open in group order, the order of the index slots, not the
/// order of the channels of the open.
#[test]
fn opens_the_writer_of_each_group_in_group_order() {
    let home = read_session(
        493,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let (mut reading, mut reader) = read(&agent).await;
            let mut stream = agent.write(&["value-b", "value"]).await;
            let start = stream.started().await;
            assert_eq!(
                start.places,
                [
                    place(1, STAMP, "time", 0),
                    place(2, I64, "value", 0),
                    place(3, STAMP, "time-b", 2),
                    place(4, I64, "value-b", 2),
                ]
            );
            let (ends, body) =
                frame(&[(0, &[1]), (1, &[10]), (2, &[5, 6]), (3, &[50, 60])]);
            stream.frame(live(4), &[1, 2], &ends, &body).await;
            assert_eq!(stream.reply().await, taken_in(1, 0, 0, 1));
            assert_eq!(stream.reply().await, taken_in(1, 1, 0, 2));
            assert_eq!(next(&mut reading, &mut reader).await, (10, 1));
            let mut replies = end(&mut stream).await;
            replies.sort_by_key(|reply| format!("{reply:?}"));
            let stored = |group| Reply::Stored { group, frames: 1 };
            assert_eq!(replies, [stored(0), stored(1)]);
            reading.sender.finish().expect("finishes");
            assert_eq!(reading.recv().await, Ok(None));
            finish(&mut agent).await;
        },
    );
    assert_eq!(home.served[0], Ok(Got::Ended));
}
