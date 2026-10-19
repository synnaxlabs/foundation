//! A client write that the home serves, once warm.

use std::net::SocketAddr;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use env::tasks::Tasks;
use transport::Address;
use types::frame::Form;
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

/// A program writes frames of `value` that the node stamps, one at a time. Once warm,
/// the poll of the serve that takes a frame, writes it, and sends its outcome and the
/// `Stored` of each commit makes no heap allocation.
pub(crate) fn relays_a_frame_with_no_allocation() {
    let progress = Arc::new(Progress::default());
    let mut sim = sim::Sim::new(sim::Config::default());
    let [home, program] = [(); 2].map(|()| sim.node(sim::node::Config::default()));
    let at = Address::Udp(SocketAddr::new(home.addresses()[0], net::PORT));
    let config = env::shards::Config {
        name: "program".into(),
        core: None,
    };
    let (node, kept) = (program.clone(), Arc::clone(&progress));
    let main = move |tasks| write(node, tasks, at, kept);
    drop(program.shards().start(config, main).expect("starts"));
    sim.run_on(&home, move |node, tasks| serve(node, tasks, progress))
        .expect("the run ends");
}

/// The home: serves the program's write, and counts the poll that relays each frame.
async fn serve(node: sim::node::Node, tasks: Tasks, progress: Arc<Progress>) {
    let transport = net::transport(&node, &tasks, &net::own_pool(), net::HOME, 1 << 16);
    let (hub, _) = common::hub(&node, tasks.clone()).await;
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

/// The program: admits a hello, opens a write of `value`, and sends each frame of the
/// run once the outcome of the frame before it came.
async fn write(
    node: sim::node::Node,
    tasks: Tasks,
    at: Address,
    progress: Arc<Progress>,
) {
    let mut agent = Agent::dial(&node, tasks, at, common::name(SUBJECT)).await;
    agent.admit().await;
    let mut stream = agent.write(&["value"]).await;
    stream.started().await;
    progress.started.store(true, Ordering::Relaxed);
    let head = Head {
        time: Time::Node,
        form: Form::Raw,
        series: 2,
    };
    let mut body = [0; 16];
    for n in 0..WARM + COUNTED {
        body[8..].copy_from_slice(&n.to_le_bytes());
        stream.frame(head, &[1], &[(0, 8), (1, 16)], &body).await;
        progress.sent.fetch_add(1, Ordering::Relaxed);
        outcome(&mut stream, n).await;
        progress.taken.fetch_add(1, Ordering::Relaxed);
    }
}

/// Takes replies from `stream` up to the outcome of frame `n`, which the home takes.
async fn outcome(stream: &mut Stream, n: i64) {
    loop {
        match stream.reply().await {
            Reply::Stored { .. } => {}
            Reply::Outcome { frame, outcome, .. } => {
                let seq = u64::try_from(n).expect("a seq");
                let taken = wire::hub::write::Outcome::Taken {
                    range: types::frame::Range { seq, count: 1 },
                };
                assert_eq!(outcome, taken, "the outcome of frame {n}");
                assert_eq!(frame, seq + 1, "the frame of outcome {n}");
                return;
            }
        }
    }
}
