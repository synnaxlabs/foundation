//! Tests of the simulated network through `env::net`.

use std::collections::BTreeSet;
use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use env::net::udp::{Config as Udp, Meta, Receiver, Sender, Transmit};
use env::net::{Ecn, Error as Net};
use env::thread::Handle;
use types::time::{Monotonic, Span};

use super::{after, at, delay, millis, pair, panicked, shard, sim};
use crate::drivers::yield_now;
use crate::net::addresses;
use crate::{Config, Crash, Error, Sim, link, node};

/// Arrivals: the receiver's clock, the meta, and the bytes of each batch.
type Log = Arc<Mutex<Vec<(Monotonic, Meta, Vec<u8>)>>>;

/// A socket of `node` on `local`, whose receive queue holds 10,000 small datagrams.
fn bind(node: &node::Node, local: SocketAddr) -> Result<(Sender, Receiver), Net> {
    node.net().udp(&Udp {
        local,
        send_buffer_bytes: 1 << 20,
        recv_buffer_bytes: 1 << 24,
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

/// Starts a shard on `node` that sends each of `datagrams` to `to`, in order, and
/// sleeps for `gap` after each.
fn send_spaced(
    node: &node::Node,
    mut sender: Sender,
    to: SocketAddr,
    datagrams: Vec<Vec<u8>>,
    gap: Span,
) -> Handle {
    let clock = node.clock();
    let handle = node.shards().start(shard("send"), move |_| async move {
        for contents in &datagrams {
            let transmit = transmit(to, contents);
            poll_fn(|cx| sender.poll_send(cx, &transmit)).await.unwrap();
            clock.sleep(gap).await;
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

/// A link that carries 1,000 bytes in a millisecond.
fn rated() -> link::Config {
    link::Config {
        rate: NonZeroU64::new(1_000_000),
        ..link::Config::default()
    }
}

#[test]
fn a_link_with_a_rate_carries_datagrams_sent_at_once_one_after_another() {
    // With its 28 header bytes, each datagram takes 1,000 bytes of the link.
    let (_sim, log) = exchange(0, rated(), vec![vec![1; 972], vec![2; 972]]);
    let left = |n| after(delay()) + millis(n);
    assert_eq!(times(&log), [left(1), left(2)]);
    assert_eq!(datagrams(&log), [vec![1; 972], vec![2; 972]]);
}

#[test]
fn each_link_of_a_node_has_its_own_rate() {
    let (mut sim, a, b) = pair(0, rated());
    let c = sim.node(node::Config::default());
    let (logs, mut sockets) = ([Log::default(), Log::default()], Vec::new());
    for (node, log) in [&b, &c].into_iter().zip(&logs) {
        let (sender, receiver) = udp(node, 4433);
        sockets.push((sender, receive(node, receiver, log)));
    }
    let (sender, _a) = udp(&a, 4433);
    let to = [at(&b, 4433), at(&c, 4433)];
    let _send = a.shards().start(shard("send"), move |_| async move {
        let mut sender = sender;
        for destination in to {
            let transmit = transmit(destination, &[0; 972]);
            poll_fn(|cx| sender.poll_send(cx, &transmit)).await.unwrap();
        }
    });
    sim.run_for(Span::SECOND).unwrap();
    for log in &logs {
        assert_eq!(times(log), [after(delay()) + millis(1)]);
    }
}

#[test]
fn a_datagram_sent_on_an_idle_link_with_a_rate_waits_only_for_its_own_transmit() {
    let (mut sim, a, b) = pair(0, rated());
    let log = Log::default();
    let (sender, _a) = udp(&a, 4433);
    let (_b, receiver) = udp(&b, 4433);
    let _receive = receive(&b, receiver, &log);
    let contents = vec![vec![1; 972], vec![2; 972]];
    let _send = send_spaced(&a, sender, at(&b, 4433), contents, millis(10));
    sim.run_for(Span::SECOND).unwrap();
    let left = |n| after(delay()) + millis(n);
    assert_eq!(times(&log), [left(1), left(11)]);
}

/// The clock of a sender when each of its sends was ready, and the polls it took.
type Sent = Arc<Mutex<Vec<(Monotonic, usize)>>>;

/// A socket on `port` of the IPv4 address of `node`, whose send buffer holds `bytes`.
fn buffered(node: &node::Node, port: u16, bytes: usize) -> (Sender, Receiver) {
    node.net()
        .udp(&Udp {
            local: at(node, port),
            send_buffer_bytes: bytes,
            recv_buffer_bytes: 1 << 24,
        })
        .unwrap()
}

/// Starts a shard on `node` that sends each of `datagrams` to `to`, in order, and logs
/// each send in `sent`.
fn send_logged(
    node: &node::Node,
    mut sender: Sender,
    to: SocketAddr,
    datagrams: Vec<Vec<u8>>,
    sent: &Sent,
) -> Handle {
    let (clock, sent) = (node.clock(), Arc::clone(sent));
    let handle = node.shards().start(shard("send"), move |_| async move {
        for contents in &datagrams {
            let (transmit, mut polls) = (transmit(to, contents), 0);
            poll_fn(|cx| {
                polls += 1;
                sender.poll_send(cx, &transmit)
            })
            .await
            .unwrap();
            sent.lock().unwrap().push((clock.now(), polls));
        }
    });
    handle.unwrap()
}

/// Sends `count` datagrams of 972 bytes from `sender` to `to` in one send, and runs
/// until true time `until`.
fn burst(
    sim: &mut Sim,
    node: &node::Node,
    mut sender: Sender,
    to: SocketAddr,
    count: usize,
    until: Span,
) {
    let _burst = node.shards().start(shard("burst"), move |_| async move {
        let contents = vec![1; 972 * count];
        let transmit = Transmit {
            segment: NonZeroUsize::new(972),
            ..transmit(to, &contents)
        };
        poll_fn(|cx| sender.poll_send(cx, &transmit)).await.unwrap();
    });
    sim.run_for(until).unwrap();
}

#[test]
fn a_send_to_a_full_send_buffer_waits_until_a_datagram_leaves_its_link() {
    let (mut sim, a, b) = pair(0, rated());
    let (log, sent) = (Log::default(), Sent::default());
    // One datagram takes 972 + 768 bytes of the buffer, so the buffer is then full.
    let (sender, _a) = buffered(&a, 4433, 1740);
    let (_b, receiver) = udp(&b, 4433);
    let _receive = receive(&b, receiver, &log);
    let contents = vec![vec![1; 972], vec![2; 972], vec![3; 972]];
    let _send = send_logged(&a, sender, at(&b, 4433), contents.clone(), &sent);
    sim.run_for(Span::SECOND).unwrap();
    let ready = [(0, 1), (1, 2), (2, 2)].map(|(n, polls)| (after(millis(n)), polls));
    assert_eq!(*sent.lock().unwrap(), ready);
    assert_eq!(times(&log), [1, 2, 3].map(|n| after(delay()) + millis(n)));
    assert_eq!(datagrams(&log), contents);
}

#[test]
fn a_send_that_waits_wakes_only_when_the_send_buffer_has_room() {
    let (mut sim, a, b) = pair(0, rated());
    let sent = Sent::default();
    let (sender, _a) = buffered(&a, 4433, 1740);
    let (_b, _receiver) = udp(&b, 4433);
    let to = at(&b, 4433);
    burst(
        &mut sim,
        &a,
        sender.clone(),
        to,
        2,
        Span::from_nanos(500_000),
    );
    // When the first datagram leaves, the buffer still takes 1,740 bytes.
    let _send = send_logged(&a, sender, to, vec![vec![2]], &sent);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(*sent.lock().unwrap(), [(after(millis(2)), 2)]);
}

#[test]
fn a_send_buffer_on_a_link_with_no_rate_never_fills() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let sent = Sent::default();
    let (sender, _a) = buffered(&a, 4433, 1);
    let (_b, _receiver) = udp(&b, 4433);
    let datagrams = vec![vec![1; 972]; 3];
    let _send = send_logged(&a, sender, at(&b, 4433), datagrams, &sent);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(*sent.lock().unwrap(), [(after(millis(0)), 1); 3]);
}

#[test]
fn each_send_that_waits_on_a_full_send_buffer_wakes() {
    let (mut sim, a, b) = pair(0, rated());
    let sent = Sent::default();
    let (sender, _a) = buffered(&a, 4433, 4300);
    let (_b, _receiver) = udp(&b, 4433);
    let to = at(&b, 4433);
    burst(
        &mut sim,
        &a,
        sender.clone(),
        to,
        3,
        Span::from_nanos(500_000),
    );
    // When the first datagram leaves, the buffer takes 3,480 bytes, and has room for
    // two sends of one byte, which take 769 bytes each.
    let _one = send_logged(&a, sender.clone(), to, vec![vec![1]], &sent);
    let _two = send_logged(&a, sender, to, vec![vec![2]], &sent);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(*sent.lock().unwrap(), [(after(millis(1)), 2); 2]);
}

#[test]
fn a_datagram_leaves_its_link_after_its_socket_drops() {
    let (mut sim, a, b) = pair(0, rated());
    let log = Log::default();
    let (sender, receiver) = buffered(&a, 4433, 1);
    drop(receiver);
    let (_b, receiver) = udp(&b, 4433);
    let _receive = receive(&b, receiver, &log);
    let _send = send(&a, sender, at(&b, 4433), vec![vec![1; 972]]);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(times(&log), [after(delay()) + millis(1)]);
}

#[test]
fn the_two_directions_of_a_link_send_at_once() {
    let (mut sim, a, b) = pair(0, rated());
    let (logs, mut sockets) = ([Log::default(), Log::default()], Vec::new());
    for ((from, to), log) in [(&a, &b), (&b, &a)].into_iter().zip(&logs) {
        let (receiving, receiver) = udp(to, 4433);
        let (sender, sending) = udp(from, 4434);
        let receive = receive(to, receiver, log);
        let send = send(from, sender, at(to, 4433), vec![vec![1; 972]]);
        sockets.push((receiving, sending, receive, send));
    }
    sim.run_for(Span::SECOND).unwrap();
    for log in &logs {
        assert_eq!(times(log), [after(delay()) + millis(1)]);
    }
}

#[test]
fn a_link_carries_its_rate_over_a_burst_of_small_datagrams() {
    // 56 bytes a nanosecond: ten empty datagrams of 28 header bytes take 5 ns.
    let link = link::Config {
        rate: NonZeroU64::new(56_000_000_000),
        ..link::Config::default()
    };
    let (_sim, log) = exchange(0, link, vec![Vec::new(); 10]);
    let last = after(delay()) + Span::from_nanos(5);
    assert_eq!(times(&log).last(), Some(&last));
}

#[test]
fn a_datagram_over_the_mtu_takes_no_time_on_a_link_with_a_rate() {
    // With its headers, the first is 1,501 bytes.
    let contents = vec![vec![1; 1_473], vec![2; 972]];
    let (_sim, log) = exchange(0, rated(), contents);
    assert_eq!(times(&log), [after(delay()) + millis(1)]);
    assert_eq!(datagrams(&log), [vec![2; 972]]);
}

#[test]
fn a_lost_datagram_takes_its_time_on_a_link_with_a_rate() {
    let lossy = link::Config {
        loss: 1.0,
        ..rated()
    };
    let (mut sim, a, b) = pair(0, lossy);
    let log = Log::default();
    let (sender, _a) = udp(&a, 4433);
    let (_b, receiver) = udp(&b, 4433);
    let _receive = receive(&b, receiver, &log);
    let contents = vec![vec![1; 972], vec![2; 972]];
    let gap = Span::from_nanos(500_000);
    let _send = send_spaced(&a, sender, at(&b, 4433), contents, gap);
    sim.run_for(Span::from_nanos(250_000)).unwrap();
    sim.link(&a, &b, rated());
    sim.run_for(Span::SECOND).unwrap();
    // The first leaves at 1 ms and is lost. The second, sent at 0.5 ms, waits for it.
    assert_eq!(times(&log), [after(delay()) + millis(2)]);
    assert_eq!(datagrams(&log), [vec![2; 972]]);
}

#[test]
fn a_duplicate_takes_no_more_time_on_a_link_with_a_rate() {
    let link = link::Config {
        duplication: 1.0,
        ..rated()
    };
    let (_sim, log) = exchange(0, link, vec![vec![1; 972]]);
    assert_eq!(datagrams(&log), [vec![1; 972], vec![1; 972]]);
    let left = after(delay()) + millis(1);
    assert_eq!(BTreeSet::from_iter(times(&log)), BTreeSet::from([left]));
}

#[test]
fn a_link_whose_rate_goes_still_sends_one_packet_at_a_time() {
    let (mut sim, a, b) = pair(0, rated());
    let log = Log::default();
    let (sender, _a) = udp(&a, 4433);
    let (_b, receiver) = udp(&b, 4433);
    let _receive = receive(&b, receiver, &log);
    let contents = vec![vec![1; 972], vec![2; 972]];
    let gap = Span::from_nanos(500_000);
    let _send = send_spaced(&a, sender, at(&b, 4433), contents.clone(), gap);
    sim.run_for(Span::from_nanos(250_000)).unwrap();
    sim.link(&a, &b, link::Config::default());
    sim.run_for(Span::SECOND).unwrap();
    // The second, sent at 0.5 ms with no rate, leaves when the first has left, at 1 ms.
    assert_eq!(datagrams(&log), contents);
    let left = after(delay()) + millis(1);
    assert_eq!(BTreeSet::from_iter(times(&log)), BTreeSet::from([left]));
}

#[test]
fn a_link_whose_rate_changes_sends_at_the_new_rate_after_the_packets_before() {
    let (mut sim, a, b) = pair(0, rated());
    let log = Log::default();
    let (sender, _a) = udp(&a, 4433);
    let (_b, receiver) = udp(&b, 4433);
    let _receive = receive(&b, receiver, &log);
    let contents = vec![vec![1; 972], vec![2; 972]];
    let gap = Span::from_nanos(500_000);
    let _send = send_spaced(&a, sender, at(&b, 4433), contents, gap);
    sim.run_for(Span::from_nanos(250_000)).unwrap();
    let fast = link::Config {
        rate: NonZeroU64::new(2_000_000),
        ..link::Config::default()
    };
    sim.link(&a, &b, fast);
    sim.run_for(Span::SECOND).unwrap();
    // The second, sent at 0.5 ms, starts when the first has left, at 1 ms.
    let left = |nanos| after(delay()) + Span::from_nanos(nanos);
    assert_eq!(times(&log), [left(1_000_000), left(1_500_000)]);
}

#[test]
fn a_power_cut_drops_the_datagrams_that_have_not_left_the_node() {
    let (mut sim, a, b) = pair(0, rated());
    let log = Log::default();
    let (sender, _a) = udp(&a, 4433);
    let (_b, receiver) = udp(&b, 4433);
    let _receive = receive(&b, receiver, &log);
    let _send = send(&a, sender, at(&b, 4433), vec![vec![1; 972]; 100]);
    sim.run_for(millis(10)).unwrap();
    sim.crash(&a, Crash::Power);
    let (sender, _booted) = udp(&a, 4434);
    let _again = send(&a, sender, at(&b, 4433), vec![vec![2; 972]]);
    sim.run_for(Span::SECOND).unwrap();
    // Ten left by the cut. The one sent after the boot finds the link idle.
    let left = |n| after(delay()) + millis(n);
    assert_eq!(times(&log), (1..=11).map(left).collect::<Vec<_>>());
    assert_eq!(datagrams(&log).last(), Some(&vec![2; 972]));
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
    let make = move |_: &node::Node, _: &node::Node| transmit(nowhere, b"lost");
    sent(|a| at(a, 4433), make, Ok(()));
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
    let (_b, receiver) = queue(&b, 4433, 4 + 768);
    let _send = send(&a, sender, at(&b, 4433), vec![vec![7; 4]; 4]);
    sim.run_for(Span::SECOND).unwrap();
    let log = Log::default();
    let _receive = receive(&b, receiver, &log);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(datagrams(&log), [vec![7; 4], vec![7; 4]]);
}

/// The count of datagrams of `len` bytes that a socket of `b` with a receive queue
/// of `bytes` holds after `a` sends it three.
fn held(len: usize, bytes: usize) -> usize {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let (sender, _a) = udp(&a, 4433);
    let (_b, receiver) = queue(&b, 4433, bytes);
    let _send = send(&a, sender, at(&b, 4433), vec![vec![7; len]; 3]);
    sim.run_for(Span::SECOND).unwrap();
    let log = Log::default();
    let _receive = receive(&b, receiver, &log);
    sim.run_for(Span::SECOND).unwrap();
    let log = log.lock().unwrap();
    (log.iter())
        .map(|(_, meta, _)| meta.len.div_ceil(meta.stride.max(1)).max(1))
        .sum()
}

#[test]
fn each_datagram_takes_768_bytes_of_the_receive_queue_past_its_length() {
    let small = [771, 772, 2 * 772 - 1, 2 * 772].map(|bytes| held(4, bytes));
    let empty = [767, 768].map(|bytes| held(0, bytes));
    assert_eq!((small, empty), ([1, 2, 2, 3], [1, 2]));
}

#[test]
fn a_receive_queue_takes_one_datagram_past_its_bytes() {
    assert_eq!([held(0, 0), held(1_200, 1_500)], [1, 1]);
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
fn a_receive_joins_only_datagrams_of_one_ecn() {
    let (mut sim, a, b) = pair(batched(64), link::Config::default());
    let (mut sender, _a) = udp(&a, 4433);
    let (_b, receiver) = udp(&b, 4433);
    let to = at(&b, 4433);
    let _send = a.shards().start(shard("send"), move |_| async move {
        for ecn in [Some(Ecn::Ect0), Some(Ecn::Ce), None] {
            let transmit = Transmit {
                ecn,
                ..transmit(to, &[1; 10])
            };
            poll_fn(|cx| sender.poll_send(cx, &transmit)).await.unwrap();
        }
    });
    sim.run_for(Span::SECOND).unwrap();
    let log = Log::default();
    let _receive = receive(&b, receiver, &log);
    sim.run_for(Span::SECOND).unwrap();
    let batches: Vec<_> = (log.lock().unwrap().iter())
        .map(|(_, meta, _)| (meta.ecn, meta.len))
        .collect();
    let ecns = [(Some(Ecn::Ect0), 10), (Some(Ecn::Ce), 10), (None, 10)];
    assert_eq!(batches, ecns);
}

#[test]
fn a_receive_frees_its_bytes_in_the_queue() {
    let (mut sim, a, b) = pair(batched(64), link::Config::default());
    let (sender, _a) = udp(&a, 4433);
    let (_b, receiver) = queue(&b, 4433, 2 * (4 + 768) - 1);
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
    let (meta, bytes) = sim
        .run_on(&b, move |_, _| async move { recv(&mut receiver, 2).await })
        .unwrap();
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
fn a_send_to_a_mapped_ipv4_address_goes_to_the_ipv4_address() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let log = Log::default();
    let (sender, _a) = bind(&a, any()).unwrap();
    let (_b, receiver) = udp(&b, 4433);
    let _receive = receive(&b, receiver, &log);
    let _send = send(&a, sender, mapped(at(&b, 4433)), vec![b"v4".to_vec()]);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(arrivals(&log), [(at(&a, 4433), b"v4".to_vec())]);
}

#[test]
fn a_send_from_a_mapped_ipv4_address_goes_from_the_ipv4_address() {
    let make = |a: &node::Node, b: &node::Node| Transmit {
        source: Some(mapped(at(a, 4433)).ip()),
        ..transmit(mapped(at(b, 4433)), b"v4")
    };
    sent(|_| any(), make, Ok(()));
}

#[test]
fn a_socket_on_a_v6_address_cannot_reach_a_mapped_ipv4_address() {
    let remote = mapped(SocketAddr::new(addresses(1)[0], 4433));
    let make = move |_: &node::Node, _: &node::Node| transmit(remote, b"v4");
    sent(v6, make, Err(Net::Unreachable { remote }));
}

#[test]
fn a_v4_socket_cannot_reach_a_mapped_ipv4_address() {
    let remote = mapped(SocketAddr::new(addresses(1)[0], 4433));
    let make = move |_: &node::Node, _: &node::Node| transmit(remote, b"v4");
    sent(|a| at(a, 4433), make, Err(Net::Unreachable { remote }));
}

/// Port 4433 on the unspecified IPv6 address, which receives IPv4 too.
fn any() -> SocketAddr {
    SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 4433)
}

/// Port 4433 on the IPv6 address of `node`.
fn v6(node: &node::Node) -> SocketAddr {
    SocketAddr::new(node.addresses()[1], 4433)
}

/// IPv4 `address` as an IPv4-mapped IPv6 address.
fn mapped(address: SocketAddr) -> SocketAddr {
    let SocketAddr::V4(v4) = address else {
        panic!("{address} is not IPv4");
    };
    SocketAddr::from((v4.ip().to_ipv6_mapped(), v4.port()))
}

/// The source and bytes of each batch in `log`.
fn arrivals(log: &Log) -> Vec<(SocketAddr, Vec<u8>)> {
    let log = log.lock().unwrap();
    (log.iter())
        .map(|(_, meta, bytes)| (meta.source, bytes.clone()))
        .collect()
}

#[test]
fn a_datagram_reaches_only_the_node_of_its_destination() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let any = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 4433);
    let (to_a, to_b) = (Log::default(), Log::default());
    let (sender_a, receiver_a) = bind(&a, any).unwrap();
    let (sender_b, receiver_b) = bind(&b, any).unwrap();
    let _receive_a = receive(&a, receiver_a, &to_a);
    let _receive_b = receive(&b, receiver_b, &to_b);
    let _send_a = send(&a, sender_a, at(&b, 4433), vec![b"to b".to_vec()]);
    let _send_b = send(&b, sender_b, at(&a, 4433), vec![b"to a".to_vec()]);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(arrivals(&to_a), [(at(&b, 4433), b"to a".to_vec())]);
    assert_eq!(arrivals(&to_b), [(at(&a, 4433), b"to b".to_vec())]);
}

#[test]
fn a_node_sends_to_itself_over_the_default_link() {
    let (mut sim, a, _b) = pair(0, link::Config::default());
    let log = Log::default();
    let (sender, _one) = udp(&a, 4433);
    let (_two, receiver) = udp(&a, 4434);
    let _receive = receive(&a, receiver, &log);
    let _send = send(&a, sender, at(&a, 4434), vec![b"self".to_vec()]);
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(times(&log), [after(delay())]);
    assert_eq!(arrivals(&log), [(at(&a, 4433), b"self".to_vec())]);
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

/// Sends once from a socket of `a` on the address that `local` gives for `a`, with
/// the transmit that `make` gives for `a` and `b`, and checks that the send gives
/// `expected`.
fn sent(
    local: fn(&node::Node) -> SocketAddr,
    make: impl FnOnce(&node::Node, &node::Node) -> Transmit<'static>,
    expected: Result<(), Net>,
) {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let (mut sender, _a) = bind(&a, local(&a)).unwrap();
    let transmit = make(&a, &b);
    let sent = sim.run_on(&a, move |_, _| async move {
        poll_fn(|cx| sender.poll_send(cx, &transmit)).await
    });
    assert_eq!(sent, Ok(expected));
}

#[test]
fn a_v4_socket_cannot_reach_a_v6_address() {
    let ip = Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 2);
    let remote = SocketAddr::V6(SocketAddrV6::new(ip, 4433, 7, 3));
    let make = move |_: &node::Node, _: &node::Node| transmit(remote, b"v6");
    sent(|a| at(a, 4433), make, Err(Net::Unreachable { remote }));
}

#[test]
fn a_send_from_an_address_of_another_node_fails() {
    let spoof = |_: &node::Node, b: &node::Node| Transmit {
        source: Some(b.addresses()[0]),
        ..transmit(at(b, 4433), b"spoof")
    };
    sent(|a| at(a, 4433), spoof, Err(Net::Io { code: 99 }));
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

#[test]
fn a_link_with_no_rate_keeps_the_digest_that_it_had_before_rates() {
    // `DefaultHasher` makes the digest, so a new toolchain can change this value.
    assert_eq!(digest(3), 15_790_775_888_564_560_557);
}

/// The digest of a run that sends `contents` twice from `a` over a link with `loss`
/// to a socket of `b` with a receive queue of `bytes`.
fn traced(contents: &[u8], loss: f64, bytes: usize) -> u64 {
    let link = link::Config {
        loss,
        ..link::Config::default()
    };
    let (mut sim, a, b) = pair(0, link);
    let (sender, _a) = udp(&a, 4433);
    let (_b, _receiver) = queue(&b, 4433, bytes);
    let _send = send(&a, sender, at(&b, 4433), vec![contents.to_vec(); 2]);
    sim.run_for(Span::SECOND).unwrap();
    sim.digest()
}

#[test]
fn the_digest_holds_each_send() {
    assert_ne!(traced(b"ab", 1.0, 1 << 20), traced(b"abc", 1.0, 1 << 20));
}

#[test]
fn the_digest_holds_the_fate_of_each_arrival() {
    assert_ne!(traced(b"abc", 0.0, 1 << 20), traced(b"abc", 0.0, 2));
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

#[test]
fn a_socket_half_polled_on_a_second_thread_panics() {
    let message = "a socket half polls only on thread \"first\" of its first poll";
    assert_eq!(stray(send_once), panicked("second", message));
    assert_eq!(stray(recv_once), panicked("second", message));
}

/// Polls the halves of a socket of `a` with `poll` on a shard of `b`, and gives the
/// error of the run.
fn foreign(poll: fn(&mut Sender, &mut Receiver)) -> Error {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let (mut sender, mut receiver) = udp(&a, 4433);
    let _b = b.shards().start(shard("b"), move |_| async move {
        poll(&mut sender, &mut receiver);
    });
    sim.run().unwrap_err()
}

#[test]
fn a_socket_polled_on_a_thread_of_another_node_panics() {
    let message = "a socket half of node 0 runs on a thread of node 1";
    assert_eq!(foreign(send_once), panicked("b", message));
    assert_eq!(foreign(recv_once), panicked("b", message));
}

/// Polls the halves of a socket of `a` with `poll` on a shard of `a` after a crash
/// of `a`, with a sender clone made after the crash when `cloned`, and gives the
/// error of the run.
fn crashed(poll: fn(&mut Sender, &mut Receiver), cloned: bool) -> Error {
    let (mut sim, a, _b) = pair(0, link::Config::default());
    let (mut sender, mut receiver) = udp(&a, 4433);
    sim.crash(&a, Crash::Process);
    if cloned {
        sender = sender.clone();
    }
    let _after = a.shards().start(shard("after"), move |_| async move {
        poll(&mut sender, &mut receiver);
    });
    sim.run().unwrap_err()
}

#[test]
fn a_socket_half_from_before_a_crash_panics_when_it_polls() {
    let message = "a socket half of node 0 polls after a crash of the node";
    assert_eq!(crashed(send_once, false), panicked("after", message));
    assert_eq!(crashed(send_once, true), panicked("after", message));
    assert_eq!(crashed(recv_once, false), panicked("after", message));
}

/// Polls the halves of a socket with `poll` on a thread that the sim did not start.
fn outside(poll: fn(&mut Sender, &mut Receiver)) {
    let (_sim, a, _b) = pair(0, link::Config::default());
    let (mut sender, mut receiver) = udp(&a, 4433);
    poll(&mut sender, &mut receiver);
}

#[test]
#[should_panic(expected = "a socket half needs a thread that the sim started")]
fn a_socket_sending_outside_the_sim_panics() {
    outside(send_once);
}

#[test]
#[should_panic(expected = "a socket half needs a thread that the sim started")]
fn a_socket_receiving_outside_the_sim_panics() {
    outside(recv_once);
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

/// The digest of a run in which a shard of `a` sends one datagram to `b` and yields
/// once: before the send when `late`, after it otherwise.
fn sent_in_poll(late: bool) -> u64 {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let (mut sender, _a) = udp(&a, 4433);
    let _b = udp(&b, 4433);
    let to = at(&b, 4433);
    let _send = a.shards().start(shard("send"), move |_| async move {
        if late {
            yield_now().await;
        }
        let transmit = transmit(to, b"x");
        poll_fn(|cx| sender.poll_send(cx, &transmit)).await.unwrap();
        if !late {
            yield_now().await;
        }
    });
    sim.run_for(Span::SECOND).unwrap();
    sim.digest()
}

#[test]
fn the_digest_holds_the_poll_of_each_send() {
    assert_ne!(sent_in_poll(false), sent_in_poll(true));
}

/// The digest of a run in which `a` sends one datagram to `b`, which arrives while a
/// shard of `b` sleeps: before its second yield when `early`, after it otherwise.
fn polled_around_arrival(early: bool) -> u64 {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let (sender, _a) = udp(&a, 4433);
    let _b = udp(&b, 4433);
    let _send = send(&a, sender, at(&b, 4433), vec![b"x".to_vec()]);
    let clock = b.clock();
    let _wait = b.shards().start(shard("wait"), move |_| async move {
        yield_now().await;
        if early {
            clock.sleep(Span::MILLISECOND).await;
        }
        yield_now().await;
        if !early {
            clock.sleep(Span::MILLISECOND).await;
        }
    });
    sim.run_for(Span::SECOND).unwrap();
    sim.digest()
}

#[test]
fn the_digest_holds_the_polls_before_an_arrival() {
    assert_ne!(polled_around_arrival(true), polled_around_arrival(false));
}

/// The error of a receive of a failed socket.
const EIO: Net = Net::Io { code: 5 };

/// The results of receives into a buffer of 8 bytes.
type Results = Arc<Mutex<Vec<Result<usize, Net>>>>;

/// Starts a shard on `node` that receives from `receiver` into `results` three
/// times, whatever each result.
fn receive_three(
    node: &node::Node,
    mut receiver: Receiver,
    results: &Results,
) -> Handle {
    let results = Arc::clone(results);
    let handle = node.shards().start(shard("receive"), move |_| async move {
        for _ in 0..3 {
            let (mut bytes, mut meta) = ([0; 8], [Meta::default()]);
            let result = poll_fn(|cx| {
                let mut buffers = [IoSliceMut::new(&mut bytes)];
                receiver.poll_recv(cx, &mut buffers, &mut meta)
            })
            .await;
            results.lock().unwrap().push(result);
        }
    });
    handle.unwrap()
}

/// Fails the socket of `b` on port 4433 `faults` times after three datagrams from
/// `a` arrive at it, then gives the results of its receives.
fn results_after(faults: usize) -> Vec<Result<usize, Net>> {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let (sender, _a) = udp(&a, 4433);
    let (_b, receiver) = udp(&b, 4433);
    let _send = send(&a, sender, at(&b, 4433), numbered(3));
    sim.run_for(Span::SECOND).unwrap();
    for _ in 0..faults {
        b.fail_udp(at(&b, 4433));
    }
    let results = Results::default();
    let _receive = receive_three(&b, receiver, &results);
    sim.run_for(Span::SECOND).unwrap();
    results.lock().unwrap().clone()
}

#[test]
fn each_receive_of_a_failed_socket_gives_eio_and_never_its_queue() {
    assert_eq!(results_after(0).first(), Some(&Ok(1)));
    assert_eq!(results_after(1), [Err(EIO), Err(EIO), Err(EIO)]);
}

#[test]
fn a_second_fault_does_nothing() {
    assert_eq!(results_after(2), [Err(EIO), Err(EIO), Err(EIO)]);
}

#[test]
fn a_receive_that_waits_wakes_with_the_fault() {
    let (mut sim, _a, b) = pair(0, link::Config::default());
    let (_b, receiver) = udp(&b, 4433);
    let results = Results::default();
    let _receive = receive_three(&b, receiver, &results);
    sim.run_for(millis(10)).unwrap();
    b.fail_udp(at(&b, 4433));
    sim.run_for(millis(10)).unwrap();
    assert_eq!(*results.lock().unwrap(), [Err(EIO), Err(EIO), Err(EIO)]);
}

#[test]
fn a_send_of_a_failed_socket_still_arrives() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let (sender, _a) = udp(&a, 4433);
    a.fail_udp(at(&a, 4433));
    let (_b, receiver) = udp(&b, 4433);
    let log = Log::default();
    let _receive = receive(&b, receiver, &log);
    let _send = send(&a, sender, at(&b, 4433), numbered(2));
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(datagrams(&log), numbered(2));
}

#[test]
fn a_socket_bound_after_a_failed_socket_drops_works() {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let (sender, _a) = udp(&a, 4433);
    let socket = udp(&b, 4433);
    b.fail_udp(at(&b, 4433));
    drop(socket);
    let (_b, receiver) = udp(&b, 4433);
    let log = Log::default();
    let _receive = receive(&b, receiver, &log);
    let _send = send(&a, sender, at(&b, 4433), numbered(2));
    sim.run_for(Span::SECOND).unwrap();
    assert_eq!(datagrams(&log), numbered(2));
}

/// The digest of a run that sends two datagrams from `a` to port 4433 of `b`, where
/// `b` binds a socket that nothing reads at port `bound`, and fails it first when
/// `faulted`.
fn arrivals_digest(bound: u16, faulted: bool) -> u64 {
    let (mut sim, a, b) = pair(0, link::Config::default());
    let (sender, _a) = udp(&a, 4433);
    let _b = udp(&b, bound);
    if faulted {
        b.fail_udp(at(&b, bound));
    }
    let _send = send(&a, sender, at(&b, 4433), numbered(2));
    sim.run_for(Span::SECOND).unwrap();
    sim.digest()
}

#[test]
fn the_datagrams_that_arrive_at_a_failed_socket_drop_as_where_none_is_bound() {
    let dropped = arrivals_digest(4434, false);
    assert_eq!(arrivals_digest(4433, true), dropped);
    assert_ne!(arrivals_digest(4433, false), dropped);
}

#[test]
#[should_panic(expected = "no UDP socket of node 0 is bound at 10.0.0.1:4433")]
fn a_fault_where_no_socket_is_bound_panics() {
    let (_sim, a, _b) = pair(0, link::Config::default());
    a.fail_udp(at(&a, 4433));
}

#[test]
#[should_panic(expected = "no UDP socket of node 0 is bound at 10.0.0.2:4433")]
fn a_fault_on_a_socket_of_another_node_panics() {
    let (_sim, a, b) = pair(0, link::Config::default());
    let _b = udp(&b, 4433);
    a.fail_udp(at(&b, 4433));
}
