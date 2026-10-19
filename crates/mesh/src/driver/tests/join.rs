//! Tests of the answer of a voter to a join request.

use transport::Class;
use transport::stream::{Incoming, Receiver};

use super::founding::{add_subject, fill_to};
use super::serve::{MALFORMED, Peer, REFUSED, codes, next, run, run_each, stopped};
use super::*;
use crate::driver::founding::FILE;

// The single-use ticket refuses a second stamp, so the second answer comes from
// the applied member.
#[test]
fn a_voter_admits_a_join_and_admits_it_again_after_it_applied() {
    solo(|node, tasks| async move {
        let mesh = recorded(&node, &tasks, &[ticket_of(8, "plant", false, LAST)]).await;
        let admitted = mesh.admit(public(4), request(4, 8, &["disk"])).await;
        assert_eq!(admitted, Ok(Ok(())));
        let member = mesh.member(key(4)).unwrap();
        assert_eq!(member.card, common::member(4).card);
        let again = mesh.admit(public(4), request(4, 8, &["disk"])).await;
        assert_eq!(again, Ok(Ok(())));
        assert_eq!(mesh.member(key(4)), Some(member));
        let used = ticket::Refused::Used {
            public_key: public(8),
        };
        let other = mesh.admit(public(5), request(5, 8, &[])).await;
        let refused = Refusal::Refused(region::Refused::Ticket(used));
        assert_eq!(other, Ok(Err(refused)));
    });
}

// Only the holder of the card's private key proves its public key, so no other
// peer asks for a join in its name.
#[test]
fn a_join_from_a_peer_that_is_not_the_card_key_is_spoofed() {
    solo(|node, tasks| async move {
        let mesh = recorded(&node, &tasks, &[ticket_of(8, "plant", true, LAST)]).await;
        let spoofed = mesh.admit(public(5), request(4, 8, &[])).await;
        assert_eq!(spoofed, Err(Error::Spoofed { from: key(4) }));
        assert_eq!(mesh.member(key(4)), None);
    });
}

#[test]
fn a_join_that_the_voter_cannot_admit_gives_the_refusal() {
    for synced in [true, false] {
        solo(move |node, tasks| async move {
            let unknown = region::Refused::Unknown {
                public_key: public(9),
            };
            let (time, request, refusal) = if synced {
                (
                    super::synced(&node),
                    request(4, 9, &[]),
                    Refusal::Refused(unknown),
                )
            } else {
                let unsynced = clock::Clock::new(node.clock()).1;
                (unsynced, request(4, 8, &[]), Refusal::Unsynced)
            };
            let ticket = ticket_of(8, "plant", true, LAST);
            let mesh = recorded_at(&node, &tasks, time, &[ticket]).await;
            let refused = mesh.admit(public(4), request).await;
            assert_eq!(refused, Ok(Err(refusal)));
            assert_eq!(mesh.member(key(4)), None);
        });
    }
}

// A refused join of the log binds key 4 to the public key of its card, so the
// leader refuses a join of key 4 under another key.
#[test]
fn a_join_of_a_bound_key_under_another_public_key_is_contested() {
    solo(|node, tasks| async move {
        let mesh = recorded(&node, &tasks, &[ticket_of(8, "plant", true, LAST)]).await;
        let unknown = join_change_with(&common::member(4).card, 9, Stamp::EPOCH);
        mesh.propose(unknown).await.unwrap();
        let mut card = common::member(4).card.card().clone();
        card.public_key = public(5);
        let card = card::Signed::sign(key(4), card, &common::private(5));
        let request = Request {
            ticket: public(8),
            card: card.clone().into(),
            admission: common::ticket(8).admission(&card),
            status: BTreeSet::new(),
        };
        let contested = mesh.admit(public(5), request).await;
        assert_eq!(contested, Ok(Err(Refusal::Contested)));
        assert_eq!(mesh.member(key(4)), None);
    });
}

#[test]
fn a_node_that_is_not_a_voter_or_that_stopped_admits_no_join() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 2, &IDS, &[1]).await.unwrap();
        let refused = mesh.admit(public(4), request(4, 8, &[])).await;
        assert_eq!(refused, Ok(Err(Refusal::NoVote)));
    });
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
        stop_by_an_empty_change(&mesh).await;
        let refused = mesh.admit(public(4), request(4, 8, &[])).await;
        assert_eq!(refused, Ok(Err(Refusal::Stopped)));
    });
}

// Both calls propose before the first entry applies, so the entry of the second
// applies after the join and the region refuses it.
#[test]
fn a_join_that_another_call_applied_first_is_admitted() {
    solo(|node, tasks| async move {
        let mesh = recorded(&node, &tasks, &[ticket_of(8, "plant", false, LAST)]).await;
        let mut calls =
            [1, 2].map(|_| Box::pin(mesh.admit(public(4), request(4, 8, &["disk"]))));
        let polls = poll_each(&mut calls).await;
        assert!(polls.iter().all(Poll::is_pending));
        let [first, second] = calls;
        assert_eq!(first.await, Ok(Ok(())));
        assert_eq!(second.await, Ok(Ok(())));
        assert_eq!(mesh.member(key(4)).unwrap().card, common::member(4).card);
    });
}

// Both joins stamp before the first applies, so the single-use ticket refuses the
// second only when it applies.
#[test]
fn a_join_whose_entry_the_region_refuses_gives_the_refusal() {
    solo(|node, tasks| async move {
        let mesh = recorded(&node, &tasks, &[ticket_of(8, "plant", false, LAST)]).await;
        let mut calls =
            [4, 5].map(|id| Box::pin(mesh.admit(public(id), request(id, 8, &[]))));
        let polls = poll_each(&mut calls).await;
        assert!(polls.iter().all(Poll::is_pending));
        let [first, second] = calls;
        assert_eq!(first.await, Ok(Ok(())));
        let used = ticket::Refused::Used {
            public_key: public(8),
        };
        let refused = Refusal::Refused(region::Refused::Ticket(used));
        assert_eq!(second.await, Ok(Err(refused)));
        assert_eq!(mesh.member(key(5)), None);
    });
}

// A member of the same card key and public key that another ticket admitted is no
// earlier answer to this request.
#[test]
fn a_join_of_a_member_by_another_ticket_is_refused() {
    solo(|node, tasks| async move {
        let tickets = [7, 8].map(|id| ticket_of(id, "plant", true, LAST));
        let mesh = recorded(&node, &tasks, &tickets).await;
        assert_eq!(mesh.admit(public(4), request(4, 7, &[])).await, Ok(Ok(())));
        let again = mesh.admit(public(4), request(4, 8, &[])).await;
        let duplicate = region::Refused::Unfit(Unfit::Duplicate { key: key(4) });
        assert_eq!(again, Ok(Err(Refusal::Refused(duplicate))));
    });
}

// The admission signs the card, so a copy of it under another public key does not
// admit that key.
#[test]
fn a_copied_admission_under_another_public_key_is_not_admitted() {
    solo(|node, tasks| async move {
        let mesh = recorded(&node, &tasks, &[ticket_of(8, "plant", true, LAST)]).await;
        let admitted = request(4, 8, &[]);
        assert_eq!(mesh.admit(public(4), admitted.clone()).await, Ok(Ok(())));
        let mut card = common::member(4).card.card().clone();
        card.public_key = public(5);
        let card = card::Signed::sign(key(4), card, &common::private(5));
        let copied = Request {
            card: card.into(),
            ..admitted
        };
        let forged = ticket::Refused::Forged {
            node: key(4),
            public_key: public(8),
        };
        let refused = Refusal::Refused(region::Refused::Ticket(forged));
        assert_eq!(mesh.admit(public(5), copied).await, Ok(Err(refused)));
    });
}

// The status names of a node are a table of its build, so more than 64 is a malformed
// request, not a refusal.
#[test]
fn serve_stops_a_join_of_more_than_64_status_names() {
    let ((served, member), seen) = run(
        |node, tasks, incoming| async move {
            let tickets = [ticket_of(8, "plant", true, LAST)];
            let mesh = recorded(&node, &tasks, &tickets).await;
            let served = mesh.serve(public(4), incoming).await;
            (served, mesh.member(key(4)))
        },
        |peer| async move {
            let names: Vec<_> = (0..65).map(|at| format!("status{at}")).collect();
            let names: Vec<_> = names.iter().map(String::as_str).collect();
            let request = Box::new(request(4, 8, &names));
            stopped(peer, &[Message::Join { request }.encode()]).await
        },
    );
    assert_eq!((served, member), (Err(Error::Malformed), None));
    assert_eq!(seen, codes(MALFORMED));
}

// A write of the log that finds no block holds each later proposal back, so no
// leader takes the join in the one try.
#[test]
fn a_join_that_no_leader_takes_is_unled() {
    solo(|node, tasks| async move {
        let mesh = recorded(&node, &tasks, &[ticket_of(8, "plant", true, LAST)]).await;
        let held = fill(&mesh.pool);
        let mut waits = pin!(mesh.propose(home_to(3)));
        assert!(now(waits.as_mut()).await.is_pending());
        node.clock().sleep(TICK).await;
        let unled = mesh.admit(public(4), request(4, 8, &[])).await;
        assert_eq!(unled, Ok(Err(Refusal::Unled)));
        assert_eq!(mesh.member(key(4)), None);
        drop(held);
        waits.await.unwrap();
        assert_eq!(mesh.admit(public(4), request(4, 8, &[])).await, Ok(Ok(())));
    });
}

#[test]
fn a_join_whose_log_write_fails_is_stopped() {
    solo(|node, tasks| async move {
        let mesh = recorded(&node, &tasks, &[ticket_of(8, "plant", true, LAST)]).await;
        fail_sync(&node);
        let stopped = mesh.admit(public(4), request(4, 8, &[])).await;
        assert_eq!(stopped, Ok(Err(Refusal::Stopped)));
        assert_eq!(mesh.member(key(4)), None);
    });
}

// The first serve is in its read when it drops. The second waits in a task of its
// own, so only the end of the turn wakes it.
#[test]
fn serve_answers_after_an_answer_that_drops_in_its_read() {
    let (served, answer) = run_each(
        2,
        |node, tasks, incoming| async move {
            let [a, b] = <[Incoming; 2]>::try_from(incoming).unwrap();
            let tickets = [ticket_of(8, "plant", true, LAST)];
            let mesh = recorded(&node, &tasks, &tickets).await;
            let admitted = mesh.admit(public(4), request(4, 8, &[])).await;
            assert_eq!(admitted, Ok(Ok(())));
            node.clock().sleep(seconds(1)).await;
            let mut first = Box::pin(mesh.serve(public(4), a));
            assert!(now(first.as_mut()).await.is_pending());
            let served = Rc::new(RefCell::new(None));
            let (other, seen) = (mesh.clone(), Rc::clone(&served));
            tasks.spawn(async move {
                *seen.borrow_mut() = Some(other.serve(public(4), b).await);
            });
            node.clock().sleep(Span::MILLISECOND).await;
            drop(first);
            node.clock().sleep(seconds(1)).await;
            served.take()
        },
        |peer| async move {
            let mut streams = Vec::new();
            for _ in 0..2 {
                let (mut sender, receiver) =
                    peer.open(Class::Complete, &[join(8)]).await;
                sender.finish().unwrap();
                streams.push(receiver);
            }
            answer(&mut streams[1]).await
        },
    );
    assert_eq!(served, Some(Ok(())));
    assert_eq!(answer.len(), 2);
}

// The read ends in the poll that drops its only copy, before the call that waits
// runs, so only the copy that the read gives saves a second open.
#[test]
fn a_call_that_waits_gets_the_copy_of_a_read_whose_caller_drops_it() {
    solo(|node, tasks| async move {
        let mesh = open(&node, &tasks, 1, &IDS, &IDS).await.unwrap();
        let opens = || {
            let closes = node.file_closes();
            closes.iter().filter(|path| path.ends_with(FILE)).count()
        };
        let before = opens();
        let first = async { mesh.founding.get().await.map(drop) };
        let (first, second) = both(first, mesh.founding.get()).await;
        assert_eq!(first, Ok(()));
        assert!(region::Founding::decode(second.unwrap().body()).is_some());
        assert_eq!(opens() - before, 1);
    });
}

/// The join request of node 4 by ticket `ticket`.
fn join(ticket: u8) -> Vec<u8> {
    let request = Box::new(request(4, ticket, &[]));
    Message::Join { request }.encode()
}

/// Sends `messages` on a join stream, then ends it, and gives each message of the
/// answer.
async fn ask_to_join(peer: Peer, messages: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let (mut sender, mut receiver) = peer.open(Class::Complete, messages).await;
    sender.finish().unwrap();
    answer(&mut receiver).await
}

// Node 4 is a member before both answers start, so both ask for the founding in one
// poll and the file opens once. No copy stays for a later answer.
#[test]
fn serve_sends_two_answers_at_one_time_from_one_read() {
    let ((answers, opens, last), (first, second, third)) = run_each(
        3,
        |node, tasks, incoming| async move {
            let [a, b, c] = <[Incoming; 3]>::try_from(incoming).unwrap();
            let tickets = [ticket_of(8, "plant", true, LAST)];
            let mesh = recorded(&node, &tasks, &tickets).await;
            let admitted = mesh.admit(public(4), request(4, 8, &[])).await;
            assert_eq!(admitted, Ok(Ok(())));
            let opens = || {
                let closes = node.file_closes();
                closes.iter().filter(|path| path.ends_with(FILE)).count()
            };
            let before = opens();
            let [a, b] =
                [a, b].map(|incoming| Box::pin(mesh.serve(public(4), incoming)));
            let answers = both(a, b).await;
            let opens = opens() - before;
            node.files().remove(Path::new(FILE)).await.unwrap();
            (answers, opens, mesh.serve(public(4), c).await)
        },
        |peer| async move {
            let mut streams = Vec::new();
            for _ in 0..3 {
                let (mut sender, receiver) =
                    peer.open(Class::Complete, &[join(8)]).await;
                sender.finish().unwrap();
                streams.push(receiver);
            }
            let [mut first, mut second, mut third] =
                <[Receiver; 3]>::try_from(streams).unwrap();
            let (first, second) = (answer(&mut first).await, answer(&mut second).await);
            (first, second, next(&mut third).await)
        },
    );
    assert_eq!((answers, opens), ((Ok(()), Ok(())), 1));
    let path = PathBuf::from(FILE);
    assert_eq!(last, Err(Error::Unfounded { path }));
    assert!(first == second, "the answers differ");
    let joined = Message::decode(&first[0]);
    assert!(
        matches!(joined, Some(Message::Joined { .. })),
        "no Joined first"
    );
    assert_eq!(third, Err(transport::Error::Reset { code: Code(0) }));
}

/// Polls `a` and `b` in turn until both are done, and gives their outputs.
async fn both<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (pin!(a), pin!(b));
    let (mut done_a, mut done_b) = (None, None);
    poll_fn(|cx| {
        if done_a.is_none()
            && let Poll::Ready(output) = a.as_mut().poll(cx)
        {
            done_a = Some(output);
        }
        if done_b.is_none()
            && let Poll::Ready(output) = b.as_mut().poll(cx)
        {
            done_b = Some(output);
        }
        if done_a.is_some() && done_b.is_some() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    (done_a.unwrap(), done_b.unwrap())
}

/// Each message on `receiver` until it ends.
async fn answer(receiver: &mut Receiver) -> Vec<Vec<u8>> {
    let mut answer = Vec::new();
    while let Some(message) = receiver.recv().await.unwrap() {
        answer.push(message.to_vec());
    }
    answer
}

// The founding is over two messages, so each part but the last fills one.
#[test]
fn serve_admits_a_join_and_sends_the_founding_in_parts() {
    let ((served, member, founding), answer) = run(
        |node, tasks, incoming| async move {
            let mut config = config(&node, &tasks, 1, &[1], &[1]).await;
            add_subject(&mut config.founding, 5_000);
            let founding = config.founding.encode();
            let tickets = [ticket_of(8, "plant", true, LAST)];
            let mesh = recorded_with(&node, config, &tickets).await;
            let served = mesh.serve(public(4), incoming).await;
            (served, mesh.member(key(4)), founding)
        },
        |peer| async move { ask_to_join(peer, &[join(8)]).await },
    );
    assert_eq!(served, Ok(()));
    assert_eq!(member.unwrap().card, common::member(4).card);
    let (joined, parts) = answer.split_first().unwrap();
    let bytes = founding.len();
    assert_eq!(Message::decode(joined), Some(Message::Joined { bytes }));
    let lengths: Vec<_> = parts.iter().map(Vec::len).collect();
    let last = bytes - 2 * (BYTES_MAX - 1) + 1;
    assert_eq!(lengths, [BYTES_MAX, BYTES_MAX, last]);
    let mut sent = Vec::new();
    for part in parts {
        let Some(Message::Founding { part }) = Message::decode(part) else {
            panic!("not a part");
        };
        sent.extend(part);
    }
    assert!(sent == founding, "the parts are not the founding");
}

// The largest block of the pool of the mesh is under the bound of the peer, so each
// part but the last fills one block.
#[test]
fn serve_sends_the_founding_in_parts_of_the_largest_block_of_the_pool() {
    let ((served, largest), answer) = run(
        |node, tasks, incoming| async move {
            let pool = pool_of(1 << 16);
            let largest = pool.largest();
            let mut config = Config {
                pool,
                ..config(&node, &tasks, 1, &[1], &[1]).await
            };
            fill_to(&mut config.founding, largest);
            let tickets = [ticket_of(8, "plant", true, LAST)];
            let mesh = recorded_with(&node, config, &tickets).await;
            (mesh.serve(public(4), incoming).await, largest)
        },
        |peer| async move { ask_to_join(peer, &[join(8)]).await },
    );
    assert_eq!(served, Ok(()));
    assert!(largest < BYTES_MAX, "{largest}");
    let lengths: Vec<_> = answer[1..].iter().map(Vec::len).collect();
    assert_eq!(lengths, [largest, 2]);
}

#[test]
fn serve_answers_a_join_that_it_does_not_admit_with_the_refusal() {
    let unknown = Refusal::Refused(region::Refused::Unknown {
        public_key: public(9),
    });
    for (voter, ticket, refusal) in [(1, 9, unknown), (2, 8, Refusal::NoVote)] {
        let (served, answer) = run(
            move |node, tasks, incoming| async move {
                let mesh = if voter == 1 {
                    recorded(&node, &tasks, &[ticket_of(8, "plant", true, LAST)]).await
                } else {
                    open(&node, &tasks, voter, &IDS, &[1]).await.unwrap()
                };
                mesh.serve(public(4), incoming).await
            },
            move |peer| async move { ask_to_join(peer, &[join(ticket)]).await },
        );
        assert_eq!(served, Ok(()));
        assert_eq!(answer, [Message::Unjoined { refusal }.encode()]);
    }
}

#[test]
fn serve_stops_a_join_of_a_card_that_the_peer_does_not_hold() {
    let ((served, member), seen) = run(
        |node, tasks, incoming| async move {
            let tickets = [ticket_of(8, "plant", true, LAST)];
            let mesh = recorded(&node, &tasks, &tickets).await;
            let served = mesh.serve(public(5), incoming).await;
            (served, mesh.member(key(4)))
        },
        |peer| async move { stopped(peer, &[join(8)]).await },
    );
    assert_eq!(
        (served, member),
        (Err(Error::Spoofed { from: key(4) }), None)
    );
    assert_eq!(seen, codes(REFUSED));
}

// The answer is on the stream when the second message comes.
#[test]
fn serve_stops_a_join_stream_at_a_second_message() {
    let (served, (answer, finished)) = run(
        |node, tasks, incoming| async move {
            let tickets = [ticket_of(8, "plant", true, LAST)];
            let mesh = recorded(&node, &tasks, &tickets).await;
            mesh.serve(public(4), incoming).await
        },
        |peer| async move {
            let (mut sender, mut receiver) =
                peer.open(Class::Complete, &[join(9), join(9)]).await;
            let answer = next(&mut receiver).await;
            peer.settle().await;
            (answer, sender.finish())
        },
    );
    let unknown = Refusal::Refused(region::Refused::Unknown {
        public_key: public(9),
    });
    assert_eq!(served, Err(Error::Malformed));
    assert_eq!(answer, Ok(Some(Message::Unjoined { refusal: unknown })));
    let stopped = transport::Error::Stopped { code: MALFORMED };
    assert_eq!(finished, Err(stopped));
}

// The node is a member once the voter admits it, so the failure is no refusal, and
// the node can ask again.
#[test]
fn serve_gives_no_code_of_the_mesh_when_it_cannot_read_the_founding() {
    let ((served, member), seen) = run(
        |node, tasks, incoming| async move {
            let tickets = [ticket_of(8, "plant", true, LAST)];
            let mesh = recorded(&node, &tasks, &tickets).await;
            node.files().remove(Path::new(FILE)).await.unwrap();
            let served = mesh.serve(public(4), incoming).await;
            (served, mesh.member(key(4)).is_some())
        },
        |peer| async move { stopped(peer, &[join(8)]).await },
    );
    let path = PathBuf::from(FILE);
    assert_eq!((served, member), (Err(Error::Unfounded { path }), true));
    assert_eq!(seen, codes(Code(0)));
}
