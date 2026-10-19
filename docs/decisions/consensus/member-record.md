- **MEMBER RECORD (#242)** The region's record of a node is a `mesh::Member`: a
  `card::Signed` (name, Ed25519 public key, seal key, addresses, and version, which the
  node signs over `foundation/card/1`, its `node::Key` (16 bytes), and the card's one
  byte form), the join ticket's signature over the first card, `ephemeral` (for an
  ephemeral node, the time offline after which the region removes it), and the key of
  each status channel (X27) by its name relative to the node's name (`clock.offset`,
  never the full name); `card.name` is the one copy of the node's name in region state.
  The node's own copy is its file `name` (NODE NAME). Supersedes "the one copy of the
  node's name" of
  https://github.com/synnaxlabs/foundation/issues/242#issuecomment-6031533205
  (`laptop.architect`, 2026-10-09T19:53:33Z,
  https://github.com/synnaxlabs/foundation/pull/2172#issuecomment-6088190305).
  The founding that a node stores holds the card of each founding member, and no change
  gives a member another name. So `node` reads the name of a founding member there when
  its file `name` is gone (NODE NAME, which holds the trigger; `laptop.architect-2`,
  #2355, 2026-10-11T07:44:40Z,
  https://github.com/synnaxlabs/foundation/issues/2355#issuecomment-6106801760; the file
  first, 2026-10-11T10:19:18Z,
  https://github.com/synnaxlabs/foundation/pull/2387#issuecomment-6108005495). The
  founding holds no card of a node that joined (`laptop.architect`,
  2026-10-11T10:58:48Z,
  https://github.com/synnaxlabs/foundation/pull/2387#issuecomment-6108309120).
  The joining node gives its own release's names; the voters assign the keys at join
  (X27). A status name keeps its meaning and data type in every release, and a change
  takes a new name, so `hub` resolves a status channel from the record alone. The byte
  form (#336) writes the status entries in name order. A list in the order of a table in
  `node` lost, because `hub` cannot read that table and a new release would change what
  a stored position means. Cost: about 40 bytes of names per member (architect, #242:
  https://github.com/synnaxlabs/foundation/issues/242#issuecomment-6031533205). The
  card's byte form is the name behind a length byte, the public key (32 bytes), the seal
  key (32 bytes), a count of addresses (8 bytes), at most 32, each address, and the
  version (8 bytes). An address is a kind byte (UDP 0, TCP 1, relay 2, which adds the
  relay's public key of 32 bytes), a family byte (4 or 6), the IP (4 or 16 bytes, in
  network order), and the port (2 bytes). An IPv6 address has no flow info and no scope,
  because each means something only on the node that sets it. Every number but the IP is
  little endian. The cap is part of the byte form because every member keeps every card:
  without it, one node sets the size of each member's state. Lost: a bound from a MESH
  WIRE frame limit, which ties what a valid card is to a link setting and bounds no
  region state. Decided by the architect, #336
  (https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6032881587). It
  lives only in `mesh` region state (X1), with no voter flag (the raft configuration is
  the one source) and no lease. The seal key is inside the signed card (S8). A join is
  one `Join` change. Every node that applies it checks the card, and the admission
  against the ticket's public key, scope, uses, and expiry at the change's mesh time
  (BQ12), so a ticket is an Ed25519 key pair (#336). The voter that admits a join
  stamps the `Join` with the later edge of its mesh time interval, so the error of the
  voter clock never admits a request that comes at or after the expiry. Every node
  checks the expiry against the stamp, not against the time of the commit, so a join
  that commits after the expiry still admits its node, and every node checks the same
  stamp at every replay. A voter with no mesh time with a known error at or after the
  Unix epoch stamps no join: a guess at the expiry is the case that the later edge
  stops. Decided by `laptop.architect` (2026-10-07T13:11:29Z):
  https://github.com/synnaxlabs/foundation/pull/1390#issuecomment-6038661706. The stamp
  is crate-private. Until the join answer of #336 calls it, it takes the mesh time as an
  argument and gives its own type, `driver::Unstamped`, so `mesh::Config` has no mesh
  time and `mesh::Error` has no case for a join that no voter stamps. #336 decides with
  its caller whether such a join goes out of `Mesh` as an `Error`, or back to the node
  as a stop code or an answer. Decided by `laptop.architect` (2026-10-07T22:33:29Z):
  https://github.com/synnaxlabs/foundation/issues/1051#issuecomment-6048235563.
  Supersedes, for the type that `stamp` gives, the `Error::Unsynced` of
  https://github.com/synnaxlabs/foundation/pull/1390#issuecomment-6038661706. So a
  region whose voters all have an unknown clock error admits no node by ticket, and the
  operator adds a voter with a known error: a Linux or macOS node, or, after #145, a
  Windows node with a peer of known error. Decided by `laptop.director`
  (2026-10-07T13:12:52Z):
  https://github.com/synnaxlabs/foundation/issues/1397#issuecomment-6038688551. The
  voter runs the check of apply on its applied state at the stamp, before it gives the
  join: `region::State::check`, which runs the one function that apply also runs
  (`State::admitted`), never a copy. So a join that its state refuses never gets into
  the log, which grows on every node until #253, and the joining node gets the exact
  refusal. The check takes the card, then the ticket and the admission, then the fit of
  the member, then the use of the ticket. So a peer with no good admission learns only
  that its card or its admission is forged, or that no ticket of the key that it sent is
  recorded, never a key or a name that the region holds. A repeat of an applied join
  finds its node a member before the use counts, so it gets `Unfit::Duplicate`, not
  `Used` (`laptop.architect`, 2026-10-11T08:12:48Z:
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6107014728, and
  2026-10-18T22:43:22Z:
  https://github.com/synnaxlabs/foundation/pull/3280#issuecomment-6256084322, which
  supersedes the order of item 1 of the first). A voter whose state lags can refuse a
  valid request; the node then tries again or tries another voter of its ticket.
  Decided by `laptop.architect` (2026-10-07T13:11:29Z), ruling 3 of
  https://github.com/synnaxlabs/foundation/pull/1390#issuecomment-6038661706. After a
  stop, `stamp` refuses first and draws no entropy, because a stamp after a stop has no
  use. Decided by `laptop.architect` (2026-10-07T14:27:15Z):
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6040088128. The
  refusal is `Unstamped::Stopped`, not `Error::Stopped`, by the `Unstamped` ruling
  above. Supersedes the `Error::Stopped` of
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6040088128:
  `laptop.architect` (2026-10-11T03:32:30Z),
  https://github.com/synnaxlabs/foundation/pull/2346#issuecomment-6105051086. The
  stamping voter makes each status key (UUIDv7, X27) from the stamp and its entropy, and
  the byte form refuses a name twice. Decided by `laptop.architect`
  (2026-10-07T09:27:39Z):
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6035046918. The voter
  that admits a join answers with the whole `region::Founding`: each founding member,
  also one that is not a voter, and the founding voters. The node opens with it, and
  with its voters as `Start.voters` (RAFT VOTERS). A node that joins is not one of its
  members: its record comes from its own `Join` in the log. Changed by
  `laptop.architect`, 2026-10-08T10:34:37Z, from "the founding voters and their cards":
  https://github.com/synnaxlabs/foundation/issues/1859#issuecomment-6057975061.
  Amended (2026-10-19, PR 5c of #336): the join request is the first message of a
  stream that goes both ways, of `Class::Complete`, as a blob stream: the answer is
  bulk. The voter gives a refusal as `Unjoined`. An admitted join gets `Joined` with
  the length of `region::Founding::encode`, then the founding in `Founding` parts of
  at most one message each. The joining node refuses a length over
  `FOUNDING_BYTES_MAX` (64 MiB, 7.5 times the 8,897,816 bytes, measured on box2, of a
  founding of 100,000 channels) before it reads a part. No later build makes it
  smaller. Only a
  first open checks it: a founding over it gives `Error::Unjoinable { bytes, most }`,
  so a node with a stored founding always opens. Lost: a founding of at most one
  message, which fails the first open of a large region, and a head with the members
  and homes, which still has no bound and needs the chunks of the founding tree at
  the voter (`laptop.architect`, 2026-10-19T00:03:42Z:
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6256819172).
  `Config::time` gives the mesh time of the stamp, and a voter with no such time answers
  `Unsynced` (same ruling). `stamp` gives the `Refusal` of the answer, so `Unstamped` is
  gone, and `Unstamped::Stopped` above is `Refusal::Stopped`, which holds no cause: the
  cause is a fault inside the voter, and the joining node is not a member yet, so it
  must not read it. Each later call on this node gives `Error::Stopped` with the cause
  (`laptop.architect`, 2026-10-19T11:58:22Z, item 2 of
  https://github.com/synnaxlabs/foundation/pull/3335#issuecomment-6266662803, which
  supersedes "with the first cause" of
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6040088128). The
  decode of a request refuses more than 64 status names, and the voter stops the stream
  with code 2: the status names are a table of the build, so more than 64 is a defect
  that no answer helps, and a refusal kind is a format that each later build keeps. Only
  `Unsynced` of item 2 goes back as a kind of `Unjoined`. A voter makes one try: when
  the join does not commit in it, it answers `Unled`, at once when a new leader replaces
  its entry, and a tick after the try when no leader takes it. The joining node asks
  again later, as after `Unsynced`, `NoVote`, and `Stopped`. An entry of an earlier try
  that applies later gives `Joined` at the next ask. Lost: a count refusal, and a voter
  that tries again with no bound (`laptop.architect`, 2026-10-19T04:13:39Z:
  https://github.com/synnaxlabs/foundation/pull/3335#issuecomment-6259492643). The asks
  again of the joining node after `Unled` end within a bound, and then the node gives an
  error whose text says that no leader took the join (the final call of
  `laptop.monitor`, 2026-10-19T04:46:28Z: "Make that ask again end within a bound, with
  a text that says no leader took the join.", recorded in
  https://github.com/synnaxlabs/foundation/pull/3335#issuecomment-6259874004). PR 5d of
  #336 builds it. Since `Unled` also comes after a new leader replaced the entry, that
  text says that no leader committed the join, and 5d also builds the status names as a
  bounded type of `status` (`laptop.architect`, 2026-10-19T11:58:22Z, items 1 and 4 of
  https://github.com/synnaxlabs/foundation/pull/3335#issuecomment-6266662803; OK of
  `laptop.monitor`, 2026-10-19T12:33:19Z:
  https://github.com/synnaxlabs/foundation/pull/3335#issuecomment-6267301786).
  An idle voter holds no copy of the founding: the `Mesh` keeps a `Weak` of the
  checked `founding` file, and each answer holds the strong copy, so answers at one
  time share one copy. One read runs at one time, and an answer that starts during it
  waits and takes its copy. The copy is the framed bytes of the read, and the parts
  are ranges of it. There is no count of answers in flight.
  Lost: a copy for the life of the voter, 8.9 MB at idle for 100,000 channels
  (`laptop.architect`, 2026-10-19T00:16:42Z:
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6256957634, and
  2026-10-19T00:24:25Z:
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6257042845). A
  failed read of the file after the admission ends the reply half with no mesh code,
  because the node is a member, and it can ask again.
  Until snapshots (#253), a region whose founders all left cannot admit a node. `secret`
  finds no key itself: `ops` and `node` read the member and pass its seal key. A
  rotation and `Remove` wait for a caller; a rotation that only the node signs lets a
  stolen key lock the node out. Lost: a record that only the admitting
  voter checks (a voter that lies admits any key, against BQ12). A `card::Signed` holds
  the `node::Key` that its signature covers
  (`Signed::key`): the key cannot come from the public key, which can rotate, so the
  signed card is its one place (decided by `laptop.architect`, 2026-10-07T08:07:47Z:
  https://github.com/synnaxlabs/foundation/issues/1259#issuecomment-6033747312). A
  `Signed` comes only from `sign` or from `Unchecked::check`. `Signed::decode`
  (crate-private) checks the signature and gives `None` when it does not hold; a change
  record holds an `Unchecked` card, which each node checks at apply. Approved by
  `laptop.architect` (2026-10-07T11:17:10Z):
  https://github.com/synnaxlabs/foundation/pull/1323#issuecomment-6036785467. The field
  is `ephemeral`, never `expiry`, because the join ticket's expiry is a mesh time
  (`Stamp`) with another meaning (decided by `laptop.architect`, 2026-10-07T10:14:01Z:
  https://github.com/synnaxlabs/foundation/pull/1322#issuecomment-6035800302). Decided
  by the architect, #242
  (https://github.com/synnaxlabs/foundation/issues/242#issuecomment-6030855135). A
  `mesh::ticket::Ticket` is the secret part that an operator carries: the private key,
  the region's prefix, and the voters to dial first (X37), at least one. Its `Debug`
  writes the region and the public key only, and it has no `Display`, `Clone`, or
  equality. `Ticket::admission` signs `foundation/admission/1`, the card's node key, and
  the card's byte form. The region's `ticket::Record` holds the public key, the
  `Options` (prefix, reusable, expiry, ephemeral), and the use count. `Record::check`
  refuses, in this order, a forged admission, a name outside the prefix, and a join at
  or after the expiry. `Record::unused` refuses a second use of a single-use ticket.
  Apply counts a use only when all checks pass, so a refused join never uses up a
  ticket. The expiry is the first mesh time at which the ticket admits no node, so the
  `Expired` text is "ticket {public_key} expired at {expiry}, and the join is at {at}".
  The text and the admit order approved by `laptop.architect` (2026-10-07T11:03:29Z):
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6036571225; the use
  moved after the fit of the member (`laptop.architect`, 2026-10-18T22:43:22Z,
  https://github.com/synnaxlabs/foundation/pull/3280#issuecomment-6256084322). The
  ephemeral expiry of a `Member` comes from its ticket, because the admin decides what a
  ticket admits (BQ11a) and the joining node is outside input. Lost: a bearer secret in
  the `Join`, which every member could replay and which binds to no card. Decided by
  `laptop.architect` (2026-10-07T09:27:39Z):
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6035046918. A
  `ticket::Voter` holds only the node key, the public key to pin, and the addresses, not
  a signed card: the ticket is the trust root, so a voter's signature over its own card
  checks nothing that the ticket does not give. The ticket's text form can reuse the
  byte form of `Addresses`. Decided by `laptop.architect` (2026-10-07T10:14:01Z):
  https://github.com/synnaxlabs/foundation/pull/1322#issuecomment-6035800302. A `Ticket`
  change (kind 3) records a ticket's public key and `Options`. Apply refuses a second
  record for one public key and a prefix that is not under the region's prefix; the
  signature of the admin who made the ticket waits for #1213. A `Join` change (kind 2)
  carries the ticket's public key, a `Stamp` (the later edge of the admitting voter's
  mesh time interval; a voter with no mesh time of known error at or after the Unix
  epoch stamps no `Join`), the node key, the card and its signature, the admission,
  and the status keys, which the voter assigns (UUIDv7). Apply refuses, in this order,
  a forged card, an unknown ticket, each refusal of `Record::check`, a reserved name
  (A3), a name outside the region, a status channel `<name>.<status>` that is longer
  than a name can be or reserved, a key that is already a member, a public key that the
  card of a member holds (`Unfit::Held`; `laptop.architect`, 2026-10-08T22:29:26Z,
  https://github.com/synnaxlabs/foundation/issues/2023#issuecomment-6070338077), a
  name that a member holds, a status key that a member holds or that the join repeats
  (A4), and the refusal of `Record::unused`. So no refusal counts a use. A member's
  names are its card name and each `<name>.<status>`, and two names are equal when they
  differ only in ASCII case (A3, X27), so each full name maps to at most one member.
  Region state cannot see the keys of the spec, so the status key check covers members
  only. The name and key checks are one function, which `State::new` also runs on the
  founding members; both give a `region::Unfit`, which `Refused::Unfit` wraps. A member
  and a `Join` hold at most 64 status entries, as the 32 of `Addresses`. Decided by
  `laptop.architect` (2026-10-07T10:44:26Z):
  https://github.com/synnaxlabs/foundation/pull/1328#issuecomment-6036265582. The type
  `mesh::status::Status` holds the cap of 64: `Status::new` refuses more (`Many`), and
  the decode refuses more before it reads an entry. So each `Member` that `encode`
  writes decodes, and `region::Unfit` has no count check. Lost: `Unfit::Many` in the
  member checks, which covers only where they run. Decided by `laptop.architect`
  (2026-10-07T11:11:49Z):
  https://github.com/synnaxlabs/foundation/pull/1328#issuecomment-6036702954. A refused
  change changes no region state, with one exception: a refused `Join` binds its node
  key to its public key when no join bound it before (MESH DRIVER; `laptop.architect`,
  2026-10-18T23:23:43Z,
  https://github.com/synnaxlabs/foundation/pull/3280#issuecomment-6256460268, item 1,
  which supersedes "a no-op on every node"). So a forged card in the log cannot stop a
  node. A `Join` holds a `card::Unchecked`, not a `card::Signed`: it has the byte form
  of a signed card, decode keeps a join whose signature does not hold, and apply
  refuses it as `Forged`. Each number in a change is little endian; a `Ticket` is the
  public key, the prefix behind a length byte, a reusable byte (0 or 1), the expiry (8
  bytes), and the ephemeral span behind a presence byte. Decided by `laptop.architect`
  (2026-10-07T09:27:39Z):
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6035046918. The
  reserved name check is region state, not a ticket check, because "no member name is
  reserved" holds for every member, like "no key twice". Decided by `laptop.architect`
  (2026-10-07T10:14:51Z):
  https://github.com/synnaxlabs/foundation/pull/1322#issuecomment-6035813123.
  Amended (2026-10-10, #1744): a founding member, which no ticket admitted, has 64
  zero bytes as its admission. Only the apply of a `Join` checks an admission. Lost:
  `admission: Option<[u8; 64]>`: no code reads an admission after the apply of its
  `Join`, so a presence byte adds no check and changes the byte form of each member.
  Decided by `laptop.architect` (2026-10-10T03:33:06Z:
  https://github.com/synnaxlabs/foundation/issues/2225#issuecomment-6093336972).
  A `Card` change (kind 5) makes a card the card of its member, so a node can change
  its addresses. It holds a `card::Unchecked` in the byte form of a `Join` card: the
  node key, the card, and its signature. Apply refuses, in this order, a card whose key
  is not a member (`Refused::Stranger`), whose signature does not hold with the
  member's public key (`Forged`), that changes more than the addresses and the version
  (`Changed`), and whose version is not above the held version (`Behind`). So a repeat
  and a late older proposal are no-ops, and the name and the keys of a member never
  change. Approved by `laptop.architect` (2026-10-18T03:29:11Z:
  https://github.com/synnaxlabs/foundation/issues/3115#issuecomment-6245196820).
