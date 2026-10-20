//! Program writes at a member node whose region names another node the home of their
//! index: the member forwards each write to that home.

use std::cell::Cell;
use std::future::poll_fn;
use std::net::SocketAddr;
use std::pin::pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use access::proof::Kind;
use hub::{serve, writer};
use transport::{Address, Code, Session, Transport};
use types::authority::Authority;
use types::frame::{Form, Path};
use types::time::Span;
use wire::Protocol;
use wire::header::MALFORMED;
use wire::hub::client::write::{Open, Write};
use wire::hub::client::{
    BODY_BYTES_MAX, CHANGED, Challenge, EXPIRED, Place, Places, REFUSED, Refusal,
    Signed, VIA, place,
};
use wire::hub::forward::Forward;
use wire::hub::write::{End, Head, Outcome, Reply, Time};
use wire::hub::{BUSY, FAILED, NOT_HOME, UNKNOWN};

use super::region::OTHER;
use super::write::{stored, taken};
use super::{AREA, BODY_MAX, I64, NODE, POOL, STAMP, TIME, Test};
use crate::agent::{AGENT, Agent, SUBJECT, Stream, name, place as at, reset_with};
use crate::forwarded::{Forwarded, accept_hub, send_places};
use crate::net::{
    HOME, PEER, PORT, STREAMS, own_pool, public_key, transport, transport_sized,
};
use crate::sessions::{Got, Kept, closed_with, recorded, rules, serve_each};

/// The bytes of a message that the transport of each node takes, unless a test says
/// otherwise.
const MESSAGE: usize = 1 << 16;
/// How long the home waits to see that no message comes.
const QUIET: Span = Span::from_nanos(100_000_000);

/// The node of [`OTHER`], its transport, and the session that the member dialed it on.
struct Raw {
    node: sim::node::Node,
    transport: Rc<Transport>,
    session: Session,
}

/// The home's end of a forward that the member sent it.
struct Home {
    node: sim::node::Node,
    stream: Stream,
    /// The first messages of the forward: the forward, the hello, the write, its open,
    /// and the run of places.
    forward: Forward,
    signed: Signed,
    write: Write,
    open: Open,
    run: Vec<u32>,
}

impl Home {
    /// Sends the places of a write of `value`: `time` at place 0, `value` at place 1.
    async fn start(&mut self) {
        send_places(&mut self.stream, &places(), MESSAGE).await;
    }

    /// The next `count` messages of the member.
    async fn messages(&mut self, count: usize) -> Vec<Vec<u8>> {
        let mut got = Vec::new();
        for _ in 0..count {
            let message = self.stream.recv().await.expect("receives");
            got.push(message.expect("a message before the finish"));
        }
        got
    }

    /// The next message of the member, or `None` when none comes in [`QUIET`].
    async fn quiet(&mut self) -> Option<Vec<u8>> {
        let clock = self.node.clock();
        let mut slept = pin!(clock.sleep(QUIET));
        let mut received = pin!(self.stream.recv());
        poll_fn(|cx| {
            if let Poll::Ready(received) = received.as_mut().poll(cx) {
                return Poll::Ready(received.expect("receives"));
            }
            slept.as_mut().poll(cx).map(|()| None)
        })
        .await
    }

    async fn reply(&mut self, reply: Reply) {
        self.stream.send(&encoded(reply)).await;
    }

    /// Stops and resets the forward with `code`.
    fn stop(self, code: u32) {
        let Stream {
            sender, receiver, ..
        } = self.stream;
        sender.reset(Code(code));
        receiver.stop(Code(code));
    }
}

/// The places of a write of `value`.
fn places() -> [Place; 2] {
    [at(1, STAMP, "time", 0), at(2, I64, "value", 0)]
}

/// The messages of a live frame of `stamp` at place 0 and `value` at place 1: the head,
/// the run of ends and counts, and the body.
fn frame(stamp: i64, value: i64) -> [Vec<u8>; 3] {
    let head = Head {
        time: Time::Frame(Path::Live),
        form: Form::Raw,
        series: 2,
    };
    let mut first = vec![0; Head::LEN];
    head.encode(&mut first);
    let mut ends = vec![0; 2 * wire::hub::ends::LEN];
    wire::hub::ends::encode([(0, 8), (1, 16)], &mut ends);
    ends.extend(1u32.to_le_bytes());
    let body = [stamp.to_le_bytes(), value.to_le_bytes()].concat();
    [first, ends, body]
}

/// Sends each of `messages` on `stream`.
async fn send(stream: &mut Stream, messages: &[Vec<u8>]) {
    for message in messages {
        stream.send(message).await;
    }
}

fn encoded(reply: Reply) -> Vec<u8> {
    let mut out = vec![0; reply.encoded_len()];
    reply.encode(&mut out);
    out
}

fn ended(frame: u64, end: End) -> Reply {
    Reply::Outcome {
        frame,
        group: 0,
        outcome: Outcome::Ended(end),
    }
}

/// Opens a write of `value` and gives the stream once the places came.
async fn write(agent: &Agent) -> Stream {
    let mut stream = agent.write(&["value"]).await;
    assert_eq!(stream.started().await.places, places());
    stream
}

/// Finishes `stream`, and gives each reply that the member sent before it finished.
async fn end(stream: &mut Stream) -> Vec<Reply> {
    super::write::end(stream).await
}

/// Runs a program at a third node that admits a hello at the member, a [`Test`] hub at
/// [`NODE`] whose region names [`OTHER`] the home of `time`, then runs `program`. The
/// home at the node of [`OTHER`] takes messages of at most `message` bytes, takes the
/// first forward of the member, reads its first messages, and runs `home` on it. Gives
/// what the member's `serve` gave for each stream of the program, in the order they
/// ended.
fn away<H, P>(
    seed: u64,
    home: impl FnOnce(Home) -> H + Send + 'static,
    program: impl FnOnce(Agent, Challenge) -> P + Send + 'static,
) -> Vec<Result<Got, serve::Error>>
where
    H: Future<Output = ()> + 'static,
    P: Future<Output = ()> + 'static,
{
    let home = async |raw| home(Home::forwarded(raw).await).await;
    away_on(seed, MESSAGE, home, program)
}

/// As [`away`], where the home takes messages of at most `message` bytes, and runs
/// `home` on the session that the member dialed first.
fn away_on<H, P>(
    seed: u64,
    message: usize,
    home: impl FnOnce(Raw) -> H + Send + 'static,
    program: impl FnOnce(Agent, Challenge) -> P + Send + 'static,
) -> Vec<Result<Got, serve::Error>>
where
    H: Future<Output = ()> + 'static,
    P: Future<Output = ()> + 'static,
{
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let nodes = [1, 2, 3].map(|_| sim.node(sim::node::Config::default()));
    let address = |node: &sim::node::Node| {
        Address::Udp(SocketAddr::new(node.addresses()[0], PORT))
    };
    let (at, other) = (address(&nodes[0]), address(&nodes[1]));
    let done = Arc::new(AtomicBool::new(false));
    let served = Arc::new(Mutex::new(Vec::new()));
    let shard = |name: &str| env::shards::Config {
        name: name.into(),
        core: None,
    };
    let (node, kept, ends) = (nodes[0].clone(), Arc::clone(&served), Arc::clone(&done));
    let main = move |tasks| member(node, tasks, other, kept, ends);
    drop(
        nodes[0]
            .shards()
            .start(shard("member"), main)
            .expect("starts"),
    );
    let (node, ends) = (nodes[1].clone(), Arc::clone(&done));
    let main = move |tasks: env::tasks::Tasks| async move {
        let sizes = (message, 1 << 20);
        let transport =
            transport_sized(&node, &tasks, &own_pool(), PEER, sizes, STREAMS);
        let transport = Rc::new(transport);
        let session = transport.accept().await.expect("a session");
        let raw = Raw {
            node: node.clone(),
            transport: Rc::clone(&transport),
            session: session.clone(),
        };
        home(raw).await;
        while !ends.load(Ordering::Relaxed) {
            node.clock().sleep(Span::MILLISECOND).await;
        }
        drop(session);
    };
    drop(
        nodes[1]
            .shards()
            .start(shard("home"), main)
            .expect("starts"),
    );
    let node = nodes[2].clone();
    let main = move |tasks: env::tasks::Tasks| async move {
        let mut agent = Agent::dial(&node, tasks, at, name(SUBJECT)).await;
        let challenge = agent.admit().await;
        program(agent, challenge).await;
    };
    drop(
        nodes[2]
            .shards()
            .start(shard("program"), main)
            .expect("starts"),
    );
    sim.run().expect("the run ends");
    std::mem::take(&mut *served.lock().expect("not poisoned"))
}

/// The member: a [`Test`] hub at [`NODE`] whose region names [`OTHER`], at `other`,
/// the home of `time`. Serves one session of a program, records what each stream gave
/// into `kept`, then sets `done`.
async fn member(
    node: sim::node::Node,
    tasks: env::tasks::Tasks,
    other: Address,
    kept: Kept,
    done: Arc<AtomicBool>,
) {
    let transport = Rc::new(transport(&node, &tasks, &own_pool(), HOME, MESSAGE));
    let region = super::region::open(
        &node,
        &tasks,
        Rc::clone(&transport),
        vec![other],
        super::channels(),
    )
    .await;
    let layout = buffer::Layout::new(AREA, BODY_MAX).expect("a ring");
    let mut test = Test::new(
        node.clone(),
        NODE,
        tasks.clone(),
        layout,
        POOL,
        Some(region),
    )
    .await;
    test.sync().await;
    test.set_home(TIME, OTHER).await;
    test.hub.set_rules(rules());
    let session = transport.accept().await.expect("a session");
    let link = test.hub.link(session.clone());
    let accepted = Rc::new(Cell::new(0));
    serve_each(&session, &link, &tasks, &node.clock(), &kept, &accepted).await;
    recorded(&node, &kept, accepted.get()).await;
    done.store(true, Ordering::Relaxed);
    node.clock().sleep(Span::MILLISECOND).await;
}

impl Home {
    /// The first forward that the member sends on the session of `raw`, after its first
    /// messages.
    async fn forwarded(raw: Raw) -> Home {
        let Forwarded {
            stream,
            forward,
            signed,
            write,
            open,
            run,
        } = Forwarded::accept(&raw.session).await;
        Home {
            node: raw.node,
            stream,
            forward,
            signed,
            write,
            open,
            run,
        }
    }
}

/// The member forwards the open of the program, signed by the program, with the hello
/// that it admitted, then the places of the home, each frame, and each reply, as they
/// came.
#[test]
fn forwards_a_write_whose_home_is_another_node_as_it_came() {
    let served = away(
        1,
        |mut home| async move {
            assert_eq!(home.forward, Forward { places: 1 });
            assert_eq!(home.signed.hello.subject, name(SUBJECT));
            assert_eq!(home.signed.hello.via, public_key(&HOME));
            let open = Open {
                authority: Authority(1),
                lease: None,
                channels: vec![name("value")],
            };
            assert_eq!(home.open, open);
            assert_eq!(
                home.write.length,
                u64::try_from(open.encoded_len()).unwrap()
            );
            assert_eq!(home.run, [0]);
            home.start().await;
            for (seq, value) in (0..).zip([10, 20]) {
                assert_eq!(home.messages(3).await, frame(value, value));
                home.reply(taken(seq + 1, seq, 1)).await;
                home.reply(stored(seq + 1)).await;
            }
            assert_eq!(home.stream.recv().await, Ok(None));
            home.stream.sender.finish().expect("finishes");
        },
        |mut agent, _| async move {
            let mut stream = write(&agent).await;
            for (seq, value) in (0..).zip([10, 20]) {
                send(&mut stream, &frame(value, value)).await;
                assert_eq!(stream.reply().await, taken(seq + 1, seq, 1));
                assert_eq!(stream.reply().await, stored(seq + 1));
            }
            assert_eq!(end(&mut stream).await, []);
            super::read::finish(&mut agent).await;
        },
    );
    assert_eq!(served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A renewal that the member admits inside a frame reaches the home after the frame's
/// last message, and one while the program sends nothing reaches it at once.
#[test]
fn sends_the_home_each_renewal_between_frames() {
    let renewed = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&renewed);
    let served = away(
        2,
        |mut home| async move {
            home.start().await;
            assert_eq!(home.messages(3).await, frame(10, 10));
            let mut nonces = vec![renewal(&home.messages(1).await[0])];
            home.reply(taken(1, 0, 1)).await;
            assert_eq!(home.messages(3).await, frame(20, 20));
            home.reply(taken(2, 1, 1)).await;
            nonces.push(renewal(&home.messages(1).await[0]));
            home.reply(stored(2)).await;
            assert_eq!(home.stream.recv().await, Ok(None));
            home.stream.sender.finish().expect("finishes");
            assert_eq!(*kept.lock().expect("not poisoned"), nonces);
        },
        |mut agent, challenge| async move {
            let mut stream = write(&agent).await;
            let [head, ends, body] = frame(10, 10);
            stream.send(&head).await;
            let challenge = renew(&mut agent, challenge, &renewed).await;
            send(&mut stream, &[ends, body]).await;
            assert_eq!(stream.reply().await, taken(1, 0, 1));
            send(&mut stream, &frame(20, 20)).await;
            assert_eq!(stream.reply().await, taken(2, 1, 1));
            renew(&mut agent, challenge, &renewed).await;
            assert_eq!(stream.reply().await, stored(2));
            assert_eq!(end(&mut stream).await, []);
            super::read::finish(&mut agent).await;
        },
    );
    assert_eq!(served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A renewal that the member admits while it waits for the places of the home reaches
/// the home then, so the hello that the home holds does not expire in that wait.
#[test]
fn sends_the_home_each_renewal_before_its_places() {
    let renewed = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&renewed);
    let served = away(
        22,
        |mut home| async move {
            let mut got = Vec::new();
            for _ in 0..3 {
                if let Some(message) = home.quiet().await {
                    got.push(renewal(&message));
                }
            }
            assert_eq!(got.len(), 1);
            assert_eq!(*kept.lock().expect("not poisoned"), got);
            home.start().await;
            assert_eq!(home.stream.recv().await, Ok(None));
            home.stream.sender.finish().expect("finishes");
        },
        |mut agent, challenge| async move {
            let mut stream = agent.write(&["value"]).await;
            agent.sleep(Span::from_nanos(QUIET.nanos() / 2)).await;
            renew(&mut agent, challenge, &renewed).await;
            assert_eq!(stream.started().await.places, places());
            assert_eq!(end(&mut stream).await, []);
            super::read::finish(&mut agent).await;
        },
    );
    assert_eq!(served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// The member holds the places of the home in the share of the subject until it sends
/// them to the program: the size of a [`Place`] for each that the home counts, then
/// each message of places. Here, with the 40 bytes of the ends and the 8 of the open,
/// it holds `held` bytes of the share, of which `free` were free, when the hold of
/// `length` fails.
fn stops_a_forward_whose_places_pass_the_share(
    seed: u64,
    free: u64,
    length: u64,
    held: u64,
) {
    let served = away(
        seed,
        |mut home| async move {
            home.start().await;
            assert_eq!(home.stream.recv().await, reset_with(0));
        },
        move |mut agent, _| async move {
            let held = agent.unfinished(BODY_BYTES_MAX - free, &[]).await;
            agent.sleep(Span::MILLISECOND).await;
            let mut busy = agent.write(&["value"]).await;
            assert_eq!(busy.recv().await, reset_with(BUSY));
            drop(held);
            super::read::finish(&mut agent).await;
        },
    );
    let share = serve::Error::Share {
        subject: name(SUBJECT),
        length,
        held: BODY_BYTES_MAX - free + held,
    };
    let held = serve::Error::Stream(transport::Error::Reset { code: Code(0) });
    assert_eq!(served, [Err(share), Err(held), Ok(Got::Ended)]);
}

/// The size of the 2 places of a write of `value`.
fn fixed() -> u64 {
    u64::try_from(2 * size_of::<Place>()).expect("a u64 holds a usize")
}

#[test]
fn stops_a_forward_whose_places_pass_the_share_as_busy() {
    stops_a_forward_whose_places_pass_the_share(23, 48 + fixed() - 1, fixed(), 48);
}

/// The 2 places of `value` come in one message of 65 bytes.
#[test]
fn stops_a_forward_whose_message_of_places_passes_the_share_as_busy() {
    let free = 48 + fixed() + 65 - 1;
    stops_a_forward_whose_places_pass_the_share(24, free, 65, 48 + fixed());
}

/// Once the member sends the places to the program, it gives back their whole hold,
/// the message of 65 bytes too, and holds only the 40 bytes of the ends.
#[test]
fn frees_the_places_of_a_forward_once_it_sends_them() {
    let served = away(
        26,
        |mut home| async move {
            home.start().await;
            assert_eq!(home.stream.recv().await, reset_with(0));
        },
        |mut agent, _| async move {
            let started = write(&agent).await;
            let mut busy = agent.unfinished(BODY_BYTES_MAX - 39, &[]).await;
            assert_eq!(busy.recv().await, reset_with(BUSY));
            drop(started);
            super::read::finish(&mut agent).await;
        },
    );
    let share = serve::Error::Share {
        subject: name(SUBJECT),
        length: BODY_BYTES_MAX - 39,
        held: 40,
    };
    let held = serve::Error::Stream(transport::Error::Reset { code: Code(0) });
    assert_eq!(served, [Err(share), Err(held), Ok(Got::Ended)]);
}

/// The member holds the places of a forward in the share of its subject while it sends
/// them to a program that takes none. The frames of 10 001 channels hold 200 020
/// bytes, and the places 3 280 006.
#[test]
fn holds_the_places_of_a_forward_while_it_sends_them() {
    const COUNT: usize = 10_000;
    let served = away(
        27,
        |mut home| async move {
            let mut out = [0; Places::LEN];
            let count = u32::try_from(COUNT).expect("a count");
            Places { count }.encode(&mut out);
            home.stream.send(&out).await;
            let places: Vec<Place> = (0..COUNT)
                .map(|place| {
                    let data_type = if place == 0 { STAMP } else { I64 };
                    let key = u128::try_from(place).expect("a key") + 1;
                    at(key, data_type, &format!("{place:0>255}"), 0)
                })
                .collect();
            for message in place::messages(&places, MESSAGE) {
                let mut out = vec![0; message.encoded_len()];
                message.encode(&mut out);
                home.stream.send(&out).await;
                // Paced, so the pool of 1 MiB of the home keeps room for the next.
                home.node.clock().sleep(Span::MILLISECOND).await;
            }
            assert_eq!(home.stream.recv().await, reset_with(0));
        },
        |mut agent, _| async move {
            let write = agent.write(&vec!["value"; COUNT]).await;
            agent.node.clock().sleep(QUIET).await;
            let mut busy = agent.unfinished(BODY_BYTES_MAX - 39, &[]).await;
            assert_eq!(busy.recv().await, reset_with(BUSY));
            drop(write);
            super::read::finish(&mut agent).await;
        },
    );
    let share = serve::Error::Share {
        subject: name(SUBJECT),
        length: BODY_BYTES_MAX - 39,
        held: 200_020 + 3_280_006,
    };
    let closed = serve::Error::Stream(transport::Error::Closed { code: Code(0) });
    assert_eq!(served, [Err(share), Ok(Got::Ended), Err(closed)]);
}

/// Renews the hello of `agent` with one that echoes `challenge`, keeps its nonce in
/// `nonces`, and gives the next challenge once the member admitted it.
async fn renew(
    agent: &mut Agent,
    challenge: Challenge,
    nonces: &Mutex<Vec<[u8; 16]>>,
) -> Challenge {
    nonces.lock().expect("not poisoned").push(challenge.nonce);
    agent.send_hello(Agent::hello(challenge), &AGENT).await;
    agent.hello.challenge().await
}

/// The nonce of the hello of `message`, a renewal.
fn renewal(message: &[u8]) -> [u8; 16] {
    Signed::decode(message).expect("a hello").hello.nonce
}

/// When the home stops the forward with `code` after its places, the member resets
/// the program's write with the code of `refusal`.
fn resets_the_write(seed: u64, code: u32, refusal: Refusal) {
    let served = away(
        seed,
        move |mut home| async move {
            home.start().await;
            assert_eq!(home.messages(3).await, frame(10, 10));
            home.stop(code);
        },
        move |mut agent, _| async move {
            let mut stream = write(&agent).await;
            send(&mut stream, &frame(10, 10)).await;
            assert_eq!(stream.recv().await, reset_with(refusal.code()));
            super::read::finish(&mut agent).await;
        },
    );
    let forward = serve::Error::Forward {
        home: OTHER,
        refusal,
    };
    assert_eq!(served, [Err(forward), Ok(Got::Ended)]);
}

#[test]
fn resets_the_write_as_refused_when_the_home_refuses_it() {
    resets_the_write(3, REFUSED, Refusal::Refused);
}

#[test]
fn resets_the_write_as_changed_when_the_home_sees_another_subject() {
    resets_the_write(4, CHANGED, Refusal::Changed);
}

#[test]
fn resets_the_write_as_via_when_the_home_sees_another_carrier() {
    resets_the_write(15, VIA, Refusal::Via);
}

#[test]
fn resets_the_write_as_unknown_when_the_home_finds_a_channel_removed() {
    resets_the_write(5, UNKNOWN, Refusal::Unknown);
}

#[test]
fn resets_the_write_as_expired_when_the_hello_expires_at_the_home() {
    resets_the_write(6, EXPIRED, Refusal::Expired);
}

/// A `MALFORMED` of the home tells of a message of the member, not of the program.
#[test]
fn resets_the_write_as_failed_when_the_home_stops_it_as_malformed() {
    resets_the_write(18, MALFORMED, Refusal::Failed);
}

/// A reply out of order, a finish of the home before the program's, or a finish with
/// an outcome due breaks HUB WIRE: the member resets the program's write as `FAILED`,
/// and stops the forward as `MALFORMED`, which the home gets when the program has not
/// finished.
fn resets_a_broken_forward(
    seed: u64,
    finished: bool,
    broken: impl AsyncFnOnce(&mut Home) + Send + 'static,
) {
    let served = away(
        seed,
        move |mut home| async move {
            home.start().await;
            assert_eq!(home.messages(3).await, frame(10, 10));
            broken(&mut home).await;
            if !finished {
                assert_eq!(home.stream.recv().await, reset_with(MALFORMED));
            }
        },
        move |mut agent, _| async move {
            let mut stream = write(&agent).await;
            send(&mut stream, &frame(10, 10)).await;
            if finished {
                stream.sender.finish().expect("finishes");
            }
            assert_eq!(stream.recv().await, reset_with(FAILED));
            super::read::finish(&mut agent).await;
        },
    );
    let forward = serve::Error::Forward {
        home: OTHER,
        refusal: Refusal::Failed,
    };
    assert_eq!(served, [Err(forward), Ok(Got::Ended)]);
}

#[test]
fn resets_the_write_when_the_home_answers_a_frame_it_did_not_get() {
    resets_a_broken_forward(8, false, async |home| {
        home.reply(taken(2, 0, 1)).await;
    });
}

#[test]
fn resets_the_write_when_the_home_finishes_before_the_program() {
    resets_a_broken_forward(9, false, async |home| {
        home.stream.sender.finish().expect("finishes");
    });
}

#[test]
fn resets_the_write_when_the_home_finishes_with_an_outcome_due() {
    resets_a_broken_forward(19, true, async |home| {
        assert_eq!(home.stream.recv().await, Ok(None));
        home.stream.sender.finish().expect("finishes");
    });
}

/// Each reply of the home before the forward breaks reaches the program before the
/// reset.
#[test]
fn sends_the_program_each_reply_of_the_home_before_the_forward_breaks() {
    let removed = Reply::Ended {
        group: 0,
        end: End::Removed,
    };
    let served = away(
        7,
        move |mut home| async move {
            home.start().await;
            assert_eq!(home.messages(3).await, frame(10, 10));
            home.reply(removed).await;
            home.reply(ended(1, End::Removed)).await;
            assert_eq!(home.messages(3).await, frame(20, 20));
            home.stream.sender.finish().expect("finishes");
            assert_eq!(home.stream.recv().await, reset_with(MALFORMED));
        },
        move |mut agent, _| async move {
            let mut stream = write(&agent).await;
            send(&mut stream, &frame(10, 10)).await;
            assert_eq!(stream.reply().await, removed);
            assert_eq!(stream.reply().await, ended(1, End::Removed));
            send(&mut stream, &frame(20, 20)).await;
            assert_eq!(stream.recv().await, reset_with(FAILED));
            super::read::finish(&mut agent).await;
        },
    );
    let forward = serve::Error::Forward {
        home: OTHER,
        refusal: Refusal::Failed,
    };
    assert_eq!(served, [Err(forward), Ok(Got::Ended)]);
}

/// A stop of the home before its places stops the program's write with the code of
/// `refusal`.
fn stops_the_write(seed: u64, code: u32, refusal: Refusal) {
    let served = away(
        seed,
        move |home| async move { home.stop(code) },
        move |mut agent, _| async move {
            let mut stream = agent.write(&["value"]).await;
            assert_eq!(stream.recv().await, reset_with(refusal.code()));
            super::read::finish(&mut agent).await;
        },
    );
    let forward = serve::Error::Forward {
        home: OTHER,
        refusal,
    };
    assert_eq!(served, [Err(forward), Ok(Got::Ended)]);
}

#[test]
fn stops_the_write_as_not_home_when_the_home_has_moved() {
    stops_the_write(10, NOT_HOME, Refusal::NotHome);
}

#[test]
fn stops_the_write_with_the_code_of_the_home_before_its_places() {
    stops_the_write(11, EXPIRED, Refusal::Expired);
}

#[test]
fn stops_the_write_as_failed_when_the_home_stops_it_as_malformed_before_its_places() {
    stops_the_write(20, MALFORMED, Refusal::Failed);
}

/// A home that sends more places than the channels of the open and its index breaks
/// HUB WIRE.
#[test]
fn stops_the_write_as_failed_when_the_home_sends_more_places_than_the_open() {
    let served = away(
        21,
        |mut home| async move {
            let mut out = [0; Places::LEN];
            Places { count: 3 }.encode(&mut out);
            home.stream.send(&out).await;
            assert_eq!(home.stream.recv().await, reset_with(MALFORMED));
        },
        |mut agent, _| async move {
            let mut stream = agent.write(&["value"]).await;
            assert_eq!(stream.recv().await, reset_with(FAILED));
            super::read::finish(&mut agent).await;
        },
    );
    let forward = serve::Error::Forward {
        home: OTHER,
        refusal: Refusal::Failed,
    };
    assert_eq!(served, [Err(forward), Ok(Got::Ended)]);
}

/// The member refuses a lease that is not longer than zero before it finds the home,
/// so the first forward that the home gets is of the next write.
#[test]
fn stops_a_write_whose_lease_is_zero_as_malformed_before_the_forward() {
    let served = away(
        25,
        |home| async move {
            assert_eq!(home.open.lease, None);
            home.stop(REFUSED);
        },
        |mut agent, _| async move {
            let open = Open {
                authority: Authority(1),
                lease: Some(Span::ZERO),
                channels: vec![name("value")],
            };
            let mut stream = agent.open().await;
            stream.write(&open, Kind::Write, &AGENT).await;
            assert_eq!(stream.recv().await, reset_with(MALFORMED));
            let mut stream = agent.write(&["value"]).await;
            assert_eq!(stream.recv().await, reset_with(REFUSED));
            super::read::finish(&mut agent).await;
        },
    );
    let lease = home::writer::Error::Lease { span: Span::ZERO };
    let lease = serve::Error::Writer(Box::new(writer::Error::Home(lease)));
    let forward = serve::Error::Forward {
        home: OTHER,
        refusal: Refusal::Refused,
    };
    assert_eq!(served, [Err(lease), Err(forward), Ok(Got::Ended)]);
}

/// The member sends each message of the program as it came, so a home that takes
/// smaller messages than the member fails the write at its open: the home gets no
/// stream of the hub before its session ends.
#[test]
fn stops_the_write_as_failed_when_the_home_takes_smaller_messages() {
    let served = away_on(
        12,
        MESSAGE / 2,
        async |raw| {
            let ended = loop {
                let mut incoming = match raw.session.accept().await {
                    Ok(incoming) => incoming,
                    Err(error) => break error,
                };
                let header = incoming.receiver.recv().await.expect("receives");
                let header = header.expect("the header comes before the finish");
                assert!(
                    matches!(wire::header::decode(&header), Ok((Protocol::Mesh, []))),
                    "a stream of the mesh"
                );
            };
            assert_eq!(ended, transport::Error::TimedOut);
        },
        |mut agent, _| async move {
            let mut stream = agent.write(&["value"]).await;
            assert_eq!(stream.recv().await, reset_with(FAILED));
            super::read::finish(&mut agent).await;
        },
    );
    let limit = serve::Error::Limit {
        home: OTHER,
        home_max: MESSAGE / 2,
        max: MESSAGE,
    };
    assert_eq!(
        limit.to_string(),
        format!(
            "the home {OTHER} takes messages of at most {} bytes, under the {MESSAGE} \
             bytes of this node: give each node of the mesh the same message limit",
            MESSAGE / 2
        )
    );
    assert_eq!(served, [Err(limit), Ok(Got::Ended)]);
}

/// A session to the home that closes with `Code(0)` before the places, as one that
/// loses the tie-break does, gets another try.
#[test]
fn forwards_the_write_again_when_the_home_closes_the_session_with_code_0() {
    let served = away_on(
        16,
        MESSAGE,
        async |raw| {
            accept_hub(&raw.session).await.expect("a stream");
            raw.session.close(Code(0));
            let session = raw.transport.accept().await.expect("a session");
            let held = session.clone();
            let home = Home::forwarded(Raw { session, ..raw }).await;
            assert_eq!(home.run, [0]);
            let node = home.node.clone();
            home.stop(REFUSED);
            // A session closes with its last handle, so hold it until the stop is read.
            node.clock().sleep(QUIET).await;
            drop(held);
        },
        |mut agent, _| async move {
            let mut stream = agent.write(&["value"]).await;
            assert_eq!(stream.recv().await, reset_with(REFUSED));
            super::read::finish(&mut agent).await;
        },
    );
    let forward = serve::Error::Forward {
        home: OTHER,
        refusal: Refusal::Refused,
    };
    assert_eq!(served, [Err(forward), Ok(Got::Ended)]);
}

/// The member holds 20 bytes for each channel of the open and for its index in the
/// share of the subject until the forward ends. The open of `value` is 8 bytes and its
/// names 21, so they fit where its ends do not.
#[test]
fn stops_a_forward_whose_ends_pass_the_share_as_busy() {
    let served = away(
        17,
        |home| async move { home.stop(REFUSED) },
        |mut agent, _| async move {
            let held = agent.unfinished(BODY_BYTES_MAX - 39, &[]).await;
            agent.sleep(Span::MILLISECOND).await;
            let mut busy = agent.write(&["value"]).await;
            assert_eq!(busy.recv().await, reset_with(BUSY));
            drop(held);
            agent.sleep(Span::MILLISECOND).await;
            let mut stream = agent.write(&["value"]).await;
            assert_eq!(stream.recv().await, reset_with(REFUSED));
            super::read::finish(&mut agent).await;
        },
    );
    let share = serve::Error::Share {
        subject: name(SUBJECT),
        length: 40,
        held: BODY_BYTES_MAX - 39,
    };
    let forward = serve::Error::Forward {
        home: OTHER,
        refusal: Refusal::Refused,
    };
    let held = serve::Error::Stream(transport::Error::Reset { code: Code(0) });
    assert_eq!(
        served,
        [Err(share), Err(held), Err(forward), Ok(Got::Ended)]
    );
}

/// With 64 frames that have no outcome, the member reads no more frames of the program
/// until an outcome comes.
#[test]
fn reads_no_more_of_the_program_while_64_frames_have_no_outcome() {
    let served = away(
        13,
        |mut home| async move {
            home.start().await;
            for value in 0..64 {
                assert_eq!(home.messages(3).await, frame(value, value));
            }
            assert_eq!(home.quiet().await, None);
            home.reply(taken(1, 0, 1)).await;
            assert_eq!(home.messages(3).await, frame(64, 64));
            for frame in 2..=65 {
                home.reply(taken(frame, frame - 1, 1)).await;
            }
            assert_eq!(home.stream.recv().await, Ok(None));
            home.stream.sender.finish().expect("finishes");
        },
        |mut agent, _| async move {
            let mut stream = write(&agent).await;
            for value in 0..65 {
                send(&mut stream, &frame(value, value)).await;
            }
            let replies = end(&mut stream).await;
            let expected: Vec<_> =
                (1..=65).map(|frame| taken(frame, frame - 1, 1)).collect();
            assert_eq!(replies, expected);
            super::read::finish(&mut agent).await;
        },
    );
    assert_eq!(served, [Ok(Got::Ended), Ok(Got::Ended)]);
}

/// A close of the program's session drops the write, which resets the forward.
#[test]
fn resets_the_forward_when_the_program_closes_its_session() {
    let served = away(
        14,
        |mut home| async move {
            home.start().await;
            assert_eq!(home.stream.recv().await, reset_with(0));
        },
        |agent, _| async move {
            let _stream = write(&agent).await;
            agent.session.close(Code(0));
            drop(agent.session.closed().await);
        },
    );
    let closed = || Err(serve::Error::Stream(closed_with(0)));
    assert_eq!(served, [closed(), closed()]);
}
