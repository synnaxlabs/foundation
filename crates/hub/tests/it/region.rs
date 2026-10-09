//! Sessions of a hub whose node is in a region, where the mesh names the home of each
//! index.

use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use env::files::Operation;
use env::tasks::Tasks;
use hub::{Link, serve};
use mesh::Member;
use mesh::card::addresses::Addresses;
use mesh::card::{self, Card};
use transport::stream::Incoming;
use transport::{Address, Class, Code, Transport};
use types::channel;
use types::ed25519::PrivateKey;
use types::node::SealKey;
use types::time::Span;
use wire::hub::Mode;

use super::serve::{HOME, PEER, Peer, own_pool, public_key, session_in, stopped};
use super::{
    AREA, BODY_MAX, NODE, POOL, TIME, TIME_B, Test, config, name, poll_once, reader,
    samples, write, writer,
};

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
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, move |node, tasks| async move {
        let transport =
            super::serve::transport(&node, &tasks, &own_pool(), HOME, 1 << 16);
        let region = open(&node, &tasks, Rc::new(transport), Vec::new()).await;
        let layout = buffer::Layout::new(AREA, BODY_MAX).expect("a ring");
        let mut test = Test::new(node, tasks, layout, POOL, Some(region)).await;
        test.sync().await;
        main(test).await;
    })
    .expect("the run ends");
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
