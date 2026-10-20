//! Tests of `join::ask` by node 5, against the voters of a cluster or a voter that
//! sends scripted answers.

use super::serve::MALFORMED;
use super::*;
use crate::join::{self, Miss};
use crate::ticket::{Ticket, Voter};

/// The node that asks to join. It is the fifth node of each run, so each cluster
/// has at most 4 members.
const JOINER: u8 = 5;
/// The ticket that the tests record.
const TICKET: u8 = 8;

/// What each ask of a node gave, with the time it took, in order.
type Asked = Arc<Mutex<Vec<(Result<region::Founding, Error>, Span)>>>;

/// The voter `id` of a ticket, at `at`.
fn voter_at(id: u8, at: SocketAddr) -> Voter {
    let addresses = vec![Address::Udp(at)];
    Voter {
        key: key(id),
        public_key: public(id),
        addresses: card::addresses::Addresses::new(addresses).unwrap(),
    }
}

/// The ticket `id` of region `plant`, whose node asks the voters `voters` in order.
fn ticket_to(id: u8, voters: &[u8]) -> Ticket {
    let voters = voters.iter().map(|&of| voter_at(of, address(of))).collect();
    Ticket::new(private(id), "plant".parse().unwrap(), voters)
}

/// The founding that each voter of a cluster of `ids` holds, with the voters `voters`.
fn founding_of(ids: &[u8], voters: &[u8]) -> region::Founding {
    region::Founding {
        prefix: "plant".parse().unwrap(),
        members: ids.iter().map(|&id| create_voter(id)).collect(),
        voters: voters.iter().map(|&id| key(id)).collect(),
        definitions: BTreeMap::new(),
        homes: BTreeMap::new(),
    }
}

/// The transport of node `id` on `node`.
fn transport_of(node: &sim::node::Node, tasks: &Tasks, id: u8) -> Rc<Transport> {
    let config = transport_config(node, tasks, id, create_pool());
    Rc::new(bind(node, PORT, config))
}

/// The config on `node` that asks by `ticket` with the card of [`JOINER`] over
/// `transport`.
fn config_of(
    node: &sim::node::Node,
    transport: Rc<Transport>,
    ticket: Ticket,
) -> join::Config {
    join::Config {
        ticket,
        card: common::member(JOINER).card,
        status: common::names([]),
        clock: node.clock(),
        pool: create_pool(),
        transport,
    }
}

/// Starts [`JOINER`] on a new node of `sim`, which asks by each of `tickets` in turn.
/// Gives what each ask gave.
#[expect(clippy::arithmetic_side_effects, reason = "a test may panic")]
fn ask_each(sim: &mut Sim, tickets: Vec<Ticket>) -> Asked {
    let node = sim.node(sim::node::Config::default());
    let asked = Asked::default();
    let (own, slot) = (node.clone(), Arc::clone(&asked));
    let main = move |tasks: Tasks| async move {
        let node = own;
        let clock = node.clock();
        let transport = transport_of(&node, &tasks, JOINER);
        for ticket in tickets {
            let config = config_of(&node, Rc::clone(&transport), ticket);
            let start = clock.now();
            let founding = join::ask(config).await;
            slot.lock().unwrap().push((founding, clock.now() - start));
        }
        // The transport sends each stop of the last ask while it lives.
        std::future::pending::<()>().await;
    };
    let config = env::shards::Config {
        name: "joiner".into(),
        core: None,
    };
    drop(node.shards().start(config, main).unwrap());
    asked
}

/// A cluster of `ids` that recorded ticket 8, which admits `plant.*` any number of
/// times.
fn recorded_cluster(seed: u64, ids: &[u8], learners: &[u8]) -> Cluster {
    let mut cluster = Cluster::of(seed, ids);
    cluster.board.lock().unwrap().learners = learners.iter().copied().collect();
    cluster.script_each(&[encoded(&ticket_of(TICKET, "plant", true, LAST))]);
    cluster.start();
    cluster.run(seconds(10));
    cluster
}

#[test]
fn a_node_that_asks_gets_the_founding_and_each_voter_applies_its_join() {
    for seed in 0..4 {
        let mut cluster = recorded_cluster(seed, &IDS, &[]);
        let asked = ask_each(&mut cluster.sim, vec![ticket_to(TICKET, &IDS)]);
        cluster.run(seconds(10));
        let founding = Ok(founding_of(&IDS, &IDS));
        let asked: Vec<_> = asked.lock().unwrap().drain(..).map(|(a, _)| a).collect();
        assert_eq!(asked, [founding], "run {seed}");
        cluster.script(|_| home_to(9));
        cluster.run(seconds(5));
        let records = cluster.board().records;
        for id in IDS {
            let card = records[&id].get(&JOINER).map(|member| member.card.clone());
            assert_eq!(
                card,
                Some(common::member(JOINER).card),
                "run {seed}: node {id}"
            );
        }
    }
}

// Node 4 is a member that is not a voter.
#[test]
fn a_node_that_asks_a_member_that_is_not_a_voter_asks_the_next_voter() {
    for seed in 0..4 {
        let ids = [1, 2, 3, 4];
        let mut cluster = recorded_cluster(seed, &ids, &[4]);
        let tickets = vec![ticket_to(TICKET, &[4]), ticket_to(TICKET, &[4, 1])];
        let asked = ask_each(&mut cluster.sim, tickets);
        cluster.run(seconds(30));
        let asked: Vec<_> = asked.lock().unwrap().drain(..).map(|(a, _)| a).collect();
        let unjoined = Error::Unjoined {
            missed: vec![(key(4), Miss::NoVote)],
        };
        let founding = founding_of(&ids, &IDS);
        assert_eq!(asked, [Err(unjoined), Ok(founding)], "run {seed}");
    }
}

#[test]
fn a_ticket_that_the_region_does_not_record_gives_the_refusal_of_each_voter() {
    let mut cluster = recorded_cluster(0, &IDS, &[]);
    let asked = ask_each(&mut cluster.sim, vec![ticket_to(9, &IDS)]);
    cluster.run(seconds(30));
    let (asked, _) = asked.lock().unwrap().pop().unwrap();
    let error = asked.unwrap_err();
    let text = format!(
        "no voter of the ticket admitted this node; {}: the region refuses the join: \
         no ticket {} is recorded; {}: the region refuses the join: no ticket {} is \
         recorded; {}: the region refuses the join: no ticket {} is recorded",
        key(1),
        public(9),
        key(2),
        public(9),
        key(3),
        public(9),
    );
    assert_eq!(error.to_string(), text);
    let Error::Unjoined { missed } = error else {
        panic!("not unjoined: {error}");
    };
    let voters: Vec<_> = missed.iter().map(|(voter, _)| *voter).collect();
    assert_eq!(voters, IDS.map(key));
}

/// What an ask by ticket 8 gives after voters `crashed` stop, with the time it took.
fn ask_with_crashed(
    seed: u64,
    crashed: &[u8],
) -> (Result<region::Founding, Error>, Span) {
    let mut cluster = recorded_cluster(seed, &IDS, &[]);
    for &id in crashed {
        let node = cluster.node(id).clone();
        cluster.sim.crash(&node, Crash::Process);
    }
    cluster.run(seconds(5));
    let asked = ask_each(&mut cluster.sim, vec![ticket_to(TICKET, &IDS)]);
    cluster.run(seconds(300));
    asked.lock().unwrap().pop().unwrap()
}

// Voters 2 and 3 stop after the ticket commits, so voter 1 commits no join.
#[test]
fn a_node_whose_voters_commit_no_join_is_unjoined_within_the_bound() {
    for seed in 0..4 {
        let (asked, took) = ask_with_crashed(seed, &[2, 3]);
        let missed = vec![
            (key(1), Miss::Uncommitted),
            (key(2), Miss::Deadline),
            (key(3), Miss::Deadline),
        ];
        assert_eq!(asked, Err(Error::Unjoined { missed }), "run {seed}");
        let bound = join::bound(&ticket_to(TICKET, &IDS));
        assert!(took <= bound, "run {seed}: after {took}");
    }
}

// 8 rounds of 3 deadlines of 10 s, and 7 pauses of 1 s.
#[test]
fn a_node_whose_voters_are_all_down_is_unjoined_within_the_bound() {
    let (asked, took) = ask_with_crashed(0, &IDS);
    let missed = IDS.map(|id| (key(id), Miss::Deadline)).to_vec();
    assert_eq!(asked, Err(Error::Unjoined { missed }));
    assert!(near(took, seconds(247)), "after {took}");
    assert!(took < join::bound(&ticket_to(TICKET, &IDS)));
}

#[test]
fn the_bound_of_a_ticket_is_8_rounds_of_10_s_for_each_voter_and_1_s() {
    assert_eq!(join::bound(&ticket_to(TICKET, &[1])), seconds(88));
    assert_eq!(join::bound(&ticket_to(TICKET, &IDS)), seconds(248));
}

/// Whether `took` is at least `at`, and less than 500 ms after it: the most time that
/// the dials and the messages of these tests add to a sum of deadlines and pauses.
#[expect(clippy::arithmetic_side_effects, reason = "a test may panic")]
fn near(took: Span, at: Span) -> bool {
    took >= at && took.nanos() < at.nanos() + 500 * Span::MILLISECOND.nanos()
}

/// What a scripted voter sends on each join stream, after it read the request.
struct Script {
    /// The messages of the reply half. An empty one resets the half with code 7.
    messages: Vec<Vec<u8>>,
    /// The time before each message.
    gap: Span,
    /// The time after the messages before the voter finishes the reply half.
    hold: Span,
}

/// What [`scripted`] gives: what the ask gave, the time it took, and what the finish of
/// each reply half gave, in order.
type Scripted = (
    Result<region::Founding, Error>,
    Span,
    Vec<Result<(), transport::Error>>,
);

/// Runs [`JOINER`], which asks node 1 to join by a ticket of node 1 alone, against
/// node 1, which answers each stream with `script`. Gives what [`Scripted`] states.
fn scripted(script: Script) -> Scripted {
    let mut sim = Sim::new(common::simulation(0));
    let node = sim.node(sim::node::Config::default());
    let at = SocketAddr::new(node.addresses()[0], PORT);
    let finished = Arc::new(Mutex::new(Vec::new()));
    let (own, ends) = (node.clone(), Arc::clone(&finished));
    let main = move |tasks: Tasks| async move {
        let (node, script) = (own, Rc::new(script));
        let pool = create_pool();
        let transport = bind(
            &node,
            PORT,
            transport_config(&node, &tasks, 1, Rc::clone(&pool)),
        );
        let session = transport.accept().await.unwrap();
        while let Ok(mut incoming) = session.accept().await {
            let (node, pool, ends) =
                (node.clone(), Rc::clone(&pool), Arc::clone(&ends));
            let script = Rc::clone(&script);
            tasks.spawn(async move {
                let header = incoming.receiver.recv().await.unwrap().unwrap();
                let protocol = wire::header::decode(&header).unwrap();
                assert_eq!(protocol, (Protocol::Mesh, &[][..]));
                let join = incoming.receiver.recv().await.unwrap().unwrap();
                let Some(Message::Join { request }) = Message::decode(&join) else {
                    panic!("not a join");
                };
                assert_eq!(request.card, unchecked(&common::member(JOINER).card));
                assert!(incoming.receiver.recv().await.unwrap().is_none());
                let mut sender = incoming.sender.unwrap();
                for message in &script.messages {
                    node.clock().sleep(script.gap).await;
                    if message.is_empty() {
                        sender.reset(transport::Code(7));
                        return;
                    }
                    if sender.send(pool.copy(message).unwrap()).await.is_err() {
                        break;
                    }
                }
                node.clock().sleep(script.hold).await;
                ends.lock().unwrap().push(sender.finish());
            });
        }
    };
    let config = env::shards::Config {
        name: "voter".into(),
        core: None,
    };
    drop(node.shards().start(config, main).unwrap());
    let ticket =
        Ticket::new(private(1), "plant".parse().unwrap(), vec![voter_at(1, at)]);
    let asked = ask_each(&mut sim, vec![ticket]);
    sim.run_for(seconds(300)).unwrap();
    let (asked, took) = asked.lock().unwrap().pop().unwrap();
    let finished = finished.lock().unwrap().drain(..).collect();
    (asked, took, finished)
}

fn joined(bytes: usize) -> Vec<u8> {
    Message::Joined { bytes }.encode()
}

fn part(part: &[u8]) -> Vec<u8> {
    Message::Founding {
        part: part.to_vec(),
    }
    .encode()
}

fn unjoined(refusal: Refusal) -> Vec<u8> {
    Message::Unjoined { refusal }.encode()
}

/// What [`scripted`] gives for a voter that answers each ask with `messages`.
fn answered(
    messages: Vec<Vec<u8>>,
) -> (
    Result<region::Founding, Error>,
    Vec<Result<(), transport::Error>>,
) {
    let (asked, _, finished) = scripted(Script {
        messages,
        gap: Span::ZERO,
        hold: seconds(1),
    });
    (asked, finished)
}

fn unjoined_by(miss: Miss) -> Result<region::Founding, Error> {
    Err(Error::Unjoined {
        missed: vec![(key(1), miss)],
    })
}

#[test]
fn ask_reads_a_founding_in_parts() {
    let founding = founding_of(&[1, 2], &[1]);
    let body = founding.encode();
    let mut messages = vec![joined(body.len())];
    messages.extend(body.chunks(100).map(part));
    let (asked, finished) = answered(messages);
    assert_eq!(asked, Ok(founding));
    assert_eq!(finished, [Ok(())]);
}

#[test]
fn ask_stops_a_reply_half_that_is_not_a_join_answer_with_code_2() {
    let body = founding_of(&[1], &[1]).encode();
    let mut whole = vec![joined(body.len()), part(&body)];
    whole.push(part(&[1]));
    let cases = [
        vec![unjoined(Refusal::Unsynced), unjoined(Refusal::Unsynced)],
        vec![joined(4), part(&[1; 5])],
        vec![joined(4), part(&[])],
        vec![joined(4), encoded(&home_to(1))],
        vec![joined(4), Message::Propose { change: home_to(1) }.encode()],
        whole,
        vec![vec![0xff]],
        vec![Message::NotLeader { leader: None }.encode()],
    ];
    let stopped = Err(transport::Error::Stopped { code: MALFORMED });
    for (case, messages) in cases.into_iter().enumerate() {
        let (asked, finished) = answered(messages);
        assert_eq!(asked, unjoined_by(Miss::Malformed), "case {case}");
        assert_eq!(finished, vec![stopped.clone(); 8], "case {case}");
    }
}

#[test]
fn ask_misses_a_reply_half_that_ends_before_its_answer_or_its_founding() {
    let cases = [
        vec![],
        vec![joined(4)],
        vec![joined(4), part(&[1; 3])],
        vec![joined(4), part(&[1; 4])],
    ];
    for (case, messages) in cases.into_iter().enumerate() {
        let (asked, finished) = answered(messages);
        assert_eq!(asked, unjoined_by(Miss::Malformed), "case {case}");
        assert_eq!(finished, vec![Ok(()); 8], "case {case}");
    }
}

#[test]
fn ask_gives_the_miss_of_each_refusal() {
    let unknown = region::Refused::Unknown {
        public_key: public(1),
    };
    let cases = [
        (Refusal::Contested, Miss::Contested),
        (Refusal::Unsynced, Miss::Unsynced),
        (Refusal::Unled, Miss::Uncommitted),
        (Refusal::NoVote, Miss::NoVote),
        (Refusal::Stopped, Miss::Stopped),
        (
            Refusal::Refused(unknown.clone()),
            Miss::from(Refusal::Refused(unknown)),
        ),
    ];
    for (refusal, miss) in cases {
        let (asked, finished) = answered(vec![unjoined(refusal)]);
        assert_eq!(asked, unjoined_by(miss));
        assert_eq!(finished, vec![Ok(()); 8]);
    }
}

#[test]
fn ask_misses_a_voter_that_holds_its_answer_or_founding_past_the_deadline() {
    for (messages, miss) in [
        (vec![], Miss::Deadline),
        (vec![joined(4)], Miss::Slow),
        (vec![joined(8), part(&[1; 4])], Miss::Slow),
    ] {
        let (asked, took, finished) = scripted(Script {
            messages,
            gap: Span::ZERO,
            hold: seconds(1000),
        });
        assert_eq!(asked, unjoined_by(miss));
        assert!(near(took, seconds(87)), "{took}");
        assert_eq!(finished, []);
    }
}

// 1 MiB has 10 s + 10.24 s from its `Joined`, which comes at 9 s.
#[test]
fn ask_misses_a_founding_that_comes_slower_than_100_kib_each_second() {
    let mut messages = vec![joined(1 << 20)];
    messages.extend((0..100).map(|_| part(&[1])));
    let (asked, took, _) = scripted(Script {
        messages,
        gap: seconds(9),
        hold: seconds(1000),
    });
    assert_eq!(asked, unjoined_by(Miss::Slow));
    let at = Span::from_nanos((8 * 29_240 + 7_000) * Span::MILLISECOND.nanos());
    assert!(near(took, at), "{took}");
}

// 614,400 bytes have 16 s from their `Joined` at 1.5 s, and the last part comes at
// 16.5 s, so the end of the half has 1 s.
#[test]
fn ask_misses_a_founding_whose_half_does_not_end_within_its_transfer() {
    let mut messages = vec![joined(614_400)];
    messages.extend((0..10).map(|_| part(&vec![1; 61_440])));
    let (asked, took, _) = scripted(Script {
        messages,
        gap: Span::from_nanos(1_500 * Span::MILLISECOND.nanos()),
        hold: seconds(1000),
    });
    assert_eq!(asked, unjoined_by(Miss::Slow));
    let at = Span::from_nanos((8 * 17_500 + 7_000) * Span::MILLISECOND.nanos());
    assert!(near(took, at), "{took}");
}

// The founding of 1 MiB has 20.24 s, but each message only 10 s after the one before.
#[test]
fn ask_misses_a_founding_whose_voter_stops_sending_after_10_s() {
    for messages in [vec![joined(1 << 20)], vec![joined(1 << 20), part(&[1])]] {
        let (asked, took, _) = scripted(Script {
            messages,
            gap: Span::ZERO,
            hold: seconds(1000),
        });
        assert_eq!(asked, unjoined_by(Miss::Slow));
        assert!(near(took, seconds(87)), "{took}");
    }
}

// The refusal comes at 9 s, and its half has not ended at 10 s.
#[test]
fn a_refusal_whose_half_does_not_end_in_time_gives_its_miss_and_stops_the_half() {
    let stopped = Err(transport::Error::Stopped {
        code: transport::Code(0),
    });
    for (refusal, miss) in [
        (Refusal::Contested, Miss::Contested),
        (Refusal::Unsynced, Miss::Unsynced),
    ] {
        let (asked, took, finished) = scripted(Script {
            messages: vec![unjoined(refusal)],
            gap: seconds(9),
            hold: seconds(11),
        });
        assert_eq!(asked, unjoined_by(miss));
        assert!(near(took, seconds(87)), "{took}");
        assert_eq!(finished, vec![stopped.clone(); 8]);
    }
}

#[test]
fn a_refusal_whose_half_resets_gives_its_miss() {
    let (asked, _, _) = scripted(Script {
        messages: vec![unjoined(Refusal::Contested), vec![]],
        gap: seconds(1),
        hold: Span::ZERO,
    });
    assert_eq!(asked, unjoined_by(Miss::Contested));
}

#[test]
fn the_text_of_each_miss_names_its_cause() {
    let unknown = region::Refused::Unknown {
        public_key: public(9),
    };
    let cases = [
        (
            Miss::Stream(transport::Error::TimedOut),
            format!("the stream failed: {}", transport::Error::TimedOut),
        ),
        (Miss::Deadline, "no answer came within 10 s".into()),
        (
            Miss::Slow,
            "the founding came under 100 KiB/s or with a gap over 10 s".into(),
        ),
        (
            Miss::Malformed,
            "the answer is not one of a join stream".into(),
        ),
        (Miss::Uncommitted, "no leader committed the join".into()),
        (
            Miss::Unsynced,
            "the voter has no mesh time to stamp a join".into(),
        ),
        (Miss::NoVote, "the node is not a voter".into()),
        (Miss::Stopped, "the group of the voter stopped".into()),
        (
            Miss::Contested,
            "a member or an earlier join holds the node key with another public key"
                .into(),
        ),
        (
            Miss::from(Refusal::Refused(unknown)),
            format!(
                "the region refuses the join: no ticket {} is recorded",
                public(9)
            ),
        ),
    ];
    for (miss, text) in cases {
        assert_eq!(miss.to_string(), text);
    }
}

#[test]
fn ask_gives_the_error_of_a_pool_with_no_block_for_the_request() {
    solo(|node, tasks| async move {
        let transport = transport_of(&node, &tasks, JOINER);
        let config = config_of(&node, transport, ticket_to(TICKET, &IDS));
        let held = fill(&config.pool);
        let asked = join::ask(config).await;
        drop(held);
        let header = wire::header::encode(Protocol::Mesh).len();
        assert_eq!(asked, Err(exhausted(header)));
    });
}

// The voter at the address of node 1 does not prove the public key of node 2.
#[test]
fn ask_gives_the_miss_of_a_dial_that_fails() {
    let mut cluster = recorded_cluster(0, &IDS, &[]);
    let voter = Voter {
        public_key: public(2),
        ..voter_at(1, address(1))
    };
    let ticket = Ticket::new(private(TICKET), "plant".parse().unwrap(), vec![voter]);
    let asked = ask_each(&mut cluster.sim, vec![ticket]);
    cluster.run(seconds(300));
    let (asked, _) = asked.lock().unwrap().pop().unwrap();
    let expected = transport::Error::Authentication {
        expected: public(2),
    };
    let unreachable = transport::Error::Unreachable {
        peer: Some(public(2)),
        attempts: Box::new([(Address::Udp(address(1)), expected)]),
    };
    assert_eq!(asked, unjoined_by(Miss::Stream(unreachable)));
}

#[test]
#[should_panic(expected = "invariant: the transport proves the public key of the card")]
fn ask_panics_on_a_transport_of_another_key() {
    solo(|node, tasks| async move {
        let transport = transport_of(&node, &tasks, 4);
        let config = config_of(&node, transport, ticket_to(TICKET, &IDS));
        drop(join::ask(config).await);
    });
}
