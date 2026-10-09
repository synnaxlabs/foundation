//! The loopback sockets that the test binaries of `os` open.

use std::future::poll_fn;
use std::io::IoSliceMut;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;

use env::net::tcp::{self, Listen, Options};
use env::net::udp::{self, Meta, Transmit};
use env::net::{Listener, Net, Tcp};

const LOCAL: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

pub(crate) const BIND: udp::Config = udp::Config {
    local: LOCAL,
    send_buffer_bytes: 1 << 16,
    recv_buffer_bytes: 1 << 16,
};

const OPTIONS: Options = Options {
    send_buffer_bytes: 1 << 16,
    recv_buffer_bytes: 1 << 16,
    unsent_bytes_max: NonZeroUsize::new(1 << 14).unwrap(),
    delayed: false,
};

pub(crate) const LISTEN: Listen = Listen {
    local: LOCAL,
    backlog: 1,
    options: OPTIONS,
};

/// A socket of each kind, open until the drop.
pub(crate) struct Each {
    _listener: Listener,
    _client: Tcp,
    _server: Tcp,
    _halves: (udp::Sender, udp::Receiver),
}

/// Opens a listener, a stream that `net` connects and one that it accepts, and a UDP
/// socket whose halves each send or receive once, so each holds a descriptor of its
/// own. Needs a runtime with an I/O driver.
pub(crate) async fn open(net: &Net) -> Each {
    let mut listener = net.listen(&LISTEN).expect("listen on loopback");
    let connect = tcp::Config {
        remote: listener.local(),
        options: OPTIONS,
    };
    let client = net.connect(&connect).await.expect("the listener accepts");
    let accepted = poll_fn(|cx| listener.poll_accept(cx)).await;
    let server = accepted.expect("a stream waits in the backlog");
    let (mut sender, mut receiver) = net.udp(&BIND).expect("bind on loopback");
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
    Each {
        _listener: listener,
        _client: client,
        _server: server,
        _halves: (sender, receiver),
    }
}
