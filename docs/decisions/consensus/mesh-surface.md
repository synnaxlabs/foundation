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
  of `mesh`. The `Debug` text of a `Mesh` is `Mesh { .. }`, of an `Ended` is `Ended { ..
  }`, and of a `Watch` is its index only. The text of `Ended`: decided by
  `laptop.architect` (2026-10-08T04:42:48Z):
  https://github.com/synnaxlabs/foundation/pull/1791#issuecomment-6052417077. It
  supersedes the `#[derive(Debug)]` of `Ended` in
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051912643. A crate
  outside `mesh` opens a region with `Config` and `Mesh::open`, and gives it each stream
  of a peer with `Mesh::serve`. The three are public since the senders (#1410). `Error`,
  `claim::Error`, and `region::Unfit` are public with them, because `open` and `serve`
  give them. `claim::Error` is the `grant::Error` of the rulings: #1460 gave the module
  its new name. `Error` adds `raft::Error` and `transport::Error` to the types of other
  crates. `Config` and `serve` add types that the caller builds: `env::files::Files`,
  `env::clock::Clock`, `env::entropy::Entropy`, `env::tasks::Tasks`, `block::Pool`,
  `transport::Transport`, `transport::stream::Incoming`, `types::name::Prefix`, and
  `types::ed25519::PrivateKey`. So a crate that opens a region has `env`, `block`, and
  `transport` in its line of the crate map. `Config::founding` is a `region::Founding`:
  the prefix, the founding members and voters, the founding definitions, and the home of
  each founding index by channel key and node key, the same at each member and at each
  open. `State::new` takes the homes, so the first state holds them. An index with no
  entry has no home until a spec change gives one (#1931; `laptop.architect`,
  2026-10-08T18:35:16Z:
  https://github.com/synnaxlabs/foundation/issues/1931#issuecomment-6066553633). At each
  open, the state starts at the founding, so until the replay a watch can give a
  founding home that the log moved, as at a follower behind the leader. A home that the
  mesh names is never the authority to write. Trigger: before a production path moves a
  home, the home takes a write only while it holds its node lease, so a node whose state
  is old takes no write as a home that it lost (`laptop.architect`,
  2026-10-08T19:28:51Z:
  https://github.com/synnaxlabs/foundation/pull/1978#issuecomment-6067470813). A
  founding node builds it from its config, and a node that joins takes it whole from its
  join answer. It derives `PartialEq` and `Eq` and has no constructor: `Mesh::open`
  stays its one check, of the members and voters. It refuses no definition. A founding
  spec with problems is not an error of the open: when no file names a pointer, the node
  uses no spec until a valid change takes effect (SPEC IN USE; `laptop.architect`,
  2026-10-08T15:42:09Z:
  https://github.com/synnaxlabs/foundation/pull/1897#issuecomment-6063561498). The node
  that founds the region checks the definitions (SPEC CHANGE). Nothing checks the homes
  (`laptop.architect`, 2026-10-08T18:35:16Z:
  https://github.com/synnaxlabs/foundation/issues/1931#issuecomment-6066553633). `Start`
  lost, because `driver.rs` holds `raft::Start`, which changes at each open
  (`laptop.architect`, 2026-10-08T10:34:37Z:
  https://github.com/synnaxlabs/foundation/issues/1859#issuecomment-6057975061).
  `Founding::definitions` adds `spec::definition::Definition` and `types::name::Name`,
  and `Mesh::pointer` gives a `spec::Pointer`, whose root is a `types::digest::Digest`.
  So a crate that opens a region also has `spec` in its line. Decided by
  `laptop.architect`: the founding definitions, 2026-10-08T06:12:36Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6053614771); the
  pointer, 2026-10-08T08:22:08Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6055806836); this
  text, 2026-10-08T08:41:43Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056116151). `Config`
  has no `clock::Reader`, and `Error` has no `Unsynced` and no `Status`: no public call
  reads the one or gives the two. The join answer of #336 decides, with its caller,
  where a join that no voter stamps goes (MEMBER RECORD). Decided by `laptop.architect`
  (2026-10-07T22:33:29Z):
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
  https://github.com/synnaxlabs/foundation/pull/1607#issuecomment-6046249552. The line
  "Private still" of the plan names `set_home` (4c-2) as the first call that changes the
  region (https://github.com/synnaxlabs/foundation/issues/1051#issuecomment-6041243466),
  which the architect approved, 2026-10-07T16:24:54Z:
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
  three types. Amended (2026-10-08, the `mesh` PR before PR 3b of #585): `Config::dir`
  is the mesh's directory, relative to the data directory. The mesh makes it and syncs
  its parent, and the log goes in `log` in it. Its parent must be there and durable.
  `node` gives `mesh`. `Mesh::ended` gives `Ended`, a future that resolves once each
  task of the mesh has ended: the group's task and each task that sends. It holds no
  clone, so it does not keep the group running. Once it resolves, the mesh holds no
  file, and a new open of its directory can take the log. Decided by `laptop.architect`,
  2026-10-08T04:00:49Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051912643, after the
  ruling on PR 3b, 2026-10-08T03:37:20Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051658475, and the
  field over a `Files` call by `laptop.architect-2`, 2026-10-08T03:54:37Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051833866. Lost: a
  counting `Tasks` driver in `node`, because `node` then watches the tasks of another
  crate; `Mesh::close(self)`, because the hub holds a clone, so one clone cannot end the
  group; `env::files::Files::within`, because `env` then gives two ways to scope the
  files of a crate, beside `buffer::Config::dir`. A change that wants it later moves
  `buffer` and `mesh` together.
