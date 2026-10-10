//! The first poll of each UDP half with no free descriptor, which binds the half to
//! its thread and leaves it usable.

// Lets Clippy treat the helpers as test code.
#![cfg(test)]
#![cfg(target_os = "linux")]

#[path = "common/descriptors.rs"]
mod descriptors;

use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::{Ipv4Addr, SocketAddr};
use std::task::{Context, Poll, Waker};

use env::net::Error;
use env::net::udp::{self, Meta, Receiver, Sender, Transmit};
use env::thread::Panicked;
use rustix::io::Errno;

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

fn emfile<T>() -> Poll<Result<T, Error>> {
    Poll::Ready(Err(Error::Io {
        code: Errno::MFILE.raw_os_error(),
    }))
}

#[test]
fn a_first_poll_with_no_free_descriptor_binds_the_half_and_leaves_it_usable() {
    descriptors::limit();
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
    let (mut moved, mut moved_receiver) = os::net().udp(&config).unwrap();
    let destination = receiver.local();
    let mut cx = Context::from_waker(Waker::noop());
    let mut held = descriptors::take_each();
    assert_eq!(send(&mut sender, destination), emfile());
    assert_eq!(send(&mut moved, destination), emfile());
    assert_eq!(receive(&mut receiver, &mut cx), emfile());
    assert_eq!(receive(&mut moved_receiver, &mut cx), emfile());
    drop(held.pop());
    assert_eq!(send(&mut sender, destination), Poll::Ready(Ok(())));
    drop(held.pop());
    let arrived = runtime.block_on(poll_fn(|cx| receive(&mut receiver, cx)));
    assert_eq!(arrived, Ok(1));
    drop(held);
    let threads = os::threads().expect("the OS gives the cores of this process");
    let handle = threads.start("udp-moved", move || async move {
        drop(send(&mut moved, destination));
    });
    let panicked = Err(Panicked {
        name: "udp-moved".into(),
    });
    assert_eq!(
        handle.expect("the thread starts").join(),
        panicked,
        "sender"
    );
    let handle = threads.start("udp-moved", move || async move {
        let mut cx = Context::from_waker(Waker::noop());
        drop(receive(&mut moved_receiver, &mut cx));
    });
    assert_eq!(
        handle.expect("the thread starts").join(),
        panicked,
        "receiver"
    );
}
