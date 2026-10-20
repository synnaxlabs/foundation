//! The read streams of a client session that `Link::serve` serves, over a real
//! transport from a program to a simulated node.

use std::cell::Cell;
use std::path::{Path as FilePath, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use access::proof::{Error as Refusal, Kind};
use env::files::Operation;
use hub::{reader, serve};
use transport::Class;
use types::channel;
use types::time::Span;
use wire::header::MALFORMED;
use wire::hub::client::{
    self, BODY_BYTES_MAX, Mode, Open, PLACES_MAX, Place, Places, REFUSED,
};
use wire::hub::{BUSY, FAILED, FromHome, NOT_HOME, UNKNOWN};

use super::link::{as_agent, serve_session_on};
use super::serve::{decoded, got, places as placed};
use super::{
    AREA, BODY_MAX, COMMIT, DataType, I64, NODE, POOL, RING, STAMP, TIME, TIME_B, Test,
    channels, region, without, write, write_wide,
};
use crate::agent::{AGENT, Agent, SUBJECT, Stream, name, place, reset_with};
use crate::net::{HOME, own_pool, transport};
use crate::sessions::{
    Got, Home, OTHER, QUIET, recorded, rules, run_program, serve_each,
};

/// How long after the home accepts the session its first write or change comes, so
/// the program has opened its read by then.
const START: Span = Span::from_nanos(100_000_000);

/// Runs one client session on a synced [`Test`] hub, as `link::session` does, where
/// `ready` gets the hub before the home serves the session's streams.
pub(super) fn read_session<P>(
    seed: u64,
    ready: impl AsyncFnOnce(&Test) + Send + 'static,
    program: impl FnOnce(Agent) -> P + Send + 'static,
) -> Home
where
    P: Future<Output = ()> + 'static,
{
    serve_session_on(
        seed,
        sim::link::Config::default(),
        true,
        POOL,
        Some(rules()),
        ready,
        move |node, tasks, at| as_agent(node, tasks, at, program),
    )
}

/// Writes each of `values` to `value` in a frame of its own, [`COMMIT`] apart, from
/// [`START`] on.
async fn writes(test: &Test, values: &'static [i64]) {
    let mut writer = test.writer("a", &["value"]).await;
    let (clock, now) = (test.clock.clone(), test.now());
    test.tasks.spawn(async move {
        clock.sleep(START).await;
        for (stamp, &value) in (now..).zip(values) {
            write(&mut writer, &[stamp], &[value]);
            clock.sleep(Span::from_nanos(2 * COMMIT.nanos())).await;
        }
    });
}

/// Writes `frames` wide frames to `value` from [`START`] on, in runs of ten that are
/// 2 × [`COMMIT`] apart.
async fn writes_wide(test: &Test, frames: i64) {
    let mut writer = test.writer("a", &["value"]).await;
    let (clock, now) = (test.clock.clone(), test.now());
    test.tasks.spawn(async move {
        clock.sleep(START).await;
        for n in 0..frames {
            write_wide(&mut writer, now, n);
            if n % 10 == 9 {
                clock.sleep(Span::from_nanos(2 * COMMIT.nanos())).await;
            }
        }
    });
}

fn open(mode: Mode, select: &[&str]) -> Open {
    Open {
        mode,
        select: types::name::Selector::new(select.iter().copied()).expect("a selector"),
    }
}

/// The place of the index of each place of `run`.
fn indexes(run: &[Place]) -> Vec<usize> {
    run.iter().map(|place| place.index).collect()
}

/// Finishes the hello stream and waits, so the hello stream ends as `Ended`.
pub(super) async fn finish(agent: &mut Agent) {
    agent.hello.sender.finish().expect("finishes");
    agent.sleep(QUIET).await;
}

/// The places of a read are the channels that its selector matches, in name order,
/// then its index when the selector does not match it.
#[test]
fn sends_the_places_and_each_frame_of_a_complete_read() {
    let ready = async |test: &Test| writes(test, &[10, 20]).await;
    let home = read_session(300, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = agent.read(Mode::Complete, &["value"]).await;
        let (places, run, mut reader) = stream.opened().await;
        assert_eq!(places, Places { count: 2 });
        assert_eq!(run, [place(2, I64, "value", 1), place(1, STAMP, "time", 1)]);
        for value in [10, 20] {
            let got = got(&mut stream.receiver, &mut reader).await;
            assert_eq!(placed(&got), [0, 1]);
            assert_eq!(decoded(&got, &[I64, STAMP])[0], [value]);
        }
        stream.sender.finish().expect("finishes");
        assert_eq!(stream.recv().await, Ok(None));
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A complete read that opens after two frames gets its open position in `Opened`,
/// and its first frame starts there.
#[test]
fn sends_the_open_position_of_a_complete_read_in_opened() {
    let ready = async |test: &Test| {
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        for (stamp, value) in (now..).zip([10, 20]) {
            write(&mut writer, &[stamp], &[value]);
        }
        let clock = test.clock.clone();
        test.tasks.spawn(async move {
            clock.sleep(START).await;
            write(&mut writer, &[now + 2], &[30]);
        });
    };
    let home = read_session(321, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = agent.read(Mode::Complete, &["value"]).await;
        let (_, _, mut reader) = stream.opened().await;
        assert_eq!(stream.live, Some(2));
        let got = got(&mut stream.receiver, &mut reader).await;
        assert_eq!(got.head.range.seq, 2);
        assert_eq!(decoded(&got, &[I64, STAMP])[0], [30]);
        stream.sender.finish().expect("finishes");
        assert_eq!(stream.recv().await, Ok(None));
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

#[test]
fn sends_a_latest_read_the_index_once_when_its_selector_matches_it() {
    let ready = async |test: &Test| writes(test, &[10]).await;
    let home = read_session(301, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = agent.read(Mode::Latest, &["value", "time"]).await;
        let (places, run, mut reader) = stream.opened().await;
        assert_eq!(places, Places { count: 2 });
        assert_eq!(run, [place(1, STAMP, "time", 0), place(2, I64, "value", 0)]);
        let got = got(&mut stream.receiver, &mut reader).await;
        assert_eq!(placed(&got), [0, 1]);
        assert_eq!(decoded(&got, &[STAMP, I64])[1], [10]);
        stream.sender.finish().expect("finishes");
        assert_eq!(stream.recv().await, Ok(None));
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

#[test]
fn refuses_a_read_signed_with_another_key_or_as_a_request() {
    let home = read_session(
        302,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            for (kind, key) in [(Kind::Read, &OTHER), (Kind::Request, &AGENT)] {
                let mut stream = agent.open_as(Class::Complete).await;
                stream
                    .read(&open(Mode::Complete, &["value"]), kind, key)
                    .await;
                assert_eq!(stream.recv().await, reset_with(REFUSED));
            }
            finish(&mut agent).await;
        },
    );
    let signature = || Err(serve::Error::Access(Box::new(Refusal::Signature)));
    assert_eq!(home.served, [signature(), signature(), Ok(Got::Ended)]);
}

#[test]
fn stops_a_read_whose_selector_matches_no_channel_or_two_indexes() {
    let home = read_session(
        303,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut empty = agent.read(Mode::Complete, &["nope"]).await;
            assert_eq!(empty.recv().await, reset_with(UNKNOWN));
            let mut many = agent.read(Mode::Latest, &["value", "value-b"]).await;
            assert_eq!(many.recv().await, reset_with(MALFORMED));
            finish(&mut agent).await;
        },
    );
    let empty = serve::Error::Reader(Box::new(reader::Error::Empty));
    let many = reader::Error::ManyIndexes {
        first: name("value"),
        other: name("value-b"),
    };
    assert_eq!(
        home.served,
        [
            Err(empty.clone()),
            Err(serve::Error::Reader(Box::new(many))),
            Ok(Got::Ended)
        ]
    );
    assert_eq!(
        empty.to_string(),
        "the reader of a read did not open: the selector matches no channel"
    );
}

/// A latest read takes a `Latest` stream, and a complete read a `Complete` stream.
#[test]
fn stops_a_read_on_a_stream_of_the_other_class_as_malformed() {
    let home = read_session(
        304,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            for (class, mode) in [
                (Class::Complete, Mode::Latest),
                (Class::Latest, Mode::Complete),
            ] {
                let mut stream = agent.open_as(class).await;
                stream
                    .read(&open(mode, &["value"]), Kind::Read, &AGENT)
                    .await;
                assert_eq!(stream.recv().await, reset_with(MALFORMED));
            }
            finish(&mut agent).await;
        },
    );
    assert_eq!(
        home.served,
        [
            Err(serve::Error::Class(Class::Complete)),
            Err(serve::Error::Class(Class::Latest)),
            Ok(Got::Ended)
        ]
    );
}

/// A read reserves its open under the share of its subject, not the request turn of its
/// link, until its open decodes after the wait for the home of its index. The open of
/// `time` and `value` is 14 bytes, and that of `value` 8, which fills the share.
#[test]
fn opens_reads_beside_a_request_and_stops_ones_over_the_share_as_busy() {
    let home = read_session(
        305,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let held = agent.unfinished(BODY_BYTES_MAX - 8, &[]).await;
            agent.sleep(Span::MILLISECOND).await;
            let mut busy = agent.read(Mode::Complete, &["time", "value"]).await;
            assert_eq!(busy.recv().await, reset_with(BUSY));
            let mut stream = agent.read(Mode::Complete, &["value"]).await;
            let (places, run, _) = stream.opened().await;
            assert_eq!(places, Places { count: 2 });
            assert_eq!(indexes(&run), [1, 1]);
            drop(held);
            agent.sleep(Span::MILLISECOND).await;
            for _ in 0..2 {
                let mut stream = agent.read(Mode::Complete, &["time", "value"]).await;
                let (places, run, _) = stream.opened().await;
                assert_eq!(places, Places { count: 2 });
                assert_eq!(indexes(&run), [0, 0]);
            }
        },
    );
    let share = serve::Error::Share {
        subject: name(SUBJECT),
        length: 14,
        held: BODY_BYTES_MAX - 8,
    };
    assert_eq!(home.served[0], Err(share.clone()));
    assert_eq!(
        share.to_string(),
        "14 bytes of a body or of the ends, counts, or names of a write do not fit \
         under the share of 16777216 bytes of subject ops.agent: its open requests, \
         reads, and writes hold 16777208 bytes"
    );
}

/// A read holds the reservation of its open only until the open decodes after the wait
/// for the home of its index, so reads whose opens fit the share one at a time each
/// open.
#[test]
fn frees_the_share_of_the_open_of_a_read_once_it_decodes() {
    let home = read_session(
        316,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let held = agent.unfinished(BODY_BYTES_MAX - 8, &[]).await;
            agent.sleep(Span::MILLISECOND).await;
            let mut streams = Vec::new();
            for _ in 0..2 {
                let mut stream = agent.read(Mode::Complete, &["value"]).await;
                let (places, run, _) = stream.opened().await;
                assert_eq!(places, Places { count: 2 });
                assert_eq!(indexes(&run), [1, 1]);
                streams.push(stream);
            }
            drop((held, streams));
            // So each reset reaches the home before the finish ends the session.
            agent.sleep(QUIET).await;
            finish(&mut agent).await;
        },
    );
    let reset = serve::Error::Stream(transport::Error::Reset {
        code: transport::Code(0),
    });
    assert_eq!(
        home.served,
        [
            Err(reset.clone()),
            Err(reset.clone()),
            Err(reset),
            Ok(Got::Ended)
        ]
    );
}

/// A read frees the share of its open before it sends its places, so a read whose
/// program does not take its places holds no share.
#[test]
fn frees_the_share_of_the_open_of_a_read_before_its_places_go_out() {
    const MANY: usize = 10_000;
    let ready = async |test: &Test| {
        let long = "x".repeat(240);
        let names: Vec<String> = (0..MANY).map(|n| format!("w.{long}.{n}")).collect();
        test.define(names.iter().enumerate().map(|(n, channel)| {
            let key = 100 + u128::try_from(n).expect("fits");
            (key, channel.as_str(), DataType::Sample(I64), 1)
        }))
        .await;
    };
    read_session(318, ready, |mut agent| async move {
        agent.admit().await;
        let held = agent.unfinished(BODY_BYTES_MAX - 8, &[]).await;
        agent.sleep(Span::MILLISECOND).await;
        let mut wide = agent.read(Mode::Complete, &["w.**"]).await;
        agent.sleep(Span::from_nanos(10_000_000)).await;
        let mut stream = agent.read(Mode::Complete, &["value"]).await;
        agent.sleep(Span::from_nanos(10_000_000)).await;
        wide.opened().await;
        let (places, run, _) = stream.opened().await;
        assert_eq!(places, Places { count: 2 });
        assert_eq!(indexes(&run), [1, 1]);
        drop((held, wide, stream));
        finish(&mut agent).await;
    });
}

/// The places of a read hold no reservation, so a read whose places pass the share of
/// its subject opens and gets its frames.
#[test]
fn opens_a_read_whose_places_pass_the_share() {
    const MANY: usize = 60_000;
    let ready = async |test: &Test| {
        let long = "x".repeat(240);
        let names: Vec<String> = (0..MANY).map(|n| format!("w.{long}.{n}")).collect();
        test.define(names.iter().enumerate().map(|(n, channel)| {
            let key = 100 + u128::try_from(n).expect("fits");
            (key, channel.as_str(), DataType::Sample(I64), 1)
        }))
        .await;
        writes(test, &[10]).await;
    };
    let home = read_session(313, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = agent.read(Mode::Complete, &["w.**", "value"]).await;
        let (places, run, mut reader) = stream.opened().await;
        let time = u32::try_from(MANY + 1).expect("fits");
        assert_eq!(places, Places { count: time + 1 });
        assert_eq!(indexes(&run), vec![MANY + 1; MANY + 2]);
        let bytes: usize = run
            .iter()
            .map(|place| size_of::<Place>() + place.name.as_str().len())
            .sum();
        let share = usize::try_from(BODY_BYTES_MAX).expect("fits");
        assert!(bytes > share, "the places hold {bytes} bytes");
        let got = got(&mut stream.receiver, &mut reader).await;
        assert_eq!(placed(&got), [0, time]);
        assert_eq!(decoded(&got, &[I64, STAMP])[0], [10]);
        stream.sender.finish().expect("finishes");
        assert_eq!(stream.recv().await, Ok(None));
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// The node caps the places of a read at `PLACES_MAX`: the `MANY` channels of `w` and
/// their index are at the cap, and `value` takes them over it. The read over the cap
/// does not carry the index, so only the read after it polls the hub's tasks, at its
/// new carry.
#[test]
fn stops_a_read_whose_places_are_over_the_cap_as_malformed() {
    const MANY: u32 = PLACES_MAX - 1;
    let (polls, ended) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicBool::new(false)),
    );
    let (counted, ending) = (Arc::clone(&polls), Arc::clone(&ended));
    let ready = async move |test: &Test| {
        let names: Vec<String> = (0..MANY).map(|n| format!("w.{n}")).collect();
        test.define(names.iter().enumerate().map(|(n, channel)| {
            let key = 100 + u128::try_from(n).expect("fits");
            (key, channel.as_str(), DataType::Sample(I64), 1)
        }))
        .await;
        let (pause, clock) = (Rc::clone(&test.pause), test.clock.clone());
        let before = pause.polls.get();
        test.tasks.spawn(async move {
            while !ending.load(Ordering::Relaxed) {
                counted.store(pause.polls.get() - before, Ordering::Relaxed);
                clock.sleep(Span::MILLISECOND).await;
            }
        });
    };
    let home = read_session(322, ready, |mut agent| async move {
        agent.admit().await;
        let mut over = agent.read(Mode::Complete, &["w.**", "value"]).await;
        assert_eq!(over.recv().await, reset_with(MALFORMED));
        agent.sleep(QUIET).await;
        assert_eq!(
            polls.load(Ordering::Relaxed),
            0,
            "the read over the cap polled"
        );
        let mut stream = agent.read(Mode::Complete, &["w.**"]).await;
        let (places, run, _) = stream.opened().await;
        agent.sleep(QUIET).await;
        assert_eq!(
            polls.load(Ordering::Relaxed),
            1,
            "the new carry polled once"
        );
        ended.store(true, Ordering::Relaxed);
        assert_eq!(places, Places { count: PLACES_MAX });
        let (index, count) = (usize::try_from(MANY), usize::try_from(PLACES_MAX));
        assert_eq!(
            indexes(&run),
            vec![index.expect("fits"); count.expect("fits")]
        );
        stream.sender.finish().expect("finishes");
        assert_eq!(stream.recv().await, Ok(None));
        finish(&mut agent).await;
    });
    let over = serve::Error::Places {
        count: usize::try_from(PLACES_MAX).expect("fits") + 1,
    };
    assert_eq!(
        home.served,
        [Err(over.clone()), Ok(Got::Ended), Ok(Got::Ended)]
    );
    assert_eq!(
        over.to_string(),
        "the open matches 65537 places, over the cap of 65536"
    );
}

#[test]
fn stops_a_read_whose_channel_is_removed_as_unknown() {
    let ready = async |test: &Test| {
        let (clock, hub) = (test.clock.clone(), test.hub.clone());
        test.tasks.spawn(async move {
            clock.sleep(START).await;
            hub.set_definitions(&without(&["value"]));
        });
    };
    let home = read_session(306, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = agent.read(Mode::Complete, &["value"]).await;
        stream.opened().await;
        assert_eq!(stream.recv().await, reset_with(UNKNOWN));
        finish(&mut agent).await;
    });
    let removed = reader::Ended::Removed(channel::Key::from_u128(2));
    let removed = serve::Error::Ended(Box::new(removed));
    assert_eq!(home.served, [Err(removed.clone()), Ok(Got::Ended)]);
    assert_eq!(
        removed.to_string(),
        "the reader of a read ended: channel 00000000-0000-0000-0000-000000000002 was \
         removed: open a new reader"
    );
}

#[test]
fn stops_a_read_that_gets_a_message_after_its_open_as_malformed() {
    let home = read_session(
        307,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut stream = agent.read(Mode::Latest, &["value"]).await;
            stream.opened().await;
            stream.send(b"x").await;
            assert_eq!(stream.recv().await, reset_with(MALFORMED));
            finish(&mut agent).await;
        },
    );
    let trailing = serve::Error::Client(client::Error::Trailing);
    assert_eq!(home.served, [Err(trailing), Ok(Got::Ended)]);
}

/// A failed sync of the buffer ends the reader of a complete read, which stops it as
/// `FAILED`.
#[test]
fn stops_a_read_whose_buffer_failed_as_failed() {
    let ready = async |test: &Test| {
        let mut writer = test.writer("a", &["value"]).await;
        let (clock, node, now) = (test.clock.clone(), test.node.clone(), test.now());
        test.tasks.spawn(async move {
            clock.sleep(START).await;
            node.fail_file(FilePath::new(RING), Operation::Sync);
            write(&mut writer, &[now], &[1]);
        });
    };
    let home = read_session(311, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = agent.read(Mode::Complete, &["value"]).await;
        stream.opened().await;
        assert_eq!(stream.recv().await, reset_with(FAILED));
        finish(&mut agent).await;
    });
    let failed = reader::Ended::Buffer(env::files::Error::Io {
        path: PathBuf::from(RING),
        operation: Operation::Sync,
        code: 5,
    });
    let failed = serve::Error::Ended(Box::new(failed));
    assert_eq!(home.served, [Err(failed), Ok(Got::Ended)]);
}

/// Runs a complete read of 400 wide frames whose program calls `stop` on its stream 1 s
/// after the open, and asserts that the program gets `heads` heads, then `end`.
fn stop_while_frames_come(
    stop: impl AsyncFnOnce(&mut Stream) + Send + 'static,
    heads: usize,
    end: Result<Option<Vec<u8>>, transport::Error>,
) -> Home {
    let ready = async |test: &Test| writes_wide(test, 400).await;
    read_session(310, ready, move |mut agent| async move {
        agent.admit().await;
        let mut stream = agent.read(Mode::Complete, &["value"]).await;
        let (_, _, mut reader) = stream.opened().await;
        agent.sleep(Span::SECOND).await;
        stop(&mut stream).await;
        let mut frames = 0;
        let got = loop {
            match stream.recv().await {
                Ok(Some(message)) => {
                    if let Ok(FromHome::Head(_)) = reader.decode(Some(&message)) {
                        frames += 1;
                    }
                }
                got => break got,
            }
        };
        assert_eq!(got, end);
        assert_eq!(frames, heads);
        finish(&mut agent).await;
    })
}

/// A message after the open stops a read while its frames still come.
#[test]
fn stops_a_read_with_frames_to_send_at_a_message_after_its_open() {
    let message = async |stream: &mut Stream| stream.send(b"x").await;
    let home = stop_while_frames_come(message, 228, reset_with(MALFORMED));
    let trailing = serve::Error::Client(client::Error::Trailing);
    assert_eq!(home.served, [Err(trailing), Ok(Got::Ended)]);
}

/// A finish after the open ends a read whose frames still come; the node takes no frame
/// after the poll in which the finish arrives.
#[test]
fn ends_a_read_with_frames_to_send_at_a_finish_after_its_open() {
    let finish = async |stream: &mut Stream| stream.sender.finish().expect("finishes");
    let home = stop_while_frames_come(finish, 228, Ok(None));
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A finish that comes with the open ends a read before the node opens its reader.
#[test]
fn ends_a_read_at_a_finish_that_comes_with_its_open() {
    let home = read_session(
        319,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut stream = agent.read(Mode::Complete, &["value"]).await;
            stream.sender.finish().expect("finishes");
            assert_eq!(stream.recv().await, Ok(None));
            finish(&mut agent).await;
        },
    );
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A read that is still open when the session finishes ends as closed.
#[test]
fn ends_a_read_that_is_open_at_the_finish_of_its_session_as_closed() {
    let home = read_session(
        320,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut stream = agent.read(Mode::Complete, &["value"]).await;
            let (places, run, _) = stream.opened().await;
            assert_eq!(places, Places { count: 2 });
            assert_eq!(indexes(&run), [1, 1]);
            finish(&mut agent).await;
            drop(stream);
        },
    );
    let closed = serve::Error::Stream(transport::Error::Closed {
        code: transport::Code(0),
    });
    assert_eq!(home.served, [Ok(Got::Ended), Err(closed)]);
}

/// The channels of a read are removed while its run of places waits for room in the
/// program's window.
#[test]
fn stops_a_read_whose_channel_is_removed_while_its_places_wait_as_unknown() {
    const MANY: u128 = 10_000;
    let ready = async |test: &Test| {
        let long = "x".repeat(240);
        let names: Vec<String> = (0..MANY).map(|n| format!("w.{long}.{n}")).collect();
        test.define(names.iter().enumerate().map(|(n, channel)| {
            let key = 100 + u128::try_from(n).expect("fits");
            (key, channel.as_str(), DataType::Sample(I64), 1)
        }))
        .await;
        let (clock, hub) = (test.clock.clone(), test.hub.clone());
        test.tasks.spawn(async move {
            clock.sleep(START).await;
            hub.set_definitions(&channels());
        });
    };
    let home = read_session(309, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = agent.read(Mode::Complete, &["w.**"]).await;
        agent.sleep(Span::from_nanos(2 * START.nanos())).await;
        let end = loop {
            match stream.recv().await {
                Ok(Some(_)) => {}
                end => break end,
            }
        };
        assert_eq!(end, reset_with(UNKNOWN));
        finish(&mut agent).await;
    });
    let removed = reader::Ended::Removed(channel::Key::from_u128(100));
    assert_eq!(
        home.served,
        [Err(serve::Error::Ended(Box::new(removed))), Ok(Got::Ended)]
    );
}

/// A complete read whose program does not take its frames misses them live, so the
/// node sends each of them from disk, in order.
#[test]
fn sends_a_complete_read_that_misses_a_frame_each_frame_from_disk() {
    const FRAMES: i64 = 400;
    let ready = async |test: &Test| writes_wide(test, FRAMES).await;
    let home = read_session(308, ready, |mut agent| async move {
        agent.admit().await;
        let mut stream = agent.read(Mode::Complete, &["value"]).await;
        let (_, _, mut reader) = stream.opened().await;
        agent.sleep(Span::SECOND).await;
        let mut firsts = Vec::new();
        for _ in 0..FRAMES {
            let got = got(&mut stream.receiver, &mut reader).await;
            firsts.push(decoded(&got, &[I64, STAMP])[1][0]);
        }
        let expected: Vec<_> = (0..FRAMES).map(|n| firsts[0] + n * 1000).collect();
        assert_eq!(firsts, expected);
        stream.sender.finish().expect("finishes");
        assert_eq!(stream.recv().await, Ok(None));
        finish(&mut agent).await;
    });
    assert_eq!(home.served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// Runs one client session on a synced hub whose mesh names no home of `time`, and
/// gives what each stream served, then what `then` gave at [`START`].
fn homeless<T, P>(
    seed: u64,
    then: impl AsyncFnOnce(&mut Test) -> T + Send + 'static,
    program: impl FnOnce(Agent) -> P + Send + 'static,
) -> (Vec<Result<Got, serve::Error>>, T)
where
    T: Send + 'static,
    P: Future<Output = ()> + 'static,
{
    let (served, given) =
        (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(None)));
    let (kept, giving) = (Arc::clone(&served), Arc::clone(&given));
    let home = move |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let transport = Rc::new(transport(&node, &tasks, &own_pool(), HOME, 1 << 16));
        let region = Some(
            region::open(&node, &tasks, Rc::clone(&transport), Vec::new(), channels())
                .await,
        );
        let layout = buffer::Layout::new(AREA, BODY_MAX).expect("a ring");
        let mut test =
            Test::new(node.clone(), NODE, tasks.clone(), layout, POOL, region).await;
        test.sync().await;
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
        test.clock.sleep(START).await;
        *giving.lock().expect("not poisoned") = Some(then(&mut test).await);
        session.closed().await;
        recorded(&node, &kept, accepted.get()).await;
    };
    run_program(seed, home, |node, tasks, at| {
        as_agent(node, tasks, at, program)
    });
    let given = given.lock().expect("not poisoned").take().expect("given");
    let served = std::mem::take(&mut *served.lock().expect("not poisoned"));
    (served, given)
}

/// A read that waits for the home of an index that never gets one stops with
/// `UNKNOWN` at once when a spec in use removes its one channel, as a new read does.
#[test]
fn stops_a_read_whose_channel_is_removed_while_it_waits_for_a_home_as_unknown() {
    let (served, ()) = homeless(
        320,
        async |test| test.set_spec(without(&["value"])).await,
        |mut agent| async move {
            agent.admit().await;
            let mut stream = agent.read(Mode::Complete, &["value"]).await;
            assert_eq!(stream.recv().await, reset_with(UNKNOWN));
            finish(&mut agent).await;
        },
    );
    let empty = serve::Error::Reader(Box::new(reader::Error::Empty));
    assert_eq!(served, [Err(empty), Ok(Got::Ended)]);
}

/// A read that waits for the home of its index stops with `FAILED` when the mesh
/// stops.
#[test]
fn stops_a_read_on_a_stopped_mesh_as_failed() {
    let (served, stopped) = homeless(
        312,
        async |test| test.stop_mesh().await,
        |mut agent| async move {
            agent.admit().await;
            let mut stream = agent.read(Mode::Complete, &["value"]).await;
            assert_eq!(stream.recv().await, reset_with(FAILED));
            finish(&mut agent).await;
        },
    );
    let mesh = serve::Error::Reader(Box::new(reader::Error::Mesh(stopped)));
    assert_eq!(served, [Err(mesh), Ok(Got::Ended)]);
}

/// A read that waits for the home of its index still reads the program's stream: a
/// finish ends it, a message stops it as `MALFORMED`, and a reset ends it with the
/// error of the stream.
#[test]
fn ends_a_read_whose_program_finishes_or_breaks_its_stream_while_it_waits_for_a_home() {
    let (served, ()) = homeless(
        315,
        async |_| {},
        |mut agent| async move {
            agent.admit().await;
            let mut finished = agent.read(Mode::Complete, &["value"]).await;
            agent.sleep(QUIET).await;
            finished.sender.finish().expect("finishes");
            assert_eq!(finished.recv().await, Ok(None));
            let mut trailing = agent.read(Mode::Latest, &["value"]).await;
            agent.sleep(QUIET).await;
            trailing.send(b"x").await;
            assert_eq!(trailing.recv().await, reset_with(MALFORMED));
            let reset = agent.read(Mode::Complete, &["value"]).await;
            agent.sleep(QUIET).await;
            reset.sender.reset(transport::Code(7));
            agent.sleep(QUIET).await;
            finish(&mut agent).await;
        },
    );
    let trailing = serve::Error::Client(client::Error::Trailing);
    let reset = transport::Error::Reset {
        code: transport::Code(7),
    };
    assert_eq!(
        served,
        [
            Ok(Got::Ended),
            Err(trailing),
            Err(serve::Error::Stream(reset)),
            Ok(Got::Ended)
        ]
    );
}

/// A read holds the reservation of its open while it waits for the home of its index,
/// and frees it when it ends in the wait or when its open decodes after the wait. The
/// open of `value` is 8 bytes, so beside the held body one such open fits at a time.
#[test]
fn holds_the_share_of_the_open_of_a_read_while_it_waits_for_a_home() {
    let (served, ()) = homeless(
        317,
        async |test| test.set_home(TIME, NODE).await,
        |mut agent| async move {
            agent.admit().await;
            let mut waiting = agent.read(Mode::Complete, &["value"]).await;
            agent.sleep(Span::MILLISECOND).await;
            let held = agent.unfinished(BODY_BYTES_MAX - 8, &[]).await;
            agent.sleep(Span::MILLISECOND).await;
            let mut busy = agent.read(Mode::Complete, &["value"]).await;
            assert_eq!(busy.recv().await, reset_with(BUSY));
            waiting.sender.finish().expect("finishes");
            assert_eq!(waiting.recv().await, Ok(None));
            let mut streams = Vec::new();
            for _ in 0..2 {
                let mut stream = agent.read(Mode::Complete, &["value"]).await;
                let (places, run, _) = stream.opened().await;
                assert_eq!(places, Places { count: 2 });
                assert_eq!(indexes(&run), [1, 1]);
                streams.push(stream);
            }
            drop((held, streams));
            // So each reset reaches the home before the finish ends the session.
            agent.sleep(QUIET).await;
            finish(&mut agent).await;
        },
    );
    let share = serve::Error::Share {
        subject: name(SUBJECT),
        length: 8,
        held: BODY_BYTES_MAX,
    };
    let reset = serve::Error::Stream(transport::Error::Reset {
        code: transport::Code(0),
    });
    assert_eq!(
        served,
        [
            Err(share),
            Ok(Got::Ended),
            Err(reset.clone()),
            Err(reset.clone()),
            Err(reset),
            Ok(Got::Ended)
        ]
    );
}

/// A node serves a read only of an index whose home it is: a read whose home is
/// another node stops with `NOT_HOME` and interns no key set. One over `PLACES_MAX`
/// stops with `MALFORMED` before its home.
#[test]
fn stops_a_read_whose_home_is_another_node_as_not_home() {
    let (served, sets) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(None)));
    let (kept, counted) = (Arc::clone(&served), Arc::clone(&sets));
    let home = move |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let transport = Rc::new(transport(&node, &tasks, &own_pool(), HOME, 1 << 16));
        let region = Some(
            region::open(&node, &tasks, Rc::clone(&transport), Vec::new(), channels())
                .await,
        );
        let layout = buffer::Layout::new(AREA, BODY_MAX).expect("a ring");
        let mut test =
            Test::new(node.clone(), NODE, tasks.clone(), layout, POOL, region).await;
        test.sync().await;
        test.set_home(TIME, region::OTHER).await;
        test.set_home(TIME_B, NODE).await;
        test.hub.set_rules(rules());
        let names: Vec<String> = (1..PLACES_MAX).map(|n| format!("w.{n}")).collect();
        test.define(
            names.iter().zip(100..).map(|(channel, key)| {
                (key, channel.as_str(), DataType::Sample(I64), 1)
            }),
        )
        .await;
        let before = test.writer("a", &["value-b"]).await.set().key().get();
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
        session.closed().await;
        recorded(&node, &kept, accepted.get()).await;
        let after = test.writer("a", &["time-b"]).await.set().key().get();
        *counted.lock().expect("not poisoned") = Some((before, after));
    };
    run_program(314, home, |node, tasks, at| {
        as_agent(node, tasks, at, |mut agent| async move {
            agent.admit().await;
            let mut over = agent.read(Mode::Latest, &["w.**", "value"]).await;
            assert_eq!(over.recv().await, reset_with(MALFORMED));
            let mut stream = agent.read(Mode::Latest, &["value"]).await;
            assert_eq!(stream.recv().await, reset_with(NOT_HOME));
            finish(&mut agent).await;
        })
    });
    let remote = reader::Error::Remote {
        home: region::OTHER,
    };
    let over = serve::Error::Places {
        count: usize::try_from(PLACES_MAX).expect("fits") + 1,
    };
    let served = std::mem::take(&mut *served.lock().expect("not poisoned"));
    assert_eq!(
        served,
        [
            Err(over),
            Err(serve::Error::Reader(Box::new(remote))),
            Ok(Got::Ended)
        ]
    );
    let (before, after) = sets.lock().expect("not poisoned").take().expect("counted");
    assert_eq!(after, before + 1);
}

/// A client read that opens after the mesh puts in use a spec that adds `added`, and
/// before the spec task of the hub runs, opens on `added`.
#[test]
fn a_client_read_opens_on_a_channel_that_a_spec_in_use_adds() {
    let (served, ()) = homeless(
        318,
        async |test| {
            test.set_home(TIME, NODE).await;
            test.pause.pause();
            let mut added = channels();
            added.insert(
                name("added"),
                super::definition(9, DataType::Sample(I64), 1),
            );
            test.set_spec(added).await;
        },
        |mut agent| async move {
            agent.admit().await;
            agent
                .sleep(Span::from_nanos(10 * Span::SECOND.nanos()))
                .await;
            let mut stream = agent.read(Mode::Latest, &["added"]).await;
            let (places, run, _) = stream.opened().await;
            assert_eq!(places, Places { count: 2 });
            assert_eq!(indexes(&run), [1, 1]);
            drop(stream);
            finish(&mut agent).await;
        },
    );
    assert_eq!(served.len(), 2, "{served:?}");
}

/// A read stream that is open when the session of its client closes ends with that
/// close.
#[test]
fn ends_a_read_that_is_open_at_the_close_of_its_session_as_closed() {
    let (served, ()) = homeless(
        318,
        async |test| test.set_home(TIME, NODE).await,
        |mut agent| async move {
            agent.admit().await;
            let mut stream = agent.read(Mode::Latest, &["value"]).await;
            let (places, run, _) = stream.opened().await;
            assert_eq!(places, Places { count: 2 });
            assert_eq!(indexes(&run), [1, 1]);
            finish(&mut agent).await;
            drop(stream);
        },
    );
    let closed = serve::Error::Stream(transport::Error::Closed {
        code: transport::Code(0),
    });
    assert_eq!(served, [Ok(Got::Ended), Err(closed)]);
}
