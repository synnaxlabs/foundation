//! The cost of a change of key set to a latest `hub` reader, on one shard of a sim
//! node. Run with `cargo bench -p hub --bench keyset`.
//!
//! Each case defines `time`, `value`, and `n` more channels `w.0` to `w.<n-1>` on
//! `time`. Before each frame, a writer opens, writes one frame, and closes, outside
//! the timed poll. The lines, each figured per frame:
//!
//! - `timer`: an empty closure, the floor of each line's figure.
//! - `one set <n>`: a reader of `w.**`, and one writer of half of `w.*` each time, so
//!   the key set does not change.
//! - `two sets <n>`: the same, with two writers that take turns, each of one half of
//!   `w.*`, so the key set changes at each frame.
//! - `sizes <n>`: a reader of `w.0`, and two writers that take turns, one of all of
//!   `w.*` and one of `w.0`, so each other key set is larger.
//! - `named <line>`: the same, and the reader names each entry of each view.
//!
//! Each line has its own hub. A debug build, which `cargo test` makes, runs one small
//! case, so a test run stays fast.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../tests/common/mod.rs"]
mod common;
#[path = "../tests/common/node.rs"]
mod node;
mod table;

use std::hint::black_box;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use common::{name, unnamed};
use hub::reader::{Mode, Reader};
use hub::writer::{self, Writer};
use table::Line;
use types::authority::Authority;
use types::frame::{Draft, Form, Label, Path};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// `n`, frames of a round, warm-up rounds, timed rounds.
type Case = (u64, usize, usize, usize);
const CASES: &[Case] = &[(64, 32, 5, 60), (256, 16, 4, 30), (1000, 8, 2, 16)];

/// How the writers of a line take turns.
#[derive(Clone, Copy, Debug)]
enum Turns {
    One,
    Two,
    Sizes,
}

fn main() {
    let cases: &[Case] = if cfg!(debug_assertions) {
        &[(8, 4, 1, 2)]
    } else {
        CASES
    };
    let (mut timer, mut lines) = (None, Vec::new());
    for &case in cases {
        for turns in [Turns::One, Turns::Two, Turns::Sizes] {
            for named in [false, true] {
                let mut sim = sim::Sim::new(sim::Config::default());
                let node = sim.node(sim::node::Config::default());
                let (floor, line) = sim
                    .run_on(&node, move |node, tasks| {
                        bench(node, tasks, case, turns, named)
                    })
                    .expect("the run ends");
                timer.get_or_insert(floor);
                lines.push(line);
            }
        }
    }
    let timer = timer.expect("a case");
    table::print("ns per frame", &timer, &lines);
}

async fn bench(
    node: sim::node::Node,
    tasks: env::tasks::Tasks,
    (n, frames, warmup, rounds): Case,
    turns: Turns,
    named: bool,
) -> (Line, Line) {
    let more: Vec<_> = (0..n).map(|at| name(&format!("w.{at}"))).collect();
    let hub = common::hub_alone(&node, tasks, &more).await;
    let half = more.len() / 2;
    let config = |at: usize, channels: &[types::name::Name]| writer::Config {
        subject: name(&format!("bench{at}")),
        authority: Authority(1),
        lease: None,
        channels: channels.to_vec(),
    };
    let (configs, select) = match turns {
        Turns::One => {
            let first = config(0, &more[..half]);
            ([first.clone(), first], "w.**")
        }
        Turns::Two => ([config(0, &more[..half]), config(1, &more[half..])], "w.**"),
        Turns::Sizes => ([config(0, &more), config(1, &more[..1])], "w.0"),
    };
    let kind = match turns {
        Turns::One => "one set",
        Turns::Two => "two sets",
        Turns::Sizes => "sizes",
    };
    let label = format!("{}{kind} {n}", if named { "named " } else { "" });
    let label: &'static str = Box::leak(label.into_boxed_str());
    let mut reader = hub
        .reader(unnamed(&[select], Mode::Latest))
        .await
        .expect("opens");
    let mut timer = Line::new("timer", frames);
    let mut line = Line::new(label, frames);
    let mut stamp = 0_i64;
    for round in 0..warmup + rounds {
        for frame in 0..frames {
            let mut writer =
                hub.writer(configs[frame % 2].clone()).await.expect("opens");
            let now = writer.now().nanos() + stamp;
            stamp += 1;
            let draft = draft(&writer, now);
            let outcome = writer.write(Label::Path(Path::Live), draft).expect("takes");
            assert!(
                matches!(outcome, [hub::home::Outcome::Applied { .. }]),
                "{label}: the ring has room"
            );
            drop(writer);
            timer.add(table::timed(&ALLOCATOR, || ()).1);
            line.add(take(&mut reader, named));
        }
        timer.close(round >= warmup);
        line.close(round >= warmup);
    }
    (timer, line)
}

fn draft(writer: &Writer, stamp: i64) -> Draft {
    let set = writer.set();
    let series: Vec<_> = (0..set.entries().len()).map(|entry| (entry, 8)).collect();
    let mut draft = writer.draft(Form::Raw, &series).expect("a draft");
    for (at, entry) in set.entries().iter().enumerate() {
        draft
            .series_mut(at)
            .expect("a series")
            .copy_from_slice(&stamp.to_le_bytes());
        draft.set_count(entry.group, 1);
    }
    draft
}

/// Takes a frame, called out of line so that no line measures how the open path in
/// the bench body inlines.
#[inline(never)]
fn take(reader: &mut Reader, named: bool) -> (u64, u64) {
    for _ in 0..2 {
        let (got, figures) = table::timed(&ALLOCATOR, || {
            let mut cx = Context::from_waker(Waker::noop());
            match pin!(reader.next()).poll(&mut cx) {
                Poll::Ready(Ok(received)) => {
                    for (entry, _) in received.view().iter() {
                        if named {
                            black_box(received.name(black_box(entry)));
                        } else {
                            black_box(entry);
                        }
                    }
                    true
                }
                Poll::Ready(Err(ended)) => panic!("the reader ended: {ended:?}"),
                Poll::Pending => false,
            }
        });
        if got {
            return figures;
        }
    }
    panic!("a frame waits for the reader");
}
