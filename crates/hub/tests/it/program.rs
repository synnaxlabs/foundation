//! `hub::client::Client`, a program's session, against a simulated node that serves
//! it on one `hub::Link`.

use std::cell::Cell;
use std::future::poll_fn;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use hub::client::{Client, Config, Error, LIFE};
use hub::serve;
use transport::stream::Incoming;
use transport::{Address, Code, Port};
use types::ed25519::PrivateKey;
use types::time::{Interval, Span, Stamp};
use wire::header::MALFORMED;
use wire::hub::BUSY;
use wire::hub::client::{BODY_BYTES_MAX, Challenge, REFUSED, Refusal, Response};

use super::client::{
    AGENT, Got, OTHER, QUIET, SUBJECT, accept, header, name, rules, run_program,
    serve_session,
};
use super::serve::{HOME, PORT, own_pool, public_key, transport};
use super::{NODE, POOL};

/// Connects to the home at `at` from `node` as [`SUBJECT`], signing with `key`.
async fn connect(
    node: &sim::node::Node,
    tasks: env::tasks::Tasks,
    at: Address,
    key: PrivateKey,
) -> Result<Client, Error> {
    connect_with(node, tasks, at, key, own_pool()).await
}

/// As [`connect`], where the client sends from `pool`, and the transport from a pool
/// of its own.
async fn connect_with(
    node: &sim::node::Node,
    tasks: env::tasks::Tasks,
    at: Address,
    key: PrivateKey,
    pool: Rc<block::Pool>,
) -> Result<Client, Error> {
    let own = SocketAddr::new(node.addresses()[0], PORT);
    let mut parts = Port::bind(&node.net(), own)
        .expect("binds")
        .split(NonZeroUsize::MIN);
    let config = transport::client::Config {
        clock: node.clock(),
        entropy: node.entropy(),
        tasks: tasks.clone(),
        pool: own_pool(),
    };
    let transport = transport::Client::new(config, parts.pop().expect("one part"))
        .expect("a client");
    let config = Config {
        via: NODE,
        node: public_key(&HOME),
        addresses: vec![at],
        subject: name(SUBJECT),
        key,
        clock: node.clock(),
        entropy: node.entropy(),
        tasks,
        pool,
    };
    Client::connect(&transport, config).await
}

/// Runs `program` with a client of [`SUBJECT`] on a home with mesh time and the
/// rules of `rules`, and gives what the home saw.
fn with_client<P>(
    seed: u64,
    program: impl FnOnce(Client, sim::node::Node) -> P + Send + 'static,
) -> super::client::Home
where
    P: Future<Output = ()> + 'static,
{
    serve_session(
        seed,
        true,
        POOL,
        Some(rules()),
        |node, tasks, at| async move {
            let client = connect(&node, tasks, at, AGENT).await.expect("connects");
            program(client, node.clone()).await;
            node.clock().sleep(QUIET).await;
        },
    )
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
    let home = with_client(120, move |client, _| async move {
        assert_eq!(client.request(b"").await, Ok(Vec::new()));
        assert_eq!(client.request(&sent).await, Ok(reversed(&sent)));
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
    let home = with_client(121, |client, node| async move {
        let clone = client.clone();
        drop(client);
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

#[test]
fn refuses_a_hello_of_a_key_the_spec_does_not_list() {
    let got = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&got);
    let home = serve_session(
        122,
        true,
        POOL,
        Some(rules()),
        |node, tasks, at| async move {
            let refused = connect(&node, tasks, at, OTHER).await.map(drop);
            *kept.lock().expect("not poisoned") = Some(refused);
        },
    );
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
    let home = serve_session(
        123,
        false,
        POOL,
        Some(rules()),
        |node, tasks, at| async move {
            let refused = connect(&node, tasks, at, AGENT).await.map(drop);
            *kept.lock().expect("not poisoned") = Some(refused);
        },
    );
    let got = got.lock().expect("not poisoned").take();
    assert_eq!(got, Some(Err(Error::Refused(Refusal::Unsynced))));
    assert_eq!(
        home.served,
        [Err(serve::Error::Access(access::proof::Error::Unsynced))]
    );
}

/// The client renews the hello, so a request long after the first hello's expiry
/// gets its reply, and the hello stream stays open.
#[test]
fn renews_the_hello_before_it_expires() {
    let home = with_client(124, |client, node| async move {
        node.clock()
            .sleep(Span::from_nanos(2 * LIFE.nanos() + Span::SECOND.nanos()))
            .await;
        assert_eq!(client.request(b"late").await, Ok(b"etal".to_vec()));
    });
    assert_eq!(
        home.served[0],
        Ok(Got::Request(name(SUBJECT), b"late".to_vec()))
    );
}

/// Two tasks that call `request` on clones of one client at once each get their own
/// replies, and the node never stops one as `MALFORMED`.
#[test]
fn takes_requests_from_two_tasks_one_at_a_time() {
    const EACH: usize = 8;
    let home = serve_session(
        125,
        true,
        POOL,
        Some(rules()),
        |node, tasks, at| async move {
            let client = connect(&node, tasks.clone(), at, AGENT)
                .await
                .expect("connects");
            let replied = Rc::new(Cell::new(0));
            for tag in [1, 2] {
                let (client, replied) = (client.clone(), Rc::clone(&replied));
                tasks.spawn(async move {
                    for i in 0..EACH {
                        let body = [tag, u8::try_from(i).expect("small")];
                        assert_eq!(client.request(&body).await, Ok(reversed(&body)));
                        replied.set(replied.get() + 1);
                    }
                });
            }
            node.clock()
                .sleep(Span::from_nanos(10 * Span::SECOND.nanos()))
                .await;
            assert_eq!(replied.get(), 2 * EACH);
        },
    );
    let requests = home
        .served
        .iter()
        .filter(|got| matches!(got, Ok(Got::Request(..))))
        .count();
    assert_eq!(requests, 2 * EACH, "{:?}", home.served);
}

#[test]
fn refuses_a_body_over_the_cap_with_nothing_sent() {
    let home = with_client(126, |client, _| async move {
        let length = usize::try_from(BODY_BYTES_MAX).expect("fits") + 1;
        let refused = client.request(&vec![0; length]).await;
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

/// Runs a program whose client connects to a home that admits its hello on a link,
/// then gives the session to `rest`, which serves what follows by hand.
fn by_hand<R>(
    seed: u64,
    rest: impl FnOnce(transport::Session, sim::node::Node) -> R + Send + 'static,
    program: impl FnOnce(
        Client,
        sim::node::Node,
    ) -> std::pin::Pin<Box<dyn Future<Output = ()>>>
    + Send
    + 'static,
) where
    R: Future<Output = ()> + 'static,
{
    let home = move |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let (test, session, link) =
            accept(&node, &tasks, POOL, true, Some(rules())).await;
        let mut hello = session.accept().await.expect("a stream");
        header(&mut hello).await;
        let hello = link.serve(hello);
        tasks.spawn(async move {
            drop(hello.await);
        });
        rest(session, node).await;
        drop((link, test));
    };
    run_program(seed, home, |node, tasks, at| async move {
        let client = connect(&node, tasks, at, AGENT).await.expect("connects");
        program(client, node).await;
    });
}

/// The next request stream of `session`, read to its finish.
async fn read_request(session: &transport::Session) -> Incoming {
    let mut incoming = session.accept().await.expect("a stream");
    header(&mut incoming).await;
    while incoming.receiver.recv().await.expect("a message").is_some() {}
    incoming
}

/// A close with a code of `Refusal` gives the refusal, and a close with 0 or another
/// code gives the transport error.
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
            move |client, node| {
                Box::pin(async move {
                    node.clock().sleep(QUIET).await;
                    node.clock().sleep(QUIET).await;
                    assert_eq!(client.request(b"ab").await, Err(error));
                })
            },
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
        |client, _| {
            Box::pin(async move {
                assert_eq!(client.request(b"ab").await, Err(Error::Unanswered));
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
        |client, _| {
            Box::pin(async move {
                assert_eq!(
                    client.request(b"abcd").await,
                    Err(Error::Message(wire::hub::Error::Unfinished { remain: 2 }))
                );
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
        |client, node| {
            Box::pin(async move {
                let mut reply = pin!(client.request(b"ab"));
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

/// A program that gives up on a slow request, then sends its next request, gets the
/// reply of the next request: the node holds the first open until it replies.
#[test]
fn sends_the_next_request_after_a_dropped_one() {
    let home = move |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let (test, session, link) =
            accept(&node, &tasks, POOL, true, Some(rules())).await;
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
    };
    run_program(133, home, |node, tasks, at| async move {
        let client = connect(&node, tasks, at, AGENT).await.expect("connects");
        {
            let mut slow = pin!(client.request(b"slow"));
            let mut quiet = pin!(node.clock().sleep(QUIET));
            let first = poll_fn(|cx| match slow.as_mut().poll(cx) {
                Poll::Ready(got) => Poll::Ready(Some(got)),
                Poll::Pending => quiet.as_mut().poll(cx).map(|()| None),
            })
            .await;
            assert_eq!(first, None, "the first request is dropped while it waits");
        }
        assert_eq!(client.request(b"ab").await, Ok(b"ba".to_vec()));
    });
}

/// Runs a program whose client connects to a home that serves the hello stream by
/// hand with `hello`, with no hub.
fn raw<H, P>(
    seed: u64,
    hello: impl FnOnce(transport::Session, Incoming, sim::node::Node) -> H + Send + 'static,
    program: impl FnOnce(sim::node::Node, env::tasks::Tasks, Address) -> P + Send + 'static,
) where
    H: Future<Output = ()> + 'static,
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

/// A challenge that `wire` refuses ends the renewal, closes the session with
/// `MALFORMED`, and each later request gives the error.
#[test]
fn closes_the_session_on_a_challenge_that_is_not_valid() {
    raw(
        135,
        |session, mut hello, _| async move {
            let sender = hello.sender.as_mut().expect("two-way");
            let pool = own_pool();
            let mut challenge = [0; Challenge::LEN];
            Challenge {
                nonce: [7; 16],
                now: Interval {
                    earliest: Stamp::from_nanos(0),
                    latest: Stamp::from_nanos(0),
                },
            }
            .encode(&mut challenge);
            for _ in 0..2 {
                sender
                    .send(pool.copy(&challenge).expect("room"))
                    .await
                    .expect("sends");
                hello
                    .receiver
                    .recv()
                    .await
                    .expect("a hello")
                    .expect("not finished");
            }
            let refused = pool.copy(&[0xff]).expect("room");
            sender.send(refused).await.expect("sends");
            assert_eq!(
                session.closed().await,
                transport::Error::PeerClosed {
                    code: Code(MALFORMED)
                }
            );
        },
        |node, tasks, at| async move {
            let client = connect(&node, tasks, at, AGENT).await.expect("connects");
            node.clock().sleep(LIFE).await;
            assert_eq!(
                client.request(b"ab").await,
                Err(Error::Message(wire::hub::Error::Kind { kind: 0xff }))
            );
        },
    );
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
    let home = serve_session(
        136,
        true,
        POOL,
        Some(rules()),
        |node, tasks, at| async move {
            let pool = own_pool();
            let client = connect_with(&node, tasks, at, AGENT, Rc::clone(&pool))
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
            assert_eq!(client.request(b"late").await, Ok(b"etal".to_vec()));
            node.clock().sleep(QUIET).await;
        },
    );
    assert_eq!(
        home.served[0],
        Ok(Got::Request(name(SUBJECT), b"late".to_vec()))
    );
}

#[test]
fn names_each_error() {
    let cases = [
        (Error::Refused(Refusal::Expired), "the hello expired"),
        (
            Error::Transport(transport::Error::TimedOut),
            "the session failed: the peer stopped answering",
        ),
        (
            Error::Message(wire::hub::Error::Empty),
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
            "the pool had no block for a message: pool is full: asked for 9 bytes, 0 \
             bytes free",
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
