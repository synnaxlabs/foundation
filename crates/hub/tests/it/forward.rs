//! Forwards that `Link::serve` serves at the home of their index: a member node of the
//! region carries a program's write over a real transport.

use std::cell::Cell;
use std::path::Path as FilePath;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use access::proof::{Error as Refusal, Field, Kind};
use block::Pool;
use hub::{serve, writer};
use transport::{Address, Class, Code, Session};
use types::authority::Authority;
use types::frame::{Form, Path};
use types::hello::Hello;
use types::time::Span;
use wire::Protocol;
use wire::header::MALFORMED;
use wire::hub::client::write::Open;
use wire::hub::client::{BODY_BYTES_MAX, CHANGED, EXPIRED, REFUSED, UNSYNCED, VIA};
use wire::hub::forward::{Forward, places};
use wire::hub::write::{Head, Time};
use wire::hub::{BUSY, NOT_HOME};

use super::write::{end, place, send, stored, taken};
use super::{AREA, BODY_MAX, I64, POOL, RING, STAMP, TIME, Test, channels, region};
use crate::agent::{
    AGENT, Agent, CONNECTION, LIFE, SECOND, SUBJECT, Stream, name, reset_with, signed,
};
use crate::net::{HOME, NODE, PEER, own_pool, public_key, transport};
use crate::sessions::{
    Got, OTHER as UNLISTED, closed_with, recorded, rules, run_program, serve_each,
};

/// When the home of a test changes what the forward meets, after the member sent it.
const START: Span = Span::from_nanos(100_000_000);
/// The life of a hello that ends during a test.
const SHORT: Span = Span::from_nanos(500_000_000);

/// The member node's end of its session with the home.
struct Member {
    node: sim::node::Node,
    tasks: env::tasks::Tasks,
    /// The mesh clock of the member's node, which has mesh time.
    mesh: clock::Reader,
    /// The address of the home.
    at: Address,
    pool: Rc<Pool>,
    session: Session,
}

impl Member {
    /// A hello of [`SUBJECT`] through [`region::OTHER`] that lives `life` from the
    /// latest mesh time of the member's node.
    fn hello(&self, life: Span) -> Hello {
        Hello {
            subject: name(SUBJECT),
            key: public_key(&AGENT),
            via: region::OTHER,
            connection: CONNECTION,
            nonce: [0; 16],
            expires: self.mesh.now().mesh.expect("mesh time").latest + life,
        }
    }

    /// Opens a forward of a group of `places` places, and sends its header, the
    /// forward, and `hello` signed by [`AGENT`].
    async fn forward(&self, places: u32, hello: Hello) -> Stream {
        let (sender, receiver) =
            self.session.open(Class::Complete).await.expect("opens");
        let mut stream = Stream::new(Rc::clone(&self.pool), sender, receiver);
        stream.send(&wire::header::encode(Protocol::Hub)).await;
        let mut out = [0; Forward::LEN];
        Forward { places }.encode(&mut out);
        stream.send(&out).await;
        stream.send(&signed(hello, &AGENT)).await;
        stream
    }

    /// As [`Member::forward`], then a write of `channels` signed by [`AGENT`], and the
    /// run of `at`, the places of the group.
    async fn opened(&self, hello: Hello, channels: &[&str], at: &[u32]) -> Stream {
        let count = u32::try_from(at.len()).expect("a count");
        let mut stream = self.forward(count, hello).await;
        stream.write(&open(channels), Kind::Write, &AGENT).await;
        stream.send(&run(at)).await;
        stream
    }

    /// As [`Member::opened`], with a hello of [`LIFE`].
    async fn write(&self, channels: &[&str], at: &[u32]) -> Stream {
        self.opened(self.hello(LIFE), channels, at).await
    }

    async fn sleep(&self, span: Span) {
        self.node.clock().sleep(span).await;
    }
}

/// An open of `channels` at authority 1 with no lease.
fn open(channels: &[&str]) -> Open {
    Open {
        authority: Authority(1),
        lease: None,
        channels: channels.iter().map(|channel| name(channel)).collect(),
    }
}

/// The run of `at` in one message.
fn run(at: &[u32]) -> Vec<u8> {
    let mut out = vec![0; at.len() * places::LEN];
    places::encode(at.iter().copied(), &mut out);
    out
}

/// Makes this node the home of `time`.
async fn homed(test: &Test) {
    test.set_home(TIME, NODE).await;
}

/// Runs one session of the member node [`region::OTHER`], which proves [`PEER`], with
/// a home at [`NODE`] in the region of [`region::open`]. The home sets [`rules`],
/// serves each hub stream of the session as [`serve_each`] does, and runs `during`
/// while the session is open. Gives what `serve` gave for each stream, in the order
/// they ended.
fn forwarded<P>(
    seed: u64,
    during: impl AsyncFnOnce(&Test) + Send + 'static,
    member: impl FnOnce(Member) -> P + Send + 'static,
) -> Vec<Result<Got, serve::Error>>
where
    P: Future<Output = ()> + 'static,
{
    forwarded_with(seed, 1, during, async |_| {}, member)
}

/// As [`forwarded`], where the home serves the member's session and `sessions - 1`
/// sessions after it, and runs `after` once each stream ended.
fn forwarded_with<P>(
    seed: u64,
    sessions: usize,
    during: impl AsyncFnOnce(&Test) + Send + 'static,
    after: impl AsyncFnOnce(&Test) + Send + 'static,
    member: impl FnOnce(Member) -> P + Send + 'static,
) -> Vec<Result<Got, serve::Error>>
where
    P: Future<Output = ()> + 'static,
{
    forwarded_on(seed, sessions, Mesh::Synced, during, after, member)
}

/// Runs the mesh clock of `test` with [`Mesh::Synced`] and sets [`rules`]. Gives
/// `tasks` under [`Test::pause`], for the serve tasks.
async fn ready(
    test: &mut Test,
    tasks: &env::tasks::Tasks,
    mesh: Mesh,
) -> env::tasks::Tasks {
    if matches!(mesh, Mesh::Synced) {
        test.sync().await;
    }
    test.hub.set_rules(rules());
    env::tasks::Tasks::new(super::Counted {
        tasks: tasks.clone(),
        ended: Rc::new(Cell::new(0)),
        pause: Rc::clone(&test.pause),
    })
}

/// Whether the home has mesh time when it serves a forward.
enum Mesh {
    Synced,
    Unsynced,
}

/// As [`forwarded_with`], at a home whose mesh clock runs only with `mesh`. The serve
/// task of each stream runs under [`Test::pause`].
fn forwarded_on<P>(
    seed: u64,
    sessions: usize,
    mesh: Mesh,
    during: impl AsyncFnOnce(&Test) + Send + 'static,
    after: impl AsyncFnOnce(&Test) + Send + 'static,
    member: impl FnOnce(Member) -> P + Send + 'static,
) -> Vec<Result<Got, serve::Error>>
where
    P: Future<Output = ()> + 'static,
{
    let served = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&served);
    let home = move |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let transport = Rc::new(transport(&node, &tasks, &own_pool(), HOME, 1 << 16));
        let region =
            region::open(&node, &tasks, Rc::clone(&transport), Vec::new(), channels())
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
        let paused = ready(&mut test, &tasks, mesh).await;
        let accepted = Rc::new(Cell::new(0));
        let mut ends = Vec::new();
        for _ in 0..sessions {
            let session = transport.accept().await.expect("a session");
            let link = test.hub.link(session.clone());
            tasks.spawn(serve_each(
                &session,
                &link,
                &paused,
                &node.clock(),
                &kept,
                &accepted,
            ));
            ends.push(session);
        }
        during(&test).await;
        for session in ends {
            session.closed().await;
        }
        recorded(&node, &kept, accepted.get()).await;
        after(&test).await;
    };
    run_program(seed, home, |node, tasks, at| async move {
        let (clock, mesh) = clock::Clock::new(node.clock());
        let wall = node.wall();
        tasks.spawn(async move { clock.run(wall).await });
        while mesh.now().mesh.is_none() {
            node.clock().sleep(Span::from_nanos(1)).await;
        }
        let pool = own_pool();
        let transport = transport(&node, &tasks, &pool, PEER, 1 << 16);
        let session = transport
            .dial(public_key(&HOME), &[at])
            .await
            .expect("dials");
        let ends = session.clone();
        member(Member {
            node: node.clone(),
            tasks,
            mesh,
            at,
            pool,
            session,
        })
        .await;
        ends.close(Code(0));
        node.clock().sleep(Span::MILLISECOND).await;
    });
    std::mem::take(&mut *served.lock().expect("not poisoned"))
}

/// A forward of the group of `value`, in an open of `value` and `value-b`, writes as a
/// program's write of its group, and the handoff to its writer names the member as
/// its `via`.
#[test]
fn writes_the_group_at_the_places_of_a_forward() {
    let after = async |test: &Test| {
        let file = test
            .node
            .files()
            .open(FilePath::new(RING), env::files::Mode::Read)
            .await
            .expect("opens");
        let block = test.pool.alloc(1 << 16).expect("a block");
        let ring = file.read_at(0, block).await.expect("reads");
        let via = region::OTHER.as_u128().to_le_bytes();
        let body = [&[1], &via[..], SUBJECT.as_bytes()].concat();
        assert!(
            ring.windows(body.len()).any(|bytes| bytes == body),
            "no handoff body names the member"
        );
    };
    let served = forwarded_with(4100, 1, homed, after, |member| async move {
        let mut stream = member.write(&["value", "value-b"], &[0]).await;
        let start = stream.started().await;
        assert_eq!(
            start.places,
            [place(1, STAMP, "time", 0), place(2, I64, "value", 0)]
        );
        send(&mut stream, &[1], &[10]).await;
        assert_eq!(stream.reply().await, taken(1, 0, 1));
        assert_eq!(end(&mut stream).await, [stored(1)]);
    });
    assert_eq!(served, [Ok(Got::Ended)]);
}

/// The home admits a forwarded hello only when its `via` names the member that
/// carries it.
#[test]
fn stops_a_forward_whose_hello_names_another_node_as_via() {
    let served = forwarded(4101, homed, |member| async move {
        let hello = Hello {
            via: NODE,
            ..member.hello(LIFE)
        };
        let mut stream = member.forward(1, hello).await;
        assert_eq!(stream.recv().await, reset_with(VIA));
    });
    let via = Refusal::Via {
        via: NODE,
        peer: region::OTHER,
    };
    assert_eq!(served, [Err(serve::Error::Access(via))]);
}

#[test]
fn refuses_a_forwarded_open_signed_with_another_key() {
    let served = forwarded(4102, homed, |member| async move {
        let mut stream = member.forward(1, member.hello(LIFE)).await;
        stream
            .write(&open(&["value"]), Kind::Write, &UNLISTED)
            .await;
        assert_eq!(stream.recv().await, reset_with(REFUSED));
    });
    assert_eq!(served, [Err(serve::Error::Access(Refusal::Signature))]);
}

#[test]
fn stops_a_forward_at_a_renewal_of_another_subject_as_changed() {
    let served = forwarded(4103, homed, |member| async move {
        let mut stream = member.write(&["value"], &[0]).await;
        stream.started().await;
        let hello = Hello {
            subject: name(SECOND),
            ..member.hello(LIFE)
        };
        stream.send(&signed(hello, &AGENT)).await;
        assert_eq!(stream.recv().await, reset_with(CHANGED));
    });
    let changed = Refusal::Changed {
        field: Field::Subject,
    };
    assert_eq!(served, [Err(serve::Error::Access(changed))]);
}

/// A forward takes frames until its hello ends, then stops as `EXPIRED`.
#[test]
fn stops_a_forward_as_expired_at_the_end_of_its_hello() {
    let served = forwarded(4104, homed, |member| async move {
        let mut stream = member.opened(member.hello(SHORT), &["value"], &[0]).await;
        stream.started().await;
        member
            .sleep(Span::from_nanos(SHORT.nanos() - START.nanos()))
            .await;
        send(&mut stream, &[1], &[10]).await;
        assert_eq!(stream.reply().await, taken(1, 0, 1));
        assert_eq!(stream.reply().await, stored(1));
        assert_eq!(stream.recv().await, reset_with(EXPIRED));
    });
    assert!(
        matches!(
            served[..],
            [Err(serve::Error::Access(Refusal::Expired { .. }))]
        ),
        "{served:?}"
    );
}

/// A renewal between frames moves the end of the forward, so a frame after the end of
/// the first hello is taken.
#[test]
fn moves_the_end_of_a_forward_at_a_renewal_between_frames() {
    let served = forwarded(4105, homed, |member| async move {
        let mut stream = member.opened(member.hello(SHORT), &["value"], &[0]).await;
        stream.started().await;
        send(&mut stream, &[1], &[10]).await;
        assert_eq!(stream.reply().await, taken(1, 0, 1));
        assert_eq!(stream.reply().await, stored(1));
        stream.send(&signed(member.hello(LIFE), &AGENT)).await;
        member.sleep(SHORT).await;
        send(&mut stream, &[2], &[20]).await;
        assert_eq!(stream.reply().await, taken(2, 1, 1));
        assert_eq!(end(&mut stream).await, [stored(2)]);
    });
    assert_eq!(served, [Ok(Got::Ended)]);
}

/// A renewal during the wait for the home of the index moves the end of the forward.
#[test]
fn renews_a_forward_while_it_waits_for_its_home() {
    let during = async |test: &Test| {
        test.clock
            .sleep(Span::from_nanos(SHORT.nanos() + START.nanos()))
            .await;
        homed(test).await;
    };
    let served = forwarded(4106, during, |member| async move {
        let mut stream = member.opened(member.hello(SHORT), &["value"], &[0]).await;
        member.sleep(START).await;
        stream.send(&signed(member.hello(LIFE), &AGENT)).await;
        stream.started().await;
        send(&mut stream, &[1], &[10]).await;
        assert_eq!(stream.reply().await, taken(1, 0, 1));
        assert_eq!(end(&mut stream).await, [stored(1)]);
    });
    assert_eq!(served, [Ok(Got::Ended)]);
}

/// A message inside a frame is data, also when its first byte is the kind of a hello:
/// the body of this frame starts with the stamp 4.
#[test]
fn takes_a_frame_whose_body_starts_with_the_kind_of_a_hello() {
    let served = forwarded(4107, homed, |member| async move {
        let mut stream = member.write(&["value"], &[0]).await;
        stream.started().await;
        send(&mut stream, &[4], &[10]).await;
        assert_eq!(stream.reply().await, taken(1, 0, 1));
        assert_eq!(end(&mut stream).await, [stored(1)]);
    });
    assert_eq!(served, [Ok(Got::Ended)]);
}

/// A place that is not after the place before it, or not below the channels of the
/// open, stops the forward as `MALFORMED`.
#[test]
fn stops_a_forward_at_a_place_out_of_its_run_as_malformed() {
    let served = forwarded(4108, homed, |member| async move {
        let mut stream = member.write(&["value", "value-c"], &[1, 1]).await;
        assert_eq!(stream.recv().await, reset_with(MALFORMED));
        let mut stream = member.write(&["value"], &[1]).await;
        assert_eq!(stream.recv().await, reset_with(MALFORMED));
    });
    let place = |place| Err(serve::Error::Message(wire::hub::Error::Place { place }));
    assert_eq!(served, [place(1), place(1)]);
}

/// A forward whose index has another home stops as `NOT_HOME`.
#[test]
fn stops_a_forward_whose_home_is_another_node_as_not_home() {
    let during = async |test: &Test| test.set_home(TIME, region::OTHER).await;
    let served = forwarded(4109, during, |member| async move {
        let mut stream = member.write(&["value"], &[0]).await;
        assert_eq!(stream.recv().await, reset_with(NOT_HOME));
    });
    let remote = writer::Error::Remote {
        home: region::OTHER,
    };
    assert_eq!(served, [Err(serve::Error::Writer(Box::new(remote)))]);
}

/// A forward whose stream ends before its last place opens nothing, and finishes.
#[test]
fn finishes_a_forward_that_ends_before_its_last_place() {
    let served = forwarded(4110, homed, |member| async move {
        let mut stream = member.forward(2, member.hello(LIFE)).await;
        stream
            .write(&open(&["value", "value-c"]), Kind::Write, &AGENT)
            .await;
        stream.send(&run(&[0])).await;
        assert_eq!(end(&mut stream).await, []);
    });
    assert_eq!(served, [Ok(Got::Ended)]);
}

/// A forward holds its decoded names and 4 bytes for each channel of its open in the
/// share of its subject before it reads a place. The names of `value` and `value-b`
/// are 44 bytes, so a forward of them holds 52.
#[test]
fn stops_a_forward_whose_places_pass_the_share_as_busy() {
    let held = BODY_BYTES_MAX - 24;
    let served = forwarded_with(
        4111,
        2,
        homed,
        async |_| {},
        move |member| async move {
            let (node, tasks) = (&member.node, member.tasks.clone());
            let mut agent = Agent::dial(node, tasks, member.at, name(SUBJECT)).await;
            agent.admit().await;
            let _holding = agent.unfinished(held, &[]).await;
            member.sleep(Span::MILLISECOND).await;
            let mut busy = member.forward(1, member.hello(LIFE)).await;
            let channels = open(&["value", "value-b"]);
            busy.write(&channels, Kind::Write, &AGENT).await;
            assert_eq!(busy.recv().await, reset_with(BUSY));
            agent.session.close(Code(0));
        },
    );
    let share = serve::Error::Share {
        subject: name(SUBJECT),
        length: 52,
        held,
    };
    let closed = || Err(serve::Error::Stream(closed_with(0)));
    assert_eq!(served, [Err(share), closed(), closed()]);
}

/// A message of a run of counts whose first byte is the kind of a hello is data: the
/// counts of this frame of 4 samples come in their own message.
#[test]
fn takes_a_frame_whose_counts_start_with_the_kind_of_a_hello() {
    let served = forwarded(4112, homed, |member| async move {
        let mut stream = member.write(&["value"], &[0]).await;
        stream.started().await;
        let mut head = [0; Head::LEN];
        Head {
            time: Time::Frame(Path::Live),
            form: Form::Raw,
            series: 2,
        }
        .encode(&mut head);
        stream.send(&head).await;
        let mut ends = vec![0; 2 * wire::hub::ends::LEN];
        wire::hub::ends::encode([(0, 32), (1, 64)], &mut ends);
        stream.send(&ends).await;
        stream.send(&4_u32.to_le_bytes()).await;
        let body: Vec<u8> = [1_i64, 2, 3, 4, 10, 20, 30, 40]
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect();
        stream.send(&body).await;
        assert_eq!(stream.reply().await, taken(1, 0, 4));
        assert_eq!(end(&mut stream).await, [stored(1)]);
    });
    assert_eq!(served, [Ok(Got::Ended)]);
}

/// During the wait for the home, a renewal signed with another key stops the forward
/// as `REFUSED`, a renewal of another subject as `CHANGED`, and a message that is not
/// a hello as `MALFORMED`.
#[test]
fn stops_a_forward_at_a_bad_message_while_it_waits_for_its_home() {
    let during = async |test: &Test| test.clock.sleep(SHORT).await;
    let served = forwarded(4113, during, |member| async move {
        let mut stream = member.write(&["value"], &[0]).await;
        stream.send(&signed(member.hello(LIFE), &UNLISTED)).await;
        assert_eq!(stream.recv().await, reset_with(REFUSED));
        let mut stream = member.write(&["value"], &[0]).await;
        let hello = Hello {
            subject: name(SECOND),
            ..member.hello(LIFE)
        };
        stream.send(&signed(hello, &AGENT)).await;
        assert_eq!(stream.recv().await, reset_with(CHANGED));
        let mut stream = member.write(&["value"], &[0]).await;
        let mut head = [0; Head::LEN];
        Head {
            time: Time::Node,
            form: Form::Raw,
            series: 1,
        }
        .encode(&mut head);
        stream.send(&head).await;
        assert_eq!(stream.recv().await, reset_with(MALFORMED));
    });
    let changed = Refusal::Changed {
        field: Field::Subject,
    };
    let kind = wire::hub::client::Error::Kind { kind: 7 };
    assert_eq!(
        served,
        [
            Err(serve::Error::Access(Refusal::Signature)),
            Err(serve::Error::Access(changed)),
            Err(serve::Error::Client(kind)),
        ]
    );
}

/// Between frames, a cut hello stops the forward as `MALFORMED`.
#[test]
fn stops_a_forward_at_a_cut_hello_between_frames_as_malformed() {
    let served = forwarded(4114, homed, |member| async move {
        let mut stream = member.write(&["value"], &[0]).await;
        stream.started().await;
        send(&mut stream, &[1], &[10]).await;
        assert_eq!(stream.reply().await, taken(1, 0, 1));
        assert_eq!(stream.reply().await, stored(1));
        let hello = signed(member.hello(LIFE), &AGENT);
        stream.send(&hello[..10]).await;
        assert_eq!(stream.recv().await, reset_with(MALFORMED));
    });
    let cut = wire::hub::client::Error::Length { len: 10 };
    assert_eq!(served, [Err(serve::Error::Client(cut))]);
}

/// A forward holds the ends and counts of its group after its last place: 40 bytes
/// for `value` and its index, past the share that its names and places fit.
#[test]
fn stops_a_forward_whose_ends_pass_the_share_as_busy() {
    let held = BODY_BYTES_MAX - 52 - 39;
    let served = forwarded_with(
        4115,
        2,
        homed,
        async |_| {},
        move |member| async move {
            let (node, tasks) = (&member.node, member.tasks.clone());
            let mut agent = Agent::dial(node, tasks, member.at, name(SUBJECT)).await;
            agent.admit().await;
            let _holding = agent.unfinished(held, &[]).await;
            member.sleep(Span::MILLISECOND).await;
            let mut busy = member.write(&["value", "value-b"], &[0]).await;
            assert_eq!(busy.recv().await, reset_with(BUSY));
            agent.session.close(Code(0));
        },
    );
    let share = serve::Error::Share {
        subject: name(SUBJECT),
        length: 40,
        held: held + 52,
    };
    let closed = || Err(serve::Error::Stream(closed_with(0)));
    assert_eq!(served, [Err(share), closed(), closed()]);
}

/// A first hello that has ended stops the forward as `EXPIRED`.
#[test]
fn stops_a_forward_whose_first_hello_has_ended_as_expired() {
    let served = forwarded(4116, homed, |member| async move {
        let mut stream = member.forward(1, member.hello(Span::ZERO)).await;
        assert_eq!(stream.recv().await, reset_with(EXPIRED));
    });
    assert!(
        matches!(
            served[..],
            [Err(serve::Error::Access(Refusal::Expired { .. }))]
        ),
        "{served:?}"
    );
}

/// A renewal that comes in the poll of the forward's task in which the first hello
/// ends moves the end: the write polls before the expiry.
#[test]
fn renews_a_forward_whose_renewal_and_end_come_in_one_poll() {
    let during = async |test: &Test| {
        homed(test).await;
        test.clock.sleep(START).await;
        test.pause.pause();
        test.clock.sleep(SHORT).await;
        test.pause.resume();
    };
    let served = forwarded(4117, during, |member| async move {
        let mut stream = member.opened(member.hello(SHORT), &["value"], &[0]).await;
        stream.started().await;
        send(&mut stream, &[1], &[10]).await;
        assert_eq!(stream.reply().await, taken(1, 0, 1));
        assert_eq!(stream.reply().await, stored(1));
        member.sleep(Span::from_nanos(2 * START.nanos())).await;
        stream.send(&signed(member.hello(LIFE), &AGENT)).await;
        member.sleep(SHORT).await;
        send(&mut stream, &[2], &[20]).await;
        assert_eq!(stream.reply().await, taken(2, 1, 1));
        assert_eq!(end(&mut stream).await, [stored(2)]);
    });
    assert_eq!(served, [Ok(Got::Ended)]);
}

/// A home with no mesh time stops a forward at its first hello with `UNSYNCED`.
#[test]
fn stops_a_forward_at_a_home_with_no_mesh_time_as_unsynced() {
    let served = forwarded_on(
        4118,
        1,
        Mesh::Unsynced,
        async |_| {},
        async |_| {},
        |member| async move {
            let mut stream = member.forward(1, member.hello(LIFE)).await;
            assert_eq!(stream.recv().await, reset_with(UNSYNCED));
        },
    );
    assert_eq!(served, [Err(serve::Error::Access(Refusal::Unsynced))]);
}
