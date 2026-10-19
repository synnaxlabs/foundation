//! Sessions of a hub whose node is in a region, where the mesh names the home of each
//! index.

use std::collections::BTreeMap;
use std::future::poll_fn;
use std::path::Path;
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use env::files::Operation;
use hub::reader::Ended;
use hub::writer::Failure;
use hub::{Link, serve};
use spec::data_type::DataType;
use spec::definition::Definition;
use transport::stream::Incoming;
use transport::{Class, Code};
use types::channel;
use types::name::Name;
use types::time::Span;
use wire::hub::Mode;

use super::definitions::{I32, write_i32};
use super::serve::{Peer, session_in, stopped};
use super::{
    AREA, BODY_MAX, I64, NODE, POOL, SETTLE, TIME, TIME_B, Test, VALUE, applied,
    channels, config, definition, entry, keys, name, poll_once, reader, samples,
    unnamed, without, write, write_series, writer, written,
};
use crate::net::{HOME, PEER, own_pool, public_key};

#[path = "../common/region.rs"]
mod plant;
pub(super) use plant::{OTHER, open};

/// The first file of the mesh's log.
const LOG: &str = "mesh/log/log-0";

/// Runs `main` on a [`Test`] hub with mesh time, at [`NODE`] in the region of
/// [`open`].
fn run<F>(seed: u64, main: impl FnOnce(Test) -> F + Send + 'static)
where
    F: Future<Output = ()> + 'static,
{
    unsynced(seed, |mut test| async move {
        test.sync().await;
        main(test).await;
    });
}

/// As [`run`], with no mesh time until `main` calls [`Test::sync`].
fn unsynced<F>(seed: u64, main: impl FnOnce(Test) -> F + Send + 'static)
where
    F: Future<Output = ()> + 'static,
{
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, move |node, tasks| async move {
        let transport =
            crate::net::transport(&node, &tasks, &own_pool(), HOME, 1 << 16);
        let region =
            open(&node, &tasks, Rc::new(transport), Vec::new(), channels()).await;
        let layout = buffer::Layout::new(AREA, BODY_MAX).expect("a ring");
        main(Test::new(node, NODE, tasks, layout, POOL, Some(region)).await).await;
    })
    .expect("the run ends");
}

#[test]
fn a_writer_does_not_open_at_a_home_that_moved_while_it_waits_for_mesh_time() {
    unsynced(11, |mut test| async move {
        let hub = test.hub.clone();
        let mut opening = std::pin::pin!(hub.writer(config("a", &["value"])));
        assert!(poll_once(opening.as_mut()).is_pending());
        test.set_home(TIME, NODE).await;
        test.clock.sleep(Span::SECOND).await;
        assert!(poll_once(opening.as_mut()).is_pending());
        test.set_home(TIME, OTHER).await;
        let region = test.region.as_ref().expect("a region");
        let named = region.mesh.watch(TIME).next().await;
        assert_eq!(named, Ok(Some(OTHER)));
        test.sync().await;
        let opened = opening.await;
        let error = opened.expect_err("the mesh names OTHER as the home of time");
        assert_eq!(error, writer::Error::Remote { home: OTHER });
    });
}

/// A writer on two indexes checks the home of the first again once the second has one.
#[test]
fn a_writer_does_not_open_at_a_home_that_moved_while_it_waits_for_another_home() {
    run(13, |test| async move {
        test.set_home(TIME, NODE).await;
        let config = config("a", &["value", "value-b"]);
        let mut opening = std::pin::pin!(test.hub.writer(config));
        assert!(poll_once(opening.as_mut()).is_pending());
        test.set_home(TIME, OTHER).await;
        let region = test.region.as_ref().expect("a region");
        let named = region.mesh.watch(TIME).next().await;
        assert_eq!(named, Ok(Some(OTHER)));
        test.set_home(TIME_B, NODE).await;
        let opened = opening.await;
        let error = opened.expect_err("the mesh names OTHER as the home of time");
        assert_eq!(error, writer::Error::Remote { home: OTHER });
    });
}

#[test]
fn a_writer_does_not_open_at_a_home_that_moved_after_its_wait_in_the_same_pass() {
    run(14, |test| async move {
        let config = config("a", &["value", "value-b"]);
        let mut opening = std::pin::pin!(test.hub.writer(config));
        assert!(poll_once(opening.as_mut()).is_pending());
        test.set_home(TIME, NODE).await;
        assert!(poll_once(opening.as_mut()).is_pending());
        test.set_home(TIME, OTHER).await;
        let region = test.region.as_ref().expect("a region");
        let named = region.mesh.watch(TIME).next().await;
        assert_eq!(named, Ok(Some(OTHER)));
        test.set_home(TIME_B, NODE).await;
        let opened = opening.await;
        let error = opened.expect_err("the mesh names OTHER as the home of time");
        assert_eq!(error, writer::Error::Remote { home: OTHER });
    });
}

#[test]
fn a_writer_sees_its_channel_move_while_it_waits_for_mesh_time() {
    unsynced(16, |mut test| async move {
        test.set_home(TIME, NODE).await;
        test.set_home(TIME_B, OTHER).await;
        let hub = test.hub.clone();
        let mut opening = std::pin::pin!(hub.writer(config("a", &["value"])));
        assert!(poll_once(opening.as_mut()).is_pending());
        let mut moved = super::channels();
        moved.insert(
            name("value"),
            super::definition(2, DataType::Sample(I64), 3),
        );
        test.set_spec(moved).await;
        test.sync().await;
        let opened = opening.await;
        let error = opened.expect_err("the mesh names OTHER as the home of time-b");
        assert_eq!(error, writer::Error::Remote { home: OTHER });
    });
}

#[test]
fn opens_no_writer_on_a_channel_removed_while_it_waits_for_mesh_time() {
    unsynced(17, |mut test| async move {
        let hub = test.hub.clone();
        let mut opening = std::pin::pin!(hub.writer(config("a", &["value"])));
        assert!(poll_once(opening.as_mut()).is_pending());
        test.set_spec(super::without(&["time", "value", "value-c"]))
            .await;
        test.sync().await;
        test.clock.sleep(Span::from_nanos(5_000_000_000)).await;
        let std::task::Poll::Ready(opened) = poll_once(opening.as_mut()) else {
            panic!("the open still waits after its channel was removed");
        };
        let error = opened.expect_err("value was removed");
        assert_eq!(error, writer::Error::Unknown(name("value")));
    });
}

#[test]
fn a_writer_does_not_open_at_a_home_that_moved_while_it_waits_for_a_middle_home() {
    run(15, |test| async move {
        let mut defs = super::channels();
        defs.insert(
            name("time-c"),
            super::definition(6, DataType::Sample(super::STAMP), 6),
        );
        defs.insert(
            name("value-d"),
            super::definition(7, DataType::Sample(I64), 6),
        );
        test.set_spec(defs).await;
        let time_c = channel::Key::from_u128(6);
        test.set_home(TIME, NODE).await;
        test.set_home(time_c, NODE).await;
        let config = config("a", &["value", "value-b", "value-d"]);
        let mut opening = std::pin::pin!(test.hub.writer(config));
        assert!(poll_once(opening.as_mut()).is_pending());
        test.set_home(TIME, OTHER).await;
        let region = test.region.as_ref().expect("a region");
        let named = region.mesh.watch(TIME).next().await;
        assert_eq!(named, Ok(Some(OTHER)));
        test.set_home(TIME_B, NODE).await;
        let opened = opening.await;
        let error = opened.expect_err("the mesh names OTHER as the home of time");
        assert_eq!(error, writer::Error::Remote { home: OTHER });
    });
}

#[test]
fn a_writer_of_two_indexes_opens_after_an_index_it_left_moves_away() {
    run(19, |test| async move {
        let config = config("a", &["value", "value-b"]);
        let mut opening = std::pin::pin!(test.hub.writer(config));
        assert!(poll_once(opening.as_mut()).is_pending());
        test.set_home(TIME, NODE).await;
        assert!(poll_once(opening.as_mut()).is_pending());
        let mut defs = super::channels();
        defs.insert(
            name("time-c"),
            super::definition(6, DataType::Sample(super::STAMP), 6),
        );
        defs.insert(
            name("value"),
            super::definition(2, DataType::Sample(I64), 6),
        );
        test.set_spec(defs).await;
        test.set_home(channel::Key::from_u128(6), NODE).await;
        test.set_home(TIME, OTHER).await;
        let region = test.region.as_ref().expect("a region");
        let named = region.mesh.watch(TIME).next().await;
        assert_eq!(named, Ok(Some(OTHER)));
        test.set_home(TIME_B, NODE).await;
        let opened = opening.await;
        opened.expect("value is on time-c and value-b on time-b, both homed here");
    });
}

#[test]
fn a_writer_does_not_open_on_a_mesh_that_stopped_while_it_waits_for_mesh_time() {
    unsynced(12, |mut test| async move {
        let hub = test.hub.clone();
        let mut opening = std::pin::pin!(hub.writer(config("a", &["value"])));
        assert!(poll_once(opening.as_mut()).is_pending());
        test.set_home(TIME, NODE).await;
        test.clock.sleep(Span::SECOND).await;
        assert!(poll_once(opening.as_mut()).is_pending());
        let stopped = test.stop_mesh().await;
        test.sync().await;
        let opened = opening.await;
        let error = opened.expect_err("the mesh stopped");
        assert_eq!(error, writer::Error::Mesh(stopped));
    });
}

/// Puts in use on `mesh` a spec of `definitions`, and returns once the mesh uses it.
pub(super) async fn set_spec(
    mesh: &mesh::Mesh,
    definitions: BTreeMap<Name, Definition>,
) {
    let applied = mesh
        .apply(mesh.pointer(), definitions, BTreeMap::new())
        .await;
    let pointer = applied.expect("applies the spec");
    let used = mesh.spec().await.expect("the mesh runs");
    assert_eq!(used.pointer, Some(pointer));
}

impl Test {
    /// Sets `home` as the home of `index` in the region.
    pub(super) async fn set_home(&self, index: channel::Key, home: types::node::Key) {
        let region = self.region.as_ref().expect("a region");
        let set = region.mesh.set_home(index, home).await;
        set.expect("sets the home");
    }

    /// Puts in use a spec of `definitions` in the region ([`set_spec`]), and returns
    /// once each ready task, the hub's spec task too, has run.
    pub(super) async fn set_spec(&self, definitions: BTreeMap<Name, Definition>) {
        set_spec(&self.region.as_ref().expect("a region").mesh, definitions).await;
        self.clock.sleep(Span::from_nanos(1)).await;
    }

    /// Makes the mesh stop at its next write of the log, and gives why it stopped.
    pub(super) async fn stop_mesh(&self) -> mesh::Stopped {
        self.node.fail_file(Path::new(LOG), Operation::Sync);
        let region = self.region.as_ref().expect("a region");
        match region.mesh.set_home(TIME, NODE).await {
            Err(mesh::Error::Stopped(stopped)) => stopped,
            other => panic!("the mesh did not stop: {other:?}"),
        }
    }
}

#[test]
fn a_session_waits_for_the_first_home_and_the_home_carries_the_index() {
    run(1, |test| async move {
        let mut opening = std::pin::pin!(test.hub.writer(config("a", &["value"])));
        let names = ["value"];
        let mut reading =
            std::pin::pin!(test.hub.reader(unnamed(&names, reader::Mode::Complete)));
        test.clock.sleep(Span::SECOND).await;
        assert!(poll_once(opening.as_mut()).is_pending());
        assert!(poll_once(reading.as_mut()).is_pending());
        test.set_home(TIME, NODE).await;
        let mut writer = opening.await.expect("opens");
        let mut reader = reading.await.expect("opens");
        write(&mut writer, &[test.now()], &[7]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [7]);
    });
}

/// A session that waits for the home of its index finds a channel that a spec in use
/// removed meanwhile unknown.
#[test]
fn a_session_does_not_open_on_a_channel_removed_while_it_waits_for_a_home() {
    run(7, |test| async move {
        let mut opening = std::pin::pin!(test.hub.writer(config("a", &["value"])));
        let names = ["value"];
        let mut reading =
            std::pin::pin!(test.hub.reader(unnamed(&names, reader::Mode::Latest)));
        assert!(poll_once(opening.as_mut()).is_pending());
        assert!(poll_once(reading.as_mut()).is_pending());
        test.set_spec(without(&["value"])).await;
        test.set_home(TIME, NODE).await;
        let error = opening.await.expect_err("value was removed");
        assert_eq!(error, writer::Error::Unknown(name("value")));
        let error = reading.await.expect_err("value was removed");
        assert_eq!(error, reader::Error::Empty);
    });
}

/// A session that waits for the home of its index waits for the home of the new index
/// of a channel that a spec in use moved meanwhile, and opens on it.
#[test]
fn a_session_opens_on_the_new_index_of_a_channel_moved_while_it_waits_for_a_home() {
    run(8, |test| async move {
        let mut opening = std::pin::pin!(test.hub.writer(config("a", &["value"])));
        let names = ["value"];
        let mut reading =
            std::pin::pin!(test.hub.reader(unnamed(&names, reader::Mode::Latest)));
        assert!(poll_once(opening.as_mut()).is_pending());
        assert!(poll_once(reading.as_mut()).is_pending());
        let mut moved = channels();
        moved.insert(name("value"), definition(2, DataType::Sample(I64), 3));
        test.set_spec(moved).await;
        test.set_home(TIME, NODE).await;
        assert!(poll_once(opening.as_mut()).is_pending());
        assert!(poll_once(reading.as_mut()).is_pending());
        test.set_home(TIME_B, NODE).await;
        let mut writer = opening.await.expect("opens");
        let mut reader = reading.await.expect("opens");
        let now = test.now();
        write_series(&mut writer, &[(3, &[now]), (2, &[7])]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [7]);
    });
}

/// A writer whose channel moves while it waits for the home of the old index opens on
/// the new index when the mesh then names another node the home of the old one.
#[test]
fn a_writer_opens_on_the_new_index_when_the_old_one_moves_away_during_its_home_wait() {
    run(18, |test| async move {
        let mut opening = std::pin::pin!(test.hub.writer(config("a", &["value"])));
        assert!(poll_once(opening.as_mut()).is_pending());
        let mut moved = channels();
        moved.insert(name("value"), definition(2, DataType::Sample(I64), 3));
        test.set_spec(moved).await;
        test.set_home(TIME_B, NODE).await;
        test.set_home(TIME, OTHER).await;
        let mut writer = opening.await.expect("opens on time-b, homed at this node");
        let names = ["value"];
        let mut reader = test
            .hub
            .reader(unnamed(&names, reader::Mode::Latest))
            .await
            .expect("opens");
        let now = test.now();
        write_series(&mut writer, &[(3, &[now]), (2, &[7])]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(samples(&received, 2), [7]);
    });
}

/// A writer whose channel a spec in use moves while it waits for the home of an index
/// that never gets one opens on the new index once that index has its home.
#[test]
fn a_writer_opens_on_the_new_index_when_the_old_one_never_gets_a_home() {
    run(40, |test| async move {
        let mut opening = pin!(test.hub.writer(config("a", &["value"])));
        assert!(poll_once(opening.as_mut()).is_pending());
        let mut moved = channels();
        moved.insert(name("value"), definition(2, DataType::Sample(I64), 3));
        test.set_spec(moved).await;
        test.set_home(TIME_B, NODE).await;
        test.clock.sleep(Span::SECOND).await;
        let Poll::Ready(opened) = poll_once(opening.as_mut()) else {
            panic!("the open still waits for time, an index it no longer writes");
        };
        let mut writer = opened.expect("opens");
        let names = ["value"];
        let mut reader = test
            .hub
            .reader(unnamed(&names, reader::Mode::Latest))
            .await
            .expect("opens");
        let now = test.now();
        write_series(&mut writer, &[(3, &[now]), (2, &[7])]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(keys(&received), [3, 2]);
    });
}

/// As above, for a reader.
#[test]
fn a_reader_opens_on_the_new_index_when_the_old_one_never_gets_a_home() {
    run(41, |test| async move {
        let names = ["value"];
        let mut reading = pin!(test.hub.reader(unnamed(&names, reader::Mode::Latest)));
        assert!(poll_once(reading.as_mut()).is_pending());
        let mut moved = channels();
        moved.insert(name("value"), definition(2, DataType::Sample(I64), 3));
        test.set_spec(moved).await;
        test.set_home(TIME_B, NODE).await;
        test.clock.sleep(Span::SECOND).await;
        let Poll::Ready(opened) = poll_once(reading.as_mut()) else {
            panic!("the open still waits for time, an index it no longer reads");
        };
        let mut reader = opened.expect("opens");
        let mut writer = test.writer("a", &["value"]).await;
        let now = test.now();
        write_series(&mut writer, &[(3, &[now]), (2, &[7])]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(keys(&received), [3, 2]);
    });
}

/// A writer that waits for the home of an index that never gets one fails at once
/// with `Unknown` when a spec in use removes its channel.
#[test]
fn a_writer_fails_when_its_channel_is_removed_while_it_waits_for_a_home() {
    run(42, |test| async move {
        let mut opening = pin!(test.hub.writer(config("a", &["value"])));
        assert!(poll_once(opening.as_mut()).is_pending());
        test.set_spec(without(&["value"])).await;
        let Poll::Ready(opened) = poll_once(opening.as_mut()) else {
            panic!("the open still waits for the home of time");
        };
        let error = opened.expect_err("value was removed");
        assert_eq!(error, writer::Error::Unknown(name("value")));
    });
}

/// A reader that waits for the home of an index that never gets one fails at once
/// with `Empty` when a spec in use removes its one channel.
#[test]
fn a_reader_fails_when_its_one_channel_is_removed_while_it_waits_for_a_home() {
    run(43, |test| async move {
        let names = ["value"];
        let mut reading = pin!(test.hub.reader(unnamed(&names, reader::Mode::Latest)));
        assert!(poll_once(reading.as_mut()).is_pending());
        test.set_spec(without(&["value"])).await;
        let Poll::Ready(opened) = poll_once(reading.as_mut()) else {
            panic!("the open still waits for the home of time");
        };
        let error = opened.expect_err("value was removed");
        assert_eq!(error, reader::Error::Empty);
    });
}

/// A reader whose names match a channel that a spec in use adds on its index while it
/// waits for the home opens on that channel too.
#[test]
fn a_reader_opens_on_a_channel_added_while_it_waits_for_a_home() {
    run(45, |test| async move {
        let names = ["value", "value-x"];
        let mut reading = pin!(test.hub.reader(unnamed(&names, reader::Mode::Latest)));
        assert!(poll_once(reading.as_mut()).is_pending());
        let mut added = channels();
        added.insert(name("value-x"), definition(9, DataType::Sample(I64), 1));
        test.set_spec(added).await;
        assert!(poll_once(reading.as_mut()).is_pending());
        test.set_home(TIME, NODE).await;
        let mut reader = reading.await.expect("opens on value and value-x");
        let mut writer = test.writer("a", &names).await;
        let now = test.now();
        write_series(&mut writer, &[(1, &[now]), (2, &[7]), (9, &[8])]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(keys(&received), [1, 2, 9]);
        assert_eq!(samples(&received, 9), [8]);
    });
}

/// A reader of two channels on one index, one of which a spec in use removes while it
/// waits for the home, opens on the other.
#[test]
fn a_reader_opens_on_the_channel_left_when_one_is_removed_while_it_waits_for_a_home() {
    run(44, |test| async move {
        let names = ["value", "value-c"];
        let mut reading = pin!(test.hub.reader(unnamed(&names, reader::Mode::Latest)));
        assert!(poll_once(reading.as_mut()).is_pending());
        test.set_spec(without(&["value"])).await;
        assert!(poll_once(reading.as_mut()).is_pending());
        test.set_home(TIME, NODE).await;
        let mut reader = reading.await.expect("opens on value-c");
        let mut writer = test.writer("a", &["value-c"]).await;
        let now = test.now();
        write_series(&mut writer, &[(1, &[now]), (5, &[7])]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(keys(&received), [1, 5]);
        assert_eq!(samples(&received, 5), [7]);
    });
}

/// A session that waits for the home of its index opens on the sample type that a
/// call gave its channel meanwhile, on the slot of the new definition: a session that
/// opens after the wait sees its frames.
#[test]
fn a_session_opens_on_the_new_type_of_a_channel_changed_while_it_waits_for_a_home() {
    run(9, |test| async move {
        let mut opening = std::pin::pin!(test.hub.writer(config("a", &["value"])));
        let names = ["value"];
        let mut reading =
            std::pin::pin!(test.hub.reader(unnamed(&names, reader::Mode::Latest)));
        assert!(poll_once(opening.as_mut()).is_pending());
        assert!(poll_once(reading.as_mut()).is_pending());
        let mut changed = channels();
        changed.insert(name("value"), definition(2, DataType::Sample(I32), 1));
        test.set_spec(changed).await;
        test.set_home(TIME, NODE).await;
        let mut writer = opening.await.expect("opens");
        let mut reader = reading.await.expect("opens");
        let mut after = test.reader(&["value"], reader::Mode::Latest).await;
        assert_eq!(write_i32(&mut writer, test.now(), 20), [applied(0)]);
        for received in [after.next().await, reader.next().await] {
            let received = received.expect("a frame");
            assert_eq!(keys(&received), [1, 2]);
            let at = entry(received.set(), 2);
            assert_eq!(received.set().entries()[at].data_type, I32);
        }
    });
}

/// A session that waits for the home of its index opens on the key that a spec in use
/// gave its channel meanwhile, and a later removal of that key ends it.
#[test]
fn a_session_opens_on_the_new_key_of_a_channel_changed_while_it_waits_for_a_home() {
    run(10, |test| async move {
        let mut opening = std::pin::pin!(test.hub.writer(config("a", &["value"])));
        let names = ["value"];
        let mut reading =
            std::pin::pin!(test.hub.reader(unnamed(&names, reader::Mode::Latest)));
        assert!(poll_once(opening.as_mut()).is_pending());
        assert!(poll_once(reading.as_mut()).is_pending());
        let mut changed = channels();
        changed.insert(name("value"), definition(7, DataType::Sample(I64), 1));
        test.set_spec(changed).await;
        test.set_home(TIME, NODE).await;
        let mut writer = opening.await.expect("opens");
        let mut reader = reading.await.expect("opens");
        let now = test.now();
        write_series(&mut writer, &[(1, &[now]), (7, &[20])]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(keys(&received), [1, 7]);
        assert_eq!(samples(&received, 7), [20]);
        test.set_spec(without(&["value"])).await;
        let removed = channel::Key::from_u128(7);
        let failure = written(&mut writer, &[(1, &[now + 1]), (7, &[30])]);
        assert_eq!(failure, Err(Failure::Removed(removed)));
        let ended = reader.next().await.expect_err("the reader ended");
        assert_eq!(ended, Ended::Removed(removed));
    });
}

/// Names this node the home of `time`, holds the hub's tasks, and puts `definitions`
/// in use, so that only an open can apply the spec.
async fn held_spec(test: &Test, definitions: BTreeMap<Name, Definition>) {
    test.set_home(TIME, NODE).await;
    test.pause.pause();
    test.set_spec(definitions).await;
}

/// [`channels`] and `added`, a data channel on `time`.
fn added() -> BTreeMap<Name, Definition> {
    let mut added = channels();
    added.insert(name("added"), definition(9, DataType::Sample(I64), 1));
    added
}

#[test]
fn opens_a_writer_at_once_on_a_channel_that_a_spec_in_use_adds() {
    run(31, |test| async move {
        held_spec(&test, added()).await;
        let Poll::Ready(opened) = poll_once(test.hub.writer(config("a", &["added"])))
        else {
            panic!("the open waits");
        };
        opened.expect("opens");
    });
}

#[test]
fn opens_a_reader_at_once_on_a_channel_that_a_spec_in_use_adds() {
    run(32, |test| async move {
        held_spec(&test, added()).await;
        let config = unnamed(&["added"], reader::Mode::Latest);
        let Poll::Ready(opened) = poll_once(test.hub.reader(config)) else {
            panic!("the open waits");
        };
        opened.expect("opens");
    });
}

#[test]
fn opens_no_writer_on_a_channel_that_a_spec_in_use_removes() {
    run(33, |test| async move {
        held_spec(&test, without(&["value"])).await;
        let Poll::Ready(opened) = poll_once(test.hub.writer(config("a", &["value"])))
        else {
            panic!("the open waits");
        };
        let error = opened.expect_err("value was removed");
        assert_eq!(error, writer::Error::Unknown(name("value")));
    });
}

#[test]
fn opens_no_reader_on_a_channel_that_a_spec_in_use_removes() {
    run(34, |test| async move {
        held_spec(&test, without(&["value"])).await;
        let config = unnamed(&["value"], reader::Mode::Latest);
        let Poll::Ready(opened) = poll_once(test.hub.reader(config)) else {
            panic!("the open waits");
        };
        let error = opened.expect_err("value was removed");
        assert_eq!(error, reader::Error::Empty);
    });
}

#[test]
#[should_panic(
    expected = "a hub with a region takes its definitions from the region's mesh"
)]
fn set_definitions_on_a_hub_with_a_region_panics() {
    run(35, |test| async move {
        test.hub.set_definitions(&channels());
    });
}

/// The spec task, as the commit and gate tasks, ends only once the hub and each
/// session that outlives it drop.
#[test]
fn ends_the_spec_task_once_the_hub_and_each_session_drop() {
    run(36, |test| async move {
        test.set_home(TIME, NODE).await;
        let reader = test.reader(&["value"], reader::Mode::Complete).await;
        let writer = test.writer("a", &["value"]).await;
        let Test {
            clock,
            commit,
            hub,
            ended,
            region,
            ..
        } = test;
        drop((hub, commit));
        clock.sleep(SETTLE).await;
        assert_eq!(ended.get(), 0, "the sessions hold the state");
        drop((writer, reader));
        clock.sleep(SETTLE).await;
        assert_eq!(ended.get(), 3, "the commit, gate, and spec tasks ended");
        drop(region);
    });
}

/// The spec task ends when the mesh's group stops.
#[test]
fn ends_the_spec_task_when_the_mesh_stops() {
    run(38, |test| async move {
        test.clock.sleep(SETTLE).await;
        assert_eq!(test.ended.get(), 0, "each task runs");
        test.stop_mesh().await;
        test.clock.sleep(SETTLE).await;
        assert_eq!(test.ended.get(), 1, "the spec task ended");
    });
}

/// The spec task applies the last spec in use when the group stops before its next
/// poll, so a removal still ends the readers on the channel with no open.
#[test]
fn a_spec_in_use_just_before_the_mesh_stops_ends_the_readers_it_removes() {
    run(39, |test| async move {
        test.set_home(TIME, NODE).await;
        let mut reader = test.reader(&["value"], reader::Mode::Complete).await;
        test.clock.sleep(SETTLE).await;
        test.pause.pause();
        test.set_spec(without(&["value"])).await;
        test.stop_mesh().await;
        test.pause.resume();
        test.clock.sleep(SETTLE).await;
        let Poll::Ready(ended) = poll_once(reader.next()) else {
            panic!("the reader on a removed channel waits");
        };
        assert_eq!(ended.map(|_| ()), Err(Ended::Removed(VALUE)));
    });
}

#[test]
fn a_writer_with_an_index_whose_home_is_another_node_does_not_open() {
    run(2, |test| async move {
        test.set_home(TIME, NODE).await;
        test.set_home(TIME_B, OTHER).await;
        let opened = test.hub.writer(config("a", &["value", "value-b"])).await;
        let error = opened.expect_err("the home of time-b is the other node");
        assert_eq!(error, writer::Error::Remote { home: OTHER });
        assert_eq!(
            error.to_string(),
            "the home of an index of the writer is node \
             00000000-0000-0000-0000-000000000002, and a writer writes only at this \
             node"
        );
    });
}

#[test]
fn a_named_reader_does_not_open_at_the_home_of_another_node() {
    run(4, |test| async move {
        test.set_home(TIME, OTHER).await;
        let mut open = unnamed(&["value"], reader::Mode::Complete);
        open.name = Some(name("r"));
        let error = test.hub.reader(open).await.expect_err("the home is OTHER");
        assert_eq!(error, reader::Error::Remote { home: OTHER });
        assert_eq!(
            error.to_string(),
            "the home of the index of the reader is node \
             00000000-0000-0000-0000-000000000002, and this reader opens only at the \
             home"
        );
    });
}

#[test]
fn a_reader_of_an_index_at_a_member_with_no_address_does_not_reach_it() {
    run(3, |test| async move {
        test.set_home(TIME, OTHER).await;
        let names = ["value"];
        let opened = test.hub.reader(unnamed(&names, reader::Mode::Latest)).await;
        let error = opened.expect_err("the other node has no address");
        let unreachable = transport::Error::Unreachable {
            peer: Some(public_key(&PEER)),
            attempts: Vec::new(),
        };
        assert_eq!(error, reader::Error::Transport(unreachable));
        assert_eq!(
            error.to_string(),
            "the transport to the home failed: no address reached peer \
             8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394"
        );
    });
}

#[test]
fn a_session_on_a_stopped_mesh_gets_why_it_stopped() {
    run(4, |test| async move {
        let stopped = test.stop_mesh().await;
        let opened = test.hub.writer(config("a", &["value"])).await;
        let error = opened.expect_err("the mesh stopped");
        assert_eq!(error, writer::Error::Mesh(stopped.clone()));
        assert_eq!(error.to_string(), format!("the mesh stopped: {stopped}"));
        let names = ["value"];
        let opened = test.hub.reader(unnamed(&names, reader::Mode::Latest)).await;
        let error = opened.expect_err("the mesh stopped");
        assert_eq!(error, reader::Error::Mesh(stopped.clone()));
        assert_eq!(error.to_string(), format!("the mesh stopped: {stopped}"));
    });
}

/// Runs a session at [`NODE`] in the region, after `prepare` ran on the test and a
/// [`Pause`](super::Pause) that holds the polls of `serve`, and gives what `serve`
/// returned and the code that the peer saw on its stream.
fn served<F>(
    seed: u64,
    prepare: impl FnOnce(Rc<Test>, Rc<super::Pause>) -> F + Send + 'static,
) -> (Option<Result<(), serve::Error>>, Option<Code>)
where
    F: Future<Output = ()> + 'static,
{
    let (result, code) = (Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None)));
    let (kept, codes) = (Arc::clone(&result), Arc::clone(&code));
    let home = move |test: Test, link: Link, incoming: Incoming| async move {
        let test = Rc::new(test);
        let held = Rc::new(super::Pause::default());
        prepare(Rc::clone(&test), Rc::clone(&held)).await;
        let mut serving = pin!(super::serve::serve(&link, incoming));
        let served = poll_fn(|cx| held.poll(cx, serving.as_mut())).await;
        *kept.lock().expect("not poisoned") = Some(served);
    };
    let peer = move |mut peer: Peer| async move {
        let limit_bytes = 1 << 20;
        peer.open(Mode::Complete { limit_bytes }, &[1, 2]).await;
        let reply = peer.recv().await;
        let stopped = stopped(&mut peer).await;
        let transport::Error::Stopped { code } = stopped else {
            panic!("the home did not stop the stream: {stopped:?}");
        };
        assert_eq!(reply, Err(transport::Error::Reset { code }));
        *codes.lock().expect("not poisoned") = Some(code);
    };
    session_in(seed, Class::Complete, false, true, home, peer);
    let served = result.lock().expect("not poisoned").take();
    let code = *code.lock().expect("not poisoned");
    (served, code)
}

#[test]
fn stops_an_open_of_an_index_whose_home_is_another_node_with_not_home() {
    let (served, code) = served(5, |test, _| async move {
        test.set_home(TIME, OTHER).await;
    });
    assert_eq!(served, Some(Err(serve::Error::NotHome)));
    assert_eq!(code, Some(Code(wire::hub::NOT_HOME)));
    assert_eq!(
        serve::Error::NotHome.to_string(),
        "the mesh names another node as the home of the open's index"
    );
}

/// An open that waits for the home of its index stops with `UNKNOWN` when a spec in
/// use removed one of its channels meanwhile.
#[test]
fn stops_an_open_of_a_channel_removed_while_it_waits_for_a_home_with_unknown() {
    let (served, code) = served(7, |test, _| async move {
        let changing = Rc::clone(&test);
        test.tasks.spawn(async move {
            changing.clock.sleep(Span::SECOND).await;
            changing.set_spec(without(&["value"])).await;
            changing.set_home(TIME, NODE).await;
        });
    });
    let removed = serve::Error::Removed(channel::Key::from_u128(2));
    assert_eq!(served, Some(Err(removed)));
    assert_eq!(code, Some(Code(wire::hub::UNKNOWN)));
}

/// An open of `[time, value]` that waits for the home of `time`, while a spec in use
/// moves `value` to `time-b`, stops with `UNKNOWN`, as for a removal.
#[test]
fn stops_an_open_of_a_channel_moved_while_it_waits_with_unknown() {
    let (served, code) = served(7, |test, _| async move {
        let changing = Rc::clone(&test);
        test.tasks.spawn(async move {
            changing.clock.sleep(Span::SECOND).await;
            let mut moved = channels();
            moved.insert(name("value"), definition(2, DataType::Sample(I64), 3));
            changing.set_spec(moved).await;
            changing.set_home(TIME, NODE).await;
        });
    });
    let removed = serve::Error::Removed(channel::Key::from_u128(2));
    assert_eq!(served, Some(Err(removed)));
    assert_eq!(code, Some(Code(wire::hub::UNKNOWN)));
}

/// As above, while a spec in use changes the data type of `value`.
#[test]
fn stops_an_open_of_a_channel_retyped_while_it_waits_with_unknown() {
    let (served, code) = served(7, |test, _| async move {
        let changing = Rc::clone(&test);
        test.tasks.spawn(async move {
            changing.clock.sleep(Span::SECOND).await;
            let mut retyped = channels();
            let f64 = types::sample::Type::Scalar(types::sample::Scalar::F64);
            retyped.insert(name("value"), definition(2, DataType::Sample(f64), 1));
            changing.set_spec(retyped).await;
            changing.set_home(TIME, NODE).await;
        });
    });
    let removed = serve::Error::Removed(channel::Key::from_u128(2));
    assert_eq!(served, Some(Err(removed)));
    assert_eq!(code, Some(Code(wire::hub::UNKNOWN)));
}

/// As above, while a spec changes the data type of `value` after the home is named
/// and before the open runs again: the open reads its removal after the wait.
#[test]
fn stops_an_open_of_a_channel_retyped_after_its_home_is_named_with_unknown() {
    let (served, code) = served(7, |test, open| async move {
        let changing = Rc::clone(&test);
        test.tasks.spawn(async move {
            changing.clock.sleep(Span::SECOND).await;
            open.pause();
            changing.set_home(TIME, NODE).await;
            let mut retyped = channels();
            let f64 = types::sample::Type::Scalar(types::sample::Scalar::F64);
            retyped.insert(name("value"), definition(2, DataType::Sample(f64), 1));
            changing.set_spec(retyped).await;
            assert_eq!(open.resume(), 1, "the home woke the open");
        });
    });
    let removed = serve::Error::Removed(channel::Key::from_u128(2));
    assert_eq!(served, Some(Err(removed)));
    assert_eq!(code, Some(Code(wire::hub::UNKNOWN)));
}

/// An open that waits for the home of `time` stops when a spec in use removes `time`,
/// with no home named after the spec.
#[test]
fn stops_an_open_whose_index_is_removed_while_it_waits_with_unknown() {
    let (served, code) = served(7, |test, _| async move {
        let changing = Rc::clone(&test);
        test.tasks.spawn(async move {
            changing.clock.sleep(Span::SECOND).await;
            let removed = without(&["time", "value", "value-c"]);
            changing.set_spec(removed).await;
        });
    });
    assert_eq!(served, Some(Err(serve::Error::Removed(TIME))));
    assert_eq!(code, Some(Code(wire::hub::UNKNOWN)));
}

/// An open that waits stops with the channel of the first spec in use that removed
/// one of its channels.
#[test]
fn stops_an_open_that_waits_with_its_first_removed_channel() {
    let (served, code) = served(7, |test, _| async move {
        let changing = Rc::clone(&test);
        test.tasks.spawn(async move {
            changing.clock.sleep(Span::SECOND).await;
            changing.set_spec(without(&["value"])).await;
            let removed = without(&["time", "value", "value-c"]);
            changing.set_spec(removed).await;
        });
    });
    let removed = serve::Error::Removed(channel::Key::from_u128(2));
    assert_eq!(served, Some(Err(removed)));
    assert_eq!(code, Some(Code(wire::hub::UNKNOWN)));
}

/// A spec in use that defines `value` again as it was does not undo its removal by
/// the spec before it.
#[test]
fn stops_an_open_of_a_channel_removed_and_defined_again_while_it_waits_with_unknown() {
    let (served, code) = served(7, |test, _| async move {
        let changing = Rc::clone(&test);
        test.tasks.spawn(async move {
            changing.clock.sleep(Span::SECOND).await;
            changing.set_spec(without(&["value"])).await;
            changing.set_spec(channels()).await;
            changing.set_home(TIME, NODE).await;
        });
    });
    let removed = serve::Error::Removed(channel::Key::from_u128(2));
    assert_eq!(served, Some(Err(removed)));
    assert_eq!(code, Some(Code(wire::hub::UNKNOWN)));
}

#[test]
fn stops_a_served_open_on_a_channel_that_a_spec_in_use_removes_with_unknown() {
    let (served, code) = served(37, |test, _| async move {
        held_spec(&test, without(&["value"])).await;
    });
    let unknown = serve::Error::Unknown(channel::Key::from_u128(2));
    assert_eq!(served, Some(Err(unknown)));
    assert_eq!(code, Some(Code(wire::hub::UNKNOWN)));
}

#[test]
fn stops_an_open_on_a_stopped_mesh_with_failed() {
    let stopped = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&stopped);
    let (served, code) = served(6, move |test, _| async move {
        let stopped = test.stop_mesh().await;
        *kept.lock().expect("not poisoned") = Some(stopped);
    });
    let stopped = stopped
        .lock()
        .expect("not poisoned")
        .take()
        .expect("stopped");
    assert_eq!(served, Some(Err(serve::Error::Mesh(stopped.clone()))));
    assert_eq!(code, Some(Code(wire::hub::FAILED)));
    assert_eq!(
        serve::Error::Mesh(stopped.clone()).to_string(),
        format!("the mesh stopped: {stopped}")
    );
}

#[test]
fn a_waiting_writer_fails_on_a_spec_in_use_that_came_before_its_home() {
    run(46, |test| async move {
        let mut opening = pin!(test.hub.writer(config("a", &["value"])));
        assert!(poll_once(opening.as_mut()).is_pending());
        test.pause.pause();
        test.set_spec(without(&["value"])).await;
        test.set_home(TIME, NODE).await;
        let Poll::Ready(opened) = poll_once(opening.as_mut()) else {
            panic!("the open still waits");
        };
        let fresh = poll_once(test.hub.writer(config("b", &["value"])));
        let Poll::Ready(Err(fresh)) = fresh else {
            panic!("an open at that instant does not fail");
        };
        assert_eq!(fresh, writer::Error::Unknown(name("value")));
        match opened {
            Ok(_) => {
                panic!("the waiting writer opened on value, which the spec removed")
            }
            Err(error) => assert_eq!(error, writer::Error::Unknown(name("value"))),
        }
    });
}

#[test]
fn a_waiting_reader_matches_a_channel_of_a_spec_in_use_before_its_home() {
    run(47, |test| async move {
        let names = ["value", "value-x"];
        let mut reading = pin!(test.hub.reader(unnamed(&names, reader::Mode::Latest)));
        assert!(poll_once(reading.as_mut()).is_pending());
        test.pause.pause();
        let mut added = channels();
        added.insert(name("value-x"), definition(9, DataType::Sample(I64), 1));
        test.set_spec(added).await;
        test.set_home(TIME, NODE).await;
        let Poll::Ready(opened) = poll_once(reading.as_mut()) else {
            panic!("the open still waits");
        };
        let mut reader = opened.expect("opens");
        test.pause.resume();
        let mut writer = test.writer("a", &names).await;
        let now = test.now();
        write_series(&mut writer, &[(1, &[now]), (2, &[7]), (9, &[8])]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(keys(&received), [1, 2, 9]);
    });
}

#[test]
fn a_writer_woken_by_a_removal_opens_on_a_spec_in_use_that_adds_its_channel_back() {
    run(48, |test| async move {
        let mut opening = pin!(test.hub.writer(config("a", &["value"])));
        assert!(poll_once(opening.as_mut()).is_pending());
        test.pause.pause();
        test.set_spec(without(&["value"])).await;
        let removed = poll_once(test.hub.writer(config("b", &["value"])));
        let Poll::Ready(Err(removed)) = removed else {
            panic!("an open on a removed channel does not fail");
        };
        assert_eq!(removed, writer::Error::Unknown(name("value")));
        test.set_spec(channels()).await;
        test.set_home(TIME, NODE).await;
        let Poll::Ready(opened) = poll_once(opening.as_mut()) else {
            panic!("the open still waits");
        };
        opened.expect("opens on value, which the spec in use defines");
        let fresh = poll_once(test.hub.writer(config("c", &["value"])));
        assert!(matches!(fresh, Poll::Ready(Ok(_))), "a fresh open opens");
    });
}

#[test]
fn a_reader_woken_by_a_removal_opens_on_a_spec_in_use_that_adds_its_channel_back() {
    run(49, |test| async move {
        let latest = || unnamed(&["value"], reader::Mode::Latest);
        let mut reading = pin!(test.hub.reader(latest()));
        assert!(poll_once(reading.as_mut()).is_pending());
        test.pause.pause();
        test.set_spec(without(&["value"])).await;
        let removed = poll_once(test.hub.reader(latest()));
        let Poll::Ready(Err(removed)) = removed else {
            panic!("an open on a removed channel does not fail");
        };
        assert_eq!(removed, reader::Error::Empty);
        test.set_spec(channels()).await;
        test.set_home(TIME, NODE).await;
        let Poll::Ready(opened) = poll_once(reading.as_mut()) else {
            panic!("the open still waits");
        };
        opened.expect("opens on value, which the spec in use defines");
        let fresh = poll_once(test.hub.reader(latest()));
        assert!(matches!(fresh, Poll::Ready(Ok(_))), "a fresh open opens");
    });
}

#[test]
fn a_writer_waiting_for_mesh_time_opens_on_a_spec_in_use_that_moves_its_channel() {
    unsynced(50, |mut test| async move {
        test.set_home(TIME_B, NODE).await;
        let hub = test.hub.clone();
        let mut opening = pin!(hub.writer(config("a", &["value"])));
        assert!(poll_once(opening.as_mut()).is_pending());
        test.pause.pause();
        let mut moved = channels();
        moved.insert(name("value"), definition(2, DataType::Sample(I64), 3));
        test.set_spec(moved).await;
        test.sync().await;
        test.clock.sleep(Span::SECOND).await;
        let Poll::Ready(opened) = poll_once(opening.as_mut()) else {
            panic!("the open still waits for the home of the old index");
        };
        opened.expect("opens on time-b, which the spec in use names");
        let fresh = poll_once(test.hub.writer(config("b", &["value"])));
        assert!(matches!(fresh, Poll::Ready(Ok(_))), "a fresh open opens");
    });
}
