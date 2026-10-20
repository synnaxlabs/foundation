//! The per-frame cost of `program::Reader::next` over a sim transport. Run with
//! `cargo bench -p program --bench read`.
//!
//! A node by hand opens a complete read and sends `FRAMES` frames per round. The
//! program waits until they are in its transport, then times each call of `next`, one
//! poll each, which takes a block of the pool, the series bytes, and gives the frame.
//! The drop of the frame is timed too. The reader does the same work in each mode;
//! only the class of the stream differs. Each line gives ns per frame:
//!
//! - `timer`: an empty closure, the floor of each timed call.
//! - `narrow`: 64 channels on one index, whose frames hold one sample of each: 512
//!   bytes of body.
//! - `wide`: an index and one data channel of `SAMPLES` samples per frame: 4096 bytes
//!   of body.
//! - `gap`: the frames of `narrow`, each after one lost seq, so each call notes the gap
//!   for each channel and formats a warning into a buffer.
//!
//! A second table gives the p99 of the single calls of each line, ns per frame, for
//! the limit of a latest-mode hop. Judge a change by `net`, and compare two builds on
//! one pinned core whose SMT sibling is idle.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../../hub/tests/common/agent.rs"]
mod agent;
#[path = "../tests/common/connect.rs"]
mod connect;
#[path = "../tests/common/hand.rs"]
mod hand;
#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../tests/common/home.rs"]
mod home;
#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../../hub/tests/common/net.rs"]
mod net;
#[path = "../../hub/tests/common/node.rs"]
mod node;
#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../../hub/tests/common/sessions.rs"]
mod sessions;
#[path = "../../hub/benches/table/mod.rs"]
mod table;

use std::fmt::{self, Write};
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use table::Line;
use tracing::field::{Field, Visit};
use tracing::instrument::WithSubscriber;
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};
use types::channel;
use types::frame::Range;
use types::name::Selector;
use types::sample::{Scalar, Type};
use types::time::Span;
use wire::hub::client::{Mode, Place};

use crate::hand::{by_hand, opened, send_frame};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// Frames per round.
const FRAMES: usize = 64;
const WARMUP: usize = 10;
const ROUNDS: usize = 50;
/// Samples per series of a `wide` frame.
const SAMPLES: u32 = 256;
/// How long the program waits for a round of frames to be in its transport.
const SENT: Span = Span::from_nanos(100_000_000);
/// How long the node waits between two checks of what the program took.
const STEP: Span = Span::MILLISECOND;

/// The channels of a line.
#[derive(Clone, Copy)]
enum Shape {
    Narrow,
    Wide,
    Gap,
}

const LINES: [(&str, Shape); 3] = [
    ("narrow", Shape::Narrow),
    ("wide", Shape::Wide),
    ("gap", Shape::Gap),
];

impl Shape {
    /// The places of the read: the index first.
    fn places(self) -> Vec<Place> {
        let place = |key: u128, data_type| Place {
            key: channel::Key::from_u128(key),
            data_type,
            name: format!("c{key}").parse().expect("a valid name"),
            index: 0,
        };
        let data = match self {
            Self::Narrow | Self::Gap => 2..=64,
            Self::Wide => 2..=2,
        };
        std::iter::once(place(1, Type::Scalar(Scalar::Stamp)))
            .chain(data.map(|key| place(key, Type::Scalar(Scalar::I64))))
            .collect()
    }

    /// The selector of the places.
    fn select(self) -> Selector {
        let names: Vec<_> = self
            .places()
            .iter()
            .map(|place| place.name.to_string())
            .collect();
        Selector::new(names.iter().map(String::as_str)).expect("a selector")
    }

    /// The samples of each series of a frame.
    fn samples(self) -> u32 {
        match self {
            Self::Narrow | Self::Gap => 1,
            Self::Wide => SAMPLES,
        }
    }

    /// The seqs from the start of a frame to the start of the next.
    fn step(self) -> u64 {
        let lost = u64::from(matches!(self, Self::Gap));
        u64::from(self.samples()) + lost
    }

    /// The entry and the end of each series in the body.
    fn run(self) -> Vec<(u32, u32)> {
        let series = u32::try_from(self.places().len()).expect("few");
        (0..series)
            .map(|entry| (entry, (entry + 1) * self.samples() * 8))
            .collect()
    }
}

fn main() {
    let taken = Arc::new(AtomicUsize::new(0));
    let kept = Arc::clone(&taken);
    by_hand(
        0,
        move |session, node| send(session, node, kept),
        move |session, node| {
            Box::pin(
                async move {
                    let mut timer = Line::new("timer", FRAMES);
                    let mut lines = Vec::new();
                    let mut calls = Vec::new();
                    for (name, shape) in LINES {
                        let reader =
                            session.reader(shape.select(), Mode::Complete).await;
                        let mut reader = reader.expect("opens");
                        let mut line = Line::new(name, FRAMES);
                        let mut each = Vec::with_capacity(ROUNDS * FRAMES);
                        for round in 0..WARMUP + ROUNDS {
                            node.clock().sleep(SENT).await;
                            let mut context = Context::from_waker(Waker::noop());
                            for _ in 0..FRAMES {
                                timer.add(table::timed(&ALLOCATOR, || ()).1);
                                let (ready, figures) = table::timed(&ALLOCATOR, || {
                                    let next = pin!(reader.next()).poll(&mut context);
                                    matches!(next, Poll::Ready(Ok(_)))
                                });
                                assert!(
                                    ready,
                                    "each frame of {name} is in the transport"
                                );
                                if round >= WARMUP {
                                    each.push(figures.0);
                                }
                                line.add(figures);
                            }
                            taken.fetch_add(FRAMES, Ordering::Relaxed);
                            timer.close(round >= WARMUP);
                            line.close(round >= WARMUP);
                        }
                        lines.push(line);
                        calls.push((name, each));
                    }
                    let title =
                        format!("ns per frame over {ROUNDS} rounds of {FRAMES} frames");
                    table::print(&title, &timer, &lines);
                    print_p99(&mut calls);
                }
                .with_subscriber(Sink::default()),
            )
        },
    );
}

/// Sends the frames of each line once the program took those before them, as
/// `taken` counts.
async fn send(
    session: transport::Session,
    node: sim::node::Node,
    taken: Arc<AtomicUsize>,
) {
    for (line, (_, shape)) in LINES.into_iter().enumerate() {
        let mut read = opened(&session, &shape.places()).await;
        let run = shape.run();
        let body = vec![0; usize::try_from(run[run.len() - 1].1).expect("fits")];
        let mut seq = 0;
        for round in 0..WARMUP + ROUNDS {
            let start = (line * (WARMUP + ROUNDS) + round) * FRAMES;
            while taken.load(Ordering::Relaxed) < start {
                node.clock().sleep(STEP).await;
            }
            for _ in 0..FRAMES {
                let range = Range {
                    seq,
                    count: shape.samples(),
                };
                send_frame(&mut read, range, &run, &body, &[body.len()]).await;
                seq += shape.step();
            }
        }
        let end = (line + 1) * (WARMUP + ROUNDS) * FRAMES;
        while taken.load(Ordering::Relaxed) < end {
            node.clock().sleep(STEP).await;
        }
    }
}

/// Formats the message of each event into one buffer that it keeps.
#[derive(Default)]
struct Sink(Mutex<String>);

impl Subscriber for Sink {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut buffer = self.0.lock().expect("no event panics");
        buffer.clear();
        event.record(&mut Message(&mut buffer));
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

struct Message<'b>(&'b mut String);

impl Visit for Message<'_> {
    fn record_debug(&mut self, _: &Field, value: &dyn fmt::Debug) {
        write!(self.0, "{value:?}").expect("a String takes each write");
    }
}

/// Prints the p99 of the ns of each single call of each line.
#[expect(clippy::print_stdout, reason = "a benchmark prints its results")]
fn print_p99(lines: &mut [(&str, Vec<u64>)]) {
    println!("ns per single call");
    println!("{:<18} {:>9} {:>9}", "line", "p99", "max");
    for (name, nanos) in lines {
        nanos.sort_unstable();
        let p99 = nanos[nanos.len() * 99 / 100];
        let max = nanos[nanos.len() - 1];
        println!("{name:<18} {p99:>9} {max:>9}");
    }
}
