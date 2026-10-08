- **PLAN SURFACE (#1082, 2026-10-08)** `config::plan::plan(documents, base, applied,
  members, kinds)` gives a `config::plan::Plan { base, changes, homes }`, or
  diagnostics. `base` is the `spec::Pointer { version, root }` of the applied spec.
  `version` is 0 before the first apply, and one more at each apply. `applied` is the
  definitions of the spec at `base`, by tree key, with no problem from
  `spec::region::check`: the spec that a node uses (#1741).
  `config::plan::Plan::changes` maps each tree key to a `config::plan::Change { old, new
  }`, which holds the digest of the stored bytes and the `Entry` of the files. The
  stored bytes are the `encode` of each applied definition: `decode` takes only
  canonical bytes, so they are the bytes of the tree. The plan holds no channel key
  (A4). `homes` gives the home of each index that the stored spec has no index at. A
  channel keeps the stored key at its name, and a new name gets `Key::from_u128(n)`, a
  key that no stored channel holds. A definition changes when its encoded bytes differ
  from the stored bytes. A stored definition that no file holds is removed (A2), except
  one whose label is reserved (FIRST ADMIN), or whose kind no block of a file defines,
  such as `Time` and `Compression` until their blocks come: the files cannot state such
  a kind, so they ask for no removal. Lost: remove it, and refuse the plan, which stops
  each apply with no fix in the files (`laptop.architect`, #1886, 2026-10-08T15:22:47Z,
  https://github.com/synnaxlabs/foundation/pull/1886#issuecomment-6063170391). An
  edge that `check` cannot resolve stays `config.unknown-channel` (CHANNEL BLOCK). An
  edge to a channel of the wrong kind is `config.wrong-channel`. `place` runs for each
  index, with the node of its first writer: a connector whose `writes` holds the index
  or a channel on it. Its `Tie`, or the `Homeless` of its `home`, is `config.unplaced`
  at the label of the index, with each placement by its label.
  `place` gives `Result<Placed, Tie>`, and `Placed::home` is `Result<&Name,
  Homeless>`, so `config` takes the winner from `Placed::placement` also when there is
  no home. `config` and `place` each keep a private `label`: the one in `config` fails
  loud on its own keys, and the `Display` of `Tie` and `Homeless` falls back to the key
  (`laptop.architect-2`, 2026-10-08T21:10:14Z,
  https://github.com/synnaxlabs/foundation/issues/1903#issuecomment-6069112623).
  `config.unknown-node` is at each node that a connector or a placement names and that
  `members` does not hold, and the fix names a member that is equal to it without case.
  `config.writer-nodes` is at the `node` of the first connector on a second node that
  writes one index. The first writer, the first of the names of one key, and the first
  connector of a name come first by `Source`, then in source order, so the order of
  `documents` changes no problem (#1886 round 2, 2026-10-08T14:14:16Z,
  https://github.com/synnaxlabs/foundation/pull/1886#issuecomment-6061802143). A tie,
  with no span or with one `Source` in two Documents, has no defined choice (#1886 round
  4, 2026-10-08T14:35:32Z,
  https://github.com/synnaxlabs/foundation/pull/1886#issuecomment-6062234090), approved
  as merged (`laptop.architect`, 2026-10-08T15:21:08Z,
  https://github.com/synnaxlabs/foundation/pull/1886#issuecomment-6063133042), which
  changes the text of
  https://github.com/synnaxlabs/foundation/pull/1886#issuecomment-6062122870. Trigger:
  before a path makes Documents with no spans, such as an SDK that builds a spec in
  code, PLAN SURFACE states the order on a tie (the order of `documents`), with a test
  for the writer, for the connector of a name, and for the name of a key. The problems
  come in `Source` order, then in source order, as the problems of `check` do.
  `place` also runs for each connector, with the connector's `node` as `writer`, and its
  `Tie` or `Homeless` is `config.unplaced` at the label of the connector. The fix of
  `Homeless::Overlap` is "Move the node to `home` when it is the one node of the
  placement, else remove it from the placement": `Overlap` occurs only when the
  placement names no `home`, and a removal that leaves no node gives
  `config.empty-placement`. Two inputs need two edits. In the first, the node is the
  one node of `p`, and `p` wins for a connector on another node. The move then gives
  `config.connector-home`, whose fix plans. `Homeless::fix` is static and cannot name
  that connector (`laptop.architect`, 2026-10-08T16:51:46Z,
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6064796239, item 2,
  changed by `laptop.architect`, 2026-10-08T17:23:56Z,
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6065346716, and at
  2026-10-08T17:30:22Z,
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6065460014).
  In the second, `p` names two or more nodes, and each is the node of an `Overlap` of
  `p`. The removals then give `config.empty-placement`, whose fix names a node
  (`laptop.architect`, 2026-10-08T17:11:23Z,
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6065131594, changed
  by `laptop.architect`, 2026-10-08T17:22:53Z,
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6065329233).
  `config.connector-home` (X22) is at the `home` of a placement `p` that wins for a
  connector `a` on the node `n` and names another node. Its fix is "Name `n` as the
  `home`, and keep `n` out of `standby` and `copies`" when `p` wins for no connector on
  another node, and for no index whose nearest connector is on another node, because a
  new `home` moves each index that `p` wins (`laptop.architect`, 2026-10-08T16:22:59Z,
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6064298961). Else it
  is "Exclude the connector `a` and its indexes from the `select` of `p`, and select
  them with another placement whose `home` is `n`", which changes no other connector of
  `p`, and which lists after `p` each placement that wins for an index of `a`. The
  indexes of a connector are those whose nearest connector it is, as
  `config.split-placement` reads them (`laptop.architect`, #1901, 2026-10-08T15:12:13Z,
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6062948556,
  2026-10-08T15:21:54Z,
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6063150126,
  2026-10-08T15:31:52Z,
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6063369171, and
  2026-10-08T16:04:09Z, for "another" and the placements of the indexes,
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6063962123).
  `config.split-placement` (BQ10) is at each index when the placement that wins for it
  is not the one that wins for its nearest connector, the connector with the longest
  name above the index (`Name::starts_with`): at the label of the index's placement, or
  of the connector's when no placement selects the index. So the index `d.e.time`
  follows the connector `d.e`, not `d`: the indexes of BQ10 are the connector's own, the
  unit of failover (`laptop.director`, #1901, 2026-10-08T15:20:28Z,
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6063118459). Its fix
  is "Make the placement `p` win for the connector `c` and its indexes", where `p` wins
  for `c`, or for the index when no placement selects `c` (same comment of 15:21:54Z,
  and `laptop.architect`, 2026-10-08T16:22:59Z). It is the target state that each other
  fix names, so each split diagnostic of `c` gives one edit. When no placement can win
  for `c` and each of its indexes at the node `n` of `c`, each diagnostic of `c` gives
  one fix that names each winner, so one edit applies it. When `t` wins for `c` and gets
  case 2 of `config.connector-home`, the fix is that of case 2: "Exclude the connector
  `c` and its indexes from the `select` of `t` and `r`, and select them with another
  placement whose `home` is `n`", where `t` and `r` are each placement that wins for `c`
  or an index of `c`. When no placement selects `c`, and more than one placement wins
  for the indexes of `c` or one names a `home` that is not `n`, the fix is "Exclude the
  indexes of the connector `c` from the `select` of `p`, and select the connector and
  its indexes with another placement whose `home` is `n`", where `p` is each placement
  that wins for an index of `c`. When `p`, the placement that the fix names (the winner
  of `c`, else the one placement that wins for its indexes), names no `home` and an
  index of `c` has no writer, the fix is "Name `n` as the `home` of `p`, keep `n` out of
  its `standby` and `copies`, and make `p` win for the connector `c` and its indexes"
  when `p` wins for no connector on another node, and for no index whose nearest
  connector is on another node, because a placement with no `home` cannot hold an index
  that no connector writes. Else it is the case 2 text, or the text for no placement
  that selects `c` (`laptop.architect`, 2026-10-08T16:51:46Z,
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6064796239, item 1). A
  list of winners is "`p`", "`p` and `q`", or "`p`, `q`, and `r`": the winner of `c`
  first, then the others in tree key order. "Another" keeps a listed placement from
  being the new one, which its exclusion would empty (`laptop.architect`,
  2026-10-08T15:46:46Z,
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6063647980, and
  2026-10-08T16:04:09Z,
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6063962123). A tie for
  the index or the connector gives no `config.split-placement`. The region check and the
  region of each key (REGION CHECK) come with #1029. Lost: a `Planned` with keys (A4), a
  home on each change, a `config::Error` for a lazy fetch of chunks, a provisional tree
  and `tree::diff`, which writes chunks that the plan drops, and the chunks of the
  applied tree as an input, with which `ops` reads the tree a second time and a missing
  chunk panics in `config`, though #1741 names that case (`Cause::Tree`), and, for
  checks 2 and 3, a `spec::placement::check` over the whole spec, a second text in
  `config`, no report for the `Tie` or `Homeless` of a connector, a check against each
  connector above the index, with which two nested connectors on two nodes share one
  placement, the `config.connector-home` fixes "Leave out `home`", which can leave an
  empty placement or an index with no home, and "Name `n` as the `home`" in each case,
  which moves the problem between two connectors of one placement, and "Select the
  connector `a` and each index under its name with a more specific placement", which no
  placement can follow when `p` names `a` by its exact name, and the
  `config.split-placement` fix "and each name under it", which also moves the indexes of
  a nested connector, and, when no placement can win for `c` and each of its indexes at
  `n`, a fix that names one placement, which moves the index `i` alone or conflicts with
  the fix of another diagnostic of `c`, case 1 when `p` wins for an index whose nearest
  connector is on another node, which moves that index away from its connector, and the
  `config.split-placement` fix "and the index `i`", which gives each split diagnostic of
  `c` another edit, the node of the nearest connector as the home of an index with no
  writer, which guesses a home for data that no connector on that node makes, the
  `Overlap` fix "Move the node to `home`, or remove it from the placement", which offers
  a removal that leaves no node, an `Overlap` fix computed in `config` for each name,
  which gives one variant a second source of text, and a fix computed in `config` from
  each `Overlap` of `p`.
  Supersedes the fix texts of
  https://github.com/synnaxlabs/foundation/issues/1082#issuecomment-6062457087,
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6062816747, and
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6063036478, the
  case 1 condition of
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6062948556, the
  `config.split-placement` fix "and the index `i`" of
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6063150126, the
  check of an index against each connector above it of
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6063036478, case 2 of
  the `config.connector-home` fix of
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6062948556 and
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6063150126, the
  `config.split-placement` fix of
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6063150126 when no
  placement can win for `c` and each of its indexes at `n`, the
  `config.split-placement` fix of
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6064298961 (item 2)
  when `p` names no `home` and an index of `c` has no writer, the `Overlap` fix of
  https://github.com/synnaxlabs/foundation/issues/1082#issuecomment-6062513561, the
  case 2 text of
  https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6063369171 and the
  text of https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6063647980,
  which name one placement and "a placement", the `chunks` input and its panic of
  https://github.com/synnaxlabs/foundation/issues/1082#issuecomment-6053787187, and the
  provisional tree of
  https://github.com/synnaxlabs/foundation/issues/1082#issuecomment-6040866688 and its
  "which no stored v7 key can be". Decided by `laptop.architect-2`
  (2026-10-07T15:17:11Z,
  https://github.com/synnaxlabs/foundation/issues/1082#issuecomment-6040866688, and
  2026-10-08T06:24:09Z,
  https://github.com/synnaxlabs/foundation/issues/1082#issuecomment-6053787187). The
  `version` doc and the `Hash` derive: `laptop.architect` (2026-10-08T06:35:18Z,
  https://github.com/synnaxlabs/foundation/issues/1082#issuecomment-6053976538).
  `applied` in place of chunks, the key that no stored channel holds, and one sort:
  `laptop.architect` (2026-10-08T13:26:52Z,
  https://github.com/synnaxlabs/foundation/pull/1886#issuecomment-6060906734). Checks 2
  and 3: `laptop.architect-2` (2026-10-08T14:47:21Z,
  https://github.com/synnaxlabs/foundation/issues/1082#issuecomment-6062457087),
  approved by `laptop.architect` (2026-10-08T14:50:25Z,
  https://github.com/synnaxlabs/foundation/issues/1082#issuecomment-6062513561).
