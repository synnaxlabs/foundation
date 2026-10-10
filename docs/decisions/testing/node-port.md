- **NODE PORT (2026-10-07)** `Node::start` binds the node's one port at `Config::listen`
  on `Config::net` before any shard starts; a failed bind starts no shard, and
  `Node::join` gives `Error::Port`. The port's one part (#77) moves to shard 0, which
  builds the transport with the node's private key once the last shard has opened its
  buffer (X42), so the node takes no session before that. Its limits are patches until
  #1662 makes them settings, as LIMITS of SHARD HOMES is: window 1 MiB, 64 streams of
  each kind, idle 30 s, and messages of the smaller of 64 KiB and the pool's largest
  block. Each session runs in its own future, and each stream of it reads its header in
  its own future, so a late header delays no other stream. One exhaustive `match` on
  `wire::Protocol` in `node` routes each stream; until a protocol has a server, its arm
  stops the stream with `Code(wire::header::REJECTED)` and resets the reply half with
  the same code, as for a header that does not decode, or for a first message with bytes
  after the header. The node reads no datagram until the first protocol that takes
  datagrams has a server (#1661). The node admits each peer that completes the
  handshake, within the bound on sessions of #1628 (below); the mesh checks each message
  of a mesh stream (NODE MESH). With no mesh, at the stop, each session and stream
  future drops, then the transport. A header has 10 s (#1628, below).
  Decided by `laptop.architect-2` (2026-10-07 21:09 UTC):
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6046900669, on the
  plan https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6046861267;
  datagrams and the key, by `laptop.architect-2` (2026-10-07 23:26 UTC):
  https://github.com/synnaxlabs/foundation/pull/1649#issuecomment-6048898047.
  Amended (2026-10-07, #1649 round 1, a finding of `performance` that
  `laptop.integrator-1` deferred, 23:41 UTC): the window caps a session at the window
  over the round trip, about 21 MB/s at 50 ms, until #1662 sizes it from the
  bandwidth-delay product. #1628 bounds the number of sessions (below).
  https://github.com/synnaxlabs/foundation/pull/1649#issuecomment-6049077609.
  Amended (2026-10-08, #1647, by `laptop.architect-2`, 00:05 UTC): a transport that
  stops with an error stops the node, and `Node::join` gives `Error::Transport`. The
  node does not rebind the port:
  https://github.com/synnaxlabs/foundation/issues/1647#issuecomment-6049354544.
  Supersedes the deferral of
  https://github.com/synnaxlabs/foundation/pull/1649#issuecomment-6048464411
  (2026-10-07 22:50 UTC), under which the node ran on with no port until #1647.
  Amended (2026-10-08, PR 3b of #585, by `laptop.architect`, 03:37 UTC): shard 0 opens
  the mesh before the first session. `route` gives each `Mesh` stream of a
  `Peer::Node` session to `Mesh::serve` with that key, never a key from a header or a
  message. A `Mesh` stream of a `Peer::Client` session stops with
  `Code(wire::header::REJECTED)`, and its reply half resets with the same code. For
  `Mesh` streams, the admission rule is the mesh's check of each message: a peer that
  is not the member it names gets `Spoofed`, and the stream stops at that message. A
  `Hub` stream of a `Peer::Node` session goes to the hub's link of the session only
  when the node has a mesh and a member of its region, in this node's view, has the
  peer's public key; else it stops with `Code(wire::header::REJECTED)`, and its reply
  half resets with the same code. A `Hub` stream of a `Peer::Client` session is
  rejected until #1744 (PR 4b of #585, by `laptop.architect-2`, 22:22 UTC,
  https://github.com/synnaxlabs/foundation/pull/2021#issuecomment-6070221342). At the
  stop, each session and stream future drops, then the mesh, and shard 0 waits for the
  mesh's task to end before it drops `lock` (DATA DIRECTORY LOCK) and before
  `Node::join` returns, so a restart at once opens the log:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051658475. For a
  node with a mesh, this supersedes the stop order of
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6046900669.
  Amended (2026-10-08, #1830, by `laptop.architect-2`, 07:26 UTC): with a mesh, each
  session and stream future drops, then the mesh, and the transport drops when the
  last task of the mesh ends, before `lock` drops:
  https://github.com/synnaxlabs/foundation/pull/1830#issuecomment-6054871235. Amended
  again (2026-10-08, #1962, by `laptop.architect-2`, 20:07 UTC): with a mesh, shard 0
  waits for each task of the mesh to end, and the transport drops with the last of the
  port's future and the tasks of the mesh:
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6068113010. This
  holds also when the mesh's group stops before the node, which then stops the node
  (NODE MESH), by `laptop.architect-2` (21:31 UTC):
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6069443456.
  Supersedes the #1780 clause of
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6068113010. Under
  `sim`, the port is free once `lock` is, which a test pins, by `laptop.architect-2`
  (21:58 UTC, words of 22:23 UTC and 22:47 UTC):
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6069829972,
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6070247529,
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6070577797.
  Amended (2026-10-09, #2017, by `laptop.architect-2`, 02:17 UTC:
  https://github.com/synnaxlabs/foundation/issues/2017#issuecomment-6072896717, on
  https://github.com/synnaxlabs/foundation/issues/2017): shard 0 drops `lock` only once
  the transport has freed the port (`transport::Transport::ended`), also when the mesh
  does not open, so a restart at once binds the port under `os` too. Once the node
  has confirmed the handshake of a session, the close of that session leaves at the
  drop: a peer whose one-way delay is under 3 s gets it before `lock` is free, unless
  the link loses it, and a peer with a delay of 4 s gets it after, which a test pins
  for each (`laptop.architect`, 04:36 UTC:
  https://github.com/synnaxlabs/foundation/pull/2089#issuecomment-6074355159; the
  condition, 06:16 UTC:
  https://github.com/synnaxlabs/foundation/pull/2089#issuecomment-6075490388; this
  text, 06:41 and 06:42 UTC:
  https://github.com/synnaxlabs/foundation/pull/2089#issuecomment-6075817836 and
  https://github.com/synnaxlabs/foundation/pull/2089#issuecomment-6075828783). The node
  confirms a session that a peer dialed when the transport gives it, and one that the
  node dialed one round trip or more later. A close before then can be paced (the
  `noq-proto` patch in `docs/dependencies.md`), so it can leave after `lock` is free,
  or not at all when the pacer holds it until the transport frees the port. The peer
  then waits for its idle time, as when the link loses the close. Each clone of the
  transport and each session lives in a future that shard 0 drops before it waits for
  the port, in a task of the mesh that it waits for, or in the hub's task of a reader
  whose home is another node, which ends at its next poll after the drop of the reader.
  So, unless the socket broke, a leak holds the stop. The drop of each session closes
  it. The drop of the transport closes each session that no caller accepted and each
  handshake in flight, its own dials too (#2084, by `laptop.architect-2`, 02:57 UTC:
  https://github.com/synnaxlabs/foundation/issues/2084). The stop waits for each to
  drain, in about 3 PTO, and at most 3 s after the last one ended, because a peer's
  round trip sets the PTO with no bound (`laptop.architect`, 03:46 UTC:
  https://github.com/synnaxlabs/foundation/pull/2089#issuecomment-6073847772). The
  `transport` surface, by `laptop.architect` (02:19 UTC):
  https://github.com/synnaxlabs/foundation/issues/2017#issuecomment-6072912165.
  Supersedes the `os` sentences of that port rule, and the sentence of
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6069568312 that no
  test sees the drop of the transport.
  Amended (2026-10-08, #1660, by `laptop.architect-2`, 19:52 UTC): `Config` has no key.
  Once each buffer has opened, shard 0 reads the node's key and private key from the
  file `node.key` in the data directory, before the hub, the transport, and the mesh
  open. The file is 68 bytes: the tag `foundation/key/1`, the node key (big-endian;
  UUIDv7 when shard 0 makes it), the Ed25519 private key, and the CRC32C of those 64
  bytes (little-endian). It is one sector, which a crash keeps whole or old. At the
  first start, shard 0 makes the file with `Mode::Create`, unless `node::create_key`
  made it first (NODE MESH); a file with no bytes or with 68 zero bytes is a key not yet
  written, so shard 0 makes a key (`types::node::Key::v7` at mesh time, once it has one,
  from `Config::entropy`, and 32 random bytes). At each start, shard 0 writes the key
  back and syncs the file and the directory before the transport proves it, because a
  failed sync of an earlier start can leave a key that a read sees but a power cut
  loses. A node that joins by ticket (#336) makes its key the same way at its first
  start. A file of another length that is not 0, or of another tag or checksum, gives
  `Error::Key`, which `Node::join` ranks above `Error::Blob`, `Error::Mesh`,
  `Error::Transport`, and `Error::Group`; the node never writes over it, because a new
  key is a new node to its region. The file with no bytes, by `laptop.architect-2`
  (01:08 UTC):
  https://github.com/synnaxlabs/foundation/pull/1962#issuecomment-6072179320. Each other
  file error on `node.key` gives `Error::Directory`. The form is not a contract: only
  `node` reads it. The seal key goes into `node.key` with its first caller, as the tag
  `foundation/key/2` with 32 more bytes. `admin.key` (#1744 PR 1b) shares this code when
  it lands. `os` gives each new file the mode `0600` and each new directory `0700`, and
  on Linux a new directory takes the setgid bit of its parent; the umask can clear more
  bits. It does not change the mode of one that is there (#1988):
  https://github.com/synnaxlabs/foundation/issues/1660#issuecomment-6067866831, on the
  plan https://github.com/synnaxlabs/foundation/issues/1660#issuecomment-6067848563. The
  umask, the setgid bit, and a file that is there, by `laptop.architect-2` (23:24 UTC):
  https://github.com/synnaxlabs/foundation/pull/2028#issuecomment-6071026954, which
  https://github.com/synnaxlabs/foundation/pull/2028#issuecomment-6071534642 confirms
  (00:09 UTC). The time of a new key, by `laptop.architect-2` (20:10 UTC):
  https://github.com/synnaxlabs/foundation/issues/1660#issuecomment-6068150017. The
  private pool and the rank above `Error::Blob` and `Error::Mesh` are the amendment
  https://github.com/synnaxlabs/foundation/issues/1660#issuecomment-6068057306, which
  the same comment approves. The rank above `Error::Transport` and `Error::Group`, from
  the merge with #1936, by `laptop.architect-2` (21:48 UTC):
  https://github.com/synnaxlabs/foundation/pull/1991#issuecomment-6069688780. The load
  before the hub and the write back at each start (the fix of a finding of `breaker` in
  round 1 of #1991), by `laptop.architect-2` (20:23 UTC):
  https://github.com/synnaxlabs/foundation/pull/1991#issuecomment-6068373063. This
  supersedes `Config::private_key`, the patch of
  https://github.com/synnaxlabs/foundation/pull/1649#issuecomment-6048898047. `node`
  runs no check of the spec: `mesh` uses only a spec with no problems (SPEC IN USE), so
  a founding with problems gives no spec in use and the hub knows no channel, by
  `laptop.architect-2` (2026-10-09T20:41:17Z, #1957 PR 2):
  https://github.com/synnaxlabs/foundation/issues/1957#issuecomment-6088900310. This
  supersedes the panic of a founding with a dangling index or two channels of one key,
  and the check in `node` that was to end it, by
  `laptop.architect-2` (2026-10-08 20:08 UTC):
  https://github.com/synnaxlabs/foundation/issues/1660#issuecomment-6068130409, on the
  ruling of `laptop.architect` (20:08 UTC):
  https://github.com/synnaxlabs/foundation/pull/1966#issuecomment-6068129791.
  Amended (2026-10-09, #1628, by `laptop.architect-2`, 13:35 UTC):
  https://github.com/synnaxlabs/foundation/issues/1628#issuecomment-6082002858, on the
  plan https://github.com/synnaxlabs/foundation/issues/1628#issuecomment-6081954276. Two
  bounds, patches until #1662 as the limits above are. A stream whose header has not
  arrived 10 s after the stream did is rejected as a header that does not decode is. A
  test cannot open a stream with no bytes, because the transport queues a stream at its
  first byte, so a first message that is still arriving at 10 s tests the wait. Shard 0
  gives peers outside the region (`Peer::Client`, and a `Peer::Node` whose key no member
  holds in this node's view) at most 256 places, one for each session of a program and
  one for the key of a node. It closes each session that gets no place at once with
  `Code(wire::session::REFUSED)`. A node's new session takes the place of its old one
  when the old one still holds it at `accept`. The transport closes the old session as
  the new one arrives (ONE SESSION PER PEER), so at the bound another peer can take the
  place first (`laptop.architect-2`, 15:18 UTC:
  https://github.com/synnaxlabs/foundation/pull/2153#issuecomment-6083798077). It never
  refuses a member, so a flood of programs cannot lock the region out. Trigger: the PR
  that adds the change kind that removes a member (MESH DRIVER). That change also closes
  each session of that node, with the code of a refused session, and so ends its `Hub`
  and `Mesh` streams. The node then admits it again as any other peer. Supersedes the
  deferrals of
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6046900669 (item 5,
  2026-10-07 21:09 UTC) and
  https://github.com/synnaxlabs/foundation/pull/1649#issuecomment-6049077609 (23:41
  UTC), and the sentence on the wait for a header of
  https://github.com/synnaxlabs/foundation/issues/1628#issuecomment-6051519122
  (2026-10-08 03:23 UTC).
