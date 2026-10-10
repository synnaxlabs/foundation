- **NODE MESH (#585, 2026-10-08)** The node's key and private key come from the file
  `node.key` (NODE PORT, amended for #1660, by `laptop.architect-2`, 19:52 UTC:
  https://github.com/synnaxlabs/foundation/issues/1660#issuecomment-6067866831). This
  supersedes `Config::key` and `Config::private_key` of
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051655452.
  `Config::region: Option<mesh::region::Founding>` gives the region that the node is a
  member of: its prefix, its members (one card has the node's key), the voters before
  the first entry of the log, and its founding definitions. The caller gives the same
  region at each start: a start whose mesh log holds no record keeps it in the data
  directory, and a start whose log holds a record and that gives another region stops
  the node with `mesh::Error::Founding` (#1209, `laptop.architect-2`,
  2026-10-08T16:32:10Z:
  https://github.com/synnaxlabs/foundation/issues/1209#issuecomment-6064460084; the
  condition on the log, `laptop.architect-2`, 2026-10-09T23:58:45Z:
  https://github.com/synnaxlabs/foundation/pull/2200#issuecomment-6091323223, and
  `laptop.architect`, 2026-10-10T00:00:37Z:
  https://github.com/synnaxlabs/foundation/pull/2200#issuecomment-6091342010). `None`
  opens no mesh, and the hub of each task then gets no mesh: a node runs with no region
  before it founds or joins one. Changed by `laptop.architect`, 2026-10-08T18:42:42Z
  (https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6066677536). The
  long-term path takes it out of `Config`: the node keeps its membership in its data
  directory when it founds or joins, and reads it at each start.
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
  `node::create_key(files, key, private_key)` makes `node.key` in the data directory of
  a node that has not started, so the `acceptance` lab knows each key before the first
  start and puts it in the founding. It writes with the code of `identity`, so the file
  has one owner, and a file with no bytes or with 68 zero bytes counts as no key. A
  68-byte file that holds other bytes gives `Error::Directory` with `Exists`, and a file
  of another length that is not 0 gives it with `Length`; nothing is written over
  either. There is no new `Error` variant, as the advice of `Error::Key` is wrong for
  this case, and no idempotent form: the lab calls it once, in `Lab::start`. The write
  runs the simulation to its end: a node that runs never ends, and a task of a test runs
  before its time. So a `Lab::start` after the first `Lab::run` or a task of a test
  panics. No scenario adds a node after a run. Trigger: a scenario that does needs a new
  ruling on where the lab calls `create_key`. Trigger: when #1744 lands, the lab founds
  its region through the node, and `create_key` stays only if a tool still needs it.
  Until a tool calls it, `create_key` is behind the `sim` feature (`laptop.architect-2`,
  2026-10-09T03:42:57Z,
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6073816041).
  Trigger: a tool that needs a node's key before its first start takes it out of
  `sim`, with a new ruling.
  Lost: a second copy of the format in `acceptance`; a restart of each node and a read
  of its key from outside `node`; a form that gives back only the public key, as the lab
  signs each card with the private key. Decided by `laptop.architect-2` at
  2026-10-08T23:23:28Z
  (https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6071015421), with
  the start rule and its trigger approved by `laptop.architect-2`
  (2026-10-09T00:36:11Z, #1962,
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6071836755), and the
  file with no bytes approved by `laptop.architect-2` (2026-10-09T00:51:50Z,
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6072002986).
  Amended (2026-10-09, #1756): `node` builds one `ops::Node` on shard 0 when the mesh
  opens, and keeps it until the stop. `Node::operate` gives a task that value
  (`Rc<ops::Node>`), the value that `ops::serve` of #1744 takes, so the lab runs the
  production value and the node has one key maker. A new channel key is a UUIDv7 at mesh
  time. `ops::Node` holds the handles that the operation table uses: the mesh, the key
  maker, the front ends, and the connector kinds. `new` takes `ops::FrontEnds`, which
  holds at least one front end (`laptop.architect-2`, 2026-10-09T05:10:54Z,
  https://github.com/synnaxlabs/foundation/pull/2078#issuecomment-6074720909).
  Supersedes "`new` refuses an empty table of front ends" of
  https://github.com/synnaxlabs/foundation/issues/1756#issuecomment-6072660664. `node`
  may take `config-hcl`, as the composition root. Trigger: when the
  lab reaches the node through the CLI, remove `Node::operate` if nothing else calls it.
  Lost: a `call(body)` dispatch, which is a table entry of #1744; typed methods on
  `ops::Node` that make `Output`, `Applied`, `Error`, and `Problem` public; a `hub` that
  gives the mesh. Decided by `laptop.architect-2` (2026-10-09T01:55:14Z,
  https://github.com/synnaxlabs/foundation/issues/1756#issuecomment-6072660664).
  Amended in review (2026-10-09, #2078): the key time is the best guess of mesh time
  (`Measurement::time`), which never goes back, and 0 before the node has mesh time or
  before 1970: the time only orders keys, and the random bits make each key unique.
  `ops::Node::mesh` gives the mesh, so the lab reads the home of a channel. `node` also
  takes `document` and `connector`. The trigger above also makes `ops::Node::plan`,
  `apply`, and `mesh` crate-private. Approved by `laptop.architect` for the key time
  rule and `ops::Node::mesh` (2026-10-09T03:33:40Z,
  https://github.com/synnaxlabs/foundation/pull/2078#issuecomment-6073726053). Approved
  by `laptop.architect-2` for the `node` and `ops` parts (2026-10-09T03:40:04Z,
  https://github.com/synnaxlabs/foundation/pull/2078#issuecomment-6073787912).
