//! Tests of `join::ask` by node 5, against the voters of a cluster or a voter that
//! sends scripted answers.

use env::files::Mode;

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
        files: node.files(),
        dir: PathBuf::new(),
    }
}

/// Starts [`JOINER`] on a new node of `sim`, which asks by each of `tickets` in turn.
/// Gives what each ask gave.
fn ask_each(sim: &mut Sim, tickets: Vec<Ticket>) -> Asked {
    let card = common::member(JOINER).card;
    let asks = tickets.into_iter().map(|ticket| (ticket, card.clone()));
    asks_on(sim, asks.collect()).1
}

/// Starts [`JOINER`] on a new node of `sim`, which asks by each ticket of `asks` with
/// its card, in turn. Gives the node and what each ask gave.
fn asks_on(
    sim: &mut Sim,
    asks: Vec<(Ticket, card::Signed)>,
) -> (sim::node::Node, Asked) {
    let node = sim.node(sim::node::Config::default());
    let asked = asks_at(&node, asks);
    (node, asked)
}

/// As [`asks_on`], on `node`.
#[expect(clippy::arithmetic_side_effects, reason = "a test may panic")]
fn asks_at(node: &sim::node::Node, asks: Vec<(Ticket, card::Signed)>) -> Asked {
    let asked = Asked::default();
    let (own, slot) = (node.clone(), Arc::clone(&asked));
    let main = move |tasks: Tasks| async move {
        let node = own;
        let clock = node.clock();
        let transport = transport_of(&node, &tasks, JOINER);
        for (ticket, card) in asks {
            let config = join::Config {
                card,
                ..config_of(&node, Rc::clone(&transport), ticket)
            };
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
        let mut body = Vec::new();
        request_of(&config).encode(&mut body);
        let held = fill(&config.pool);
        let asked = join::ask(config).await;
        drop(held);
        // The write of the join file takes the first block.
        assert_eq!(asked, Err(exhausted(crate::file::frame(1, &body).len())));
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

/// The request that an ask with `config` and no join file sends.
fn request_of(config: &join::Config) -> region::Request {
    region::Request {
        ticket: config.ticket.public_key(),
        admission: config.ticket.admission(&config.card),
        card: config.card.clone().into(),
        status: config.status.clone(),
    }
}

/// Reads the join file of the mesh directory of `node`.
async fn stored(node: &sim::node::Node) -> Result<Option<region::Request>, Error> {
    let blocks = crate::file::Blocks::new(create_pool()).unwrap();
    join::file::read(&node.files(), Path::new(""), &blocks, Mode::Read).await
}

/// The miss of each voter of [`IDS`] that refuses the unknown ticket `id`.
fn refused_by_each(id: u8) -> Result<region::Founding, Error> {
    let unknown = region::Refused::Unknown {
        public_key: public(id),
    };
    let miss = Miss::from(Refusal::Refused(unknown));
    let missed = IDS.iter().map(|&of| (key(of), miss.clone())).collect();
    Err(Error::Unjoined { missed })
}

// The second ask gives another card, as a node that stops before the answer and signs
// a new card does. It sends the stored request, so each voter admits it again and
// keeps the first card.
#[test]
fn a_second_ask_sends_the_stored_request_and_not_its_card() {
    for seed in 0..2 {
        let mut cluster = recorded_cluster(seed, &IDS, &[]);
        let first = common::member(JOINER).card;
        let asks = vec![
            (ticket_to(TICKET, &IDS), first.clone()),
            (
                ticket_to(TICKET, &IDS),
                member_signed(JOINER, JOINER, 2).card,
            ),
        ];
        let (_, asked) = asks_on(&mut cluster.sim, asks);
        cluster.run(seconds(20));
        let founding = Ok(founding_of(&IDS, &IDS));
        let asked: Vec<_> = asked.lock().unwrap().drain(..).map(|(a, _)| a).collect();
        assert_eq!(asked, [founding.clone(), founding], "run {seed}");
        cluster.script(|_| home_to(9));
        cluster.run(seconds(5));
        let records = cluster.board().records;
        for id in IDS {
            let card = records[&id].get(&JOINER).map(|member| member.card.clone());
            assert_eq!(card, Some(first.clone()), "run {seed}: node {id}");
        }
    }
}

// Each voter refuses ticket 9, which is not recorded. The second ask has another
// ticket, so it writes a new request, which joins.
#[test]
fn an_ask_by_another_ticket_writes_a_new_request_and_joins() {
    let mut cluster = recorded_cluster(0, &IDS, &[]);
    let tickets = vec![ticket_to(9, &IDS), ticket_to(TICKET, &IDS)];
    let asked = ask_each(&mut cluster.sim, tickets);
    cluster.run(seconds(60));
    let asked: Vec<_> = asked.lock().unwrap().drain(..).map(|(a, _)| a).collect();
    assert_eq!(asked, [refused_by_each(9), Ok(founding_of(&IDS, &IDS))]);
}

// No voter of the ticket runs. The file held the request of another public key, and
// the ask writes its own over it before it asks.
#[test]
fn ask_keeps_its_request_in_the_join_file_over_one_of_another_public_key() {
    solo(|node, tasks| async move {
        let blocks = crate::file::Blocks::new(create_pool()).unwrap();
        let other = region::Request {
            ticket: public(TICKET),
            card: common::member(6).card.into(),
            admission: [0; 64],
            status: common::names([]),
        };
        let files = node.files();
        join::file::write(&files, Path::new(""), &blocks, &other)
            .await
            .unwrap();
        let transport = transport_of(&node, &tasks, JOINER);
        let config = config_of(&node, transport, ticket_to(TICKET, &IDS));
        let request = request_of(&config);
        let asked = join::ask(config).await;
        assert!(matches!(asked, Err(Error::Unjoined { .. })), "{asked:?}");
        assert_eq!(stored(&node).await, Ok(Some(request)));
    });
}

// A changed byte of the check, of the version, or of the body.
#[test]
fn ask_gives_the_error_of_a_join_file_that_does_not_read_back_whole() {
    for at in [0, 8, 10] {
        solo(move |node, tasks| async move {
            let transport = transport_of(&node, &tasks, JOINER);
            let config = config_of(&node, transport, ticket_to(TICKET, &IDS));
            let blocks = crate::file::Blocks::new(create_pool()).unwrap();
            let files = node.files();
            let request = request_of(&config);
            join::file::write(&files, Path::new(""), &blocks, &request)
                .await
                .unwrap();
            let file = files.open(Path::new("join"), Mode::Write).await.unwrap();
            let pool = create_pool();
            let byte = file.read_at(at, pool.alloc(1).unwrap()).await.unwrap();
            let block = pool.copy(&[byte.first().unwrap() ^ 1]).unwrap();
            file.write_at(at, &[block]).await.unwrap();
            file.sync().await.unwrap();
            file.close().await;
            let path = PathBuf::from("join");
            assert_eq!(
                join::ask(config).await,
                Err(Error::JoinFile { path }),
                "{at}"
            );
        });
    }
}

/// What `body` gives on a new shard of `node`, which runs for 1 s of `cluster`.
fn on<T: Send + 'static, F: Future<Output = T> + 'static>(
    cluster: &mut Cluster,
    node: &sim::node::Node,
    body: impl FnOnce(sim::node::Node) -> F + Send + 'static,
) -> T {
    let slot = Arc::new(Mutex::new(None));
    let (own, given) = (node.clone(), Arc::clone(&slot));
    let main = move |_| async move {
        let out = body(own).await;
        *given.lock().unwrap() = Some(out);
    };
    let config = env::shards::Config {
        name: "on".into(),
        core: None,
    };
    drop(node.shards().start(config, main).unwrap());
    cluster.run(seconds(1));
    slot.lock()
        .unwrap()
        .take()
        .expect("the body ends within 1 s")
}

/// What the join file of [`JOINER`] holds after a crash by `crash` at `span` into an
/// ask by ticket 8 over the request of ticket 9, in a cluster of [`IDS`]. The next
/// ask joins.
fn crash_in_write(seed: u64, span: Span, crash: Crash) -> Option<region::Request> {
    let mut cluster = recorded_cluster(seed, &IDS, &[]);
    let joiner = cluster.sim.node(sim::node::Config::default());
    let card = common::member(JOINER).card;
    let first = (ticket_to(9, &IDS), card.clone());
    let request = |(ticket, card): &(Ticket, card::Signed)| region::Request {
        ticket: ticket.public_key(),
        admission: ticket.admission(card),
        card: card.clone().into(),
        status: common::names([]),
    };
    let stored_first = request(&first);
    on(&mut cluster, &joiner, |node| async move {
        let blocks = crate::file::Blocks::new(create_pool()).unwrap();
        let files = node.files();
        join::file::write(&files, Path::new(""), &blocks, &stored_first)
            .await
            .unwrap();
    });
    let second = || (ticket_to(TICKET, &IDS), card.clone());
    drop(asks_at(&joiner, vec![second()]));
    cluster.sim.run_for(span).unwrap();
    cluster.sim.crash(&joiner, crash);
    let left = on(
        &mut cluster,
        &joiner,
        |node| async move { stored(&node).await },
    );
    let left = left.unwrap_or_else(|error| panic!("run {seed}, {span:?}: {error}"));
    let outcomes = [Some(request(&first)), Some(request(&second())), None];
    assert!(outcomes.contains(&left), "run {seed}, {span:?}: {left:?}");
    let asked = asks_at(&joiner, vec![second()]);
    cluster.run(seconds(20));
    let asked: Vec<_> = asked.lock().unwrap().drain(..).map(|(a, _)| a).collect();
    assert_eq!(asked, [Ok(founding_of(&IDS, &IDS))], "run {seed}, {span:?}");
    left
}

// A crash at each step of the write of a new request leaves the old request, the new
// one, or none, and the next ask joins.
#[test]
fn a_crash_in_the_write_of_the_join_file_leaves_a_whole_request_and_the_node_joins() {
    for crash in [Crash::Process, Crash::Power] {
        let mut left = Vec::new();
        for step in 0..64_i64 {
            let span = Span::from_nanos(step.saturating_mul(25_000));
            let stored = crash_in_write(step.unsigned_abs(), span, crash);
            left.push(stored.map(|request| request.ticket));
        }
        for ticket in [None, Some(public(9)), Some(public(TICKET))] {
            assert!(
                left.contains(&ticket),
                "{crash:?}: no crash left {ticket:?}"
            );
        }
    }
}

/// What [`Mesh::start`] of [`JOINER`] with the founding of [`IDS`] gives, after
/// `before` ran on its node.
#[expect(clippy::unwrap_in_result, reason = "a test may panic")]
fn open_joiner<F: Future<Output = ()> + 'static>(
    before: impl FnOnce(sim::node::Node) -> F + Send + 'static,
) -> Result<Option<Member>, Error> {
    let mut sim = Sim::new(common::simulation(0));
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        before(node.clone()).await;
        let config = config(&node, &tasks, JOINER, &IDS, &IDS).await;
        let mesh = Mesh::start(config).await?;
        Ok(mesh.member(key(JOINER)))
    })
    .unwrap()
}

/// Writes the join file of `card` by ticket 8 on `node`.
async fn write_join(node: &sim::node::Node, card: card::Signed) {
    let ticket = ticket_to(TICKET, &IDS);
    let request = region::Request {
        ticket: ticket.public_key(),
        admission: ticket.admission(&card),
        card: card.into(),
        status: common::names([]),
    };
    let blocks = crate::file::Blocks::new(create_pool()).unwrap();
    join::file::write(&node.files(), Path::new(""), &blocks, &request)
        .await
        .unwrap();
}

#[test]
fn an_open_of_a_founding_without_this_node_reads_its_join_file() {
    let none = open_joiner(|_| async {});
    assert_eq!(none, Err(Error::NotMember(key(JOINER))));
    let other = open_joiner(|node| async move {
        write_join(&node, common::member(6).card).await;
    });
    assert_eq!(other, Err(Error::NotMember(key(JOINER))));
    let rotated = open_joiner(|node| async move {
        write_join(&node, member_signed(JOINER, 6, 1).card).await;
    });
    assert_eq!(rotated, Err(Error::WrongKey));
    // It opens as a member that is not a voter, with no record until its join applies.
    let joined = open_joiner(|node| async move {
        write_join(&node, common::member(JOINER).card).await;
    });
    assert_eq!(joined, Ok(None));
}

#[test]
fn an_open_of_a_founding_without_this_node_gives_the_error_of_its_join_file() {
    let torn = open_joiner(|node| async move {
        write_join(&node, common::member(JOINER).card).await;
        let file = node
            .files()
            .open(Path::new("join"), Mode::Write)
            .await
            .unwrap();
        let pool = create_pool();
        let byte = file.read_at(10, pool.alloc(1).unwrap()).await.unwrap();
        let block = pool.copy(&[byte.first().unwrap() ^ 1]).unwrap();
        file.write_at(10, &[block]).await.unwrap();
        file.sync().await.unwrap();
        file.close().await;
    });
    let path = PathBuf::from("join");
    assert_eq!(torn, Err(Error::JoinFile { path }));
}

/// Starts [`JOINER`] on `node` of `cluster`. With no founding file it asks by ticket
/// 8 first. It opens with the founding, and puts what it applies on the board.
fn start_joiner(cluster: &Cluster, node: &sim::node::Node) {
    let (own, board) = (node.clone(), Arc::clone(&cluster.board));
    let main = move |tasks: Tasks| async move {
        let node = own;
        let base = config_at(&node, &tasks, JOINER, PORT, BYTES_MAX, &[], &[]).await;
        let transport = Rc::clone(&base.transport);
        let pool = Rc::clone(&base.pool);
        let stored = founding(&base.files, &base.dir, pool).await.unwrap();
        let founding = if let Some(founding) = stored {
            founding
        } else {
            let ticket = ticket_to(TICKET, &IDS);
            let asking = config_of(&node, Rc::clone(&transport), ticket);
            join::ask(asking).await.unwrap()
        };
        let mesh = Mesh::open(Config { founding, ..base }).await.unwrap();
        run_member(mesh, transport, node, tasks, JOINER, board).await;
    };
    let config = env::shards::Config {
        name: "joiner".into(),
        core: None,
    };
    drop(node.shards().start(config, main).unwrap());
}

/// The home that node `id` last applied.
fn last_home(cluster: &Cluster, id: u8) -> Option<node::Key> {
    let board = cluster.board.lock().unwrap();
    board.homes.get(&id)?.last().copied().flatten()
}

/// A cluster of [`IDS`] and [`JOINER`], which joined by ticket 8 with no address on
/// its card, in which each voter proposes itself as the home. Gives the cluster, the
/// node of [`JOINER`], and the leader.
fn joined_cluster(seed: u64) -> (Cluster, sim::node::Node, u8) {
    let mut cluster = recorded_cluster(seed, &IDS, &[]);
    let joiner = cluster.sim.node(sim::node::Config::default());
    start_joiner(&cluster, &joiner);
    cluster.run(seconds(10));
    cluster.script(home_to);
    cluster.run(seconds(10));
    let leader = *cluster.board.lock().unwrap().led.last().unwrap();
    assert_eq!(last_home(&cluster, JOINER), Some(key(leader)), "run {seed}");
    (cluster, joiner, leader)
}

// The node becomes a voter when it appends the voters entry, before it applies its
// own join. The node calls each millisecond while it is not a voter, so a call comes
// in that gap: it waits until the join applies, then proposes the card of the join
// with the new addresses.
#[test]
fn a_node_that_joins_and_becomes_a_voter_sets_its_addresses() {
    for seed in 0..4 {
        let mut cluster = recorded_cluster(seed, &IDS, &[]);
        let at = SocketAddr::new(Ipv4Addr::new(10, 0, 9, JOINER).into(), PORT);
        let addresses = card::addresses::Addresses::new(vec![Address::Udp(at)]);
        let addresses = addresses.unwrap();
        let joiner = cluster.sim.node(sim::node::Config::default());
        let (node, moving) = (joiner.clone(), addresses.clone());
        let moved = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&moved);
        let board = Arc::clone(&cluster.board);
        let main = move |tasks: Tasks| async move {
            let base =
                config_at(&node, &tasks, JOINER, PORT, BYTES_MAX, &[], &[]).await;
            let transport = Rc::clone(&base.transport);
            let asking =
                config_of(&node, Rc::clone(&transport), ticket_to(TICKET, &IDS));
            let founding = join::ask(asking).await.unwrap();
            let mesh = Mesh::open(Config { founding, ..base }).await.unwrap();
            let (serving, streams) = (mesh.clone(), tasks.clone());
            tasks.spawn(async move {
                accept(serving, transport, streams, JOINER, board).await;
            });
            let result = loop {
                match mesh.set_addresses(moving.clone()).await {
                    Err(Error::NoVote) => node.clock().sleep(Span::MILLISECOND).await,
                    result => break result,
                }
            };
            let card = mesh.member(key(JOINER)).unwrap().card.card().clone();
            *slot.lock().unwrap() = Some((result, card));
            std::future::pending::<()>().await;
        };
        let config = env::shards::Config {
            name: "joiner".into(),
            core: None,
        };
        drop(joiner.shards().start(config, main).unwrap());
        cluster.run(seconds(10));
        let leader = *cluster.board.lock().unwrap().led.last().unwrap();
        let voters = [1, 2, 3, JOINER].into();
        cluster
            .board
            .lock()
            .unwrap()
            .configurations
            .insert(leader, voters);
        cluster.run(seconds(10));
        let card = card::Card {
            addresses,
            version: 2,
            ..common::member(JOINER).card.card().clone()
        };
        let moved = moved.lock().unwrap().take();
        assert_eq!(moved, Some((Ok(()), card)), "run {seed}");
    }
}

// No leader can dial the node, so it dials the voters in turn until it reaches the
// one that leads.
#[test]
fn a_node_that_joins_by_ticket_applies_the_changes_of_each_leader() {
    for seed in 0..4 {
        let (mut cluster, _, leader) = joined_cluster(seed);
        let node = cluster.node(leader).clone();
        cluster.sim.crash(&node, Crash::Process);
        cluster.script(|_| home_to(9));
        cluster.run(seconds(20));
        assert_eq!(last_home(&cluster, JOINER), Some(key(9)), "run {seed}");
    }
}

// The node opens again from its founding file and its join file, with no ask.
#[test]
fn a_node_that_joins_and_restarts_applies_the_change_that_it_missed() {
    for seed in 0..4 {
        let (mut cluster, joiner, leader) = joined_cluster(seed);
        cluster.sim.crash(&joiner, Crash::Process);
        cluster.script(|_| home_to(9));
        cluster.run(seconds(10));
        let homes = IDS.map(|id| last_home(&cluster, id));
        assert_eq!(homes, [Some(key(9)); 3], "run {seed}");
        assert_eq!(last_home(&cluster, JOINER), Some(key(leader)), "run {seed}");
        start_joiner(&cluster, &joiner);
        cluster.run(seconds(20));
        assert_eq!(last_home(&cluster, JOINER), Some(key(9)), "run {seed}");
    }
}
