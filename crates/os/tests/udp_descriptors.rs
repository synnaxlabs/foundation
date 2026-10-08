//! The first poll of each UDP half with no free descriptor. It takes each descriptor
//! of the process, so it runs in a test binary of its own, with this one test only:
//! the harness runs the tests of a binary on threads of one process.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]

use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::OwnedFd;
use std::task::{Context, Poll, Waker};

use env::net::Error;
use env::net::udp::{self, Meta, Receiver, Sender, Transmit};
use rustix::fs::{Mode, OFlags, open};
use rustix::io::Errno;
use rustix::process::{self, Resource};

fn send(sender: &mut Sender, destination: SocketAddr) -> Poll<Result<(), Error>> {
    let transmit = Transmit {
        destination,
        source: None,
        ecn: None,
        contents: b"x",
        segment: None,
    };
    sender.poll_send(&mut Context::from_waker(Waker::noop()), &transmit)
}

fn receive(
    receiver: &mut Receiver,
    cx: &mut Context<'_>,
) -> Poll<Result<usize, Error>> {
    let mut buffer = [0; 8];
    let mut meta = [Meta::default()];
    let mut buffers = [IoSliceMut::new(&mut buffer)];
    receiver.poll_recv(cx, &mut buffers, &mut meta)
}

/// Takes each free descriptor of the process until the result drops.
fn take_each() -> Vec<OwnedFd> {
    let mut held = Vec::new();
    while let Ok(fd) = open("/dev/null", OFlags::RDONLY, Mode::empty()) {
        held.push(fd);
    }
    held
}

#[test]
fn a_first_poll_with_no_free_descriptor_leaves_the_half_usable() {
    let mut limit = process::getrlimit(Resource::Nofile);
    limit.current = Some(128);
    process::setrlimit(Resource::Nofile, limit).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .expect("a current-thread runtime builds");
    let _entered = runtime.enter();
    let config = udp::Config {
        local: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
        send_buffer_bytes: 1 << 16,
        recv_buffer_bytes: 1 << 16,
    };
    let (mut sender, mut receiver) = os::net().udp(&config).unwrap();
    let destination = receiver.local();
    let io = Error::Io {
        code: Errno::MFILE.raw_os_error(),
    };
    let held = take_each();
    assert_eq!(send(&mut sender, destination), Poll::Ready(Err(io.clone())));
    let mut cx = Context::from_waker(Waker::noop());
    assert_eq!(receive(&mut receiver, &mut cx), Poll::Ready(Err(io)));
    drop(held);
    assert_eq!(send(&mut sender, destination), Poll::Ready(Ok(())));
    let arrived = runtime.block_on(poll_fn(|cx| receive(&mut receiver, cx)));
    assert_eq!(arrived, Ok(1));
}
