- **TRANSPORT SURFACE (#45, 2026-10-04)** One `Transport` per shard dials and accepts;
  the node's sockets and relays sit in one node-level part, `transport::Port` (ONE
  PORT PER NODE), which `node` binds once and splits into one part for each shard. A
  `Session` goes to one peer over one path, direct or relayed, fixed for its life, and
  runs every class on one carrier. A second carrier for some classes waits for the
  measurement in TRANSPORT SHAPE LOCKED, which must show that `Latest` p99 holds while
  `CatchUp` runs on the other carrier. Until then, QUIC is the one carrier (r19): it
  keeps `Latest` p99 low while bulk shares the connection, at 1.5 times the CPU per byte
  of TLS over TCP. The person decided on 2026-10-05: "QUIC" (#10). Streams carry whole
  messages in pool blocks, not bytes; the QUIC carrier benchmark decides whether decode
  reads chunks in place instead. A stream reaches the peer with its first message, and a
  `Sender` dropped without `finish` resets it. A `Sender` can also send without waiting
  (`try_send`): it gives the message back whole when the stream cannot take it now, and
  it never resets the stream. Each stream has a `Class` (`Command`, `Latest`,
  `Complete`, `CatchUp`) that sets its priority and preferred carrier. A peer is a node
  key or a `Client` (an SDK, proved by its signed hello, CLIENT HELLO). Callers admit
  peers, dispatch streams (STREAM DISPATCH), and cancel stale latest frames. Builds on
  SIM NETWORK. Proposed by `network` in #45; approved by the coordinator on PR #53.
  `Transport::public_key` gives the key that the transport proves to each peer, so
  `Mesh::open` can check it against the public half of the private key of its config
  (MESH SURFACE). Decided by laptop.architect and laptop.architect-2 (#1587, 2026-10-07
  19:55 UTC):
  https://github.com/synnaxlabs/foundation/issues/1587#issuecomment-6045695196,
  https://github.com/synnaxlabs/foundation/issues/1587#issuecomment-6045706124.
  `Transport::new` takes the smaller of `Config::message_bytes_max` and `pool.largest()`
  as the message limit, so no caller clips it. The hello, the QUIC datagram limit, and
  each check on the send side use that limit. When it is below 1472 because of the pool,
  `Error::Config` names `pool`. Lost: an `Option` field whose `None` means
  `pool.largest()`, which keeps the error and adds a case to each caller. Decided by
  `laptop.architect-2` (#1659, 2026-10-08T06:05:25Z):
  https://github.com/synnaxlabs/foundation/issues/1659#issuecomment-6053512557.
