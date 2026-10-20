- **MESH SURFACE (#1051)** A crate outside `mesh` reads a region through `Mesh::watch`,
  `Watch::next`, and `Mesh::member` (#562). `mesh` gives no `Mesh::key`: a crate that
  holds a `Mesh` reads this node's key from its own config, as the hub reads
  `hub::Config::node` (`laptop.architect`, 2026-10-08T18:42:42Z:
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6066677536).
  Supersedes the approval of `Mesh::key` in item 2 of
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6048960511. `next`
  gives `Stopped`, which holds the cause types `log::Error` and `change::Unknown`, each
  public in its own module, so a caller can match the exact cause. `next` gives
  `Stopped` and not `Error`, because a stop is the only error that it has: the type says
  what the call gives. For a read of a home, `hub` gets the variant
  `Error::Mesh(mesh::Stopped)` in #340, which supersedes the `Error::Mesh(mesh::Error)`
  of its plan
  (https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6002776268). The
  cause types at the root (`mesh::LogError`) lost, because each name repeats its module.
  A `Stopped` that holds a text for each cause lost, because a caller cannot match a
  text. The surface holds types of other crates, among them `raft::Position`,
  `block::Error`, `env::files::Error`, and `types::ed25519::PublicKey`, which the card
  of a `Member` holds. A caller whose line of the crate map does not hold the crate of
  such a type reads it only through `Display` and `Debug`. A caller that must match one
  gets the crate in its line through an `interface` issue first. `mesh` does not
  re-export such a type: a re-export makes each change to `raft` a change to the surface
  of `mesh`. The `Debug` text of a `Mesh` is `Mesh { .. }`, of an `Ended` is
  `Ended { .. }`, and of a `Watch` is its index only. The text of `Ended`: decided by
  `laptop.architect` (2026-10-08T04:42:48Z):
  https://github.com/synnaxlabs/foundation/pull/1791#issuecomment-6052417077. It
  supersedes the `#[derive(Debug)]` of `Ended` in
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051912643. A crate
  outside `mesh` opens a region with `Config` and `Mesh::open`, and gives it each stream
  of a peer with `Mesh::serve`. The three are public since the senders (#1410). `Error`,
  `claim::Error`, and `region::Unfit` are public with them, because `open` and `serve`
  give them. `claim::Error` is the `grant::Error` of the rulings: #1460 gave the module
  its new name. `Error` adds `raft::Error` and `transport::Error` to the types of other
  crates. `Config` and `serve` add types that the caller builds:
  `env::files::Files`, `env::clock::Clock`, `env::entropy::Entropy`,
  `env::tasks::Tasks`, `block::Pool`, `transport::Transport`,
  `transport::stream::Incoming`, `types::name::Prefix`, and
  `types::ed25519::PrivateKey`. So a crate that opens a region has `env`, `block`, and
  `transport` in its line of the crate map. `Config::founding` is a `region::Founding`:
  the prefix, the founding members and voters, the founding definitions, and the home of
  each founding index by channel key and node key, the same at each member and at each
  open. `State::new` takes the homes, so the first state holds them. An index with no
  entry has no home until a spec change gives one (#1931; `laptop.architect`,
  2026-10-08T18:35:16Z:
  https://github.com/synnaxlabs/foundation/issues/1931#issuecomment-6066553633). At
  each open, the state starts at the founding, so until the replay a watch can give a
  founding home that the log moved, as at a follower behind the leader. A home that
  the mesh names is never the authority to write. Trigger: before a production path
  moves a home, the home takes a write only while it holds its node lease, so a node
  whose state is old takes no write as a home that it lost (`laptop.architect`,
  2026-10-08T19:28:51Z:
  https://github.com/synnaxlabs/foundation/pull/1978#issuecomment-6067470813). A
  founding node builds it from its config, and a node that joins takes it whole from its
  join answer. It derives `PartialEq` and `Eq` and has no constructor: `Mesh::open`
  stays its one check: this node, each voter, and the node of each home must be a
  member (`Error::NotMember`), other than this node when the join file of the mesh
  directory holds its card (MEMBER RECORD, 2026-10-20T09:51:13Z,
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6288239430). It
  refuses no definition. A founding spec with problems is not an error of the open: when
  no file names a pointer, the node uses no spec until
  a valid change takes effect (SPEC IN USE; `laptop.architect`, 2026-10-08T15:42:09Z:
  https://github.com/synnaxlabs/foundation/pull/1897#issuecomment-6063561498). The node
  that founds the region checks the definitions (SPEC CHANGE). Changed by
  `laptop.architect` (2026-10-08T21:26:46Z:
  https://github.com/synnaxlabs/foundation/pull/1978#issuecomment-6069370914), which
  changes "with no check at `Mesh::open`" of
  https://github.com/synnaxlabs/foundation/issues/1931#issuecomment-6066553633. `Start`
  lost, because `driver.rs` holds `raft::Start`, which changes at each open
  (`laptop.architect`, 2026-10-08T10:34:37Z:
  https://github.com/synnaxlabs/foundation/issues/1859#issuecomment-6057975061).
  `Founding::definitions` adds
  `spec::definition::Definition` and `types::name::Name`, and `Mesh::pointer` gives a
  `spec::Pointer`, whose root is a `types::digest::Digest`. So a crate that opens a
  region also has `spec` in its line. Decided by `laptop.architect`: the founding
  definitions, 2026-10-08T06:12:36Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6053614771); the
  pointer, 2026-10-08T08:22:08Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6055806836); this
  text, 2026-10-08T08:41:43Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056116151).
  `Config` has no `clock::Reader`, and `Error` has no `Unsynced` and no `Status`: no
  public call reads the one or gives the two. The join answer of #336 decides, with its
  caller, where a join that no voter stamps goes (MEMBER RECORD).
  Decided by `laptop.architect` (2026-10-07T22:33:29Z):
  https://github.com/synnaxlabs/foundation/issues/1051#issuecomment-6048235563.
  Supersedes, in
  https://github.com/synnaxlabs/foundation/issues/1051#issuecomment-6042136383, the
  sentence on `Unsynced` and `Status`. The same ruling supersedes the approval of
  `Unsynced`, `Status`, and `Config.time` in item 2 of that comment. It also supersedes
  the approval of `Config.time` (a `clock::Reader`) in
  https://github.com/synnaxlabs/foundation/pull/1575#issuecomment-6045694724 (ruled by
  `laptop.architect`, 2026-10-08T00:46:02Z:
  https://github.com/synnaxlabs/foundation/issues/1051#issuecomment-6049818540). `open`
  panics when `Config.transport` proves a key that is not the public half of
  `Config.private_key`. `node` builds both from the one key that it loads, so a mismatch
  is a defect in `node`, not bad outside input. `Error::WrongKey` stays for a key that
  is not the key of the member record (ruled by the architect, 2026-10-07T19:55:13Z:
  https://github.com/synnaxlabs/foundation/issues/1587#issuecomment-6045695196).
  Supersedes the sentence that `open` does not check the key of the transport:
  https://github.com/synnaxlabs/foundation/pull/1575#issuecomment-6045694724. The
  `Debug` text of a `Config` does not show the private key. `Mesh::set_home` is the
  first public call that changes the region (#471), and `Error::NoVote` is public with
  it (MESH DRIVER), approved by the architect, 2026-10-07T20:29:07Z:
  https://github.com/synnaxlabs/foundation/pull/1607#issuecomment-6046249552.
  The line "Private still" of the plan names `set_home` (4c-2) as the first call that
  changes the region
  (https://github.com/synnaxlabs/foundation/issues/1051#issuecomment-6041243466), which
  the architect approved, 2026-10-07T16:24:54Z:
  https://github.com/synnaxlabs/foundation/issues/1051#issuecomment-6042136383. The
  other calls that change the region and the change records stay private. The surface is
  approved in the same comment (`Unsynced`, `Status`, and `Config.time` superseded
  above). The architect approved the surface as built at 2026-10-07T19:55:12Z:
  https://github.com/synnaxlabs/foundation/pull/1575#issuecomment-6045694724. It has the
  types that the caller builds, `Config.time`, and the sentence that `open` does not
  check the key of the transport. The last two are superseded above. `member` is
  approved by the architect, 2026-10-07T15:17:13Z:
  https://github.com/synnaxlabs/foundation/issues/562#issuecomment-6040867482. The order
  of the PRs is decided by the architect, 2026-10-07T17:18:52Z:
  https://github.com/synnaxlabs/foundation/issues/1051#issuecomment-6043038615. The
  architect then ruled the type that `next` gives, the later PR for `Error`,
  `claim::Error`, and `region::Unfit`, and the rule for the types of other crates,
  2026-10-07T17:38:37Z:
  https://github.com/synnaxlabs/foundation/pull/1508#issuecomment-6043385150. That
  ruling supersedes the list of the export PR in the ruling on the order, for those
  three types.
  Amended (2026-10-08, the `mesh` PR before PR 3b of #585): `Config::dir` is the
  mesh's directory, relative to the data directory. The mesh makes it and syncs its
  parent, and the log goes in `log` in it. Its parent must be there and durable. `node`
  gives `mesh`. `Mesh::ended` gives `Ended`, a future that resolves once each task of
  the mesh has ended: the group's task and each task that sends. It holds no clone, so
  it does not keep the group running. Once it resolves, the mesh holds no file, and a
  new open of its directory can take the log. Decided by `laptop.architect`,
  2026-10-08T04:00:49Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051912643, after
  the ruling on PR 3b, 2026-10-08T03:37:20Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051658475, and the
  field over a `Files` call by `laptop.architect-2`, 2026-10-08T03:54:37Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051833866. Lost: a
  counting `Tasks` driver in `node`, because `node` then watches the tasks of another
  crate; `Mesh::close(self)`, because the hub holds a clone, so one clone cannot end
  the group; `env::files::Files::within`, because `env` then gives two ways to scope
  the files of a crate, beside `buffer::Config::dir`. A change that wants it later
  moves `buffer` and `mesh` together.
  Amended (2026-10-10, #2064): the mesh spawns each task through one
  `env::tasks::Group` made from its `Tasks`, and `Mesh::ended` returns the
  `env::tasks::Ended` that `Group::ended` gives. `mesh::Ended` leaves the exports of
  `mesh`. The contract above holds. Decided by `laptop.architect`
  (2026-10-09T20:44:15Z):
  https://github.com/synnaxlabs/foundation/issues/2064#issuecomment-6088942460.
  `Ended` is a type of another crate, under the rule above (`laptop.architect`,
  2026-10-10T03:02:58Z:
  https://github.com/synnaxlabs/foundation/pull/2218#issuecomment-6093108453), and the
  `Debug` text of `Ended` above is that of `env::tasks::Ended`, set in ENV SEAMS by
  `laptop.architect-2` (2026-10-09T21:02:50Z):
  https://github.com/synnaxlabs/foundation/issues/2064#issuecomment-6089208454. This
  supersedes the `mesh::Ended` of
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051912643.
  `Mesh::holder(PublicKey) -> Option<node::Key>` gives the member whose card holds a
  public key, so a caller such as `node` serves a `Hub` stream of a peer only when a
  member holds the key that its transport proves. It gives the key, not a `Member`,
  which a caller reads with `Mesh::member` when it needs the record. At most one member
  holds a key: `region::State::fits` refuses a second with `Unfit::Held`, at open and at
  each join. Lost: `Option<Member>`, which clones the names, addresses, and status at
  each stream, and which a caller can keep after the record changes; `admits -> bool`,
  which fits only one caller. Decided by `laptop.architect` (2026-10-08T22:29:26Z:
  https://github.com/synnaxlabs/foundation/issues/2023#issuecomment-6070338077). A
  `Card` change keeps the public key of its member (MEMBER RECORD), so `Held` holds with
  no new check (`laptop.architect`, 2026-10-18T03:29:11Z:
  https://github.com/synnaxlabs/foundation/issues/3115#issuecomment-6245196820).
  Supersedes the trigger "the change that first replaces a card checks `Held` against
  each other member" of
  https://github.com/synnaxlabs/foundation/issues/2023#issuecomment-6070338077.
  `region::State` keeps a map from the bytes of each public key to its member, beside
  the names and the status keys, and `holder` reads it. Only the insert of a member
  writes it, because a `Card` change keeps the public key and no change removes a
  member. The map replaces a scan: after #2363, each new session of a peer outside the
  region calls `holder` up to 257 times on shard 0, and the peer needs no right to open
  it (`laptop.architect`, 2026-10-11T06:50:38Z:
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6106400777).
  Supersedes the scan of the members, its lost "a map by public key", and its trigger,
  of https://github.com/synnaxlabs/foundation/issues/2023#issuecomment-6070338077 and
  https://github.com/synnaxlabs/foundation/pull/2021#issuecomment-6070861987.
  Amended (2026-10-09, #1756): `Mesh::names() -> BTreeSet<Name>` gives the name of
  each member in this node's view of the region, also after the group stops, so
  `ops::Node` gives `config::plan::plan` its exact input. Lost: `members() ->
  Vec<Member>`, which clones each card and status map for a caller that reads only the
  name; `keys()` with `member(key)`, which puts the loop in each caller. Decided by
  `laptop.architect` (2026-10-09T01:46:26Z:
  https://github.com/synnaxlabs/foundation/issues/1756#issuecomment-6072570741).
  Amended (2026-10-09, #2179): `Mesh::watch_spec() -> used::Watch` watches the spec
  that this node uses, so the hub of a node in a region takes the definitions of each
  new spec in use (`laptop.architect`, 2026-10-10T23:49:57Z:
  https://github.com/synnaxlabs/foundation/issues/2326#issuecomment-6103471301;
  supersedes "`node` gives the hub the definitions of each new spec in use" of
  https://github.com/synnaxlabs/foundation/issues/2179#issuecomment-6088928607), also of
  a change that another node proposed. The first `next` gives the spec in use at once.
  Each later `next` waits until the pointer in use differs from the one it last gave,
  and gives the newest spec, so two changes between calls give one result. A change of
  only `behind` wakes no watch. The wake comes where the pointer of the spec in use
  changes, in the same call. After a stop, `next` gives `Stopped` as `Mesh::watch` does.
  Its `Debug` gives its last pointer, and its `Drop` frees its slot. Lost:
  `Mesh::spec_after(pointer)`, a call with no state that waits for a pointer other than
  the one given: one type less, but the caller keeps the last pointer, and the two
  watches of `Mesh` have two forms. Decided by `laptop.architect` (2026-10-09T20:43:17Z:
  https://github.com/synnaxlabs/foundation/issues/2179#issuecomment-6088928607).
  Amended (2026-10-10, #1744): the free function `mesh::founding(files, dir, pool)`
  gives the founding that a mesh directory holds, or `None` when the directory is not
  there or holds no founding file, so a node opens its region again at each start
  with no copy of its own. It reads no log: a first open whose log holds no record
  and whose directory holds the same founding writes nothing (MESH DRIVER; approved
  by `laptop.architect`, 2026-10-10T04:24:20Z:
  https://github.com/synnaxlabs/foundation/pull/2234#issuecomment-6093736250). A
  founding file that does not read back whole gives `Error::Unfounded`, whose text is
  "{path} does not read back whole, or is not there while the log of the mesh
  directory holds a record". Lost: `Mesh::founding`, an
  associated function that gives no `Mesh` and reads as a getter of an open mesh.
  Decided by `laptop.architect` (2026-10-10T03:33:06Z:
  https://github.com/synnaxlabs/foundation/issues/2225#issuecomment-6093336972).
  Amended (2026-10-11, #2326): `Mesh::used() -> used::Spec` gives the spec that this
  node uses now, with no wait, as `pointer` and `holder` do, also after the group
  stops. The hub reads it at each open (#2326 PR 2), so an open works on a spec no
  older than each spec that a task told by `Mesh::spec` or a spec watch knows of.
  Lost: a hub task that follows the spec watch alone, which `keep` wakes in the same
  step as the told task; a doc that states the window and a call that waits for the
  hub. Decided by `laptop.architect` (2026-10-10T23:49:57Z:
  https://github.com/synnaxlabs/foundation/issues/2326#issuecomment-6103471301; the
  text, 2026-10-12T19:41:06Z:
  https://github.com/synnaxlabs/foundation/pull/2476#issuecomment-6131357049).
  Lost also: `Mesh::pointer_used() -> Option<Pointer>`, after which an open calls
  `Mesh::spec`, which can wait for a read (`laptop.architect`, 2026-10-11T17:40:47Z:
  https://github.com/synnaxlabs/foundation/pull/2422#issuecomment-6111819824).
  The stop fixes the view: after a stop, `keep` does not settle, so `used` keeps the
  spec that it gave at the stop, while `spec` and each watch give `Stopped`. The file
  part of a read that runs at the stop runs to its end, a file that it made durable
  stays, and the next open reads it, as after a power cut at the end of that part.
  Lost: `used` that moves after the stop with no watch woken, so a caller gets the old
  or the new spec by the time of its read. Decided by `laptop.architect`
  (2026-10-11T16:47:07Z:
  https://github.com/synnaxlabs/foundation/pull/2422#issuecomment-6111328950).
  Amended (2026-10-12, #2224): `Mesh::ended` waits for each task that the mesh spawned.
  Its doc and this entry list no tasks, because a list goes stale when the mesh adds a
  task. Supersedes "the group's task and each task that sends" of the amendment of
  2026-10-08 above (2026-10-08T04:00:49Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051912643). Decided
  by `laptop.architect` (2026-10-12T23:18:24Z:
  https://github.com/synnaxlabs/foundation/pull/2506#issuecomment-6134467487).
  Amended (2026-10-20, PR A of #1157): `Mesh::ticket(options) -> Result<Ticket,
  Error>` makes a join ticket. The key pair comes from `Config::entropy`. The call
  records the ticket with a `Ticket` change, which a follower forwards, and returns
  when an entry that records it has committed and this node applied it. It proposes
  again as `set_home` does, and an entry of an earlier try counts as the record. The
  ticket lists each voter of the configuration that this node has applied, of both
  halves of a joint one, in node key order, with the addresses of its card: a voter of
  a newer configuration that is only in the log can be a node that this node has not
  applied as a member, with no card. A prefix that is not under the region's prefix
  gives the new `Error::Outside { prefix, region }` before any proposal; `NoVote` and
  `Stopped` come as for `set_home`. Lost: a token that the admin's node signs and that
  no change records, since a single-use ticket needs a use count that each member
  agrees on (BQ11a), and a token cannot be revoked. Plan:
  https://github.com/synnaxlabs/foundation/issues/1157#issuecomment-6293270267.
  Approved by `laptop.architect` (2026-10-20T14:33:01Z:
  https://github.com/synnaxlabs/foundation/issues/1157#issuecomment-6293328111;
  2026-10-20T15:36:02Z:
  https://github.com/synnaxlabs/foundation/pull/3611#issuecomment-6294701855;
  2026-10-20T16:06:59Z:
  https://github.com/synnaxlabs/foundation/pull/3611#issuecomment-6295368878).
