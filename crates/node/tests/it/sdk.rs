//! The Rust SDK against a node process.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use sdk::{Client, Error, Node, Subject};
use types::ed25519::PrivateKey;
use wire::hub::client::Refusal;

use crate::rig::Rig;

/// The bytes of the file `name` of the data directory of `rig`, once it holds `tag`.
/// Checks the CRC32C of the file.
fn read<const LEN: usize>(rig: &Rig, name: &str, tag: &[u8]) -> [u8; LEN] {
    let path = rig.dir.join("foundation-data/data").join(name);
    let bytes = rig.wait(&format!("the node writes {name}"), || held(&path, tag));
    let body = LEN - 4;
    assert_eq!(
        crc32c::crc32c(&bytes[..body]).to_le_bytes(),
        bytes[body..],
        "the CRC32C of {name}"
    );
    bytes
}

/// The bytes at `path` when they are `LEN` long and start with `tag`.
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads a file of a node process"
)]
fn held<const LEN: usize>(path: &Path, tag: &[u8]) -> Result<[u8; LEN], String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    match <[u8; LEN]>::try_from(bytes) {
        Ok(bytes) if bytes.starts_with(tag) => Ok(bytes),
        Ok(bytes) => Err(format!("{bytes:?}")),
        Err(bytes) => Err(format!("{} bytes", bytes.len())),
    }
}

/// The node of `rig`, with an unspecified IP as loopback, and its admin, from the
/// files `address` and `admin.key`.
fn admin(rig: &Rig) -> (Node, Subject) {
    let address: [u8; 99] = read(rig, "address", b"foundation/address/1");
    let key: [u8; 54] = read(rig, "admin.key", b"foundation/admin/1");
    let port = u16::from_be_bytes([address[85], address[86]]);
    let ip = match address[68] {
        4 => IpAddr::from(<[u8; 4]>::try_from(&address[69..73]).unwrap()),
        6 => IpAddr::from(<[u8; 16]>::try_from(&address[69..85]).unwrap()),
        family => panic!("the address has family {family}"),
    };
    let ip = match ip {
        IpAddr::V4(ip) if ip.is_unspecified() => Ipv4Addr::LOCALHOST.into(),
        IpAddr::V6(ip) if ip.is_unspecified() => Ipv6Addr::LOCALHOST.into(),
        ip => ip,
    };
    let node = u128::from_be_bytes(address[20..36].try_into().unwrap());
    let public_key = <[u8; 32]>::try_from(&address[36..68]).unwrap();
    let node = Node {
        addresses: vec![SocketAddr::new(ip, port)],
        key: types::node::Key::from_u128(node),
        public_key: types::ed25519::PublicKey::new(public_key).unwrap(),
    };
    let subject = Subject {
        name: spec::founding::admin(),
        private_key: PrivateKey(key[18..50].try_into().unwrap()),
    };
    (node, subject)
}

#[test]
fn a_client_connects_to_a_node_and_connects_again_after_its_close() {
    let mut rig = Rig::new();
    rig.start();
    let (node, subject) = admin(&rig);
    let client = Client::connect(&node, subject.clone()).unwrap();
    assert_eq!(client.close(), Ok(()));
    let client = Client::connect(&node, subject).unwrap();
    drop(client);
    rig.stop();
}

/// A UDP relay to `node` for one client. Gives its address and the count of the
/// datagrams that the client sent through it.
#[expect(
    clippy::disallowed_methods,
    reason = "a relay between a client and a node"
)]
fn relay(node: SocketAddr) -> (SocketAddr, Arc<AtomicUsize>) {
    let front = UdpSocket::bind((node.ip(), 0)).unwrap();
    let back = UdpSocket::bind((node.ip(), 0)).unwrap();
    back.connect(node).unwrap();
    let address = front.local_addr().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let (to_client, from_node) =
        (front.try_clone().unwrap(), back.try_clone().unwrap());
    let (client, counted) = std::sync::mpsc::channel();
    let sent = Arc::clone(&count);
    std::thread::spawn(move || {
        let mut buffer = vec![0; 1 << 16];
        while let Ok((len, from)) = front.recv_from(&mut buffer) {
            if sent.fetch_add(1, Ordering::SeqCst) == 0 {
                client.send(from).unwrap();
            }
            back.send(&buffer[..len]).unwrap();
        }
    });
    std::thread::spawn(move || {
        let client = counted.recv().unwrap();
        let mut buffer = vec![0; 1 << 16];
        while let Ok(len) = from_node.recv(&mut buffer) {
            to_client.send_to(&buffer[..len], client).unwrap();
        }
    });
    (address, count)
}

#[test]
fn the_close_of_a_client_sends_the_close_to_the_node() {
    let mut rig = Rig::new();
    rig.start();
    let (node, subject) = admin(&rig);
    let (front, count) = relay(node.addresses[0]);
    let node = Node {
        addresses: vec![front],
        ..node
    };
    let client = Client::connect(&node, subject).unwrap();
    let before = count.load(Ordering::SeqCst);
    assert_eq!(client.close(), Ok(()));
    rig.wait("a datagram after the close", || {
        let after = count.load(Ordering::SeqCst);
        if after > before {
            Ok(())
        } else {
            Err(format!("{after} datagrams"))
        }
    });
    rig.stop();
}

/// The node sends its close before it exits. No call of the client shows that the
/// client took it, so the test gives the client 1 s on loopback.
#[test]
fn the_close_of_a_client_gives_the_cause_when_the_node_closed_the_session() {
    let mut rig = Rig::new();
    rig.start();
    let (node, subject) = admin(&rig);
    let client = Client::connect(&node, subject).unwrap();
    rig.stop();
    #[expect(
        clippy::disallowed_methods,
        reason = "the client takes the close of a node process in real time"
    )]
    std::thread::sleep(std::time::Duration::from_secs(1));
    let closed = transport::Error::PeerClosed {
        code: transport::Code(0),
    };
    assert_eq!(
        client.close(),
        Err(Error::Session(program::Error::Transport(closed)))
    );
}

#[test]
fn a_client_dials_the_next_address_when_the_first_does_not_answer() {
    let mut rig = Rig::new();
    rig.start();
    let (node, subject) = admin(&rig);
    #[expect(clippy::disallowed_methods, reason = "a socket that never reads")]
    let silent = std::net::UdpSocket::bind((node.addresses[0].ip(), 0)).unwrap();
    let (front, count) = relay(node.addresses[0]);
    let node = Node {
        addresses: vec![silent.local_addr().unwrap(), front],
        ..node
    };
    let client = Client::connect(&node, subject).unwrap();
    assert_ne!(count.load(Ordering::SeqCst), 0);
    assert_eq!(client.close(), Ok(()));
    rig.stop();
}

#[test]
fn a_client_with_a_wrong_public_key_gets_the_error_of_the_handshake() {
    let mut rig = Rig::new();
    rig.start();
    let (node, subject) = admin(&rig);
    let peer = PrivateKey([7; 32]).public();
    let node = Node {
        public_key: peer,
        ..node
    };
    let error = Client::connect(&node, subject).unwrap_err();
    let unreachable = transport::Error::Unreachable {
        peer,
        attempts: vec![(
            transport::Address::Udp(node.addresses[0]),
            transport::Error::Authentication { expected: peer },
        )],
    };
    assert_eq!(
        error,
        Error::Session(program::Error::Transport(unreachable))
    );
    rig.stop();
}

#[test]
fn a_client_of_a_subject_that_the_spec_does_not_list_is_refused() {
    let mut rig = Rig::new();
    rig.start();
    let (node, subject) = admin(&rig);
    let subject = Subject {
        name: "plant.operator".parse().unwrap(),
        ..subject
    };
    let error = Client::connect(&node, subject).unwrap_err();
    assert_eq!(
        error,
        Error::Session(program::Error::Refused(Refusal::Refused))
    );
    assert_eq!(error.to_string(), Refusal::Refused.to_string());
    rig.stop();
}

#[test]
fn a_client_of_an_address_that_no_node_reads_is_unreachable() {
    #[expect(clippy::disallowed_methods, reason = "a socket that never reads")]
    let silent = std::net::UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();
    let address = silent.local_addr().unwrap();
    let peer = PrivateKey([7; 32]).public();
    let node = Node {
        addresses: vec![address],
        key: types::node::Key::from_u128(1),
        public_key: peer,
    };
    let subject = Subject {
        name: spec::founding::admin(),
        private_key: PrivateKey([8; 32]),
    };
    let error = Client::connect(&node, subject).unwrap_err();
    let unreachable = transport::Error::Unreachable {
        peer,
        attempts: vec![(transport::Address::Udp(address), transport::Error::TimedOut)],
    };
    assert_eq!(
        error,
        Error::Session(program::Error::Transport(unreachable))
    );
}
