//! The UDP sockets of `os::net` on the loopback.

use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use env::net::udp::{self, Meta, Receiver, Sender, Transmit};
use env::net::{Ecn, Error, Net};
use tokio::sync::Notify;
use tokio::time::timeout;

#[cfg(target_os = "linux")]
#[path = "../../common/gso.rs"]
mod gso;

use super::{
    BOUND, Counted, LOCALHOST, assert_joins, net, on_thread, runtime,
    runtime_with_no_io,
};

/// How long a receive waits before it takes that no more datagrams come.
const SILENCE: Duration = Duration::from_millis(100);
/// The largest datagram these tests receive.
const DATAGRAM_BYTES_MAX: usize = 2048;

fn config(local: SocketAddr) -> udp::Config {
    udp::Config {
        local,
        send_buffer_bytes: 1 << 20,
        recv_buffer_bytes: 1 << 20,
    }
}

fn bind(net: &Net, local: SocketAddr) -> (Sender, Receiver) {
    net.udp(&config(local)).expect("the address is free")
}

/// Binds a free port of the IPv4 loopback.
fn loopback(net: &Net) -> (Sender, Receiver) {
    bind(net, SocketAddr::new(LOCALHOST.into(), 0))
}

fn transmit(destination: SocketAddr, contents: &[u8]) -> Transmit<'_> {
    Transmit {
        destination,
        source: None,
        ecn: None,
        contents,
        segment: None,
    }
}

async fn send(sender: &mut Sender, transmit: &Transmit<'_>) -> Result<(), Error> {
    poll_fn(|cx| sender.poll_send(cx, transmit)).await
}

/// One received datagram.
#[derive(Debug, PartialEq, Eq)]
struct Datagram {
    source: SocketAddr,
    destination: Option<IpAddr>,
    ecn: Option<Ecn>,
    contents: Vec<u8>,
}

/// Receives until `count` datagrams arrive, each split from its batch.
async fn receive(receiver: &mut Receiver, count: usize) -> Vec<Datagram> {
    let batch_bytes = receiver.batch_max().get() * DATAGRAM_BYTES_MAX;
    let mut storage = vec![vec![0; batch_bytes]; 4];
    let mut datagrams = Vec::new();
    while datagrams.len() < count {
        let mut buffers: Vec<IoSliceMut<'_>> =
            storage.iter_mut().map(|b| IoSliceMut::new(b)).collect();
        let mut meta = [Meta::default(); 4];
        let batches = timeout(
            BOUND,
            poll_fn(|cx| receiver.poll_recv(cx, &mut buffers, &mut meta)),
        )
        .await
        .expect("the datagrams arrive")
        .expect("the receive succeeds");
        for (meta, buffer) in meta.iter().zip(&buffers).take(batches) {
            for contents in buffer[..meta.len].chunks(meta.stride) {
                datagrams.push(Datagram {
                    source: meta.source,
                    destination: meta.destination,
                    ecn: meta.ecn,
                    contents: contents.to_vec(),
                });
            }
        }
    }
    datagrams
}

#[test]
fn a_round_trip_gives_the_bytes_and_both_addresses() {
    on_thread("udp-round-trip", || async {
        let net = net();
        let (mut sender, _) = loopback(&net);
        let (_, mut receiver) = loopback(&net);
        let to = transmit(receiver.local(), b"telemetry");
        assert_eq!(send(&mut sender, &to).await, Ok(()));
        let datagrams = receive(&mut receiver, 1).await;
        let expected = Datagram {
            source: sender.local(),
            destination: Some(LOCALHOST.into()),
            ecn: None,
            contents: b"telemetry".to_vec(),
        };
        assert_eq!(datagrams, [expected]);
    });
}

#[test]
fn a_full_batch_arrives_in_order() {
    on_thread("udp-batch", || async {
        let net = net();
        let (mut sender, _) = loopback(&net);
        let (_, mut receiver) = loopback(&net);
        let count = sender.batch_max().get();
        let contents: Vec<u8> = (0..=u8::MAX).cycle().take(count * 100).collect();
        let to = Transmit {
            segment: NonZeroUsize::new(100),
            ..transmit(receiver.local(), &contents)
        };
        assert_eq!(send(&mut sender, &to).await, Ok(()));
        let datagrams = receive(&mut receiver, count).await;
        let arrived: Vec<&[u8]> = datagrams.iter().map(|d| &d.contents[..]).collect();
        let sent: Vec<&[u8]> = contents.chunks(100).collect();
        assert_eq!(arrived, sent);
    });
}

/// Only Linux sends more than one datagram in a batch, with GSO.
#[test]
#[cfg(target_os = "linux")]
fn a_batch_with_a_short_last_datagram_arrives_whole() {
    on_thread("udp-short", || async {
        let net = net();
        let (mut sender, _) = loopback(&net);
        let (_, mut receiver) = loopback(&net);
        let contents: Vec<u8> = (0..250).collect();
        let to = Transmit {
            segment: NonZeroUsize::new(100),
            ..transmit(receiver.local(), &contents)
        };
        assert_eq!(send(&mut sender, &to).await, Ok(()));
        let datagrams = receive(&mut receiver, 3).await;
        let arrived: Vec<&[u8]> = datagrams.iter().map(|d| &d.contents[..]).collect();
        let sent: Vec<&[u8]> = contents.chunks(100).collect();
        assert_eq!(arrived, sent);
    });
}

/// A batch goes out in one call with GSO, and the receiver takes it back with GRO.
#[test]
#[cfg(target_os = "linux")]
fn a_batch_arrives_as_one_batch() {
    on_thread("udp-gso", || async {
        let net = net();
        let (mut sender, _) = loopback(&net);
        let (_, mut receiver) = loopback(&net);
        let contents = [5; 300];
        let to = Transmit {
            ecn: Some(Ecn::Ce),
            segment: NonZeroUsize::new(100),
            ..transmit(receiver.local(), &contents)
        };
        assert_eq!(send(&mut sender, &to).await, Ok(()));
        let mut buffer = vec![0; receiver.batch_max().get() * DATAGRAM_BYTES_MAX];
        let mut meta = [Meta::default()];
        let mut buffers = [IoSliceMut::new(&mut buffer)];
        let batches = timeout(
            BOUND,
            poll_fn(|cx| receiver.poll_recv(cx, &mut buffers, &mut meta)),
        )
        .await;
        assert_eq!(batches.expect("the batch arrives"), Ok(1));
        let [meta] = meta;
        assert_eq!((meta.len, meta.stride, meta.ecn), (300, 100, Some(Ecn::Ce)));
    });
}

/// Receives one batch, and gives its length, stride, and ECN mark.
#[cfg(target_os = "linux")]
async fn receive_batch(receiver: &mut Receiver) -> (usize, usize, Option<Ecn>) {
    let mut buffer = vec![0; receiver.batch_max().get() * DATAGRAM_BYTES_MAX];
    let mut meta = [Meta::default()];
    let mut buffers = [IoSliceMut::new(&mut buffer)];
    let batches = timeout(
        BOUND,
        poll_fn(|cx| receiver.poll_recv(cx, &mut buffers, &mut meta)),
    )
    .await;
    assert_eq!(batches.expect("the batch arrives"), Ok(1));
    let [meta] = meta;
    (meta.len, meta.stride, meta.ecn)
}

/// A transmit that the kernel refuses leaves GSO and the IPv4 ECN mark on.
#[test]
#[cfg(target_os = "linux")]
fn a_refused_transmit_leaves_gso_and_ecn_on() {
    on_thread("udp-refused", || async {
        let net = net();
        let any_v6 = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0);
        let to_v6 = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 9);
        let far_v6 = IpAddr::from([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1]);
        let cases = [
            (Some(far_v6), to_v6),
            (None, SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 0)),
            (None, SocketAddr::new(LOCALHOST.into(), 0)),
            (Some(LOCALHOST.into()), to_v6),
        ];
        let (_, mut receiver) = loopback(&net);
        let contents = [5; 300];
        let batch = Transmit {
            ecn: Some(Ecn::Ce),
            segment: NonZeroUsize::new(100),
            ..transmit(receiver.local(), &contents)
        };
        for (source, destination) in cases {
            for segment in [None, NonZeroUsize::new(1)] {
                let (mut sender, _) = bind(&net, any_v6);
                let refused = Transmit {
                    source,
                    segment,
                    ..transmit(destination, b"xy")
                };
                let case = format!("source {source:?}, to {destination}, {segment:?}");
                assert_eq!(
                    send(&mut sender, &refused).await,
                    Err(Error::Io { code: 22 }),
                    "{case}"
                );
                assert_eq!(send(&mut sender, &batch).await, Ok(()));
                assert_eq!(
                    receive_batch(&mut receiver).await,
                    (300, 100, Some(Ecn::Ce)),
                    "after {case}"
                );
            }
        }
    });
}

/// A kernel that refuses GSO still gets each datagram of a batch, with its ECN mark.
#[test]
#[cfg(target_os = "linux")]
fn a_batch_arrives_when_the_kernel_refuses_gso() {
    on_thread("udp-no-gso", || async {
        let net = net();
        let (mut sender, _) = loopback(&net);
        gso::refuse(sender.local());
        let (_, mut receiver) = loopback(&net);
        let contents: Vec<u8> = (0..3).flat_map(|i| [i; 100]).collect();
        let batch = Transmit {
            ecn: Some(Ecn::Ce),
            segment: NonZeroUsize::new(100),
            ..transmit(receiver.local(), &contents)
        };
        let expected: Vec<_> = contents
            .chunks(100)
            .map(|c| (c.to_vec(), Some(Ecn::Ce)))
            .collect();
        for round in ["first", "second"] {
            assert_eq!(send(&mut sender, &batch).await, Ok(()), "{round} batch");
            let datagrams: Vec<_> = receive(&mut receiver, 3)
                .await
                .into_iter()
                .map(|d| (d.contents, d.ecn))
                .collect();
            assert_eq!(datagrams, expected, "{round} batch");
        }
    });
}

/// macOS loses it: `a_datagram_over_the_path_mtu_is_lost`.
#[test]
#[cfg(target_os = "linux")]
fn a_datagram_of_the_byte_max_arrives() {
    on_thread("udp-max", || async {
        let net = net();
        let (mut sender, _) = loopback(&net);
        let (_, mut receiver) = loopback(&net);
        let contents = vec![7; udp::TRANSMIT_BYTES_MAX];
        assert_eq!(
            send(&mut sender, &transmit(receiver.local(), &contents)).await,
            Ok(())
        );
        let mut buffer = vec![0; udp::TRANSMIT_BYTES_MAX + 1];
        let mut buffers = [IoSliceMut::new(&mut buffer)];
        let mut meta = [Meta::default()];
        let batches = poll_fn(|cx| receiver.poll_recv(cx, &mut buffers, &mut meta));
        assert_eq!(batches.await, Ok(1));
        assert_eq!(meta[0].len, udp::TRANSMIT_BYTES_MAX);
        assert_eq!(&buffer[..udp::TRANSMIT_BYTES_MAX], &contents[..]);
    });
}

/// Linux loopback takes each datagram up to the byte max, and macOS loopback has an
/// MTU of 16,384 bytes.
#[test]
#[cfg(target_os = "macos")]
fn a_datagram_over_the_path_mtu_is_lost() {
    on_thread("udp-mtu", || async {
        let net = net();
        let (mut sender, _) = loopback(&net);
        let (_, mut receiver) = loopback(&net);
        let big = vec![1; 20_000];
        assert_eq!(
            send(&mut sender, &transmit(receiver.local(), &big)).await,
            Ok(())
        );
        let to = transmit(receiver.local(), b"after");
        assert_eq!(send(&mut sender, &to).await, Ok(()));
        let datagrams = receive(&mut receiver, 1).await;
        assert_eq!(datagrams[0].contents, b"after");
    });
}

#[test]
fn a_datagram_past_the_end_of_its_buffer_arrives_cut() {
    on_thread("udp-cut", || async {
        let net = net();
        let (mut sender, _) = loopback(&net);
        let (_, mut receiver) = loopback(&net);
        let contents: Vec<u8> = (0..100).collect();
        let to = transmit(receiver.local(), &contents);
        assert_eq!(send(&mut sender, &to).await, Ok(()));
        tokio::time::sleep(SILENCE).await;
        let mut buffer = [0; 10];
        let mut buffers = [IoSliceMut::new(&mut buffer)];
        let mut meta = [Meta::default()];
        let batches = timeout(
            Duration::from_secs(2),
            poll_fn(|cx| receiver.poll_recv(cx, &mut buffers, &mut meta)),
        )
        .await;
        assert_eq!(batches, Ok(Ok(1)));
        assert_eq!(meta[0].len, 10);
        assert_eq!(buffer, contents[..10]);
    });
}

#[test]
fn an_ecn_mark_arrives() {
    on_thread("udp-ecn", || async {
        let net = net();
        for ip in [IpAddr::from(LOCALHOST), Ipv6Addr::LOCALHOST.into()] {
            let (mut sender, _) = bind(&net, SocketAddr::new(ip, 0));
            let (_, mut receiver) = bind(&net, SocketAddr::new(ip, 0));
            for ecn in [Ecn::Ect0, Ecn::Ect1, Ecn::Ce] {
                let to = Transmit {
                    ecn: Some(ecn),
                    ..transmit(receiver.local(), b"marked")
                };
                assert_eq!(send(&mut sender, &to).await, Ok(()));
                let marked = receive(&mut receiver, 1).await[0].ecn;
                assert_eq!(marked, Some(ecn), "{ip}");
            }
        }
    });
}

#[test]
fn an_any_v6_socket_talks_plain_ipv4() {
    on_thread("udp-dual", || async {
        let net = net();
        let (mut any_sender, mut any) =
            bind(&net, SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0));
        let (mut v4_sender, mut v4) = loopback(&net);
        let to_any = SocketAddr::new(LOCALHOST.into(), any.local().port());
        assert_eq!(send(&mut v4_sender, &transmit(to_any, b"in")).await, Ok(()));
        let datagrams = receive(&mut any, 1).await;
        assert_eq!(datagrams[0].source, v4.local());
        assert_eq!(datagrams[0].destination, Some(LOCALHOST.into()));
        let mapped =
            SocketAddr::new(LOCALHOST.to_ipv6_mapped().into(), v4.local().port());
        for destination in [v4.local(), mapped] {
            let to = Transmit {
                ecn: Some(Ecn::Ect0),
                ..transmit(destination, b"out")
            };
            assert_eq!(send(&mut any_sender, &to).await, Ok(()));
            let datagrams = receive(&mut v4, 1).await;
            assert_eq!(datagrams[0].source, to_any);
            assert_eq!(datagrams[0].ecn, Some(Ecn::Ect0));
        }
    });
}

/// Linux holds all of 127.0.0.0/8 on the loopback, and macOS holds only 127.0.0.1.
#[test]
#[cfg(target_os = "linux")]
fn a_source_address_picks_the_local_address() {
    on_thread("udp-source", || async {
        let net = net();
        let (mut sender, _) =
            bind(&net, SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0));
        let (_, mut receiver) = bind(&net, SocketAddr::new(LOCALHOST.into(), 0));
        let other = Ipv4Addr::new(127, 0, 0, 2);
        let to = Transmit {
            source: Some(other.into()),
            ..transmit(receiver.local(), b"from")
        };
        assert_eq!(send(&mut sender, &to).await, Ok(()));
        let datagrams = receive(&mut receiver, 1).await;
        let port = sender.local().port();
        assert_eq!(datagrams[0].source, SocketAddr::new(other.into(), port));
    });
}

/// Linux wakes each writable registration of a socket for each datagram that any
/// descriptor of it sends, so a sender that keeps one wakes its parked thread.
#[test]
fn the_sends_of_a_clone_wake_no_parked_sender() {
    let net = net();
    let (mut parked, receiver) = loopback(&net);
    let mut busy = parked.clone();
    let to = receiver.local();
    let unparks = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&unparks);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .on_thread_unpark(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })
        .build()
        .expect("a current-thread runtime builds");
    runtime.block_on(async {
        assert_eq!(send(&mut parked, &transmit(to, b"first")).await, Ok(()));
        let before = unparks.load(Ordering::SeqCst);
        let done = Arc::new(Notify::new());
        let sent = Arc::clone(&done);
        let handle = os::threads()
            .expect("the OS gives the cores of this process")
            .start("udp-busy", move || async move {
                for _ in 0..200 {
                    assert_eq!(send(&mut busy, &transmit(to, b"busy")).await, Ok(()));
                }
                sent.notify_one();
            })
            .expect("the thread starts");
        done.notified().await;
        let wakes = unparks.load(Ordering::SeqCst) - before;
        assert!(wakes <= 2, "the parked sender woke {wakes} times");
        assert_joins(handle, Ok(()));
    });
}

#[test]
fn a_v4_socket_cannot_reach_ipv6() {
    on_thread("udp-v4-v6", || async {
        let net = net();
        let (mut sender, _) = loopback(&net);
        let port = sender.local().port();
        let mapped = SocketAddr::new(LOCALHOST.to_ipv6_mapped().into(), port);
        let v6 = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port);
        for remote in [mapped, v6] {
            assert_eq!(
                send(&mut sender, &transmit(remote, b"x")).await,
                Err(Error::Unreachable { remote })
            );
        }
    });
}

#[test]
fn a_v6_socket_on_a_specific_address_cannot_reach_ipv4() {
    on_thread("udp-v6-v4", || async {
        let net = net();
        let (mut sender, _) =
            bind(&net, SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 0));
        let port = sender.local().port();
        let mapped = SocketAddr::new(LOCALHOST.to_ipv6_mapped().into(), port);
        let v4 = SocketAddr::new(LOCALHOST.into(), port);
        for remote in [mapped, v4] {
            assert_eq!(
                send(&mut sender, &transmit(remote, b"x")).await,
                Err(Error::Unreachable { remote })
            );
        }
    });
}

/// ENV SEAMS: `os` gives the kernel's answer for a source that is not local or is of
/// the other family, and for port 0.
#[cfg(target_os = "linux")]
#[test]
fn a_bad_source_or_port_0_gives_the_answer_of_linux() {
    on_thread("udp-bad-source", || async {
        let net = net();
        let v4 = SocketAddr::new(LOCALHOST.into(), 0);
        let any_v6 = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0);
        let to_v4 = SocketAddr::new(LOCALHOST.into(), 9);
        let to_v6 = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 9);
        let far_v4 = IpAddr::from([192, 0, 2, 1]);
        let far_v6 = IpAddr::from([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1]);
        let multicast = IpAddr::from([224, 0, 0, 1]);
        let broadcast = IpAddr::from([255, 255, 255, 255]);
        let invalid = Err(Error::Io { code: 22 });
        let cases = [
            (
                v4,
                Some(far_v4),
                to_v4,
                Err(Error::Unreachable { remote: to_v4 }),
            ),
            (
                v4,
                None,
                SocketAddr::new(LOCALHOST.into(), 0),
                invalid.clone(),
            ),
            (v4, Some(multicast), to_v4, invalid.clone()),
            (v4, Some(broadcast), to_v4, invalid.clone()),
            (any_v6, Some(far_v6), to_v6, invalid.clone()),
            (any_v6, Some(multicast), to_v4, invalid.clone()),
            (any_v6, Some(broadcast), to_v4, invalid.clone()),
            (
                any_v6,
                Some(far_v4),
                to_v4,
                Err(Error::Unreachable { remote: to_v4 }),
            ),
            (
                any_v6,
                Some(Ipv6Addr::LOCALHOST.into()),
                to_v4,
                invalid.clone(),
            ),
            (any_v6, Some(LOCALHOST.into()), to_v6, invalid.clone()),
            (
                any_v6,
                None,
                SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 0),
                invalid.clone(),
            ),
        ];
        for (local, source, destination, expected) in cases {
            let (mut sender, _) = bind(&net, local);
            let to = Transmit {
                source,
                ..transmit(destination, b"x")
            };
            assert_eq!(
                send(&mut sender, &to).await,
                expected,
                "a socket on {local}, source {source:?}, to {destination}"
            );
        }
    });
}

/// ENV SEAMS: `os` gives the kernel's answer for a source that is not local.
#[cfg(target_os = "macos")]
#[test]
fn a_source_that_is_not_local_gives_the_answer_of_macos() {
    on_thread("udp-far-source", || async {
        let net = net();
        let any_v4 = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0);
        let any_v6 = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0);
        let to_v4 = SocketAddr::new(LOCALHOST.into(), 9);
        let to_v6 = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 9);
        let cases = [
            (any_v4, IpAddr::from([192, 0, 2, 1]), to_v4),
            (any_v4, IpAddr::from([127, 0, 0, 2]), to_v4),
            (any_v6, IpAddr::from([192, 0, 2, 1]), to_v4),
            (
                any_v6,
                IpAddr::from([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1]),
                to_v6,
            ),
        ];
        for (local, source, destination) in cases {
            let (mut sender, _) = bind(&net, local);
            let to = Transmit {
                source: Some(source),
                ..transmit(destination, b"x")
            };
            assert_eq!(
                send(&mut sender, &to).await,
                Err(Error::Io { code: 49 }),
                "a socket on {local}, source {source}"
            );
        }
    });
}

/// The OS drops each datagram that the receive buffer has no room for. A receiver
/// with the default buffer of Linux holds about 90 of these. macOS delivers on the
/// loopback from a queue, and a receive during delivery makes room for more, so the
/// test sleeps before it receives.
#[test]
fn a_small_receive_buffer_holds_few_datagrams() {
    on_thread("udp-buffer", || async {
        let net = net();
        let config = udp::Config {
            recv_buffer_bytes: 1 << 12,
            ..config(SocketAddr::new(LOCALHOST.into(), 0))
        };
        let (_, mut receiver) = net.udp(&config).expect("the address is free");
        let (mut sender, _) = loopback(&net);
        let to = transmit(receiver.local(), &[7; 1_000]);
        for _ in 0..200 {
            assert_eq!(send(&mut sender, &to).await, Ok(()));
        }
        tokio::time::sleep(SILENCE).await;
        let mut held = receive(&mut receiver, 1).await.len();
        while let Ok(datagrams) = timeout(SILENCE, receive(&mut receiver, 1)).await {
            held += datagrams.len();
        }
        assert!(
            (1..20).contains(&held),
            "the receiver held {held} datagrams"
        );
    });
}

#[test]
fn a_receive_with_no_datagram_is_pending_until_one_arrives() {
    on_thread("udp-pending", || async {
        let net = net();
        let (mut sender, _) = loopback(&net);
        let (_, mut receiver) = loopback(&net);
        let counted = Arc::new(Counted {
            wakes: AtomicUsize::new(0),
            woken: Notify::new(),
        });
        let waker = Waker::from(Arc::clone(&counted));
        let mut cx = Context::from_waker(&waker);
        let mut buffer = [0; 8];
        let mut buffers = [IoSliceMut::new(&mut buffer)];
        let mut meta = [Meta::default()];
        let poll = receiver.poll_recv(&mut cx, &mut buffers, &mut meta);
        assert_eq!(poll, Poll::Pending);
        assert_eq!(counted.wakes.load(Ordering::SeqCst), 0);
        let to = transmit(receiver.local(), b"wake");
        assert_eq!(send(&mut sender, &to).await, Ok(()));
        timeout(BOUND, counted.woken.notified())
            .await
            .expect("the datagram wakes the receiver");
        let poll = receiver.poll_recv(&mut cx, &mut buffers, &mut meta);
        assert_eq!(poll, Poll::Ready(Ok(1)));
        assert_eq!(&buffer[..4], b"wake");
    });
}

#[test]
fn a_send_to_a_closed_port_leaves_both_halves_working() {
    on_thread("udp-closed", || async {
        let net = net();
        let (mut sender, mut receiver) = loopback(&net);
        let closed = {
            let (gone, _) = loopback(&net);
            gone.local()
        };
        assert_eq!(send(&mut sender, &transmit(closed, b"lost")).await, Ok(()));
        let to = transmit(receiver.local(), b"kept");
        assert_eq!(send(&mut sender, &to).await, Ok(()));
        assert_eq!(receive(&mut receiver, 1).await[0].contents, b"kept");
    });
}

#[test]
fn port_zero_binds_a_free_port() {
    let net = net();
    let (sender, receiver) = loopback(&net);
    assert_eq!(sender.local().ip(), LOCALHOST);
    assert_ne!(sender.local().port(), 0);
    assert_eq!(receiver.local(), sender.local());
}

#[test]
fn a_bind_to_a_held_address_is_in_use() {
    let net = net();
    let (held, _) = loopback(&net);
    let local = held.local();
    assert_eq!(
        net.udp(&config(local)).map(drop),
        Err(Error::AddressInUse { local })
    );
}

#[test]
fn clones_send_from_two_threads() {
    let net = net();
    let (sender, mut receiver) = loopback(&net);
    let to = receiver.local();
    let other = sender.clone();
    for (name, mut sender) in [("udp-one", sender), ("udp-two", other)] {
        on_thread(name, move || async move {
            assert_eq!(send(&mut sender, &transmit(to, b"clone")).await, Ok(()));
        });
    }
    let datagrams = runtime().block_on(receive(&mut receiver, 2));
    assert_eq!(datagrams.len(), 2);
}

#[test]
#[should_panic(expected = "a UDP sender polls only on the thread of its first poll")]
fn a_sender_poll_on_a_second_thread_panics() {
    let (mut sender, _receiver) = on_thread("udp-first", || async {
        let net = net();
        let (mut sender, receiver) = loopback(&net);
        let to = transmit(receiver.local(), b"x");
        assert_eq!(send(&mut sender, &to).await, Ok(()));
        (sender, receiver)
    });
    let to = transmit(sender.local(), b"y");
    runtime().block_on(async {
        drop(send(&mut sender, &to).await);
    });
}

#[test]
#[should_panic(expected = "a UDP receiver polls only on the thread of its first poll")]
fn a_receiver_poll_on_a_second_thread_panics() {
    let mut receiver = on_thread("udp-first", || async {
        let net = net();
        let (_, mut receiver) = loopback(&net);
        let mut cx = Context::from_waker(Waker::noop());
        let mut buffer = [0; 8];
        let poll = receiver.poll_recv(
            &mut cx,
            &mut [IoSliceMut::new(&mut buffer)],
            &mut [Meta::default()],
        );
        assert_eq!(poll, Poll::Pending);
        receiver
    });
    runtime().block_on(async {
        drop(receive(&mut receiver, 1).await);
    });
}

#[test]
#[should_panic(expected = "must be called from the context of a Tokio 1.x runtime")]
fn a_first_send_with_no_runtime_panics() {
    let (mut sender, _receiver) = loopback(&net());
    let to = transmit(sender.local(), b"x");
    let mut cx = Context::from_waker(Waker::noop());
    drop(sender.poll_send(&mut cx, &to));
}

#[test]
#[should_panic(expected = "must be called from the context of a Tokio 1.x runtime")]
fn a_first_receive_with_no_runtime_panics() {
    let (_sender, mut receiver) = loopback(&net());
    let mut cx = Context::from_waker(Waker::noop());
    let mut buffer = [0; 8];
    drop(receiver.poll_recv(
        &mut cx,
        &mut [IoSliceMut::new(&mut buffer)],
        &mut [Meta::default()],
    ));
}

#[test]
fn an_unspecified_source_gives_einval() {
    on_thread("udp-unspecified-source", || async {
        let net = net();
        let (_, mut to_v4) = loopback(&net);
        let (_, mut to_v6) = bind(&net, SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 0));
        let any_v4 = IpAddr::from(Ipv4Addr::UNSPECIFIED);
        let any_v6 = IpAddr::from(Ipv6Addr::UNSPECIFIED);
        let mapped = IpAddr::from(Ipv4Addr::UNSPECIFIED.to_ipv6_mapped());
        let cases = [
            (IpAddr::from(LOCALHOST), any_v4),
            (any_v6, any_v4),
            (any_v6, mapped),
            (any_v6, any_v6),
            (Ipv6Addr::LOCALHOST.into(), any_v6),
        ];
        for (local, source) in cases {
            let receiver = if source.to_canonical().is_ipv4() {
                &mut to_v4
            } else {
                &mut to_v6
            };
            let (mut sender, _) = bind(&net, SocketAddr::new(local, 0));
            let from = Transmit {
                source: Some(source),
                ..transmit(receiver.local(), b"from")
            };
            let case = format!("a socket on {local}, source {source}");
            let refused = send(&mut sender, &from).await;
            assert_eq!(refused, Err(Error::Io { code: 22 }), "{case}");
            let after = transmit(receiver.local(), b"after");
            assert_eq!(send(&mut sender, &after).await, Ok(()), "{case}");
            let arrived = receive(receiver, 1).await;
            assert_eq!(arrived[0].contents, b"after", "{case}");
        }
    });
}

#[test]
#[should_panic(expected = "A Tokio 1.x context was found, but IO is disabled")]
fn a_first_send_in_a_runtime_with_no_io_driver_panics() {
    let (mut sender, _receiver) = loopback(&net());
    let to = transmit(sender.local(), b"x");
    runtime_with_no_io().block_on(async {
        drop(send(&mut sender, &to).await);
    });
}

/// Linux skips the `IPV6_PKTINFO` of a send on an IPv4 socket, so `os` refuses it.
#[test]
fn an_ipv6_source_on_an_ipv4_socket_gives_einval() {
    on_thread("udp-source", || async {
        let net = net();
        let (_, mut receiver) = loopback(&net);
        let sources = [Ipv6Addr::LOCALHOST, LOCALHOST.to_ipv6_mapped()];
        for (local, source) in [LOCALHOST, Ipv4Addr::UNSPECIFIED]
            .into_iter()
            .flat_map(|local| sources.map(|source| (local, source)))
        {
            let (mut sender, _) = bind(&net, SocketAddr::new(local.into(), 0));
            let from = Transmit {
                source: Some(source.into()),
                ..transmit(receiver.local(), b"from")
            };
            let case = format!("a socket on {local}, source {source}");
            let refused = send(&mut sender, &from).await;
            assert_eq!(refused, Err(Error::Io { code: 22 }), "{case}");
            let after = transmit(receiver.local(), b"after");
            assert_eq!(send(&mut sender, &after).await, Ok(()), "{case}");
            let arrived = receive(&mut receiver, 1).await;
            assert_eq!(arrived[0].contents, b"after", "{case}");
        }
    });
}

/// macOS ignores the `IPV6_PKTINFO` of a send to an IPv4 destination, and sends from
/// an address of its choice.
#[test]
fn an_ipv6_source_to_an_ipv4_destination_gives_einval() {
    on_thread("udp-source-family", || async {
        let net = net();
        let (_, mut receiver) = loopback(&net);
        let (mut sender, _) =
            bind(&net, SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0));
        let v4 = receiver.local();
        let mapped = SocketAddr::new(LOCALHOST.to_ipv6_mapped().into(), v4.port());
        let sources = [
            Ipv6Addr::LOCALHOST,
            Ipv6Addr::from([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1]),
        ];
        for (source, destination) in sources
            .into_iter()
            .flat_map(|source| [(source, v4), (source, mapped)])
        {
            let from = Transmit {
                source: Some(source.into()),
                ..transmit(destination, b"from")
            };
            let case = format!("source {source}, to {destination}");
            let refused = send(&mut sender, &from).await;
            assert_eq!(refused, Err(Error::Io { code: 22 }), "{case}");
            let after = transmit(destination, b"after");
            assert_eq!(send(&mut sender, &after).await, Ok(()), "{case}");
            let arrived = receive(&mut receiver, 1).await;
            assert_eq!(arrived[0].contents, b"after", "{case}");
        }
    });
}

#[test]
#[should_panic(expected = "A Tokio 1.x context was found, but IO is disabled")]
fn a_first_receive_in_a_runtime_with_no_io_driver_panics() {
    let (_sender, mut receiver) = loopback(&net());
    let mut buffer = [0; 8];
    let mut buffers = [IoSliceMut::new(&mut buffer)];
    let mut meta = [Meta::default()];
    runtime_with_no_io().block_on(async {
        drop(poll_fn(|cx| receiver.poll_recv(cx, &mut buffers, &mut meta)).await);
    });
}
