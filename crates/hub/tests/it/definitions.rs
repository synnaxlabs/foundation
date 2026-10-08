//! What a call of `set_definitions` does to the sessions on each channel.

use std::pin::pin;
use std::sync::atomic::Ordering;

use hub::home::Outcome;
use hub::reader::{self, Ended, Mode};
use hub::writer::{self, Failure};
use spec::data_type::DataType;
use types::channel::Key;
use types::frame::Range;

use super::{
    I64, applied, channels, config, definition, name, poll_flagged, run, samples,
    without, write, write_series, written,
};

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
