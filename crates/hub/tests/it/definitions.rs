//! What a call of `set_definitions` does to the sessions on each channel.

use std::collections::BTreeMap;
use std::path::Path as FilePath;
use std::pin::pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use hub::home::Outcome;
use hub::reader::{self, Ended, Mode};
use hub::writer::{self, Failure, Writer};
use spec::channel::{Channel, Data, Kind};
use spec::data_type::DataType;
use spec::definition::Definition;
use spec::unit::Unit;
use types::channel::Key;
use types::frame::{Form, Range};
use types::name::Name;
use types::sample::{Scalar, Type};

use super::{
    I64, LIVE, RING, SETTLE, applied, channels, config, definition, entry, keys, name,
    poll_flagged, run, samples, unnamed, without, write, write_series, written,
};

pub(super) const I32: Type = Type::Scalar(Scalar::I32);

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
        let error = test.hub.reader(unnamed(&["value"], Mode::Latest)).await;
        assert_eq!(error.expect_err("unknown"), reader::Error::Empty);
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
        // The move keeps the data slot of `value`, which is lower than the slot of
        // `time-b`.
        assert_eq!(keys, [2, 3, 4]);
    });
}

#[test]
fn ends_the_sessions_of_a_data_channel_whose_unit_changes() {
    run(47, |test| async move {
        let mut writer = test.writer("a", &["value"]).await;
        let unit = Unit::new("kPa").expect("a unit");
        let data =
            Data::new(Key::from_u128(1), None, DataType::Sample(I64), Some(unit));
        let value = Channel {
            key: Key::from_u128(2),
            kind: Kind::Data(data.expect("a number")),
        };
        let mut changed = channels();
        changed.insert(name("value"), Definition::Channel(value));
        test.hub.set_definitions(&changed);
        let now = test.now();
        let failure = written(&mut writer, &[(1, &[now]), (2, &[20])]);
        let failure = failure.expect_err("the writer ended");
        assert_eq!(failure, Failure::Removed(Key::from_u128(2)));
    });
}

#[test]
fn ends_the_sessions_of_a_data_channel_whose_quality_channel_changes() {
    run(49, |test| async move {
        let mut writer = test.writer("a", &["value"]).await;
        let quality = Some(Key::from_u128(5));
        let data = Data::new(Key::from_u128(1), quality, DataType::Sample(I64), None);
        let value = Channel {
            key: Key::from_u128(2),
            kind: Kind::Data(data.expect("no unit")),
        };
        let mut changed = channels();
        changed.insert(name("value"), Definition::Channel(value));
        test.hub.set_definitions(&changed);
        let now = test.now();
        let failure = written(&mut writer, &[(1, &[now]), (2, &[20])]);
        let failure = failure.expect_err("the writer ended");
        assert_eq!(failure, Failure::Removed(Key::from_u128(2)));
    });
}

/// The index `time` with the edges `error` and `control`.
fn index_with(error: Option<u128>, control: Option<u128>) -> Definition {
    Definition::Channel(Channel {
        key: Key::from_u128(1),
        kind: Kind::Index {
            error: error.map(Key::from_u128),
            control: control.map(Key::from_u128),
        },
    })
}

#[test]
fn ends_the_sessions_of_an_index_whose_error_or_control_edge_changes() {
    for (n, error, control) in [(48, None, Some(5)), (50, Some(5), None)] {
        run(n, move |test| async move {
            let mut reader = test.reader(&["time"], Mode::Latest).await;
            let mut changed = channels();
            changed.insert(name("time"), index_with(error, control));
            test.hub.set_definitions(&changed);
            let ended = reader.next().await.expect_err("the reader ended");
            assert_eq!(ended, Ended::Removed(Key::from_u128(1)));
        });
    }
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
pub(super) fn write_i32(writer: &mut Writer, stamp: i64, value: i32) -> Vec<Outcome> {
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

/// A latest reader on a channel whose type changed at its key takes the newest frame
/// at once, with no series of the old type, and the series of the next frame.
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
        let received = reader.next().await.expect("the newest frame");
        assert_eq!(keys(&received), [1]);
        assert_eq!(samples(&received, 1), [now]);
        let mut writer = test.writer("b", &["value"]).await;
        assert_eq!(write_i32(&mut writer, now + 1, 20), [applied(1)]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(keys(&received), [1, 2]);
        let at = entry(received.set(), 2);
        assert_eq!(received.set().entries()[at].data_type, I32);
        let (_, bytes) = received
            .view()
            .iter()
            .find(|&(present, _)| present == at)
            .expect("the view holds the series");
        let mut out = [0; 4];
        codec::decode(I32, 1, bytes, &mut out).expect("decodes");
        assert_eq!(i32::from_le_bytes(out), 20);
    });
}

/// A latest reader on a renamed channel takes the newest frame with the series written
/// under the old name: a rename keeps the history of the channel.
#[test]
fn gives_a_latest_reader_of_a_renamed_channel_the_series_of_its_old_name() {
    run(47, |test| async move {
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        assert_eq!(write(&mut writer, &[now], &[10]), [applied(0)]);
        let mut renamed = channels();
        let value = renamed
            .remove(&name("value"))
            .expect("a channel of the test");
        renamed.insert(name("level"), value);
        test.hub.set_definitions(&renamed);
        let mut reader = test.reader(&["level"], Mode::Latest).await;
        let received = reader.next().await.expect("the newest frame");
        assert_eq!(keys(&received), [1, 2]);
        assert_eq!(samples(&received, 2), [10]);
    });
}

/// A latest reader on a data channel that stays takes the newest frame of its index
/// at once, with its own series, after another channel of the index changed.
#[test]
fn gives_a_latest_reader_of_a_kept_channel_the_newest_frame_after_a_change() {
    run(38, |test| async move {
        let mut writer = test.writer("a", &["value", "value-c"]).await;
        let now = test.now();
        let outcomes =
            write_series(&mut writer, &[(1, &[now]), (2, &[10]), (5, &[30])]);
        assert_eq!(outcomes, [applied(0)]);
        let mut changed = channels();
        changed.insert(name("value"), definition(2, DataType::Sample(I32), 1));
        test.hub.set_definitions(&changed);
        let mut reader = test.reader(&["value-c"], Mode::Latest).await;
        let received = reader.next().await.expect("the newest frame");
        assert_eq!(keys(&received), [1, 5]);
        assert_eq!(samples(&received, 5), [30]);
    });
}

/// An index that is removed and returns gives a latest reader no newest frame from
/// before it was removed.
#[test]
fn gives_a_latest_reader_no_frame_of_an_index_from_before_it_was_removed() {
    run(44, |test| async move {
        let mut writer = test.writer("a", &["value-b"]).await;
        let now = test.now();
        write_series(&mut writer, &[(3, &[now]), (4, &[10])]);
        test.hub.set_definitions(&without(&["time-b", "value-b"]));
        drop(writer);
        test.hub.set_definitions(&channels());
        let mut reader = test.reader(&["value-b"], Mode::Latest).await;
        let (polled, _) = poll_flagged(pin!(reader.next()));
        assert!(
            polled.is_pending(),
            "the removed index left no newest frame"
        );
    });
}

/// A renamed index keeps its newest frame for a latest reader of its data.
#[test]
fn gives_a_latest_reader_the_newest_frame_of_a_renamed_index() {
    run(45, |test| async move {
        let mut writer = test.writer("a", &["value", "value-c"]).await;
        let now = test.now();
        let outcomes =
            write_series(&mut writer, &[(1, &[now]), (2, &[10]), (5, &[30])]);
        assert_eq!(outcomes, [applied(0)]);
        let mut renamed = channels();
        let time = renamed
            .remove(&name("time"))
            .expect("a channel of the test");
        renamed.insert(name("clock"), time);
        test.hub.set_definitions(&renamed);
        let mut reader = test.reader(&["value-c"], Mode::Latest).await;
        let received = reader.next().await.expect("the newest frame");
        assert_eq!(keys(&received), [1, 5]);
        assert_eq!(samples(&received, 5), [30]);
    });
}

/// A data channel that moves to another index and back with a new type leaves no
/// series of its old type on its first index.
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
        let received = reader.next().await.expect("the newest frame");
        assert_eq!(keys(&received), [1]);
    });
}

/// A data channel at the key of an index between two definitions of a data channel
/// there does not give the second one the series of the first.
#[test]
fn gives_a_latest_reader_no_series_of_a_data_channel_before_an_index_at_its_key() {
    run(43, |test| async move {
        let mut data = without(&["time-b", "value-b"]);
        data.insert(name("time-b"), definition(3, DataType::Sample(I64), 1));
        test.hub.set_definitions(&data);
        let mut writer = test.writer("a", &["time-b"]).await;
        let now = test.now();
        let outcomes = write_series(&mut writer, &[(1, &[now]), (3, &[10])]);
        assert_eq!(seq(&outcomes), 0);
        drop(writer);
        test.hub.set_definitions(&channels());
        let mut data = without(&["time-b", "value-b"]);
        data.insert(name("time-b"), definition(3, DataType::Sample(I32), 1));
        test.hub.set_definitions(&data);
        let mut reader = test.reader(&["time-b"], Mode::Latest).await;
        let received = reader.next().await.expect("the newest frame");
        assert_eq!(keys(&received), [1]);
    });
}

/// An index that becomes a data channel at its key and then an index again continues
/// its seq from the buffer, as an index that leaves and returns does.
#[test]
fn continues_the_seq_of_an_index_that_was_a_data_channel_between() {
    run(31, |test| async move {
        let mut writer = test.writer("a", &["value-b"]).await;
        let now = test.now();
        assert_eq!(
            seq(&write_series(&mut writer, &[(3, &[now]), (4, &[10])])),
            0
        );
        drop(writer);
        let mut data = without(&["time-b", "value-b"]);
        data.insert(name("time-b"), definition(3, DataType::Sample(I64), 1));
        test.hub.set_definitions(&data);
        test.hub.set_definitions(&channels());
        let mut writer = test.writer("b", &["value-b"]).await;
        let outcomes = write_series(&mut writer, &[(3, &[now + 1]), (4, &[20])]);
        assert_eq!(seq(&outcomes), 1);
    });
}

/// An index that becomes a data channel at its key and then an index again gives a
/// latest reader no newest frame from before it changed.
#[test]
fn gives_a_latest_reader_no_frame_of_an_index_that_was_a_data_channel_between() {
    run(46, |test| async move {
        let mut writer = test.writer("a", &["value-b"]).await;
        let now = test.now();
        write_series(&mut writer, &[(3, &[now]), (4, &[10])]);
        drop(writer);
        let mut data = without(&["time-b", "value-b"]);
        data.insert(name("time-b"), definition(3, DataType::Sample(I64), 1));
        test.hub.set_definitions(&data);
        test.hub.set_definitions(&channels());
        let mut reader = test.reader(&["value-b"], Mode::Latest).await;
        let (polled, _) = poll_flagged(pin!(reader.next()));
        assert!(
            polled.is_pending(),
            "the changed index left no newest frame"
        );
    });
}

/// The seq of a write on `value-b` after a restart, where the new hub gets `first`, if
/// any, before the definitions of `channels`.
fn seq_after_restart(n: u64, first: Option<BTreeMap<Name, Definition>>) -> u64 {
    let out = Arc::new(Mutex::new(None));
    let set = Arc::clone(&out);
    run(n, move |test| async move {
        let mut writer = test.writer("a", &["value-b"]).await;
        let now = test.now();
        assert_eq!(
            seq(&write_series(&mut writer, &[(3, &[now]), (4, &[10])])),
            0
        );
        test.clock.sleep(SETTLE).await;
        let super::Test {
            node,
            mut commit,
            hub,
            mesh,
            tasks,
            clock,
            ..
        } = test;
        drop((hub, writer));
        assert_eq!((&mut commit).await, Ok(()));
        drop(commit);
        clock.sleep(SETTLE).await;
        let hub = reopen(&node, tasks, mesh.clone()).await;
        if let Some(first) = first {
            hub.set_definitions(&first);
        }
        hub.set_definitions(&channels());
        let mut writer = hub.writer(config("b", &["value-b"])).await.expect("opens");
        let now = mesh.now().mesh.expect("mesh time");
        let now = now.earliest.nanos().midpoint(now.latest.nanos());
        let outcomes = write_series(&mut writer, &[(3, &[now]), (4, &[20])]);
        *set.lock().expect("not poisoned") = Some(seq(&outcomes));
    });
    out.lock().expect("not poisoned").expect("the run ended")
}

/// A hub on the ring that a hub of `node` left, as after a restart, with no
/// definitions.
async fn reopen(
    node: &sim::node::Node,
    tasks: env::tasks::Tasks,
    mesh: clock::Reader,
) -> hub::Hub {
    let budget = block::Config {
        budget: super::POOL,
    };
    let pool = std::rc::Rc::new(block::Pool::new(
        budget.clone(),
        block::Heap::new(budget.reservation()),
    ));
    let mut interner = types::frame::key_set::Interner::new();
    let layout = buffer::Layout::new(super::AREA, super::BODY_MAX).expect("a ring");
    let ring = buffer::Config {
        files: node.files(),
        dir: std::path::PathBuf::from(super::DIR),
        pool,
        clock: node.clock(),
        tasks: tasks.clone(),
        entropy: node.entropy(),
        layout,
        commit: super::COMMIT,
    };
    let buffer = buffer::Buffer::open(ring, interner.slots())
        .await
        .expect("opens");
    let home = home::Shard::new(home::Config {
        shard: 0,
        buffer,
        clock: mesh.clone(),
        limits: super::LIMITS,
    });
    hub::Hub::new(hub::Config {
        home,
        interner,
        tasks,
        node: super::NODE,
        time: mesh.clone(),
        entropy: node.entropy(),
        region: None,
    })
}

/// Control: an index defined at the first call after a restart continues its seq.
#[test]
fn continues_the_seq_of_an_index_after_a_restart() {
    assert_eq!(seq_after_restart(42, None), 1);
}

/// An index whose key is a data channel at the first call after a restart, and then
/// an index again, continues its seq from the buffer.
#[test]
fn continues_the_seq_of_an_index_that_is_a_data_channel_after_a_restart() {
    let mut data = without(&["time-b", "value-b"]);
    data.insert(name("time-b"), definition(3, DataType::Sample(I64), 1));
    assert_eq!(seq_after_restart(42, Some(data)), 1);
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
