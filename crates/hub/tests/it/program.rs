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

use access::proof::Error as Refusal;
use hub::client::{Client, Config, Error, LIFE};
use hub::serve;
use transport::stream::Incoming;
use transport::{Address, Code, Port};
use types::ed25519::PrivateKey;
use types::time::Span;
use wire::header::MALFORMED;
use wire::hub::BUSY;
use wire::hub::client::{
    BODY_BYTES_MAX, CAPPED, CHANGED, EXPIRED, REFUSED, Response, STALE, UNSYNCED, VIA,
};

use super::client::{
    AGENT, Got, OTHER, QUIET, SUBJECT, header, home, name, rules, run_program,
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
    let pool = own_pool();
    let own = SocketAddr::new(node.addresses()[0], PORT);
    let mut parts = Port::bind(&node.net(), own)
        .expect("binds")
        .split(NonZeroUsize::MIN);
    let config = transport::client::Config {
        clock: node.clock(),
        entropy: node.entropy(),
        tasks: tasks.clone(),
        pool: Rc::clone(&pool),
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
    assert_eq!(got, Some(Err(Error::Stopped { code: REFUSED })));
    let refusal = Refusal::Unlisted {
        subject: name(SUBJECT),
        key: public_key(&OTHER),
    };
    assert_eq!(home.served, [Err(serve::Error::Access(refusal))]);
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
    assert_eq!(got, Some(Err(Error::Stopped { code: UNSYNCED })));
    assert_eq!(home.served, [Err(serve::Error::Access(Refusal::Unsynced))]);
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
    let home = with_client(125, |client, _| async move {
        let replies = |tag: u8, client: Client| async move {
            for i in 0..EACH {
                let body = [tag, u8::try_from(i).expect("small")];
                assert_eq!(client.request(&body).await, Ok(reversed(&body)));
            }
        };
        let mut a = pin!(replies(1, client.clone()));
        let mut b = pin!(replies(2, client));
        let (a_done, b_done) = (Cell::new(false), Cell::new(false));
        poll_fn(|cx| {
            if !a_done.get() {
                a_done.set(a.as_mut().poll(cx).is_ready());
            }
            if !b_done.get() {
                b_done.set(b.as_mut().poll(cx).is_ready());
            }
            if a_done.get() && b_done.get() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    });
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
        let test = home(&node, &tasks, POOL, true).await;
        test.hub.set_rules(rules());
        let transport = transport(&node, &tasks, &own_pool(), HOME, 1 << 16);
        let session = transport.accept().await.expect("a session");
        let link = test.hub.link(session.clone());
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

#[test]
fn gives_the_code_of_a_node_that_closed_the_session() {
    by_hand(
        127,
        |session, node| async move {
            node.clock().sleep(QUIET).await;
            session.close(Code(BUSY));
            node.clock().sleep(QUIET).await;
        },
        |client, node| {
            Box::pin(async move {
                node.clock().sleep(QUIET).await;
                node.clock().sleep(QUIET).await;
                assert_eq!(
                    client.request(b"ab").await,
                    Err(Error::Stopped { code: BUSY })
                );
            })
        },
    );
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

#[test]
fn names_the_cause_of_each_stop_code() {
    let cases = [
        (
            Error::Stopped { code: 0 },
            "the node stopped with code 0: the node closed with no cause",
        ),
        (
            Error::Stopped { code: MALFORMED },
            "the node stopped with code 2: the client broke the client wire",
        ),
        (
            Error::Stopped { code: BUSY },
            "the node stopped with code 19: the node had no block for the response",
        ),
        (
            Error::Stopped { code: REFUSED },
            "the node stopped with code 20: the spec has no such subject, does not \
             list the key for it, or the signature is not valid",
        ),
        (
            Error::Stopped { code: UNSYNCED },
            "the node stopped with code 21: the node has no mesh time yet",
        ),
        (
            Error::Stopped { code: STALE },
            "the node stopped with code 22: the hello does not echo the nonce of the \
             node's last challenge",
        ),
        (
            Error::Stopped { code: VIA },
            "the node stopped with code 23: the hello names another node as via",
        ),
        (
            Error::Stopped { code: EXPIRED },
            "the node stopped with code 24: the hello expired",
        ),
        (
            Error::Stopped { code: CAPPED },
            "the node stopped with code 25: the hello expires past the cap",
        ),
        (
            Error::Stopped { code: CHANGED },
            "the node stopped with code 26: the renewal changed the subject, key, via, \
             or connection",
        ),
        (
            Error::Stopped { code: 99 },
            "the node stopped with code 99: the client does not know the code",
        ),
    ];
    for (error, text) in cases {
        assert_eq!(error.to_string(), text);
    }
}

#[test]
fn names_each_error() {
    let cases = [
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

/// A node's reset and close with a code each give `Stopped`, so a program sees one
/// error for a refusal whichever frame comes first.
#[test]
fn gives_a_reset_and_a_close_as_one_stop() {
    let code = Code(REFUSED);
    for error in [
        transport::Error::Reset { code },
        transport::Error::PeerClosed { code },
    ] {
        assert_eq!(Error::from(error), Error::Stopped { code: REFUSED });
    }
    assert_eq!(
        Error::from(transport::Error::TimedOut),
        Error::Transport(transport::Error::TimedOut)
    );
}
