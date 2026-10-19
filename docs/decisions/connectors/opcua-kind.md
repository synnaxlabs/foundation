- **OPCUA KIND (#435)** Five choices of the `opcua` kind, each approved by the person
  as `box2.builder-6` recommended (2026-10-13T20:14:06Z, "All 5 decisions are approved
  for OPC UA":
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6154297722).
  Security: the kind connects with the mode None and an anonymous user now, so it is
  only for an isolated network or a test. SignAndEncrypt is the next part of #435,
  before a release, with user names, passwords, and certificates (question:
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6150341828). Tests:
  the tests of the kind and exit test 1 run against the open62541 test server on the
  simulated clock and network. One more test on real I/O against `opc-plc` runs in CI
  with #1733, before a release (question:
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6150342217). Types:
  the kind reads Double first, then Boolean and each numeric scalar type, each into
  the sample type of the same width and sign (Float to `f32`, Int32 to `i32`, Boolean
  to `bool`), with no conversion. A channel of another type is a config error. The
  kind reads each of these types since
  https://github.com/synnaxlabs/foundation/pull/2635. The `influx` kind writes the same
  types (#2555). String, DateTime, and arrays come when a user needs them (question:
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6150342865). A
  lossless widening (an `i16` value into an `f64` channel) comes when a user's node
  types do not match the channel types that the user wants (`laptop.architect-2`,
  2026-10-14T00:36:21Z:
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6158211076). Time:
  each frame has the midpoint of the request on mesh time (STAMP). The SourceTimestamp
  comes with the device clock and its fit to mesh time, when a user needs the time of
  the measurement (question:
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6150343218). The
  device clock is the server clock. The client reads `CurrentTime` as a `Stamp`, and a
  `DateTime` that a `Stamp` cannot hold, the null one too, gives `BadOutOfRange`, and
  the plan of PR 5 of #2622 states what the kind does with that status. PR 5 maps each
  server time, `CurrentTime` and each timestamp of a value, to the `Monotonic` of the
  fit with one crate-private function, as ns from `i64::MIN` ns past the Unix epoch, so
  the map drops no `Stamp` (`laptop.architect-2`, 2026-10-19T13:20:40Z:
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6268217457). Lost:
  ns from the Unix epoch, with a drop and a warning before 1970, and a `Monotonic` from
  1601, which loses each time after mid 2185. Status: a Good with a subcode counts as
  Good. A frame leaves out each value that is not Good and keeps the others: an
  Uncertain value adds 1 to the count `uncertain`, and each other one, also no value
  (`BadNoData`) and a `BadTypeMismatch` of the server, adds 1 to `bad`. A tick that
  leaves out each value writes no frame. A quality channel comes with the quality model
  of the MVP (question:
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6150343529). Approved
  by the person (2026-10-15T16:34:12Z:
  https://github.com/synnaxlabs/foundation/issues/1737#issuecomment-6197131223). A Good
  value of another type than the sample type of its channel is also left out, and adds 1
  to `mistyped`: the user fixes it in the spec or on the server, so it is not in `bad`.
  The note names the first such node of the latest tick that has one, and the OPC UA
  type of its value, such as String, Range, or array of Double. A tick with none clears
  it (`laptop.architect-2`, 2026-10-14T02:31:51Z:
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6159574954, the type
  of an array or a String at 2026-10-15T19:32:35Z:
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6200602125, and the
  `BadTypeMismatch` of the server, the note of the latest tick, and the name of the type
  at 2026-10-15T22:16:48Z:
  https://github.com/synnaxlabs/foundation/pull/2766#issuecomment-6203414079). A connect
  calls `Context::clear_error`: until then, the status shows the error of the last run
  (scenario 3 of CONNECTOR CONFORMANCE found it, 2026-10-17T21:08:01Z:
  https://github.com/synnaxlabs/foundation/issues/2641#issuecomment-6241104680; text of
  `laptop.architect-2`, 2026-10-17T21:10:51Z:
  https://github.com/synnaxlabs/foundation/issues/2641#issuecomment-6241157217). Close:
  after a cancel or an error, the run closes its session. It sends CloseSession, waits
  at most 1 s for the answer, then closes the connection, so a run ends within 1 s of a
  cancel. The connect races the cancel, and the close does not. The 1 s also holds the
  wait for a CreateSession in flight, so a server that answers it later can keep that
  session until it times out (`laptop.architect-2`, 2026-10-16T02:08:17Z:
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6206438915, and its
  amendment at 2026-10-16T02:44:43Z:
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6206868666).
  Keep-alive: between ticks, the run waits through `Client::idle`, which drives the
  client. When 10 s pass with no read, `idle` reads `Server_ServerStatus_State`
  (ns=0;i=2259) to check the connection, so a run that reads more often than each 10 s
  sends no check. A check fails when its answer has a bad service result, when it gets
  no answer in the request timeout (5 s), or when the close of the channel cuts it. The
  status and the value of the server state are not judged: each answer came over the
  channel and the session (`laptop.architect-2`, 2026-10-16T05:10:17Z,
  https://github.com/synnaxlabs/foundation/pull/2809#issuecomment-6208565249). The first
  failed check ends the run with `Error::Device` and the text "the connectivity check of
  <address> failed with <status>", where <address> is the `address` of the connector, so
  the supervisor connects again with its backoff. A channel or a session that the client
  loses between two checks ends the run at once with the same text, from the state of
  the client: the status of the connect, else `BadConnectionClosed` or
  `BadSessionClosed`. The check is in Rust: open62541 calls its inactivity callback only
  for `BadTimeout` and drops each other status of its own check, such as the
  `BadSessionIdInvalid` of a lost session, so `connectivityCheckInterval` stays 0. The
  retry of a failed read stays. Approved by the person (2026-10-16T03:44:54Z, "Yes,
  approved", on "Keep the 10-second keep-alive, and add one rule: if a keep-alive fails,
  reconnect right away instead of waiting for the next read":
  https://github.com/synnaxlabs/foundation/pull/2809#issuecomment-6207602232). The count
  of one failed check, what fails a check, the check in Rust, the text, and the tests:
  decided by `laptop.architect-2` (2026-10-16T03:46:32Z,
  https://github.com/synnaxlabs/foundation/pull/2809#issuecomment-6207619278). The end
  at once on a lost channel or session: decided by `laptop.architect-2`
  (2026-10-16T04:04:38Z,
  https://github.com/synnaxlabs/foundation/pull/2809#issuecomment-6207818990). Import:
  the block of a point is `read "<channel>" { node_id = "<tag>" }`, which `parse` reads.
  A tag that is not a node ID that `parse` reads gives `opcua.bad-node-id`, with the
  tag. The browse of the kind gives no `Write`, so `block` reads no fact and each point
  gets a read block. A match on the kind of a built-in type, Boolean (1) to Double (11),
  decodes a read, and the sample type of each kind comes from that decode at build time.
  The type of a browse comes from `SCALARS`, so the two cannot disagree. The plan:
  `laptop.architect-2`, 2026-10-16T21:14:25Z
  (https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6224815609). No
  `opcua.no-write`, in place of the plan's: `laptop.architect-2`, 2026-10-16T21:59:16Z
  (https://github.com/synnaxlabs/foundation/pull/2931#issuecomment-6225496840). The
  decode by match, with no table of the built-in types: `laptop.architect-2`,
  2026-10-16T22:17:22Z
  (https://github.com/synnaxlabs/foundation/pull/2931#issuecomment-6225757093). Browse
  (`laptop.architect-2`, 2026-10-16T21:14:25Z:
  https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6224815609): the
  kind browses from the Objects folder (`i=85`) on one session, which it closes at the
  end, and gives `Error::Device` when the connect, a Browse, a `BrowseNext`, or a Read
  fails. The header of each request holds the mesh time of the node, which the kind
  reads with `browse::Context::time` before the connect. An entry is one forward
  `HierarchicalReferences` reference: an Object or a View is a folder, and a Variable is
  a point with its type, unit, and `access`, or is unreadable with a reason: the server
  does not let the user read it, its value rank is that of an array, or its type is
  another built-in type. A node that `Node` cannot hold (a GUID or opaque key, a name
  that is empty or not UTF-8, or another server) is unreadable, so `import` refuses it
  by shape. Its tag is the text of its ID, with `%` and each byte that is not UTF-8 as
  `%XX`, and `s%=` in place of the `s=` of a name, so no node ID that the kind reads has
  it (key choice e, the plan, 2026-10-16T21:13:48Z:
  https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6224806107; key
  choice f, sent to `laptop.monitor` at 2026-10-17T03:37Z for round 2 finding 7:
  https://github.com/synnaxlabs/foundation/pull/2966#issuecomment-6229156018). It uses
  the `%XX` of URI percent-encoding only for `%` and for bytes that are not UTF-8. The
  format of each tag is stable, because a config keeps it. In the pages of one folder, a
  second reference to a node gives no entry, a name that an earlier entry has gets `
  (<tag>)` after it, and an empty name is the tag. So the plain name goes to the first
  reference in the order of the server. The text of a cursor is the count of references
  of the folder before its page, and a cursor that is not a count gives
  `opcua.bad-cursor`. The browse keeps the continuation point of each cursor that it
  gives. A page of the server with no references gives a page with no entries and a
  cursor, so the budget of `browse::walk` counts it. A browse for a cursor of an earlier
  browse reads past at most `VISITS` pages of the server with no references, so that the
  cursor of a page with no entries goes on in a new browse. Past them, it gives
  `Error::Device`. So a new browse of a cursor fails there, where a walk with a held
  point passes, and #2985 removes this bound (key choice 8, `laptop.architect-2`,
  2026-10-17T04:36:43Z,
  https://github.com/synnaxlabs/foundation/pull/2966#issuecomment-6229704794, accepted
  by `laptop.monitor`). A reference to a node of another class counts in the text of a
  cursor and gives no entry. A cursor with no point, or whose point the server freed
  (`BadContinuationPointInvalid`), browses the folder again from its start and skips
  that count when the count is at most 100 000. While it skips, it reads past at most
  `VISITS` pages with no references, and then gives a page with no entries and the same
  cursor. A cursor that the same browse gave then reads past the pages with no
  references that it already gave, so it gives the page that a held cursor gives. Past
  100 000, a freed point gives `Error::Device` with `BadContinuationPointInvalid`, and a
  cursor with no point gives `opcua.bad-cursor`. A skip assumes that the server keeps
  the order of the references of the folder: when the server changes that order, a skip
  can drop or repeat a reference, and a name with ` (<tag>)` after it can move to
  another node. A browse keeps the tag and the name of each node before the page of each
  cursor, at most 16 MiB of them across its cursors (round 2 findings 1 and 2,
  2026-10-17T03:40:23Z, for this bound and for `VISITS`:
  https://github.com/synnaxlabs/foundation/pull/2966#issuecomment-6229156018, approved
  by `laptop.architect-2`, 2026-10-17T04:05:28Z:
  https://github.com/synnaxlabs/foundation/pull/2966#issuecomment-6229428004). Past it,
  the browse releases its points one at a time, the one that it used least lately first,
  of those at a count of at most 100 000, and gives `Error::Device` when no point is
  left to release (round 2 finding 1 of #3102, 2026-10-18T03:25:25Z:
  https://github.com/synnaxlabs/foundation/pull/3102#issuecomment-6245168498, approved
  by `laptop.architect-2`, 2026-10-18T03:34:04Z:
  https://github.com/synnaxlabs/foundation/pull/3102#issuecomment-6245234798).
  Supersedes the `Error::Device` at the bound of
  https://github.com/synnaxlabs/foundation/pull/2966#issuecomment-6229428004. BROWSE
  states the bound of browses at once. The browse holds at most as many points at a
  count of at most 100 000 as the server holds for a session
  (`MaxBrowseContinuationPoints`, or `DEPTH` when the server gives 0, no value, or a
  value that is not a `UInt16`). Before a Browse at that count, it releases the point
  that it used least lately of those at a count of at most 100 000, so a walk holds one
  point for each open folder down to the bound, and a cursor whose point it released
  browses again from the start of its folder and gives each page of the server with no
  references, as a held cursor does. When a Browse gives `BadNoContinuationPoints`, it
  releases each point at a count of at most 100 000 and browses once again, and a second
  one fails with `Error::Device` and that status. A point past that count stays, as its
  cursor cannot browse its folder again. A page that gives a cursor that the browse
  holds, as a folder that a walk opens twice does, releases the earlier point. The
  points of the type walk of `Client::describe` are not in the count: at the bound, a
  data type with more than one supertype can get no point on a server that keeps its
  bound, and its variable then has no type (`laptop.architect-2`, 2026-10-17T03:23:07Z:
  https://github.com/synnaxlabs/foundation/pull/2974#issuecomment-6228994877). A node
  whose Read alone does not fit one message of the server gives no value. A call that
  the walk drops at its deadline keeps its request in flight until its answer or the
  close of the connection (`laptop.architect-2`, 2026-10-19T03:10:54Z, item 1:
  https://github.com/synnaxlabs/foundation/pull/3333#issuecomment-6258813810). The point
  of that answer lives until the session of its browse closes, and each browse opens its
  own session, so a dropped call holds no point of a later browse (`laptop.monitor`,
  2026-10-19T03:11:18Z:
  https://github.com/synnaxlabs/foundation/pull/3333#issuecomment-6258818536, and
  `laptop.architect-2`, 2026-10-19T03:13:42Z:
  https://github.com/synnaxlabs/foundation/pull/3333#issuecomment-6258845870). No read:
  a connector has zero or more `read` blocks, so a user with only an address can plan,
  apply, browse, and import. `opcua.no-read` is gone. A run with no `read` block
  connects, calls `Context::clear_error`, and sets the note "the connector has no `read`
  block, so it reads nothing. `browse` lists the points of the server, and `import` adds
  a `read` block for each." Then it keeps the session with `Client::idle` until the
  cancel, and closes it as a run with reads does. It opens no writer, as
  `poll::Writer::open` refuses no channel, and writes no frame. A wrong address shows
  the `device` error of the connect. The end of the run clears the note, so the run
  after an import shows none. Lost: a refusal with a fix text that names `browse` and
  `import` (the config must apply, and a file with it cannot apply its other blocks); a
  run that does not connect (`status` then says nothing of the address); and a
  `poll::Writer` with no channel (a change to `connector`, and an empty Read each tick,
  which the server answers with `BadNothingToDo`). `modbus` keeps `modbus.no-read`, as
  it has no browse (`laptop.architect-2`, 2026-10-18T21:32:25Z:
  https://github.com/synnaxlabs/foundation/issues/3269#issuecomment-6255402516; key
  choice 3 sent to `laptop.monitor`; key choice 7 approved by the person
  (`laptop.monitor`, 2026-10-18T22:01:56Z:
  https://github.com/synnaxlabs/foundation/pull/3272#issuecomment-6255686369); the
  removed refused case approved by the person (`laptop.monitor`, 2026-10-18T21:32:10Z:
  https://github.com/synnaxlabs/foundation/issues/3269#issuecomment-6255400141), and the
  new check case at 2026-10-18T21:35:52Z:
  https://github.com/synnaxlabs/foundation/issues/3269#issuecomment-6255436768).
