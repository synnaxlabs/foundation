//! The dial of [`Transport::dial`](crate::Transport::dial): one attempt for each
//! address, staggered, until one gives a session.

use std::future::poll_fn;
use std::net::SocketAddr;
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
/// [`Error::Network`] when the socket is broken, or breaks before an attempt connects,
/// or [`Error::Unreachable`] with each attempt's cause, in the order started, when
/// none connects.
pub(crate) async fn dial(
    carrier: &quic::Carrier,
    clock: &Clock,
    peer: PublicKey,
    addresses: &[Address],
) -> Result<quic::Session, Error> {
    carrier.check()?;
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
        // The first attempt starts without it, and resets it.
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
            if let Some(ended) = self.poll_flying(cx) {
                return Poll::Ready(ended);
            }
            let next = self.causes.len();
            if next == self.addresses.len() {
                if self.flying.is_empty() {
                    return Poll::Ready(Err(self.unreachable()));
                }
                return Poll::Pending;
            }
            let due = next == 0 || self.causes[next - 1].is_some();
            if !due && Pin::new(&mut self.sleep).poll(cx).is_pending() {
                return Poll::Pending;
            }
            if let Err(error) = self.start(next) {
                return Poll::Ready(Err(error));
            }
        }
    }

    /// Takes each attempt that ended. Gives the session of one that connected, or
    /// [`Error::Network`] when the socket broke and none connected.
    fn poll_flying(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Option<Result<quic::Session, Error>> {
        let (mut at, mut broken) = (0, None);
        while let Some((index, session)) = self.flying.get(at) {
            match session.poll_connected(cx) {
                Poll::Ready(Ok(())) => return Some(Ok(self.flying.swap_remove(at).1)),
                // A later attempt can have connected before the break.
                Poll::Ready(Err(error @ Error::Network { .. })) => {
                    broken = Some(error);
                    at += 1;
                }
                Poll::Ready(Err(error)) => {
                    self.causes[*index] = Some(error);
                    self.flying.swap_remove(at);
                }
                Poll::Pending => at += 1,
            }
        }
        broken.map(Err)
    }

    /// Starts the attempt at `index`.
    ///
    /// # Errors
    ///
    /// [`Error::Network`] when the socket broke.
    fn start(&mut self, index: usize) -> Result<(), Error> {
        // Also for an unroutable address, so the kind of the next address does not
        // decide whether a break ends the dial.
        self.carrier.check()?;
        match self.addresses[index] {
            Address::Udp(remote) if routable(remote) => {
                let session = self.carrier.dial(self.peer, remote)?;
                self.flying.push((index, session));
                self.causes.push(None);
                self.sleep.reset(self.clock.now() + STAGGER);
            }
            // This node runs no carrier for TCP or relays.
            _ => self.causes.push(Some(Error::Unroutable)),
        }
        Ok(())
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

/// Whether a datagram can go to `remote`: its port is not 0 and its IP is specified.
fn routable(remote: SocketAddr) -> bool {
    remote.port() != 0 && !remote.ip().is_unspecified()
}

#[cfg(test)]
mod tests {
    use std::future::poll_fn;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::pin::pin;
    use std::sync::{Arc, Mutex};
    use std::task::Poll;

    use sim::node::Node;
    use types::node::PrivateKey;
    use types::time::Span;

    use crate::testing::{self, IDLE, address, nodes, spans};
    use crate::tls::public;
    use crate::{Address, Code, Error, Peer, Session};

    const CLIENT: PrivateKey = PrivateKey([1; 32]);
    const SERVER: PrivateKey = PrivateKey([2; 32]);
    const OTHER: PrivateKey = PrivateKey([3; 32]);

    /// An address at port 0, where no datagram can go.
    const PORT_ZERO: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

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

    /// Dials `SERVER` from `CLIENT` in a run from `value`, after a silent address
    /// when `silent`, and breaks the client's socket the instant the server's attempt
    /// connects. Gives whether the server accepted, which it does only once the
    /// client's handshake finished, and what the dial gave.
    fn break_as_it_connects(value: u64, silent: bool) -> (bool, Result<Peer, Error>) {
        let (mut sim, client, server) = nodes(value);
        let quiet = sim.node(sim::node::Config::default());
        let (accepted, dialed) =
            (Arc::new(Mutex::new(false)), Arc::new(Mutex::new(None)));
        let flag = Arc::clone(&accepted);
        testing::transport(&server, SERVER, move |transport, node| async move {
            let mut accept = pin!(transport.accept());
            let mut sleep = pin!(node.clock().sleep(spans(IDLE, 3)));
            let ok = poll_fn(|cx| match accept.as_mut().poll(cx) {
                Poll::Ready(accepted) => Poll::Ready(accepted.is_ok()),
                Poll::Pending => sleep.as_mut().poll(cx).map(|()| false),
            });
            *flag.lock().expect("a lock") = ok.await;
        });
        let (mut addresses, mut at) = (vec![Address::Udp(address(&server))], 1_500_000);
        if silent {
            addresses.insert(0, Address::Udp(address(&quiet)));
            at += 250_000_000;
        }
        let out = Arc::clone(&dialed);
        testing::transport(&client, CLIENT, move |transport, node| async move {
            let result = transport.dial(public(&SERVER), &addresses).await;
            let peer = result.as_ref().map(Session::peer).map_err(Clone::clone);
            *out.lock().expect("a lock") = Some(peer);
            node.clock().sleep(spans(IDLE, 3)).await;
        });
        testing::shard(&client, OTHER, move |_, node| async move {
            node.clock().sleep(Span::from_nanos(at)).await;
            node.fail_udp(address(&node));
        });
        assert_eq!(sim.run(), Ok(()));
        let accepted = *accepted.lock().expect("a lock");
        (
            accepted,
            dialed.lock().expect("a lock").take().expect("a dial"),
        )
    }

    #[test]
    fn an_attempt_that_connected_before_the_break_wins_with_or_without_a_silent_one() {
        for silent in [false, true] {
            let mut connected = 0;
            for value in 0..64 {
                let (accepted, dialed) = break_as_it_connects(value, silent);
                if accepted {
                    connected += 1;
                    let peer = Ok(Peer::Node(public(&SERVER)));
                    assert_eq!(dialed, peer, "silent {silent}, value {value}");
                }
            }
            assert!(connected > 0, "silent {silent}");
        }
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
        let ms = Span::MILLISECOND;
        dial(&client, addresses, spans(ms, 250), spans(ms, 255));
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
    fn a_failed_address_then_a_silent_one_waits_the_stagger_before_the_next() {
        let (mut sim, client, server) = nodes(0);
        let silent = sim.node(sim::node::Config::default());
        serve(&server);
        let addresses = vec![
            Address::Udp(PORT_ZERO),
            Address::Udp(address(&silent)),
            Address::Udp(address(&server)),
        ];
        let ms = Span::MILLISECOND;
        dial(&client, addresses, spans(ms, 250), spans(ms, 255));
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_dial_with_no_address_on_a_broken_socket_gives_why_it_broke() {
        let (mut sim, client, _) = nodes(0);
        testing::transport(&client, CLIENT, move |transport, node| async move {
            node.fail_udp(address(&node));
            // The carrier sees the break when its task next polls the socket.
            node.clock().sleep(Span::MILLISECOND).await;
            let dialed = transport.dial(public(&SERVER), &[]).await;
            let broken = Error::Network {
                error: env::net::Error::Io { code: 5 },
            };
            assert_eq!(dialed.err(), Some(broken));
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_socket_that_breaks_between_attempts_ends_the_dial() {
        let (mut sim, client, _) = nodes(0);
        let impostor_node = sim.node(sim::node::Config::default());
        impostor(&impostor_node);
        let other = address(&impostor_node);
        let silent = address(&sim.node(sim::node::Config::default()));
        testing::transport(&client, CLIENT, move |transport, node| async move {
            let addresses = [Address::Udp(other), Address::Udp(silent)];
            let mut dialed = pin!(transport.dial(public(&SERVER), &addresses));
            let started =
                poll_fn(|cx| Poll::Ready(dialed.as_mut().poll(cx).is_pending()));
            assert!(started.await);
            // The first attempt fails, and the socket breaks, while the dial is not
            // polled. So the next start meets the break.
            node.clock().sleep(spans(Span::MILLISECOND, 10)).await;
            node.fail_udp(address(&node));
            node.clock().sleep(Span::MILLISECOND).await;
            let broken = Error::Network {
                error: env::net::Error::Io { code: 5 },
            };
            assert_eq!(dialed.await.err(), Some(broken));
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_socket_that_breaks_before_an_unroutable_attempt_ends_the_dial() {
        let (mut sim, client, _) = nodes(0);
        let impostor_node = sim.node(sim::node::Config::default());
        impostor(&impostor_node);
        let other = address(&impostor_node);
        testing::transport(&client, CLIENT, move |transport, node| async move {
            let addresses = [Address::Udp(other), Address::Udp(PORT_ZERO)];
            let mut dialed = pin!(transport.dial(public(&SERVER), &addresses));
            let started =
                poll_fn(|cx| Poll::Ready(dialed.as_mut().poll(cx).is_pending()));
            assert!(started.await);
            node.clock().sleep(spans(Span::MILLISECOND, 10)).await;
            node.fail_udp(address(&node));
            node.clock().sleep(Span::MILLISECOND).await;
            let broken = Error::Network {
                error: env::net::Error::Io { code: 5 },
            };
            assert_eq!(dialed.await.err(), Some(broken));
        });
        assert_eq!(sim.run(), Ok(()));
    }

    #[test]
    fn a_socket_that_breaks_during_a_dial_ends_it() {
        let (mut sim, client, _) = nodes(0);
        let silent = address(&sim.node(sim::node::Config::default()));
        testing::transport(&client, CLIENT, move |transport, node| async move {
            let (start, addresses) = (node.clock().now(), [Address::Udp(silent)]);
            let dialed = transport.dial(public(&SERVER), &addresses).await;
            let broken = Error::Network {
                error: env::net::Error::Io { code: 5 },
            };
            assert_eq!(dialed.err(), Some(broken));
            assert_eq!(node.clock().now() - start, spans(Span::MILLISECOND, 10));
        });
        assert_eq!(sim.run_for(spans(Span::MILLISECOND, 10)), Ok(()));
        client.fail_udp(address(&client));
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
            (Address::Udp(PORT_ZERO), Error::Unroutable),
            (Address::Tcp(other), Error::Unroutable),
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
            (Address::Udp(PORT_ZERO), Error::Unroutable),
            (Address::Udp(unspecified), Error::Unroutable),
            (Address::Tcp(b), Error::Unroutable),
            (Address::Tcp(a), Error::Unroutable),
            (relay, Error::Unroutable),
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
