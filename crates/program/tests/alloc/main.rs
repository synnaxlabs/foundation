//! A warm `Reader::next` makes no heap allocation when its frame is in the transport:
//! it takes a block of the pool and the series bytes, and gives the frame. A warm
//! `Writer::write` makes none either: it fills a draft, sends it, and takes its
//! outcome. This binary
//! has no test harness: the count covers each thread, and a harness allocates on its
//! own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[path = "../../../hub/tests/common/agent.rs"]
#[expect(dead_code, reason = "the binary uses only some of the helpers")]
mod agent;
#[path = "../common/connect.rs"]
mod connect;
#[path = "../common/hand.rs"]
mod hand;
#[path = "../common/home.rs"]
#[expect(dead_code, reason = "the binary uses only some of the helpers")]
mod home;
#[path = "../../../hub/tests/common/net.rs"]
#[expect(dead_code, reason = "the binary uses only some of the helpers")]
mod net;
#[path = "../../../hub/tests/common/node.rs"]
mod node;
#[path = "../../../hub/tests/common/sessions.rs"]
#[expect(dead_code, reason = "the binary uses only some of the helpers")]
mod sessions;

use std::pin::pin;
use std::task::{Context, Poll, Waker};

use program::write::{Config, Outcome, Time};
use types::authority::Authority;
use types::channel;
use types::frame::{Form, Path, Range};
use types::name::Selector;
use types::sample::{Scalar, Type};
use types::time::Span;
use wire::hub::client::write::Write;
use wire::hub::client::{Mode, Place};
use wire::hub::write::Reply;

use crate::hand::{accept_stream, by_hand, opened, send, send_frame, start};

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

/// Frames that warm the reader and the pool up.
const WARM: u64 = 4;
/// Frames whose `next` is counted.
const COUNTED: u64 = 100;
/// The time after the open in which the node sends each frame.
const SENT: Span = Span::SECOND;

fn main() {
    takes_a_frame_with_no_allocation();
    writes_a_frame_with_no_allocation();
}

fn place(key: u128, data_type: Type, channel: &str, index: usize) -> Place {
    Place {
        key: channel::Key::from_u128(key),
        data_type,
        name: channel.parse().expect("a valid name"),
        index,
    }
}

/// Each warm call of `next` on a frame that the node sent gives the frame at the first
/// poll, and it and the drop of the frame make no allocation.
fn takes_a_frame_with_no_allocation() {
    let places = [
        place(10, Type::Scalar(Scalar::I64), "a", 1),
        place(1, Type::Scalar(Scalar::Stamp), "time", 1),
        place(11, Type::Scalar(Scalar::I64), "b", 1),
    ];
    let body: Vec<u8> = (0..24).collect();
    by_hand(
        430,
        move |session, node| async move {
            let mut read = opened(&session, &places).await;
            for seq in 0..WARM + COUNTED {
                let range = Range { seq, count: 1 };
                let run = [(0, 8), (1, 16), (2, 24)];
                send_frame(&mut read, range, &run, &body, &[24]).await;
            }
            node.clock().sleep(SENT).await;
            node.clock().sleep(SENT).await;
        },
        move |session, node| {
            Box::pin(async move {
                let select = Selector::new(["a", "b"]).expect("a selector");
                let mut reader =
                    session.reader(select, Mode::Complete).await.expect("opens");
                node.clock().sleep(SENT).await;
                let mut context = Context::from_waker(Waker::noop());
                for seq in 0..WARM + COUNTED {
                    let (taken, count) = ALLOCATOR.count(|| {
                        let Poll::Ready(frame) = pin!(reader.next()).poll(&mut context)
                        else {
                            return false;
                        };
                        let frame = frame.expect("a frame");
                        frame.range(0) == Some(Range { seq, count: 1 })
                    });
                    assert!(taken, "frame {seq} is ready and in order");
                    if seq >= WARM {
                        assert_eq!(count, 0, "frame {seq}");
                    }
                }
            })
        },
    );
}

/// Each warm call of `write` takes a draft, sends its frame at the first poll, and
/// gives the outcome at the first poll after the node replied, and none of it makes
/// an allocation.
fn writes_a_frame_with_no_allocation() {
    let places = [
        place(1, Type::Scalar(Scalar::Stamp), "time", 0),
        place(10, Type::Scalar(Scalar::I64), "a", 0),
    ];
    by_hand(
        431,
        move |session, node| async move {
            let mut write = accept_stream(&session, |first| {
                Write::decode(first).expect("a write").body()
            })
            .await;
            start(&mut write, &places, places.len()).await;
            for seq in 0..WARM + COUNTED {
                for _ in ["head", "ends", "body"] {
                    let message = write.receiver.recv().await.expect("receives");
                    message.expect("a message of the frame");
                }
                let reply = Reply::Outcome {
                    frame: seq + 1,
                    outcome: Outcome::Taken {
                        range: Range { seq, count: 1 },
                    },
                };
                let mut bytes = vec![0; reply.encoded_len()];
                reply.encode(&mut bytes);
                send(write.sender.as_mut().expect("two-way"), &bytes).await;
            }
            node.clock().sleep(SENT).await;
        },
        move |session, node| {
            Box::pin(async move {
                let config = Config {
                    authority: Authority(1),
                    lease: None,
                    channels: vec!["a".parse().expect("a valid name")],
                };
                let mut writer = session.writer(config).await.expect("opens");
                let mut context = Context::from_waker(Waker::noop());
                for seq in 0..WARM + COUNTED {
                    let (ready, drafted) = ALLOCATOR.count(|| {
                        let draft = pin!(writer.draft(Form::Raw, 1, &[(0, 8), (1, 8)]));
                        matches!(draft.poll(&mut context), Poll::Ready(Ok(_)))
                    });
                    assert!(ready, "frame {seq} has a block");
                    let mut write = pin!(writer.write(Time::Frame(Path::Live)));
                    let (sent, sending) = ALLOCATOR
                        .count(|| write.as_mut().poll(&mut context).is_pending());
                    assert!(sent, "frame {seq} waits for its outcome");
                    node.clock().sleep(SENT).await;
                    let (outcome, taking) =
                        ALLOCATOR.count(|| write.as_mut().poll(&mut context));
                    let taken = Outcome::Taken {
                        range: Range { seq, count: 1 },
                    };
                    assert_eq!(outcome, Poll::Ready(Ok(taken)), "frame {seq}");
                    if seq >= WARM {
                        assert_eq!(
                            (drafted, sending, taking),
                            (0, 0, 0),
                            "frame {seq}"
                        );
                    }
                }
            })
        },
    );
}
