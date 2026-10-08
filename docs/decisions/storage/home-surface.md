- **HOME SURFACE (#963)** The public surface of `home` names only `types`, `env`,
  `codec`, and `home` items, apart from three. `Config`, which only `node` builds, names
  `buffer` and `clock` types. `Shard::pool` gives a `block::Pool`, the pool of the
  shard's buffer. `block` is in the `hub` row. A writer's frames come from that pool,
  so `hub` takes no pool of its own and the two cannot differ (architect,
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6031955051).
  `home::reader` re-exports the `delivery` values that the surface names: `Next`,
  `Position`, `Error`, `named::Key`, and `complete::Charge` (`laptop.architect`,
  2026-10-08T11:12:45Z:
  https://github.com/synnaxlabs/foundation/pull/1863#issuecomment-6058607367).
  Supersedes the clause "apart from `Config`" of
  https://github.com/synnaxlabs/foundation/issues/963#issuecomment-6031464116, which the
  `Shard::pool` ruling above made two. `Config` takes no pool: the shard uses
  `Buffer::pool()`. It takes one `clock: clock::Reader` for monotonic and mesh time. The
  shard is the only writer of the buffer in `Config`: the caller gives it with no entry
  that waits for a commit. The condition is stated, not checked: `node` appends nothing
  before `Shard::new`, and `Config` takes the buffer by value, so no later append can
  come from outside (architect,
  https://github.com/synnaxlabs/foundation/pull/1130#issuecomment-6033691871; lost: a
  check in `Shard::new`). `replica` (X13) and copy mode (X43) are out of the MVP. Their
  PR decides how `replica` gets to the buffer of a shard and what `committed` waits for.
  Until then, the shard is the only writer (architect,
  https://github.com/synnaxlabs/foundation/pull/1130#issuecomment-6034204295).
  `home::Error` holds only what `write` gives, and each other call has its own error.
  Conversions from `control` errors are private. The `hub` row stays as it is. `Shard`
  gives no stored seq until a caller needs one (architect review,
  https://github.com/synnaxlabs/foundation/pull/1130#issuecomment-6031908363). Lost:
  `control` and `delivery` in the `hub` row, because `hub` then knows how the home is
  built and a `control` change becomes a `hub` change. Lost: no call surface, with
  requests through a ring, because on one shard a call costs nothing and a message costs
  a copy and a wake, and it adds a second protocol beside `wire`. Lost: one reader key
  and a panic at a grant to a latest reader, because the precondition is not in the
  type. Lost: one `home::Error` for every call, because `write` would list `Unsynced`
  and `Lease`, which it never gives. The plan has the full text
  (https://github.com/synnaxlabs/foundation/issues/963#issuecomment-6022924709). Decided
  by the architect, #963
  (https://github.com/synnaxlabs/foundation/issues/963#issuecomment-6031464116).
  Amended (2026-10-08T22:24:43Z, #2020): `Shard::shed` stops carrying an index, the pair
  of `carry`. Its control gate goes, and with it a handoff that waits for room. Each
  named reader of the index stops holding its position. Its frames stay in the buffer,
  and a later `carry` of the slot continues each path where it stood: the live path
  continues after the seq of each lost frame. It panics on an open writer or reader of
  the index: the hub ends each session on the index first. A shed frees the place of the
  index in the shard, and the index at the last place moves there. The hub sheds an
  index only when its key leaves the definitions; a rename or a changed definition at
  the same key ends its sessions and keeps the index. No reader key is given twice, also
  after a shed and a carry: `delivery::Readers::end` gives the number after each key of
  a shed index, and panics while a record waits to be taken, and the shard carries each
  index with `Readers::after` at the highest of these. So a hub finds a session by its
  home key alone. Decided by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/issues/2020#issuecomment-6070259814, and
  the `delivery` items:
  https://github.com/synnaxlabs/foundation/issues/2020#issuecomment-6070368046).
