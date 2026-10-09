//! Sessions of a hub whose node is in a region, where the mesh names the home of each
//! index.

use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use env::files::Operation;
use env::tasks::Tasks;
use hub::reader::Ended;
use hub::writer::Failure;
use hub::{Link, serve};
use mesh::Member;
use mesh::card::addresses::Addresses;
use mesh::card::{self, Card};
use spec::data_type::DataType;
use transport::stream::Incoming;
use transport::{Address, Class, Code, Transport};
use types::channel;
use types::ed25519::PrivateKey;
use types::node::SealKey;
use types::time::Span;
use wire::hub::Mode;

use super::definitions::{I32, write_i32};
use super::serve::{Peer, session_in, stopped};
use super::{
    AREA, BODY_MAX, I64, NODE, POOL, TIME, TIME_B, Test, applied, channels, config,
    definition, entry, keys, name, poll_once, reader, samples, without, write,
    write_series, writer, written,
};
use crate::net::{HOME, PEER, own_pool, public_key};

/// The other member of the region, which is not a voter.
pub(super) const OTHER: types::node::Key = types::node::Key::from_u128(2);
/// The first file of the mesh's log.
const LOG: &str = "mesh/log/log-0";

/// Region `plant` at [`NODE`] over `transport`, whose one voter is [`NODE`] and whose
/// other member is [`OTHER`] at `other`.
pub(super) async fn open(
    node: &sim::node::Node,
    tasks: &Tasks,
    transport: Rc<Transport>,
    other: Vec<Address>,
) -> hub::Region {
    let pool = own_pool();
    let store = blob::Store::open(blob::Config {
        files: node.files(),
        dir: "blob".into(),
        pool: Rc::clone(&pool),
    })
    .await
    .expect("the store opens");
    let config = mesh::Config {
        key: NODE,
        private_key: HOME,
        founding: mesh::region::Founding {
            prefix: "plant".parse().expect("a prefix"),
            members: vec![member(NODE, &HOME, Vec::new()), member(OTHER, &PEER, other)],
            voters: [NODE].into(),
            definitions: BTreeMap::new(),
            homes: BTreeMap::new(),
        },
        files: node.files(),
        dir: "mesh".into(),
        clock: node.clock(),
        entropy: node.entropy(),
        tasks: tasks.clone(),
        pool,
        transport: Rc::clone(&transport),
        store: Rc::new(store),
    };
    let mesh = mesh::Mesh::open(config).await.expect("the mesh opens");
    hub::Region { mesh, transport }
}

/// The member `key` of region `plant` at `addresses`.
fn member(
    key: types::node::Key,
    private_key: &PrivateKey,
    addresses: Vec<Address>,
) -> Member {
    let card = Card {
        name: format!("plant.node{key}").parse().expect("a name"),
        public_key: public_key(private_key),
        seal_key: SealKey::new([9; 32]).expect("a seal key"),
        addresses: Addresses::new(addresses).expect("addresses"),
        version: 1,
    };
    Member {
        card: card::Signed::sign(key, card, private_key),
        admission: [0; 64],
        ephemeral: None,
        status: mesh::status::Status::new(BTreeMap::new()).expect("a status"),
    }
}

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
        let region = open(&node, &tasks, Rc::new(transport), Vec::new()).await;
        let layout = buffer::Layout::new(AREA, BODY_MAX).expect("a ring");
        main(Test::new(node, tasks, layout, POOL, Some(region)).await).await;
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

impl Test {
    /// Sets `home` as the home of `index` in the region.
    pub(super) async fn set_home(&self, index: channel::Key, home: types::node::Key) {
        let region = self.region.as_ref().expect("a region");
        let set = region.mesh.set_home(index, home).await;
        set.expect("sets the home");
    }

    /// Makes the mesh stop at its next write of the log, and gives why it stopped.
    async fn stop_mesh(&self) -> mesh::Stopped {
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
        let names = [name("value")];
        let mut reading =
            std::pin::pin!(test.hub.reader(&names, reader::Mode::Complete));
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

/// A session that waits for the home of its index finds a channel that a call removed
/// meanwhile unknown.
#[test]
fn a_session_does_not_open_on_a_channel_removed_while_it_waits_for_a_home() {
    run(7, |test| async move {
        let mut opening = std::pin::pin!(test.hub.writer(config("a", &["value"])));
        let names = [name("value")];
        let mut reading = std::pin::pin!(test.hub.reader(&names, reader::Mode::Latest));
        assert!(poll_once(opening.as_mut()).is_pending());
        assert!(poll_once(reading.as_mut()).is_pending());
        test.hub.set_definitions(&without(&["value"]));
        test.set_home(TIME, NODE).await;
        let error = opening.await.expect_err("value was removed");
        assert_eq!(error, writer::Error::Unknown(name("value")));
        let error = reading.await.expect_err("value was removed");
        assert_eq!(error, reader::Error::Unknown(name("value")));
    });
}

/// A session that waits for the home of its index waits for the home of the new index
/// of a channel that a call moved meanwhile, and opens on it.
#[test]
fn a_session_opens_on_the_new_index_of_a_channel_moved_while_it_waits_for_a_home() {
    run(8, |test| async move {
        let mut opening = std::pin::pin!(test.hub.writer(config("a", &["value"])));
        let names = [name("value")];
        let mut reading = std::pin::pin!(test.hub.reader(&names, reader::Mode::Latest));
        assert!(poll_once(opening.as_mut()).is_pending());
        assert!(poll_once(reading.as_mut()).is_pending());
        let mut moved = channels();
        moved.insert(name("value"), definition(2, DataType::Sample(I64), 3));
        test.hub.set_definitions(&moved);
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

/// A session that waits for the home of its index opens on the sample type that a
/// call gave its channel meanwhile, on the slot of the new definition: a session that
/// opens after the wait sees its frames.
#[test]
fn a_session_opens_on_the_new_type_of_a_channel_changed_while_it_waits_for_a_home() {
    run(9, |test| async move {
        let mut opening = std::pin::pin!(test.hub.writer(config("a", &["value"])));
        let names = [name("value")];
        let mut reading = std::pin::pin!(test.hub.reader(&names, reader::Mode::Latest));
        assert!(poll_once(opening.as_mut()).is_pending());
        assert!(poll_once(reading.as_mut()).is_pending());
        let mut changed = channels();
        changed.insert(name("value"), definition(2, DataType::Sample(I32), 1));
        test.hub.set_definitions(&changed);
        test.set_home(TIME, NODE).await;
        let mut writer = opening.await.expect("opens");
        let mut reader = reading.await.expect("opens");
        let mut after = test.reader(&["value"], reader::Mode::Latest).await;
        assert_eq!(write_i32(&mut writer, test.now(), 20), [applied(0)]);
        for received in [after.next().await, reader.next().await] {
            let received = received.expect("a frame");
            assert_eq!(keys(&received), [1, 2]);
            let at = entry(received.set, 2);
            assert_eq!(received.set.entries()[at].data_type, I32);
        }
    });
}

/// A session that waits for the home of its index opens on the key that a call gave
/// its channel meanwhile, and a later removal of that key ends it.
#[test]
fn a_session_opens_on_the_new_key_of_a_channel_changed_while_it_waits_for_a_home() {
    run(10, |test| async move {
        let mut opening = std::pin::pin!(test.hub.writer(config("a", &["value"])));
        let names = [name("value")];
        let mut reading = std::pin::pin!(test.hub.reader(&names, reader::Mode::Latest));
        assert!(poll_once(opening.as_mut()).is_pending());
        assert!(poll_once(reading.as_mut()).is_pending());
        let mut changed = channels();
        changed.insert(name("value"), definition(7, DataType::Sample(I64), 1));
        test.hub.set_definitions(&changed);
        test.set_home(TIME, NODE).await;
        let mut writer = opening.await.expect("opens");
        let mut reader = reading.await.expect("opens");
        let now = test.now();
        write_series(&mut writer, &[(1, &[now]), (7, &[20])]);
        let received = reader.next().await.expect("a frame");
        assert_eq!(keys(&received), [1, 7]);
        assert_eq!(samples(&received, 7), [20]);
        test.hub.set_definitions(&without(&["value"]));
        let removed = channel::Key::from_u128(7);
        let failure = written(&mut writer, &[(1, &[now + 1]), (7, &[30])]);
        assert_eq!(failure, Err(Failure::Removed(removed)));
        let ended = reader.next().await.expect_err("the reader ended");
        assert_eq!(ended, Ended::Removed(removed));
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
fn a_reader_of_an_index_at_a_member_with_no_address_does_not_reach_it() {
    run(3, |test| async move {
        test.set_home(TIME, OTHER).await;
        let names = [name("value")];
        let opened = test.hub.reader(&names, reader::Mode::Latest).await;
        let error = opened.expect_err("the other node has no address");
        let unreachable = transport::Error::Unreachable {
            peer: public_key(&PEER),
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
        let names = [name("value")];
        let opened = test.hub.reader(&names, reader::Mode::Latest).await;
        let error = opened.expect_err("the mesh stopped");
        assert_eq!(error, reader::Error::Mesh(stopped.clone()));
        assert_eq!(error.to_string(), format!("the mesh stopped: {stopped}"));
    });
}

/// Runs a session at [`NODE`] in the region, after `prepare` ran on the test, and gives
/// what `serve` returned and the code that the peer saw on its stream.
fn served<F>(
    seed: u64,
    prepare: impl FnOnce(Rc<Test>) -> F + Send + 'static,
) -> (Option<Result<(), serve::Error>>, Option<Code>)
where
    F: Future<Output = ()> + 'static,
{
    let (result, code) = (Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None)));
    let (kept, codes) = (Arc::clone(&result), Arc::clone(&code));
    let home = move |test: Test, link: Link, incoming: Incoming| async move {
        let test = Rc::new(test);
        prepare(Rc::clone(&test)).await;
        let served = super::serve::serve(&link, incoming).await;
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
    let (served, code) = served(5, |test| async move {
        test.set_home(TIME, OTHER).await;
    });
    assert_eq!(served, Some(Err(serve::Error::NotHome)));
    assert_eq!(code, Some(Code(wire::hub::NOT_HOME)));
    assert_eq!(
        serve::Error::NotHome.to_string(),
        "the mesh names another node as the home of the open's index"
    );
}

/// An open that waits for the home of its index stops with `UNKNOWN` when a call
/// removed one of its channels meanwhile.
#[test]
fn stops_an_open_of_a_channel_removed_while_it_waits_for_a_home_with_unknown() {
    let (served, code) = served(7, |test| async move {
        let changing = Rc::clone(&test);
        test.tasks.spawn(async move {
            changing.clock.sleep(Span::SECOND).await;
            changing.hub.set_definitions(&without(&["value"]));
            changing.set_home(TIME, NODE).await;
        });
    });
    let removed = serve::Error::Removed(channel::Key::from_u128(2));
    assert_eq!(served, Some(Err(removed)));
    assert_eq!(code, Some(Code(wire::hub::UNKNOWN)));
}

/// An open of `[time, value]` that waits for the home of `time`, while a call moves
/// `value` to `time-b`, stops with `UNKNOWN`, as for a removal.
#[test]
fn stops_an_open_of_a_channel_moved_while_it_waits_with_unknown() {
    let (served, code) = served(7, |test| async move {
        let changing = Rc::clone(&test);
        test.tasks.spawn(async move {
            changing.clock.sleep(Span::SECOND).await;
            let mut moved = channels();
            moved.insert(name("value"), definition(2, DataType::Sample(I64), 3));
            changing.hub.set_definitions(&moved);
            changing.set_home(TIME, NODE).await;
        });
    });
    let removed = serve::Error::Removed(channel::Key::from_u128(2));
    assert_eq!(served, Some(Err(removed)));
    assert_eq!(code, Some(Code(wire::hub::UNKNOWN)));
}

/// As above, while a call changes the data type of `value`.
#[test]
fn stops_an_open_of_a_channel_retyped_while_it_waits_with_unknown() {
    let (served, code) = served(7, |test| async move {
        let changing = Rc::clone(&test);
        test.tasks.spawn(async move {
            changing.clock.sleep(Span::SECOND).await;
            let mut retyped = channels();
            let f64 = types::sample::Type::Scalar(types::sample::Scalar::F64);
            retyped.insert(name("value"), definition(2, DataType::Sample(f64), 1));
            changing.hub.set_definitions(&retyped);
            changing.set_home(TIME, NODE).await;
        });
    });
    let removed = serve::Error::Removed(channel::Key::from_u128(2));
    assert_eq!(served, Some(Err(removed)));
    assert_eq!(code, Some(Code(wire::hub::UNKNOWN)));
}

/// As above, while a call changes the data type of `value` after the home is named
/// and before the open runs again: the open reads its removal after the wait.
#[test]
fn stops_an_open_of_a_channel_retyped_after_its_home_is_named_with_unknown() {
    let (served, code) = served(7, |test| async move {
        let changing = Rc::clone(&test);
        test.tasks.spawn(async move {
            changing.clock.sleep(Span::SECOND).await;
            changing.set_home(TIME, NODE).await;
            let mut retyped = channels();
            let f64 = types::sample::Type::Scalar(types::sample::Scalar::F64);
            retyped.insert(name("value"), definition(2, DataType::Sample(f64), 1));
            changing.hub.set_definitions(&retyped);
        });
    });
    let removed = serve::Error::Removed(channel::Key::from_u128(2));
    assert_eq!(served, Some(Err(removed)));
    assert_eq!(code, Some(Code(wire::hub::UNKNOWN)));
}

/// An open that waits for the home of `time` stops when a call removes `time`, with
/// no home named after the call.
#[test]
fn stops_an_open_whose_index_is_removed_while_it_waits_with_unknown() {
    let (served, code) = served(7, |test| async move {
        let changing = Rc::clone(&test);
        test.tasks.spawn(async move {
            changing.clock.sleep(Span::SECOND).await;
            let removed = without(&["time", "value", "value-c"]);
            changing.hub.set_definitions(&removed);
        });
    });
    assert_eq!(served, Some(Err(serve::Error::Removed(TIME))));
    assert_eq!(code, Some(Code(wire::hub::UNKNOWN)));
}

/// An open that waits stops with the channel of the first call that removed one of
/// its channels.
#[test]
fn stops_an_open_that_waits_with_its_first_removed_channel() {
    let (served, code) = served(7, |test| async move {
        let changing = Rc::clone(&test);
        test.tasks.spawn(async move {
            changing.clock.sleep(Span::SECOND).await;
            changing.hub.set_definitions(&without(&["value"]));
            let removed = without(&["time", "value", "value-c"]);
            changing.hub.set_definitions(&removed);
        });
    });
    let removed = serve::Error::Removed(channel::Key::from_u128(2));
    assert_eq!(served, Some(Err(removed)));
    assert_eq!(code, Some(Code(wire::hub::UNKNOWN)));
}

/// A call that defines `value` again as it was does not undo its removal by the call
/// before it.
#[test]
fn stops_an_open_of_a_channel_removed_and_defined_again_while_it_waits_with_unknown() {
    let (served, code) = served(7, |test| async move {
        let changing = Rc::clone(&test);
        test.tasks.spawn(async move {
            changing.clock.sleep(Span::SECOND).await;
            changing.hub.set_definitions(&without(&["value"]));
            changing.hub.set_definitions(&channels());
            changing.set_home(TIME, NODE).await;
        });
    });
    let removed = serve::Error::Removed(channel::Key::from_u128(2));
    assert_eq!(served, Some(Err(removed)));
    assert_eq!(code, Some(Code(wire::hub::UNKNOWN)));
}

#[test]
fn stops_an_open_on_a_stopped_mesh_with_failed() {
    let stopped = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&stopped);
    let (served, code) = served(6, move |test| async move {
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
