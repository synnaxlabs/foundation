//! A UDP sender whose OS send buffer is full at a chosen datagram. A seccomp filter
//! answers each `sendmsg` and `epoll_ctl` of the test thread, so each test runs on a
//! thread of its own.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]
#![expect(unsafe_code, reason = "a seccomp filter is an OS call")]

#[path = "common/gso.rs"]
mod gso;

use std::collections::BTreeMap;
use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use env::net::Error;
use env::net::udp::{self, Meta, Receiver, Sender, Transmit};

/// How long a receive waits before it takes that no more datagrams come.
const SILENCE: Duration = Duration::from_millis(300);

/// Gives the `k`th call `nr` of this thread, from 0, the errno of `plan(nr, k)`, and
/// runs it when that is `None`. It holds `sendmsg` and `epoll_ctl` only.
fn answer_calls(plan: impl FnMut(i64, usize) -> Option<i32> + Send + 'static) {
    use libc::{BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W};

    let (sender, listener) = mpsc::channel();
    // Started before the filter, so its own calls never wait on itself.
    #[expect(clippy::disallowed_methods, reason = "the test answers the filter")]
    std::thread::spawn(move || answer(&listener.recv().unwrap(), plan));
    let op = |code: u32, jt, k| libc::sock_filter {
        code: u16::try_from(code).unwrap(),
        jt,
        jf: 0,
        k,
    };
    let call = |nr: libc::c_long| u32::try_from(nr).unwrap();
    let mut filter = [
        op(BPF_LD | BPF_W | BPF_ABS, 0, 0),
        op(BPF_JMP | BPF_JEQ | BPF_K, 2, call(libc::SYS_sendmsg)),
        op(BPF_JMP | BPF_JEQ | BPF_K, 1, call(libc::SYS_epoll_ctl)),
        op(BPF_RET | BPF_K, 0, libc::SECCOMP_RET_ALLOW),
        op(BPF_RET | BPF_K, 0, libc::SECCOMP_RET_USER_NOTIF),
    ];
    let program = libc::sock_fprog {
        len: u16::try_from(filter.len()).unwrap(),
        filter: filter.as_mut_ptr(),
    };
    // The kernel reads each argument as a whole register.
    let (one, zero): (libc::c_ulong, libc::c_ulong) = (1, 0);
    // SAFETY: the call sets one flag of this thread.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, one, zero, zero, zero) };
    assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
    let mode = libc::c_ulong::from(libc::SECCOMP_SET_MODE_FILTER);
    let flags = libc::SECCOMP_FILTER_FLAG_NEW_LISTENER;
    // SAFETY: `program` points at `filter`, which outlives the call. The kernel copies
    // it.
    let fd =
        unsafe { libc::syscall(libc::SYS_seccomp, mode, flags, &raw const program) };
    let fd = i32::try_from(fd).unwrap();
    assert!(fd >= 0, "{}", std::io::Error::last_os_error());
    // SAFETY: the kernel gave the listener to this process alone.
    sender.send(unsafe { OwnedFd::from_raw_fd(fd) }).unwrap();
}

/// Answers each call that the filter of `listener` holds, as `plan` says.
fn answer(listener: &OwnedFd, mut plan: impl FnMut(i64, usize) -> Option<i32>) {
    let data = libc::seccomp_data {
        nr: 0,
        arch: 0,
        instruction_pointer: 0,
        args: [0; 6],
    };
    let mut counts = BTreeMap::new();
    loop {
        let mut held = libc::seccomp_notif {
            id: 0,
            pid: 0,
            flags: 0,
            data,
        };
        let receive = libc::SECCOMP_IOCTL_NOTIF_RECV;
        // SAFETY: `held` is a whole `seccomp_notif` for the kernel to fill.
        if unsafe { libc::ioctl(listener.as_raw_fd(), receive, &raw mut held) } != 0 {
            return;
        }
        let mut reply = libc::seccomp_notif_resp {
            id: held.id,
            val: 0,
            error: 0,
            flags: 0,
        };
        let nr = i64::from(held.data.nr);
        let k = counts.entry(nr).or_insert(0);
        let answer = plan(nr, *k);
        *k += 1;
        match answer {
            Some(errno) => reply.error = -errno,
            None => {
                reply.flags =
                    u32::try_from(libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE).unwrap();
            }
        }
        let send = libc::SECCOMP_IOCTL_NOTIF_SEND;
        // SAFETY: `reply` is a whole `seccomp_notif_resp`.
        unsafe { libc::ioctl(listener.as_raw_fd(), send, &raw mut reply) };
    }
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
