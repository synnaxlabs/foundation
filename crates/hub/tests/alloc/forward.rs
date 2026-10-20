//! A program's write that a member forwards to the home of its index, once warm.

use std::net::SocketAddr;
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use env::tasks::Tasks;
use transport::Address;
use types::channel;
use types::sample::{Scalar, Type};
use types::time::Span;
use wire::hub::write::{Outcome, Reply};

use crate::agent::{Stream, place};
use crate::forwarded::{Forwarded, send_places};
use crate::write::{Groups, Progress, SPACING, TAKE, wait, write};
use crate::{ALLOCATOR, COUNTED, WARM, common, net, region, serving, sessions};

/// The bytes of a message that the transport of each node takes.
const MESSAGE: usize = 1 << 16;

/// A program writes `value` at a member whose region names another node the home of
/// `time`, one frame at a time. Once warm, the polls of the member's serve that send
/// a frame on to the home and its outcome and `Stored` back make no heap allocation.
pub(crate) fn forwards_a_frame_with_no_allocation() {
    let progress = Arc::new(Progress::default());
    let mut sim = sim::Sim::new(sim::Config::default());
    let [member, home, program] =
        [(); 3].map(|()| sim.node(sim::node::Config::default()));
    let address = |node: &sim::node::Node| {
        Address::Udp(SocketAddr::new(node.addresses()[0], net::PORT))
    };
    let (at, other) = (address(&member), address(&home));
    let shard = |name: &str| env::shards::Config {
        name: name.into(),
        core: None,
    };
    let node = home.clone();
    let main = move |tasks| serve_home(node, tasks);
    drop(home.shards().start(shard("home"), main).expect("starts"));
    let (node, kept) = (program.clone(), Arc::clone(&progress));
    let main = move |tasks| write(node, tasks, at, kept, Groups::One);
    drop(
        program
            .shards()
            .start(shard("program"), main)
            .expect("starts"),
    );
    sim.run_on(&member, move |node, tasks| {
        serve(node, tasks, other, progress)
    })
    .expect("the run ends");
}

/// The member: forwards the program's write to the home at `other`, and counts the
/// polls that forward each frame and its replies.
async fn serve(
    node: sim::node::Node,
    tasks: Tasks,
    other: Address,
    progress: Arc<Progress>,
) {
    let transport = net::transport(&node, &tasks, &net::own_pool(), net::HOME, MESSAGE);
    let transport = Rc::new(transport);
    let definitions = common::definitions(&[]);
    let region = region::open(
        &node,
        &tasks,
        Rc::clone(&transport),
        vec![other],
        definitions,
    )
    .await;
    let mesh = region.mesh.clone();
    let hub = common::hub_in(&node, tasks.clone(), region).await;
    hub.set_rules(sessions::rules());
    let set = mesh
        .set_home(channel::Key::from_u128(1), region::OTHER)
        .await;
    set.expect("sets the home");
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
        let end = node.clock().now() + TAKE;
        let mut allocations = 0;
        while progress.taken.load(Ordering::Relaxed) < frames {
            assert!(node.clock().now() < end, "frame {n} did not go through");
            allocations += ALLOCATOR.count(|| serving::poll(write.as_mut())).1;
            node.clock().sleep(Span::MILLISECOND).await;
        }
        if n >= WARM {
            assert_eq!(allocations, 0, "frame {n} of a forward allocated");
        }
    }
}

/// The home at the node of [`region::OTHER`]: takes the first forward of the member,
/// sends the places of `time` and `value`, and answers each frame of the run with its
/// outcome and `Stored`.
async fn serve_home(node: sim::node::Node, tasks: Tasks) {
    let transport = net::transport(&node, &tasks, &net::own_pool(), net::PEER, MESSAGE);
    let session = transport.accept().await.expect("a session");
    let mut stream = Forwarded::accept(&session).await.stream;
    let places = [
        place(1, Type::Scalar(Scalar::Stamp), "time", 0),
        place(2, Type::Scalar(Scalar::I64), "value", 0),
    ];
    send_places(&mut stream, &places, MESSAGE).await;
    for seq in 0..u64::try_from(WARM + COUNTED).expect("few frames") {
        for _ in 0..3 {
            next(&mut stream).await;
        }
        let taken = Outcome::Taken {
            range: types::frame::Range { seq, count: 1 },
        };
        let replies = [
            Reply::Outcome {
                frame: seq + 1,
                group: 0,
                outcome: taken,
            },
            Reply::Stored {
                group: 0,
                frames: seq + 1,
            },
        ];
        for reply in replies {
            let mut out = vec![0; reply.encoded_len()];
            reply.encode(&mut out);
            stream.send(&out).await;
        }
    }
    // A drop now could reset the stream before the last replies reach the member.
    drop(stream.recv().await);
}

/// The next message of `stream`.
async fn next(stream: &mut Stream) -> Vec<u8> {
    let message = stream.recv().await.expect("receives");
    message.expect("a message before the finish")
}
