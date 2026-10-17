//! Each behavior of a [`Session`]: the hello and its renewal, requests, the turn, the
//! pool, and each error.

use std::cell::Cell;
use std::future::poll_fn;
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use hub::serve;
use program::{Error, LIFE, Session};
use transport::stream::Incoming;
use transport::{Address, Code};
use types::time::{Interval, Span, Stamp};
use wire::header::MALFORMED;
use wire::hub::BUSY;
use wire::hub::client::{
    BODY_BYTES_MAX, Challenge, REFUSED, Refusal, Request, Response, Signed,
};

use crate::agent::{AGENT, SUBJECT, name};
use crate::connect::{ample, connect, connect_with};
use crate::hand::by_hand;
use crate::home::{accept, serve_session, serve_session_on};
use crate::net::{HOME, header, own_pool, public_key, transport};
use crate::sessions::{Got, Home, OTHER, QUIET, rules, run_program, shrink_wall_error};

/// Runs `program` with a session of [`SUBJECT`] on a home with mesh time and the
/// rules of `rules`, and gives what the home saw.
fn with_session<P>(
    seed: u64,
    program: impl FnOnce(Session, sim::node::Node) -> P + Send + 'static,
) -> Home
where
    P: Future<Output = ()> + 'static,
{
    serve_session(seed, true, Some(rules()), |node, tasks, at| async move {
        let session = connect(&node, tasks, at, AGENT).await.expect("connects");
        program(session, node.clone()).await;
        node.clock().sleep(QUIET).await;
    })
}

/// `bytes` bytes that differ from their reverse.
fn body(bytes: usize) -> Vec<u8> {
    (0..=250).cycle().take(bytes).collect()
}

fn reversed(body: &[u8]) -> Vec<u8> {
    body.iter().rev().copied().collect()
}

#[test]
fn sends_a_request_and_gives_its_whole_reply_at_each_size() {
    let large = body(256 << 10);
    let sent = large.clone();
    let home = with_session(120, move |session, _| async move {
        assert_eq!(session.request(b"").await, Ok(Vec::new()));
        assert_eq!(session.request(&sent).await, Ok(reversed(&sent)));
    });
    assert_eq!(
        home.served[..2],
        [
            Ok(Got::Request(name(SUBJECT), Vec::new())),
            Ok(Got::Request(name(SUBJECT), large)),
        ]
    );
}

#[test]
fn closes_the_session_when_the_last_clone_drops() {
    let home = with_session(121, |session, node| async move {
        let clone = session.clone();
        drop(session);
        assert_eq!(clone.request(b"ab").await, Ok(b"ba".to_vec()));
        drop(clone);
        node.clock().sleep(QUIET).await;
    });
    assert_eq!(home.served.len(), 2, "{:?}", home.served);
    assert_eq!(
        home.served[0],
        Ok(Got::Request(name(SUBJECT), b"ab".to_vec()))
    );
    assert_eq!(home.closed, transport::Error::PeerClosed { code: Code(0) });
}

/// The home keeps the end of each stream, whichever task the sim picks first.
#[test]
fn keeps_the_end_of_each_stream_at_each_seed() {
    for seed in 0..16 {
        let home = with_session(seed, |session, node| async move {
            assert_eq!(session.request(b"ab").await, Ok(b"ba".to_vec()));
            drop(session);
            node.clock().sleep(QUIET).await;
        });
        assert_eq!(home.served.len(), 2, "seed {seed}: {:?}", home.served);
    }
}

#[test]
fn refuses_a_hello_of_a_key_the_spec_does_not_list() {
    let got = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&got);
    let home = serve_session(122, true, Some(rules()), |node, tasks, at| async move {
        let refused = connect(&node, tasks, at, OTHER).await.map(drop);
        *kept.lock().expect("not poisoned") = Some(refused);
    });
    let got = got.lock().expect("not poisoned").take();
    assert_eq!(got, Some(Err(Error::Refused(Refusal::Refused))));
    let unlisted = access::proof::Error::Unlisted {
        subject: name(SUBJECT),
        key: public_key(&OTHER),
    };
    assert_eq!(home.served, [Err(serve::Error::Access(unlisted))]);
}

#[test]
fn refuses_to_connect_to_a_node_with_no_mesh_time() {
    let got = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&got);
    let home = serve_session(123, false, Some(rules()), |node, tasks, at| async move {
        let refused = connect(&node, tasks, at, AGENT).await.map(drop);
        *kept.lock().expect("not poisoned") = Some(refused);
    });
    let got = got.lock().expect("not poisoned").take();
    assert_eq!(got, Some(Err(Error::Refused(Refusal::Unsynced))));
    assert_eq!(
        home.served,
        [Err(serve::Error::Access(access::proof::Error::Unsynced))]
    );
}

/// The session renews the hello, so a request long after the first hello's expiry
/// gets its reply, and the hello stream stays open.
#[test]
fn renews_the_hello_before_it_expires() {
    let home = with_session(124, |session, node| async move {
        node.clock()
            .sleep(Span::from_nanos(2 * LIFE.nanos() + Span::SECOND.nanos()))
            .await;
        assert_eq!(session.request(b"late").await, Ok(b"etal".to_vec()));
    });
    assert_eq!(
        home.served[0],
        Ok(Got::Request(name(SUBJECT), b"late".to_vec()))
    );
}

/// Two tasks that call `request` on clones of one session at once each get their own
/// replies, in turns, and the node never stops one as `MALFORMED`.
#[test]
fn takes_requests_from_two_tasks_one_at_a_time() {
    const EACH: usize = 8;
    let home = serve_session(125, true, Some(rules()), |node, tasks, at| async move {
        let session = connect(&node, tasks.clone(), at, AGENT)
            .await
            .expect("connects");
        let replied = Rc::new(Cell::new(0));
        for tag in [1, 2] {
            let (session, replied) = (session.clone(), Rc::clone(&replied));
            tasks.spawn(async move {
                for i in 0..EACH {
                    let body = [tag, u8::try_from(i).expect("small")];
                    assert_eq!(session.request(&body).await, Ok(reversed(&body)));
                    replied.set(replied.get() + 1);
                }
            });
        }
        node.clock()
            .sleep(Span::from_nanos(10 * Span::SECOND.nanos()))
            .await;
        assert_eq!(replied.get(), 2 * EACH);
    });
    let order: Vec<_> = (0..EACH)
        .flat_map(|i| [1, 2].map(|tag| [tag, u8::try_from(i).expect("small")]))
        .map(|body| Ok(Got::Request(name(SUBJECT), body.to_vec())))
        .collect();
    let requests: Vec<_> = home
        .served
        .into_iter()
        .filter(|got| matches!(got, Ok(Got::Request(..))))
        .collect();
    assert_eq!(requests, order);
}

/// A body of exactly the cap is sent whole.
#[test]
fn sends_a_body_at_the_cap() {
    by_hand(
        137,
        |session, node| async move {
            let mut incoming = session.accept().await.expect("a stream");
            header(&mut incoming).await;
            let mut bytes = 0;
            while let Some(message) = incoming.receiver.recv().await.expect("a message")
            {
                bytes += message.len();
            }
            let cap = usize::try_from(BODY_BYTES_MAX).expect("fits");
            assert_eq!(bytes, Request::LEN + cap);
            let mut response = [0; Response::LEN];
            Response { length: 0 }.encode(&mut response);
            let sender = incoming.sender.as_mut().expect("two-way");
            let response = own_pool().copy(&response).expect("room");
            sender.send(response).await.expect("sends");
            sender.finish().expect("finishes");
            node.clock().sleep(QUIET).await;
        },
        |session, _| {
            Box::pin(async move {
                let cap = usize::try_from(BODY_BYTES_MAX).expect("fits");
                assert_eq!(session.request(&vec![7; cap]).await, Ok(Vec::new()));
            })
        },
    );
}

#[test]
fn refuses_a_body_over_the_cap_with_nothing_sent() {
    let home = with_session(126, |session, _| async move {
        let length = usize::try_from(BODY_BYTES_MAX).expect("fits") + 1;
        let refused = session.request(&vec![0; length]).await;
        assert_eq!(refused, Err(Error::Body { length }));
        assert_eq!(
            refused.expect_err("refused").to_string(),
            "the body has 16777217 bytes, over the 16777216 that a request holds"
        );
    });
    assert_eq!(
        home.served.len(),
        1,
        "only the hello stream: {:?}",
        home.served
    );
}

/// The next request stream of `session`, read to its finish.
async fn read_request(session: &transport::Session) -> Incoming {
    let mut incoming = session.accept().await.expect("a stream");
    header(&mut incoming).await;
    while incoming.receiver.recv().await.expect("a message").is_some() {}
    incoming
}

/// A close with a code of `Refusal` gives the refusal, and a close with 0 or another
/// code gives the transport error, also as the error that ended the session.
#[test]
fn gives_the_close_of_a_node_after_admission() {
    let cases = [
        (127, BUSY, Error::Refused(Refusal::Busy)),
        (
            131,
            0,
            Error::Transport(transport::Error::PeerClosed { code: Code(0) }),
        ),
        (
            132,
            99,
            Error::Transport(transport::Error::PeerClosed { code: Code(99) }),
        ),
    ];
    for (seed, code, error) in cases {
        by_hand(
            seed,
            move |session, node| async move {
                node.clock().sleep(QUIET).await;
                session.close(Code(code));
                node.clock().sleep(QUIET).await;
            },
            move |session, node| {
                Box::pin(async move {
                    assert_eq!(session.ended(), None);
                    node.clock().sleep(QUIET).await;
                    node.clock().sleep(QUIET).await;
                    assert_eq!(session.ended(), Some(error.clone()));
                    assert_eq!(session.request(b"ab").await, Err(error));
                })
            },
        );
    }
}

/// A request that the node's close ends and `ended` give one error, whichever of the
/// request and the renewal runs first after the close.
#[test]
fn ended_gives_the_close_that_a_request_gave_at_each_seed() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    for seed in 0..64 {
        let seen = Arc::clone(&seen);
        by_hand(
            seed,
            move |session, node| async move {
                let _request = read_request(&session).await;
                session.close(Code(99));
                node.clock().sleep(QUIET).await;
            },
            move |session, _| {
                Box::pin(async move {
                    let error = session.request(b"ab").await.unwrap_err();
                    seen.lock().unwrap().push((seed, error, session.ended()));
                })
            },
        );
    }
    let closed = Error::Transport(transport::Error::PeerClosed { code: Code(99) });
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 64);
    for (seed, error, ended) in seen.iter() {
        assert_eq!(
            (error, ended.as_ref()),
            (&closed, Some(&closed)),
            "seed {seed}"
        );
    }
}

#[test]
fn refuses_a_request_stream_finished_with_no_response() {
    by_hand(
        128,
        |session, node| async move {
            let mut incoming = read_request(&session).await;
            incoming
                .sender
                .as_mut()
                .expect("two-way")
                .finish()
                .expect("finishes");
            node.clock().sleep(QUIET).await;
        },
        |session, _| {
            Box::pin(async move {
                assert_eq!(session.request(b"ab").await, Err(Error::Unanswered));
            })
        },
    );
}

#[test]
fn refuses_a_response_whose_body_ends_early() {
    by_hand(
        129,
        |session, node| async move {
            let mut incoming = read_request(&session).await;
            let sender = incoming.sender.as_mut().expect("two-way");
            let pool = own_pool();
            let mut response = [0; Response::LEN];
            Response { length: 4 }.encode(&mut response);
            for message in [&response[..], b"dc"] {
                sender
                    .send(pool.copy(message).expect("room"))
                    .await
                    .expect("sends");
            }
            sender.finish().expect("finishes");
            node.clock().sleep(QUIET).await;
        },
        |session, _| {
            Box::pin(async move {
                assert_eq!(
                    session.request(b"abcd").await,
                    Err(Error::Message(wire::hub::client::Error::Unfinished {
                        remain: 2
                    }))
                );
            })
        },
    );
}

/// A request that fails once its response began gives the turn at once, as the node
/// freed it, also when the node holds the stream open.
#[test]
fn gives_the_turn_after_a_response_that_is_not_valid() {
    by_hand(
        151,
        |session, node| async move {
            let mut first = read_request(&session).await;
            let bad = own_pool().copy(&[0xff]).expect("room");
            let sender = first.sender.as_mut().expect("two-way");
            sender.send(bad).await.expect("sends");
            let mut second = read_request(&session).await;
            let sender = second.sender.as_mut().expect("two-way");
            let mut response = [0; Response::LEN];
            Response { length: 2 }.encode(&mut response);
            for message in [&response[..], b"dc"] {
                sender
                    .send(own_pool().copy(message).expect("room"))
                    .await
                    .expect("sends");
            }
            sender.finish().expect("finishes");
            node.clock().sleep(QUIET).await;
            drop(first);
        },
        |session, node| {
            Box::pin(async move {
                assert_eq!(
                    session.request(b"ab").await,
                    Err(Error::Message(wire::hub::client::Error::Kind {
                        kind: 0xff
                    }))
                );
                let next = within(&node.clock(), QUIET, session.request(b"cd")).await;
                assert_eq!(next, Some(Ok(b"dc".to_vec())));
            })
        },
    );
}

/// `request` gives the reply once its body ends, and does not wait for the node to
/// finish the stream.
#[test]
fn gives_the_reply_once_its_body_ends() {
    by_hand(
        130,
        |session, node| async move {
            let mut incoming = read_request(&session).await;
            let sender = incoming.sender.as_mut().expect("two-way");
            let pool = own_pool();
            let mut response = [0; Response::LEN];
            Response { length: 2 }.encode(&mut response);
            for message in [&response[..], b"ba"] {
                sender
                    .send(pool.copy(message).expect("room"))
                    .await
                    .expect("sends");
            }
            node.clock().sleep(QUIET).await;
            node.clock().sleep(QUIET).await;
            sender.finish().expect("finishes");
        },
        |session, node| {
            Box::pin(async move {
                let mut reply = pin!(session.request(b"ab"));
                let mut quiet = pin!(node.clock().sleep(QUIET));
                let got = poll_fn(|cx| match reply.as_mut().poll(cx) {
                    Poll::Ready(got) => Poll::Ready(Some(got)),
                    Poll::Pending => quiet.as_mut().poll(cx).map(|()| None),
                })
                .await;
                assert_eq!(got, Some(Ok(b"ba".to_vec())));
            })
        },
    );
}

/// A home whose node replies to each request a second after it came.
async fn slow(node: sim::node::Node, tasks: env::tasks::Tasks) {
    let (test, session, link) = accept(&node, &tasks, true, Some(rules())).await;
    while let Ok(mut incoming) = session.accept().await {
        let (link, clock) = (link.clone(), node.clock());
        tasks.spawn(async move {
            header(&mut incoming).await;
            if let Ok(hub::Served::Request(request)) = link.serve(incoming).await {
                clock.sleep(Span::SECOND).await;
                drop(request.reply.send(&reversed(&request.body)).await);
            }
        });
    }
    drop((link, test));
}

/// The output of `future`, or `None` when `span` passes first, which drops it.
pub(super) async fn within<T>(
    clock: &env::clock::Clock,
    span: Span,
    future: impl Future<Output = T>,
) -> Option<T> {
    let mut future = pin!(future);
    let mut quiet = pin!(clock.sleep(span));
    poll_fn(|cx| match future.as_mut().poll(cx) {
        Poll::Ready(got) => Poll::Ready(Some(got)),
        Poll::Pending => quiet.as_mut().poll(cx).map(|()| None),
    })
    .await
}

/// A program that gives up on a slow request, then sends its next request, gets the
/// reply of the next request: the node holds the first open until it replies.
#[test]
fn sends_the_next_request_after_a_dropped_one() {
    run_program(133, slow, |node, tasks, at| async move {
        let session = connect(&node, tasks, at, AGENT).await.expect("connects");
        let first = within(&node.clock(), QUIET, session.request(b"slow")).await;
        assert_eq!(first, None, "the first request is dropped while it waits");
        assert_eq!(session.request(b"ab").await, Ok(b"ba".to_vec()));
    });
}

/// A request dropped while it waits for the turn leaves the line, and the request
/// after it takes the turn.
#[test]
fn gives_the_turn_past_a_request_that_left_the_line() {
    run_program(138, slow, |node, tasks, at| async move {
        let session = connect(&node, tasks.clone(), at, AGENT)
            .await
            .expect("connects");
        let first = session.clone();
        tasks.spawn(async move {
            assert_eq!(first.request(b"a1").await, Ok(b"1a".to_vec()));
        });
        node.clock().sleep(QUIET).await;
        let (second, clock) = (session.clone(), node.clock());
        tasks.spawn(async move {
            assert_eq!(within(&clock, QUIET, second.request(b"b2")).await, None);
        });
        node.clock()
            .sleep(Span::from_nanos(QUIET.nanos() / 2))
            .await;
        let third = Rc::new(Cell::new(None));
        let (session, got) = (session.clone(), Rc::clone(&third));
        tasks.spawn(async move { got.set(Some(session.request(b"c3").await)) });
        node.clock()
            .sleep(Span::from_nanos(5 * Span::SECOND.nanos()))
            .await;
        assert_eq!(third.take(), Some(Ok(b"3c".to_vec())));
    });
}

/// Runs a program whose session connects to a home that serves the hello stream by
/// hand with `hello`, with no hub.
pub(super) fn raw<F, H, G, P>(seed: u64, hello: F, program: G)
where
    F: FnOnce(transport::Session, Incoming, sim::node::Node) -> H + Send + 'static,
    H: Future<Output = ()> + 'static,
    G: FnOnce(sim::node::Node, env::tasks::Tasks, Address) -> P + Send + 'static,
    P: Future<Output = ()> + 'static,
{
    let home = move |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let transport = transport(&node, &tasks, &own_pool(), HOME, 1 << 16);
        let session = transport.accept().await.expect("a session");
        let mut incoming = session.accept().await.expect("a stream");
        header(&mut incoming).await;
        hello(session, incoming, node).await;
    };
    run_program(seed, home, program);
}

#[test]
fn refuses_a_hello_stream_finished_with_no_challenge() {
    raw(
        134,
        |session, mut hello, node| async move {
            hello
                .sender
                .as_mut()
                .expect("two-way")
                .finish()
                .expect("finishes");
            node.clock().sleep(QUIET).await;
            drop(session);
        },
        |node, tasks, at| async move {
            let got = connect(&node, tasks, at, AGENT).await.map(drop);
            assert_eq!(got, Err(Error::Unanswered));
        },
    );
}

/// The renewal comes 5 minutes, half of `LIFE`, after the admission. A challenge that
/// `wire` refuses ends the renewal, closes the session with `MALFORMED`, and each later
/// request gives the error.
#[test]
fn closes_the_session_on_a_challenge_that_is_not_valid() {
    raw(
        135,
        |session, mut hello, node| async move {
            let renewal = admit(&mut hello, &node.clock()).await;
            assert!(
                renewal >= HALF
                    && renewal.nanos() < HALF.nanos() + Span::SECOND.nanos(),
                "the renewal comes at half of LIFE: {renewal:?}"
            );
            let sender = hello.sender.as_mut().expect("two-way");
            let refused = own_pool().copy(&[0xff]).expect("room");
            sender.send(refused).await.expect("sends");
            assert_eq!(
                session.closed().await,
                transport::Error::PeerClosed {
                    code: Code(MALFORMED)
                }
            );
        },
        |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            node.clock().sleep(LIFE).await;
            assert_eq!(
                session.request(b"ab").await,
                Err(Error::Message(wire::hub::client::Error::Kind {
                    kind: 0xff
                }))
            );
        },
    );
}

/// A call of `ended` on an open session takes constant time and memory, so a
/// program can ask it as often as it likes.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test bounds its own run time"
)]
fn ended_on_an_open_session_stays_cheap() {
    let elapsed = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&elapsed);
    with_session(127, move |session, _| async move {
        let start = std::time::Instant::now();
        for _ in 0..100_000 {
            assert_eq!(session.ended(), None);
        }
        *kept.lock().unwrap() = Some(start.elapsed());
        assert_eq!(session.request(b"ab").await, Ok(b"ba".to_vec()));
    });
    let elapsed = elapsed.lock().unwrap().expect("ran");
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "100000 calls of `ended` took {elapsed:?}"
    );
}

/// Each request after the end of the renewal gives the error that ended it, not only
/// the first.
#[test]
fn gives_the_error_of_the_renewal_to_each_later_request() {
    raw(
        135,
        |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let sender = hello.sender.as_mut().expect("two-way");
            let refused = own_pool().copy(&[0xff]).expect("room");
            sender.send(refused).await.expect("sends");
            drop(session.closed().await);
        },
        |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            node.clock().sleep(LIFE).await;
            let ended = Err(Error::Message(wire::hub::client::Error::Kind {
                kind: 0xff,
            }));
            assert_eq!(session.ended(), ended.clone().err());
            assert_eq!(session.request(b"ab").await, ended);
            assert_eq!(session.request(b"cd").await, ended);
        },
    );
}

/// A request in flight when a challenge that `wire` refuses ends the renewal gives
/// that error, not the error of the close.
#[test]
fn gives_the_error_of_the_renewal_to_a_request_in_flight() {
    raw(
        135,
        |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let _request = read_request(&session).await;
            let sender = hello.sender.as_mut().expect("two-way");
            let refused = own_pool().copy(&[0xff]).expect("room");
            sender.send(refused).await.expect("sends");
            drop(session.closed().await);
        },
        |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            assert_eq!(
                session.request(b"ab").await,
                Err(Error::Message(wire::hub::client::Error::Kind {
                    kind: 0xff
                }))
            );
        },
    );
}

/// A request in flight when the node finishes the hello stream gives
/// [`Error::Unanswered`], the error that ended the renewal.
#[test]
fn gives_an_unanswered_renewal_to_a_request_in_flight() {
    raw(
        136,
        |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let _request = read_request(&session).await;
            let sender = hello.sender.as_mut().expect("two-way");
            sender.finish().expect("finishes");
            drop(session.closed().await);
        },
        |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            assert_eq!(session.request(b"ab").await, Err(Error::Unanswered));
        },
    );
}

/// A body over the cap gives [`Error::Body`] also once the renewal ended.
#[test]
fn gives_the_body_error_first_after_the_renewal_ended() {
    raw(
        152,
        |session, mut hello, node| async move {
            admit(&mut hello, &node.clock()).await;
            let sender = hello.sender.as_mut().expect("two-way");
            let refused = own_pool().copy(&[0xff]).expect("room");
            sender.send(refused).await.expect("sends");
            drop(session.closed().await);
        },
        |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            node.clock().sleep(LIFE).await;
            let length = usize::try_from(BODY_BYTES_MAX).expect("fits") + 1;
            assert_eq!(
                session.request(&vec![0; length]).await,
                Err(Error::Body { length })
            );
        },
    );
}

/// The retry of a message that finds the pool full, as the record states it.
const RETRY: Span = Span::from_nanos(10_000_000);

/// The most that a renewal comes after the pool has room: two retries, which hold the
/// time a hello takes to the node.
const RETRY_MAX: Span = Span::from_nanos(2 * RETRY.nanos());

/// When the pool has room after the time of a renewal: half a retry past a whole
/// number of retries, so a retry of another span comes at another time.
const ROOM: Span = Span::from_nanos(25 * RETRY.nanos() + RETRY.nanos() / 2);

/// Half of `LIFE`, as the record states it.
const HALF: Span = Span::from_nanos(5 * Span::MINUTE.nanos());

/// Sends a challenge on `hello` whose mesh time is `at`.
async fn challenge(hello: &mut Incoming, at: Stamp) {
    let mut bytes = [0; Challenge::LEN];
    Challenge {
        nonce: [7; 16],
        now: Interval {
            earliest: at,
            latest: at,
        },
    }
    .encode(&mut bytes);
    let message = own_pool().copy(&bytes).expect("room");
    let sender = hello.sender.as_mut().expect("two-way");
    sender.send(message).await.expect("sends");
}

/// Takes the next hello on `hello`.
async fn take(hello: &mut Incoming) -> Signed {
    let message = hello
        .receiver
        .recv()
        .await
        .expect("a hello")
        .expect("not finished");
    Signed::decode(&message).expect("a signed hello")
}

/// Sends a challenge on `hello` and takes the hello, then admits it with the next
/// challenge and takes the renewal. Gives the time from the admission to the renewal.
pub(super) async fn admit(hello: &mut Incoming, clock: &env::clock::Clock) -> Span {
    let mut came = [Span::from_nanos(0); 2];
    for slot in &mut came {
        let sent = clock.now();
        challenge(hello, Stamp::EPOCH).await;
        take(hello).await;
        *slot = clock.now() - sent;
    }
    assert!(
        came[0] < Span::SECOND,
        "the hello comes at once: {:?}",
        came[0]
    );
    came[1]
}

/// Takes each block of `pool` that it has room for.
fn fill(pool: &block::Pool) -> Vec<block::Unique> {
    let mut held = Vec::new();
    let mut len = pool.largest();
    while len > 0 {
        match pool.alloc(len) {
            Ok(block) => held.push(block),
            Err(_) => len /= 2,
        }
    }
    held
}

/// A renewal that finds the pool full tries again, so the hello stays admitted.
#[test]
fn renews_the_hello_once_the_pool_has_room() {
    let home = serve_session(136, true, Some(rules()), |node, tasks, at| async move {
        let pool = own_pool();
        let session = connect_with(&node, tasks, at, AGENT, Rc::clone(&pool), ample())
            .await
            .expect("connects");
        let half = Span::from_nanos(LIFE.nanos() / 2 - Span::SECOND.nanos());
        node.clock().sleep(half).await;
        let held = fill(&pool);
        node.clock()
            .sleep(Span::from_nanos(10 * Span::SECOND.nanos()))
            .await;
        drop(held);
        node.clock().sleep(LIFE).await;
        node.clock().sleep(LIFE).await;
        assert_eq!(session.request(b"late").await, Ok(b"etal".to_vec()));
        node.clock().sleep(QUIET).await;
    });
    assert_eq!(
        home.served[0],
        Ok(Got::Request(name(SUBJECT), b"late".to_vec()))
    );
}

/// Frames that a reader holds until they fill
/// [`Config::frames`](program::Config::frames) leave the renewal on time.
#[test]
fn renews_the_hello_on_time_while_held_frames_fill_their_pool() {
    /// More frames than a pool of `small` holds.
    const SENT: u64 = 3000;
    let small = || {
        let config = block::Config::new(
            u64::try_from(block::footprint(1 << 16)).expect("a usize fits a u64"),
        )
        .expect("the budget fits");
        Rc::new(block::Pool::new(
            config.clone(),
            block::Heap::new(config.reservation()),
        ))
    };
    raw(
        426,
        |session, mut hello, node| async move {
            let clock = node.clock();
            challenge(&mut hello, Stamp::EPOCH).await;
            take(&mut hello).await;
            let sent = clock.now();
            challenge(&mut hello, Stamp::EPOCH).await;
            let stamp = types::sample::Type::Scalar(types::sample::Scalar::Stamp);
            let i64 = types::sample::Type::Scalar(types::sample::Scalar::I64);
            let place = |at: u128, data_type, channel: &str| wire::hub::client::Place {
                key: types::channel::Key::from_u128(at),
                data_type,
                name: name(channel),
            };
            let places = [
                place(10, i64, "a"),
                place(1, stamp, "time"),
                place(11, i64, "b"),
            ];
            let mut read = crate::hand::opened(&session, &places, 1).await;
            for seq in 0..SENT {
                let range = types::frame::Range { seq, count: 1 };
                crate::hand::send_frame(&mut read, range, &[(1, 8)], &[0; 8], &[8])
                    .await;
            }
            take(&mut hello).await;
            let came = clock.now() - sent;
            let late = Span::from_nanos(came.nanos() - HALF.nanos());
            assert!(late < RETRY, "the renewal comes on time: {late:?}");
            drop(session);
        },
        move |node, tasks, at| async move {
            let session = connect_with(&node, tasks, at, AGENT, small(), small())
                .await
                .expect("connects");
            let select = types::name::Selector::new(["a", "b"]).expect("a selector");
            let mut reader = session
                .reader(select, wire::hub::client::Mode::Complete)
                .await
                .expect("opens");
            let mut held = Vec::new();
            while let Some(frame) =
                within(&node.clock(), Span::SECOND, reader.next()).await
            {
                held.push(frame.expect("a frame"));
            }
            let count = u64::try_from(held.len()).expect("fits");
            assert!(count < SENT, "the frames fill their pool: {count}");
            node.clock().sleep(HALF).await;
            node.clock().sleep(Span::SECOND).await;
            drop((held, reader, session));
        },
    );
}

/// A renewal that finds the pool full tries again every 10 ms, so it comes at the
/// first retry after the pool has room.
#[test]
fn retries_a_renewal_every_ten_milliseconds_while_the_pool_is_full() {
    raw(
        140,
        |session, mut hello, node| async move {
            let clock = node.clock();
            challenge(&mut hello, Stamp::EPOCH).await;
            take(&mut hello).await;
            let sent = clock.now();
            challenge(&mut hello, Stamp::EPOCH).await;
            let renewal = take(&mut hello).await;
            let came = clock.now() - sent;
            let late = Span::from_nanos(came.nanos() - HALF.nanos());
            let next = ROOM.nanos() + RETRY.nanos() / 2;
            assert!(
                late.nanos() >= next && late.nanos() < ROOM.nanos() + RETRY_MAX.nanos(),
                "the renewal comes at the first retry after the pool has room: {late:?}"
            );
            // The challenge gives a mesh time of zero, so a hello whose wait for a
            // block took none of its life expires `LIFE` after it came.
            let life = renewal.hello.expires - (Stamp::EPOCH + came);
            assert!(
                life <= LIFE && life.nanos() > LIFE.nanos() - RETRY_MAX.nanos(),
                "the wait for a block takes none of the hello's life: {life:?}"
            );
            drop(session);
        },
        |node, tasks, at| async move {
            let pool = own_pool();
            let session =
                connect_with(&node, tasks, at, AGENT, Rc::clone(&pool), ample())
                    .await
                    .expect("connects");
            node.clock()
                .sleep(Span::from_nanos(HALF.nanos() - Span::SECOND.nanos()))
                .await;
            let held = fill(&pool);
            node.clock()
                .sleep(Span::from_nanos(Span::SECOND.nanos() + ROOM.nanos()))
                .await;
            drop(held);
            node.clock().sleep(Span::SECOND).await;
            drop(session);
        },
    );
}

/// A request that waits for a block of a full pool ends with the error of the close
/// when the node closes the session.
#[test]
fn ends_the_wait_for_a_block_when_the_session_closes() {
    raw(
        144,
        |session, mut hello, node| async move {
            challenge(&mut hello, Stamp::EPOCH).await;
            take(&mut hello).await;
            challenge(&mut hello, Stamp::EPOCH).await;
            node.clock().sleep(QUIET).await;
            session.close(Code(BUSY));
            node.clock().sleep(QUIET).await;
        },
        |node, tasks, at| async move {
            let pool = own_pool();
            let session =
                connect_with(&node, tasks, at, AGENT, Rc::clone(&pool), ample())
                    .await
                    .expect("connects");
            let held = fill(&pool);
            let got = within(&node.clock(), LIFE, session.request(b"ab")).await;
            assert_eq!(got, Some(Err(Error::Refused(Refusal::Busy))));
            drop(held);
        },
    );
}

/// A challenge at the end of mesh time gives a hello that expires then, not a panic,
/// also once the time since the challenge alone passes the end.
#[test]
fn ends_the_life_of_a_hello_at_the_end_of_time() {
    raw(
        149,
        |session, mut hello, node| async move {
            let end = Stamp::from_nanos(i64::MAX);
            challenge(&mut hello, end).await;
            assert_eq!(take(&mut hello).await.hello.expires, end);
            challenge(&mut hello, Stamp::from_nanos(i64::MAX - 1)).await;
            assert_eq!(take(&mut hello).await.hello.expires, end);
            challenge(&mut hello, Stamp::EPOCH).await;
            node.clock().sleep(QUIET).await;
            drop(session);
        },
        |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            node.clock().sleep(HALF).await;
            node.clock().sleep(QUIET).await;
            drop(session);
        },
    );
}

/// A pool of 1 MiB over memory that a test can make refuse, and the switch for it.
fn scarce() -> (Rc<block::Pool>, block::testing::Switch) {
    let config = block::Config::new(1 << 20).expect("the budget fits");
    let (memory, switch) = block::testing::Scarce::new(config.reservation());
    (Rc::new(block::Pool::new(config, memory)), switch)
}

/// Lets each commit of `switch` succeed after `wait` on `clock`.
fn allow_after(
    tasks: &env::tasks::Tasks,
    clock: env::clock::Clock,
    switch: block::testing::Switch,
    wait: Span,
) {
    tasks.spawn(async move {
        clock.sleep(wait).await;
        switch.allow();
    });
}

/// A pool whose memory refuses a block waits for it as for a full pool, so `connect`
/// sends its header and hello once the memory has room.
#[test]
fn connects_once_the_memory_of_the_pool_has_room() {
    let home = serve_session(147, true, Some(rules()), |node, tasks, at| async move {
        let (pool, switch) = scarce();
        switch.refuse();
        allow_after(&tasks, node.clock(), switch, Span::SECOND);
        let began = node.clock().now();
        let session = connect_with(&node, tasks, at, AGENT, pool, ample())
            .await
            .expect("connects");
        assert!(node.clock().now() - began >= Span::SECOND);
        assert_eq!(session.request(b"ab").await, Ok(b"ba".to_vec()));
        node.clock().sleep(QUIET).await;
    });
    assert_eq!(
        home.served[0],
        Ok(Got::Request(name(SUBJECT), b"ab".to_vec()))
    );
}

/// The `Request` of a request waits for its block as each message does. The pool
/// holds no block of its size yet, so its memory must commit one.
#[test]
fn sends_a_request_once_the_memory_of_the_pool_has_room() {
    let home = serve_session(148, true, Some(rules()), |node, tasks, at| async move {
        let (pool, switch) = scarce();
        let session = connect_with(&node, tasks.clone(), at, AGENT, pool, ample())
            .await
            .expect("connects");
        node.clock().sleep(QUIET).await;
        switch.refuse();
        allow_after(&tasks, node.clock(), switch, Span::SECOND);
        let began = node.clock().now();
        assert_eq!(session.request(b"ab").await, Ok(b"ba".to_vec()));
        assert!(node.clock().now() - began >= Span::SECOND);
        node.clock().sleep(QUIET).await;
    });
    assert_eq!(
        home.served[0],
        Ok(Got::Request(name(SUBJECT), b"ab".to_vec()))
    );
}

/// A pool whose largest block cannot hold the header fails `connect` at once.
#[test]
fn refuses_to_connect_from_a_pool_with_no_block_for_the_header() {
    let got = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&got);
    serve_session(145, true, Some(rules()), |node, tasks, at| async move {
        let config = block::Config::new(0).expect("the budget fits");
        let heap = block::Heap::new(config.reservation());
        let pool = Rc::new(block::Pool::new(config, heap));
        let refused = connect_with(&node, tasks, at, AGENT, pool, ample())
            .await
            .map(drop);
        *kept.lock().expect("not poisoned") = Some(refused);
    });
    let got = got.lock().expect("not poisoned").take().expect("ran");
    let error = block::Error::TooLarge {
        requested: wire::header::LEN,
        largest: 0,
    };
    assert_eq!(got, Err(Error::Pool(error)));
    assert_eq!(
        got.expect_err("refused").to_string(),
        "no block of the pool holds a message or a frame: block of 3 bytes is above \
         the largest block of 0 bytes"
    );
}

/// A session splits a body into blocks that its pool holds.
#[test]
fn sends_a_body_over_the_largest_block_of_its_pool() {
    let home = serve_session(141, true, Some(rules()), |node, tasks, at| async move {
        let config = block::Config::new(1 << 16).expect("the budget fits");
        let pool =
            block::Pool::new(config.clone(), block::Heap::new(config.reservation()));
        let body = body(pool.largest() + 1);
        let session = connect_with(&node, tasks, at, AGENT, Rc::new(pool), ample())
            .await
            .expect("connects");
        assert_eq!(session.request(&body).await, Ok(reversed(&body)));
    });
    assert!(
        matches!(home.served[0], Ok(Got::Request(..))),
        "{:?}",
        home.served
    );
}

/// A pool with less room than the node's flow window sends a large body, with each
/// chunk after a block that the node acknowledged, and gets the whole reply.
#[test]
fn sends_a_body_over_the_room_of_its_pool() {
    serve_session(143, true, Some(rules()), |node, tasks, at| async move {
        let config = block::Config::new(1 << 17).expect("the budget fits");
        let pool =
            block::Pool::new(config.clone(), block::Heap::new(config.reservation()));
        let session = connect_with(&node, tasks, at, AGENT, Rc::new(pool), ample())
            .await
            .expect("connects");
        let sent = body(256 << 10);
        let reply = session.request(&sent).await.expect("replies");
        assert!(reply.iter().eq(sent.iter().rev()));
    });
}

/// A request that the turn gave to and that dropped before it polled again hands the
/// turn on.
#[test]
fn hands_on_a_turn_given_to_a_dropped_request() {
    run_program(139, slow, |node, tasks, at| async move {
        let session = connect(&node, tasks, at, AGENT).await.expect("connects");
        let mut first = pin!(session.request(b"ab"));
        let mut second = Box::pin(session.request(b"cd"));
        let mut waits = false;
        let got = poll_fn(|cx| {
            if let Poll::Ready(got) = first.as_mut().poll(cx) {
                return Poll::Ready(got);
            }
            if !waits {
                assert!(second.as_mut().poll(cx).is_pending());
                waits = true;
            }
            Poll::Pending
        })
        .await;
        assert_eq!(got, Ok(b"ba".to_vec()));
        drop(second);
        assert_eq!(session.request(b"ef").await, Ok(b"fe".to_vec()));
    });
}

/// Three requests that wait for the turn take it in the order they began, also when
/// the last is polled first.
#[test]
fn gives_the_turn_in_the_order_requests_began() {
    let home = serve_session(142, true, Some(rules()), |node, tasks, at| async move {
        let session = connect(&node, tasks, at, AGENT).await.expect("connects");
        let mut requests: Vec<_> = [[1], [2], [3]]
            .iter()
            .map(|body| Some(Box::pin(session.request(body))))
            .collect();
        poll_fn(|cx| {
            for request in requests.iter_mut().flatten() {
                assert!(request.as_mut().poll(cx).is_pending());
            }
            Poll::Ready(())
        })
        .await;
        poll_fn(|cx| {
            for slot in requests.iter_mut().rev() {
                if let Some(request) = slot
                    && let Poll::Ready(got) = request.as_mut().poll(cx)
                {
                    assert!(got.is_ok(), "{got:?}");
                    *slot = None;
                }
            }
            if requests.iter().all(Option::is_none) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    });
    let requests: Vec<_> = home
        .served
        .into_iter()
        .filter(|got| matches!(got, Ok(Got::Request(..))))
        .collect();
    let order: Vec<_> = [1, 2, 3]
        .map(|tag| Ok(Got::Request(name(SUBJECT), vec![tag])))
        .into();
    assert_eq!(requests, order);
}

#[test]
fn names_each_error() {
    let cases = [
        (Error::Refused(Refusal::Expired), "the hello ended"),
        (
            Error::Transport(transport::Error::TimedOut),
            "the session failed: the peer stopped answering",
        ),
        (
            Error::Message(wire::hub::client::Error::Empty),
            "the node sent a message that is not valid: the hub message is empty",
        ),
        (
            Error::Unanswered,
            "the node finished a stream with no answer",
        ),
        (
            Error::Pool(block::Error::Exhausted {
                requested: 9,
                available: 0,
            }),
            "no block of the pool holds a message or a frame: pool is full: asked \
             for 9 bytes, 0 bytes free",
        ),
        (
            Error::Open { length: 70_000 },
            "the open of the read has 70000 bytes, over the 65536 that a read holds",
        ),
        (
            Error::Message(wire::hub::client::Error::Repeated {
                key: types::channel::Key::from_u128(1),
            }),
            "the node sent a message that is not valid: the places of the read name \
             channel 00000000-0000-0000-0000-000000000001 more than once",
        ),
        (
            Error::Reply(wire::hub::Error::Reopen { kind: 1 }),
            "the node sent a reply that is not valid: the hub message has kind 1, \
             which opens the session, and the session is open",
        ),
        (
            Error::Frame(types::frame::Error::IndexAbsent { entry: 0, index: 1 }),
            "the node sent a frame that is not valid: entry 0 is present but its \
             index, entry 1, is absent",
        ),
    ];
    for (error, text) in cases {
        assert_eq!(error.to_string(), text);
    }
}

/// A node's reset and close with a code each give `Refused`, so a program sees one
/// error for a refusal whichever frame comes first.
#[test]
fn gives_a_reset_and_a_close_as_one_refusal() {
    let code = Code(REFUSED);
    for error in [
        transport::Error::Reset { code },
        transport::Error::PeerClosed { code },
    ] {
        assert_eq!(Error::from(error), Error::Refused(Refusal::Refused));
    }
    assert_eq!(
        Error::from(transport::Error::Stopped { code }),
        Error::Refused(Refusal::Refused)
    );
    let reset = transport::Error::Reset { code: Code(0) };
    assert_eq!(Error::from(reset.clone()), Error::Transport(reset));
}

/// A pool with room for one block of a chunk sends a body at the cap on each link: a
/// chunk that finds no block waits until the node acknowledges the chunks before it.
#[test]
fn sends_a_body_at_the_cap_from_a_pool_of_one_chunk_on_each_link() {
    let micros = |n: i64| Span::from_nanos(n * 1_000);
    let lossless = sim::link::Config::default();
    let links = [
        lossless,
        sim::link::Config {
            jitter: micros(200),
            ..lossless
        },
        sim::link::Config {
            loss: 0.05,
            ..lossless
        },
    ];
    for wire in links {
        let label = format!("{wire:?}");
        serve_session_on(
            143,
            wire,
            true,
            Some(rules()),
            move |node, tasks, at| async move {
                let config = block::Config::new(
                    u64::try_from(block::footprint(1 << 16))
                        .expect("a usize fits in a u64"),
                )
                .expect("the budget fits");
                let pool = block::Pool::new(
                    config.clone(),
                    block::Heap::new(config.reservation()),
                );
                let session =
                    connect_with(&node, tasks, at, AGENT, Rc::new(pool), ample())
                        .await
                        .expect("connects");
                let cap = usize::try_from(BODY_BYTES_MAX).expect("fits");
                let sent = body(cap);
                let got = session.request(&sent).await;
                assert!(got == Ok(reversed(&sent)), "{label}");
            },
        );
    }
}

/// Asserts that a program connects to a home whose node has mesh time with
/// `wall_error`.
fn connects_at_wall_error(seed: u64, wall_error: Option<Span>) {
    let (connected, served) = (Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None)));
    let (session_end, home_end) = (Arc::clone(&connected), Arc::clone(&served));
    run_program(
        seed,
        move |node, tasks| async move {
            node.set_wall_error(wall_error);
            let (test, session, link) =
                accept(&node, &tasks, true, Some(rules())).await;
            let mut hello = session.accept().await.expect("a hello stream");
            header(&mut hello).await;
            let result = link.serve(hello).await.map(drop);
            *home_end.lock().expect("not poisoned") = Some(result);
            drop((link, test));
        },
        move |node, tasks, at| async move {
            let result = connect(&node, tasks, at, AGENT).await.map(drop);
            *session_end.lock().expect("not poisoned") = Some(result);
        },
    );
    let served = served.lock().expect("not poisoned").take();
    let connected = connected.lock().expect("not poisoned").take();
    assert_eq!(connected, Some(Ok(())), "the home gave {served:?}");
}

#[test]
fn connects_to_a_node_whose_mesh_time_has_an_error_of_minutes() {
    connects_at_wall_error(170, Some(Span::from_nanos(3 * Span::MINUTE.nanos())));
}

#[test]
fn connects_to_a_node_whose_mesh_time_has_an_unknown_error() {
    connects_at_wall_error(171, None);
}

/// A node with no time source admits a program, then gets one, so its error
/// shrinks from unknown to 10 ms. The program's session lives on: its renewals pass.
#[test]
fn keeps_a_session_when_the_error_of_mesh_time_shrinks() {
    let (got, served) = (Arc::new(Mutex::new(None)), Arc::new(Mutex::new(Vec::new())));
    let (kept, seen) = (Arc::clone(&got), Arc::clone(&served));
    run_program(
        172,
        move |node, tasks| async move {
            shrink_wall_error(&node, &tasks);
            let (test, session, link) =
                accept(&node, &tasks, true, Some(rules())).await;
            crate::sessions::serve_each(
                &session,
                &link,
                &tasks,
                &node.clock(),
                &seen,
                &Rc::default(),
            )
            .await;
            drop((link, test));
        },
        move |node, tasks, at| async move {
            let session = connect(&node, tasks, at, AGENT).await.expect("connects");
            node.clock()
                .sleep(Span::from_nanos(2 * LIFE.nanos() + Span::SECOND.nanos()))
                .await;
            let late = session.request(b"late").await;
            *kept.lock().expect("not poisoned") = Some(late);
            drop(session);
            node.clock().sleep(QUIET).await;
        },
    );
    let served = std::mem::take(&mut *served.lock().expect("not poisoned"));
    let got = got.lock().expect("not poisoned").take();
    assert_eq!(got, Some(Ok(b"etal".to_vec())), "the home gave {served:?}");
}
