//! The per-frame cost of `program::Writer::draft` and `program::Writer::write` over a
//! sim transport. Run with `cargo bench -p program --bench write`.
//!
//! A node by hand opens a write for each line, takes each frame whole, and answers it
//! with `Taken` and then `Stored` for each group. The program times four calls for each
//! frame: the `draft`, the first poll of `write`, which sends the head, the ends, and
//! the body, once the outcome is in its transport, the second poll, which takes it, and
//! a poll of `stored`, which takes the `Stored`. Each line gives ns per frame:
//!
//! - `timer`: an empty closure, the floor of each timed call.
//! - `1x2`: an index and one data channel, one sample of each.
//! - `100x2`: an index and one data channel, 100 samples of each: a body of 1600
//!   bytes, over one segment of the carrier.
//! - `1x64`: an index and 63 data channels, one sample of each.
//! - `64x(1x2)`: 64 groups, each an index and one data channel, one sample of each.
//!
//! A second table gives the p99 of the single calls of each line, ns per frame: the
//! draft and the send, and the whole call. Judge a change by `net`, and compare two
//! builds on one pinned core whose SMT sibling is idle.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../../hub/tests/common/agent.rs"]
mod agent;
#[path = "../tests/common/connect.rs"]
mod connect;
#[expect(dead_code, reason = "the bench uses only some of the helpers")]
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

use std::pin::pin;
use std::task::{Context, Poll, Waker};

use program::write::{Config, Outcome, Time, Writer};
use table::Line;
use types::authority::Authority;
use types::channel;
use types::frame::{Form, Path, Range};
use types::sample::{Scalar, Type};
use types::time::Span;
use wire::hub::client::Place;
use wire::hub::client::write::Write;
use wire::hub::write::Reply;

use crate::hand::{accept_stream, by_hand, send, start};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// Frames per round.
const FRAMES: usize = 64;
const WARMUP: usize = 10;
const ROUNDS: usize = 50;
/// How long the program waits for the outcome of a frame to be in its transport.
const ANSWERED: Span = Span::from_nanos(10_000_000);

/// The frames of one line.
struct Shape {
    names: [&'static str; 4],
    /// The series of each frame, the index of each group first in the group.
    series: usize,
    /// The groups of each frame, each of `series / groups` series.
    groups: usize,
    /// The samples of each series.
    samples: u32,
}

const LINES: [Shape; 4] = [
    Shape {
        names: ["1x2 draft", "1x2 send", "1x2 outcome", "1x2 stored"],
        series: 2,
        groups: 1,
        samples: 1,
    },
    Shape {
        names: ["100x2 draft", "100x2 send", "100x2 outcome", "100x2 stored"],
        series: 2,
        groups: 1,
        samples: 100,
    },
    Shape {
        names: ["1x64 draft", "1x64 send", "1x64 outcome", "1x64 stored"],
        series: 64,
        groups: 1,
        samples: 1,
    },
    Shape {
        names: [
            "64x(1x2) draft",
            "64x(1x2) send",
            "64x(1x2) outcome",
            "64x(1x2) stored",
        ],
        series: 128,
        groups: 64,
        samples: 1,
    },
];

impl Shape {
    /// The places of the write: the index of each group first in the group.
    fn places(&self) -> Vec<Place> {
        let size = self.series / self.groups;
        (0..self.series)
            .map(|at| {
                let index = at - at % size;
                let key = u128::try_from(at + 1).expect("few");
                let data = if at == index {
                    Scalar::Stamp
                } else {
                    Scalar::I64
                };
                Place {
                    key: channel::Key::from_u128(key),
                    data_type: Type::Scalar(data),
                    name: format!("c{key}").parse().expect("a valid name"),
                    index,
                }
            })
            .collect()
    }

    /// The bytes of each series of a frame.
    fn len(&self) -> usize {
        8 * usize::try_from(self.samples).expect("few")
    }

    /// The bytes of the head, the ends, the counts, and the body of a frame.
    fn total(&self) -> usize {
        7 + 8 * self.series + 4 * self.groups + self.series * self.len()
    }
}

fn main() {
    by_hand(0, answer, |session, node| {
        Box::pin(async move {
            let mut timer = Line::new("timer", FRAMES);
            let mut lines = Vec::new();
            let mut calls = Vec::new();
            for shape in &LINES {
                let config = Config {
                    authority: Authority(1),
                    lease: None,
                    channels: shape
                        .places()
                        .iter()
                        .enumerate()
                        .filter(|&(at, place)| place.index != at)
                        .map(|(_, place)| place.name.clone())
                        .collect(),
                };
                let mut writer = session.writer(config).await.expect("opens");
                let layout: Vec<(usize, usize)> = (0..shape.series)
                    .map(|entry| (entry, shape.len()))
                    .collect();
                let [draft, sends, outcome, stored] = shape.names;
                let mut parts =
                    [draft, sends, outcome, stored].map(|n| Line::new(n, FRAMES));
                let mut out = Vec::with_capacity(ROUNDS * FRAMES);
                let mut whole = Vec::with_capacity(ROUNDS * FRAMES);
                for round in 0..WARMUP + ROUNDS {
                    for _ in 0..FRAMES {
                        timer.add(table::timed(&ALLOCATOR, || ()).1);
                        let [drafted, sent, taken, kept] =
                            frame(&mut writer, &node, shape, &layout).await;
                        if round >= WARMUP {
                            out.push(drafted.0 + sent.0);
                            whole.push(drafted.0 + sent.0 + taken.0);
                        }
                        for (line, figures) in
                            parts.iter_mut().zip([drafted, sent, taken, kept])
                        {
                            line.add(figures);
                        }
                    }
                    timer.close(round >= WARMUP);
                    for line in &mut parts {
                        line.close(round >= WARMUP);
                    }
                }
                lines.extend(parts);
                calls.push((draft, out));
                calls.push((outcome, whole));
            }
            let title = format!("ns per frame over {ROUNDS} rounds of {FRAMES} frames");
            table::print(&title, &timer, &lines);
            print_p99(&mut calls);
        })
    });
}

/// Times the calls of one frame: the draft, the send, the outcome, and the stored.
async fn frame(
    writer: &mut Writer,
    node: &sim::node::Node,
    shape: &Shape,
    layout: &[(usize, usize)],
) -> [(u64, u64); 4] {
    let mut context = Context::from_waker(Waker::noop());
    let (ready, drafted) = table::timed(&ALLOCATOR, || {
        let draft = pin!(writer.draft(Form::Raw, shape.samples, layout));
        matches!(draft.poll(&mut context), Poll::Ready(Ok(_)))
    });
    assert!(ready, "the pool has a block");
    let (sent, taken) = {
        let mut write = pin!(writer.write(Time::Frame(Path::Live)));
        let (pending, sent) = table::timed(&ALLOCATOR, || {
            write.as_mut().poll(&mut context).is_pending()
        });
        assert!(pending, "the write waits for its outcome");
        node.clock().sleep(ANSWERED).await;
        let (ready, taken) = table::timed(&ALLOCATOR, || {
            matches!(write.as_mut().poll(&mut context), Poll::Ready(Ok(_)))
        });
        assert!(ready, "the outcome is in the transport");
        (sent, taken)
    };
    let (ready, kept) = table::timed(&ALLOCATOR, || {
        let stored = pin!(writer.stored());
        matches!(stored.poll(&mut context), Poll::Ready(Ok(())))
    });
    assert!(ready, "the `Stored` is in the transport");
    [drafted, sent, taken, kept]
}

/// Opens the write of each line, takes each frame whole, and answers it with `Taken`
/// for each group and then `Stored` for each group.
async fn answer(session: transport::Session, node: sim::node::Node) {
    let mut writes = Vec::new();
    for shape in &LINES {
        let places = shape.places();
        let mut write = accept_stream(&session, |first| {
            Write::decode(first).expect("a write").body()
        })
        .await;
        start(&mut write, &places, places.len()).await;
        let mut seq = 0;
        for frame in 1..=(WARMUP + ROUNDS) * FRAMES {
            let mut got = 0;
            while got < shape.total() {
                let message = write.receiver.recv().await.expect("receives");
                got += message.expect("a message of the frame").len();
            }
            let frame = u64::try_from(frame).expect("a frame");
            let groups = 0..u32::try_from(shape.groups).expect("few");
            let taken = groups.clone().map(|group| Reply::Outcome {
                frame,
                group,
                outcome: Outcome::Taken {
                    range: Range {
                        seq,
                        count: shape.samples,
                    },
                },
            });
            let stored = groups.map(|group| Reply::Stored {
                group,
                frames: frame,
            });
            for reply in taken.chain(stored) {
                let mut bytes = vec![0; reply.encoded_len()];
                reply.encode(&mut bytes);
                send(write.sender.as_mut().expect("two-way"), &bytes).await;
            }
            seq += u64::from(shape.samples);
        }
        writes.push(write);
    }
    node.clock().sleep(ANSWERED).await;
}

/// Prints the p99 of the ns of each single call of each line: under the name of the
/// draft, the draft and the send; under the name of the outcome, the whole call.
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
