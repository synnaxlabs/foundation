//! A request that the hub refuses for its length allocates no body, and the heap of a
//! hub does not grow over many opens and closes of a named complete reader. This
//! binary has no test harness: the count covers each thread, and a harness allocates
//! on its own thread at any time.

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
#[path = "../common/sessions.rs"]
#[expect(dead_code, reason = "the binary uses only some of the helpers")]
mod sessions;

use std::sync::{Arc, Mutex};

use agent::{SUBJECT, name, reset_with};
use hub::reader::{self, Mode};
use hub::serve;
use sessions::{closed_after, sessions};
use transport::{Class, Code};
use types::channel;
use types::name::Selector;
use types::time::Span;
use wire::hub::client::BODY_BYTES_MAX;
use wire::hub::{BUSY, Open, keys};

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

/// The opens after which the heap is read the first time.
const FEW: usize = 10;
/// The opens after which the heap is read the last time.
const MANY: usize = 1000;

fn main() {
    refuses_a_request_over_the_share_before_it_allocates_the_body();
    holds_the_same_bytes_over_many_opens_of_a_named_reader();
    holds_no_more_bytes_for_an_open_that_names_one_channel_many_times();
}

/// Keys of `value` that the peer sends in each message.
const KEYS: usize = 1024;
/// Messages of keys that the peer sends.
const MESSAGES: usize = 1280;
/// The most bytes that the home may hold for the keys of one open of two channels.
const OPEN_MAX: usize = 4 << 20;

/// A peer node opens a latest session of `u32::MAX` channels, and sends `value` up
/// to 1.3 million times. The home stops the stream with `MALFORMED` at the second
/// `value`, so it holds no more bytes after the keys than before them.
fn holds_no_more_bytes_for_an_open_that_names_one_channel_many_times() {
    let measured = Arc::new(Mutex::new(None));
    let kept = Arc::clone(&measured);
    let home = |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let (hub, _) = common::hub(&node, tasks.clone()).await;
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
            let mut block = pool.alloc(bytes.len()).expect("the pool has room");
            block.copy_from_slice(bytes);
            sender.send(block.freeze()).await
        };
        send(&wire::header::encode(wire::Protocol::Hub))
            .await
            .expect("sends the header");
        let open = Open {
            mode: wire::hub::Mode::Latest,
            channels: u32::MAX,
        };
        let mut out = vec![0; open.encoded_len()];
        open.encode(&mut out);
        send(&out).await.expect("sends the open");
        let mut run = vec![0; KEYS * keys::LEN];
        keys::encode(&[channel::Key::from_u128(2); KEYS], &mut run);
        send(&run).await.expect("sends keys");
        node.clock().sleep(common::SETTLE).await;
        let before = ALLOCATOR.held();
        let mut failed = None;
        for _ in 1..MESSAGES {
            if let Err(error) = send(&run).await {
                failed = Some(error);
                break;
            }
        }
        let code = Code(wire::header::MALFORMED);
        let stopped = transport::Error::Stopped { code };
        assert_eq!(failed, Some(stopped), "the home stops the stream");
        node.clock().sleep(common::SETTLE).await;
        *kept.lock().expect("not poisoned") = Some((before, ALLOCATOR.held()));
        session.close(Code(0));
        node.clock().sleep(Span::MILLISECOND).await;
    };
    sessions::run_program(0, home, peer);
    let (before, after) = measured
        .lock()
        .expect("not poisoned")
        .take()
        .expect("the program measured");
    assert!(
        after.saturating_sub(before) < OPEN_MAX,
        "the keys of one open of two channels raised the bytes held by {} from {before}",
        after - before,
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

/// After [`FEW`] opens and drops of a named complete reader, [`MANY`] leave the heap
/// at the same bytes. The sim runs on one thread, so the count is exact.
fn holds_the_same_bytes_over_many_opens_of_a_named_reader() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    sim.run_on(&node, |node, tasks| async move {
        let (hub, _) = common::hub(&node, tasks).await;
        let config = || reader::Config {
            select: Selector::new(["value"]).expect("a selector"),
            mode: Mode::Complete,
            subject: common::name("a"),
            name: Some(common::name("r")),
            hold: Span::SECOND,
        };
        let mut few = 0;
        for n in 1..=MANY {
            drop(hub.reader(config()).await.expect("opens"));
            if n == FEW {
                few = ALLOCATOR.held();
            }
        }
        assert_eq!(ALLOCATOR.held(), few, "bytes held after {MANY} opens");
    })
    .expect("the run ends");
}
