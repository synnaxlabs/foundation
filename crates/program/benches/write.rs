//! The per-frame cost of `program::Writer::draft` and `program::Writer::write` over a
//! sim transport. Run with `cargo bench -p program --bench write`.
//!
//! A node by hand opens a write for each line, takes each frame whole, and answers it
//! with `Taken`. The program times three calls for each frame: the `draft`, the first
//! poll of `write`, which sends the head, the ends, and the body, and, once the
//! outcome is in its transport, the second poll, which takes it. Each line gives ns
//! per frame:
//!
//! - `timer`: an empty closure, the floor of each timed call.
//! - `1x2`: an index and one data channel, one sample of each.
//! - `100x2`: an index and one data channel, 100 samples of each: a body of 1600
//!   bytes, over one segment of the carrier.
//! - `1x64`: an index and 63 data channels, one sample of each.
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

use program::write::{Config, Outcome, Time};
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
    names: [&'static str; 3],
    /// The series of each frame, the index first.
    series: usize,
    /// The samples of each series.
    samples: u32,
}

const LINES: [Shape; 3] = [
    Shape {
        names: ["1x2 draft", "1x2 send", "1x2 outcome"],
        series: 2,
        samples: 1,
    },
    Shape {
        names: ["100x2 draft", "100x2 send", "100x2 outcome"],
        series: 2,
        samples: 100,
    },
    Shape {
        names: ["1x64 draft", "1x64 send", "1x64 outcome"],
        series: 64,
        samples: 1,
    },
];

impl Shape {
    /// The places of the write: the index first.
    fn places(&self) -> Vec<Place> {
        let place = |key: u128, data_type| Place {
            key: channel::Key::from_u128(key),
            data_type,
            name: format!("c{key}").parse().expect("a valid name"),
            index: 0,
        };
        let series = u128::try_from(self.series).expect("few");
        std::iter::once(place(1, Type::Scalar(Scalar::Stamp)))
            .chain((2..=series).map(|key| place(key, Type::Scalar(Scalar::I64))))
            .collect()
    }

    /// The bytes of each series of a frame.
    fn len(&self) -> usize {
        8 * usize::try_from(self.samples).expect("few")
    }

    /// The bytes of the head, the ends, the count, and the body of a frame.
    fn total(&self) -> usize {
        7 + 8 * self.series + 4 + self.series * self.len()
    }
}

fn main() {
    by_hand(0, answer, |session, node| {
        Box::pin(async move {
            let mut timer = Line::new("timer", FRAMES);
            let mut lines = Vec::new();
            let mut calls = Vec::new();
            let mut context = Context::from_waker(Waker::noop());
            for shape in &LINES {
                let config = Config {
                    authority: Authority(1),
                    lease: None,
                    channels: shape.places()[1..]
                        .iter()
                        .map(|place| place.name.clone())
                        .collect(),
                };
                let mut writer = session.writer(config).await.expect("opens");
                let layout: Vec<(usize, usize)> = (0..shape.series)
                    .map(|entry| (entry, shape.len()))
                    .collect();
                let [draft, sends, outcome] = shape.names;
                let mut parts = [draft, sends, outcome].map(|n| Line::new(n, FRAMES));
                let mut out = Vec::with_capacity(ROUNDS * FRAMES);
                let mut whole = Vec::with_capacity(ROUNDS * FRAMES);
                for round in 0..WARMUP + ROUNDS {
                    for _ in 0..FRAMES {
                        timer.add(table::timed(&ALLOCATOR, || ()).1);
                        let (ready, drafted) = table::timed(&ALLOCATOR, || {
                            let draft =
                                pin!(writer.draft(Form::Raw, shape.samples, &layout));
                            matches!(draft.poll(&mut context), Poll::Ready(Ok(_)))
                        });
                        assert!(ready, "the pool has a block");
                        let mut write = pin!(writer.write(Time::Frame(Path::Live)));
                        let (pending, sent) = table::timed(&ALLOCATOR, || {
                            write.as_mut().poll(&mut context).is_pending()
                        });
                        assert!(pending, "the write waits for its outcome");
                        node.clock().sleep(ANSWERED).await;
                        let (ready, taken) = table::timed(&ALLOCATOR, || {
                            matches!(
                                write.as_mut().poll(&mut context),
                                Poll::Ready(Ok(_))
                            )
                        });
                        assert!(ready, "the outcome is in the transport");
                        if round >= WARMUP {
                            out.push(drafted.0 + sent.0);
                            whole.push(drafted.0 + sent.0 + taken.0);
                        }
                        for (line, figures) in
                            parts.iter_mut().zip([drafted, sent, taken])
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

/// Opens the write of each line, takes each frame whole, and answers it with `Taken`.
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
            let reply = Reply::Outcome {
                frame: u64::try_from(frame).expect("a frame"),
                group: 0,
                outcome: Outcome::Taken {
                    range: Range {
                        seq,
                        count: shape.samples,
                    },
                },
            };
            let mut bytes = vec![0; reply.encoded_len()];
            reply.encode(&mut bytes);
            send(write.sender.as_mut().expect("two-way"), &bytes).await;
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
