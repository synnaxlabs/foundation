- **BROWSE (C1, #2705)** Three operations: `discover` finds devices, `browse` looks
  inside one and changes nothing, and `import` writes the config of chosen tags into the
  file and only adds. One engine serves every front end: the CLI with `--json` and the
  MCP tools, both from the operation table, each designed first for an agent. No TUI in
  C1; a front end holds no device state and no field of a kind, so a TUI later needs no
  engine change. Every kind gives one tree shape, `connector::browse`: an `Entry` has a
  `Path`, a `tag` (the address in the device; two entries with the same tag are one
  entry of the device), and a `Shape`: `Folder`, `Point(Facts)`, or `Unreadable(reason)`
  for a point that the kind cannot read, such as an array. `Facts` holds the sample
  type, the unit (its text and its UN/CEFACT Recommendation 20 code), and read or
  write, each `None` when the source cannot know it;
  `import` writes such a fact as `fill("<what>")`, which `plan` refuses. `browse` has
  two forms: one level, paged by a `Cursor` of the kind, or each entry that is not a
  folder whose path from the start matches a `Pattern` (`/` splits names, `**` alone
  matches any number of names, `*` any text in one name, and `\` takes the next
  character as it is). It is not a `types::name` selector, where `*` matches one whole
  segment of a dot-separated name: a device's names hold any character, a glob looks
  inside one name, and the walk asks at each folder whether a longer path can match.
  A run of `**` names reads as one, so `**/**/x` is `**/x`. A pattern of more than
  `DEPTH` (64) names other than `**` gives `Malformed::Deep` ("it has more than 64
  names, and a browse finds no path that long") before any call to the device: a match
  of n names holds n folders open. A path has no such limit. So a step of a walk costs
  at most 129 segments, and its close is linear (`laptop.architect-2`,
  2026-10-16T17:52:04Z:
  https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6221453512). A
  glob matches a name in time linear in both: it splits at each `*` that no `\` takes,
  and a name matches when it holds at least the bytes of all pieces, starts with the
  first, ends with the last, and holds each other piece in order at its leftmost place.
  No cap on the length of a name (`laptop.architect-2`, 2026-10-16T17:55:41Z:
  https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6221521864). A
  pattern of more than `STARS` (256) `*` in its names other than `**`, a run of `*` as
  one, gives `Malformed::Stars` ("it has more than 256 `*`, and each costs a search of
  each name") before any call to the device (`laptop.architect-2`,
  2026-10-16T22:19:36Z:
  https://github.com/synnaxlabs/foundation/pull/2927#issuecomment-6225785631). This
  supersedes the "no new error" and "no cap on a segment" of the ruling of 17:55:41Z.
  The message says that each `*` costs a search, but a `*` at an end of a name makes
  none; #2948 fixes it.
  The cap does not bound the cost of a walk: each entry tests each glob of its state,
  in time linear in its name. On box1 in release, 63 globs `**/*<1 998 a>*` over
  folders and points named 2 000 `a` take 3.9 s to the `Budget`, at load 5.8. A budget of compared
  bytes is open in #2948 (`laptop.architect-2`, 2026-10-16T22:37:40Z:
  https://github.com/synnaxlabs/foundation/pull/2927#issuecomment-6226011243).
  Values are read only when the `Query` asks, in one call for the points that it gives.
  A new filter is a new field of `Query`. The walk lives once in `connector`
  (`browse::walk`). It finds the start from the root, one name at a time; a name that is
  not there or not a folder gives `connector.no-folder`. A pattern walks depth first in
  device order, browses no folder that no match can be under, gives at most `LIMIT`
  (500) points and ends `Limit` when one more matches, and looks at most at `VISITS` (10
  000) entries, also the entries that find the start, where a page with no entries
  counts as one (a server may give a continuation point with no references), then ends
  `Budget`. With `Limit`, `import` says "More than `<limit>` points match, the most for
  one import; narrow the pattern, or import in parts", not a count. It browses no folder
  whose tag and pattern state are those of a folder that it holds open, so a loop in a
  graph such as an OPC UA address space ends, and a path that a pattern names through a
  loop still matches. A pattern ends `Budget` when it holds `DEPTH` (64) folders open,
  its start with them, and a match can be under one more, so a chain of folders that a
  server makes cannot hold the walk's memory. A kind gives each entry as a `Child`, with
  its name and not its path, and the walk makes the path of each entry from its
  folder's, so an entry holds one link of its own at any depth of the start, and the
  walk holds at most `DEPTH` pages that the device sent. No path that the walk gives has
  an empty name, and the record of each kind says what it gives for an empty name of its
  device, so the text of each path parses. `walk` does not check that no two entries of
  one folder have the same name: one level gives one page, so it never holds each name
  of a folder. The record of each kind says how it names two entries of one folder that
  the device gives the same name. When a kind breaks one of these rules of `Device`: a
  page over `LIMIT`, an empty name, a count of values that is not the count of tags,
  `walk` gives `Error::Config` with `connector.broken-browse`, whose message names the
  rule, then what broke it, and the browse fails with no retry. It never panics: in
  release a panic stops the node and each of its connectors. The browse operation of
  `ops` adds the connector and its kind to each error of `Supervisor::browse`;
  `Supervisor::browse` adds neither. `Kind::browse(ctx, query)` connects and gives
  `walk` over the session, and gives `connector.no-browse` by default. `Kind::discover`
  gives `connector.no-discover` by default, not an empty list, which says that the kind
  looked and found none. Only the default is decided. Its signature is deferred to
  #2851; trigger: the first kind that finds devices. `discover` for OPC UA is not in C1;
  trigger: the person asks, or a kind that can find its devices (NI cards) is built
  (`laptop.architect-2`, 2026-10-15 17:32 UTC, parameter 5 of
  https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6198270136).
  `Supervisor::browse(kind, config, query)` runs `Kind::browse` apart from any run of
  the connector, with the network, clock, randomness, and secrets of the supervisor; a
  kind reads only what it needs to reach the device, so a block with only `kind`,
  `node`, and `address` browses. A kind reads mesh time with `browse::Context::time`,
  which races `Hub::time` against `WAIT` (5 s) and gives `Error::Retry` with "the node
  has no mesh time after 5s" at the bound, so a kind that reads no mesh time never
  waits (`laptop.architect-2`, 2026-10-17T03:22:56Z:
  https://github.com/synnaxlabs/foundation/pull/2966#issuecomment-6228993285).
  `browse::Context` and `kind::Context` stay two types: each input that both give is a
  read of a field or one call, and `secret` is one function. Trigger to make them one:
  a third context, or a second shared input with a body of its own, as `secret` has in
  `read_secret`. A kind implements `browse::Device` on its open session, so one walk is
  one session and can use a continuation point; a `Cursor` that a front end gets holds
  no session state. `level` takes a folder as the
  `Entry` that the device gave, never a path, and gives at most `LIMIT` entries a page;
  `values` gives `types::sample::Value`, so there is one value model. Trigger for a text
  value: a kind that reads a `string` channel. Lost: a count of matches up to the
  budget; trigger: the agent test (PR 9) shows a task with two over-limit refusals one
  after the other. `import --match` uses the same walk, so the points that `browse
  --match` shows are the points that `import --match` takes.
  `config::connector(documents, name)` gives the `connector` block of `name` for a
  browse. It gives `config.private-key` for each private key in `documents` and then no
  other problem, as `config::check` does. It runs no check of a kind. Of the other
  blocks, it reads only private keys, labels, and the bodies of the `connector` blocks
  whose label is `name` in any ASCII case. A node makes one `Supervisor`, on shard 0,
  and gives it to `ops` and to the connector runs: `ops` checks `plan` and `apply` with
  its kinds, reads the secrets of `plan` through its resolver, and browses with it.
  The person's words, through `laptop.monitor`: decision 1, 2026-10-15 17:17 UTC, "Ok,
  this is fine. I approve"
  (https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6197989240);
  decision 2, 17:20 UTC, "Let's avoid the TUI picker for now and make a really good
  agentic approach instead. As logn as we can make a TUI later its all good"
  (https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6198014500);
  decision 3, 17:23 UTC, "Yeah thats fine and this query achitecture can be evolved"
  (https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6198047489);
  decision 6, 17:33 UTC, "Ok this is good"
  (https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6198173531). The
  shape `Unreadable`, the limit, and the PR list: `laptop.architect-2`, 2026-10-15 17:32
  UTC (https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6198270136).
  `browse::Device`, `level` by entry, the page bound, `sample::Value`, the name
  `Malformed`, `Kind::browse` with the default `connector.no-browse`, `Kind::discover`
  with the default `connector.no-discover`, `browse::Context`, `Supervisor::browse`
  apart from any run, a block with only `kind`, `node`, and `address`, and "more than
  `<limit>`" in place of the count of decision 6: `laptop.architect-2`, 2026-10-16 04:00
  UTC, on the plan of `box1.builder-3`
  (https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6207771367). One
  session for the whole browse: the person, through `laptop.monitor`, 2026-10-16
  03:54:58 UTC, "As long as thats the industry standard browsing practice I approve"
  (https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6207717907). It is:
  OPC UA Part 4 requires BrowseNext on the session of the Browse that it continues. A
  page with no entries counts as one visit, not a panic: `laptop.architect-2`,
  2026-10-16 04:36 UTC
  (https://github.com/synnaxlabs/foundation/pull/2823#issuecomment-6208174481). `DEPTH`,
  the shared names, and the empty name: `laptop.architect-2`, 2026-10-16 04:42 UTC
  (https://github.com/synnaxlabs/foundation/pull/2823#issuecomment-6208244581). An
  error, not a panic, for a break of `Device`: the person, through `laptop.monitor`,
  2026-10-16 04:54:26 UTC, "Yes", on "Should a browse bug return an error instead of
  crashing the node?"
  (https://github.com/synnaxlabs/foundation/pull/2823#issuecomment-6208381924). The
  shape of the error: `laptop.architect-2`, 2026-10-16 04:56 UTC
  (https://github.com/synnaxlabs/foundation/pull/2823#issuecomment-6208402982); the
  private `Broken` and a `Path::child` that cannot fail: `laptop.architect-2`, 04:57 UTC
  (https://github.com/synnaxlabs/foundation/pull/2823#issuecomment-6208408668). The loop
  check by tag and pattern state, `DEPTH` as the count of open folders (it replaces the
  meaning of 04:42 UTC), and the four rules that `walk` checks: `laptop.architect-2`,
  05:23 UTC
  (https://github.com/synnaxlabs/foundation/pull/2823#issuecomment-6208716424). The
  `types::name` sentence: `laptop.architect`, 05:20 UTC
  (https://github.com/synnaxlabs/foundation/pull/2823#issuecomment-6208688678). A `Path`
  that shares the path of its folder (it replaces the bound of a deep start of 05:23
  UTC): `laptop.architect-2`, 05:30 UTC
  (https://github.com/synnaxlabs/foundation/pull/2823#issuecomment-6208798023). One tag,
  one entry: `laptop.architect-2`, 2026-10-16 05:04 UTC
  (https://github.com/synnaxlabs/foundation/pull/2823#issuecomment-6208496065).
  `VISITS`: the walk's own time, median of 100 on box1 in release, is 1.1 ms over a flat
  tree of 9 999 points and 0.9 ms over a chain of 63 folders of 150 points each; PR 5 of
  the PR list measures the cost of the OPC UA sim server. The two triggers come from the
  ruling of 2026-10-16 04:00 UTC. A page of `Child` entries, with a name and no path,
  and a private `Path::child` (Supersedes
  https://github.com/synnaxlabs/foundation/pull/2823#issuecomment-6208716424, the rule
  of an entry outside its folder, and
  https://github.com/synnaxlabs/foundation/pull/2823#issuecomment-6208798023, the path
  that a kind built from the root): `laptop.architect-2`, 2026-10-16 05:50 UTC
  (https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6209047339). The
  operation of `ops`, not `Supervisor::browse`, names the connector and its kind in an
  error: `laptop.architect-2`, 2026-10-16 06:46 UTC
  (https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6209790559). No
  check of two names of one folder in `walk`: `laptop.architect-2`, 2026-10-16 06:50 UTC
  (https://github.com/synnaxlabs/foundation/pull/2836#issuecomment-6209844506). The
  deferral of mesh time, and two contexts: `laptop.architect-2`, 2026-10-16 06:55 UTC
  (https://github.com/synnaxlabs/foundation/pull/2836#issuecomment-6209911107).
  `Hub::time`, which waits with no bound, and a bound that `connector` sets:
  `laptop.architect`, 2026-10-16 06:56 UTC
  (https://github.com/synnaxlabs/foundation/issues/2837#issuecomment-6209931375). The
  race on the first read of mesh time (Supersedes
  https://github.com/synnaxlabs/foundation/issues/2837#issuecomment-6209931375, the race
  in `Supervisor::browse`), and the trigger of a body of its own (Supersedes
  https://github.com/synnaxlabs/foundation/pull/2836#issuecomment-6209911107, a shared
  input whose body is more than a field read): `laptop.architect-2`, 2026-10-16 07:09
  UTC (https://github.com/synnaxlabs/foundation/pull/2836#issuecomment-6210109901); its
  `hub` text: `laptop.architect`, 07:20 UTC
  (https://github.com/synnaxlabs/foundation/pull/2836#issuecomment-6210289921). The
  deferral of the signature of `discover`: `laptop.architect-2`, 2026-10-16 08:30 UTC
  (https://github.com/synnaxlabs/foundation/pull/2850#issuecomment-6211422272).
  `config::connector`, its private-key alarm first, and one `Supervisor` of shard 0:
  `laptop.architect-2`, 2026-10-17 05:14 UTC
  (https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6230042870).
  `plan` reads secrets through `Supervisor::secrets`: `laptop.architect-2`,
  2026-10-17 05:40 UTC
  (https://github.com/synnaxlabs/foundation/pull/2999#issuecomment-6230284696).
