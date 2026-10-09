- **OPS OUTPUT (#337, 2026-10-08)** The plan output is a typed `plan::Output { base,
  changes, homes, added, changed, removed }`, with `Change { action, kind, name, place:
  Option<Place> }`. It derives `Serialize`, `Deserialize`, and `JsonSchema`, and `ops`
  resolves each span to a file, line, and column. `text()` gives the terminal text, as
  `Response` and `Error` do. #1744 makes it a `Response` variant. Each error of `ops`,
  in the CLI and in MCP, has one JSON shape: `{"errors": [...]}`, each item a typed
  `Problem { code, message, fix, place: Option<Place>, notes }`. An error that is not a
  diagnostic, such as `ops.argument`, is a list of one item with no place. A place is
  `{"file", "line", "column"}`, each count from 1, and the key is absent when there is
  no place. `notes` is a list, empty when there is no note. The text of an error is each
  item as `error[<code>]: <message>`, its place, `fix: <fix>`, and each note, with an
  empty line between two items. Lost: a second shape for diagnostics, with which a
  client parses two shapes and must know which operation gives which. Decided by
  `laptop.architect-2` (2026-10-08T16:26:42Z,
  https://github.com/synnaxlabs/foundation/pull/1911#issuecomment-6064364728). The
  place and notes details: `laptop.architect-2` (2026-10-08T16:40:27Z,
  https://github.com/synnaxlabs/foundation/pull/1911#issuecomment-6064603605).
  A subject change has `fingerprints`: the `SHA256:` fingerprint of each key, as
  `ssh-keygen -l` writes it, sorted by the bytes of the key, after the apply, or before
  it for a removal. The key is absent for each other kind. The text gives one line
  `    key SHA256:...` for each, under its change. Lost: a diff of the keys of a change;
  a reviewer must trust the end state, and the full list shows each key it grants.
  Trigger: `fingerprints` is the only field of `Change` for one kind. When a second
  kind adds a field of its own, those fields become one tagged `detail` enum. Decided
  by `laptop.architect-2` (2026-10-08T17:21:06Z,
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6065299343).
  `config` owns the OpenSSH form, so `config::openssh::fingerprint(PublicKey) ->
  String` makes it, and `ops` has no `ssh-key`. Lost: `ssh-key` in `ops`, a second
  owner of the form. Decided by `laptop.architect-2` (2026-10-08T17:56:21Z,
  https://github.com/synnaxlabs/foundation/pull/1943#issuecomment-6065898724).
  Supersedes the `ops` dependency on `ssh-key` in item 3 of
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6065299343.
  `plan` refuses a path that is not UTF-8 with `ops.path-not-utf8`, before its
  extension, so `Place::file` is the exact path. `ops.path-not-utf8` has no span,
  since a place cannot hold the path, and writes it with `{path:?}` until #941.
  `ops.unknown-extension` gets a place and names no path in its message (FRONT ENDS),
  so its path is exact too (`laptop.architect-2`, 2026-10-08T18:11:55Z,
  https://github.com/synnaxlabs/foundation/pull/1950#issuecomment-6066159732).
  Supersedes
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6066070872.
  Supersedes item 4 of
  https://github.com/synnaxlabs/foundation/pull/1911#issuecomment-6066009681.
  Lost: the lossy text, with which two files give one place; the `Debug` form in
  `Place::file`, which each JSON reader must decode; and the escaped path in the
  message of `ops.unknown-extension`, which an agent must decode. The `Place::file` doc
  names the code, not the function that refuses the path (`laptop.architect-2`,
  2026-10-08T18:12:57Z,
  https://github.com/synnaxlabs/foundation/pull/1950#issuecomment-6066177474).
  Supersedes items 2 and 3 of
  https://github.com/synnaxlabs/foundation/pull/1911#issuecomment-6066009681.
  Decided by `laptop.architect-2` (2026-10-08T18:03:02Z,
  https://github.com/synnaxlabs/foundation/pull/1911#issuecomment-6066009681).
  The apply gives six codes of its own. A plan file path that is not UTF-8 is
  `ops.path-not-utf8`, status 2, as in `plan`. Each `config::plan::Error` is
  `ops.bad-plan`, status 2, with its `Display` as the message and the fix ``Make a plan
  with `foundation plan`, and apply it with no edits``. A node with no spec in use or
  with `Spec::behind` set gives `ops.behind`, status 1, after the decode and before the
  base compare: the message names the newest pointer and one line for its `Cause`, and
  the fix is `Fix the cause, then plan again`. One private function of `ops` reads
  `Mesh::spec` and gives the spec in use or this error, for each caller. A plan whose
  base is not the spec in use, before the proposal or after another change applies
  first, is `ops.stale-plan`, status 1, fix `Plan again`. Each other error of
  `Mesh::apply` is `ops.apply`, status 1, with its `Display` as the message and the fix
  `Fix the cause in the message, then plan and apply again`. A stop of the group of the
  mesh, from `Mesh::spec` or from `Mesh::apply`, is `ops.stopped`, status 1, in `plan`
  and in `apply`, with the `Display` of the stop as the message and the fix `Start the
  node, then plan again`. It can come after a proposal, so plan again to see whether
  the change applied. `ops.stopped` decided by `laptop.architect-2`
  (2026-10-09T03:42:35Z, (b) of
  https://github.com/synnaxlabs/foundation/issues/1756#issuecomment-6073812317). There
  is no `ops.reserved-change`: `Plan::definitions` refuses a change at a reserved label
  (FIRST ADMIN). After `definitions`, each problem of `config::plan::check` is a
  `Config` problem with its own code and no place, status 2 (PLAN FILE): plan
  https://github.com/synnaxlabs/foundation/issues/2013#issuecomment-6071877567, approved
  by `laptop.architect-2` (2026-10-09T00:48:39Z,
  https://github.com/synnaxlabs/foundation/issues/2013#issuecomment-6071969872).
  `ops.path-not-utf8` in the apply decided by `laptop.architect-2`
  (2026-10-09T20:50:27Z, item 1 of
  https://github.com/synnaxlabs/foundation/pull/2178#issuecomment-6089032114).
  `ops.behind` and the `ops.apply` fix decided by `laptop.architect-2`
  (2026-10-08T23:51:37Z, items 1 and 2 of
  https://github.com/synnaxlabs/foundation/pull/2035#issuecomment-6071339913). The
  order and the codes `ops.stale-plan` and `ops.apply` are steps 3 and 4 of the plan
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6063695586, approved
  by `laptop.architect-2` (2026-10-09T00:07:17Z,
  https://github.com/synnaxlabs/foundation/pull/2035#issuecomment-6071513636) and by
  `laptop.architect` (2026-10-09T00:29:47Z,
  https://github.com/synnaxlabs/foundation/pull/2035#issuecomment-6071766367). The
  `ops.bad-plan` fix and the order of the base compare decided by `laptop.architect-2`
  (2026-10-08T22:57:59Z,
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6070706432).
  Supersedes the code, message, fix, and order of
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6060043966.
  The apply output is `Applied { file, pointer, added, changed, removed, homes }`, where
  `homes` counts the homes that the plan lists. Its text is `Applied <file>: <a> added,
  <c> changed, <r> removed, <h> homes listed.`, with each count of 0 left out and `1
  home listed` for one home, and `no change` only for a plan with no change and no home.
  The text without `homes` and the rule that leaves out a count of 0 are step 5 of the
  plan https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6063695586.
  `homes`, its text, and the `no change` rule decided by `laptop.architect-2`
  (2026-10-09T00:42:24Z,
  https://github.com/synnaxlabs/foundation/pull/2035#issuecomment-6071903259). The whole
  text approved by `laptop.architect-2` (2026-10-09T01:02:46Z,
  https://github.com/synnaxlabs/foundation/pull/2035#issuecomment-6072118033) and, for
  the contract with `mesh`, by `laptop.architect` (2026-10-09T00:57:47Z,
  https://github.com/synnaxlabs/foundation/pull/2035#issuecomment-6072065332).
