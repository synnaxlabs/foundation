//! The heap of a hub does not grow over many opens and closes of a named complete
//! reader. The count covers each thread, so this binary has no test harness. The sim
//! runs on one thread, so the count is exact.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[expect(dead_code, reason = "the test uses only some of the helpers")]
#[path = "../common/mod.rs"]
mod common;
#[path = "../common/node.rs"]
mod node;

use common::{hub, name};
use hub::reader::{self, Mode};
use types::name::Selector;
use types::time::Span;

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

/// The opens after which the heap is read the first time.
const FEW: usize = 10;
/// The opens after which the heap is read the last time.
const MANY: usize = 1000;

fn main() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let (hub, _) = hub(&node, tasks).await;
        let config = || reader::Config {
            select: Selector::new(["value"]).expect("a selector"),
            mode: Mode::Complete,
            subject: name("a"),
            name: Some(name("r")),
            hold: Span::SECOND,
        };
        let mut few = 0;
        for n in 1..=MANY {
            drop(hub.reader(config()).await.expect("opens"));
            if n == FEW {
                few = ALLOCATOR.held();
            }
        }
        assert_eq!(ALLOCATOR.held(), few, "bytes held after {MANY} opens");
    })
    .expect("the run ends");
}
