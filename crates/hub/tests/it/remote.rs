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
use wire::hub::{Credit, Head, Refusal, Reply, ends};

use super::region::{OTHER, TIME};
use super::serve::{HOME, PEER, PORT, own_pool, transport_sized};
use super::{
    AREA, BODY_MAX, I64, POOL, Test, definition, fill, name, samples, write,
    write_series, write_wide,
};

/// The fewest bytes that a transport takes in one message.
const MESSAGE_MIN: usize = 1472;
/// The window of a transport of a test, in bytes.
const WINDOW: usize = 1 << 20;

/// What the two nodes of [`remote`] wait on.
#[derive(Default)]
struct Steps {
    /// The reader opened its session.
    opened: AtomicBool,
    /// The home saw the code that the reader stopped the stream with.
    stopped: AtomicBool,
    /// The reader's pool has no room.
    full: AtomicBool,
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
    remote_sized(seed, link, [(1 << 16, WINDOW); 2], home, reader);
}

/// As [`remote`], where the transport of the reader's node takes messages of at most
/// `sizes[0].0` bytes and has a window of `sizes[0].1` bytes, and that of the home's
/// node `sizes[1]`.
fn remote_sized<H, R>(
    seed: u64,
    link: sim::link::Config,
    sizes: [(usize, usize); 2],
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
        let transport = transport_sized(&node, &tasks, &own_pool(), HOME, sizes[0]);
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
        let transport = transport_sized(&node, &tasks, &own_pool(), PEER, sizes[1]);
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
            let extra = (name("extra"), definition(9, DataType::Sample(I64), 1));
            test.hub.define([(&extra.0, &extra.1)]);
            let names = [name("extra")];
            let error = test
                .hub
                .reader(&names, Mode::Latest)
                .await
                .expect_err("the home refuses");
            assert_eq!(error, reader::Error::Refused(Refusal::Unknown));
            assert_eq!(
                error.to_string(),
                "the home refused the reader with code 16: the home does not know a \
                 channel of the open"
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

/// The streams that the home's transport allows the reader's node at once.
const STREAMS: usize = 16;

#[test]
fn a_reader_past_the_streams_of_its_home_waits_also_after_another_reader_drops() {
    remote(
        22,
        sim::link::Config::default(),
        |node, tasks, transport, steps| async move {
            hub_home(node, tasks, transport, steps, |_| async {}).await;
        },
        |test, _| async move {
            let mut readers = Vec::new();
            for _ in 0..STREAMS {
                readers.push(test.reader(&["value"], Mode::Complete).await);
            }
            let mut opening = pin!(test.reader(&["value"], Mode::Complete));
            let wait = test.clock.sleep(Span::from_nanos(1_000_000_000));
            assert!(
                race(opening.as_mut(), wait).await.is_err(),
                "no stream is free"
            );
            drop(readers.pop());
            // A transport defect (#2018): a session gives back stream slots only
            // in batches, so one drop frees none. Once #2029 merges, the reader
            // opens here.
            let wait = test.clock.sleep(Span::from_nanos(2_000_000_000));
            assert!(
                race(opening, wait).await.is_err(),
                "the session gives back no stream"
            );
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
            let header = wire::header::encode(Protocol::Hub).len();
            let expected = test.pool.alloc(header).expect_err("the pool is full");
            drop(blocks);
            assert_eq!(error, reader::Error::Pool(expected.clone()));
            assert_eq!(
                error.to_string(),
                format!("the pool had no block for the open: {expected}")
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

/// Defines `count` data channels on `time` at `test`'s hub, and gives their names.
fn define_many(test: &Test, count: u128) -> Vec<types::name::Name> {
    let channels: Vec<_> = (100..100 + count)
        .map(|key| {
            let channel = definition(key, DataType::Sample(I64), 1);
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
        [(MESSAGE_MIN, WINDOW); 2],
        |node, tasks, transport, steps| async move {
            let kept = Arc::clone(&steps);
            hub_home(node, tasks, transport, steps, |test| async move {
                define_many(&test, 100);
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
            let mut names = define_many(&test, 100);
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

#[test]
fn a_remote_reader_yields_once_after_a_streak_of_frames() {
    const FRAMES: i64 = 300;
    remote(
        25,
        sim::link::Config::default(),
        |node, tasks, transport, steps| async move {
            let kept = Arc::clone(&steps);
            hub_home(node, tasks, transport, steps, |test| async move {
                let mut writer = test.writer("w", &["time", "value"]).await;
                until(&test.clock, &kept.opened).await;
                for n in 0..FRAMES {
                    write(&mut writer, &[10 + n], &[n]);
                }
            })
            .await;
        },
        |test, steps| async move {
            let mut reader = test.reader(&["value"], Mode::Complete).await;
            steps.open();
            test.clock.sleep(Span::from_nanos(500_000_000)).await;
            let mut given = 0;
            while given < FRAMES {
                match super::poll_once(reader.next()) {
                    Poll::Ready(received) => {
                        received.expect("a frame");
                        given += 1;
                    }
                    Poll::Pending => break,
                }
            }
            assert_eq!(given, 128, "the reader yields after 128 frames in a row");
        },
    );
}

#[test]
fn a_remote_reader_lets_other_tasks_run_while_frames_wait() {
    const FRAMES: i64 = 1000;
    remote(
        26,
        sim::link::Config::default(),
        |node, tasks, transport, steps| async move {
            let kept = Arc::clone(&steps);
            hub_home(node, tasks, transport, steps, |test| async move {
                let mut writer = test.writer("w", &["time", "value"]).await;
                until(&test.clock, &kept.opened).await;
                for n in 0..FRAMES {
                    write(&mut writer, &[10 + n], &[n]);
                }
            })
            .await;
        },
        |test, steps| async move {
            let mut reader = test.reader(&["value"], Mode::Complete).await;
            steps.open();
            test.clock.sleep(Span::from_nanos(500_000_000)).await;
            reader.next().await.expect("a frame");
            let taken = Rc::new(std::cell::Cell::new(1));
            let other = Rc::new(std::cell::Cell::new(None));
            let (seen, ran) = (Rc::clone(&taken), Rc::clone(&other));
            test.tasks.spawn(async move { ran.set(Some(seen.get())) });
            for n in 1..FRAMES {
                reader.next().await.expect("a frame");
                taken.set(n + 1);
            }
            let other = other.get().expect("the other task ran");
            assert!(other < FRAMES, "the other task ran after {other} frames");
        },
    );
}

#[test]
fn a_reader_after_the_home_closed_the_held_session_dials_again() {
    remote(
        13,
        sim::link::Config::default(),
        |node, tasks, transport, steps| async move {
            let session = transport.accept().await.expect("a session");
            session.accept().await.expect("a stream");
            session.close(Code(0));
            let kept = Arc::clone(&steps);
            hub_home(node, tasks, transport, steps, |test| {
                write_three(test, kept)
            })
            .await;
        },
        |test, steps| async move {
            let names = [name("value")];
            let error = test
                .hub
                .reader(&names, Mode::Latest)
                .await
                .expect_err("the home closed the session");
            let closed = transport::Error::PeerClosed { code: Code(0) };
            assert_eq!(error, reader::Error::Transport(closed));
            read_three(test, steps).await;
        },
    );
}

/// `Ok` with the output of `a` when it is done first, else `Err` with that of `b`.
async fn race<A: Future, B: Future>(a: A, b: B) -> Result<A::Output, B::Output> {
    let (mut a, mut b) = (pin!(a), pin!(b));
    poll_fn(|cx| {
        if let Poll::Ready(output) = a.as_mut().poll(cx) {
            return Poll::Ready(Ok(output));
        }
        b.as_mut().poll(cx).map(Err)
    })
    .await
}

#[test]
fn a_complete_reader_whose_credit_finds_no_room_gets_each_frame() {
    const FRAMES: usize = 63;
    const BODY: usize = 16_320;
    let charge = frame::charge(2, BODY);
    let sent = charge * u64::try_from(FRAMES).expect("a few frames");
    assert!(
        sent <= 1 << 20 && sent + charge > 1 << 20,
        "the grant ends here"
    );
    let half = u32::try_from(BODY / 2).expect("a short body");
    remote_sized(
        12,
        sim::link::Config::default(),
        [(1 << 16, WINDOW), (MESSAGE_MIN, 2 * MESSAGE_MIN)],
        move |node, _, transport, steps| async move {
            let session = transport.accept().await.expect("a session");
            let mut incoming = session.accept().await.expect("a stream");
            let mut sender = incoming.sender.take().expect("a two-way stream");
            for _ in 0..3 {
                incoming
                    .receiver
                    .recv()
                    .await
                    .expect("a message")
                    .expect("open");
            }
            send(&mut sender, 1, |out| Reply::Opened.encode(out)).await;
            // The second reader's keys fill the window of the session until the home
            // takes them.
            let mut second = session.accept().await.expect("a second stream");
            for _ in 0..FRAMES {
                send_head(&mut sender, half / 8, &[(0, half), (1, 2 * half)]).await;
                send(&mut sender, BODY, |out| out.fill(1)).await;
            }
            node.clock().sleep(Span::from_nanos(500_000_000)).await;
            let credit = loop {
                match race(incoming.receiver.recv(), second.receiver.recv()).await {
                    Ok(credit) => break credit,
                    Err(keys) => drop(keys.expect("a message").expect("open")),
                }
            };
            let credit = credit.expect("a credit").expect("open");
            assert_eq!(credit.len(), Credit::LEN);
            send_head(&mut sender, half / 8, &[(0, half), (1, 2 * half)]).await;
            send(&mut sender, BODY, |out| out.fill(1)).await;
            until(&node.clock(), &steps.done).await;
        },
        |test, _| async move {
            let mut reader = test.reader(&["value"], Mode::Complete).await;
            let names = define_many(&test, 1000);
            let hub = test.hub.clone();
            test.tasks.spawn(async move {
                drop(hub.reader(&names, Mode::Complete).await);
            });
            for _ in 0..FRAMES {
                reader.next().await.expect("a frame");
            }
            let deadline = test.clock.sleep(Span::from_nanos(3_000_000_000));
            let next = race(reader.next(), deadline).await;
            assert!(matches!(next, Ok(Ok(_))), "the frame after the credit came");
        },
    );
}

// The home never sees the code of this stop: #2014.
#[test]
fn a_reader_whose_pool_has_room_for_its_open_and_not_its_keys_gets_pool() {
    remote(
        14,
        sim::link::Config::default(),
        |node, _, transport, steps| async move {
            drop(transport.accept().await.expect("a session"));
            until(&node.clock(), &steps.done).await;
        },
        |test, _| async move {
            let names = define_many(&test, 200);
            let mut blocks = fill(&test.pool);
            let small = blocks
                .iter()
                .rposition(|block| (256..1024).contains(&block.len()))
                .expect("a small block");
            drop(blocks.swap_remove(small));
            let error = test
                .hub
                .reader(&names, Mode::Latest)
                .await
                .expect_err("the pool has no room for the keys");
            let keys = 201 * wire::hub::keys::LEN;
            let expected = test.pool.alloc(keys).expect_err("the pool is full");
            drop(blocks);
            assert_eq!(error, reader::Error::Pool(expected));
        },
    );
}

#[test]
fn a_reader_whose_home_replies_with_a_head_before_opened_stops_the_stream_as_malformed()
{
    remote(
        15,
        sim::link::Config::default(),
        |node, _, transport, steps| async move {
            let session = transport.accept().await.expect("a session");
            let mut incoming = session.accept().await.expect("a stream");
            let mut sender = incoming.sender.take().expect("a two-way stream");
            for _ in 0..3 {
                incoming.receiver.recv().await.expect("a message");
            }
            send_head(&mut sender, 1, &[(0, 8), (1, 16)]).await;
            let error = loop {
                if let Err(error) = incoming.receiver.recv().await {
                    break error;
                }
            };
            let malformed = Code(MALFORMED);
            assert_eq!(error, transport::Error::Reset { code: malformed });
            steps.stopped.store(true, Ordering::Relaxed);
            until(&node.clock(), &steps.done).await;
        },
        |test, steps| async move {
            let names = [name("value")];
            let error = test
                .hub
                .reader(&names, Mode::Latest)
                .await
                .expect_err("the home did not open the session");
            let expected = wire::hub::Error::Unopened { kind: 2 };
            assert_eq!(error, reader::Error::Message(expected));
            assert_eq!(
                error.to_string(),
                format!("the reply of the home broke the hub protocol: {expected}")
            );
            until(&test.clock, &steps.stopped).await;
        },
    );
}

#[test]
fn a_reader_gets_the_seq_and_path_of_each_frame_from_the_home() {
    remote(
        16,
        sim::link::Config::default(),
        |node, _, transport, steps| async move {
            let (mut sender, _receiver) = fake_open(&transport).await;
            let head = Reply::Head(Head {
                path: Path::Backfill,
                range: Range { seq: 41, count: 2 },
                series: 2,
            });
            send(&mut sender, head.encoded_len(), |out| head.encode(out)).await;
            send(&mut sender, 2 * ends::LEN, |out| {
                ends::encode([(0, 16), (1, 32)], out);
            })
            .await;
            send(&mut sender, 32, |out| out.fill(1)).await;
            until(&node.clock(), &steps.done).await;
        },
        |test, _| async move {
            let mut reader = test.reader(&["value"], Mode::Latest).await;
            let frame = reader.next().await.expect("a frame");
            assert_eq!(frame.view.path(), Path::Backfill);
            assert_eq!(frame.view.range(0), Some(Range { seq: 41, count: 2 }));
        },
    );
}

#[test]
fn a_complete_reader_that_the_home_ends_with_behind_gets_behind_at_each_next() {
    remote(
        17,
        sim::link::Config::default(),
        |node, _, transport, steps| async move {
            let (mut sender, _receiver) = fake_open(&transport).await;
            send(&mut sender, Reply::Behind.encoded_len(), |out| {
                Reply::Behind.encode(out);
            })
            .await;
            until(&node.clock(), &steps.done).await;
        },
        |test, _| async move {
            let mut reader = test.reader(&["value"], Mode::Complete).await;
            let ended = reader.next().await.expect_err("the home ended the session");
            assert_eq!(ended, Ended::Behind);
            assert_eq!(
                ended.to_string(),
                "the reader missed a frame and gets no later one: open a new reader"
            );
            assert_eq!(reader.next().await.expect_err("ended"), Ended::Behind);
        },
    );
}

#[test]
fn a_reader_whose_home_resets_the_open_stream_with_failed_gets_refused() {
    remote(
        18,
        sim::link::Config::default(),
        |node, _, transport, steps| async move {
            let (sender, _receiver) = fake_open(&transport).await;
            until(&node.clock(), &steps.opened).await;
            sender.reset(Code(Refusal::Failed.code()));
            until(&node.clock(), &steps.done).await;
        },
        |test, steps| async move {
            let mut reader = test.reader(&["value"], Mode::Latest).await;
            steps.open();
            let ended = reader.next().await.expect_err("the home ended the session");
            assert_eq!(ended, Ended::Refused(Refusal::Failed));
            assert_eq!(
                ended.to_string(),
                "the home ended the session with code 18: the home's buffer failed, or \
                 its mesh stopped"
            );
            assert_eq!(reader.next().await.expect_err("ended"), ended);
        },
    );
}

#[test]
fn a_reader_whose_home_resets_the_open_stream_with_a_code_outside_hub_wire_gets_stream()
{
    remote(
        19,
        sim::link::Config::default(),
        |node, _, transport, steps| async move {
            let (sender, _receiver) = fake_open(&transport).await;
            until(&node.clock(), &steps.opened).await;
            sender.reset(Code(7));
            until(&node.clock(), &steps.done).await;
        },
        |test, steps| async move {
            let mut reader = test.reader(&["value"], Mode::Latest).await;
            steps.open();
            let ended = reader.next().await.expect_err("the home reset the stream");
            let reset = transport::Error::Reset { code: Code(7) };
            assert_eq!(ended, Ended::Stream(reset));
            assert_eq!(
                ended.to_string(),
                "the stream to the home broke: the peer reset the stream (7)"
            );
        },
    );
}

#[test]
fn a_reader_whose_home_finishes_the_stream_between_frames_stops_it_as_malformed() {
    remote(
        20,
        sim::link::Config::default(),
        |node, _, transport, steps| async move {
            let (mut sender, mut receiver) = fake_open(&transport).await;
            sender.finish().expect("finishes");
            let error = loop {
                if let Err(error) = receiver.recv().await {
                    break error;
                }
            };
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
            let ended = reader.next().await.expect_err("the home finished");
            assert_eq!(ended, Ended::Message(wire::hub::Error::Finished));
            assert_eq!(
                ended.to_string(),
                "a message from the home broke the hub protocol: the home finished the \
                 stream before it ended the session"
            );
            until(&test.clock, &steps.stopped).await;
        },
    );
}

#[test]
fn a_complete_remote_reader_whose_home_stops_reading_credits_gets_its_frames_then_behind()
 {
    remote(
        40,
        sim::link::Config::default(),
        |node, tasks, transport, steps| async move {
            let kept = Arc::clone(&steps);
            hub_home(node, tasks, transport, steps, |test| async move {
                let mut writer = test.writer("w", &["time", "value"]).await;
                until(&test.clock, &kept.opened).await;
                let now = test.now();
                for n in 0..200 {
                    write_wide(&mut writer, now, n);
                }
                test.clock.sleep(Span::from_nanos(200_000_000)).await;
                write_wide(&mut writer, now, 200);
            })
            .await;
        },
        |test, steps| async move {
            let mut reader = test.reader(&["value"], Mode::Complete).await;
            steps.open();
            test.clock.sleep(Span::from_nanos(1_000_000_000)).await;
            let (charges, ended) = super::take_all(&mut reader).await;
            // The home sends the 128 frames that its grant of a window holds, then
            // Behind, finishes, and drops its receiver, which stops the stream with 0.
            assert_eq!((charges.len(), ended), (128, Ended::Behind));
        },
    );
}

#[test]
fn a_reader_whose_pool_has_no_room_for_a_frame_stops_the_stream_with_busy() {
    remote(
        21,
        sim::link::Config::default(),
        |node, _, transport, steps| async move {
            let (mut sender, mut receiver) = fake_open(&transport).await;
            until(&node.clock(), &steps.full).await;
            send_head(&mut sender, 1, &[(0, 8), (1, 16)]).await;
            let error = loop {
                if let Err(error) = receiver.recv().await {
                    break error;
                }
            };
            let busy = Code(Refusal::Busy.code());
            assert_eq!(error, transport::Error::Reset { code: busy });
            steps.stopped.store(true, Ordering::Relaxed);
            until(&node.clock(), &steps.done).await;
        },
        |test, steps| async move {
            let mut reader = test.reader(&["value"], Mode::Latest).await;
            let blocks = fill(&test.pool);
            steps.full.store(true, Ordering::Relaxed);
            let ended = reader.next().await.expect_err("the pool is full");
            let Ended::Pool(block::Error::Exhausted { requested, .. }) = ended else {
                panic!("not an exhausted pool: {ended:?}");
            };
            let expected = test.pool.alloc(requested).expect_err("the pool is full");
            drop(blocks);
            assert_eq!(ended, Ended::Pool(expected.clone()));
            assert_eq!(
                ended.to_string(),
                format!("the pool had no block for the reader: {expected}")
            );
            assert_eq!(reader.next().await.expect_err("ended"), ended);
            until(&test.clock, &steps.stopped).await;
        },
    );
}

#[test]
fn a_reader_whose_pool_has_no_room_for_a_message_of_keys_stops_the_stream_with_busy() {
    remote_sized(
        41,
        sim::link::Config::default(),
        [(MESSAGE_MIN, WINDOW), (MESSAGE_MIN, 2 * MESSAGE_MIN)],
        |node, _, transport, steps| async move {
            let session = transport.accept().await.expect("a session");
            let mut incoming = session.accept().await.expect("a stream");
            let mut sender = incoming.sender.take().expect("a two-way stream");
            until(&node.clock(), &steps.full).await;
            let error = loop {
                if let Err(error) = incoming.receiver.recv().await {
                    break error;
                }
            };
            let busy = Code(Refusal::Busy.code());
            assert_eq!(error, transport::Error::Reset { code: busy });
            let block = own_pool().alloc(1).expect("the pool has room");
            let stopped = sender.send(block.freeze()).await.expect_err("stopped");
            assert_eq!(stopped, transport::Error::Stopped { code: busy });
            steps.stopped.store(true, Ordering::Relaxed);
            until(&node.clock(), &steps.done).await;
        },
        |test, steps| async move {
            let names = define_many(&test, 1000);
            let full = async {
                test.clock.sleep(Span::from_nanos(100_000_000)).await;
                let blocks = fill(&test.pool);
                steps.full.store(true, Ordering::Relaxed);
                blocks
            };
            let (opened, blocks) =
                both(test.hub.reader(&names, Mode::Latest), full).await;
            let error = opened.expect_err("the pool has no room for the keys");
            let reader::Error::Pool(block::Error::Exhausted { requested, .. }) = error
            else {
                panic!("not an exhausted pool: {error:?}");
            };
            assert_eq!(requested, MESSAGE_MIN / 16 * 16, "a full message of keys");
            let expected = test.pool.alloc(requested).expect_err("the pool is full");
            drop(blocks);
            assert_eq!(error, reader::Error::Pool(expected));
            until(&test.clock, &steps.stopped).await;
        },
    );
}

/// Sends the head and the body of the `n`th frame of the credit tests: 31 frames of
/// the first body and one of the second charge exactly half a window.
async fn send_credit_frame(sender: &mut Sender, n: usize) {
    let body = if n == 31 { 14_272 } else { 16_320 };
    let half = u32::try_from(body / 2).expect("a short body");
    send_head(sender, half / 8, &[(0, half), (1, 2 * half)]).await;
    send(sender, body, |out| out.fill(1)).await;
}

#[test]
fn a_complete_reader_whose_pool_has_no_room_for_its_credit_stops_the_stream_with_busy()
{
    remote(
        42,
        sim::link::Config::default(),
        |node, _, transport, steps| async move {
            let (mut sender, mut receiver) = fake_open(&transport).await;
            for n in 0..32 {
                send_credit_frame(&mut sender, n).await;
            }
            let error = receiver.recv().await.expect_err("the reader stopped");
            let busy = Code(Refusal::Busy.code());
            assert_eq!(error, transport::Error::Reset { code: busy });
            let block = own_pool().alloc(1).expect("the pool has room");
            let stopped = sender.send(block.freeze()).await.expect_err("stopped");
            assert_eq!(stopped, transport::Error::Stopped { code: busy });
            steps.stopped.store(true, Ordering::Relaxed);
            until(&node.clock(), &steps.done).await;
        },
        |test, steps| async move {
            let mut reader = test.reader(&["value"], Mode::Complete).await;
            for _ in 0..32 {
                reader.next().await.expect("a frame");
            }
            // The call gives the last frame back, so the credit falls due at its
            // first poll.
            let next = reader.next();
            let blocks = fill(&test.pool);
            let ended = next.await.expect_err("the pool has no room for the credit");
            let expected = test.pool.alloc(Credit::LEN).expect_err("the pool is full");
            drop(blocks);
            assert_eq!(ended, Ended::Pool(expected.clone()));
            assert_eq!(
                ended.to_string(),
                format!("the pool had no block for the reader: {expected}")
            );
            assert_eq!(reader.next().await.expect_err("ended"), ended);
            until(&test.clock, &steps.stopped).await;
        },
    );
}

#[test]
fn a_complete_reader_raises_its_grant_once_a_credit_that_waited_for_room_is_sent() {
    const BODY: usize = 16_320;
    const FIRST: usize = 40;
    const MORE: usize = 4;
    let charge = frame::charge(2, BODY);
    // The credit falls due once 32 frames are given back.
    let limit = 32 * charge + (1 << 20);
    assert!(31 * charge < 1 << 19 && 32 * charge >= 1 << 19);
    let half = u32::try_from(BODY / 2).expect("a short body");
    remote_sized(
        43,
        sim::link::Config::default(),
        [(1 << 16, WINDOW), (MESSAGE_MIN, 2 * MESSAGE_MIN)],
        move |node, _, transport, steps| async move {
            let session = transport.accept().await.expect("a session");
            let mut incoming = session.accept().await.expect("a stream");
            let mut sender = incoming.sender.take().expect("a two-way stream");
            for _ in 0..3 {
                incoming
                    .receiver
                    .recv()
                    .await
                    .expect("a message")
                    .expect("open");
            }
            send(&mut sender, 1, |out| Reply::Opened.encode(out)).await;
            // The second reader's keys fill the window of the session until the home
            // takes them.
            let mut second = session.accept().await.expect("a second stream");
            for _ in 0..FIRST {
                send_head(&mut sender, half / 8, &[(0, half), (1, 2 * half)]).await;
                send(&mut sender, BODY, |out| out.fill(1)).await;
            }
            node.clock().sleep(Span::from_nanos(500_000_000)).await;
            let credit = loop {
                match race(incoming.receiver.recv(), second.receiver.recv()).await {
                    Ok(credit) => break credit,
                    Err(keys) => drop(keys.expect("a message").expect("open")),
                }
            };
            let credit = credit.expect("a credit").expect("open");
            let got = u64::from_le_bytes(credit[1..].try_into().expect("a credit"));
            assert_eq!(got, limit);
            for _ in 0..MORE {
                send_head(&mut sender, half / 8, &[(0, half), (1, 2 * half)]).await;
                send(&mut sender, BODY, |out| out.fill(1)).await;
            }
            let mut deadline =
                pin!(node.clock().sleep(Span::from_nanos(1_000_000_000)));
            loop {
                let keys = race(second.receiver.recv(), deadline.as_mut());
                match race(incoming.receiver.recv(), keys).await {
                    Ok(message) => panic!("a second credit: {message:?}"),
                    Err(Ok(keys)) => drop(keys.expect("a message").expect("open")),
                    Err(Err(())) => break,
                }
            }
            steps.stopped.store(true, Ordering::Relaxed);
            until(&node.clock(), &steps.done).await;
        },
        |test, steps| async move {
            let mut reader = test.reader(&["value"], Mode::Complete).await;
            let names = define_many(&test, 1000);
            let hub = test.hub.clone();
            test.tasks.spawn(async move {
                drop(hub.reader(&names, Mode::Complete).await);
            });
            for _ in 0..FIRST + MORE {
                reader.next().await.expect("a frame");
            }
            until(&test.clock, &steps.stopped).await;
        },
    );
}

#[test]
fn a_reader_whose_home_finishes_the_stream_before_opened_stops_it_as_malformed() {
    remote(
        44,
        sim::link::Config::default(),
        |node, _, transport, steps| async move {
            let session = transport.accept().await.expect("a session");
            let mut incoming = session.accept().await.expect("a stream");
            let mut sender = incoming.sender.take().expect("a two-way stream");
            for _ in 0..3 {
                incoming.receiver.recv().await.expect("a message");
            }
            sender.finish().expect("finishes");
            let error = loop {
                if let Err(error) = incoming.receiver.recv().await {
                    break error;
                }
            };
            let malformed = Code(MALFORMED);
            assert_eq!(error, transport::Error::Reset { code: malformed });
            steps.stopped.store(true, Ordering::Relaxed);
            until(&node.clock(), &steps.done).await;
        },
        |test, steps| async move {
            let names = [name("value")];
            let error = test
                .hub
                .reader(&names, Mode::Latest)
                .await
                .expect_err("the home did not open the session");
            let expected = wire::hub::Error::Finished;
            assert_eq!(error, reader::Error::Message(expected));
            assert_eq!(
                error.to_string(),
                "the reply of the home broke the hub protocol: the home finished the \
                 stream before it ended the session"
            );
            until(&test.clock, &steps.stopped).await;
        },
    );
}

#[test]
fn a_reader_whose_home_finishes_the_stream_inside_a_body_stops_it_as_malformed() {
    remote(
        45,
        sim::link::Config::default(),
        |node, _, transport, steps| async move {
            let (mut sender, mut receiver) = fake_open(&transport).await;
            send_head(&mut sender, 1, &[(0, 8), (1, 16)]).await;
            send(&mut sender, 10, |out| out.fill(1)).await;
            sender.finish().expect("finishes");
            let error = loop {
                if let Err(error) = receiver.recv().await {
                    break error;
                }
            };
            let malformed = Code(MALFORMED);
            assert_eq!(error, transport::Error::Reset { code: malformed });
            steps.stopped.store(true, Ordering::Relaxed);
            until(&node.clock(), &steps.done).await;
        },
        |test, steps| async move {
            let mut reader = test.reader(&["value"], Mode::Latest).await;
            let ended = reader.next().await.expect_err("the home finished");
            let expected = wire::hub::Error::Unfinished { remain: 6 };
            assert_eq!(ended, Ended::Message(expected));
            assert_eq!(
                ended.to_string(),
                "a message from the home broke the hub protocol: the stream ended with 6 \
                 bytes of its body to come"
            );
            until(&test.clock, &steps.stopped).await;
        },
    );
}

#[test]
fn a_complete_reader_whose_waiting_credit_fails_to_send_sends_no_later_credit() {
    const BODY: usize = 16_320;
    const FIRST: usize = 40;
    const ALL: usize = 64;
    let half = u32::try_from(BODY / 2).expect("a short body");
    remote_sized(
        46,
        sim::link::Config::default(),
        [(1 << 16, WINDOW), (MESSAGE_MIN, 2 * MESSAGE_MIN)],
        move |node, _, transport, steps| async move {
            let session = transport.accept().await.expect("a session");
            let mut incoming = session.accept().await.expect("a stream");
            let mut sender = incoming.sender.take().expect("a two-way stream");
            for _ in 0..3 {
                incoming
                    .receiver
                    .recv()
                    .await
                    .expect("a message")
                    .expect("open");
            }
            send(&mut sender, 1, |out| Reply::Opened.encode(out)).await;
            let _second = session.accept().await.expect("a second stream");
            for _ in 0..FIRST {
                send_head(&mut sender, half / 8, &[(0, half), (1, 2 * half)]).await;
                send(&mut sender, BODY, |out| out.fill(1)).await;
            }
            node.clock().sleep(Span::from_nanos(500_000_000)).await;
            incoming.receiver.stop(Code(0));
            node.clock().sleep(Span::from_nanos(500_000_000)).await;
            for _ in FIRST..ALL {
                send_head(&mut sender, half / 8, &[(0, half), (1, 2 * half)]).await;
                send(&mut sender, BODY, |out| out.fill(1)).await;
            }
            send(&mut sender, 1, |out| Reply::Behind.encode(out)).await;
            until(&node.clock(), &steps.done).await;
        },
        |test, _| async move {
            let mut reader = test.reader(&["value"], Mode::Complete).await;
            let names = define_many(&test, 1000);
            let hub = test.hub.clone();
            test.tasks.spawn(async move {
                drop(hub.reader(&names, Mode::Complete).await);
            });
            for _ in 0..ALL {
                reader.next().await.expect("a frame");
            }
            let next = reader.next();
            let blocks = fill(&test.pool);
            let ended = next.await.expect_err("behind");
            drop(blocks);
            assert_eq!(ended, Ended::Behind);
        },
    );
}

#[test]
fn a_complete_reader_whose_credit_the_stream_refuses_sends_no_later_credit() {
    const BODY: usize = 16_320;
    const ALL: usize = 33;
    let half = u32::try_from(BODY / 2).expect("a short body");
    remote(
        47,
        sim::link::Config::default(),
        move |node, _, transport, steps| async move {
            let (mut sender, receiver) = fake_open(&transport).await;
            receiver.stop(Code(0));
            node.clock().sleep(Span::from_nanos(500_000_000)).await;
            for _ in 0..ALL {
                send_head(&mut sender, half / 8, &[(0, half), (1, 2 * half)]).await;
                send(&mut sender, BODY, |out| out.fill(1)).await;
            }
            send(&mut sender, 1, |out| Reply::Behind.encode(out)).await;
            until(&node.clock(), &steps.done).await;
        },
        |test, _| async move {
            let mut reader = test.reader(&["value"], Mode::Complete).await;
            for _ in 0..ALL {
                reader.next().await.expect("a frame");
            }
            let next = reader.next();
            let blocks = fill(&test.pool);
            let ended = next.await.expect_err("behind");
            drop(blocks);
            assert_eq!(ended, Ended::Behind);
        },
    );
}
