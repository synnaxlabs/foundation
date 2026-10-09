//! A home shard on a new ring of a sim node, for the tests and benches that count or
//! time the hub or the home.

use std::path::PathBuf;
use std::rc::Rc;

use block::{Heap, Pool};
use env::tasks::Tasks;
use types::frame::key_set::Interner;
use types::name::Name;
use types::time::{Span, Stamp};

const COMMIT: Span = Span::from_nanos(10_000_000);
/// The area of the ring of the tests and of the `reader` and `woken` benches. The timed
/// rounds of each bench fit in it, and the `lost write` line of the `reader` bench fills
/// it. A commit takes whole 4 KiB blocks (one for a frame, three for 64), and
/// nothing frees the ring until #160, so a run fills it at 1023 one-frame commits.
pub(crate) const AREA: u64 = 1 << 22;

pub(crate) fn name(name: &str) -> Name {
    name.parse().expect("a valid name")
}

/// A home shard on a new ring of `area` bytes of `node`, its interner, the node's mesh
/// time now once it has one, and the reader of that time.
pub(crate) async fn shard(
    node: &sim::node::Node,
    tasks: Tasks,
    area: u64,
) -> (home::Shard, Interner, i64, clock::Reader) {
    let config = block::Config { budget: 1 << 23 };
    let pool = Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
    let (clock, mesh) = clock::Clock::new(node.clock());
    let wall = node.wall();
    tasks.spawn(async move { clock.run(wall).await });
    let mut interner = Interner::new();
    let config = buffer::Config {
        files: node.files(),
        dir: PathBuf::from("shard-0"),
        pool: Rc::clone(&pool),
        clock: node.clock(),
        tasks,
        entropy: node.entropy(),
        layout: buffer::Layout::new(area, 1 << 16).expect("a ring"),
        commit: COMMIT,
    };
    let buffer = buffer::Buffer::open(config, interner.slots())
        .await
        .expect("opens");
    let shard = home::Shard::new(home::Config {
        shard: 0,
        buffer,
        clock: mesh.clone(),
        limits: home::order::Limits {
            earliest: Stamp::from_nanos(1),
            ahead: Span::from_nanos(1_000_000_000),
        },
    });
    loop {
        if let Some(now) = mesh.now().mesh {
            return (shard, interner, now.latest.nanos(), mesh);
        }
        node.clock().sleep(Span::from_nanos(1)).await;
    }
}
