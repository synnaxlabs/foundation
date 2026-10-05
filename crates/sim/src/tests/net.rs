//! Tests of the simulated network through `env::net`.

use std::collections::BTreeSet;
use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use env::net::udp::{Config as Udp, Meta, Receiver, Sender, Transmit};
use env::net::{Ecn, Error as Net};
use env::thread::Handle;
use types::time::{Monotonic, Span};

use super::{millis, shard, sim};
use crate::net::{addresses, under};
use crate::{Config, Error, Sim, link, node};

/// Arrivals: the receiver's clock, the meta, and the bytes of each batch.
type Log = Arc<Mutex<Vec<(Monotonic, Meta, Vec<u8>)>>>;

/// A run of two nodes, `a` and `b`, with `link` between them.
fn pair(seed: u64, link: link::Config) -> (Sim, node::Node, node::Node) {
    let mut sim = Sim::new(Config {
        seed,
        steps_max: 1_000_000,
        link,
    });
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    (sim, a, b)
}

/// The address of `port` on the IPv4 address of `node`.
fn at(node: &node::Node, port: u16) -> SocketAddr {
    SocketAddr::new(node.addresses()[0], port)
}

fn bind(node: &node::Node, local: SocketAddr) -> Result<(Sender, Receiver), Net> {
    node.net().udp(&Udp {
        local,
        send_buffer_bytes: 1 << 20,
        recv_buffer_bytes: 1 << 20,
    })
}

/// A socket on `port` of the IPv4 address of `node`.
fn udp(node: &node::Node, port: u16) -> (Sender, Receiver) {
    bind(node, at(node, port)).unwrap()
}

/// One datagram of `contents` to `destination`, with ECT(0).
fn transmit(destination: SocketAddr, contents: &[u8]) -> Transmit<'_> {
    Transmit {
        destination,
        source: None,
        ecn: Some(Ecn::Ect0),
        contents,
        segment: None,
    }
}

/// Starts a shard on `node` that sends each of `datagrams` to `to`, in order.
fn send(
    node: &node::Node,
    mut sender: Sender,
    to: SocketAddr,
    datagrams: Vec<Vec<u8>>,
) -> Handle {
    let handle = node.shards().start(shard("send"), move |_| async move {
        for contents in &datagrams {
            let transmit = transmit(to, contents);
            poll_fn(|cx| sender.poll_send(cx, &transmit)).await.unwrap();
        }
    });
    handle.unwrap()
}

/// Receives one batch into a buffer of `size` bytes.
async fn recv(receiver: &mut Receiver, size: usize) -> (Meta, Vec<u8>) {
    let mut bytes = vec![0; size];
    let mut meta = [Meta::default()];
    poll_fn(|cx| {
        let mut buffers = [IoSliceMut::new(&mut bytes)];
        receiver.poll_recv(cx, &mut buffers, &mut meta)
    })
    .await
    .unwrap();
    bytes.truncate(meta[0].len);
    (meta[0], bytes)
}

/// Starts a shard on `node` that receives batches into `log` and never ends.
fn receive(node: &node::Node, mut receiver: Receiver, log: &Log) -> Handle {
    let (clock, log) = (node.clock(), Arc::clone(log));
    let handle = node.shards().start(shard("receive"), move |_| async move {
        loop {
            let (meta, bytes) = recv(&mut receiver, 65_536).await;
            log.lock().unwrap().push((clock.now(), meta, bytes));
        }
    });
    handle.unwrap()
}

/// The datagrams in `log`, split by stride.
fn datagrams(log: &Log) -> Vec<Vec<u8>> {
    let log = log.lock().unwrap();
    (log.iter())
        .flat_map(|(_, meta, bytes)| {
            bytes.chunks(meta.stride.max(1)).map(<[u8]>::to_vec)
        })
        .collect()
}

/// The times at which the datagrams in `log` arrived.
fn times(log: &Log) -> Vec<Monotonic> {
    log.lock().unwrap().iter().map(|&(time, ..)| time).collect()
}

/// The receiver's clock `span` after the run starts.
fn after(span: Span) -> Monotonic {
    node::Config::default().monotonic + span
}

/// The default delay.
fn delay() -> Span {
    link::Config::default().delay
}

/// Sends `datagrams` from `a` to `b` on one link, runs for a second, and gives the
/// log of `b`.
fn exchange(seed: u64, link: link::Config, datagrams: Vec<Vec<u8>>) -> (Sim, Log) {
    let (mut sim, a, b) = pair(seed, link);
    let log = Log::default();
    let (sender, _a) = udp(&a, 4433);
    let (_b, receiver) = udp(&b, 4433);
    let _receive = receive(&b, receiver, &log);
    let _send = send(&a, sender, at(&b, 4433), datagrams);
    sim.run_for(Span::SECOND).unwrap();
    (sim, log)
}

/// `count` datagrams of one byte each: 0, 1, 2, and so on, wrapping at 256.
fn numbered(count: usize) -> Vec<Vec<u8>> {
    (0..count).map(|n| vec![n.to_le_bytes()[0]]).collect()
}

#[test]
fn addresses_follow_the_order_of_the_nodes() {
    let mut sim = sim(0);
    let a = sim.node(node::Config::default());
    let b = sim.node(node::Config::default());
    let v6 = |host| IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, host));
    let v4 = |host| IpAddr::V4(Ipv4Addr::new(10, 0, 0, host));
    assert_eq!(a.addresses(), [v4(1), v6(1)]);
    assert_eq!(b.addresses(), [v4(2), v6(2)]);
}

#[test]
fn a_datagram_arrives_after_the_delay_of_its_link() {
    let (_sim, log) = exchange(0, link::Config::default(), vec![b"abc".to_vec()]);
    let meta = Meta {
        source: SocketAddr::from(([10, 0, 0, 1], 4433)),
        destination: Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2))),
        ecn: Some(Ecn::Ect0),
        len: 3,
        stride: 3,
    };
    assert_eq!(
        *log.lock().unwrap(),
        [(after(delay()), meta, b"abc".to_vec())]
    );
}

#[test]
fn a_link_change_applies_to_datagrams_sent_after_it() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let log = Log::default();
    let (mut sender, _a) = udp(&a, 4433);
    let (_b, receiver) = udp(&b, 4433);
    let _receive = receive(&b, receiver, &log);
    let (clock, to) = (a.clock(), at(&b, 4433));
    let _send = a.shards().start(shard("send"), move |_| async move {
        for contents in [b"a", b"b"] {
            let transmit = transmit(to, contents);
            poll_fn(|cx| sender.poll_send(cx, &transmit)).await.unwrap();
            clock.sleep(Span::SECOND).await;
        }
    });
    sim.run_for(millis(500)).unwrap();
    let slow = link::Config {
        delay: Span::MILLISECOND,
        ..link::Config::default()
    };
    sim.link(&a, &b, slow);
    sim.run_for(Span::SECOND).unwrap();
    let late = after(Span::SECOND) + Span::MILLISECOND;
    assert_eq!(times(&log), [after(delay()), late]);
}

#[test]
fn a_link_acts_in_one_direction() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let cut = link::Config {
        loss: 1.0,
        ..link::Config::default()
    };
    sim.link(&a, &b, cut);
    let (to_a, to_b) = (Log::default(), Log::default());
    let (sender_a, receiver_a) = udp(&a, 4433);
    let (sender_b, receiver_b) = udp(&b, 4433);
    let _receive_a = receive(&a, receiver_a, &to_a);
    let _receive_b = receive(&b, receiver_b, &to_b);
    let _send_a = send(&a, sender_a, at(&b, 4433), vec![b"to b".to_vec()]);
    let _send_b = send(&b, sender_b, at(&a, 4433), vec![b"to a".to_vec()]);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(datagrams(&to_a), [b"to a".to_vec()]);
    assert_eq!(datagrams(&to_b), Vec::<Vec<u8>>::new());
}

/// The order in which 20 datagrams arrive over a link with 1 ms of jitter.
fn jittered(seed: u64) -> Vec<u8> {
    let link = link::Config {
        jitter: Span::MILLISECOND,
        ..link::Config::default()
    };
    let (_sim, log) = exchange(seed, link, numbered(20));
    let (times, late) = (times(&log), after(delay()) + Span::MILLISECOND);
    assert!(
        times.is_sorted_by(|early, later| early < later),
        "{times:?}"
    );
    assert!((after(delay())..=late).contains(&times[0]), "{times:?}");
    assert!((after(delay())..=late).contains(&times[19]), "{times:?}");
    datagrams(&log).concat()
}

#[test]
fn jitter_reorders_datagrams_and_the_seed_replays_the_order() {
    let order = jittered(1);
    let sent: Vec<u8> = (0..20).collect();
    assert_eq!(order.iter().copied().collect::<BTreeSet<_>>().len(), 20);
    assert_ne!(order, sent);
    assert_eq!(jittered(1), order);
    assert_ne!(jittered(2), order);
}

/// How many of `count` datagrams arrive over a link with `loss`.
fn delivered(loss: f64, count: usize) -> usize {
    let link = link::Config {
        loss,
        ..link::Config::default()
    };
    datagrams(&exchange(0, link, numbered(count)).1).len()
}

#[test]
fn a_chance_of_zero_holds_no_draw_and_a_chance_of_one_holds_every_draw() {
    assert!(!under(0, 0.0));
    assert!(under(u32::MAX, 1.0));
    assert!(under(u32::MAX / 2, 0.5) && !under(u32::MAX / 2 + 1, 0.5));
}

#[test]
fn a_link_loses_each_datagram_with_its_chance() {
    assert_eq!(delivered(0.0, 1_000), 1_000);
    assert_eq!(delivered(1.0, 1_000), 0);
    let half = delivered(0.5, 10_000);
    assert!((4_800..=5_200).contains(&half), "{half} of 10,000 arrived");
}

#[test]
fn duplication_delivers_each_datagram_twice() {
    let link = link::Config {
        duplication: 1.0,
        ..link::Config::default()
    };
    let (_sim, log) = exchange(0, link, numbered(3));
    let mut arrived = datagrams(&log).concat();
    arrived.sort_unstable();
    assert_eq!(arrived, [0, 0, 1, 1, 2, 2]);
}

#[test]
fn a_datagram_over_the_mtu_is_lost() {
    let sizes = vec![vec![1; 1_472], vec![2; 1_473]];
    let (_sim, log) = exchange(0, link::Config::default(), sizes);
    assert_eq!(datagrams(&log), [vec![1; 1_472]]);
    let (mut sim, a, b) = pair(0, link::Config::default());
    let log = Log::default();
    let (sender, _a) = bind(&a, SocketAddr::new(a.addresses()[1], 4433)).unwrap();
    let (_b, receiver) = bind(&b, SocketAddr::new(b.addresses()[1], 4433)).unwrap();
    let _receive = receive(&b, receiver, &log);
    let to = SocketAddr::new(b.addresses()[1], 4433);
    let _send = send(&a, sender, to, vec![vec![3; 1_452], vec![4; 1_453]]);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(datagrams(&log), [vec![3; 1_452]]);
}

#[test]
fn an_empty_datagram_arrives() {
    let (_sim, log) = exchange(0, link::Config::default(), vec![Vec::new()]);
    let log = log.lock().unwrap();
    assert_eq!((log.len(), log[0].1.len, log[0].1.stride), (1, 0, 0));
}

#[test]
fn a_datagram_to_a_port_with_nothing_bound_is_lost() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let log = Log::default();
    let (sender, _a) = udp(&a, 4433);
    let (_b, receiver) = udp(&b, 4433);
    let _receive = receive(&b, receiver, &log);
    let _lost = send(&a, sender.clone(), at(&b, 4434), vec![b"lost".to_vec()]);
    sim.run_for(Span::SECOND).unwrap();
    let _sent = send(&a, sender, at(&b, 4433), vec![b"sent".to_vec()]);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(datagrams(&log), [b"sent".to_vec()]);
}

#[test]
fn a_send_to_an_address_of_no_node_succeeds() {
    let nowhere = SocketAddr::from(([10, 0, 0, 9], 4433));
    sent(move |_| transmit(nowhere, b"lost"), Ok(()));
}

/// A socket on `port` of the IPv4 address of `node`, with a receive queue of
/// `bytes`.
fn queue(node: &node::Node, port: u16, bytes: usize) -> (Sender, Receiver) {
    let config = Udp {
        local: at(node, port),
        send_buffer_bytes: 1 << 20,
        recv_buffer_bytes: bytes,
    };
    node.net().udp(&config).unwrap()
}

#[test]
fn a_full_receive_queue_drops_datagrams() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let (sender, _a) = udp(&a, 4433);
    let (_b, receiver) = queue(&b, 4433, 10);
    let _send = send(&a, sender, at(&b, 4433), vec![vec![7; 4]; 4]);
    sim.run_for(Span::SECOND).unwrap();
    let log = Log::default();
    let _receive = receive(&b, receiver, &log);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(datagrams(&log), [vec![7; 4], vec![7; 4]]);
}

/// A seed under which the first socket that `b` binds receives `batch` datagrams
/// per buffer, and the first that `a` binds sends 64.
fn batched(batch: usize) -> u64 {
    let fits = |seed| {
        let (_sim, a, b) = pair(seed, link::Config::default());
        let (sender, _a) = udp(&a, 4433);
        let (_b, receiver) = udp(&b, 4433);
        sender.batch_max().get() == 64 && receiver.batch_max().get() == batch
    };
    (0..u64::MAX).find(|&seed| fits(seed)).unwrap()
}

/// The `(len, stride)` of each batch that `b` receives when `a` sends 1,050 bytes in
/// segments of 100, then 100 bytes, under a receive batch max of `batch`.
fn joined(batch: usize) -> Vec<(usize, usize)> {
    let (mut sim, a, b) = pair(batched(batch), link::Config::default());
    let log = Log::default();
    let (mut sender, _a) = udp(&a, 4433);
    let (_b, receiver) = udp(&b, 4433);
    let _receive = receive(&b, receiver, &log);
    let to = at(&b, 4433);
    let _send = a.shards().start(shard("send"), move |_| async move {
        let contents: Vec<u8> = (0..=u8::MAX).cycle().take(1_050).collect();
        let segmented = Transmit {
            segment: NonZeroUsize::new(100),
            ..transmit(to, &contents)
        };
        poll_fn(|cx| sender.poll_send(cx, &segmented))
            .await
            .unwrap();
        let single = transmit(to, &[9; 100]);
        poll_fn(|cx| sender.poll_send(cx, &single)).await.unwrap();
    });
    sim.run_for(Span::SECOND).unwrap();
    let mut bytes: Vec<u8> = (0..=u8::MAX).cycle().take(1_050).collect();
    bytes.extend([9; 100]);
    assert_eq!(datagrams(&log).concat(), bytes);
    let log = log.lock().unwrap();
    log.iter()
        .map(|(_, meta, _)| (meta.len, meta.stride))
        .collect()
}

#[test]
fn a_receive_joins_datagrams_of_one_source_and_size_up_to_its_batch_max() {
    assert_eq!(joined(64), [(1_050, 100), (100, 100)]);
    assert_eq!(joined(8), [(800, 100), (250, 100), (100, 100)]);
    let mut single = vec![(100, 100); 10];
    single.extend([(50, 50), (100, 100)]);
    assert_eq!(joined(1), single);
}

#[test]
fn a_receive_joins_only_the_datagrams_that_fit_its_buffer() {
    let (mut sim, a, b) = pair(batched(64), link::Config::default());
    let (sender, _a) = udp(&a, 4433);
    let (_b, mut receiver) = udp(&b, 4433);
    let _send = send(&a, sender, at(&b, 4433), vec![vec![1; 100]; 3]);
    sim.run_for(Span::SECOND).unwrap();
    let batches = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&batches);
    let _receive = b.shards().start(shard("receive"), move |_| async move {
        for _ in 0..2 {
            let (meta, _) = recv(&mut receiver, 250).await;
            log.lock().unwrap().push((meta.len, meta.stride));
        }
    });
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(*batches.lock().unwrap(), [(200, 100), (100, 100)]);
}

#[test]
fn a_receive_frees_its_bytes_in_the_queue() {
    let (mut sim, a, b) = pair(batched(64), link::Config::default());
    let (sender, _a) = udp(&a, 4433);
    let (_b, receiver) = queue(&b, 4433, 8);
    let log = Log::default();
    let _receive = receive(&b, receiver, &log);
    let _first = send(&a, sender.clone(), at(&b, 4433), vec![vec![1; 4]; 2]);
    sim.run_for(Span::SECOND).unwrap();
    let _second = send(&a, sender, at(&b, 4433), vec![vec![2; 4]; 2]);
    sim.run_for(Span::SECOND).unwrap();
    let sent = [vec![1; 4], vec![1; 4], vec![2; 4], vec![2; 4]];
    assert_eq!(datagrams(&log), sent);
}

#[test]
fn a_receive_fills_one_buffer_per_batch() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let (first, _one) = udp(&a, 4433);
    let (second, _two) = udp(&a, 4434);
    let (_sender, mut receiver) = udp(&b, 4433);
    let _sends = [
        send(&a, first, at(&b, 4433), vec![b"one".to_vec()]),
        send(&a, second, at(&b, 4433), vec![b"two".to_vec()]),
    ];
    sim.run_for(Span::SECOND).unwrap();
    let batches = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&batches);
    let _receive = b.shards().start(shard("receive"), move |_| async move {
        let mut bytes = [[0; 8]; 3];
        let mut meta = [Meta::default(); 3];
        let count = poll_fn(|cx| {
            let mut buffers = bytes.each_mut().map(|bytes| IoSliceMut::new(bytes));
            receiver.poll_recv(cx, &mut buffers, &mut meta)
        })
        .await
        .unwrap();
        let sources = meta[..count].iter().map(|meta| meta.source.port());
        log.lock().unwrap().extend(sources);
    });
    sim.run_for(Span::SECOND).unwrap();
    let mut ports = batches.lock().unwrap().clone();
    ports.sort_unstable();
    assert_eq!(ports, [4433, 4434]);
}

#[test]
fn a_receive_cuts_a_datagram_at_the_end_of_its_buffer() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let (sender, _a) = udp(&a, 4433);
    let (_b, mut receiver) = udp(&b, 4433);
    let _send = send(&a, sender, at(&b, 4433), vec![b"abcde".to_vec()]);
    let cut = Arc::new(Mutex::new(None));
    let log = Arc::clone(&cut);
    let _receive = b.shards().start(shard("receive"), move |_| async move {
        let batch = recv(&mut receiver, 2).await;
        *log.lock().unwrap() = Some(batch);
    });
    sim.run_for(Span::SECOND).unwrap();
    let (meta, bytes) = cut.lock().unwrap().clone().unwrap();
    assert_eq!((meta.len, meta.stride, bytes), (2, 2, b"ab".to_vec()));
}

#[test]
fn a_socket_on_the_unspecified_v6_address_receives_v4() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let log = Log::default();
    let (sender, _a) = udp(&a, 4433);
    let any = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 4433);
    let (_b, receiver) = bind(&b, any).unwrap();
    let _receive = receive(&b, receiver, &log);
    let _send = send(&a, sender, at(&b, 4433), vec![b"v4".to_vec()]);
    sim.run_for(Span::SECOND).unwrap();
    let log = log.lock().unwrap();
    let meta = log[0].1;
    assert_eq!(meta.source, at(&a, 4433));
    assert_eq!(meta.destination, Some(b.addresses()[0]));
}

#[test]
fn port_zero_binds_the_lowest_free_port() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let [v4, v6] = node.addresses();
    let any = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let (first, receiver) = bind(&node, any).unwrap();
    let (second, _second) = bind(&node, any).unwrap();
    assert_eq!(first.local().port(), 49_152);
    assert_eq!(second.local().port(), 49_153);
    let taken = SocketAddr::new(v4, 49_152);
    assert_eq!(
        bind(&node, taken).unwrap_err(),
        Net::AddressInUse { local: taken }
    );
    let dual = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 49_152);
    assert_eq!(
        bind(&node, dual).unwrap_err(),
        Net::AddressInUse { local: dual }
    );
    let (_v6, _v6_receiver) = bind(&node, SocketAddr::new(v6, 49_152)).unwrap();
    drop((first, receiver));
    let (again, _again) = bind(&node, taken).unwrap();
    assert_eq!(again.local(), taken);
}

#[test]
fn a_bind_to_an_address_of_another_node_fails() {
    let (_sim, a, b) = pair(0, link::Config::default());
    let other = at(&b, 4433);
    assert_eq!(bind(&a, other).unwrap_err(), Net::Io { code: 99 });
}

#[test]
fn sockets_draw_each_batch_max_from_one_eight_and_sixty_four() {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    let sockets: Vec<_> = (0..40).map(|port| udp(&node, 5_000 + port)).collect();
    let sends: BTreeSet<usize> = (sockets.iter())
        .map(|(sender, _)| sender.batch_max().get())
        .collect();
    let receives: BTreeSet<usize> = (sockets.iter())
        .map(|(_, receiver)| receiver.batch_max().get())
        .collect();
    assert_eq!(sends, BTreeSet::from([1, 8, 64]));
    assert_eq!(receives, BTreeSet::from([1, 8, 64]));
}

/// Sends once from `a`'s IPv4 socket with the transmit that `make` gives for `b`, and
/// checks that the send gives `expected`.
fn sent(
    make: impl FnOnce(&node::Node) -> Transmit<'static>,
    expected: Result<(), Net>,
) {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let (mut sender, _a) = udp(&a, 4433);
    let transmit = make(&b);
    let result = Arc::new(Mutex::new(None));
    let log = Arc::clone(&result);
    let _send = a.shards().start(shard("send"), move |_| async move {
        let sent = poll_fn(|cx| sender.poll_send(cx, &transmit)).await;
        *log.lock().unwrap() = Some(sent);
    });
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(result.lock().unwrap().clone(), Some(expected));
}

#[test]
fn a_v4_socket_cannot_reach_a_v6_address() {
    let remote =
        SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 2)), 4433);
    sent(
        move |_| transmit(remote, b"v6"),
        Err(Net::Unreachable { remote }),
    );
}

#[test]
fn a_send_from_an_address_of_another_node_fails() {
    let spoof = |b: &node::Node| Transmit {
        source: Some(b.addresses()[0]),
        ..transmit(at(b, 4433), b"spoof")
    };
    sent(spoof, Err(Net::Io { code: 99 }));
}

/// The digest of a run that sends 20 datagrams over a jittered link.
fn digest(seed: u64) -> u64 {
    let link = link::Config {
        jitter: Span::MILLISECOND,
        ..link::Config::default()
    };
    exchange(seed, link, numbered(20)).0.digest()
}

#[test]
fn the_same_seed_gives_the_same_digest() {
    assert_eq!(digest(3), digest(3));
    assert_ne!(digest(3), digest(4));
    assert_ne!(Sim::new(Config::default()).digest(), digest(3));
}

/// Sends one datagram with a context that never wakes.
fn send_once(sender: &mut Sender, _: &mut Receiver) {
    let transmit = transmit(SocketAddr::from(([10, 0, 0, 2], 4433)), b"once");
    let cx = &mut Context::from_waker(Waker::noop());
    assert_eq!(sender.poll_send(cx, &transmit), Poll::Ready(Ok(())));
}

/// Receives once from an empty queue, with a context that never wakes.
fn recv_once(_: &mut Sender, receiver: &mut Receiver) {
    let (mut bytes, mut meta) = ([0; 8], [Meta::default()]);
    let cx = &mut Context::from_waker(Waker::noop());
    let mut buffers = [IoSliceMut::new(&mut bytes)];
    assert!(receiver.poll_recv(cx, &mut buffers, &mut meta).is_pending());
}

/// Polls the halves of a socket of `a` with `poll` on a shard, then again on a
/// second shard, and gives the error of the run.
fn stray(poll: fn(&mut Sender, &mut Receiver)) -> Error {
    let (mut sim, a, _b) = pair(0, link::Config::default());
    let (mut sender, mut receiver) = udp(&a, 4433);
    let slot = Arc::new(Mutex::new(None));
    let give = Arc::clone(&slot);
    let _first = a.shards().start(shard("first"), move |_| async move {
        poll(&mut sender, &mut receiver);
        *give.lock().unwrap() = Some((sender, receiver));
    });
    sim.run().unwrap();
    let (mut sender, mut receiver) = slot.lock().unwrap().take().unwrap();
    let _second = a.shards().start(shard("second"), move |_| async move {
        poll(&mut sender, &mut receiver);
    });
    sim.run().unwrap_err()
}

/// The error of a run whose thread `thread` panicked with `message`.
fn panicked(thread: &str, message: &str) -> Error {
    Error::Panicked {
        thread: thread.into(),
        message: message.into(),
        seed: 0,
    }
}

#[test]
fn a_socket_half_polled_on_a_second_thread_panics() {
    let message = "a socket half polls only on the sim thread of its first poll";
    assert_eq!(stray(send_once), panicked("second", message));
    assert_eq!(stray(recv_once), panicked("second", message));
}

#[test]
fn a_socket_polled_on_a_thread_of_another_node_panics() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let (mut sender, mut receiver) = udp(&a, 4433);
    let _b = b.shards().start(shard("b"), move |_| async move {
        send_once(&mut sender, &mut receiver);
    });
    let message = "a socket of node 0 polls on a thread of node 1";
    assert_eq!(sim.run().unwrap_err(), panicked("b", message));
}

#[test]
#[should_panic(expected = "a socket polls only on a thread that the sim started")]
fn a_socket_polled_outside_the_sim_panics() {
    let (_sim, a, _b) = pair(0, link::Config::default());
    let (mut sender, mut receiver) = udp(&a, 4433);
    send_once(&mut sender, &mut receiver);
}

#[test]
fn the_last_node_with_an_address_has_the_last_host() {
    let last = 16_777_213;
    let v6 = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0x00ff, 0xfffe);
    let v4 = Ipv4Addr::new(10, 255, 255, 254);
    assert_eq!(addresses(last), [IpAddr::V4(v4), IpAddr::V6(v6)]);
}

#[test]
#[should_panic(
    expected = "node 16777214 has no address: 10.0.0.0/8 holds 16,777,214 nodes"
)]
fn a_node_past_the_last_host_has_no_address() {
    addresses(16_777_214);
}

#[test]
#[should_panic(expected = "has a negative span or a chance outside 0 to 1")]
fn a_link_with_a_chance_over_one_panics() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let link = link::Config {
        duplication: 1.5,
        ..link::Config::default()
    };
    sim.link(&a, &b, link);
}

#[test]
#[should_panic(expected = "has a negative span or a chance outside 0 to 1")]
fn a_run_with_a_negative_delay_panics() {
    let link = link::Config {
        delay: Span::from_nanos(-1),
        ..link::Config::default()
    };
    pair(0, link);
}

#[test]
#[should_panic(expected = "Node(0) belongs to another sim")]
fn a_link_to_a_node_of_another_sim_panics() {
    let (mut sim, a, _b) = pair(0, link::Config::default());
    let (_other, stranger, _) = pair(0, link::Config::default());
    sim.link(&a, &stranger, link::Config::default());
}
