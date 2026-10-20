- **OPS OUTPUT (#337, 2026-10-08)** The plan output is a typed `plan::Output { base,
  changes, homes, added, changed, removed }`, with `Change { action, kind, name, place:
  Option<Place> }`. It derives `Serialize`, `Deserialize`, and `JsonSchema`, and `ops`
  resolves each span to a file, line, and column. `text()` gives the terminal text, as
  `Response` and `Error` do. The CLI writes its `text()`, or its JSON with `--json`
  (`laptop.architect-2`, 2026-10-13T08:24:09Z,
  https://github.com/synnaxlabs/foundation/pull/2528#issuecomment-6141442842, which
  supersedes "#1744 makes it a `Response` variant" of
  https://github.com/synnaxlabs/foundation/pull/1911#issuecomment-6064364728). Each
  error of `ops`, in the CLI and in MCP, has one JSON shape: `{"errors": [...]}`, each
  item a typed `Problem { code, message, fix, place: Option<Place>, notes }`. An error
  that is not a diagnostic, such as `ops.argument`, is a list of one item with no place.
  A place is `{"file", "line", "column"}`, each count from 1, and the key is absent when
  there is no place. `notes` is a list, empty when there is no note. The text of an
  error is each item as `error[<code>]: <message>`, its place, `fix: <fix>`, and each
  note, with an empty line between two items. Lost: a second shape for diagnostics, with
  which a client parses two shapes and must know which operation gives which. Decided by
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
  `Mesh::spec` for each caller and gives the spec in use or the stop (`ops.stopped`). A
  call on that spec, the check that it is the newest, gives this error. This supersedes
  the one function of item 1 of
  https://github.com/synnaxlabs/foundation/pull/2035#issuecomment-6071339913, which also
  gives `ops.behind`. The one reader and its newest check are by `laptop.architect-2`
  (2026-10-12T18:47:07Z, item 2 and the first text fix of
  https://github.com/synnaxlabs/foundation/pull/2466#issuecomment-6130500446) and, for
  `Mesh::spec`, by `laptop.architect` (2026-10-12T19:42:40Z,
  https://github.com/synnaxlabs/foundation/pull/2466#issuecomment-6131381280). A plan
  whose base is not the spec in use, before the proposal or after another change applies
  first, is `ops.stale-plan`, status 1, fix `Plan again`. Each other error of
  `Mesh::apply`, except a stop, is `ops.apply`, status 1, with its `Display` as the
  message and the fix `Fix the cause in the message, then plan and apply again`. A stop
  of the group of the mesh, from `Mesh::spec` or from `Mesh::apply`, is `ops.stopped`,
  status 1, in `plan` and in `apply`, with the `Display` of the stop as the message and
  the fix `Fix the cause in the message, then start the node and plan again`. It can
  come after a proposal, so plan again to see whether the change applied. Lost: a code
  for each variant of the stop, which gives a better hint but adds three codes for one
  cause. The code has only this cause: a node that does not run gives
  `node.not-running` (below), which supersedes `ops.no-node` of
  https://github.com/synnaxlabs/foundation/pull/2192#issuecomment-6090851088.
  Supersedes the code `ops.stopped` of `Unreached::Stopped` in
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6053197935.
  `ops.stopped` decided by `laptop.architect-2` (2026-10-09T03:42:35Z, (b) of
  https://github.com/synnaxlabs/foundation/issues/1756#issuecomment-6073812317); the
  fix, which supersedes the fix of (b), by `laptop.architect` (2026-10-09T22:19:11Z,
  https://github.com/synnaxlabs/foundation/pull/2192#issuecomment-6090194371) and
  `laptop.architect-2` (2026-10-09T22:24:28Z,
  https://github.com/synnaxlabs/foundation/pull/2192#issuecomment-6090256164), one
  code with this fix for each stop by `laptop.architect-2` (2026-10-09T23:12:10Z,
  https://github.com/synnaxlabs/foundation/pull/2192#issuecomment-6090836329), and
  "except a stop" by `laptop.architect-2` (2026-10-09T22:19:29Z,
  https://github.com/synnaxlabs/foundation/pull/2192#issuecomment-6090197852).
  Supersedes the code of `mesh::Error::Stopped` in step 4 of
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6063695586. There
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
  `homes` counts the homes of the plan that the apply gives: each home that the plan
  lists, and through `ops::serve` each home on whose index the subject has `apply`
  (SERVE ACCESS). Its text is `Applied <file>: <a> added, <c> changed, <r> removed, <h>
  homes listed.`, with each count of 0 left out and `1 home listed` for one home, and
  `no change` only for a plan with no change and no home. The text without `homes` and
  the rule that leaves out a count of 0 are step 5 of the plan
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6063695586. `homes`,
  its text, and the `no change` rule decided by `laptop.architect-2`
  (2026-10-09T00:42:24Z,
  https://github.com/synnaxlabs/foundation/pull/2035#issuecomment-6071903259). The whole
  text approved by `laptop.architect-2` (2026-10-09T01:02:46Z,
  https://github.com/synnaxlabs/foundation/pull/2035#issuecomment-6072118033) and, for
  the contract with `mesh`, by `laptop.architect` (2026-10-09T00:57:47Z,
  https://github.com/synnaxlabs/foundation/pull/2035#issuecomment-6072065332).
  The plan output also has `reserved`: each definition of the spec in use that
  Foundation made, as `Reserved { name, kind, key }` with its label, its kind, and its
  tree key, in the order of its tree key. `plan` leaves these out of its changes, since
  no file can hold a reserved label (FIRST ADMIN). When the spec holds one, the text
  ends with one line after the counts: `note: <n> definitions that Foundation made are
  not in the files:` and the tree key of each, with `, ` between two, or `note: 1
  definition that Foundation made is not in the files:` and its tree key. A spec with
  none gives no line and an empty `reserved`. The text and the field are item 4 of plan
  revision 1 of #1744, by `laptop.architect-2` (2026-10-08T06:01:36Z,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6053458318), which
  revision 3 moves to PR 2 (2026-10-10T03:30:54Z,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6093320483). The
  order of the tree key is the ruling of `laptop.architect-2` (2026-10-12T18:27:00Z,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6130170904), which
  supersedes the order of the example in item 4. The field `key` is by
  `laptop.architect-2` (2026-10-12T18:47:07Z,
  https://github.com/synnaxlabs/foundation/pull/2466#issuecomment-6130500446).
  A remote program runs `plan`, `apply`, `status` (STATUS OPERATION), `browse`, and
  `import` (IMPORT OP) on a node through `ops::serve`. The request form is private to
  `ops`: one line of JSON with one operation and its input, a newline, then the bytes of
  a plan file as they are. `plan` sends the path and the text of each file:
  `{"plan":{"files":[{"path":"site.hcl","text":"..."}]}}`, with no byte after the
  newline. `apply` sends `{"apply":{}}` and then only the plan. The command line checks
  the path and fills `Applied::file`, so the node sees no path and gives the apply
  output with no `file`. The reply is `{"output":<the output>}` and a newline, then the
  plan file for `plan`. The reply of an error is `{"errors":[...], "status":<n>}` and a
  newline, with the `errors` of each error of `ops` and the exit status that the error
  has (2 for a `Config` or a `Plan` error), not always 1. Three codes are new, each with
  status 1. A body that is not this form is `ops.bad-request`, with the cause in the
  message, cut to its first 256 bytes at a char boundary and then `...` when it is
  longer, so that each such reply fits (`laptop.architect-2`, 2026-10-18T00:09:33Z,
  https://github.com/synnaxlabs/foundation/issues/3084#issuecomment-6243561471), and the
  fix ``Use a `foundation` program of the same version as the node``. So is each
  operation other than `plan`, `apply`, `status`, `browse`, and `import`, with the cause
  ``the operation `<name>` does not run on a node``. A subject with no grant gets
  `ops.denied` (SERVE ACCESS). An output of more than
  `wire::hub::client::BODY_BYTES_MAX` bytes is `ops.large-reply`, with the bound in the
  message and the fix `Plan and apply the change in smaller steps`, since
  `hub::serve::Reply::send` panics on such a body (`laptop.architect`,
  2026-10-08T11:24:14Z,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6058791545). The
  bound holds for the reply of an error too. An error reply over the bound keeps the
  first problems that fit, in order, then `ops.more-problems`, ``<n> more problems do
  not fit in one reply``, fix `Fix the problems above, then run it again`, with no place
  and no notes, and the status of the error. When not even the first problem fits with
  it, the reply is `ops.large-reply`, or `ops.large-browse` for `browse` (below)
  (`laptop.architect-2`, 2026-10-17T23:33:12Z,
  https://github.com/synnaxlabs/foundation/pull/3079#issuecomment-6243126104),
  `ops.large-status` for `status`, or `ops.large-import` for `import` (IMPORT OP).
  `ops.large-status`, status 1, takes the place of `ops.large-reply` for a reply of a
  status, its output, or an error whose first problem does not fit: ``the reply of the
  status has more than <n> bytes, the most that a reply holds``, fix `Read the status
  channels of each connector with a client of the node`, since a `status` request cannot
  ask for less (`laptop.architect-2`, 2026-10-18T00:09:33Z,
  https://github.com/synnaxlabs/foundation/issues/3084#issuecomment-6243561471;
  `laptop.architect-2`, 2026-10-18T00:32:33Z,
  https://github.com/synnaxlabs/foundation/pull/3096#issuecomment-6243760762). `serve`
  stops writing a reply at the bound. The places of the problems of one file share its
  path (`laptop.architect`, 2026-10-12T20:11:00Z,
  https://github.com/synnaxlabs/foundation/pull/2468#issuecomment-6131805444;
  `laptop.architect-2`, 2026-10-12T20:14:30Z,
  https://github.com/synnaxlabs/foundation/pull/2468#issuecomment-6131856346). `serve`
  reads the spec in use as the apply does, so a node that is behind gives `ops.behind`
  for `plan` too (later step of
  https://github.com/synnaxlabs/foundation/pull/2035#issuecomment-6071339913, in
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6071715327). A node
  that uses no spec gives `ops.denied` to each remote subject instead (SERVE ACCESS).
  The form, the status of an error, `ops.denied`, and `ops.large-reply` are decisions 5
  to 7 of plan revision 4, by `laptop.architect-2` (2026-10-11T07:10:55Z,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6106546913),
  approved for the contracts by `laptop.architect` (2026-10-11T08:10:05Z,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6106994274). The
  status of `ops.bad-request`, its fix, and its use for each operation other than `plan`
  and `apply` are by `laptop.architect-2` (2026-10-12T18:30:05Z,
  https://github.com/synnaxlabs/foundation/pull/2468#issuecomment-6130219053), which
  supersedes the `ops.unknown-operation` clause of the `serve` doc of that plan. STATUS
  OPERATION adds `status` to `plan` and `apply` there. Lost: one JSON value with the
  plan file as a list of numbers, which is four times the bytes.
  `plan`, `apply`, `status` (STATUS OPERATION), `browse`, and `import` (IMPORT OP) run
  on the node of `foundation-data`, which `node::client::reach` reaches. The command
  line gives five codes of its own. `ops.read`: `no file is at <path>`, `<path> is a
  directory, not a file`, `<path> holds more entries than one read lists`, or `the text
  of <path> is not UTF-8`, status 2, and `the read of <path> failed with OS error
  <code>`, status 1, each with the fix `Fix the cause in the message, then run the
  command again`. `ops.large-request`: `the request has more than <n> bytes, the most
  that a request holds`, for config files whose JSON together passes that bound at their
  read, where each file counts first by its bytes, before the check of its text, or for
  the encoded request (`laptop.architect-2`, 2026-10-18T20:17:02Z,
  https://github.com/synnaxlabs/foundation/pull/3241#issuecomment-6254647508), status
  2, fix `Make the config files smaller`; the check comes before the reach. `ops.write`:
  `the directory of the plan file <path> is not there` or `the write of the plan file
  <path> failed with OS error <code>`, status 1, fix ``Give `--out`, or `out` of the MCP
  tool, a path in a directory that can be written``. `ops.no-config`: `<path> holds no
  config file`, status 2, fix ``Add a file whose name ends in `.hcl` and does not start
  with `.` to <path>, or give another path``, which names each extension of the table,
  for a directory of `plan`, `browse`, or `import` that gives no config file (FRONT
  ENDS; `laptop.architect-2`, 2026-10-18T18:23:36Z,
  https://github.com/synnaxlabs/foundation/issues/2526#issuecomment-6253485402, and for
  `browse` and `import`, 2026-10-18T18:34:45Z,
  https://github.com/synnaxlabs/foundation/issues/2526#issuecomment-6253598063).
  `ops.bad-reply`: `the reply of the node is no reply: <why>`, status 1, fix ``Use a
  `foundation` program of the same version as the node``. A failure to reach the node,
  or of its request, has status 1 and one of the codes `node.not-running`,
  `node.unreadable`, `node.admin-key`, `node.version`, `node.socket`, `node.unanswered`,
  `node.session`, `node.refused`, and `node.no-reply`, with the message and fix of
  decision 3 of plan revision 4 (`laptop.architect-2`, 2026-10-11T07:10:55Z,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6106546913). The
  fix of `node.not-running` is ``Start one with `foundation start --name <name>` ``,
  with no `--data`, by decision 7 of plan revision 5
  (https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6136638641),
  approved by `laptop.architect-2` (2026-10-13T02:10:54Z,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6136677419).
  `node.session` is by `laptop.architect-2` (2026-10-13T01:44:19Z,
  https://github.com/synnaxlabs/foundation/pull/2517#issuecomment-6136254936).
  `node.version`: `<dir>/data/<file> holds format version <found>, and this
  foundation reads version <expected>`, with each version byte as its ASCII char, a
  version of the set of SECTOR VERSIONS, fix ``Use a `foundation` program of the
  same version as the node``, when `address` or `admin.key` holds a format of another
  version, which `reach` gives as
  `node::client::Error::Local(sector::local::Error::Version { .. })`
  (`laptop.architect-2`: item 7 of
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6252508020 at
  2026-10-18T16:49:11Z;
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6252530569 at
  2026-10-18T16:51:27Z; the code and its fields in
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6252630274 at
  2026-10-18T17:01:02Z; the fix text, that of `ops.bad-reply`, in
  https://github.com/synnaxlabs/foundation/pull/3231#issuecomment-6252780856 at
  2026-10-18T17:15:38Z; the wrapped variant in
  https://github.com/synnaxlabs/foundation/pull/3231#issuecomment-6252975559 at
  2026-10-18T17:34:06Z; the names `found` and `expected`, over `held` and `read` of
  6252630274, in item 1 of
  https://github.com/synnaxlabs/foundation/pull/3231#issuecomment-6253217340 at
  2026-10-18T17:57:52Z; the byte as its char, over the escaped byte of 6252630274,
  in item B of
  https://github.com/synnaxlabs/foundation/pull/3231#issuecomment-6253248771 at
  2026-10-18T18:00:53Z).
  `node.socket` covers a socket of the program that broke, with the message `the
  socket of this program failed: <the transport error>` (`laptop.architect-2`,
  2026-10-13T02:10:54Z, item 4 of
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6136677419). The
  transport error in `node.unanswered` is by `laptop.architect-2` (2026-10-13T01:44:19Z,
  https://github.com/synnaxlabs/foundation/pull/2517#issuecomment-6136254936).
  Supersedes, in decision 3 of
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6106546913, the fix
  of `node.not-running`, the message of `node.socket`, the hub error in
  `node.unanswered`, and the example `read of admin.key` of `node.unreadable`, which the
  error of `env::files` gives as `read_at of admin.key failed with OS error 13`
  (`laptop.architect-2`, 2026-10-13T04:17:48Z, item 2 of
  https://github.com/synnaxlabs/foundation/pull/2533#issuecomment-6138124615). The
  four `ops` codes are decisions 2, 4, 5, and 8 of plan revision 5,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6136638641, with
  `ops.large-request` by item 2 of the approval of `laptop.architect-2`
  (2026-10-13T02:10:54Z,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6136677419), which
  supersedes the bound on each file of that plan. `laptop.architect-2` approved the
  messages, statuses, and fixes of the four `ops` codes (2026-10-13T03:48:32Z, item 4 of
  https://github.com/synnaxlabs/foundation/pull/2533#issuecomment-6137791971).
  `plan`, `apply`, `status`, `browse`, and `import` (IMPORT OP) take `--data <dir>`, the
  data directory of the node, by default `foundation-data`, as `start` does (#2526).
  This supersedes "run on the node of `foundation-data`" above and in the doc of `cli`
  of https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6136638641. In
  each command of a fix below, `{dir}` follows `--data=` as one word of a POSIX shell:
  in single quotes, with each `'` as `'"'"'`, when it is empty or holds a character
  other than `[A-Za-z0-9_./:@%+,-]`. A `{dir}` that is not UTF-8, or that the escape of
  a fix text changes (a `\`, a control character, or another character that
  `char::escape_debug` escapes), is named in no command: the fix says to set `--data` to
  that directory. In a message, `{dir}` is the path as it is. The fix of
  `node.not-running` is ``Run `foundation start --data={dir}`, with `--name <name>` on
  its first start, or give `--data` the data directory of a node that runs``, which
  supersedes "with no `--data`" above, decision 7 of
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6136638641. A
  `{dir}` that is a file, or whose `{dir}/data` is a file, is also `node.not-running`,
  with the message `no node runs in {dir}, which is not a data directory` and the fix
  ``Give `--data` the data directory of a node that runs``, which supersedes
  `node.unreadable` for it, "a directory that does not open" of decision 3 of
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6106546913. The fix
  of `node.unanswered` is ``Start the node again with `foundation start --data={dir}`
  ``, which supersedes its fix of that decision 3. The fix of `node.session` is `Fix the
  cause in the message, then run the command again`, which supersedes the fix of
  `node.session` of item 1 of
  https://github.com/synnaxlabs/foundation/pull/2517#issuecomment-6136254936. The log
  of the node (LOG OUTPUT) does not yet hold why a session failed, so this fix points
  to it only when the node writes that event (#2767), by `laptop.architect-2`
  (2026-10-15T22:05:27Z,
  https://github.com/synnaxlabs/foundation/pull/2762#issuecomment-6203245390). The fix
  of `ops.unknown-operation` with no close name names the list of its carrier: ``Use a
  name from `foundation --help` `` on the command line, and ``Use a name from
  `tools/list` `` in MCP, because `foundation docs` lists only the operations of
  `TABLE`. By `laptop.architect-2`
  (2026-10-13T21:30:55Z,
  https://github.com/synnaxlabs/foundation/issues/2526#issuecomment-6155625024; the fix
  of `ops.unknown-operation` at 2026-10-13T21:47:43Z,
  https://github.com/synnaxlabs/foundation/pull/2614#issuecomment-6155892894; the texts
  that name `{dir}` at 2026-10-13T21:52:28Z,
  https://github.com/synnaxlabs/foundation/pull/2614#issuecomment-6155978119; the
  quotes of `{dir}` and the text of a `{dir}` that is no data directory at
  2026-10-13T22:10:06Z,
  https://github.com/synnaxlabs/foundation/pull/2614#issuecomment-6156250837;
  `--data={dir}` at 2026-10-13T22:25:40Z,
  https://github.com/synnaxlabs/foundation/pull/2614#issuecomment-6156527941; and the
  quotes of `'`, the safe set, and the fixes that name no path at 2026-10-13T22:31:15Z,
  https://github.com/synnaxlabs/foundation/pull/2614#issuecomment-6156609096).
  `version`, `foundation --version`, and `-V` give one output: the text
  `<version> (<commit>)`, with the first 12 hex digits of the commit or `unknown`, and
  the JSON `{"commit": <40 or 64 hex digits, or null>, "version": <version>}`, so a
  program tests `null`, not a word. The commit is that of `HEAD` when the `build.rs` of
  `ops` runs, also in a tree with changes, with no mark of them, and is none with no
  `git`, outside a git repository, and on a branch with no commit. Only the script sets
  it, so a variable of the builder does not reach `ops`, and it stops the build with an
  error at a commit that is not 40 or 64 lowercase hex digits and at a ref of a branch
  with a commit that git cannot read. The script watches the paths that
  `git rev-parse --git-path` gives for `HEAD` and its ref, and, only while that ref has
  no file of its own, the nearest folder above the ref, where git writes that file at
  the next change of the ref. In a reftable repository, it watches in their place the
  `reftable` folder of the worktree, which holds its `HEAD`, and that of the repository,
  which holds each branch. So a commit, a checkout, and a branch switch in a worktree
  rebuild `ops`, and a build with no change does not. MCP `serverInfo` keeps the version
  alone (`laptop.architect-2`, 2026-10-13T21:55:47Z,
  https://github.com/synnaxlabs/foundation/issues/2616#issuecomment-6156028343; the
  check of the commit: `laptop.architect-2`, 2026-10-13T22:33:11Z,
  https://github.com/synnaxlabs/foundation/pull/2620#issuecomment-6156635866; the
  reftable folder, the empty commit, and the cases with no commit: `laptop.architect-2`,
  2026-10-13T22:39:49Z,
  https://github.com/synnaxlabs/foundation/pull/2620#issuecomment-6156732029; the commit
  of 64 digits: `laptop.architect-2`, 2026-10-13T23:24:59Z,
  https://github.com/synnaxlabs/foundation/pull/2620#issuecomment-6157329508; no watch
  of `packed-refs`: `laptop.architect-2`, 2026-10-13T23:38:27Z,
  https://github.com/synnaxlabs/foundation/pull/2620#issuecomment-6157497960). Lost:
  `vergen`, a new dependency for about 30 lines; a variable that the release sets, as
  each local build must say what it is; and a watch of `packed-refs` (git writes the
  file of a ref at each change of it).
  Amended (2026-10-16T04:51:22Z, #2526): the tools of `foundation mcp` read and write
  files only under their folders, each `--files <DIR>` (repeatable), else the working
  directory, and never in the data directory. Each path, also each link in it, is
  resolved before the check (`Paths::within` of ENV SEAMS). With no `--files`, a start
  whose working directory resolves to the root or the home directory fails with
  `ops.wide-folder` (2). A `--files` that is not there, that is a file, or that fails to
  resolve is `ops.folder` (2). After these checks and before a tool serves,
  `foundation mcp` makes its data directory with `Paths::make_dir`, and a failure is
  `ops.data` (1), whose fix at `NotFound` names the directory above it. So a start that
  these checks refuse makes nothing (the order: `laptop.architect-2`,
  2026-10-16T20:44:36Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6224350407; the
  make kept: `laptop.architect-2`, 2026-10-16T20:27:06Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6224071502).
  The `out` of `plan` through MCP must resolve
  to a name that ends in `.plan`, else `ops.plan-name` (2). A path outside the folders
  is `ops.outside` (2): "`{path}` is not under a folder of this MCP server ({folders})",
  with the fix "Give a path under one of those folders. A relative path starts at the
  working directory of this MCP server, which was {working} at its start. To reach
  another folder, add `--files <DIR>` to the command of the MCP server".
  The fix stays true after a program moves that directory. A path in the data
  directory is `ops.hidden` (2):
  "`{path}` is in the data directory {data} of this MCP server, which no tool reads or
  writes", with the fix "Give a path outside {data}".
  A new name that a write or `make_dir` makes in a directory on the way to the data
  directory, while nothing is at it, is `ops.no-data` (1): "`{path}` is a new name in a
  directory on the way to the data directory {data} of this MCP server, and nothing is
  at {data} now", with the fix "Start the node on {data} again, or give a path in
  another directory".
  A path under a folder that a program moved or replaced after the start, or into the
  directory that the folder was at the start, is `ops.folder-changed` (1): "`{path}`
  is under a folder of this MCP server ({folders}) that a program moved or replaced
  after it started", with the fix "Start `foundation mcp` again" (both:
  `laptop.architect-2`, 2026-10-17T05:16:48Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6230063256; the
  moved folder and the fix of `ops.outside`, 2026-10-17T07:17:06Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6231111995).
  `ops.plan-name`, `ops.outside`, `ops.hidden`, `ops.no-data`, and
  `ops.folder-changed` are tool errors (`isError`); `ops.wide-folder`, `ops.folder`,
  and `ops.data` end the start.
  The command line keeps free paths, since a person types them. Decided by
  `laptop.monitor` for the person, who delegated it at 2026-10-16T04:51:01Z with "I'll
  let you choose the right path" (2026-10-16T04:51:22Z,
  https://github.com/synnaxlabs/foundation/issues/2526#issuecomment-6208345441; the
  details by `laptop.architect-2`, 2026-10-16T04:52:58Z,
  https://github.com/synnaxlabs/foundation/issues/2526#issuecomment-6208362869; the
  plan, approved by `laptop.architect-2` with conditions 1 to 5, 2026-10-16T08:08:47Z,
  https://github.com/synnaxlabs/foundation/issues/2526#issuecomment-6211047580;
  `ops.hidden`, 2026-10-16T08:13:29Z,
  https://github.com/synnaxlabs/foundation/issues/2526#issuecomment-6211121615). Lost:
  free paths for the tools, with which a model that text steers quotes or replaces any
  file of the user; MCP roots, which the 2026-07-28 revision of the specification
  deprecates; and `ops.outside` with no folders, which shows the agent no way out.
  Amended (2026-10-16T04:45:05Z, #2526): one MCP server talks to one node, picked at
  start with `foundation mcp --data <dir>`, and a tool call cannot pick another. A
  failed `plan`, `apply`, `status`, `browse`, or `import` (IMPORT OP) is a tool result
  with `isError` and the text `{"errors": [...]}`, while an unknown tool and a malformed
  request or argument stay a JSON-RPC error. Each operation of `foundation --help` but
  `start`, `profile`, and `mcp` is a tool, with the output schema of its `--json`.
  `version`, `docs`, `status`, and `browse` have `readOnlyHint`; `plan`, `apply`, and
  `import` (IMPORT OP) have `destructiveHint`, since the `out` of `plan` and the write
  of `import` (IMPORT OP) replace a file that is there, so a client that asks the person
  before a destructive tool asks for each (`laptop.architect-2`, 2026-10-18T07:39:38Z:
  https://github.com/synnaxlabs/foundation/pull/3154#issuecomment-6247231298). It
  supersedes the OPS OUTPUT text of
  https://github.com/synnaxlabs/foundation/pull/3154#issuecomment-6247188650.
  `foundation mcp` reaches its node at the first tool call that needs it, keeps that
  session, and reaches again only at the call after a failure of a reach or a request.
  `node` reads standard input on an I/O thread of `os` and gives `ops::cli` an async
  source of lines, so the shard that serves runs while it waits for a line, and a kept
  session sends its keep-alive. Decided by the person, "Yeah that's fine"
  (2026-10-16T04:45:05Z, relayed by `laptop.monitor`,
  https://github.com/synnaxlabs/foundation/issues/2526#issuecomment-6208278565;
  the tools and their hints by `laptop.architect-2`, 2026-10-16T04:46:50Z,
  https://github.com/synnaxlabs/foundation/issues/2526#issuecomment-6208295673; the
  kept session and the read of input on a thread, `laptop.architect-2`,
  2026-10-16T21:26:41Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6225017086). Lost: one
  MCP server for many nodes, which lets text that steers the model pick the node; and a
  reach at each call while the shard blocks on input, since a dropped session closes
  only when the shard runs, so an idle server held 23 UDP ports and 3 GB after 200
  calls.
  Until #2939 (https://github.com/synnaxlabs/foundation/issues/2939) merges, the first
  call after a stop or a restart of the node can give `node.no-reply` with nothing sent
  to the node (the deferral, `laptop.architect-2`, 2026-10-16T22:15:00Z,
  https://github.com/synnaxlabs/foundation/pull/2919#issuecomment-6225726721).
  `browse` (BROWSE) is `foundation browse <connector> <file or directory>... [--start
  <path>] [--match <pattern> | --cursor <cursor>] [--values]`, and the MCP tool `browse`
  takes `connector`, `files`, `start`, `match`, `cursor`, and `values`. The request is
  `{"browse":{...}}` with these fields, and `files` as `plan` sends them. The node
  checks, in this order, and stops at the first failure: the files read; the connector
  is a name (`ops.argument`); `config::connector` finds its block; the subject has
  `apply`, then `admin`, on the name (`ops.denied`, SERVE ACCESS; `laptop.architect-2`,
  2026-10-17T23:06:39Z,
  https://github.com/synnaxlabs/foundation/pull/3079#issuecomment-6242836290), the
  access with which a remote `plan` reads the sources (SECRET SOURCES); the `node` of
  the block is this node; the start is a path; the match is a pattern; and `match` and
  `cursor` are not both given (`ops.argument`, ``a browse takes `match` or `cursor`, not
  both``; the command line refuses it first, with status 2). Then it runs
  `Supervisor::browse`. Five codes are new. `ops.browse-node`, status 2: ``the connector
  `<c>` runs on the node `<n>`, not on this node `<this>` ``, fix ``Run it with `--data`
  of node `<n>` ``. `ops.bad-start` and `ops.bad-pattern`, status 2: ``the start
  `<text>` is not a path: <cause>`` and ``the match `<text>` is not a pattern:
  <cause>``, with the cause of `connector::browse::Malformed`, and the fixes of
  `codes.golden`. `ops.large-browse`, status 1, takes the place of `ops.large-reply` for
  a reply of a browse, its output, or an error whose first problem does not fit: ``the
  reply of the browse has more than <n> bytes, the most that a reply holds``, fix
  ``Narrow the start or the match, or browse without `values` ``, since a smaller plan
  does not make it fit. Each problem of an error of `Supervisor::browse` has status 1
  and starts with ``the browse of the connector `<c>` of kind `<k>` failed: ``. A
  diagnostic of `Config` keeps its code, fix, place, and notes. `Device` and `Retry`
  give `ops.browse`, fix `Fix the cause in the message, then browse again`. The output
  is `{"entries":[...],"end":<end>}`. An entry has `path`, `tag`, and `shape`: `folder`,
  `point`, or `unreadable` with `reason`. A point also has `type` (as a `type` attribute
  takes it, such as `f64`), `unit`, `unit_code`, and `access` (`read`, `write`, or
  `read-write`), each `null` when the device cannot know it. Only with `values` a point
  has `value`, `null` when the device gave none or gave a value that is not one scalar,
  such as that of a `string` or an array point: a bool and each integer of 32 bits or
  fewer are a JSON bool and number, an `i64` or `u64` its decimal text such as
  `"9007199254740993"`, a finite float a number, a float that is not finite `"NaN"`,
  `"Infinity"`, or `"-Infinity"`, a stamp RFC 3339 text in UTC with nine fraction
  digits, a span its text such as `"250us"`, and a UUID its hyphenated text. The text
  form uses the same texts. `end` is `"done"`, `{"next":"<cursor>"}`, `"limit"`,
  `"budget"`, or `"deadline"`. The text gives one line for each entry: `<path>/` for a
  folder, `<path> <type> <unit> <access>` and then the value with `values` for a point,
  and `<path> unreadable: <reason>`. A fact that the device cannot know is `?`, so each
  line keeps its columns, and the unit is its text, else its code. Then one end line:
  `more: --cursor <cursor>`, `More than 500 points match, the most for one browse;
  narrow the pattern or the start`, or `The browse stopped at its budget, so more may
  match; narrow the pattern or the start`, or `The browse stopped after 1m, so more may
  match; browse a deeper start`, where `1m` is the span text of `DEADLINE`
  (`laptop.architect-2`, 2026-10-19T02:24:41Z, item 1:
  https://github.com/synnaxlabs/foundation/issues/3125#issuecomment-6258321788). `done`
  adds no line, and a browse with no entries gives `The browse found no entries.` The
  text of `budget` names no count, since `DEPTH` and a later budget of compared bytes
  also end a walk with it. The plan is by `laptop.integrator-2` (2026-10-17T05:11:19Z,
  https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6230008460),
  approved by `laptop.architect-2` (2026-10-17T05:14:44Z,
  https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6230042870), whose
  items 2 and 3 replace the one `unit` field and the budget text of the plan, and whose
  last item gives `?`. The type is the text of `types::sample::Type`, which a `type`
  attribute takes, not the `float64` of the plan, so a browse output goes into a config
  as it is (kind (f), sent to `laptop.monitor` with `ops.large-browse` at
  2026-10-17T22:57:51Z,
  https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6242684265; it goes
  ahead unless the person objects). Lost: one `unit` field, which drops the code when
  the device gives a text, so an agent cannot tell which it got. `laptop.monitor`
  accepted kinds (e) and (f) with two conditions on (e), which follow the proto3 JSON
  mapping: an `i64` or `u64` is text, since a JSON number past 2^53 loses digits in
  JavaScript and in many JSON tools with no error, and a float that is not finite is
  `"NaN"`, `"Infinity"`, or `"-Infinity"`, not `"inf"` or `"-inf"`, since JavaScript
  `Number("inf")` gives NaN (as `laptop.architect-2` records, 2026-10-17T22:24:28Z,
  https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6242201049). One
  function of `ops` maps a value to JSON, so each later output with values takes this
  rule.
