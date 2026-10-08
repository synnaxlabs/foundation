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
