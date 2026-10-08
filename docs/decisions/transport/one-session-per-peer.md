- **ONE SESSION PER PEER (#1363, 2026-10-07)** Each `Transport` keeps one session to
  each node, shared by every caller. `Transport::dial` gives the open one, from a dial
  or from the peer, and dials only when it has none. `accept` also gives each session
  that a dial made. No public item is new.
  1. The table. Each `Transport` maps a node key to its open session and to the dial
     that runs for it. A session that is closing or closed is not open, so the next
     `dial` dials again. A refusal is a close. The table holds no handle to an open
     session, only to a session that waits for `accept`, so a session still closes
     when its last handle drops.
  2. One attempt. The dial runs as a task on `tasks`, not in the caller's future, so the
     first caller can drop and the others still get the result. Its session goes to the
     table and to `accept` even when no caller waits. A dial that fails gives the
     session that the peer opened meanwhile, if one did.
  3. `accept` gives every session to a node. STREAM DISPATCH runs one dispatcher per
     session, and the peer opens streams on a dialed session too, so `node` must get
     it. The node admits a dialed session as it admits an accepted one.
  4. Two sessions to one peer. A node dials only when it has no open session to the
     peer. When one node dialed both, the newer wins. When each node dialed one, the
     one that the lower key dialed wins. The higher node takes the lower peer's session
     at once, and stops or closes its own. The lower node holds the higher peer's
     session (not in the table, not given to `accept`; its streams wait) while its own
     dial runs, or until the peer acknowledges a ping sent after the higher session
     arrived. Its own completes or answers: it closes the held one. Its own fails or
     ends: the held one wins, and goes to the waiters and to `accept`. The loser closes
     with `Code(0)`. A restarted peer answers a ping on an old session with a
     stateless reset (`cid::Issuer`), so the old session ends within one round trip.
  5. Clients. A client session never enters the table, and `dial` never gives one. Two
     sessions from one client both stay open.
  6. Shards. The table is per `Transport`, so per shard. Until #77, a node runs one
     shard, so this is one session per node pair. #77 keeps the rule across shards.

  Lost: a table in `node`, given to each protocol, with `dial` unchanged. A layer 2
  crate cannot call up into `node`, so it needs a callback, and the tie-break would see
  sessions only after `accept`. Lost: a new method beside `dial` (a caller of `dial`
  makes the extra sessions), and a rename of `dial`. Rejected tie-breaks: the newest
  wins (the two nodes can see the two sessions complete in different orders and close
  both, again and again), keep both (the cost that PROTOCOL HEADER rejected), and a
  start number in the hello (a wire change, and the ping gives the same result within
  one round trip). Decided by `laptop.architect-2` at 2026-10-07T12:00:38Z:
  https://github.com/synnaxlabs/foundation/issues/1363#issuecomment-6037453233.
