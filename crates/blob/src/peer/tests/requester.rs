//! `get` and `put` against `serve`, and against a peer that sends raw `wire::blob`
//! messages.

use super::*;
use crate::peer;

/// The digests of a get message: a 1-byte head and 32 bytes for each digest, in the
/// bytes that `transport` copies.
const BATCH: usize = (COPIED - 1) / 32;

/// Serves each stream of `session` from `store` until the session ends, and gives
/// what each `serve` gave.
async fn serve_each(store: &Store, session: &Session) -> Vec<Result<(), Error>> {
    let mut served = Vec::new();
    while let Ok(incoming) = accept(session).await {
        served.push(serve(store, incoming).await);
    }
    served
}

/// The server's end of a stream, which a test drives by hand.
fn raw(incoming: Incoming) -> Stream {
    Stream {
        pool: create_pool(),
        sender: incoming.sender.expect("a two-way stream"),
        receiver: incoming.receiver,
    }
}

impl Stream {
    /// The digests of the next get.
    async fn request(&mut self) -> Vec<Digest> {
        let message = self.recv().await.expect("a get").expect("not the end");
        match Server::new(usize::MAX).decode(&message) {
            Ok(FromRequester::Get(digests)) => digests.collect(),
            other => panic!("not a get: {other:?}"),
        }
    }

    /// The next put and its body.
    async fn take_put(&mut self) -> (Put, Vec<u8>) {
        let message = self.recv().await.expect("a put").expect("not the end");
        let Ok(FromRequester::Put(put)) = Server::new(usize::MAX).decode(&message)
        else {
            panic!("not a put");
        };
        let mut bytes = Vec::new();
        while bytes.len() < usize::try_from(put.len).unwrap() {
            bytes.extend(self.recv().await.unwrap().unwrap());
        }
        (put, bytes)
    }

    async fn answer(&mut self, reply: Reply) {
        let mut out = vec![0; reply.encoded_len()];
        reply.encode(&mut out);
        self.send(&out).await;
    }
}

/// The bytes that `store` holds under `digest`, or `None`.
async fn held(store: &Store, digest: Digest) -> Option<Vec<u8>> {
    store.get(digest).await.unwrap().map(|block| block.to_vec())
}

#[test]
fn a_get_stores_each_chunk_and_gives_each_absent_in_order() {
    let (a, a_bytes) = chunk(1, 3000);
    let (b, b_bytes) = chunk(2, 0);
    let (absent, _) = chunk(3, 100);
    let (other, _) = chunk(4, 100);
    let stored = (a_bytes.clone(), b_bytes.clone());
    run_session(
        0,
        DISK,
        move |node, session| async move {
            let store = open(&node, 0).await;
            put(&store, a, &stored.0).await;
            put(&store, b, &stored.1).await;
            assert_eq!(serve_each(&store, &session).await, vec![Ok(())]);
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            let got = get(&store, &peer.session, &[absent, a, other, b]).await;
            let miss = |digest| Miss {
                digest,
                cause: Cause::Absent,
            };
            assert_eq!(got, Ok(vec![miss(absent), miss(other)]));
            assert_eq!(held(&store, a).await, Some(a_bytes));
            assert_eq!(held(&store, b).await, Some(b_bytes));
            assert_eq!(held(&store, absent).await, None);
        },
    );
}

#[test]
fn a_get_of_no_digests_opens_no_stream() {
    run_session(
        0,
        DISK,
        move |node, session| async move {
            let store = open(&node, 0).await;
            assert_eq!(serve_each(&store, &session).await, vec![]);
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            assert_eq!(get(&store, &peer.session, &[]).await, Ok(vec![]));
        },
    );
}

#[test]
fn a_get_of_more_digests_than_one_message_holds_gives_each() {
    let (last, last_bytes) = chunk(1, 100);
    let stored = last_bytes.clone();
    let mut digests: Vec<Digest> = (0..2 * BATCH)
        .map(|at| Digest::of(&at.to_le_bytes()))
        .collect();
    digests.push(last);
    run_session(
        0,
        DISK,
        move |node, session| async move {
            let store = open(&node, 0).await;
            put(&store, last, &stored).await;
            assert_eq!(serve_each(&store, &session).await, vec![Ok(())]);
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            let got = get(&store, &peer.session, &digests).await.unwrap();
            let absent: Vec<Miss> = digests[..2 * BATCH]
                .iter()
                .map(|&digest| Miss {
                    digest,
                    cause: Cause::Absent,
                })
                .collect();
            assert!(got == absent, "each digest but the last is absent");
            assert_eq!(held(&store, last).await, Some(last_bytes));
        },
    );
}

#[test]
fn a_get_asks_for_as_many_digests_as_transport_copies() {
    let digests: Vec<Digest> = (0..=BATCH)
        .map(|at| Digest::of(&at.to_le_bytes()))
        .collect();
    let asked = digests.clone();
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            for batch in [&asked[..BATCH], &asked[BATCH..]] {
                assert!(stream.request().await == batch, "a get of the batch");
                for &digest in batch {
                    stream.answer(Reply::Absent { digest }).await;
                }
            }
            stream.finish().await;
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            let got = get(&store, &peer.session, &digests).await.unwrap();
            assert_eq!(got.len(), BATCH + 1);
        },
    );
}

#[test]
fn a_get_asks_for_as_many_digests_as_the_largest_block_of_the_store_holds() {
    let create_tight_pool = || create_pool_of(64 + 1024 + 128);
    assert_eq!(create_tight_pool().largest(), 1024);
    let digests: Vec<Digest> = (0..200_usize)
        .map(|at| Digest::of(&at.to_le_bytes()))
        .collect();
    let expected: Vec<Miss> = digests
        .iter()
        .map(|&digest| Miss {
            digest,
            cause: Cause::Absent,
        })
        .collect();
    run_session(
        0,
        DISK,
        move |node, session| async move {
            let store = open(&node, 0).await;
            assert_eq!(serve_each(&store, &session).await, vec![Ok(())]);
        },
        move |peer| async move {
            let store = open_on(&peer.node, create_tight_pool(), 0).await;
            let got = get(&store, &peer.session, &digests).await;
            assert_eq!(got, Ok(expected));
        },
    );
}

#[test]
fn a_get_batch_is_the_digests_that_the_largest_block_holds() {
    let create_tight_pool = || create_pool_of(64 + 1024 + 128);
    let batch: usize = (1024 - 1) / 32;
    let digests: Vec<Digest> = (0..=batch)
        .map(|at| Digest::of(&at.to_le_bytes()))
        .collect();
    let asked = digests.clone();
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            for batch in [&asked[..batch], &asked[batch..]] {
                assert!(stream.request().await == batch, "a get of the batch");
                for &digest in batch {
                    stream.answer(Reply::Absent { digest }).await;
                }
            }
            stream.finish().await;
        },
        move |peer| async move {
            let store = open_on(&peer.node, create_tight_pool(), 0).await;
            let got = get(&store, &peer.session, &digests).await.unwrap();
            assert_eq!(got.len(), batch + 1);
        },
    );
}

#[test]
fn a_chunk_that_hashes_to_another_digest_gives_mismatch_and_the_rest_come_again() {
    let (a, _) = chunk(1, 3000);
    let (found, bytes) = chunk(9, 3000);
    let (b, b_bytes) = chunk(2, 100);
    let stored = b_bytes.clone();
    run_session(
        0,
        DISK,
        move |node, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.request().await, vec![a, b]);
            stream
                .answer(Reply::Chunk {
                    digest: a,
                    len: 3000,
                })
                .await;
            for part in bytes.chunks(MESSAGE) {
                stream.send(part).await;
            }
            assert_eq!(stream.reset().await, reset(MISMATCH));
            let store = open(&node, 0).await;
            put(&store, b, &stored).await;
            assert_eq!(serve_each(&store, &session).await, vec![Ok(())]);
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            let got = get(&store, &peer.session, &[a, b]).await;
            let cause = Cause::Mismatch { found };
            assert_eq!(got, Ok(vec![Miss { digest: a, cause }]));
            assert_eq!(held(&store, a).await, None);
            assert_eq!(held(&store, found).await, None);
            assert_eq!(held(&store, b).await, Some(b_bytes));
        },
    );
}

#[test]
fn a_chunk_over_the_largest_block_gives_too_large_and_the_rest_come_again() {
    let max = create_pool().largest();
    let len = u32::try_from(max + 1).unwrap();
    let (a, _) = chunk(1, 100);
    let (b, b_bytes) = chunk(2, 100);
    let stored = b_bytes.clone();
    run_session(
        0,
        DISK,
        move |node, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.request().await, vec![a, b]);
            stream.answer(Reply::Chunk { digest: a, len }).await;
            assert_eq!(stream.reset().await, reset(TOO_LARGE));
            let store = open(&node, 0).await;
            put(&store, b, &stored).await;
            assert_eq!(serve_each(&store, &session).await, vec![Ok(())]);
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            let got = get(&store, &peer.session, &[a, b]).await;
            let cause = Cause::TooLarge { len, max };
            assert_eq!(got, Ok(vec![Miss { digest: a, cause }]));
            assert_eq!(held(&store, b).await, Some(b_bytes));
        },
    );
}

#[test]
fn a_chunk_over_the_largest_block_of_another_digest_gives_unexpected() {
    let len = u32::try_from(create_pool().largest() + 1).unwrap();
    let (a, _) = chunk(1, 100);
    let (b, _) = chunk(2, 100);
    let reply = Reply::Chunk { digest: b, len };
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.request().await, vec![a]);
            stream.answer(reply).await;
            assert_eq!(stream.reset().await, reset(MALFORMED));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            let got = get(&store, &peer.session, &[a]).await;
            assert_eq!(got, Err(Error::Unexpected { reply }));
        },
    );
}

#[test]
fn a_stream_reset_in_a_chunk_ends_the_get_and_stores_nothing() {
    let (a, bytes) = chunk(1, 3000);
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.request().await, vec![a]);
            stream
                .answer(Reply::Chunk {
                    digest: a,
                    len: 3000,
                })
                .await;
            stream.send(&bytes[..1000]).await;
            stream.sender.reset(Code(0));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            let got = get(&store, &peer.session, &[a]).await;
            assert_eq!(got, Err(Error::Stream(reset(0))));
            assert_eq!(held(&store, a).await, None);
        },
    );
}

#[test]
fn a_reply_of_another_digest_gives_unexpected_and_stops_as_malformed() {
    let (a, _) = chunk(1, 100);
    let (b, _) = chunk(2, 100);
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.request().await, vec![a]);
            stream.answer(Reply::Absent { digest: b }).await;
            assert_eq!(stream.reset().await, reset(MALFORMED));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            let reply = Reply::Absent { digest: b };
            let got = get(&store, &peer.session, &[a]).await;
            assert_eq!(got, Err(Error::Unexpected { reply }));
        },
    );
}

#[test]
fn a_chunk_of_another_digest_gives_unexpected_and_stops_as_malformed() {
    let (a, _) = chunk(1, 100);
    let (b, _) = chunk(2, 100);
    let reply = Reply::Chunk {
        digest: b,
        len: 100,
    };
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.request().await, vec![a]);
            stream.answer(reply).await;
            assert_eq!(stream.reset().await, reset(MALFORMED));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            let got = get(&store, &peer.session, &[a]).await;
            assert_eq!(got, Err(Error::Unexpected { reply }));
        },
    );
}

#[test]
fn a_reply_after_the_last_answer_gives_unexpected() {
    let (a, _) = chunk(1, 100);
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.request().await, vec![a]);
            stream.answer(Reply::Absent { digest: a }).await;
            stream.answer(Reply::Stored { digest: a }).await;
            assert_eq!(stream.reset().await, reset(MALFORMED));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            let reply = Reply::Stored { digest: a };
            let got = get(&store, &peer.session, &[a]).await;
            assert_eq!(got, Err(Error::Unexpected { reply }));
        },
    );
}

#[test]
fn a_chunk_over_the_largest_block_after_the_last_answer_gives_unexpected() {
    let (a, _) = chunk(1, 100);
    let reply = Reply::Chunk {
        digest: a,
        len: u32::MAX,
    };
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.request().await, vec![a]);
            stream.answer(Reply::Absent { digest: a }).await;
            stream.answer(reply).await;
            assert_eq!(stream.reset().await, reset(MALFORMED));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            let got = get(&store, &peer.session, &[a]).await;
            assert_eq!(got, Err(Error::Unexpected { reply }));
        },
    );
}

#[test]
fn a_reply_that_does_not_decode_stops_with_its_code() {
    let (a, _) = chunk(1, 100);
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.request().await, vec![a]);
            stream.send(&[9]).await;
            assert_eq!(stream.reset().await, reset(MALFORMED));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            let kind = wire::blob::Error::Kind { kind: 9 };
            let got = get(&store, &peer.session, &[a]).await;
            assert_eq!(got, Err(Error::Wire(kind)));
        },
    );
}

#[test]
fn a_peer_that_finishes_with_digests_open_gives_unanswered() {
    let (a, _) = chunk(1, 100);
    let (b, _) = chunk(2, 100);
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.request().await, vec![a, b]);
            stream.answer(Reply::Absent { digest: a }).await;
            stream.sender.finish().unwrap();
            assert_eq!(stream.reset().await, reset(MALFORMED));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            let got = get(&store, &peer.session, &[a, b]).await;
            assert_eq!(got, Err(Error::Unanswered));
        },
    );
}

#[test]
fn a_get_whose_store_refuses_the_chunk_ends_the_stream_with_code_0() {
    let (a, bytes) = chunk(1, 3000);
    let stored = bytes.clone();
    run_session(
        0,
        DISK,
        move |node, session| async move {
            let store = open(&node, 0).await;
            put(&store, a, &stored).await;
            let served = serve_each(&store, &session).await;
            assert_eq!(served, vec![Err(Error::Stream(reset(0)))]);
        },
        move |peer| async move {
            let free_bytes = free(&peer.node).await;
            let floor_bytes = free_bytes - 2999;
            let store = open(&peer.node, floor_bytes).await;
            let floor = crate::Error::Floor {
                len: 3000,
                free_bytes,
                floor_bytes,
            };
            let got = get(&store, &peer.session, &[a]).await;
            assert_eq!(got, Err(Error::Store(floor)));
            assert_eq!(held(&store, a).await, None);
        },
    );
}

#[test]
fn a_dropped_get_stores_nothing_and_ends_the_stream_with_code_0() {
    let (a, bytes) = chunk(1, 3000);
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.request().await, vec![a]);
            stream
                .answer(Reply::Chunk {
                    digest: a,
                    len: 3000,
                })
                .await;
            stream.send(&bytes[..1000]).await;
            assert_eq!(stream.reset().await, reset(0));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            let digests = [a];
            {
                let mut got = pin!(get(&store, &peer.session, &digests));
                for _ in 0..10 {
                    let polled = poll_fn(|cx| Poll::Ready(got.as_mut().poll(cx)));
                    assert_eq!(polled.await, Poll::Pending);
                    peer.clock.sleep(Span::MILLISECOND).await;
                }
            }
            assert_eq!(held(&store, a).await, None);
        },
    );
}

#[test]
fn a_put_returns_once_each_chunk_is_durable_on_the_peer() {
    let (a, a_bytes) = chunk(1, 3000);
    let (b, b_bytes) = chunk(2, 0);
    let sent = (a_bytes.clone(), b_bytes.clone());
    let (mut sim, node) = run_session(
        0,
        DISK,
        move |node, session| async move {
            let store = open(&node, 0).await;
            assert_eq!(serve_each(&store, &session).await, vec![Ok(())]);
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            put(&store, a, &sent.0).await;
            put(&store, b, &sent.1).await;
            assert_eq!(peer::put(&store, &peer.session, &[a, b]).await, Ok(()));
        },
    );
    sim.crash(&node, Crash::Power);
    sim.run_on(&node, move |node, _| async move {
        let store = open(&node, 0).await;
        assert_eq!(held(&store, a).await, Some(a_bytes));
        assert_eq!(held(&store, b).await, Some(b_bytes));
    })
    .unwrap();
}

#[test]
fn a_put_frees_a_chunk_before_a_reordered_ack() {
    // Room for one chunk of the largest class and the head blocks.
    fn create_tight_pool() -> Rc<Pool> {
        create_pool_of(64 + 4096 + 128)
    }
    let (a, a_bytes) = chunk(1, 4000);
    let (b, b_bytes) = chunk(2, 4000);
    let held_bytes = (a_bytes.clone(), b_bytes.clone());
    // The jitter brings the server's ack of a chunk after its `Stored`.
    let sim = sim::Config {
        seed: 0,
        link: sim::link::Config {
            jitter: Span::from_nanos(500 * Span::MICROSECOND.nanos()),
            ..sim::link::Config::default()
        },
        ..sim::Config::default()
    };
    run_session_on(
        sim,
        DISK,
        [create_pool, create_pool],
        move |node, session, _| async move {
            let store = open(&node, 0).await;
            assert_eq!(serve_each(&store, &session).await, vec![Ok(())]);
            assert_eq!(held(&store, a).await, Some(held_bytes.0));
            assert_eq!(held(&store, b).await, Some(held_bytes.1));
        },
        move |peer| async move {
            let store = open_on(&peer.node, create_tight_pool(), 0).await;
            put(&store, a, &a_bytes).await;
            put(&store, b, &b_bytes).await;
            assert_eq!(peer::put(&store, &peer.session, &[a, b]).await, Ok(()));
        },
    );
}

#[test]
fn a_put_of_no_digests_opens_no_stream() {
    run_session(
        0,
        DISK,
        move |node, session| async move {
            let store = open(&node, 0).await;
            assert_eq!(serve_each(&store, &session).await, vec![]);
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            assert_eq!(peer::put(&store, &peer.session, &[]).await, Ok(()));
        },
    );
}

#[test]
fn a_put_under_the_floor_of_the_peer_gives_refused_with_full() {
    let (a, bytes) = chunk(1, 3000);
    run_session(
        0,
        DISK,
        move |node, session| async move {
            let free_bytes = free(&node).await;
            let floor_bytes = free_bytes - 2999;
            let store = open(&node, floor_bytes).await;
            let floor = crate::Error::Floor {
                len: 3000,
                free_bytes,
                floor_bytes,
            };
            let served = serve_each(&store, &session).await;
            assert_eq!(served, vec![Err(Error::Store(floor))]);
            assert_eq!(held(&store, a).await, None);
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            put(&store, a, &bytes).await;
            let refused = Error::Refused {
                digest: a,
                code: Code(FULL),
            };
            assert_eq!(peer::put(&store, &peer.session, &[a]).await, Err(refused));
        },
    );
}

#[test]
fn a_put_of_a_digest_that_the_store_lacks_gives_absent_and_sends_nothing_of_it() {
    let (a, a_bytes) = chunk(1, 100);
    let (absent, _) = chunk(2, 100);
    let sent = a_bytes.clone();
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.take_put().await.1, a_bytes);
            stream.answer(Reply::Stored { digest: a }).await;
            assert_eq!(stream.recv().await, Err(reset(0)));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            put(&store, a, &sent).await;
            let got = peer::put(&store, &peer.session, &[a, absent]).await;
            assert_eq!(got, Err(Error::Absent { digest: absent }));
        },
    );
}

#[test]
fn a_put_reset_with_code_0_fails_the_stream() {
    let (a, bytes) = chunk(1, 100);
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            let (put, _) = stream.take_put().await;
            assert_eq!(
                put,
                Put {
                    digest: a,
                    len: 100
                }
            );
            stream.sender.reset(Code(0));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            put(&store, a, &bytes).await;
            let got = peer::put(&store, &peer.session, &[a]).await;
            assert_eq!(got, Err(Error::Stream(reset(0))));
        },
    );
}

#[test]
fn a_dropped_put_ends_the_stream_with_code_0() {
    let (a, bytes) = chunk(1, 3000);
    let sent = bytes.clone();
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.take_put().await.1, sent);
            assert_eq!(stream.reset().await, reset(0));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            put(&store, a, &bytes).await;
            let digests = [a];
            let mut put = pin!(peer::put(&store, &peer.session, &digests));
            for _ in 0..10 {
                let polled = poll_fn(|cx| Poll::Ready(put.as_mut().poll(cx)));
                assert_eq!(polled.await, Poll::Pending);
                peer.clock.sleep(Span::MILLISECOND).await;
            }
        },
    );
}

#[test]
fn a_put_answered_with_a_chunk_over_the_largest_block_gives_unexpected() {
    let (a, bytes) = chunk(1, 100);
    let sent = bytes.clone();
    let reply = Reply::Chunk {
        digest: a,
        len: u32::MAX,
    };
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.take_put().await.1, sent);
            stream.answer(reply).await;
            assert_eq!(stream.reset().await, reset(MALFORMED));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            put(&store, a, &bytes).await;
            let got = peer::put(&store, &peer.session, &[a]).await;
            assert_eq!(got, Err(Error::Unexpected { reply }));
        },
    );
}

#[test]
fn a_put_answered_for_another_digest_gives_unexpected_and_stops_as_malformed() {
    let (a, bytes) = chunk(1, 3000);
    let (b, _) = chunk(2, 100);
    let sent = bytes.clone();
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.take_put().await.1, sent);
            stream.answer(Reply::Stored { digest: b }).await;
            assert_eq!(stream.reset().await, reset(MALFORMED));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            put(&store, a, &bytes).await;
            let reply = Reply::Stored { digest: b };
            let got = peer::put(&store, &peer.session, &[a]).await;
            assert_eq!(got, Err(Error::Unexpected { reply }));
        },
    );
}

#[test]
fn a_put_whose_peer_finishes_unanswered_gives_unanswered() {
    let (a, bytes) = chunk(1, 100);
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            stream.take_put().await;
            stream.sender.finish().unwrap();
            assert_eq!(stream.reset().await, reset(MALFORMED));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            put(&store, a, &bytes).await;
            let got = peer::put(&store, &peer.session, &[a]).await;
            assert_eq!(got, Err(Error::Unanswered));
        },
    );
}

#[test]
fn a_get_frees_the_head_of_a_chunk_before_it_takes_the_block_of_the_chunk() {
    // Room for the chunk and the messages of the transport, but not also the head.
    fn create_tight_pool() -> Rc<Pool> {
        create_pool_of(6848)
    }
    let (a, bytes) = chunk(1, 5120);
    let stored = bytes.clone();
    run_session_on(
        sim::Config::default(),
        DISK,
        [create_pool, create_tight_pool],
        move |node, session, _| async move {
            let store = open(&node, 0).await;
            put(&store, a, &stored).await;
            assert_eq!(serve_each(&store, &session).await, vec![Ok(())]);
        },
        move |peer| async move {
            let store = open_on(&peer.node, Rc::clone(&peer.pool), 0).await;
            assert_eq!(get(&store, &peer.session, &[a]).await, Ok(vec![]));
            assert_eq!(held(&store, a).await, Some(bytes));
        },
    );
}

#[test]
fn a_get_frees_its_get_message_before_a_reordered_ack() {
    // Room for one chunk of the largest class and the head blocks.
    fn create_tight_pool() -> Rc<Pool> {
        create_pool_of(64 + 4096 + 128)
    }
    let (c, bytes) = chunk(1, 4000);
    let stored = bytes.clone();
    let mut digests: Vec<Digest> = (0..126_usize)
        .map(|at| Digest::of(&at.to_le_bytes()))
        .collect();
    digests.insert(0, c);
    let absent = digests.len() - 1;
    // The jitter brings the server's ack of the get after its reply.
    let sim = sim::Config {
        seed: 2,
        link: sim::link::Config {
            jitter: Span::from_nanos(500 * Span::MICROSECOND.nanos()),
            ..sim::link::Config::default()
        },
        ..sim::Config::default()
    };
    run_session_on(
        sim,
        DISK,
        [create_pool, create_pool],
        move |node, session, _| async move {
            let store = open(&node, 0).await;
            put(&store, c, &stored).await;
            assert_eq!(serve_each(&store, &session).await, vec![Ok(())]);
        },
        move |peer| async move {
            let store = open_on(&peer.node, create_tight_pool(), 0).await;
            let got = get(&store, &peer.session, &digests).await;
            assert_eq!(got.map(|misses| misses.len()), Ok(absent));
            assert_eq!(held(&store, c).await, Some(bytes));
        },
    );
}

#[test]
fn serve_frees_each_chunk_before_it_takes_the_next() {
    // Room for one chunk of the largest class and the head blocks.
    fn create_tight_pool() -> Rc<Pool> {
        create_pool_of(64 + 4096 + 128)
    }
    let (a, a_bytes) = chunk(1, 4000);
    let (b, b_bytes) = chunk(2, 4000);
    let (a_held, b_held) = (a_bytes.clone(), b_bytes.clone());
    run_session_on(
        sim::Config::default(),
        DISK,
        [create_pool, create_pool],
        move |node, session, _| async move {
            let store = open_on(&node, create_tight_pool(), 0).await;
            put(&store, a, &a_bytes).await;
            put(&store, b, &b_bytes).await;
            assert_eq!(serve_each(&store, &session).await, vec![Ok(())]);
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            assert_eq!(get(&store, &peer.session, &[a, b]).await, Ok(vec![]));
            assert_eq!(held(&store, a).await, Some(a_held));
            assert_eq!(held(&store, b).await, Some(b_held));
        },
    );
}

// The peer waits for the second put before it ends anything, so a `Push::put` that
// waited for more than its `Stored` would never return.
#[test]
fn a_push_gives_back_each_put_once_the_peer_stored_its_chunk() {
    let (a, a_bytes) = chunk(1, 3000);
    let (b, b_bytes) = chunk(2, 100);
    let sent = (a_bytes.clone(), b_bytes.clone());
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.take_put().await.1, sent.0);
            stream.answer(Reply::Stored { digest: a }).await;
            assert_eq!(stream.take_put().await.1, sent.1);
            stream.answer(Reply::Stored { digest: b }).await;
            stream.finish().await;
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            put(&store, a, &a_bytes).await;
            put(&store, b, &b_bytes).await;
            let push = peer::Push::open(&store, &peer.session).await.unwrap();
            let push = push.put(a).await.unwrap();
            let push = push.put(b).await.unwrap();
            assert_eq!(push.finish().await, Ok(()));
        },
    );
}

#[test]
fn a_push_dropped_between_puts_ends_the_stream_with_code_0() {
    let (a, bytes) = chunk(1, 3000);
    let sent = bytes.clone();
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            assert_eq!(stream.take_put().await.1, sent);
            stream.answer(Reply::Stored { digest: a }).await;
            assert_eq!(stream.reset().await, reset(0));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            put(&store, a, &bytes).await;
            let push = peer::Push::open(&store, &peer.session).await.unwrap();
            drop(push.put(a).await.unwrap());
        },
    );
}

#[test]
fn a_reply_after_the_last_put_of_a_push_gives_unexpected_at_its_finish() {
    let (a, bytes) = chunk(1, 100);
    let reply = Reply::Stored { digest: a };
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            stream.take_put().await;
            stream.answer(reply).await;
            stream.answer(reply).await;
            assert_eq!(stream.reset().await, reset(MALFORMED));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            put(&store, a, &bytes).await;
            let push = peer::Push::open(&store, &peer.session).await.unwrap();
            let push = push.put(a).await.unwrap();
            assert_eq!(push.finish().await, Err(Error::Unexpected { reply }));
        },
    );
}

#[test]
fn a_reply_after_the_last_put_that_does_not_decode_stops_the_finish_with_its_code() {
    let (a, bytes) = chunk(1, 100);
    run_session(
        0,
        DISK,
        move |_, session| async move {
            let mut stream = raw(accept(&session).await.unwrap());
            stream.take_put().await;
            stream.answer(Reply::Stored { digest: a }).await;
            stream.send(&[9]).await;
            assert_eq!(stream.reset().await, reset(MALFORMED));
        },
        move |peer| async move {
            let store = open(&peer.node, 0).await;
            put(&store, a, &bytes).await;
            let push = peer::Push::open(&store, &peer.session).await.unwrap();
            let push = push.put(a).await.unwrap();
            let kind = wire::blob::Error::Kind { kind: 9 };
            assert_eq!(push.finish().await, Err(Error::Wire(kind)));
        },
    );
}
