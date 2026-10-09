//! A UDP sender whose OS send buffer is full at a chosen datagram. A seccomp filter
//! answers each `sendmsg` and `epoll_ctl` of the test thread, so each test runs on a
//! thread of its own.

use crate::seccomp;

use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use env::net::Error;
use env::net::udp::{self, Meta, Receiver, Sender, Transmit};

use super::{gso, runtime};

/// How long a receive waits before it takes that no more datagrams come.
const SILENCE: Duration = Duration::from_millis(300);

/// Gives the `k`th `sendmsg` or `epoll_ctl` of this thread, from 0, the errno of
/// `plan(nr, k)`, and runs it when that is `None`.
fn answer_calls(mut plan: impl FnMut(i64, usize) -> Option<i32> + Send + 'static) {
    let calls = [libc::SYS_sendmsg, libc::SYS_epoll_ctl];
    seccomp::answer_calls(&calls, move |data, k| plan(i64::from(data.nr), k));
}

/// Runs `body` on a thread of its own, so the filter of one test stays on it.
fn on_thread<F: Future<Output = ()> + 'static>(
    body: impl FnOnce() -> F + Send + 'static,
) {
    super::on_thread("udp-full", body);
}

/// A socket on the IPv4 loopback.
fn config() -> udp::Config {
    udp::Config {
        local: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
        send_buffer_bytes: 1 << 20,
        recv_buffer_bytes: 1 << 20,
    }
}

/// A sender with GSO off and a receiver, both on the IPv4 loopback.
async fn pair() -> (Sender, Receiver) {
    let (mut sender, _) = os::net().udp(&config()).unwrap();
    gso::refuse(sender.local());
    let (_, mut receiver) = os::net().udp(&config()).unwrap();
    let off = batch(&receiver, b"wwww");
    poll_fn(|cx| sender.poll_send(cx, &off)).await.unwrap();
    assert_eq!(receive(&mut receiver).await, [b"ww", b"ww"]);
    (sender, receiver)
}

/// A batch of datagrams of 2 bytes to `receiver`.
fn batch<'a>(receiver: &Receiver, contents: &'a [u8]) -> Transmit<'a> {
    Transmit {
        destination: receiver.local(),
        source: None,
        ecn: None,
        contents,
        segment: NonZeroUsize::new(2),
    }
}

/// Sends `transmit` within 5 s.
///
/// Unlike `timeout`, it gives no last poll at the deadline, which would hide a lost
/// wake.
async fn send(sender: &mut Sender, transmit: &Transmit<'_>) {
    let mut late = std::pin::pin!(tokio::time::sleep(Duration::from_secs(5)));
    let sent = poll_fn(|cx| {
        if late.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        sender.poll_send(cx, transmit).map(Some)
    });
    assert_eq!(sent.await, Some(Ok(())));
}

/// Receives each datagram until none comes for [`SILENCE`].
async fn receive(receiver: &mut Receiver) -> Vec<Vec<u8>> {
    let mut datagrams = Vec::new();
    loop {
        let mut buffer = vec![0; 1 << 16];
        let mut buffers = [IoSliceMut::new(&mut buffer)];
        let mut meta = [Meta::default()];
        let got = poll_fn(|cx| receiver.poll_recv(cx, &mut buffers, &mut meta));
        let Ok(got) = tokio::time::timeout(SILENCE, got).await else {
            return datagrams;
        };
        assert_eq!(got, Ok(1));
        datagrams.extend(
            buffer[..meta[0].len]
                .chunks(meta[0].stride)
                .map(<[u8]>::to_vec),
        );
    }
}

#[test]
fn a_retry_after_pending_sends_only_the_datagrams_that_did_not_go_out() {
    on_thread(|| async {
        let (mut sender, mut receiver) = pair().await;
        answer_calls(|nr, k| {
            (nr == libc::SYS_sendmsg && k == 2).then_some(libc::EAGAIN)
        });
        send(&mut sender, &batch(&receiver, b"abcdefgh")).await;
        assert_eq!(receive(&mut receiver).await, [b"ab", b"cd", b"ef", b"gh"]);
    });
}

#[test]
fn a_retry_skips_a_datagram_lost_over_the_path_mtu() {
    on_thread(|| async {
        let (mut sender, mut receiver) = pair().await;
        answer_calls(|nr, k| match (nr, k) {
            (libc::SYS_sendmsg, 0) => Some(libc::EMSGSIZE),
            (libc::SYS_sendmsg, 1) => Some(libc::EAGAIN),
            _ => None,
        });
        send(&mut sender, &batch(&receiver, b"abcd")).await;
        assert_eq!(receive(&mut receiver).await, [b"cd"]);
    });
}

#[test]
fn a_batch_over_the_path_mtu_sends_its_short_last_datagram_alone() {
    on_thread(|| async {
        let (mut sender, _) = os::net().udp(&config()).unwrap();
        let (_, mut receiver) = os::net().udp(&config()).unwrap();
        answer_calls(|nr, k| {
            (nr == libc::SYS_sendmsg && k == 0).then_some(libc::EMSGSIZE)
        });
        send(&mut sender, &batch(&receiver, b"abcde")).await;
        assert_eq!(receive(&mut receiver).await, [b"e"]);
    });
}

#[test]
fn a_batch_of_full_segments_over_the_path_mtu_is_lost() {
    on_thread(|| async {
        let (mut sender, _) = os::net().udp(&config()).unwrap();
        let (_, mut receiver) = os::net().udp(&config()).unwrap();
        answer_calls(|nr, k| {
            (nr == libc::SYS_sendmsg && k == 0).then_some(libc::EMSGSIZE)
        });
        send(&mut sender, &batch(&receiver, b"abcd")).await;
        assert!(receive(&mut receiver).await.is_empty());
    });
}

#[test]
fn a_retry_of_a_batch_over_the_path_mtu_sends_its_last_datagram_once() {
    on_thread(|| async {
        let (mut sender, _) = os::net().udp(&config()).unwrap();
        let (_, mut receiver) = os::net().udp(&config()).unwrap();
        answer_calls(|nr, k| match (nr, k) {
            (libc::SYS_sendmsg, 0) => Some(libc::EMSGSIZE),
            (libc::SYS_sendmsg, 1) => Some(libc::EAGAIN),
            _ => None,
        });
        send(&mut sender, &batch(&receiver, b"abcde")).await;
        assert_eq!(receive(&mut receiver).await, [b"e"]);
    });
}

#[test]
fn a_failure_of_the_last_datagram_of_a_batch_over_the_path_mtu_is_the_outcome() {
    on_thread(|| async {
        let (mut sender, _) = os::net().udp(&config()).unwrap();
        let (_, receiver) = os::net().udp(&config()).unwrap();
        answer_calls(|nr, k| match (nr, k) {
            (libc::SYS_sendmsg, 0) => Some(libc::EMSGSIZE),
            (libc::SYS_sendmsg, 1) => Some(libc::ENETUNREACH),
            _ => None,
        });
        let to = batch(&receiver, b"abcde");
        let sent = poll_fn(|cx| sender.poll_send(cx, &to)).await;
        let remote = receiver.local();
        assert_eq!(sent, Err(Error::Unreachable { remote }));
    });
}

/// Polls `transmit` until the send buffer is full at its third datagram, and empties
/// the buffer.
fn fill(sender: &mut Sender, transmit: &Transmit<'_>) {
    let full = Arc::new(AtomicBool::new(true));
    let held = Arc::clone(&full);
    answer_calls(move |nr, k| {
        let full = nr == libc::SYS_sendmsg && k >= 2 && held.load(Ordering::Relaxed);
        full.then_some(libc::EAGAIN)
    });
    let mut cx = Context::from_waker(Waker::noop());
    assert_eq!(sender.poll_send(&mut cx, transmit), Poll::Pending);
    full.store(false, Ordering::Relaxed);
}

#[test]
fn a_different_transmit_after_pending_loses_the_datagrams_that_went_out() {
    on_thread(|| async {
        let (mut sender, mut receiver) = pair().await;
        fill(&mut sender, &batch(&receiver, b"abcdefgh"));
        send(&mut sender, &batch(&receiver, b"ABCDEFGH")).await;
        assert_eq!(receive(&mut receiver).await, [b"ab", b"cd", b"EF", b"GH"]);
    });
}

#[test]
fn a_different_transmit_of_one_datagram_after_pending_goes_out_whole() {
    on_thread(|| async {
        let (mut sender, mut receiver) = pair().await;
        fill(&mut sender, &batch(&receiver, b"abcdefgh"));
        send(&mut sender, &batch(&receiver, b"XY")).await;
        assert_eq!(receive(&mut receiver).await, [b"ab", b"cd", b"XY"]);
    });
}

#[test]
fn a_failed_registration_restarts_the_next_transmit_at_its_first_datagram() {
    on_thread(|| async {
        let (mut sender, mut receiver) = pair().await;
        answer_calls(|nr, k| match nr {
            libc::SYS_sendmsg => (k == 2).then_some(libc::EAGAIN),
            _ => Some(libc::ENOSPC),
        });
        let refused = batch(&receiver, b"abcdefgh");
        let mut cx = Context::from_waker(Waker::noop());
        let code = libc::ENOSPC;
        let failed = Poll::Ready(Err(Error::Io { code }));
        assert_eq!(sender.poll_send(&mut cx, &refused), failed);
        send(&mut sender, &batch(&receiver, b"ABCDEFGH")).await;
        let datagrams = [b"ab", b"cd", b"AB", b"CD", b"EF", b"GH"];
        assert_eq!(receive(&mut receiver).await, datagrams);
    });
}

#[test]
fn a_failed_wait_restarts_the_next_transmit_at_its_first_datagram() {
    #[expect(clippy::disallowed_methods, reason = "the filter needs a thread")]
    let handle = std::thread::spawn(|| {
        let first = runtime();
        let (mut sender, gone) = first.block_on(pair());
        answer_calls(|nr, k| {
            // At 6, the next transmit waits on a new registration.
            let full = nr == libc::SYS_sendmsg && matches!(k, 2 | 3 | 6);
            full.then_some(libc::EAGAIN)
        });
        let waiting = batch(&gone, b"abcdefgh");
        let mut cx = Context::from_waker(Waker::noop());
        let full = first.block_on(async { sender.poll_send(&mut cx, &waiting) });
        assert_eq!(full, Poll::Pending);
        drop(first);
        runtime().block_on(async {
            let (_, mut receiver) = os::net().udp(&config()).unwrap();
            let failed = Poll::Ready(Err(Error::Io { code: libc::EIO }));
            assert_eq!(sender.poll_send(&mut cx, &waiting), failed);
            send(&mut sender, &batch(&receiver, b"ABCDEFGH")).await;
            let datagrams = [b"AB", b"CD", b"EF", b"GH"];
            assert_eq!(receive(&mut receiver).await, datagrams);
        });
    });
    handle.join().unwrap();
}
