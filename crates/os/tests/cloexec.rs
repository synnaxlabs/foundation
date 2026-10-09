//! Sockets that `os` opens while the OS refuses each call that sets the flags of a
//! descriptor, so a socket that is closed on exec only by a second call fails to open.
//! The filter acts on the test thread and each thread it starts, so it runs in a test
//! binary of its own, with this one test only.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]
#![expect(unsafe_code, reason = "a seccomp filter is an OS call")]

use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;

use env::net::tcp::{self, Listen, Options};
use env::net::udp::{self, Meta, Transmit};

/// Makes the OS refuse each `fcntl` of this thread with `F_SETFD`, with `EPERM`.
fn refuse_setfd() {
    use libc::{BPF_ABS, BPF_JEQ, BPF_JMP, BPF_K, BPF_LD, BPF_RET, BPF_W};

    let op = |code: u32, jt, jf, k| libc::sock_filter {
        code: u16::try_from(code).unwrap(),
        jt,
        jf,
        k,
    };
    let value = |n: libc::c_long| u32::try_from(n).unwrap();
    let refuse = libc::SECCOMP_RET_ERRNO | value(libc::EPERM.into());
    // The low half of the second argument, on a little-endian machine.
    let command = u32::try_from(std::mem::offset_of!(libc::seccomp_data, args) + 8);
    let mut filter = [
        op(BPF_LD | BPF_W | BPF_ABS, 0, 0, 0),
        op(BPF_JMP | BPF_JEQ | BPF_K, 0, 2, value(libc::SYS_fcntl)),
        op(BPF_LD | BPF_W | BPF_ABS, 0, 0, command.unwrap()),
        op(BPF_JMP | BPF_JEQ | BPF_K, 1, 0, value(libc::F_SETFD.into())),
        op(BPF_RET | BPF_K, 0, 0, libc::SECCOMP_RET_ALLOW),
        op(BPF_RET | BPF_K, 0, 0, refuse),
    ];
    let program = libc::sock_fprog {
        len: u16::try_from(filter.len()).unwrap(),
        filter: filter.as_mut_ptr(),
    };
    // The kernel reads each argument as a whole register.
    let (one, zero, mode): (libc::c_ulong, libc::c_ulong, libc::c_ulong) =
        (1, 0, libc::SECCOMP_MODE_FILTER.into());
    // SAFETY: the call sets one flag of this thread.
    let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, one, zero, zero, zero) };
    assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
    // SAFETY: `program` points at `filter`, which outlives the call. The kernel copies
    // it.
    let rc = unsafe { libc::prctl(libc::PR_SET_SECCOMP, mode, &raw const program) };
    assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
}

#[test]
fn each_socket_opens_closed_on_exec_with_no_second_call() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("a current-thread runtime builds");
    refuse_setfd();
    let local = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0);
    let options = Options {
        send_buffer_bytes: 1 << 16,
        recv_buffer_bytes: 1 << 16,
        unsent_bytes_max: NonZeroUsize::new(1 << 14).unwrap(),
        delayed: false,
    };
    let net = os::net();
    runtime.block_on(async {
        let listen = Listen {
            local,
            backlog: 1,
            options,
        };
        let mut listener = net.listen(&listen).expect("listen on loopback");
        let connect = tcp::Config {
            remote: listener.local(),
            options,
        };
        let _client = net.connect(&connect).await.expect("the listener accepts");
        let accepted = poll_fn(|cx| listener.poll_accept(cx)).await;
        let _server = accepted.expect("a stream waits in the backlog");
        let bind = udp::Config {
            local,
            send_buffer_bytes: 1 << 16,
            recv_buffer_bytes: 1 << 16,
        };
        let (mut sender, mut receiver) = net.udp(&bind).expect("bind on loopback");
        let transmit = Transmit {
            destination: receiver.local(),
            source: None,
            ecn: None,
            contents: b"x",
            segment: None,
        };
        let sent = poll_fn(|cx| sender.poll_send(cx, &transmit)).await;
        sent.expect("the send");
        let mut buffer = [0; 8];
        let mut meta = [Meta::default()];
        let mut buffers = [IoSliceMut::new(&mut buffer)];
        let arrived = poll_fn(|cx| receiver.poll_recv(cx, &mut buffers, &mut meta));
        assert_eq!(arrived.await, Ok(1));
    });
}
