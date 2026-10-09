//! A UDP sender whose OS send buffer is full at a chosen datagram. A seccomp filter
//! answers each `sendmsg` and `epoll_ctl` of the test thread, so each test runs on a
//! thread of its own.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]

#[path = "common/gso.rs"]
mod gso;
#[path = "common/seccomp.rs"]
mod seccomp;

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

/// How long a receive waits before it takes that no more datagrams come.
const SILENCE: Duration = Duration::from_millis(300);

/// Gives the `k`th `sendmsg` or `epoll_ctl` of this thread, from 0, the errno of
/// `plan(nr, k)`, and runs it when that is `None`.
fn answer_calls(mut plan: impl FnMut(i64, usize) -> Option<i32> + Send + 'static) {
    let calls = [libc::SYS_sendmsg, libc::SYS_epoll_ctl];
    seccomp::answer_calls(&calls, move |data, k| plan(i64::from(data.nr), k));
}

/// Runs `body` on a thread of its own, so the filter of one test stays on it.
fn on_thread<F: Future<Output = ()>>(body: impl FnOnce() -> F + Send + 'static) {
    #[expect(clippy::disallowed_methods, reason = "the filter needs a thread")]
    let handle = std::thread::spawn(|| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(body());
    });
    handle.join().unwrap();
}

/// A sender with GSO off and a receiver, both on the IPv4 loopback.
async fn pair() -> (Sender, Receiver) {
    let net = os::net();
    let config = udp::Config {
        local: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
        send_buffer_bytes: 1 << 20,
        recv_buffer_bytes: 1 << 20,
    };
    let (mut sender, _) = net.udp(&config).unwrap();
    gso::refuse(sender.local());
    let (_, mut receiver) = net.udp(&config).unwrap();
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

async fn send(sender: &mut Sender, transmit: &Transmit<'_>) {
    let sent = poll_fn(|cx| sender.poll_send(cx, transmit));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), sent).await,
        Ok(Ok(()))
    );
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
