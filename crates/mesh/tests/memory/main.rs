//! The tests that bound the heap of `mesh` with one count of the bytes it holds. The
//! count covers each thread, so this binary has no test harness. The sim runs on one
//! thread, so the count is exact.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::PathBuf;
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Waker};

use env::tasks::Tasks;
use mesh::card::addresses::Addresses;
use mesh::card::{self, Card};
use mesh::status::Status;
use mesh::{Config, Error, Member, Mesh};
use sim::Sim;
use sim::node::Node;
use spec::definition::{Definition, Kind};
use spec::subject::Subject;
use transport::{Port, Transport};
use types::ed25519::PrivateKey;
use types::name::{Name, Prefix};
use types::node::{self, SealKey};
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

const KEY: node::Key = node::Key::from_u128(1);
const PRIVATE_KEY: PrivateKey = PrivateKey([1; 32]);
/// The applies after which the heap is read the first time.
const FEW: usize = 8;
/// The applies after which the heap is read the second time.
const MANY: usize = 264;

fn main() {
    many_applies_on_a_stale_base_hold_no_more_heap_than_applies_that_take_effect();
    dropped_calls_of_the_spec_hold_no_heap();
}

// The raft log keeps each entry in memory until #253, so the heap grows with each
// apply, and the test compares two runs. An apply that takes effect leaves no
// refusal, so it grows the heap by its entry alone.
fn many_applies_on_a_stale_base_hold_no_more_heap_than_applies_that_take_effect() {
    let stale = growth(Base::Founding);
    let taken = growth(Base::Pointer);
    assert!(
        stale <= taken,
        "{} applies on a stale base hold {stale} more bytes than {FEW}, and applies \
         that take effect hold {taken} more",
        MANY - FEW,
    );
}

// Each call waits for the read of the new pointer, which runs only after the loop
// yields, so no read wakes the calls in the loop.
fn dropped_calls_of_the_spec_hold_no_heap() {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let ran = sim.run_on(&node, move |node, tasks| async move {
        let mesh = Mesh::open(create_config(&node, &tasks).await)
            .await
            .expect("the mesh opens");
        node.clock()
            .sleep(Span::from_nanos(Span::SECOND.nanos()))
            .await;
        let applied = mesh.apply(mesh.pointer(), create_definitions("plant.app"));
        applied.await.expect("the change takes effect");
        let mut few = 0;
        for done in 1..=MANY {
            {
                let call = pin!(mesh.spec());
                let polled = call.poll(&mut Context::from_waker(Waker::noop()));
                assert!(polled.is_pending(), "the call waits for the read");
            }
            if done == FEW {
                few = ALLOCATOR.held();
            }
        }
        let many = ALLOCATOR.held();
        assert!(
            many <= few,
            "{} more dropped calls hold {} more bytes",
            MANY - FEW,
            many.saturating_sub(few),
        );
    });
    assert_eq!(ran, Ok(()), "the run ends");
}

/// The base of each apply.
#[derive(Clone, Copy)]
enum Base {
    /// The pointer of the founding spec, which the first apply makes stale.
    Founding,
    /// The pointer of the node at the call.
    Pointer,
}

/// The heap that the node holds after `MANY` applies on `base`, less the heap after
/// `FEW`.
fn growth(base: Base) -> usize {
    let held = Arc::new(Mutex::new(Vec::new()));
    let out = Arc::clone(&held);
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let ran = sim.run_on(&node, move |node, tasks| async move {
        let mesh = Mesh::open(create_config(&node, &tasks).await)
            .await
            .expect("the mesh opens");
        node.clock()
            .sleep(Span::from_nanos(Span::SECOND.nanos()))
            .await;
        let founding = mesh.pointer();
        let pointer = mesh.apply(founding, create_definitions("plant.app")).await;
        let pointer = pointer.expect("the first change takes effect");
        for done in 1..=MANY {
            match base {
                Base::Founding => {
                    let other = create_definitions("plant.other");
                    let stale = Error::Stale {
                        base: founding,
                        pointer,
                    };
                    let applied = mesh.apply(founding, other).await;
                    assert_eq!(applied, Err(stale), "the base is stale");
                }
                Base::Pointer => {
                    let name = ["plant.other", "plant.app"][done % 2];
                    let definitions = create_definitions(name);
                    let applied = mesh.apply(mesh.pointer(), definitions).await;
                    applied.expect("the change takes effect");
                }
            }
            if done == FEW || done == MANY {
                // The node reads the spec of each new pointer after the apply.
                drop(mesh.spec().await.expect("the mesh runs"));
                out.lock().expect("not poisoned").push(ALLOCATOR.held());
            }
        }
    });
    assert_eq!(ran, Ok(()), "the run ends");
    let held = held.lock().expect("not poisoned");
    let [few, many] = held[..] else {
        panic!("the heap is read twice, not {} times", held.len());
    };
    many.saturating_sub(few)
}

/// The subject `name` with the public key of the node.
fn create_definitions(name: &str) -> BTreeMap<Name, Definition> {
    let subject = Subject::new(vec![PRIVATE_KEY.public()]).expect("one key");
    let key = Kind::Subject.key(name).expect("a name");
    [(key, Definition::Subject(subject))].into()
}

/// The config of the region `plant`, whose one member and one voter is the node `KEY`.
async fn create_config(node: &Node, tasks: &Tasks) -> Config {
    let budget = block::Config { budget: 1 << 20 };
    let memory = block::Heap::new(budget.reservation());
    let pool = Rc::new(block::Pool::new(budget, memory));
    let at = SocketAddr::new(node.addresses()[0], 7000);
    let port = Port::bind(&node.net(), at).expect("a port");
    let mut parts = port.split(NonZeroUsize::MIN);
    let transport = transport::Config {
        private_key: PRIVATE_KEY,
        message_bytes_max: NonZeroUsize::new(1 << 16).expect("not zero"),
        window_bytes: 1 << 20,
        streams_max: NonZeroU32::new(16).expect("not zero"),
        idle: Span::from_nanos(60 * Span::SECOND.nanos()),
        clock: node.clock(),
        entropy: node.entropy(),
        tasks: tasks.clone(),
        pool: Rc::clone(&pool),
    };
    let transport =
        Transport::new(transport, parts.pop().expect("one part")).expect("a transport");
    let store = blob::Store::open(blob::Config {
        files: node.files(),
        dir: "blob".into(),
        pool: Rc::clone(&pool),
    })
    .await
    .expect("the store opens");
    let card = Card {
        name: "plant.node1".parse().expect("a name"),
        public_key: PRIVATE_KEY.public(),
        seal_key: SealKey::new([9; 32]).expect("a seal key"),
        addresses: Addresses::new(Vec::new()).expect("no address"),
        version: 1,
    };
    let member = Member {
        card: card::Signed::sign(KEY, card, &PRIVATE_KEY),
        admission: [0; 64],
        ephemeral: None,
        status: Status::new([].into()).expect("an empty status"),
    };
    Config {
        key: KEY,
        private_key: PRIVATE_KEY,
        region: "plant".parse::<Prefix>().expect("a prefix"),
        voters: [KEY].into(),
        members: vec![member],
        founding: BTreeMap::new(),
        files: node.files(),
        dir: PathBuf::new(),
        clock: node.clock(),
        entropy: node.entropy(),
        tasks: tasks.clone(),
        transport: Rc::new(transport),
        pool,
        store: Rc::new(store),
    }
}
