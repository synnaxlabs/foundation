- **HANDOFF RECORD (#191)** The home records each handoff that `Gate::handoff` gives
  (GATE RULES) as a buffer entry on the live path of the index, with tag `HANDOFF`, no
  samples (`samples: None`, `len` 0 on disk), and `first` at the live tail. It records a
  handoff after the gate input that gave it and before the next stored frame of its
  index. It records it before the next input too, except for a handoff with no room or
  after a failed commit (`laptop.architect`, #2318, 2026-10-11T10:46:58Z:
  https://github.com/synnaxlabs/foundation/pull/2318#issuecomment-6108217840).
  Supersedes "before the next input or frame" of
  https://github.com/synnaxlabs/foundation/pull/402. Its bytes are empty when no writer
  holds control, else `[authority: u8]`, then the holder's `via` node key as 16
  little-endian bytes, then the holder's subject as UTF-8; the entry length gives the
  subject's length. The node key went into the layout in place, with format version 1
  and the same tag, as no buffer has shipped (`laptop.architect`, #3104,
  2026-10-19T14:00:04Z, answer 5:
  https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6269021946). Supersedes "`[authority: u8]` then the holder's subject as UTF-8" of https://github.com/synnaxlabs/foundation/blob/a1704faf5/docs/decisions/storage/handoff-record.md. A
  restart or a failover starts the gate from the last record (X18): `Gate::recover` with
  its holder, or `Gate::new` when it names none. `Shard::open` restores each gate, with
  `GRACE` (10 s) from the open, and the gate moves in at the first carry of its slot,
  also after the grace (`laptop.architect`, #275, 2026-10-10T04:04:50Z:
  https://github.com/synnaxlabs/foundation/issues/275#issuecomment-6093580361). The
  experiment that sets the gate grace (`docs/decisions/open/parameters.md`), or a test
  that needs another value, makes `GRACE` a `Config` field (`laptop.architect`, #275,
  2026-10-10T04:04:50Z, item 4). A lease or a grace that ends with no call records its
  handoff at its end: the hub's gate task calls `Shard::advance` at `Shard::deadline`,
  on the monotonic clock of the hub's `clock::Reader` (`laptop.architect`, #2307,
  2026-10-10T22:30:30Z:
  https://github.com/synnaxlabs/foundation/issues/2307#issuecomment-6102872368).
  `Shard::advance` records the handoff of each carried index that came due, with or
  without an open writer: `Shard::carry` takes the channel key of the index with its
  slot, and the hub wakes its gate task at each carry that `Shard::carry` starts, as
  only such a carry can make `Shard::deadline` earlier (`laptop.architect`, #2314,
  2026-10-10T22:57:44Z, 2026-10-10T23:13:18Z, 2026-10-10T23:22:58Z, and
  2026-10-11T01:00:41Z:
  https://github.com/synnaxlabs/foundation/issues/2314#issuecomment-6103084268,
  https://github.com/synnaxlabs/foundation/issues/2314#issuecomment-6103202889,
  https://github.com/synnaxlabs/foundation/issues/2314#issuecomment-6103276258, and
  https://github.com/synnaxlabs/foundation/issues/2314#issuecomment-6103996718; the wake
  at a carry that `Shard::carry` starts: `laptop.architect`, #2403,
  2026-10-11T15:08:57Z, finding 1:
  https://github.com/synnaxlabs/foundation/pull/2403#issuecomment-6110418644). Lost: the
  channel key in `buffer::Stored`, which each read on the data path makes, for one
  control case. Supersedes "An index with no open writer records its handoff at the next
  open of a writer on it" of
  https://github.com/synnaxlabs/foundation/issues/2307#issuecomment-6102995411. Before
  the node first has mesh time, such a handoff waits as one with no room does
  (`laptop.architect`, #2314, 2026-10-11T12:52:20Z, item 1:
  https://github.com/synnaxlabs/foundation/issues/2314#issuecomment-6109217872). A
  handoff is not on disk while the ring or the pool has no room for it, and a restart or
  a failover in that time starts the gate from the last record on disk, as above. Only
  room held back for one handoff of each index, in the pool and in the ring, closes
  this. Trigger to look again: a measured case where a ring or a pool stays full across
  a handoff in a workload on the milestone path. Until #2385, such a handoff waits for
  the next write that is not a resend, writer open or close, or end of a lease on its
  index (INDEX FRAMES), also after the room comes back or mesh time first comes
  (`laptop.architect`, #2385, 2026-10-11T08:16:48Z, the OK of the deferral, item 4, and
  the trigger of its "Lost" list:
  https://github.com/synnaxlabs/foundation/issues/2385#issuecomment-6107045587; the
  words for a restart in that time and the list of calls: `laptop.architect`, #2318,
  2026-10-11T10:46:58Z,
  https://github.com/synnaxlabs/foundation/pull/2318#issuecomment-6108217840; "or mesh
  time first comes": `laptop.architect`, #2314, 2026-10-11T13:00:55Z, item 1:
  https://github.com/synnaxlabs/foundation/issues/2314#issuecomment-6109289345). A shed
  keeps it for the next carry of the index (HOME SURFACE). The shard advances and
  records only the gate of an index that it carries. So when a grace ends before the
  first carry of its index, or after a shed, the holder stays. The first
  `Shard::advance` or writer open after the next carry drops it and records the handoff.
  A restart before that carry gives it a grace again. Lost: a deadline and a record for
  a gate that the shard keeps. It needs the key of an index before its first carry, and
  an append to an index that the hub shed. Trigger to look again: a case on the
  milestone path where a holder that had a grace in an earlier run refuses a writer
  (`laptop.architect`, #2314, 2026-10-11T13:00:55Z, item 2:
  https://github.com/synnaxlabs/foundation/issues/2314#issuecomment-6109289345). A trim
  keeps the newest handoff of each index (STORE TRIM; `laptop.architect`, #406,
  2026-10-14T01:56:45Z:
  https://github.com/synnaxlabs/foundation/issues/406#issuecomment-6159164617).
  Retention deletes nothing (decided by `laptop.architect`, 2026-10-07T12:30:53Z and
  2026-10-07T12:59:37Z:
  https://github.com/synnaxlabs/foundation/issues/1377#issuecomment-6037946637 and
  https://github.com/synnaxlabs/foundation/issues/1377#issuecomment-6038431739).
  Supersedes https://github.com/synnaxlabs/foundation/pull/402 in its clause that
  retention can remove that record. The layout is part of the disk format version (C9d).
  Copy mode checks each record once where remote records enter (X43), and the read after
  it panics on a bad record. Decided by the `write-path` builder; approved by the
  coordinator (#191).
