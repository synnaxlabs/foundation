//! The dial of [`Transport::dial`](crate::Transport::dial): one attempt for each
//! address, staggered, until one gives a session.

use std::future::poll_fn;
use std::pin::Pin;
use std::task::{Context, Poll};

use env::clock::{Clock, Sleep};
use types::node::PublicKey;
use types::time::{Monotonic, Span};

use crate::{Address, Error, quic};

/// How long the newest attempt runs before the next one starts.
const STAGGER: Span = Span::from_nanos(250 * Span::MILLISECOND.nanos());

/// Dials `peer` at each of `addresses`: UDP first, then TCP, then relays, in the
/// given order within a kind. It starts the next address [`STAGGER`] after the
/// newest, or at once when the newest fails, and gives the first session that
/// connects. Dropping the others closes their dials.
///
/// # Errors
///
/// [`Error::Unreachable`] with each attempt's cause, in the order started, when none
/// connects.
pub(crate) async fn dial(
    carrier: &quic::Carrier,
    clock: &Clock,
    peer: PublicKey,
    addresses: &[Address],
) -> Result<quic::Session, Error> {
    let mut addresses = addresses.to_vec();
    addresses.sort_by_key(|address| match address {
        Address::Udp(_) => 0,
        Address::Tcp(_) => 1,
        Address::Relay { .. } => 2,
    });
    let mut dial = Dial {
        carrier,
        clock,
        peer,
        addresses,
        causes: Vec::new(),
        flying: Vec::new(),
        sleep: clock.sleep_until(Monotonic(0)),
    };
    poll_fn(|cx| dial.poll(cx)).await
}

/// The attempts of one [`dial`].
struct Dial<'a> {
    carrier: &'a quic::Carrier,
    clock: &'a Clock,
    peer: PublicKey,
    addresses: Vec<Address>,
    /// Why each attempt started so far failed, by its index in `addresses`. `None`
    /// while it is in flight.
    causes: Vec<Option<Error>>,
    /// Each attempt in flight, with its index in `addresses`.
    flying: Vec<(usize, quic::Session)>,
    /// Due when the next attempt starts, unless the newest fails first.
    sleep: Sleep,
}

impl Dial<'_> {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<quic::Session, Error>> {
        loop {
            if let Some(session) = self.poll_flying(cx) {
                return Poll::Ready(Ok(session));
            }
            let next = self.causes.len();
            if next == self.addresses.len() {
                if self.flying.is_empty() {
                    return Poll::Ready(Err(self.unreachable()));
                }
                return Poll::Pending;
            }
            let failed = next == 0 || self.causes[next - 1].is_some();
            if !failed && Pin::new(&mut self.sleep).poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.start(next);
        }
    }

    /// Takes each attempt that ended, and gives the session of one that connected.
    fn poll_flying(&mut self, cx: &mut Context<'_>) -> Option<quic::Session> {
        let mut at = 0;
        while let Some((index, session)) = self.flying.get(at) {
            match session.poll_connected(cx) {
                Poll::Ready(Ok(())) => return Some(self.flying.swap_remove(at).1),
                Poll::Ready(Err(error)) => {
                    self.causes[*index] = Some(error);
                    self.flying.swap_remove(at);
                }
                Poll::Pending => at += 1,
            }
        }
        None
    }

    fn start(&mut self, index: usize) {
        let started = match self.addresses[index] {
            Address::Udp(remote) => self.carrier.dial(self.peer, remote),
            // This node runs no carrier for them.
            Address::Tcp(remote) | Address::Relay { at: remote, .. } => {
                Err(Error::Network {
                    error: env::net::Error::Unreachable { remote },
                })
            }
        };
        match started {
            Ok(session) => {
                self.flying.push((index, session));
                self.causes.push(None);
                self.sleep.reset(self.clock.now() + STAGGER);
            }
            Err(error) => self.causes.push(Some(error)),
        }
    }

    fn unreachable(&mut self) -> Error {
        let causes = self.causes.drain(..);
        let attempts = (self.addresses.iter().copied().zip(causes))
            .map(|(address, cause)| {
                (address, cause.expect("invariant: each attempt ended"))
            })
            .collect();
        Error::Unreachable {
            peer: self.peer,
            attempts,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use sim::node::Node;
    use types::node::PrivateKey;
    use types::time::Span;

    use super::STAGGER;
    use crate::testing::{self, IDLE, address, nodes, spans};
    use crate::tls::public;
    use crate::{Address, Code, Error, Peer};

    const CLIENT: PrivateKey = PrivateKey([1; 32]);
    const SERVER: PrivateKey = PrivateKey([2; 32]);
    const OTHER: PrivateKey = PrivateKey([3; 32]);

    /// An address at port 0, where no datagram can go.
    const PORT_ZERO: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

    /// The error of an address this node has no route to.
    fn no_route(remote: SocketAddr) -> Error {
        Error::Network {
            error: env::net::Error::Unreachable { remote },
        }
    }

    /// Starts a transport for `SERVER` on `node` that accepts one session from
    /// `CLIENT` and waits until the client closes it with code 5.
    fn serve(node: &Node) {
        testing::transport(node, SERVER, |transport, _| async move {
            let session = transport.accept().await.expect("a session");
            assert_eq!(session.peer(), Peer::Node(public(&CLIENT)));
            let closed = Error::PeerClosed { code: Code(5) };
            assert_eq!(session.closed().await, closed);
        });
    }

    /// Starts a transport for `OTHER` on `node` that lives for 3 idle timeouts.
    fn impostor(node: &Node) {
        testing::transport(node, OTHER, |transport, node| async move {
            node.clock().sleep(spans(IDLE, 3)).await;
            drop(transport);
        });
    }

    /// Starts a transport for `CLIENT` on `node` that dials `SERVER` at `addresses`,
    /// expects a session at least `from` and less than `to` after the start, and
    /// closes it with code 5.
    fn dial(node: &Node, addresses: Vec<Address>, from: Span, to: Span) {
        testing::transport(node, CLIENT, move |transport, node| async move {
            let start = node.clock().now();
            let dialed = transport.dial(public(&SERVER), &addresses).await;
            let session = dialed.expect("a session");
            let took = node.clock().now() - start;
            assert!(from <= took && took < to, "{took:?}");
            assert_eq!(session.peer(), Peer::Node(public(&SERVER)));
            session.close(Code(5));
            assert_eq!(session.closed().await, Error::Closed { code: Code(5) });
        });
    }

    /// Starts a transport for `CLIENT` on `node` that dials `SERVER` at `addresses`,
    /// and expects [`Error::Unreachable`] with `attempts`.
    fn unreachable(
        node: &Node,
        addresses: Vec<Address>,
        attempts: Vec<(Address, Error)>,
    ) {
        testing::transport(node, CLIENT, move |transport, _| async move {
            let peer = public(&SERVER);
            let dialed = transport.dial(peer, &addresses).await;
            assert_eq!(dialed.err(), Some(Error::Unreachable { peer, attempts }));
        });
    }

    #[test]
    fn a_dial_gives_the_session_of_a_server_that_proves_its_key() {
        let (mut sim, client, server) = nodes(0);
        serve(&server);
        let addresses = vec![Address::Udp(address(&server))];
        dial(&client, addresses, Span::ZERO, spans(Span::MILLISECOND, 5));
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_silent_address_starts_the_next_after_the_stagger() {
        let (mut sim, client, server) = nodes(0);
        let silent = sim.node(sim::node::Config::default());
        serve(&server);
        let addresses = vec![
            Address::Udp(address(&silent)),
            Address::Udp(address(&server)),
        ];
        let to = Span::from_nanos(STAGGER.nanos() + 5 * Span::MILLISECOND.nanos());
        dial(&client, addresses, STAGGER, to);
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_failed_address_starts_the_next_at_once() {
        let (mut sim, client, server) = nodes(0);
        let other = sim.node(sim::node::Config::default());
        serve(&server);
        impostor(&other);
        let addresses = vec![
            Address::Udp(PORT_ZERO),
            Address::Udp(address(&other)),
            Address::Udp(address(&server)),
        ];
        dial(&client, addresses, Span::ZERO, spans(Span::MILLISECOND, 10));
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_that_no_address_completes_gives_each_cause_in_the_order_started() {
        let (mut sim, client, _) = nodes(0);
        let silent = address(&sim.node(sim::node::Config::default()));
        let impostor_node = sim.node(sim::node::Config::default());
        impostor(&impostor_node);
        let other = address(&impostor_node);
        let addresses = vec![
            Address::Tcp(other),
            Address::Udp(silent),
            Address::Udp(other),
            Address::Udp(PORT_ZERO),
        ];
        let expected = public(&SERVER);
        let attempts = vec![
            (Address::Udp(silent), Error::TimedOut),
            (Address::Udp(other), Error::Authentication { expected }),
            (Address::Udp(PORT_ZERO), no_route(PORT_ZERO)),
            (Address::Tcp(other), no_route(other)),
        ];
        unreachable(&client, addresses, attempts);
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_tries_udp_then_tcp_then_relays_in_the_given_order_within_a_kind() {
        let (mut sim, client, _) = nodes(0);
        let a = address(&client);
        let b = address(&sim.node(sim::node::Config::default()));
        let relay = Address::Relay {
            node: public(&SERVER),
            at: a,
        };
        let unspecified = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 4433);
        let addresses = vec![
            relay,
            Address::Tcp(b),
            Address::Udp(PORT_ZERO),
            Address::Tcp(a),
            Address::Udp(unspecified),
        ];
        let attempts = vec![
            (Address::Udp(PORT_ZERO), no_route(PORT_ZERO)),
            (Address::Udp(unspecified), no_route(unspecified)),
            (Address::Tcp(b), no_route(b)),
            (Address::Tcp(a), no_route(a)),
            (relay, no_route(a)),
        ];
        unreachable(&client, addresses, attempts);
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_with_no_address_is_unreachable_at_once() {
        let (mut sim, client, _) = nodes(0);
        testing::transport(&client, CLIENT, move |transport, node| async move {
            let (start, peer) = (node.clock().now(), public(&SERVER));
            let dialed = transport.dial(peer, &[]).await;
            let attempts = Vec::new();
            assert_eq!(dialed.err(), Some(Error::Unreachable { peer, attempts }));
            assert_eq!(node.clock().now(), start);
        });
        assert_eq!(sim.run(), Ok(()));
    }
}
