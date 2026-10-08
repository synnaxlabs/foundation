- **SPEC CHANGE (#1083)** A `Spec` change record (kind 4) moves a region's spec pointer
  by compare-and-swap. It holds the base pointer, the new root, and the digests of the
  new tree's chunks, never their bytes, so `mesh` moves the pointer without the chunks.
  Byte form: the base version (8 bytes, little-endian), the base root (32), the new root
  (32), the chunk count (2 bytes, little-endian), then each digest (32), in strictly
  rising order, then the holder count (2 bytes, little-endian), then each holder key
  (16), in strictly rising order, then the home count (2 bytes, little-endian), then
  each index key (16) and its home key (16), in strictly rising index order (S12
  (placement part) + B7). The new version is `base.version + 1`. Lost: a version
  in the record, which can disagree with the base. A record lists at most `CHUNKS_MAX` =
  1024 digests, about 32 KiB, so that one record fits in an append of 64 KiB, a node's
  limit; decode refuses a larger count. An entry over a member's limit is never sent,
  and `raft` sends it again with no end (#1361). `raft` bounds an `Append` by its count
  of entries, not by its bytes, so two records at the bound in one `Append` go over 64
  KiB. #1361 bounds it by bytes before a milestone applies a spec change to a region of
  more than one member. Trigger: before a milestone applies a change that lists more
  than `CHUNKS_MAX` chunks, a `Spec` change can list them. The holders are the voters
  whose durable put of the listed chunks the proposer counted; until #1231 they are only
  the proposer. A record lists at most `HOLDERS_MAX` = 64 holders, and decode refuses a
  larger count, so a record at both bounds is 33 869 bytes. A majority of each half must
  fit in `HOLDERS_MAX` holders, and a holder in both halves counts in each. So a
  configuration with one set has at most 127 voters. Two halves that share no voter fit
  when their majorities sum to at most 64. This binds only after #1231. Decided by
  `laptop.architect`, 2026-10-08T12:38:01Z
  (https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6060034191).
  Supersedes the voter bound and the sum of 33 873 bytes of
  https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059278643, and the
  voter bound of
  https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059660161. The sum
  of 33 869 bytes: `laptop.architect`, 2026-10-08T12:46:52Z
  (https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6060184527). Before
  #1231 counts peers as holders, the rule of a majority of each half moves to
  `raft::Voters::quorum` (#1875). Every member refuses, at apply, a change whose
  holders are not a majority of each half of the voters as of the entry
  (`Refused::Quorum`): the voters of the last `Voters` entry at or before it, or the
  founding voters. So a `Voters` entry between the propose and the commit cannot leave
  the pointer at chunks that no majority holds. The record lists only the chunks that
  the base tree lacks, so the rule also needs the chunks of the base on a majority after
  a change of voters (#1231). Every member applies a change whose base is the pointer,
  and refuses one whose base is not (`Refused::Stale`), so of two changes from one base
  only the first applies. The state machine never reads chunks and never runs a check: a
  committed spec with problems moves the pointer, and the node keeps the last spec it
  used (#1741). The pointer before the first change is version 0 at the root of the tree
  of `Config::founding.definitions`. No BQ12 signature check on the change in this
  milestone (#1213). The pointer is `spec::Pointer`, and `mesh` has no pointer type of
  its own (#1887; `laptop.architect`, 2026-10-08T13:26:52Z,
  https://github.com/synnaxlabs/foundation/pull/1886#issuecomment-6060906734, and the
  removal at 2026-10-08T16:04:51Z,
  https://github.com/synnaxlabs/foundation/pull/1913#issuecomment-6063975359). This
  supersedes the `mesh::Pointer` of
  https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6055806836.
  `Mesh::open` refuses no founding definitions: the founding is agreed region state,
  and a refusal at each open stops a node on a later build whose checks find more
  problems. The node that founds the region checks the founding with the `spec`
  function of #1841, and does not found a region whose founding has problems (#1744).
  A founding with problems at a later build follows the rule of a committed spec with
  problems (#1741). No refusal of the founding at open decided by `laptop.architect`,
  2026-10-08T15:42:09Z
  (https://github.com/synnaxlabs/foundation/pull/1897#issuecomment-6063561498), which
  changes "runs no check of `Config::founding`" in
  https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056116151. Decided
  by `laptop.architect`: chunks through
  `blob` and no BQ12 check, 2026-10-07T06:42:23Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6032512454); a spec
  with problems, 2026-10-07T07:03:20Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6032786065); the
  founding definitions, 2026-10-08T06:12:36Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6053614771); the
  kind, its byte form, the version from the base, `CHUNKS_MAX`, `Refused::Stale`, and
  the move of `Pointer` to a layer 1 crate, 2026-10-08T08:22:08Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6055806836); the
  founding at open, 2026-10-08T08:41:43Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056116151); the
  bound of an `Append` in bytes, 2026-10-08T08:44:55Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056167437), with
  "member" for "voter", 2026-10-08T08:46:16Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056189352); the
  holders, the refusal at apply, the quorum rule, and the trigger for `CHUNKS_MAX`,
  2026-10-08T11:03:36Z
  (https://github.com/synnaxlabs/foundation/issues/1741#issuecomment-6058455178).
  `HOLDERS_MAX` and the move to `raft`, 2026-10-08T11:53:51Z
  (https://github.com/synnaxlabs/foundation/pull/1872#issuecomment-6059278643).
