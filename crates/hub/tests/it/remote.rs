//! Reader sessions at a node whose region names another node as the home of the index.

use std::future::poll_fn;
use std::net::SocketAddr;
use std::pin::pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::Poll;

use env::clock::Clock;
use env::tasks::Tasks;
use hub::Hub;
use hub::reader::{self, Ended, Mode};
use spec::data_type::DataType;
use transport::stream::{Receiver, Sender};
use transport::{Address, Code, Transport};
use types::frame::{self, Path, Range};
use types::time::Span;
use wire::Protocol;
use wire::header::MALFORMED;
use wire::hub::{Credit, Head, Reply, UNKNOWN, ends};

use super::region::{OTHER, TIME};
use super::serve::{HOME, PEER, PORT, own_pool, transport};

/// The fewest bytes that a transport takes in one message.
const MESSAGE_MIN: usize = 1472;
use super::{
    AREA, BODY_MAX, I64, POOL, Test, fill, name, samples, spec_channel, write,
    write_series, write_wide,
};

/// What the two nodes of [`remote`] wait on.
#[derive(Default)]
struct Steps {
    /// The reader opened its session.
    opened: AtomicBool,
    /// The home saw the code that the reader stopped the stream with.
    stopped: AtomicBool,
    /// The reader's node is done.
    done: AtomicBool,
    /// The sessions that the home's node accepted.
    sessions: AtomicUsize,
}

impl Steps {
    fn open(&self) {
        self.opened.store(true, Ordering::Relaxed);
    }
}

/// Waits on `clock` until `flag` is set.
async fn until(clock: &Clock, flag: &AtomicBool) {
    while !flag.load(Ordering::Relaxed) {
        clock.sleep(Span::MILLISECOND).await;
    }
}

/// Runs `reader` on a [`Test`] hub at [`NODE`], in the region of
/// [`super::region::open`], once the mesh names [`OTHER`] as the home of `time`.
/// `home` runs at the node of [`OTHER`] on its transport, which proves [`PEER`]. Each
/// way between the nodes takes `link`. The run ends once both return.
fn remote<H, R>(
    seed: u64,
    link: sim::link::Config,
    home: impl FnOnce(sim::node::Node, Tasks, Transport, Arc<Steps>) -> H + Send + 'static,
    reader: impl FnOnce(Test, Arc<Steps>) -> R + Send + 'static,
) where
    H: Future<Output = ()> + 'static,
    R: Future<Output = ()> + 'static,
{
    remote_sized(seed, link, [1 << 16; 2], home, reader);
}

/// As [`remote`], where the transport of the reader's node takes messages of at most
/// `messages[0]` bytes, and that of the home's node at most `messages[1]`.
fn remote_sized<H, R>(
    seed: u64,
    link: sim::link::Config,
    messages: [usize; 2],
    home: impl FnOnce(sim::node::Node, Tasks, Transport, Arc<Steps>) -> H + Send + 'static,
    reader: impl FnOnce(Test, Arc<Steps>) -> R + Send + 'static,
) where
    H: Future<Output = ()> + 'static,
    R: Future<Output = ()> + 'static,
{
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let nodes = [1, 2].map(|_| sim.node(sim::node::Config::default()));
    sim.link(&nodes[0], &nodes[1], link);
    sim.link(&nodes[1], &nodes[0], link);
    let at = Address::Udp(SocketAddr::new(nodes[1].addresses()[0], PORT));
    let steps = Arc::new(Steps::default());
    let shard = |name: &str| env::shards::Config {
        name: name.into(),
        core: None,
    };
    let (node, kept) = (nodes[0].clone(), Arc::clone(&steps));
    let main = move |tasks: Tasks| async move {
        let transport = transport(&node, &tasks, &own_pool(), HOME, messages[0]);
        let region =
            super::region::open(&node, &tasks, Rc::new(transport), vec![at]).await;
        let layout = buffer::Layout::new(AREA, BODY_MAX).expect("a ring");
        let mut test = Test::new(node.clone(), tasks, layout, POOL, Some(region)).await;
        test.sync().await;
        test.set_home(TIME, OTHER).await;
        reader(test, Arc::clone(&kept)).await;
        kept.done.store(true, Ordering::Relaxed);
        node.clock().sleep(Span::MILLISECOND).await;
    };
    drop(
        nodes[0]
            .shards()
            .start(shard("reader"), main)
            .expect("starts"),
    );
    let node = nodes[1].clone();
    let main = move |tasks: Tasks| async move {
        let transport = transport(&node, &tasks, &own_pool(), PEER, messages[1]);
        home(node, tasks, transport, steps).await;
    };
    drop(
        nodes[1]
            .shards()
            .start(shard("home"), main)
            .expect("starts"),
    );
    sim.run().expect("the run ends");
}

/// A home at the node of `transport`: a [`Test`] hub with no region, which serves each
/// hub stream of each session. Runs `write` on it, then waits until the reader's node
/// is done.
async fn hub_home<W>(
    node: sim::node::Node,
    tasks: Tasks,
    transport: Transport,
    steps: Arc<Steps>,
    write: impl FnOnce(Rc<Test>) -> W,
) where
    W: Future<Output = ()>,
{
    let layout = buffer::Layout::new(AREA, BODY_MAX).expect("a ring");
    let mut test = Test::new(node, tasks.clone(), layout, POOL, None).await;
    test.sync().await;
    let test = Rc::new(test);
    tasks.spawn(serve_each(
        test.hub.clone(),
        transport,
        tasks.clone(),
        Arc::clone(&steps),
    ));
    write(Rc::clone(&test)).await;
    until(&test.clock, &steps.done).await;
}

/// Serves each hub stream of each session of `transport` on `hub`.
async fn serve_each(hub: Hub, transport: Transport, tasks: Tasks, steps: Arc<Steps>) {
    while let Ok(session) = transport.accept().await {
        steps.sessions.fetch_add(1, Ordering::Relaxed);
        let link = hub.link(session.clone());
        let streams = tasks.clone();
        tasks.spawn(async move {
            while let Ok(mut incoming) = session.accept().await {
                let header = incoming.receiver.recv().await.expect("a header");
                let header = header.expect("the header comes before the finish");
                assert_eq!(wire::header::decode(&header), Ok((Protocol::Hub, &[][..])));
                let serving = link.serve(incoming);
                streams.spawn(async move { drop(serving.await) });
            }
        });
    }
}

/// Waits until the reader opened, then writes three frames of `time` and `value`.
async fn write_three(test: Rc<Test>, steps: Arc<Steps>) {
    let mut writer = test.writer("w", &["time", "value"]).await;
    until(&test.clock, &steps.opened).await;
    write(&mut writer, &[10, 11], &[1, 2]);
    write(&mut writer, &[12], &[3]);
    write(&mut writer, &[13, 14, 15], &[4, 5, 6]);
}

/// Takes three frames from a complete reader of `value` at the other node, and checks
/// the samples of [`write_three`].
async fn read_three(test: Test, steps: Arc<Steps>) {
    let mut reader = test.reader(&["value"], Mode::Complete).await;
    steps.open();
    let mut got = Vec::new();
    for _ in 0..3 {
        let received = reader.next().await.expect("a frame");
        got.push((samples(&received, 1), samples(&received, 2)));
    }
    let expected = vec![
        (vec![10, 11], vec![1, 2]),
        (vec![12], vec![3]),
        (vec![13, 14, 15], vec![4, 5, 6]),
    ];
    assert_eq!(got, expected);
}

#[test]
fn a_reader_gets_each_frame_that_the_home_of_another_node_wrote_in_order() {
    remote(
        1,
        sim::link::Config::default(),
        |node, tasks, transport, steps| async move {
            let kept = Arc::clone(&steps);
            hub_home(node, tasks, transport, steps, |test| {
                write_three(test, kept)
            })
            .await;
        },
        read_three,
    );
}

#[test]
fn a_reader_gets_each_frame_in_order_over_a_link_that_loses_reorders_and_duplicates() {
    let link = sim::link::Config {
        jitter: Span::from_nanos(2 * Span::MILLISECOND.nanos()),
        loss: 0.1,
        duplication: 0.1,
        ..sim::link::Config::default()
    };
    remote(
        2,
        link,
        |node, tasks, transport, steps| async move {
            let kept = Arc::clone(&steps);
            hub_home(node, tasks, transport, steps, |test| {
                write_three(test, kept)
            })
            .await;
        },
        read_three,
    );
}

#[test]
fn a_reader_that_names_its_index_and_a_channel_twice_gets_each_data_channel() {
    remote(
        8,
        sim::link::Config::default(),
        |node, tasks, transport, steps| async move {
            let kept = Arc::clone(&steps);
            hub_home(node, tasks, transport, steps, |test| async move {
                let channels = ["time", "value", "value-c"];
                let mut writer = test.writer("w", &channels).await;
                until(&test.clock, &kept.opened).await;
                write_series(
                    &mut writer,
                    &[(1, &[10, 11]), (2, &[1, 2]), (5, &[3, 4])],
                );
            })
            .await;
        },
        |test, steps| async move {
            let channels = ["value-c", "time", "value", "value-c"];
            let mut reader = test.reader(&channels, Mode::Complete).await;
            steps.open();
            let received = reader.next().await.expect("a frame");
            let got = [1, 2, 5].map(|key| samples(&received, key));
            assert_eq!(got, [vec![10, 11], vec![1, 2], vec![3, 4]]);
        },
    );
}

#[test]
fn a_reader_of_a_channel_that_the_home_does_not_know_is_refused_with_unknown() {
    remote(
        3,
        sim::link::Config::default(),
        |node, tasks, transport, steps| async move {
            hub_home(node, tasks, transport, steps, |_| async {}).await;
        },
        |test, _| async move {
            let extra = (name("extra"), spec_channel(9, DataType::Sample(I64), 1));
            test.hub.define([(&extra.0, &extra.1)]);
            let names = [name("extra")];
            let error = test
                .hub
                .reader(&names, Mode::Latest)
                .await
                .expect_err("the home refuses");
            assert_eq!(error, reader::Error::Refused(Code(UNKNOWN)));
            assert_eq!(
                error.to_string(),
                "the home refused the reader with code 16: the home does not know a \
                 channel of the reader"
            );
        },
    );
}

#[test]
fn a_complete_reader_gets_each_frame_of_a_commit_past_the_window_as_it_gives_frames_back()
 {
    const FRAMES: i64 = 200;
    remote(
        4,
        sim::link::Config::default(),
        |node, tasks, transport, steps| async move {
            let kept = Arc::clone(&steps);
            hub_home(node, tasks, transport, steps, |test| async move {
                let mut writer = test.writer("w", &["time", "value"]).await;
                until(&test.clock, &kept.opened).await;
                let now = test.now();
                for n in 0..FRAMES {
                    write_wide(&mut writer, now, n);
                }
            })
            .await;
        },
        |test, steps| async move {
            let mut reader = test.reader(&["value"], Mode::Complete).await;
            steps.open();
            test.clock.sleep(Span::from_nanos(200_000_000)).await;
            let mut stamps = Vec::new();
            for _ in 0..FRAMES {
                let received = reader.next().await.expect("a frame");
                stamps.extend(samples(&received, 1));
            }
            let first = stamps[0];
            assert_eq!(stamps, (first..first + FRAMES * 1000).collect::<Vec<_>>());
        },
    );
}

#[test]
fn two_readers_at_one_home_share_one_session() {
    remote(
        5,
        sim::link::Config::default(),
        |node, tasks, transport, steps| async move {
            let kept = Arc::clone(&steps);
            hub_home(node, tasks, transport, steps, |test| async move {
                let mut writer = test.writer("w", &["time", "value"]).await;
                until(&test.clock, &kept.opened).await;
                write(&mut writer, &[10], &[1]);
            })
            .await;
        },
        |test, steps| async move {
            let mut first = test.reader(&["value"], Mode::Complete).await;
            let mut second = test.reader(&["value-c"], Mode::Complete).await;
            steps.open();
            let received = first.next().await.expect("a frame");
            assert_eq!(samples(&received, 2), [1]);
            let received = second.next().await.expect("a frame");
            assert_eq!(samples(&received, 1), [10]);
            assert_eq!(steps.sessions.load(Ordering::Relaxed), 1);
        },
    );
}

/// Polls `a` and `b` until both are done.
async fn both<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (pin!(a), pin!(b));
    let (mut got_a, mut got_b) = (None, None);
    poll_fn(|cx| {
        if got_a.is_none()
            && let Poll::Ready(output) = a.as_mut().poll(cx)
        {
            got_a = Some(output);
        }
        if got_b.is_none()
            && let Poll::Ready(output) = b.as_mut().poll(cx)
        {
            got_b = Some(output);
        }
        if got_a.is_some() && got_b.is_some() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    (got_a.expect("done"), got_b.expect("done"))
}

#[test]
fn two_readers_that_open_at_once_at_one_home_each_get_its_frames() {
    remote(
        8,
        sim::link::Config::default(),
        |node, tasks, transport, steps| async move {
            let kept = Arc::clone(&steps);
            hub_home(node, tasks, transport, steps, |test| {
                write_three(test, kept)
            })
            .await;
        },
        |test, steps| async move {
            let (first, second) = both(
                test.reader(&["value"], Mode::Complete),
                test.reader(&["value-c"], Mode::Complete),
            )
            .await;
            steps.open();
            for mut reader in [first, second] {
                let mut stamps = Vec::new();
                for _ in 0..3 {
                    stamps.extend(samples(&reader.next().await.expect("a frame"), 1));
                }
                assert_eq!(stamps, [10, 11, 12, 13, 14, 15]);
            }
        },
    );
}

#[test]
fn a_reader_whose_pool_has_no_room_for_its_open_gets_pool() {
    remote(
        6,
        sim::link::Config::default(),
        |node, tasks, transport, steps| async move {
            hub_home(node, tasks, transport, steps, |_| async {}).await;
        },
        |test, _| async move {
            let blocks = fill(&test.pool);
            let names = [name("value")];
            let error = test
                .hub
                .reader(&names, Mode::Latest)
                .await
                .expect_err("the pool has no room");
            drop(blocks);
            let reader::Error::Pool(pool) = &error else {
                panic!("not a pool error: {error:?}");
            };
            assert_eq!(
                error.to_string(),
                format!("the pool had no block for the open: {pool}")
            );
        },
    );
}

/// A home's end of the first hub stream that `transport` accepts, once it took the
/// header, the open, and one message of keys, and sent `Opened`.
async fn fake_open(transport: &Transport) -> (Sender, Receiver) {
    let session = transport.accept().await.expect("a session");
    let mut incoming = session.accept().await.expect("a stream");
    let mut receiver = incoming.receiver;
    let mut sender = incoming.sender.take().expect("a two-way stream");
    for _ in 0..3 {
        receiver
            .recv()
            .await
            .expect("a message")
            .expect("not finished");
    }
    send(&mut sender, 1, |out| Reply::Opened.encode(out)).await;
    (sender, receiver)
}

/// Sends a message of `len` bytes that `fill` writes.
async fn send(sender: &mut Sender, len: usize, fill: impl FnOnce(&mut [u8])) {
    let mut block = own_pool().alloc(len).expect("the pool has room");
    fill(&mut block);
    sender.send(block.freeze()).await.expect("sends");
}

/// Sends the head of a frame of `count` samples and `ends`.
async fn send_head(sender: &mut Sender, count: u32, ends: &[(u32, u32)]) {
    let head = Reply::Head(Head {
        path: Path::Live,
        range: Range { seq: 0, count },
        series: u32::try_from(ends.len()).expect("a few ends"),
    });
    send(sender, head.encoded_len(), |out| head.encode(out)).await;
    send(sender, ends.len() * ends::LEN, |out| {
        ends::encode(ends.iter().copied(), out);
    })
    .await;
}

/// A home that opens the session of the first hub stream, sends one frame head,
/// `ends`, and a body message of each of `bodies` bytes, then gives the code that
/// stopped the stream.
async fn fake_home(
    transport: &Transport,
    ends: &[(u32, u32)],
    bodies: &[usize],
) -> transport::Error {
    let (mut sender, mut receiver) = fake_open(transport).await;
    send_head(&mut sender, 1, ends).await;
    for &len in bodies {
        send(&mut sender, len, |out| out.fill(1)).await;
    }
    loop {
        if let Err(error) = receiver.recv().await {
            return error;
        }
    }
}

#[test]
fn a_reader_stops_the_stream_as_malformed_when_an_end_names_a_place_out_of_range() {
    remote(
        7,
        sim::link::Config::default(),
        |node, _, transport, steps| async move {
            let error = fake_home(&transport, &[(0, 8), (5, 16)], &[]).await;
            assert_eq!(
                error,
                transport::Error::Reset {
                    code: Code(MALFORMED)
                }
            );
            steps.stopped.store(true, Ordering::Relaxed);
            until(&node.clock(), &steps.done).await;
        },
        |test, steps| async move {
            let mut reader = test.reader(&["value"], Mode::Latest).await;
            let ended = reader.next().await.expect_err("the frame is not valid");
            let expected = frame::Error::OutOfRange {
                entry: 5,
                entries: 2,
            };
            assert_eq!(ended, Ended::Frame(expected));
            assert_eq!(
                ended.to_string(),
                "a frame from the home is not valid: entry 5 is out of range for a key \
                 set of 2 entries"
            );
            let again = reader.next().await.expect_err("ended");
            assert_eq!(again, ended);
            until(&test.clock, &steps.stopped).await;
        },
    );
}

#[test]
fn a_reader_stops_the_stream_as_malformed_when_a_body_message_is_longer_than_the_body()
{
    remote(
        7,
        sim::link::Config::default(),
        |node, _, transport, steps| async move {
            let error = fake_home(&transport, &[(0, 8), (1, 16)], &[17]).await;
            assert_eq!(
                error,
                transport::Error::Reset {
                    code: Code(MALFORMED)
                }
            );
            steps.stopped.store(true, Ordering::Relaxed);
            until(&node.clock(), &steps.done).await;
        },
        |test, steps| async move {
            let mut reader = test.reader(&["value"], Mode::Latest).await;
            let ended = reader.next().await.expect_err("the body is too long");
            let expected = wire::hub::Error::Body {
                len: 17,
                remain: 16,
            };
            assert_eq!(ended, Ended::Message(expected));
            assert_eq!(
                ended.to_string(),
                "a message from the home broke the hub protocol: the body message has 17 \
                 bytes, and 16 remain in the body"
            );
            until(&test.clock, &steps.stopped).await;
        },
    );
}

/// Defines 100 data channels on `time` at `test`'s hub, and gives their names.
fn define_many(test: &Test) -> Vec<types::name::Name> {
    let channels: Vec<_> = (100..200)
        .map(|key| {
            let channel = spec_channel(key, DataType::Sample(I64), 1);
            (name(&format!("extra-{key}")), channel)
        })
        .collect();
    test.hub
        .define(channels.iter().map(|(name, channel)| (name, channel)));
    channels.into_iter().map(|(name, _)| name).collect()
}

#[test]
fn a_reader_whose_keys_and_frames_each_take_many_messages_gets_each_frame() {
    const FRAMES: i64 = 3;
    remote_sized(
        9,
        sim::link::Config::default(),
        [MESSAGE_MIN; 2],
        |node, tasks, transport, steps| async move {
            let kept = Arc::clone(&steps);
            hub_home(node, tasks, transport, steps, |test| async move {
                define_many(&test);
                let mut writer = test.writer("w", &["time", "value"]).await;
                until(&test.clock, &kept.opened).await;
                let now = test.now();
                for n in 0..FRAMES {
                    write_wide(&mut writer, now, n);
                }
            })
            .await;
        },
        |test, steps| async move {
            let mut names = define_many(&test);
            names.push(name("value"));
            assert!(
                (names.len() + 1) * 16 > MESSAGE_MIN,
                "the keys take two messages"
            );
            let mut reader = test
                .hub
                .reader(&names, Mode::Complete)
                .await
                .expect("opens");
            steps.open();
            let mut stamps = Vec::new();
            for _ in 0..FRAMES {
                let received = reader.next().await.expect("a frame");
                stamps.extend(samples(&received, 1));
            }
            let first = stamps[0];
            assert_eq!(stamps, (first..first + FRAMES * 1000).collect::<Vec<_>>());
        },
    );
}

#[test]
fn a_complete_reader_sends_a_credit_each_time_it_gave_back_half_a_window() {
    const WINDOW: u64 = 1 << 20;
    const FRAMES: usize = 100;
    // 31 frames of the first body and one of the second charge exactly half a window.
    let body = |n: usize| if n == 31 { 14_272 } else { 16_320 };
    let charges: Vec<_> = (0..FRAMES).map(|n| frame::charge(2, body(n))).collect();
    let (mut granted, mut taken, mut expected) = (WINDOW, 0, Vec::new());
    for charge in &charges[..FRAMES - 1] {
        taken += charge;
        if taken + WINDOW - granted >= WINDOW / 2 {
            granted = taken + WINDOW;
            expected.push(granted);
        }
    }
    assert_eq!(
        expected[0],
        WINDOW / 2 + WINDOW,
        "a credit falls due at a frame"
    );
    let credits = expected.len();
    remote(
        10,
        sim::link::Config::default(),
        move |node, _, transport, steps| async move {
            let (mut sender, mut receiver) = fake_open(&transport).await;
            for n in 0..FRAMES {
                let half = u32::try_from(body(n) / 2).expect("a short body");
                send_head(&mut sender, half / 8, &[(0, half), (1, 2 * half)]).await;
                send(&mut sender, body(n), |out| out.fill(1)).await;
            }
            let mut got = Vec::new();
            while got.len() < credits {
                let message = receiver.recv().await.expect("a credit").expect("open");
                assert_eq!(message.len(), Credit::LEN);
                let limit = message[1..].try_into().expect("a credit");
                got.push(u64::from_le_bytes(limit));
            }
            assert_eq!(got, expected);
            steps.stopped.store(true, Ordering::Relaxed);
            until(&node.clock(), &steps.done).await;
        },
        |test, steps| async move {
            let mut reader = test.reader(&["value"], Mode::Complete).await;
            for _ in 0..FRAMES {
                reader.next().await.expect("a frame");
            }
            until(&test.clock, &steps.stopped).await;
        },
    );
}

#[test]
fn a_reader_whose_stream_the_home_resets_with_a_code_outside_hub_wire_gets_transport() {
    remote(
        11,
        sim::link::Config::default(),
        |node, _, transport, steps| async move {
            let session = transport.accept().await.expect("a session");
            let mut incoming = session.accept().await.expect("a stream");
            incoming.receiver.recv().await.expect("a header");
            let sender = incoming.sender.take().expect("a two-way stream");
            sender.reset(Code(7));
            until(&node.clock(), &steps.done).await;
        },
        |test, _| async move {
            let names = [name("value")];
            let error = test
                .hub
                .reader(&names, Mode::Latest)
                .await
                .expect_err("the home reset the stream");
            let reset = transport::Error::Reset { code: Code(7) };
            assert_eq!(error, reader::Error::Transport(reset));
            assert_eq!(
                error.to_string(),
                "the transport to the home failed: the peer reset the stream (7)"
            );
        },
    );
}
