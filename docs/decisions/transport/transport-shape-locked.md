- **TRANSPORT SHAPE LOCKED** One session model: prioritized, cancellable streams plus
  optional datagrams. Carriers are adapters: QUIC (noq-proto), TLS over TCP, relay (TLS
  on 443 through designated nodes), and a one-way diode carrier. An adapter that lacks a
  feature emulates it. Reliability is per delivery mode: commands reliable and highest;
  latest drops stale frames by cancel or datagram and keeps `TCP_NOTSENT_LOWAT` small
  over TCP; complete is reliable and ordered with credits; catch-up is lowest. The
  default carrier per traffic class comes from measurement. A program with no node
  key dials nodes through `transport::Client` on the same QUIC carrier: its TLS client
  sends no certificate and pins the node key, so the node sees `Peer::Client`. It
  accepts no session, and its limits are fixed: messages up to `pool.largest()`, a
  window of that or 1 MiB, `streams_max` 1, and idle 30 s. Decided by
  `laptop.architect-2` (#1754, 2026-10-08T09:52:31Z):
  https://github.com/synnaxlabs/foundation/issues/1754#issuecomment-6057273348.
  `Client::new` binds the program's UDP socket at `[::]` port 0 on
  `client::Config::net`, through the same private function in `port` as `Port::bind`,
  and binds no TCP. A `local` field in `client::Config` comes when a program must send
  from a known port or bind one address family. Until then, on a host booted with
  `ipv6.disable=1`, the bind fails with `Error::Network`. Decided by
  `laptop.architect-2` (#1888, 2026-10-09T01:15:58Z):
  https://github.com/synnaxlabs/foundation/issues/1888#issuecomment-6072258451.
