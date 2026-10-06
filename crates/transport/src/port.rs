//! The node's sockets, and the part of them that each shard owns.

use std::net::SocketAddr;
use std::num::NonZeroUsize;

use env::net::udp;

use crate::Address;

/// The size of the OS send and receive buffers of the UDP socket.
const BUFFER_BYTES: usize = 1 << 21;

/// The node's sockets: one UDP socket that every shard sends on. `node` binds it
/// once and splits it into one part for each shard.
///
/// ```
/// use std::net::SocketAddr;
/// use std::num::NonZeroUsize;
///
/// use transport::{Port, port};
///
/// fn bind(net: &env::net::Net, at: SocketAddr) -> Result<Vec<port::Shard>, env::net::Error> {
///     let port = Port::bind(net, at)?;
///     let _ = port.addresses();
///     Ok(port.split(NonZeroUsize::MIN))
/// }
/// ```
#[derive(Debug)]
pub struct Port {
    sender: udp::Sender,
    receiver: udp::Receiver,
}

impl Port {
    /// Binds the node's UDP socket at `udp`. Port 0 binds a free port, which
    /// [`Port::addresses`] shows. `[::]` takes IPv4 and IPv6.
    ///
    /// # Errors
    ///
    /// The error of the bind: [`env::net::Error::AddressInUse`] when another socket
    /// holds `udp`.
    pub fn bind(net: &env::net::Net, udp: SocketAddr) -> Result<Self, env::net::Error> {
        let (sender, receiver) = net.udp(&udp::Config {
            local: udp,
            send_buffer_bytes: BUFFER_BYTES,
            recv_buffer_bytes: BUFFER_BYTES,
        })?;
        Ok(Self { sender, receiver })
    }

    /// The address of each socket as it is bound. An unspecified IP stays
    /// unspecified.
    #[must_use]
    pub fn addresses(&self) -> Vec<Address> {
        vec![Address::Udp(self.sender.local())]
    }

    /// Gives each of `shards` shards its part, in shard order.
    ///
    /// # Panics
    ///
    /// When `shards` is over 1: a port serves one shard until it routes packets
    /// between shards (#77).
    #[must_use]
    pub fn split(self, shards: NonZeroUsize) -> Vec<Shard> {
        assert!(
            shards == NonZeroUsize::MIN,
            "a port serves one shard until it routes packets between shards (#77), not \
             {shards}"
        );
        vec![Shard {
            index: 0,
            sender: self.sender,
            receiver: self.receiver,
        }]
    }
}

/// The part of a [`Port`] that one shard owns. It can move to the shard's thread,
/// where [`Transport::new`](crate::Transport::new) takes it.
#[derive(Debug)]
pub struct Shard {
    /// The shard's index, which each connection ID it issues starts with.
    pub(crate) index: u8,
    pub(crate) sender: udp::Sender,
    pub(crate) receiver: udp::Receiver,
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::num::NonZeroUsize;

    use super::Port;
    use crate::Address;
    use crate::testing;

    /// The first port that a bind of port 0 takes under `sim`.
    const FREE: u16 = 49152;

    #[test]
    fn a_bind_of_port_0_gives_the_port_it_took() {
        testing::run(0, |shard| {
            let port = Port::bind(shard.net(), SocketAddr::new(shard.ip(), 0));
            let bound = SocketAddr::new(shard.ip(), FREE);
            let port = port.expect("a port");
            assert_eq!(port.addresses(), [Address::Udp(bound)]);
        });
    }

    #[test]
    fn a_bind_of_a_held_address_fails_until_the_part_drops() {
        testing::run(0, |shard| {
            let local = SocketAddr::new(shard.ip(), testing::PORT);
            let port = Port::bind(shard.net(), local).expect("a port");
            let parts = port.split(NonZeroUsize::MIN);
            let held = env::net::Error::AddressInUse { local };
            assert_eq!(Port::bind(shard.net(), local).err(), Some(held));
            drop(parts);
            assert_eq!(Port::bind(shard.net(), local).err(), None);
        });
    }

    #[test]
    fn a_split_gives_one_part_for_one_shard() {
        testing::run(0, |shard| {
            let local = SocketAddr::new(shard.ip(), 0);
            let port = Port::bind(shard.net(), local).expect("a port");
            let parts = port.split(NonZeroUsize::MIN);
            let indexes: Vec<u8> = parts.iter().map(|part| part.index).collect();
            assert_eq!(indexes, [0]);
        });
    }

    #[test]
    fn a_split_into_two_shards_panics_until_the_port_routes() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let split = sim.run_on(&node, |node, _| async move {
            let local = SocketAddr::new(node.addresses()[0], 0);
            let port = Port::bind(&node.net(), local).expect("a port");
            drop(port.split(NonZeroUsize::new(2).expect("not zero")));
        });
        let message = "a port serves one shard until it routes packets between \
                       shards (#77), not 2";
        let panicked = sim::Error::Panicked {
            thread: "run_on".into(),
            message: message.into(),
            seed: 0,
        };
        assert_eq!(split, Err(panicked));
    }
}
