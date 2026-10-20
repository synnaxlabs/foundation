//! A client write that the home serves, once warm.

use std::net::SocketAddr;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use env::tasks::Tasks;
use spec::channel::{Channel, Data, Kind};
use spec::data_type::DataType;
use spec::definition::Definition;
use transport::Address;
use types::channel;
use types::frame::Form;
use types::sample::{Scalar, Type};
use types::time::Span;
use wire::hub::write::{Head, Reply, Time};

use crate::agent::{Agent, SUBJECT, Stream};
use crate::{ALLOCATOR, COUNTED, WARM, common, net, serving, sessions};

/// The longest wait for the program to take an outcome.
const TAKE: Span = Span::SECOND;
/// The time from a send of the program to the poll of the home. Longer than the 25 ms
/// for which a QUIC peer may hold an ack.
const SPACING: Span = Span::from_nanos(30_000_000);

/// What the program tells the home.
#[derive(Default)]
struct Progress {
    /// The program has the places of its write.
    started: AtomicBool,
    /// The frames that the program sent.
    sent: AtomicUsize,
    /// The outcomes that the program took.
    taken: AtomicUsize,
}

/// A program writes frames that the node stamps, one at a time: of `value` for one
/// group, else also of `value-b` on the index `time-b`, so that the serve copies each
/// group. Once warm, the poll of the serve that takes a frame, writes it, and sends
/// the outcome and the `Stored` of each group makes no heap allocation.
pub(crate) fn relays_a_frame_with_no_allocation(groups: Groups) {
    let progress = Arc::new(Progress::default());
    let mut sim = sim::Sim::new(sim::Config::default());
    let [home, program] = [(); 2].map(|()| sim.node(sim::node::Config::default()));
    let at = Address::Udp(SocketAddr::new(home.addresses()[0], net::PORT));
    let config = env::shards::Config {
        name: "program".into(),
        core: None,
    };
    let (node, kept) = (program.clone(), Arc::clone(&progress));
    let main = move |tasks| write(node, tasks, at, kept, groups);
    drop(program.shards().start(config, main).expect("starts"));
    sim.run_on(&home, move |node, tasks| serve(node, tasks, progress))
        .expect("the run ends");
}

/// The groups of the write.
#[derive(Clone, Copy)]
pub(crate) enum Groups {
    One,
    Two,
}

/// The home: serves the program's write, and counts the poll that relays each frame.
async fn serve(node: sim::node::Node, tasks: Tasks, progress: Arc<Progress>) {
    let transport = net::transport(&node, &tasks, &net::own_pool(), net::HOME, 1 << 16);
    let (hub, _) = common::hub(&node, tasks.clone()).await;
    let mut definitions = common::definitions(&[]);
    let time_b = Channel {
        key: channel::Key::from_u128(3),
        kind: Kind::Index {
            error: None,
            control: None,
        },
    };
    let i64 = DataType::Sample(Type::Scalar(Scalar::I64));
    let data = Data::new(time_b.key, None, i64, None).expect("no unit");
    let value_b = Channel {
        key: channel::Key::from_u128(4),
        kind: Kind::Data(data),
    };
    definitions.insert(common::name("time-b"), Definition::Channel(time_b));
    definitions.insert(common::name("value-b"), Definition::Channel(value_b));
    hub.set_definitions(&definitions);
    hub.set_rules(sessions::rules());
    let (program, link) = serving::program(&transport, &hub, &tasks).await;
    let mut write = pin!(link.serve(net::stream(&program).await));
    while !progress.started.load(Ordering::Relaxed) {
        serving::poll(write.as_mut());
        node.clock().sleep(Span::MILLISECOND).await;
    }
    for n in 0..WARM + COUNTED {
        let frames = usize::try_from(n + 1).expect("few frames");
        wait(&node, n, || progress.sent.load(Ordering::Relaxed) >= frames).await;
        node.clock().sleep(SPACING).await;
        let allocations = ALLOCATOR.count(|| serving::poll(write.as_mut())).1;
        wait(&node, n, || {
            progress.taken.load(Ordering::Relaxed) >= frames
        })
        .await;
        if n >= WARM {
            assert_eq!(allocations, 0, "frame {n} of a write allocated");
        }
    }
}

/// Waits until `done` holds for frame `n`.
///
/// # Panics
///
/// When it does not hold within [`TAKE`].
async fn wait(node: &sim::node::Node, n: i64, done: impl Fn() -> bool) {
    let end = node.clock().now() + TAKE;
    while !done() {
        assert!(node.clock().now() < end, "frame {n} did not go through");
        node.clock().sleep(Span::MILLISECOND).await;
    }
}

/// The program: admits a hello, opens a write of `groups`, and sends each frame of
/// the run once the outcomes of the frame before it came. The places of two groups are
/// `time`, `time-b`, `value`, and `value-b`.
async fn write(
    node: sim::node::Node,
    tasks: Tasks,
    at: Address,
    progress: Arc<Progress>,
    groups: Groups,
) {
    let mut agent = Agent::dial(&node, tasks, at, common::name(SUBJECT)).await;
    agent.admit().await;
    let (select, count): (&[&str], u32) = match groups {
        Groups::One => (&["value"], 1),
        Groups::Two => (&["value", "value-b"], 2),
    };
    let mut stream = agent.write(select).await;
    stream.started().await;
    progress.started.store(true, Ordering::Relaxed);
    let series = count * 2;
    let head = Head {
        time: Time::Node,
        form: Form::Raw,
        series,
    };
    let ends: Vec<_> = (0..series).map(|place| (place, (place + 1) * 8)).collect();
    let mut body = vec![0; ends.len() * 8];
    for n in 0..WARM + COUNTED {
        for value in body.chunks_mut(8).skip(ends.len() / 2) {
            value.copy_from_slice(&n.to_le_bytes());
        }
        stream.frame(head, &[], &ends, &body).await;
        progress.sent.fetch_add(1, Ordering::Relaxed);
        for group in 0..count {
            outcome(&mut stream, n, group).await;
        }
        progress.taken.fetch_add(1, Ordering::Relaxed);
    }
}

/// Takes replies from `stream` up to the outcome of `group` in frame `n`, which the
/// home takes.
async fn outcome(stream: &mut Stream, n: i64, group: u32) {
    loop {
        match stream.reply().await {
            Reply::Stored { .. } => {}
            reply @ Reply::Ended { .. } => panic!("the write ended: {reply:?}"),
            Reply::Outcome {
                frame,
                group: of,
                outcome,
            } => {
                let seq = u64::try_from(n).expect("a seq");
                let taken = wire::hub::write::Outcome::Taken {
                    range: types::frame::Range { seq, count: 1 },
                };
                assert_eq!(outcome, taken, "the outcome of frame {n}");
                assert_eq!((frame, of), (seq + 1, group), "the outcome of frame {n}");
                return;
            }
        }
    }
}
