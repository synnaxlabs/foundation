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
  stamping voter makes each status key (UUIDv7, X27) from the stamp and its entropy,
  and the byte form refuses a name twice. Decided by `laptop.architect`
  (2026-10-07T09:27:39Z):
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6035046918. The voter
  that admits a join answers with the whole `region::Founding`: each founding member,
  also one that is not a voter, and the founding voters. The node opens with it, and
  with its voters as `Start.voters` (RAFT VOTERS). A node that joins is not one of its
  members: its record comes from its own `Join` in the log. Changed by
  `laptop.architect`, 2026-10-08T10:34:37Z, from "the founding voters and their cards":
  https://github.com/synnaxlabs/foundation/issues/1859#issuecomment-6057975061.
  Until snapshots (#253), a region whose founders all left cannot admit a node. `secret`
  finds no key itself: `ops` and `node` read the member and pass its seal key. A
  rotation, a new card, and `Remove` wait for a caller; a rotation that only the node
  signs lets a stolen key lock the node out. Lost: a record that only the admitting
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
  `Options` (prefix, reusable, expiry, ephemeral), and the use count. `Record::admit`
  refuses, in this order, a forged admission, a name outside the prefix, a join at or
  after the expiry, and a second use of a single-use ticket, and counts a use only when
  all checks pass, so a refused join never uses up a ticket. The expiry is the first
  mesh time at which the ticket admits no node, so the `Expired` text is "ticket
  {public_key} expired at {expiry}, and the join is at {at}". The text and the admit
  order approved by `laptop.architect` (2026-10-07T11:03:29Z):
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6036571225. The
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
  a forged card, a reserved name (A3), a name outside the region, a status channel
  `<name>.<status>` that is longer than a name can be or reserved, a key that is
  already a member, a public key that the card of a member holds (`Unfit::Held`;
  `laptop.architect`, 2026-10-08T22:29:26Z,
  https://github.com/synnaxlabs/foundation/issues/2023#issuecomment-6070338077), a
  name that a member holds, a status key that a member holds or
  that the join repeats (A4), an unknown ticket, and each refusal of `Record::admit`.
  So no refusal counts a use. A member's names are its card name and each
  `<name>.<status>`, and two names are equal when they differ only in ASCII case (A3,
  X27), so each full name maps to at most one member. Region state cannot see the keys
  of the spec, so the status key check covers members only. The name and key checks are
  one function, which `State::new` also runs on the founding members; both give a
  `region::Unfit`, which `Refused::Unfit` wraps. A member and a `Join` hold at most 64
  status entries, as the 32 of `Addresses`. Decided by `laptop.architect`
  (2026-10-07T10:44:26Z):
  https://github.com/synnaxlabs/foundation/pull/1328#issuecomment-6036265582. The type
  `mesh::status::Status` holds the cap of 64: `Status::new` refuses more (`Many`), and
  the decode refuses more before it reads an entry. So each `Member` that `encode`
  writes decodes, and `region::Unfit` has no count check. Lost: `Unfit::Many` in the
  member checks, which covers only where they run. Decided by `laptop.architect`
  (2026-10-07T11:11:49Z):
  https://github.com/synnaxlabs/foundation/pull/1328#issuecomment-6036702954. A refused
  change is a no-op on every node, so a forged card in the log cannot stop a node. A
  `Join` holds a `card::Unchecked`, not a `card::Signed`: it has the byte form of a
  signed card, decode keeps a join whose signature does not hold, and apply refuses it
  as `Forged`. Each number in a change is little endian; a `Ticket` is the public key,
  the prefix behind a length byte, a reusable byte (0 or 1), the expiry (8 bytes), and
  the ephemeral span behind a presence byte. Decided by `laptop.architect`
  (2026-10-07T09:27:39Z):
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6035046918. The
  reserved name check is region state, not a ticket check, because "no member name is
  reserved" holds for every member, like "no key twice". Decided by `laptop.architect`
  (2026-10-07T10:14:51Z):
  https://github.com/synnaxlabs/foundation/pull/1322#issuecomment-6035813123.
