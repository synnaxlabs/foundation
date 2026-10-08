- **ONE PORT PER NODE (2026-10-04)** A node listens on one UDP port and one TCP port on
  the same port number, however many shards it runs, so each site's firewall needs one
  known port per conduit. Each QUIC connection belongs to one shard, and every
  connection ID a node issues encodes that shard. A receive loop on one shard reads the
  UDP socket in batches and hands each batch to the owning shard over the C2 ring; every
  shard sends on the same socket. The TCP listener accepts and moves each stream to its
  shard. `env::net` therefore splits a UDP socket into a receive half with one owner and
  a send half that any shard may use, and `sim` models the split. Rejected: a port per
  shard (a port range in every firewall), kernel reuse-port hashing (routes by address,
  breaks on NAT rebinding), and one shard doing all network work. If the receive loop
  saturates on Linux, add a reuse-port group steered by the same connection ID. Decided
  by the design session under the architecture delegation (#53). The same port number
  (2026-10-06): with port 0, UDP takes a free port and TCP binds the same one. When TCP
  finds it in use, the node closes the UDP socket and tries a new port, up to 8 tries,
  then gives the last error: TCP and UDP have separate port spaces, and no OS call gives
  a port free in both. A fixed port that fails gives its error at once. Approved by the
  coordinator on #990.
