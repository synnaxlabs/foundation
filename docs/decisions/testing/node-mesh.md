- **NODE MESH (#585, 2026-10-08)** `Config::key` is the node's key, beside
  `Config::private_key`; both are patches until #1660 moves them to node-local disk.
  `Config::region: Option<mesh::region::Founding>` gives the region that the node is a
  member of: its prefix, its members (one card has `Config::key`), the voters before the
  first entry of the log, and its founding definitions. The caller gives the same
  region at each start: the node keeps no copy of it. `None` opens no mesh. The `Option`
  is a dark patch: the `None` stays in `node`, and no lower crate gets an `Option` of
  the mesh. PR 4 of #585, which gives the mesh to the hub, makes the region required,
  unless #1660 and #1744 have already taken it out of `Config`. The long-term path takes
  it out of `Config`: the node keeps its membership in its data directory when it founds
  or joins, and reads it at each start.
  With a region, shard 0 opens `mesh::Mesh` on the node's transport after the last shard
  has opened its buffer and before it takes the first session. Its directory is `mesh`
  in the data directory (`mesh::Config::dir`; the directory by `laptop.architect`,
  2026-10-08 03:37 UTC:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051658475; the field
  by `laptop.architect-2`, 03:54 UTC:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051833866, and by
  `laptop.architect`, 04:00 UTC:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051912643). Shard 0
  waits for `Mesh::ended` before it drops `lock` (DATA DIRECTORY LOCK). Each clone of
  the mesh, also one inside the hub, lives in a future that shard 0 drops at the stop.
  PR 4 of #585 keeps this (`laptop.architect-2`, 2026-10-08 07:26 UTC:
  https://github.com/synnaxlabs/foundation/pull/1830#issuecomment-6054871235). A mesh
  that does not open stops the node, and `Node::join` gives `Error::Mesh`, ranked with
  `Error::Buffer` and below `Error::Transport`. Each `wire::Protocol::Mesh` stream of a
  peer that proved a node key goes to `Mesh::serve`, which checks each message against
  the region; the error of `serve` ends only its stream. A mesh stream of a client, or
  of a node with no region, is rejected as NODE PORT says. Shard 0 sets no home yet (PR
  4 of #585). Shard 0 opens the mesh with `Config::region` unchanged, and no caller
  gives definitions yet. From PR 1b of #1744, the code that builds the `Config::region`
  of a founding node gives the root region the definitions that
  `spec::founding::create` gives, and each other region an empty map. A node that joins
  gives the definitions of its join answer (#336). Decided by `laptop.architect` at
  2026-10-08T06:11:30Z
  (https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6053599101), with
  the text for a founding node at 15:44:46Z
  (https://github.com/synnaxlabs/foundation/pull/1904#issuecomment-6063609356).
  `mesh::region::Founding` replaced `node::Region`, which copied three fields of
  `mesh::Config`, so `node` maps no `mesh` value by hand (#1859). Decided by
  `laptop.architect` at 2026-10-08T10:23:48Z and 10:34:37Z
  (https://github.com/synnaxlabs/foundation/pull/1857#issuecomment-6057800438,
  https://github.com/synnaxlabs/foundation/issues/1859#issuecomment-6057975061).
  The type lands as PR 1 of #1209, ordered by `laptop.coordinator` at
  2026-10-08T15:21:38Z
  (https://github.com/synnaxlabs/foundation/issues/1859#issuecomment-6063144369) and
  approved by `laptop.architect` at 15:26:10Z
  (https://github.com/synnaxlabs/foundation/issues/1209#issuecomment-6063246600).
  Supersedes the trigger of 10:23:48Z, the first of PR 1 of #1744 and the join answer
  of #336. Lost: `Node::found(region)` at run time, which needs a second open path and
  a node that runs with no region before it; the key in `node::Region`, because a
  node's identity is not region data, and PR 4 needs it with no region. Decided by
  `laptop.architect-2` (2026-10-08 03:37 UTC):
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051655452, on the
  plan https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051630943. A
  mesh whose group stops stops the node, and `Node::join` gives `Error::Group` with
  the cause. Of a transport that stops and a group that stops, `join` gives the one
  that the node sees first; of two that stop at one instant, either can be first
  (#1780). Supersedes the deferral of
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051655452. Lost:
  `Mesh::stopped()`, because `Watch::next` gives the stop as its contract and one
  caller does not justify a new `mesh` item; add `Mesh::stopped` when a second caller
  needs the stop of the group and reads no home, and ask `laptop.architect` for it.
  Decided by `laptop.architect-2` at 2026-10-08T17:27:20Z
  (https://github.com/synnaxlabs/foundation/issues/1780#issuecomment-6065408760), and
  changed by `laptop.architect-2` at 17:32:00Z
  (https://github.com/synnaxlabs/foundation/pull/1936#issuecomment-6065487136): the
  stop has its own variant. Lost: `Error::Mesh` with `mesh::Error::Stopped`, which
  gives one variant two meanings. The rank by what the node sees first supersedes item
  2 of https://github.com/synnaxlabs/foundation/pull/1936#issuecomment-6065487136, and
  the private rank of shard 0's stop is a fixed tie-break with no contract. Decided by
  `laptop.architect-2` at 17:49:13Z
  (https://github.com/synnaxlabs/foundation/pull/1936#issuecomment-6065780217), and
  for `Node::spawn` at 17:58:35Z
  (https://github.com/synnaxlabs/foundation/pull/1936#issuecomment-6065936160): a
  panic gives `Error::Panicked` unless the node saw the transport or the group stop
  first. Lost: "the transport's when both stop at once", because the node sees each
  stop only at its next poll, so of two stops at one instant either can come first
  (the breaker,
  https://github.com/synnaxlabs/foundation/pull/1936#issuecomment-6065770160 and
  https://github.com/synnaxlabs/foundation/pull/1936#issuecomment-6065929818).
