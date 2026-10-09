//! What a call of `set_definitions` does to the sessions on each channel.

use std::path::Path as FilePath;
use std::pin::pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use hub::home::Outcome;
use hub::reader::{self, Ended, Mode};
use hub::writer::{self, Failure, Writer};
use spec::data_type::DataType;
use types::channel::Key;
use types::frame::{Form, Range};
use types::sample::{Scalar, Type};

use super::{
    I64, LIVE, RING, SETTLE, applied, channels, config, definition, entry, name,
    poll_flagged, poll_once, run, samples, without, write, write_series, written,
};

const I32: Type = Type::Scalar(Scalar::I32);

/// The seq of the one frame that `outcomes` applied.
fn seq(outcomes: &[Outcome]) -> u64 {
    let [Outcome::Applied { range, .. }] = outcomes else {
        panic!("{outcomes:?} is not one applied frame");
    };
    let Range { seq, count: 1 } = *range else {
        panic!("{range:?} is not one sample");
    };
    seq
}

#[test]
fn keeps_each_session_through_a_call_with_the_same_definitions() {
    run(4, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut latest = test.reader(&["value"], Mode::Latest).await;
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        assert_eq!(write(&mut writer, &[now], &[10]), [applied(0)]);
        test.hub.set_definitions(&channels());
        assert_eq!(write(&mut writer, &[now + 1], &[20]), [applied(1)]);
        for n in 0_i64..2 {
            let received = reader.next().await.expect("a frame");
            assert_eq!(samples(&received, 2), [(n + 1) * 10]);
        }
        let received = latest.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [20]);
    });
}

#[test]
fn ends_each_session_on_a_removed_data_channel_at_once() {
    run(30, |test| async move {
        let mut writer = test.writer("a", &["value"]).await;
        let mut complete = test.reader(&["value"], Mode::Complete).await;
        let mut latest = test.reader(&["value"], Mode::Latest).await;
        let mut index = test.reader(&["time"], Mode::Complete).await;
        let now = test.now();
        assert_eq!(write(&mut writer, &[now], &[10]), [applied(0)]);
        let received = complete.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [10]);
        let (polled, woken) = poll_flagged(pin!(complete.next()));
        assert!(polled.is_pending(), "no frame waits");
        test.hub.set_definitions(&without(&["value"]));
        assert!(
            woken.0.load(Ordering::Relaxed),
            "the removal wakes the reader"
        );
        let removed = Key::from_u128(2);
        let ended = complete.next().await.expect_err("the reader ended");
        assert_eq!(ended, Ended::Removed(removed));
        assert_eq!(
            ended.to_string(),
            "channel 00000000-0000-0000-0000-000000000002 was removed: open a new reader"
        );
        // The newest frame waits for the latest reader, which ends first.
        let ended = latest.next().await.expect_err("the reader ended");
        assert_eq!(ended, Ended::Removed(removed));
        let failure = written(&mut writer, &[(1, &[now + 1]), (2, &[20])]);
        let failure = failure.expect_err("the writer ended");
        assert_eq!(failure, Failure::Removed(removed));
        assert_eq!(
            failure.to_string(),
            "channel 00000000-0000-0000-0000-000000000002 was removed: open a new writer"
        );
        let error = test.hub.writer(config("b", &["value"])).await;
        assert_eq!(
            error.expect_err("unknown"),
            writer::Error::Unknown(name("value"))
        );
        let error = test.hub.reader(&[name("value")], Mode::Latest).await;
        assert_eq!(
            error.expect_err("unknown"),
            reader::Error::Unknown(name("value"))
        );
        // The removal closed the writer, so another takes control of the index.
        let mut other = test.writer("b", &["value-c"]).await;
        let outcomes = write_series(&mut other, &[(1, &[now + 1]), (5, &[30])]);
        assert_eq!(outcomes, [applied(1)]);
        for stamp in [now, now + 1] {
            let received = index.next().await.expect("a frame");
            assert_eq!(samples(&received, 1), [stamp]);
        }
        drop((writer, complete, latest));
    });
}

#[test]
fn ends_each_session_on_a_removed_index_and_continues_its_seq_when_it_returns() {
    run(31, |test| async move {
        let mut writer = test.writer("a", &["value-b"]).await;
        let mut reader = test.reader(&["value-b"], Mode::Complete).await;
        let mut index = test.reader(&["time-b"], Mode::Latest).await;
        let now = test.now();
        assert_eq!(
            seq(&write_series(&mut writer, &[(3, &[now]), (4, &[10])])),
            0
        );
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 4), [10]);
        test.hub.set_definitions(&without(&["time-b", "value-b"]));
        // The reader holds a frame of the shed index: its credit changes nothing.
        let ended = reader.next().await.expect_err("the reader ended");
        assert_eq!(ended, Ended::Removed(Key::from_u128(4)));
        let ended = index.next().await.expect_err("the reader ended");
        assert_eq!(ended, Ended::Removed(Key::from_u128(3)));
        let failure = written(&mut writer, &[(3, &[now + 1]), (4, &[20])]);
        assert_eq!(failure, Err(Failure::Removed(Key::from_u128(4))));
        drop((writer, reader, index));
        test.hub.set_definitions(&channels());
        let mut reader = test.reader(&["value-b"], Mode::Complete).await;
        let mut writer = test.writer("b", &["value-b"]).await;
        let outcomes = write_series(&mut writer, &[(3, &[now + 1]), (4, &[20])]);
        assert_eq!(seq(&outcomes), 1);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 4), [20]);
    });
}

#[test]
fn ends_the_sessions_of_a_renamed_channel_and_opens_sessions_on_its_new_name() {
    run(32, |test| async move {
        let mut writer = test.writer("a", &["value"]).await;
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let now = test.now();
        assert_eq!(write(&mut writer, &[now], &[10]), [applied(0)]);
        let mut renamed = channels();
        let value = renamed
            .remove(&name("value"))
            .expect("a channel of the test");
        renamed.insert(name("level"), value);
        test.hub.set_definitions(&renamed);
        let removed = Key::from_u128(2);
        let ended = reader.next().await.expect_err("the reader ended");
        assert_eq!(ended, Ended::Removed(removed));
        let failure = written(&mut writer, &[(1, &[now + 1]), (2, &[20])]);
        assert_eq!(failure, Err(Failure::Removed(removed)));
        let error = test.hub.writer(config("b", &["value"])).await;
        assert_eq!(
            error.expect_err("unknown"),
            writer::Error::Unknown(name("value"))
        );
        let mut reader = test.reader(&["level"], Mode::Complete).await;
        let mut writer = test.writer("b", &["level"]).await;
        assert_eq!(write(&mut writer, &[now + 1], &[20]), [applied(1)]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [20]);
    });
}

/// A data channel that moves to another index at the same key ends the sessions on
/// it, and the old index keeps its other sessions.
#[test]
fn ends_the_sessions_of_a_data_channel_that_moves_to_another_index() {
    run(33, |test| async move {
        let mut reader = test.reader(&["value"], Mode::Latest).await;
        let mut index = test.reader(&["time"], Mode::Latest).await;
        let mut moved = channels();
        moved.insert(name("value"), definition(2, DataType::Sample(I64), 3));
        test.hub.set_definitions(&moved);
        let ended = reader.next().await.expect_err("the reader ended");
        assert_eq!(ended, Ended::Removed(Key::from_u128(2)));
        let mut writer = test.writer("a", &["value-c"]).await;
        let now = test.now();
        let outcomes = write_series(&mut writer, &[(1, &[now]), (5, &[30])]);
        assert_eq!(outcomes, [applied(0)]);
        let received = index.next().await.expect("a frame");
        assert_eq!(samples(&received, 1), [now]);
        let writer = test.writer("b", &["value", "value-b"]).await;
        let entries = writer.set().entries();
        let keys: Vec<_> = entries.iter().map(|entry| entry.key.as_u128()).collect();
        assert_eq!(keys, [3, 2, 4]);
    });
}

/// A data channel whose index is renamed at the same key keeps its definition, but
/// each session on it holds the index, so each ends.
#[test]
fn ends_the_sessions_on_the_data_of_a_renamed_index() {
    run(35, |test| async move {
        let mut writer = test.writer("a", &["value"]).await;
        let mut reader = test.reader(&["value"], Mode::Complete).await;
        let mut renamed = channels();
        let time = renamed
            .remove(&name("time"))
            .expect("a channel of the test");
        renamed.insert(name("clock"), time);
        test.hub.set_definitions(&renamed);
        let removed = Key::from_u128(1);
        let ended = reader.next().await.expect_err("the reader ended");
        assert_eq!(ended, Ended::Removed(removed));
        let now = test.now();
        let failure = written(&mut writer, &[(1, &[now]), (2, &[10])]);
        assert_eq!(failure, Err(Failure::Removed(removed)));
    });
}

/// A call that panics on its definitions changes nothing.
#[test]
fn keeps_each_session_through_a_call_that_panics() {
    run(36, |test| async move {
        let mut writer = test.writer("a", &["value"]).await;
        let mut panicking = without(&["value"]);
        panicking.insert(name("other"), definition(9, DataType::Sample(I64), 8));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            test.hub.set_definitions(&panicking);
        }))
        .expect_err("the call panics");
        assert_eq!(
            panicked.downcast_ref::<String>().map(String::as_str),
            Some(
                "the index 00000000-0000-0000-0000-000000000008 of channel other is \
                 not an index of the definitions"
            )
        );
        let now = test.now();
        assert_eq!(write(&mut writer, &[now], &[10]), [applied(0)]);
    });
}

/// Writes `value` to `value` as an I32 at `stamp`.
fn write_i32(writer: &mut Writer, stamp: i64, value: i32) -> Vec<Outcome> {
    let set = writer.set();
    let (time, entry) = (entry(set, 1), entry(set, 2));
    let group = set.entries()[time].group;
    let mut draft = writer
        .draft(Form::Raw, &[(time, 8), (entry, 4)])
        .expect("a frame");
    let series = draft.series_mut(time).expect("the series is present");
    series.copy_from_slice(&stamp.to_le_bytes());
    let series = draft.series_mut(entry).expect("the series is present");
    series.copy_from_slice(&value.to_le_bytes());
    draft.set_count(group, 1);
    writer
        .write(LIVE, draft)
        .map(<[_]>::to_vec)
        .expect("the home takes it")
}

/// A latest reader on a channel whose type changed at its key takes no frame of the
/// old type: it waits for the next frame.
#[test]
fn gives_a_latest_reader_of_a_changed_channel_no_series_of_the_removed_one() {
    run(34, |test| async move {
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        assert_eq!(write(&mut writer, &[now], &[10]), [applied(0)]);
        let mut changed = channels();
        changed.insert(name("value"), definition(2, DataType::Sample(I32), 1));
        test.hub.set_definitions(&changed);
        let mut reader = test.reader(&["value"], Mode::Latest).await;
        assert!(poll_once(reader.next()).is_pending(), "no frame waits");
        let mut writer = test.writer("b", &["value"]).await;
        assert_eq!(write_i32(&mut writer, now + 1, 20), [applied(1)]);
        let received = reader.next().await.expect("a frame");
        let at = entry(received.set, 2);
        assert_eq!(received.set.entries()[at].data_type, I32);
        let (_, bytes) = received
            .view
            .iter()
            .find(|&(present, _)| present == at)
            .expect("the view holds the series");
        let mut out = [0; 4];
        codec::decode(I32, 1, bytes, &mut out).expect("decodes");
        assert_eq!(i32::from_le_bytes(out), 20);
    });
}

/// A data channel that moves to another index and back with a new type leaves no
/// frame of its old type on its first index.
#[test]
fn gives_a_latest_reader_no_series_of_a_channel_that_moved_away_and_back() {
    run(37, |test| async move {
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        assert_eq!(write(&mut writer, &[now], &[10]), [applied(0)]);
        drop(writer);
        let mut moved = channels();
        moved.insert(name("value"), definition(2, DataType::Sample(I64), 3));
        test.hub.set_definitions(&moved);
        let mut back = channels();
        back.insert(name("value"), definition(2, DataType::Sample(I32), 1));
        test.hub.set_definitions(&back);
        let mut reader = test.reader(&["value"], Mode::Latest).await;
        assert!(poll_once(reader.next()).is_pending(), "no frame waits");
    });
}

/// The ring bytes after 8 writers on `value` open and then end: by a removal of
/// `value` when `removed`, else by drops in open order.
fn ring_after_writers_end(removed: bool) -> Vec<u8> {
    let ring = Arc::new(Mutex::new(Vec::new()));
    let out = Arc::clone(&ring);
    run(34, move |test| async move {
        let mut writers = Vec::new();
        for n in 0..8 {
            writers.push(test.writer(&format!("w{n}"), &["value"]).await);
        }
        test.clock.sleep(SETTLE).await;
        if removed {
            test.hub.set_definitions(&without(&["value"]));
        } else {
            writers.into_iter().for_each(drop);
        }
        test.clock.sleep(SETTLE).await;
        let file = test
            .node
            .files()
            .open(FilePath::new(RING), env::files::Mode::Read)
            .await
            .expect("opens");
        let block = test.pool.alloc(1 << 16).expect("a block");
        let read = file.read_at(0, block).await.expect("reads");
        *out.lock().expect("not poisoned") = read.to_vec();
    });
    Arc::try_unwrap(ring)
        .expect("the run ended")
        .into_inner()
        .expect("not poisoned")
}

#[test]
fn closes_the_writers_of_a_removal_in_open_order() {
    let (removal, drops) =
        (ring_after_writers_end(true), ring_after_writers_end(false));
    assert!(removal == drops, "the handoff records differ");
}
