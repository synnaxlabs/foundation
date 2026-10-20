- **SPEC CHANGE (#1083)** A `Spec` change record (kind 4) moves a region's spec pointer
  by compare-and-swap. It holds the base pointer, the new root, and the digests of the
  new tree's chunks, never their bytes, so `mesh` moves the pointer without the chunks.
  Byte form: the base version (8 bytes, little-endian), the base root (32), the new root
  (32), the chunk count (2 bytes, little-endian), then each digest (32), in strictly
  rising order, then the holder count (2 bytes, little-endian), then each holder key
  (16), in strictly rising order, then the home count (2 bytes, little-endian), then
  each index key (16) and its home key (16), in strictly rising index order (S12
  (placement part) + B7). The new version is `base.version + 1`. A change whose new root
  is the base root and that holds no home leaves the pointer at the base. The compare
  still checks the base. Lost: a version in the record, which can disagree with the
  base. A record lists at most `CHUNKS_MAX` = 1024 digests, about 32 KiB, so that
  `message::BYTES_MIN` stays under 64 KiB, the default limit of a node; decode refuses a
  larger count. `change::BYTES_MAX` = 51 279 bytes is the largest record of any kind: a
  `Spec` change at each bound. `raft` drops the last entries of an `Append` until it
  fits `message::BYTES_MIN` = `BYTES_MAX` + 100, an `Append` of one such record with no
  proof and no link (RAFT SURFACE, MESH WIRE). So a record at the bound goes alone
  (#1361; `laptop.architect`, 2026-10-12T18:29:52Z,
  https://github.com/synnaxlabs/foundation/issues/1361#issuecomment-6130215772).
  Supersedes the bound by the count of entries of
  https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056167437 and
  https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056189352, and, in
  the reason for `CHUNKS_MAX` of
  https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6055806836, the room
  left for the rest of the append. Trigger:
  before a milestone applies a change that lists more than `CHUNKS_MAX` chunks, a `Spec`
  change can list them. The holders are the voters whose durable put of the listed
  chunks the proposer counted: the proposer when it is a voter, and each voter whose
  put of them from the proposer ended `Ok` while a half that holds it lacked a majority
  (SPEC APPLY). A record lists at most `HOLDERS_MAX` = 128 holders, and decode refuses a
  larger count, so a record at the bounds of chunks and holders is 34 893 bytes.
  `HOLDERS_MAX` is 128, the sum of the majorities of two halves of 127 voters each (MESH
  WIRE), so the holders that SPEC APPLY counts always fit. Decided by
  `laptop.architect`, 2026-10-19T17:21:45Z
  (https://github.com/synnaxlabs/foundation/pull/3399#issuecomment-6273042572), which
  supersedes the voter bound and its check by a change of voters of 6060034191,
  `HOLDERS_MAX` = 64 of 6059278643, and the sum of 33 869 bytes of 6060184527. The
  holders and decode decided by `laptop.architect`, 2026-10-08T12:38:01Z
  (https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6060034191).
  Supersedes the voter bound and the sum of 33 873 bytes of
  https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059278643, and the
  voter bound of
  https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059660161. The sum of
  33 869 bytes: `laptop.architect`, 2026-10-08T12:46:52Z
  (https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6060184527). The rule
  of a majority of each half is `raft::Voters::quorum` (#1875), which counts a majority
  as the commit of `raft` does (`Voters::committed`). So an empty half needs no holder
  (`laptop.architect`, 2026-10-12T16:14:07Z:
  https://github.com/synnaxlabs/foundation/pull/2449#issuecomment-6128112293). Every
  member refuses, at apply, a change whose holders are not a majority of each half of
  the voters as of the entry (`Refused::Quorum`, from `raft::NoMajority`): the voters of
  the last `Voters` entry at or before it, or the founding voters. So a `Voters` entry
  between the propose and the commit cannot leave the pointer at chunks that no majority
  holds. The record lists only the chunks that the base tree lacks, so the rule also
  needs the chunks of the base on a majority after a change of voters (#1231). So
  `propose_voters` puts the tree of the pointer in force on the new set before its joint
  entry, and after the entry applies puts the tree in force again when its root moved,
  each `RETRY` of the spec in use until a majority of the set holds it. No failed put
  after the joint entry fails the call, other than one that cannot run again (a stop, a
  chunk that does not fit in a tree, which gives `Error::Tree` with
  `tree::Error::Corrupt`, or a failure of the store): the set is in force. The node
  logs a warning at the first put after the joint entry that fails and runs again, and
  at most once a minute while the put runs again, with the error of the put in the
  field `error`: `Error::Quorum` names each voter whose put failed, and `Error::Tree`
  with `tree::Error::Missing` names the chunk that this node lacks (`laptop.monitor`,
  2026-10-20T15:40:27Z,
  https://github.com/synnaxlabs/foundation/pull/3599#issuecomment-6294796432, and the
  case of `tree::Error::Missing`, 2026-10-20T16:36:55Z,
  https://github.com/synnaxlabs/foundation/pull/3599#issuecomment-6295982958). Simple
  version: `raft` leaves the joint phase on its own, so between that leave and the end
  of the second put only a majority of the old voters can hold the chunks of a `Spec`
  entry that came between the read of the pointer and the joint entry. Trigger: before a
  removed voter can leave the region, the design of the leave of a node closes the
  window (#3400; the put before a `Remove`, item 1 of the ruling on #3355). Lost: a hook
  in `raft` that holds the leave. Every
  member applies a change whose base is the pointer, and refuses one whose base is not
  (`Refused::Stale`), so after a change from one base moves the pointer, each later
  change from that base gives `Stale`. The state machine never reads chunks and never
  runs a check: a committed spec with problems moves the pointer, and the node keeps the
  last spec it used (#1741). The pointer before the first change is version 0 at the
  root of the tree of `Config::founding.definitions`. No BQ12 signature check on the
  change in this milestone (#1213). The pointer is `spec::Pointer`, and `mesh` has no
  pointer type of its own (#1887; `laptop.architect`, 2026-10-08T13:26:52Z,
  https://github.com/synnaxlabs/foundation/pull/1886#issuecomment-6060906734, and the
  removal at 2026-10-08T16:04:51Z,
  https://github.com/synnaxlabs/foundation/pull/1913#issuecomment-6063975359). This
  supersedes the `mesh::Pointer` of
  https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6055806836.
  `Mesh::open` refuses no founding definitions: the founding is agreed region state, and
  a refusal at each open stops a node on a later build whose checks find more problems.
  The node that founds the region checks the founding with the `spec` function of #1841,
  and does not found a region whose founding has problems (#1744). A founding with
  problems at a later build follows the rule of a committed spec with problems (#1741).
  No refusal of the founding at open decided by `laptop.architect`, 2026-10-08T15:42:09Z
  (https://github.com/synnaxlabs/foundation/pull/1897#issuecomment-6063561498), which
  changes "runs no check of `Config::founding`" in
  https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056116151.
  `Hub::set_definitions` (#2020), and `Hub::define` into a new hub before it, panic only
  when two channels have one key or one name, or the index of a data channel is not an
  index of the input. No spec holds two channels of one name, and `spec::region::check`
  refuses the other two cases. Each panic is a defect of the caller. `mesh` runs
  `spec::region::check` on the founding and on each committed spec before it puts it in
  use (#1741). The hub of a node in a region takes its definitions only from that
  region's mesh: the spec in use at its start, each later one through its own task, and
  the one in use at each open, when its pointer moved (`laptop.architect`,
  2026-10-10T23:49:57Z:
  https://github.com/synnaxlabs/foundation/issues/2326#issuecomment-6103471301).
  Supersedes "`node` gives the hub only the specs of `Mesh::watch_spec`" of
  https://github.com/synnaxlabs/foundation/issues/2179#issuecomment-6089059684. `node`
  runs no check of its own. A spec with problems follows #1741: the mesh keeps the spec
  it uses, which is empty for a founding with problems, and the hub gets none of it. So
  no spec from disk or a peer makes an open panic. A founding with problems defines no
  channel, and `Config::region` has no panic (#1957 PR 2). The check in the mesh only,
  by `laptop.architect`, 2026-10-09T20:52:20Z
  (https://github.com/synnaxlabs/foundation/issues/2179#issuecomment-6089059684).
  Supersedes the check in `node` of
  https://github.com/synnaxlabs/foundation/pull/1966#issuecomment-6068129791,
  https://github.com/synnaxlabs/foundation/pull/1966#issuecomment-6068123507, and
  https://github.com/synnaxlabs/foundation/issues/1957#issuecomment-6070012949. Decided
  by `laptop.architect`: chunks through `blob` and no BQ12 check, 2026-10-07T06:42:23Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6032512454); a spec
  with problems, 2026-10-07T07:03:20Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6032786065); the
  founding definitions, 2026-10-08T06:12:36Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6053614771); the
  kind, its byte form, the version from the base, `CHUNKS_MAX`, `Refused::Stale`, and
  the move of `Pointer` to a layer 1 crate, 2026-10-08T08:22:08Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6055806836); the
  founding as agreed region state, and that a check at each open stops a node on a later
  build whose checks find more problems, the check of the founding by the node that
  founds the region, and a founding with problems at a later build, 2026-10-08T08:41:43Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056116151); the
  bound of an `Append` in bytes, 2026-10-08T08:44:55Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056167437), with
  "member" for "voter", 2026-10-08T08:46:16Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056189352); the
  holders, the refusal at apply, the quorum rule, and the trigger for `CHUNKS_MAX`,
  2026-10-08T11:03:36Z
  (https://github.com/synnaxlabs/foundation/issues/1741#issuecomment-6058455178).
  The move of the quorum rule to `raft` (`Voters::quorum`), 2026-10-08T11:53:51Z
  (https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059278643). The
  panics of the hub and the check before them, agreed with `laptop.architect-2`,
  2026-10-08T20:08:50Z
  (https://github.com/synnaxlabs/foundation/pull/1966#issuecomment-6068129791), with the
  check of `Config::region` by `laptop.architect-2`, 2026-10-08T20:08:26Z
  (https://github.com/synnaxlabs/foundation/pull/1966#issuecomment-6068123507). Item 1
  of 6068129791 left out the panic of `define` on a known key or name, which HUB
  SESSIONS states (#1917). `set_definitions` has no such panic (`laptop.architect`,
  2026-10-08T22:09:38Z,
  https://github.com/synnaxlabs/foundation/issues/1957#issuecomment-6070012949). A
  change at the base root with no home leaves the pointer (#2055; `laptop.architect`,
  2026-10-09T00:57:47Z,
  https://github.com/synnaxlabs/foundation/pull/2035#issuecomment-6072065332). Lost: a
  return of `base` with no entry, which no compare checks against the region; a count of
  changes in the return, which can differ at the apply. The put of the tree by
  `propose_voters`, the retry after the joint entry, and the simple version of the
  window with its trigger: `laptop.architect`, 2026-10-19T17:02:16Z
  (https://github.com/synnaxlabs/foundation/issues/1231#issuecomment-6272668202), and
  the put before a `Remove`, 2026-10-19T23:28:13Z
  (https://github.com/synnaxlabs/foundation/issues/3355#issuecomment-6279615451); the
  deferral (kind 8) sent to `laptop.monitor`, 2026-10-19 (#3400); `Error::Tree` and
  the end of the call on a corrupt chunk, 2026-10-20T14:08:15Z
  (https://github.com/synnaxlabs/foundation/issues/1231#issuecomment-6292805335).
