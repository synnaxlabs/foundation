//! A request that the hub refuses for its length allocates no body, the heap of a hub
//! does not grow over many opens and closes of a named complete reader, a served open
//! holds and copies each of its keys once, a read that waits for its home holds no
//! places, and an open defines no channel of the spec that its hub applied again. This
//! binary has no test harness: the count covers each thread, and a harness allocates on
//! its own thread at any time.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

#[path = "../common/agent.rs"]
#[expect(dead_code, reason = "the binary uses only some of the helpers")]
mod agent;
#[path = "../common/mod.rs"]
#[expect(dead_code, reason = "the binary uses only some of the helpers")]
mod common;
#[path = "../common/net.rs"]
mod net;
#[path = "../common/node.rs"]
mod node;
#[path = "../common/region.rs"]
mod region;
#[path = "../common/sessions.rs"]
#[expect(dead_code, reason = "the binary uses only some of the helpers")]
mod sessions;

use std::cell::Cell;
use std::path::PathBuf;
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Waker};

use agent::{Agent, SUBJECT, name, reset_with};
use block::{Heap, Pool};
use hub::reader::{self, Mode};
use hub::serve;
use sessions::{closed_after, rules, serve_each, sessions};
use transport::{Class, Code};
use types::channel;
use types::frame::key_set::Interner;
use types::name::Selector;
use types::time::{Span, Stamp};
use wire::hub::client::{self, BODY_BYTES_MAX, OPEN_BYTES_MAX, Place};
use wire::hub::{BUSY, Open, keys};

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

/// The opens over which the heap does not grow.
const MANY: usize = 1000;
/// The opens of a named reader after which the commits of its named entries wrapped
/// the ring of 1024 blocks that [`trimmed_hub`] gives. Until then the sim disk grows
/// the heap by each block.
const WRAPPED: usize = 1500;

fn main() {
    refuses_a_request_over_the_share_before_it_allocates_the_body();
    holds_the_same_bytes_over_many_opens_of_a_named_reader();
    holds_no_more_bytes_for_an_open_that_names_one_channel_many_times();
    copies_each_key_of_an_open_once_over_many_messages();
    holds_no_places_while_a_read_waits_for_its_home();
    holds_no_more_for_a_read_of_one_channel_whose_open_is_at_the_cap();
    holds_no_places_once_a_read_opens();
    defines_no_channel_again_at_an_open_of_the_spec_applied();
    defines_no_channel_again_at_the_first_poll_of_the_spec_task();
}

/// Patterns `x` that fill the open of a read of `value` to [`OPEN_BYTES_MAX`]: one
/// mode byte, `value` (2 + 5), and 3 bytes for each `x`.
const PADS: usize = (65_536 - 8) / 3;

/// The mesh names no home of `time`, so each read waits for it. A read of `value`
/// whose open is at the cap holds, while it waits, at most its open more than a read
/// of `value`, plus a tenth of the places of [`WIDE`] channels, the bound of
/// `holds_no_places_while_a_read_waits_for_its_home`.
fn holds_no_more_for_a_read_of_one_channel_whose_open_is_at_the_cap() {
    let long = "x".repeat(240);
    let places: usize = (0..WIDE)
        .map(|n| size_of::<Place>() + format!("w.{long}.{n}").len())
        .sum();
    let measured = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&measured);
    let home = |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let transport =
            net::transport(&node, &tasks, &net::own_pool(), net::HOME, 1 << 16);
        let transport = Rc::new(transport);
        let region = region::open(
            &node,
            &tasks,
            Rc::clone(&transport),
            Vec::new(),
            common::definitions(&[]),
        )
        .await;
        let hub = common::hub_in(&node, tasks.clone(), region).await;
        hub.set_rules(rules());
        let session = transport.accept().await.expect("a session");
        let link = hub.link(session.clone());
        let (served, accepted) = (Arc::default(), Rc::new(Cell::new(0)));
        let clock = node.clock();
        tasks.spawn(serve_each(
            &session, &link, &tasks, &clock, &served, &accepted,
        ));
        drop(session.closed().await);
    };
    let program = move |node: sim::node::Node, tasks, at| async move {
        let mut agent = Agent::dial(&node, tasks, at, name(SUBJECT)).await;
        agent.admit().await;
        agent.sleep(common::SETTLE).await;
        let before = ALLOCATOR.held();
        let _one = agent.read(client::Mode::Complete, &["value"]).await;
        agent.sleep(common::SETTLE).await;
        let one = ALLOCATOR.held();
        let mut select = vec!["value"];
        select.extend(std::iter::repeat_n("x", PADS));
        let _padded = agent.read(client::Mode::Complete, &select).await;
        drop(select);
        agent.sleep(common::SETTLE).await;
        let padded = ALLOCATOR.held();
        *kept.lock().expect("not poisoned") = Some((one - before, padded - one));
        agent.session.close(Code(0));
        node.clock().sleep(Span::MILLISECOND).await;
    };
    sessions::run_program(3, home, program);
    let (one, padded) = measured
        .lock()
        .expect("not poisoned")
        .take()
        .expect("the program measured");
    let open = usize::try_from(OPEN_BYTES_MAX).expect("fits");
    assert!(
        padded < one + open + places / 10,
        "a read of value holds {one} bytes, and one of value with an open at the cap \
         holds {padded}: the open is {open} bytes, and a tenth of the places of {WIDE} \
         channels is {}",
        places / 10
    );
}

/// Keys of `value` that the peer sends in each message.
const KEYS: usize = 1024;
/// Messages of keys that the peer sends.
const MESSAGES: usize = 1280;
/// The most bytes that the home may hold for the keys of one open of two channels.
const OPEN_MAX: usize = 4 << 20;

/// What [`served_open`] measured over the messages of `rest`.
struct Served {
    /// The bytes held before them.
    before: usize,
    /// The bytes held after them.
    after: usize,
    /// The bytes given out while the peer sent them, on both nodes.
    allocated: usize,
    /// The error of the first send that failed.
    failed: Option<transport::Error>,
}

/// A peer node opens a latest session of `channels` channels on a hub that defines
/// `time`, `value`, and each of `more`, sends `first`, and then each message of `rest`
/// until a send fails.
fn served_open(
    more: Vec<types::name::Name>,
    channels: u32,
    first: Vec<u8>,
    rest: impl Iterator<Item = Vec<u8>> + Send + 'static,
) -> Served {
    let measured = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&measured);
    let home = |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let hub = common::hub_alone(&node, tasks.clone(), &more).await;
        let transport =
            net::transport(&node, &tasks, &net::own_pool(), net::HOME, 1 << 16);
        let (session, incoming) = net::accept(&transport).await;
        let link = hub.link(session.clone());
        drop(link.serve(incoming).await);
        drop(session.closed().await);
    };
    let peer = move |node: sim::node::Node, tasks: env::tasks::Tasks, at| async move {
        let transport =
            net::transport(&node, &tasks, &net::own_pool(), net::PEER, 1 << 16);
        let config = block::Config::new(8 << 20).expect("the budget fits");
        let pool =
            block::Pool::new(config.clone(), block::Heap::new(config.reservation()));
        let session = transport
            .dial(net::public_key(&net::HOME), &[at])
            .await
            .expect("dials");
        let (mut sender, _receiver) = session.open(Class::Latest).await.expect("opens");
        let mut send = async |bytes: &[u8]| {
            let block = pool.copy(bytes).expect("the pool has room");
            sender.send(block).await
        };
        send(&wire::header::encode(wire::Protocol::Hub))
            .await
            .expect("sends the header");
        let open = Open {
            mode: wire::hub::Mode::Latest,
            channels,
        };
        let mut out = vec![0; open.encoded_len()];
        open.encode(&mut out);
        send(&out).await.expect("sends the open");
        send(&first).await.expect("sends keys");
        node.clock().sleep(common::SETTLE).await;
        let (before, given) = (ALLOCATOR.held(), ALLOCATOR.allocated());
        let mut failed = None;
        for message in rest {
            if let Err(error) = send(&message).await {
                failed = Some(error);
                break;
            }
        }
        node.clock().sleep(common::SETTLE).await;
        *kept.lock().expect("not poisoned") = Some(Served {
            before,
            after: ALLOCATOR.held(),
            allocated: ALLOCATOR.allocated() - given,
            failed,
        });
        session.close(Code(0));
        node.clock().sleep(Span::MILLISECOND).await;
    };
    sessions::run_program(0, home, peer);
    measured
        .lock()
        .expect("not poisoned")
        .take()
        .expect("the program measured")
}

/// A peer node opens a latest session of `u32::MAX` channels, and sends `value` up
/// to 1.3 million times. The home stops the stream with `MALFORMED` at the second
/// `value`, so it holds no more bytes after the keys than before them.
fn holds_no_more_bytes_for_an_open_that_names_one_channel_many_times() {
    let mut run = vec![0; KEYS * keys::LEN];
    keys::encode(&[channel::Key::from_u128(2); KEYS], &mut run);
    let rest = std::iter::repeat_n(run.clone(), MESSAGES - 1);
    let Served {
        before,
        after,
        failed,
        ..
    } = served_open(Vec::new(), u32::MAX, run, rest);
    let code = Code(wire::header::MALFORMED);
    let stopped = transport::Error::Stopped { code };
    assert_eq!(failed, Some(stopped), "the home stops the stream");
    assert!(
        after.saturating_sub(before) < OPEN_MAX,
        "the keys of one open of two channels raised the bytes held by {} from {before}",
        after - before,
    );
}

/// Channels whose keys an open sends one to a message.
const RUNS: usize = 1000;
/// The most bytes that the home may hold, and that the two nodes may allocate, for each
/// key of an open.
const KEY_MAX: usize = 256;

/// A peer node opens a latest session of one channel more than [`RUNS`], and sends
/// the key of each of [`RUNS`] channels in a message of its own. The open registers
/// each key once, so the home holds, and the two nodes allocate, at most [`KEY_MAX`]
/// bytes for each key. A copy of the keys at each message allocates about 8 MB.
fn copies_each_key_of_an_open_once_over_many_messages() {
    let more: Vec<_> = (0..RUNS).map(|n| name(&format!("w.{n}"))).collect();
    let run = |n: usize| {
        let mut run = vec![0; keys::LEN];
        let key = 100 + u128::try_from(n).expect("fits");
        keys::encode(&[channel::Key::from_u128(key)], &mut run);
        run
    };
    let channels = u32::try_from(RUNS + 1).expect("fits");
    let served = served_open(more, channels, run(0), (1..RUNS).map(run));
    assert_eq!(served.failed, None, "the home takes each key");
    assert!(
        served.after.saturating_sub(served.before) < RUNS * KEY_MAX,
        "{RUNS} keys of one open raised the bytes held by {} from {}",
        served.after - served.before,
        served.before,
    );
    assert!(
        served.allocated < RUNS * KEY_MAX,
        "{RUNS} keys of one open allocated {} bytes",
        served.allocated,
    );
}

/// While a request of 1 byte is open, a request of `BODY_BYTES_MAX` of the same
/// subject is over the share, and stops with `BUSY` before the hub allocates its body.
fn refuses_a_request_over_the_share_before_it_allocates_the_body() {
    let measured = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&measured);
    let open = async |node: &sim::node::Node, tasks: &env::tasks::Tasks| {
        (common::hub(node, tasks.clone()).await.0, ())
    };
    let served = sessions(
        1,
        [SUBJECT, SUBJECT],
        open,
        |[mut first, mut second]| async move {
            first.admit().await;
            second.admit().await;
            let _held = first.unfinished(1, &[]).await;
            first.sleep(Span::MILLISECOND).await;
            ALLOCATOR.reset_peak();
            let held = ALLOCATOR.held();
            let mut busy = second.unfinished(BODY_BYTES_MAX, &[]).await;
            assert_eq!(busy.recv().await, reset_with(BUSY), "the hub refuses it");
            *kept.lock().expect("not poisoned") = Some((held, ALLOCATOR.peak()));
        },
    );
    let refused = serve::Error::Share {
        subject: name(SUBJECT),
        length: BODY_BYTES_MAX,
        held: 1,
    };
    assert_eq!(
        served,
        closed_after(vec![Err(refused)], 2, 1),
        "refused by the share"
    );
    let (held, peak) = measured
        .lock()
        .expect("not poisoned")
        .take()
        .expect("the program measured");
    let body = usize::try_from(BODY_BYTES_MAX).expect("16 MiB is a usize");
    assert!(
        peak - held < body,
        "the refusal raised the bytes held by {} from {held}, a body of {body}",
        peak - held,
    );
}

/// After [`WRAPPED`] opens and drops of a named complete reader, [`MANY`] more leave
/// the heap at the same bytes. The sim runs on one thread, so the count is exact.
/// Each open and drop appends a named entry and waits for its commit.
fn holds_the_same_bytes_over_many_opens_of_a_named_reader() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let hub = trimmed_hub(&node, tasks).await;
        let config = || reader::Config {
            select: Selector::new(["value"]).expect("a selector"),
            mode: Mode::Complete,
            subject: common::name("a"),
            name: Some(common::name("r")),
            hold: Span::SECOND,
        };
        let mut wrapped = 0;
        for n in 1..=WRAPPED + MANY {
            drop(
                hub.reader(config())
                    .await
                    .unwrap_or_else(|e| panic!("open {n}: {e}")),
            );
            node.clock().sleep(common::SETTLE).await;
            if n == WRAPPED {
                wrapped = ALLOCATOR.held();
            }
        }
        assert_eq!(ALLOCATOR.held(), wrapped, "bytes held after {MANY} opens");
    })
    .expect("the run ends");
}

/// A hub as [`common::hub`] gives, on a ring whose commits trim it, so that its
/// named entries wrap it. Entries with no samples on many paths can stall a trim until
/// #2796; the named entries of this hub are on one path.
async fn trimmed_hub(node: &sim::node::Node, tasks: env::tasks::Tasks) -> hub::Hub {
    let config = block::Config::new(1 << 23).expect("the budget fits");
    let pool = Rc::new(Pool::new(config.clone(), Heap::new(config.reservation())));
    let (clock, mesh) = clock::Clock::new(node.clock());
    let wall = node.wall();
    tasks.spawn(async move { clock.run(wall).await });
    let mut interner = Interner::new();
    let config = buffer::Config {
        files: node.files(),
        dir: PathBuf::from("shard-0"),
        pool,
        clock: node.clock(),
        tasks: tasks.clone(),
        entropy: node.entropy(),
        layout: buffer::Layout::new(1 << 22, 1 << 16).expect("a ring"),
        commit: Span::from_nanos(10_000_000),
        trimmed: true,
    };
    let buffer = buffer::Buffer::open(config, interner.slots())
        .await
        .expect("opens");
    let home = home::Shard::open(home::Config {
        shard: 0,
        buffer,
        clock: mesh.clone(),
        limits: home::order::Limits {
            earliest: Stamp::from_nanos(1),
            ahead: Span::SECOND,
        },
    });
    while home.now().is_none() {
        node.clock().sleep(Span::from_nanos(1)).await;
    }
    let hub = hub::Hub::new(hub::Config {
        home,
        interner,
        tasks,
        node: types::node::Key::from_u128(1),
        time: mesh,
        entropy: node.entropy(),
        region: None,
    });
    hub.set_definitions(&common::definitions(&[]));
    hub
}

/// The channels that the wide read of
/// [`holds_no_places_while_a_read_waits_for_its_home`] matches.
const WIDE: usize = 10_000;

/// The mesh names no home of `time`, so each read waits for it. A read of [`WIDE`]
/// channels with long names holds about as many bytes while it waits as a read of one
/// channel: at most a tenth of the bytes of its places more.
fn holds_no_places_while_a_read_waits_for_its_home() {
    let long = "x".repeat(240);
    let wide: Vec<_> = (0..WIDE).map(|n| name(&format!("w.{long}.{n}"))).collect();
    let places: usize = wide
        .iter()
        .map(|name| size_of::<Place>() + name.as_str().len())
        .sum();
    let measured = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&measured);
    let home = |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let transport =
            net::transport(&node, &tasks, &net::own_pool(), net::HOME, 1 << 16);
        let transport = Rc::new(transport);
        let (other, definitions) = (Vec::new(), common::definitions(&wide));
        let region =
            region::open(&node, &tasks, Rc::clone(&transport), other, definitions)
                .await;
        let hub = common::hub_in(&node, tasks.clone(), region).await;
        hub.set_rules(rules());
        let session = transport.accept().await.expect("a session");
        let link = hub.link(session.clone());
        let (served, accepted) = (Arc::default(), Rc::new(Cell::new(0)));
        let clock = node.clock();
        tasks.spawn(serve_each(
            &session, &link, &tasks, &clock, &served, &accepted,
        ));
        drop(session.closed().await);
    };
    let program = move |node: sim::node::Node, tasks, at| async move {
        let mut agent = Agent::dial(&node, tasks, at, name(SUBJECT)).await;
        agent.admit().await;
        agent.sleep(common::SETTLE).await;
        let before = ALLOCATOR.held();
        let _one = agent.read(client::Mode::Complete, &["value"]).await;
        agent.sleep(common::SETTLE).await;
        let one = ALLOCATOR.held();
        let _wide = agent.read(client::Mode::Complete, &["w.**"]).await;
        agent.sleep(common::SETTLE).await;
        let wide = ALLOCATOR.held();
        *kept.lock().expect("not poisoned") = Some((one - before, wide - one));
        agent.session.close(Code(0));
        node.clock().sleep(Span::MILLISECOND).await;
    };
    sessions::run_program(2, home, program);
    let (one, wide) = measured
        .lock()
        .expect("not poisoned")
        .take()
        .expect("the program measured");
    assert!(
        wide < one + places / 10,
        "a read of one channel holds {one} bytes, and one of {WIDE} holds {wide}: its \
         places are {places} bytes"
    );
}

/// A read of [`WIDE`] channels with long names holds, once it opens, about as many
/// bytes as a read of [`WIDE`] channels with short names: at most a tenth of the
/// bytes by which their names differ.
fn holds_no_places_once_a_read_opens() {
    let long = "x".repeat(240);
    let longs = (0..WIDE).map(|n| name(&format!("l.{long}.{n}")));
    let shorts = (0..WIDE).map(|n| name(&format!("s.{n}")));
    let more: Vec<_> = longs.chain(shorts).collect();
    let (longs, shorts) = more.split_at(WIDE);
    let differ: usize = longs
        .iter()
        .zip(shorts)
        .map(|(long, short)| long.as_str().len() - short.as_str().len())
        .sum();
    let measured = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&measured);
    let home = |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let transport =
            net::transport(&node, &tasks, &net::own_pool(), net::HOME, 1 << 16);
        let hub = common::hub_alone(&node, tasks.clone(), &more).await;
        hub.set_rules(rules());
        let session = transport.accept().await.expect("a session");
        let link = hub.link(session.clone());
        let (served, accepted) = (Arc::default(), Rc::new(Cell::new(0)));
        let clock = node.clock();
        tasks.spawn(serve_each(
            &session, &link, &tasks, &clock, &served, &accepted,
        ));
        drop(session.closed().await);
    };
    let program = move |node: sim::node::Node, tasks, at| async move {
        let mut agent = Agent::dial(&node, tasks, at, name(SUBJECT)).await;
        agent.admit().await;
        // The first read of each set grows the pools and the state of its channels.
        let _first = opened(&agent, "l.**").await;
        let _second = opened(&agent, "s.**").await;
        agent.sleep(common::SETTLE).await;
        let before = ALLOCATOR.held();
        let _long = opened(&agent, "l.**").await;
        agent.sleep(common::SETTLE).await;
        let long = ALLOCATOR.held();
        let _short = opened(&agent, "s.**").await;
        agent.sleep(common::SETTLE).await;
        let short = ALLOCATOR.held();
        *kept.lock().expect("not poisoned") = Some((long - before, short - long));
        agent.session.close(Code(0));
        node.clock().sleep(Span::MILLISECOND).await;
    };
    sessions::run_program(4, home, program);
    let (long, short) = measured
        .lock()
        .expect("not poisoned")
        .take()
        .expect("the program measured");
    assert!(
        long.abs_diff(short) < differ / 10,
        "a read of {WIDE} channels with long names holds {long} bytes, and one with \
         short names holds {short}: their names differ by {differ} bytes"
    );
}

/// On a hub whose region uses the spec that the hub applied, the first poll of an
/// open allocates less than a pointer for each of the [`WIDE`] channels of that spec.
fn defines_no_channel_again_at_an_open_of_the_spec_applied() {
    let wide: Vec<_> = (0..WIDE).map(|n| name(&format!("w.{n}"))).collect();
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let transport =
            net::transport(&node, &tasks, &net::own_pool(), net::HOME, 1 << 16);
        let definitions = common::definitions(&wide);
        let region =
            region::open(&node, &tasks, Rc::new(transport), Vec::new(), definitions)
                .await;
        let hub = common::hub_in(&node, tasks, region).await;
        let mut open = pin!(hub.reader(reader::Config {
            select: Selector::new(["value"]).expect("a selector"),
            mode: Mode::Complete,
            subject: common::name("a"),
            name: Some(common::name("r")),
            hold: Span::SECOND,
        }));
        ALLOCATOR.reset_peak();
        let before = ALLOCATOR.held();
        drop(open.as_mut().poll(&mut Context::from_waker(Waker::noop())));
        let peak = ALLOCATOR.peak() - before;
        let pointers = WIDE * size_of::<usize>();
        assert!(
            peak < pointers,
            "the first poll of an open allocates {peak} bytes over those held, and a \
             pointer for each of {WIDE} channels is {pointers} bytes"
        );
    })
    .expect("the run ends");
}

/// The spec task of a new hub defines no channel of the spec that `Hub::new` applied.
fn defines_no_channel_again_at_the_first_poll_of_the_spec_task() {
    let wide: Vec<_> = (0..WIDE).map(|n| name(&format!("w.{n}"))).collect();
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let transport =
            net::transport(&node, &tasks, &net::own_pool(), net::HOME, 1 << 16);
        let definitions = common::definitions(&wide);
        let region =
            region::open(&node, &tasks, Rc::new(transport), Vec::new(), definitions)
                .await;
        node.clock().sleep(Span::MILLISECOND).await;
        let hub = common::hub_in(&node, tasks, region).await;
        ALLOCATOR.reset_peak();
        let before = ALLOCATOR.held();
        node.clock().sleep(Span::MILLISECOND).await;
        let peak = ALLOCATOR.peak() - before;
        let pointers = WIDE * size_of::<usize>();
        assert!(
            peak < pointers,
            "the first poll of the spec task allocates {peak} bytes over those held, \
             and a pointer for each of {WIDE} channels is {pointers} bytes"
        );
        drop(hub);
    })
    .expect("the run ends");
}

/// A complete read of `select` by `agent`, after the program got its places, their
/// run, and `Opened`. The program holds nothing of the places.
async fn opened(agent: &Agent, select: &str) -> agent::Stream {
    let mut stream = agent.read(client::Mode::Complete, &[select]).await;
    stream.opened().await;
    stream
}
