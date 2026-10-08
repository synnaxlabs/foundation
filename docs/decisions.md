# Decisions

The source of truth for Foundation's design. Every locked decision, where each data
structure lives, the crate map, and what is still open.

How to read this record:

- IDs come from the design interview (A7, BQ6) or its labels (REGION LOCKED). Study
  decisions carry the study number (R9-D4, R12-4, R13-5). The studies are in
  `docs/research/`.
- "Supersedes" names an earlier entry that no longer holds. Section 1.16 lists every
  retired entry once.
- The person delegated these areas to the design session: quality, memory and
  performance, failover, names, delivery and wire internals, and "decide the best
  architecture". Decisions made under a delegation are as binding as the rest.
- A **region** is the part of the name tree that one voter set governs.
- To change a decision, follow "Interface changes" in `docs/coordination.md`. A change
  to a locked decision needs the person.

---

## 1. Current decisions by topic

### 1.1 Principles and lessons

- **D1, D7** Foundation is one Rust binary that moves data between devices, nodes, and
  stores. It is not a database: no historian, no query language, no long retention.
  Nodes keep a durable buffer (hours to days) for store-and-forward. The mesh is its
  own control plane; there is no separate service. Supersedes: D7 linked meshes, D7
  Raft library, D7 bootstrap peers in the file, D7 any node relays.
- **D2** Foundation is bidirectional. Connectors carry commands through the same device
  owner that reads. No control logic runs in Foundation. Commands expire at a deadline,
  are never replayed after an outage, and return an acknowledgment. Command authority
  and audit ship in the first release.
- **D3** Monetization is deferred. Product decisions are never shaped by what we sell.
  The node is free.
- **D6** People own contracts and oracles. Agents own all other code. Oracles are
  enforced by visibility (C9c). Supersedes: D6 path lock.
- **L1 (neutral model at the boundary)** At every boundary with interchangeable forms
  (syntaxes, carriers, time sources, secret stores, SDK languages), the core works on
  one neutral model and each form is an adapter. Ship one default adapter. Never shrink
  the model to the weakest form. A piece that only one form can express gets its own
  form-independent grammar.
- **L2 (library, not framework)** The code that has the edge cases owns its control
  flow and calls small shared components. Common cases get ready-made compositions
  built only from public parts. At every boundary, test the inverted shape first.
- **L3 (naming tell)** A compound name that repeats a responsibility means the package
  holds a second job. Split until the names get simple.
- **L4 (dependency direction)** Draw what each structure points at. Prefer settings as
  selectors over fields. Reuse a core concept before you add a side structure. Keep the
  model map current.
- **L5 (SEATING REQUIREMENT)** Each feature records the internals it touches, whether
  it can sit on the public surface (yes, partly, no), its boundary, and the trade.
- **SIMPLICITY DIRECTIVE (K5)** Keep the model and its vocabulary minimal. Express new
  needs with existing concepts (policies, selectors, channels) before new terms.
- **S1 principle** Never put in a frame or a series what both ends already know from
  definitions.
- **MODEL MAP rule** Anything that changes over time is a channel (quality, clock
  error, control state, acks, status), not a field or a side array.
- **REDUCTION rule** A policy never creates channels. Anything that creates a channel is
  an explicit definition.
- **BQ11b rule** A crate's value is the source of truth. A channel is a published copy
  for people, agents, and outside tools. Core decisions (failover, fencing) never read
  channels back. Upward flow goes only through values that the upper crate pulls.
- **LIBRARY RULE** Choose the architecture first, then a library or our own build, for
  every major dependency. A library that does its own I/O or reads the clock fails T1.
  Protocol cores (consensus, wire session, control gate, spec sync, clock offset) are
  sans-I/O state machines with thin drivers. Connectors are not.
- **CANONICAL LIBRARY RULE** Per protocol, prefer the canonical production-grade
  implementation in any language, statically compiled with our own Rust bindings, when
  its license allows static linking. Its threads stay inside the connector.
- **PERFORMANCE PRINCIPLES** Minimize copies, locks, and heap allocations at all costs.
  Own the memory and storage layout. No Arrow and no arrow-rs.
- **Root CLAUDE.md principles** apply to every crate: injected dependencies, no mutable
  globals, no load-time self-wiring, concrete types by default, fail loud on an internal
  dispatch key, no defense in depth.
- **DEVX (2026-10-08)** Design each user surface for the person, agent, or program that
  uses it. A user surface is any surface that a user reaches. These are the CLI, MCP,
  the config language, the client protocol and each SDK, and each file that a user reads
  or writes. Each plan for one compares its options by the steps of each common task,
  the first use after a new install among them. It also compares them by the error and
  fix that each wrong step gives (C7). A step that Foundation can do itself is not a
  step for the user (FIRST ADMIN). The person, relayed by `laptop.monitor`: "when we're
  designing public APIs like this, we really need to think about devx"
  (2026-10-08T02:43:43Z,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6051096981). The
  plan rule is decided by `laptop.architect-2` and `laptop.architect` from those words
  (2026-10-08T02:46:51Z,
  https://github.com/synnaxlabs/foundation/pull/1759#issuecomment-6051130026).
- **Process** Each data structure and key decision is proposed with a sketch and
  locked only on agreement. RESCOPE: delivery and wire internals are tuned by
  benchmarks, not interviewed.

### 1.2 Data model

- **A0 + vocabulary** The unit of data is a channel. Identifiers are keys, never IDs.
  A sample is one value at one time. A series is an array of samples of one channel and
  one type. A frame holds series of several channels sent together. Never "batch".
- **A3** Names are dot-separated segments of letters, digits, `_`, and `-`.
  Case-sensitive; case-only collisions are rejected. `*` matches one segment, `**` any
  depth. The `@` prefix is reserved for Foundation. Hierarchy and struct fields share
  the dot. MQTT maps `.` to `/`. Letters are ASCII (decided by the person,
  2026-10-04). Widening to Unicode later stays backward compatible. `discover` maps
  non-ASCII device tags to ASCII names.
- **NAME LENGTH (2026-10-04)** A name or pattern holds at most 255 bytes. The person
  chose "255 bytes": it fits a one-byte length prefix, and raising it later stays
  backward compatible. The `!` of an exclusion is syntax: the exclusion's pattern is
  the text after it. Decided by the advisor on 2026-10-06, #762.
- **SPECIFICITY (#3)** Pattern specificity orders by more literal segments, then fewer
  `**`, then more `*`: `a.b` > `a.*` > `a.*.**` > `a.**` > `**`. A run of wildcards
  counts as its `*`s and one `**` (`a.**.*.**` is `a.*.**`). Two different patterns may
  tie (`a.*` and `*.a`); a tie between the most specific setting policies on one name is
  the S12 plan error. For node settings, the plan error is a tie between the most
  specific policies that set one budget for one node (X25). A tie below them decides
  nothing, because only the most specific value is used. Access has no ties (X25). The
  tie rule is the reading of S12 by `laptop.architect-2` (2026-10-07T15:16:46Z:
  https://github.com/synnaxlabs/foundation/issues/1150#issuecomment-6040858277), and
  `laptop.director` agrees that it changes no rule (2026-10-07T15:27:12Z:
  https://github.com/synnaxlabs/foundation/issues/1150#issuecomment-6041077733).
- **A4 + M1/M2 answer** `channel::Key` is a UUIDv7 made with the channel. It is never
  reused and never changes. Files carry names only. The stored spec maps name to key,
  and `apply` assigns a key the first time a name appears. Renames are explicit
  (`foundation rename`), and `plan` shows them. UUIDs appear only in the stored spec,
  wire setup, and disk footers.
- **A5** A timestamp is i64 nanoseconds since the Unix epoch, UTC. The home rejects
  backwards samples and stamps near 1970 or far in the future (both limits are
  settings). Supersedes: A5 tie rule (by S6).
- **A6 (as revised by A8)** Late data is backfill on the same channel, labeled by the
  writer. Unlabeled backwards samples are rejected. Backfill is ordered and ends before
  the newest live sample. Live readers never see it; recording readers get it marked.
- **A7** An index channel holds timestamps. Each data channel points at one index.
  Alignment holds only inside one frame. A frame may carry any subset of an index's
  channels. A channel that names no index gets a private index. An index has one home
  and one writer in control at a time.
- **A8** Each index has one u64 `seq` over samples, with separate live and backfill
  counters, each gapless on its path. A restart continues from the disk. A home with a
  standby reserves seq blocks from voters. The codec sends seq only on jumps.
  Supersedes: A1 epoch and seq pair, A6 batch counting.
- **A9** Base types: bool, i8 to i64, u8 to u64, f32, f64, timestamp, duration, string
  (UTF-8), bytes, uuid. Numeric series are plain fixed-width arrays.
- **A11** Enums have a fixed integer type and an explicit value per variant. Samples
  store the integer. Unknown values pass through flagged. Variant names are not part of
  channel identity.
- **A12** Flags are an unsigned word with named bits and multi-bit fields. Named bits
  are addressable by dot. Samples store the raw word.
- **A13** Fixed arrays `T[N]` and `T[N][M]` (row-major) and bounded lists
  `list<T, max>`.
- **A16** Units only, no ranges. A unit lives where the number is defined: on a
  primitive channel or on a struct field. Foundation maps common units to the standard
  codes that targets need.
  A unit is 1 to 32 printable ASCII characters with no space, case-sensitive, and
  stored as the text the file wrote. `spec::unit` maps each common unit, with one
  spelling (`degC`, `ohm`, `m/s2`), to its UN/CEFACT Recommendation 20 code, the one
  standard code. A unit that is not in the table is valid and has no code; a sink that
  needs a code fails for that channel only and names the unit (coordinator, #213).
- **A17** Calculated channels are in scope. Calculations write data channels only,
  never commands. Scaling is a calculation. Raw counts plus calculated scaling is the
  default way to meet P1's byte target.
- **A19** No JSON type. `bytes` carries no label. Structure is expressed as a struct.
- **A20** No command channel type; a command is an ordinary channel. Its ack channel has
  the same type, and the connector writes the applied value to it. Failures go to the
  quality channel. Device state is a separate channel read back from the device, never
  an echo. Command and ack channels recorded together are the audit log. Supersedes:
  A20 channel retention (by S12), A20 quality codes (by S13).
- **S5** `Channel { key, name, kind: Kind }`, with
  `Kind::Index { error: Option<channel::Key>, control: Option<channel::Key> }` and
  `Kind::Data { index, quality: Option<channel::Key>, data_type, unit }`. No calculated
  or virtual flag. Amended: no `name` field, because the name is the tree key, and
  `Kind::Data(Data)` has private fields. An array or list of size 0 is valid: no
  caller divides by its width, and a refusal, when one is needed, goes in
  `sample::Type`, which every format reads. The spec numbers its scalar codes in its
  own table, apart from STORED BODY, so a change to one format does not change the
  other. Decided by the architect, #756
  (https://github.com/synnaxlabs/foundation/issues/756#issuecomment-6031378098,
  https://github.com/synnaxlabs/foundation/pull/1119#issuecomment-6031521522).
  Amended: `Kind` and `Data` take the edge form as a parameter, with `channel::Key`
  as the default: `config` gives each edge as a name, and `plan` gives each name its
  key. `Channel`, `check`, and the encoding stay on keys (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/issues/1152#issuecomment-6036793927).
  `Kind::edges` gives each edge and the channel it points at, in this order: the error
  then the control channel of an index, or the index then the quality channel of a
  data channel. `check` reads the edges through it. Each user error has a fix:
  `unit::Error::fix`, and `document::value::Kind::noun` names a value that has the wrong
  kind (`laptop.architect-2`, 2026-10-08T00:51:39Z,
  https://github.com/synnaxlabs/foundation/issues/1152#issuecomment-6049880294).
  Amended: `config` reads each edge from its attribute, so that another bad attribute
  does not hide an unknown edge (`laptop.architect-2`, 2026-10-08T01:32:47Z,
  https://github.com/synnaxlabs/foundation/pull/1685#issuecomment-6050335562).
  Supersedes the `config` caller of `Kind::edges` in
  https://github.com/synnaxlabs/foundation/issues/1152#issuecomment-6049880294.
  `types` owns the text of `sample::Type` both ways (`Display` and `FromStr`): exact
  case, no leading zero in a count, and one space after the comma of a list; `Stamp` and
  `Span` read and show as `timestamp` and `duration` (A9), so a text that reads shows as
  itself. Decided by `laptop.architect` (2026-10-07T11:20:34Z):
  https://github.com/synnaxlabs/foundation/issues/1208#issuecomment-6036837600, and
  2026-10-07T14:40:57Z,
  https://github.com/synnaxlabs/foundation/pull/1439#issuecomment-6040351044, for the
  variants, and 2026-10-07T14:55:31Z,
  https://github.com/synnaxlabs/foundation/pull/1439#issuecomment-6040630702, for the
  scalar names and the `Lengths` fix, and 2026-10-07T15:17:16Z,
  https://github.com/synnaxlabs/foundation/pull/1439#issuecomment-6040868780, for a
  scalar element with space around it as `Syntax`.
  Amended: `sample::Type::Matrix { element, rows: u16, columns: u16 }` holds
  `T[rows][columns]` (A13), row-major, with the bytes of an array of `rows * columns`
  elements. Its fields are public: no `u16` pair overflows `width`, so no format needs a
  check. Its text is `f32[2][3]`; a length over 65535 is `Error::Matrix` (amended
  below), and more than two lengths is `Error::Lengths`. The spec's data type code of a
  matrix is `MATRIX` 6, then the element code, `rows: u16`, and `columns: u16`. A matrix
  of a number can have a unit, by the element's rule, as an array. Decided by
  `laptop.architect` (2026-10-07T17:30:55Z):
  https://github.com/synnaxlabs/foundation/issues/1341#issuecomment-6043244011, and for
  the spec by `laptop.architect-2` (2026-10-07T17:27:36Z):
  https://github.com/synnaxlabs/foundation/issues/1341#issuecomment-6043187013.
  Supersedes
  https://github.com/synnaxlabs/foundation/issues/1341#issuecomment-6042293625 and
  https://github.com/synnaxlabs/foundation/issues/1341#issuecomment-6042559685 (a
  `Matrix` with private fields and `u32` sides), and the `Lengths` text of
  https://github.com/synnaxlabs/foundation/pull/1439#issuecomment-6040630702.
  Amended: the field shape is `Type::Matrix { element, sides: Sides }`, with
  `Sides { rows: u16, columns: u16 }` and its `repr(C, align(4))`, so the sides sit at
  byte 4 of `Type`, as `Array.len`, `List.max`, and the stored `n` do. Without it,
  `home::stored::read` was 16% to 44% slower for each series than `main` (box2).
  Decided by `laptop.architect` (2026-10-07T19:03:23Z):
  https://github.com/synnaxlabs/foundation/pull/1535#issuecomment-6044826781.
  Supersedes the field shape of
  https://github.com/synnaxlabs/foundation/issues/1341#issuecomment-6043244011.
  Amended: each fault of a matrix side (not digits, a leading zero, or over 65535) is
  `Error::Matrix`, as only it states the range of a side; `Error::Count` covers only an
  array length and a list maximum. Decided by `laptop.architect` (2026-10-08T01:01:30Z):
  https://github.com/synnaxlabs/foundation/pull/1535#issuecomment-6049989554.
  Supersedes, in
  https://github.com/synnaxlabs/foundation/issues/1341#issuecomment-6043244011, its
  `Count` for a side that is not a count, and the `Matrix` message and fix.
  `spec::data_type::DataType` reads and writes `quality`, and otherwise the text of
  `sample::Type`. A text that is neither is `data_type::Error`, which holds the
  `sample::Error`; its message names `quality` when the text has no form, and its fix is
  the cause's. `channel::Error` holds only `Unit`, so each call gives only the errors it
  can make. Decided by `laptop.architect-2`: the mapping (2026-10-07T11:21:13Z,
  https://github.com/synnaxlabs/foundation/issues/1152#issuecomment-6036847520), the
  payload, message, and fix (2026-10-08T00:47:39Z,
  https://github.com/synnaxlabs/foundation/pull/1675#issuecomment-6049836256), and the
  module and error split (2026-10-08T01:32:46Z,
  https://github.com/synnaxlabs/foundation/pull/1675#issuecomment-6050335363).
  Supersedes change 2 of
  https://github.com/synnaxlabs/foundation/issues/1152#issuecomment-6036793927, and the
  `channel::Error::DataType` target of the mapping in
  https://github.com/synnaxlabs/foundation/issues/1152#issuecomment-6036847520.
- **S6** An index carries no placement, retention, or rate. Timestamps strictly
  increase per path. The clock error bound is a channel that the index points at with
  `error`.
- **S7 + QUALITY DECISIONS** A struct is a template of channels: one channel per field,
  all on one index. The runtime knows only primitive, fixed array, list, enum, flags,
  string, and bytes channels. Optional field presence is per frame (a presence mask over
  the writer's key set). Struct views are built from one frame, never from per-field
  latest values. Disk chunks record the seq ranges they cover. Supersedes: A10, A14
  validity bits, A15 struct fingerprints, S2 struct layout.
- **A15 (as revised by S7)** Types are defined as code. Enums and flags carry a
  fingerprint. A field added is a channel added; a field renamed is a channel renamed.
  Changes are sorted into safe and breaking before they take effect; breaking changes
  need explicit opt-in.
- **S11** The home's gate is the only control check. Unknown control state means "not
  in control". The home publishes control state on the channel the index points at,
  once per handoff. A writer asks for authority at open, capped by access. Authority 255
  cannot be taken. The control lease is an optional writer setting. There is no control
  policy.
- **GATE RULES (write-path, 2026-10-04)** Writers that do not hold control wait. When
  the holder closes or its control lease runs out, the waiter with the highest
  authority takes control; on a tie, the one that opened first. Each group that the
  home applies or loses renews the control lease of its index, and only that index: a
  live group with no room is lost and renews, and a backfill frame that gets
  `Error::Full` is neither and does not. A writer whose indexes have different rates
  sets its lease by its slowest index. A writer whose control lease ran out stays out
  of the gate until it reopens. Lease and grace times are the home's monotonic time,
  and the X18 grace is a positive span like a control lease. During the grace the
  recorded holder ranks first: the first writer of its subject takes its place, and a
  higher authority takes control. A handoff is recorded only when the holder's subject
  or authority changes. Basis: S11, X18, r8 trace (d). The renewal rule was decided by
  the architect (#1092,
  https://github.com/synnaxlabs/foundation/issues/1092#issuecomment-6031035230 and
  https://github.com/synnaxlabs/foundation/issues/1092#issuecomment-6031117029).
- **S13 + BQ13** Quality is an ordinary channel of type `Quality` (OPC UA 32-bit status
  codes) that data channels point at. One quality channel can serve many channels. It
  may sit on its own index (written on change; a value holds until the next) or share
  its data's index (same writer, per sample, cheap through RLE). Sinks match quality by
  time (as-of). Supersedes: A18 side array.
- **S8 (identity part)** A node has a stable `node::Key` (UUIDv7) separate from its
  rotatable Ed25519 public key. No role fields. Anything that changes about a node is a
  channel under its name. Supersedes: S8 node as a spec definition (by BQ11a). A node
  also has an X25519 seal key, which callers seal secret values to. The node makes it
  at join, signs it with its Ed25519 key, and rotates it with that key. A voter and a
  caller check the signature. The person decided on 2026-10-05: "ok fine" and "Add an
  encryption key" (#211).
  A caller seals with HPKE base mode: DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, and
  ChaCha20-Poly1305, built from aws-lc-rs parts. The info is `foundation/secret/1`,
  and the associated data is the secret's full name (`secret::seal`, #318).
- **R9 type decisions (SETTLED BY ME)** R9-D1 per-entry types are interned once in the
  key set; R9-D2 bools are one byte; R9-D3 raw series are padded to element width and
  blocks are 64-byte aligned; R9-D4 variable-length series are `ends[n]` then data;
  R9-D5 `types` is byte layout only and meaning lives in the spec; R9-D6 `time::Span`
  value and `duration` keyword; R9-D7 JSON uses RFC 3339 UTC with 9 fraction digits,
  span unit strings, and keys as UUID strings, and never appears on the data path; R9-D8
  exact reduced-fraction `Rate` with u128 offset math; R9-D10 panic on internal overflow
  and checked math for outside values; R9-D13 `types` modules are time, sample, series,
  frame, channel, node, quality, name, hash (R16-7), authority (#57: `access`, `spec`,
  `wire`, and `control` all use it), and digest (the BLAKE3 address of spec chunks and
  blobs, which `spec`, `blob`, `wire`, and `mesh` share); R9-D14 checks run once, at the
  home. R9-D11 rejected (slots won). Supersedes: R9-D13 `block` module (by SRP PASS).
- **MODEL MAP (current)** Data channel -> index, -> quality (optional), -> data type,
  -> unit. Index -> error channel (optional), -> control channel (optional). Type ->
  other types; types never point at channels. Policies -> names through selectors;
  channels never point at policies. Readers and connectors -> channels by name or
  selector. Calculation -> inputs, -> its own output index. Connector -> channels it
  reads and writes; channels never point at connectors. Region -> name prefix. Node,
  connector, subject, and channel names share the one name tree. A channel's edges stay
  in its region (REGION CHECK; `laptop.architect`, 2026-10-08T08:43:31Z,
  https://github.com/synnaxlabs/foundation/issues/1841#issuecomment-6056144931).

### 1.3 Delivery

- **B1 (as revised by S10)** The home keeps a disk buffer for each index. Data stays
  while a holding reader has not received it, up to the index's retention (RETENTION),
  within one disk budget per node. When the disk is full, the oldest data goes first,
  readers get an explicit gap, and the node warns early and names the reader that holds
  the buffer. Group commit every few ms. Complete readers get frames only after they are
  on disk. Amended: per S10 (architect, #895,
  https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6037251160).
- **S10 (reader)** A reader is a session, not a definition: `Reader { subject: Name,
  name: Option<String>, select: Selector, mode: complete or latest, from: now, oldest,
  seq, time, or resume, max_age, hold: Duration (default 0) }`. Only complete mode
  holds. A hold is capped by the index's retention. One session per named reader
  (subject and name); a new one takes over. Out connectors carry reader settings in
  their config. Current readers and holds are published on status channels. Supersedes:
  B1 durable reader, B2 durable and ad-hoc readers. A named reader belongs to the
  subject that opens it: the home keys it by subject and name, and its position record
  names both. An open by the same subject takes over. An open by another subject with
  the same name opens another reader and takes over nothing. Lost: refuse a takeover by
  another subject (decided by `laptop.architect` in the body of #1851,
  2026-10-08T10:01:28Z, and in
  https://github.com/synnaxlabs/foundation/issues/1851#issuecomment-6057659053,
  2026-10-08T10:15:04Z; raised by `laptop.architect-2` in
  https://github.com/synnaxlabs/foundation/issues/1807#issuecomment-6057222444).
- **RETENTION (architect, #895)** A retention policy `{ select, keep }` caps by store
  time the holds on the indexes it selects (READER RULES), so `buffer` may trim a sample
  past the cap (STORE TRIM). Retention deletes nothing: a ring frees only at its tail,
  so a time on one index cannot free its samples. It keeps no history window. An index
  that no policy selects has no time cap. `keep` is zero or more. At `0s` the cutoff is
  the mesh time of `home`, from its first estimate (READER RULES). A trim gives a reader
  that is behind a gap at any `keep` (STORE TRIM). Most specific wins as a whole policy
  (X25), a tie between the most specific policies is a plan error (S12, SPECIFICITY),
  and a data channel takes its index's policy (X26). Lost: a finite default `keep`
  (5.3), a value for "no cap", a size cap per index, and a read that reports each sample
  past `keep` as a gap while its bytes are on disk. That read does not depend on disk
  pressure, but at `0s` a reader a few milliseconds behind loses each sample it reads
  from disk, and each read needs `keep` and a clock. Stale commands are the job of
  `max_age` (A20), not of retention. In `config`, `select` and `keep` are both required.
  `keep` reads with `document::read::span`, which refuses a negative span with
  `document.negative-span` at the `keep` value, as it does a reader `hold` (S10,
  DOCUMENT KEYS; `laptop.architect-2`, 2026-10-08T07:04:36Z,
  https://github.com/synnaxlabs/foundation/issues/1785#issuecomment-6054474145).
  Supersedes the `config.negative-span` code and the clause "A negative span reads, and
  each caller owns its bound" of the ruling at 2026-10-07T11:44:51Z,
  https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6037207886.
  Ruling and answers:
  https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6032219156,
  https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6037207886,
  https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6037251160. The lost
  read: decided by `laptop.architect`, 2026-10-07T12:30:53Z,
  https://github.com/synnaxlabs/foundation/issues/1377#issuecomment-6037946637. The cap
  by store time: decided by `laptop.architect`, 2026-10-07T13:39:39Z,
  https://github.com/synnaxlabs/foundation/issues/1080#issuecomment-6039184732 (READER
  RULES). It supersedes 6037946637 in its clauses "past `keep` after its store time, no
  hold keeps a sample" and "At `0s` no hold keeps a sample after its store time".
  Supersedes https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6032219156
  in its clause that `buffer` trims a sample past `keep`, also when a reader holds it.
- **S10 + S11 + BQ7 (writer)** A writer session is `{ subject, authority, control
  lease, channels, confirmation: stored or replicated }`. It has no path: the label on
  each write (B7) is the only source, and a write with no label is live. The person
  decided on 2026-10-05: "as long as you've evaluated the performance costs of your
  decision against correctness then I'm ok with this" (#243).
- **B2** Selectors stay live: channels created later that match join the subscription.
  A start time maps to the first sample at or after it, per index. A range the buffer no
  longer has is an explicit gap.
- **B3 (as revised by READER RULES)** Complete mode orders per index only. A reader
  reports one cumulative position per index (a durable reader after it stores the data).
  Flow is push with reader-granted credits. A slow reader catches up from disk and never
  slows writers or other readers. Delivery is at-least-once; seq makes repeats easy to
  drop.
- **READER RULES (write-path and advisor, 2026-10-04)** A position is one cumulative seq
  per path: the first sample the reader has not received. A reader that does not record
  has no backfill position. An open session holds all data it has not received. A closed
  named reader holds from its position until `hold` after the close, in mesh time; an
  unnamed reader holds nothing after it closes. A hold is zero or more; `config` rejects
  a negative hold (#94). The floor per path is the lowest held position, or none.
  Retention caps the holds: the floor of a path is at least the lowest seq whose store
  time is at or after the cutoff (the mesh time of `home` minus `keep`), or past its
  last sample when no seq is (RETENTION). The cutoff moves with time, so `home` gives it
  on its interval, not only when a position moves. After a failover the store times of a
  path need not rise with its seq, so a sample stored before the cutoff can stay held a
  little longer, and no sample stored at or after the cutoff loses its hold (decided by
  `laptop.architect`, 2026-10-07T13:39:39Z:
  https://github.com/synnaxlabs/foundation/issues/1080#issuecomment-6039184732). Before
  its first estimate `home` has no mesh time and gives no cutoff, so the cap starts at
  the first estimate (decided by `laptop.architect`, 2026-10-07T16:35:57Z:
  https://github.com/synnaxlabs/foundation/issues/1080#issuecomment-6042343145). These
  supersede, in their clauses that `home` gives `set_floor` the index's `keep`, that
  `buffer` raises the floor past each sample stored more than `keep` ago, and that the
  2.3 row "Holds and floors" stays, part 2 of
  https://github.com/synnaxlabs/foundation/issues/1377#issuecomment-6037946637,
  https://github.com/synnaxlabs/foundation/issues/1080#issuecomment-6037950577, and the
  floor sentence of
  https://github.com/synnaxlabs/foundation/issues/1377#issuecomment-6038431739. Part 3
  of 6037946637 (at `0s` the floor is the stored mark) holds only after the first
  estimate, and only when each store time of the path is before the mesh time of `home`.
  After a failover, a copied store time can be after the mesh time of the new home. A
  trim follows STORE TRIM: under disk pressure, at the tail of the ring, whatever the
  floors (decided by `laptop.architect`, 2026-10-07T12:59:37Z:
  https://github.com/synnaxlabs/foundation/issues/1377#issuecomment-6038431739).
  Supersedes https://github.com/synnaxlabs/foundation/pull/89 in its clause that
  `buffer` trims below the floor, past retention (by store time), and under disk
  pressure. A resume takes, per path, the position the reader's `hub` presents, then the
  position at this home, then the home's fallback. A position below the floor or past
  the head is accepted as is; the `buffer` read reports any gap (B2). A resume starts a
  `buffer` read at `Mark::at(position)`, so the entries with no samples at the position
  come again. Between reads, the caller keeps the mark the last read gave, in memory
  (#510). Named readers write a position record at once when they open, close, or are
  taken over, and on the home's interval when the position changed. A session open at a
  crash restores as closed at the restore. The home drops a grant, ack, or close for a
  key it gave that is no longer open: a late message after a close or a takeover. A take
  of such a key gives nothing. A key is the home's own value, in memory only, and keys
  start again at a restore. No hub message carries a key: the home maps each one to a
  key it gave, so a key it never gave is a defect of the home and panics. Complete and
  latest sessions have separate key types, so a call in the wrong mode does not compile
  (advisor, #725; the take and the key rule: architect, #1038). Only a named complete
  session needs mesh time to close: `Readers::close_named` and
  `Readers::open_named_latest` take a stamp, and no other open or close does, so the
  home opens unnamed readers before the first estimate. A named complete session has a
  `complete::Key`, and the wrong close of an open session panics; the architect decided
  (#1024). Supersedes the B3 single position. Basis: A6, A8, B2, B3, S10, X14, #41.
- **STORE TRIM (2026-10-06)** Under disk pressure, `buffer` frees its oldest records
  itself, in the commit task, whatever the floors: a ring frees space only at its tail,
  so a floor never changes which record goes (B1). The commit writes the new tail in the
  same sync as its data, and reuses the space only after that sync. A trim never frees a
  record that a read in progress holds (#510). `buffer` keeps its own headroom (at least
  two records of `body_max`, or twice the records of the commit that trims), so a full
  ring does not refuse a live write under steady pressure. `append` gives
  `Rejected::Full` only when the records queued since the last commit do not fit after
  the trim: the full disk queue of B5, which is the commit queue. A read reports the
  trimmed seqs of a path as `Read::gap`, also when the path holds no entry, and the
  gap's length is the count of samples lost (B2). An open of a full ring frees the
  oldest record for its restart record, until segments exist (S4). `set_floor` and
  `usage` wait for their first effect, the B1 warning (#1080). Lost: a `trim` call from
  the home (it needs the commit rate, a late trim gives a second gap, and the edge cases
  of the ring move up into `home`). Decided by the architect (#160,
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6030836762). An open
  frees to the same headroom as a commit. A checkpoint never passes the newest record of
  a path unless a later synced record holds that path's tail (seq and stamp), so a
  restart continues from the disk (A8). That record syncs before the checkpoint, in a
  sync of its own: a crash can keep the checkpoint and lose a record of the same sync.
  The cost of a trim grows with the paths that lose their newest record, not with all
  paths. A carried tail is no sample: a read gives no entry for it, only the gap up to
  it. The trim does not turn on without the carried tail, and the PR that builds it
  records its form on disk here. Decided by the architect (#160,
  https://github.com/synnaxlabs/foundation/issues/160#issuecomment-6032697113).
  As built (#1222): the headroom is three times the larger of the largest record and the
  records of the commit that trims, which are the records not yet synced. The space of a
  trim is free only at its release, after its sync. From one trim to the release of the
  next, the ring takes the records of two commits and the blocks that one wrap skips,
  which are less than one largest record. So after commits of `c` bytes, the next commit
  fits when it and the skip are at most `2c`, and the next two fit when they and the
  skip are at most `3c`. A load that grows by the factor `g` with each commit fits while
  `g + g²` times `c`, and the skip, are at most `3c`: under about 30 percent a commit.
  Above that, `append` gives `Full`, the full commit queue of B5. A trim cannot free the
  commit in its sync, so the bound also needs an area that holds three commits in a row
  and one wrap skip: `3c` and the skip for a steady load, and `4c` and the skip when one
  commit is `2c`. Lost: twice the records of the commit that trims (it refused a write
  at each wrap), and twice those records plus one largest record (it refused each commit
  that was more than one largest record over the commit before it). Each figure that
  follows is measured with each commit placed while the one before it syncs. On a full
  ring of 1024 blocks after commits of 40 records of one block, a commit of 80 records
  is not refused and a commit of 81 is, and two commits of 60 records are not refused
  and commits of 60 and 61 are. A commit of 20 records of four blocks is refused when it
  wraps, because the wrap skips 3 blocks, and a commit of 19 is not. On a new ring of
  1024 blocks, with records of one block, a steady load of 341 records in each commit is
  not refused and one of 342 is, at its third commit. A commit of twice the commits
  before it is not refused after commits of 256 records of one block, and is refused
  after commits of 257. After commits of 64 records of four blocks, which are a quarter
  of the area, a commit of 128 and the commit of 64 after it are not refused when no
  record from the commit two before the 128 to the commit after it must skip a block at
  a wrap. When a record must skip 3 blocks at a wrap in one of those four, the 128 or
  the commit after it is refused. With one record in each commit, three records and the
  blocks of one wrap skip must fit in the area. Four of the largest record less one
  block always hold them, and a smaller ring can refuse a live write under a steady
  load: with a largest record of four blocks, commits of 2, 4, 4, and 4 blocks get
  `Full` on a ring of 13 or 14 blocks. So `Layout::new` refuses an area under four of
  the largest record (#1276), and a ring file with a smaller area does not open. That
  minimum is for one record in each commit: on a ring of 16 blocks, a steady load of one
  record of four blocks and one of two in each commit is refused at its third commit. A
  trim moves the tail to the boundary after a record of any kind: a wrap record and a
  restart record also end where a tail can go. Steady pressure in the ruling means a
  load whose commits fit the area. The ring size for a real load is the sizing of `node`
  (SHARD DISK), not the minimum of `Layout::new`. Decided by the architect: the headroom
  (#1222, https://github.com/synnaxlabs/foundation/pull/1222#issuecomment-6033965557),
  the area that the bound needs (#1222,
  https://github.com/synnaxlabs/foundation/pull/1222#issuecomment-6034545693,
  2026-10-07T08:57:53Z, and with the skip in the `4c` case
  https://github.com/synnaxlabs/foundation/pull/1222#issuecomment-6034791932,
  2026-10-07T09:12:31Z), and the boundaries and the minimum of `Layout::new`, built in
  #1276 (#1222,
  https://github.com/synnaxlabs/foundation/pull/1222#issuecomment-6033889998,
  2026-10-07T08:16:51Z). The blocks that a wrap skips are no record, so the headroom
  leaves them out; the bound counts one skip on its own. A release past the last synced
  record panics. Decided by the architect (#1345,
  https://github.com/synnaxlabs/foundation/issues/1345#issuecomment-6036930649,
  2026-10-07T11:26:39Z).
- **WAL BENCH (#324, 2026-10-08)** The cargo feature `sim` of `buffer`, off by default
  (`buffer`'s dev-dependency on itself turns it on for the bench), adds
  `#[doc(hidden)] pub mod bench` with `Ring { new, commit }` over `wal::Writer`, as
  STORED BENCH does for `home`. Only the bench `benches/wal.rs` (`test = true`) uses it.
  `commit` appends its records, takes `trimmed`, syncs each record, and releases to the
  trimmed tail, so a time holds the writer's cost for each commit. The bench runs
  commits of 1 and 8 records, in rings of 64 and 4096 blocks: 1 record gives the cost
  for each commit (`trimmed`, `release`), 8 the cost for each record (`append`,
  `synced`). Lost: a copy of `wal.rs` in the bench through `#[path]`, which needs
  `entry`, `record`, and `crc32c` copied too and the workspace lints off; a time of
  `Buffer::append` and `committed` on a simulated file system, whose file writes hide
  the cost of the writer; and a control bench of code that a PR does not change, since
  the bench host gives A/A. It lands before the next PR after #1698 that changes
  `Writer::append`, `synced`, `release`, or `trimmed`. Decided by `laptop.architect`
  (2026-10-08T01:37:51Z):
  https://github.com/synnaxlabs/foundation/issues/324#issuecomment-6050388456. Its
  baseline is a quiet-host run of this bench at the head of #1729, not the #1698 rerun,
  whose scratch bench had no warm-up. The run makes two passes of one binary, and each
  median agrees within 3%. If one does not, a `crate:buffer` issue follows, and there is
  no baseline until it is fixed. Decided by `laptop.architect` (2026-10-08T03:12:46Z):
  https://github.com/synnaxlabs/foundation/pull/1729#issuecomment-6051403457.
  Supersedes "Its head numbers are the baseline of the `wal` benchmark" in
  https://github.com/synnaxlabs/foundation/pull/1698#issuecomment-6050306136. The
  #1698 numbers stay the record of the P1 judgment of #1698 only.
- **CREDIT RULES (write-path, advisor, and data-path, 2026-10-05)** A complete reader's
  `hub` grants credit to each session on one index as an absolute byte limit since the
  session opened, in a `Credit` message apart from the ack. Both sides count from zero
  at each session, including a takeover and a resume at a new home. The open carries the
  first grant; until then the session has no credit. A grant only raises the limit, so a
  repeated or reordered grant does no harm. A `Credit` is sent reliably: a blocked
  session gets no frame, so no later grant would replace a lost one. The home drops a
  grant for a session it closed. The home sends a whole frame while the bytes it has
  spent are below the limit, so it passes the limit by less than one frame and never
  splits a frame. A frame that finds the limit spent waits at the home, and so does each
  later frame; the session takes it while the bytes it spent are below a later grant. A
  frame that still waits when the home releases the next commit with frames of its index
  is refused. A grant wakes no session: the task that grants takes after it. So a reader
  that takes the frames of each commit before the next such commit ends is never
  refused, whatever the size of the commit. laptop.architect decided this on
  2026-10-08T07:23:30Z:
  https://github.com/synnaxlabs/foundation/issues/1170#issuecomment-6054827276 (#1170).
  After a refusal, the session gets no later frame until it has the refused one; frames
  from catch-up spend credit too. A frame costs its charge, `Frame::charge`: the bytes a
  block of the frame's length takes from a pool. That is `block`'s header plus the whole
  frame (M3), rounded up to its size class, so a frame costs its length plus the header
  and at most 64 bytes or a quarter of its length more, and a frame with only empty
  series still costs its headers. The charge depends only on the frame's length, so the
  home and the `hub` compute the same charge for the same frame. A remote complete
  reader gets only the series of its view (M2): the home sends a frame of those series
  in the reader's entry order (HUB WIRE), and both ends charge that frame. The person
  chose this on 2026-10-05 ("B is approved ... send only partial frames"), #267. Each
  complete session has a `delivery::complete::Charge`: `Whole` (a local reader) spends
  the home's frame, and `Places` (a remote reader) spends the frame of one series for
  each slot it lists that the frame holds, in listing order, the first listing of a slot
  only. Catch-up uses the same `Charge`. So a remote session pins home blocks up to its
  window times the ratio of the home's frame to its view. laptop.architect decided this
  on 2026-10-07T22:47:54Z:
  https://github.com/synnaxlabs/foundation/issues/1642#issuecomment-6048424611.
  `home::reader::complete::Charge` re-exports it, and `home::Shard::open_complete` takes
  it, so `hub` does not depend on `delivery`. The `Charge` adds about 7 ns per frame to
  `release` with one `Whole` session; that is accepted, with the `Places` state boxed,
  so that a `Whole` session grows by one pointer and not by the size of `Places`.
  laptop.architect decided both on 2026-10-07T23:13:16Z:
  https://github.com/synnaxlabs/foundation/pull/1655#issuecomment-6048741570. The box2
  rerun gave 8.0 ns per frame at one session and +4.0% at 16; laptop.architect ruled
  on 2026-10-07T23:38:44Z that the acceptance covers it:
  https://github.com/synnaxlabs/foundation/pull/1655#issuecomment-6049045510. The
  `hash::Map` of those states adds about 0.3 ns per place to `release` at 100k places
  (+10%); laptop.architect accepted it on 2026-10-07T23:32:22Z:
  https://github.com/synnaxlabs/foundation/pull/1655#issuecomment-6048971185.
  `types::frame::Places` holds this charge and the layout of the frame that `serve`
  sends (HUB WIRE, #1648), with laptop.architect's OK on 2026-10-07T23:22:03Z to move
  it out of #1655:
  https://github.com/synnaxlabs/foundation/issues/1648#issuecomment-6048849864, and
  its surface approved on 2026-10-07T23:34:09Z:
  https://github.com/synnaxlabs/foundation/issues/1648#issuecomment-6048992122. The
  charge is part of the wire contract: a change to `block`'s header or size classes
  needs a new wire version (C9d). The classes changed to four per
  doubling under wire version 1 (#188), because no release carries that version. The
  window counts charges, not wire bytes. Per-connection framing in `wire` (X35) pins no
  pool memory and does not count. Credits apply only to complete delivery, which is
  reliable: a lost frame would leak credit. The `hub` raises the limit only after it
  releases a frame, and it bounds its decoded copies itself, since a small encoded frame
  can decode to much more. It sends a `Credit` only when the room it has not announced
  reaches half the window, and puts the grants for all sessions on one link into one
  message. It sizes one window per reader from the link's bandwidth-delay product,
  adapts it, and divides it among the indexes the reader reads. Each session with room
  can pass its limit by one frame, so the `hub` counts one largest frame per such
  session against the window, and a reader pins at most its window, plus the frames of
  the last release of each index it reads that wait for it. The sessions of an index
  share those frames, which memory held until that release; on a quiet index they stay
  until each session takes them or closes. Replaces r11 5.2 (a window beyond the
  acknowledged position): flow control stays apart from durable acks. Basis: B3, M3,
  MEMORY BOUNDS, X35, r11 5.2, #41, #267.
- **B4** Latest mode gives a new reader the current value at once. A slow reader keeps
  at most one waiting frame per index; a newer frame replaces it; frames never split. No
  replay after a disconnect. Frames go out before the disk sync. The current value is
  the index's newest live frame, even when it holds none of the reader's channels (M3).
  The person decided on 2026-10-05: "Newest frame" (#139).
- **B5** Live writes never wait. If the disk queue or the pool is full, or the link to a
  remote home cannot take the frame now, the home records an explicit gap and warns; on
  the link, the writer's `hub` drops the frame and sends the gap to the home (RECV
  WAITS). Backfill waits for room. The person decided on 2026-10-05: "Ok, as long as
  the end user UX remains the same" (#581).
- **B6** One write call is one frame. Smart batching is the default. Catch-up may merge
  consecutive frames (limit in X30). Acquisition and transmission settings are code,
  changeable on a running mesh, with defaults chosen by the end-to-end sweep.
- **B7** A frame applies whole or not at all, per index. A writer never resends on the
  live path. After a reconnect, it resends each unconfirmed frame, live or backfill,
  with its original boundaries, labeled `resend`. `resend` is a write label, not a third
  path: the home checks a resend frame by timestamp against both paths of each index,
  and each index lands on one of them or on none. The home sets the frame's path when it
  freezes the frame, after the check, so a resend frame carries the path it landed on.
  When every sample of an index exists with the same values, that index is a repeat.
  When no sample exists and the index's data starts after its newest live stamp, it
  never landed, and the home applies it to the live path. When no sample exists and the
  data fits A6, it applies as backfill. Anything else is an error: some samples stored
  and some not, a stored timestamp with other values, data that fits neither path, or a
  range below the buffer's floor. The home checks a resend per index frame (INDEX
  FRAMES): each one lands on one path or is dropped as a repeat, with no second split.
  It confirms the resend when every index frame has landed or was dropped. Live and
  backfill frames pay no check. Values are compared decoded, not as bytes. Writers
  assign no numbers: a resend comes in a new session, and the writer never learned
  them. The person decided on 2026-10-05: "By timestamp + same values" (#148), then "A
  `resend` label" (#168), then the split, a resend of every unconfirmed frame, and no
  session path: "as long as you've evaluated the performance costs of your decision
  against correctness then I'm ok with this" (#243).
- **READ COPIES (delivery part)** `hub` merges latest subscriptions for one remote home
  into one upstream flow.
- **BQ3** `hub` is the whole layer-3 window: `reader()`, `writer()`, read-only
  `spec()`, `watch(selector)`, `now()`, and `block(len)`. Remote SDKs get the same
  surface over the network.
- **BQ4** Encode once at entry: the home encodes local writes, and the writer's `hub`
  encodes remote ones. Decode once at the reader. Each series decodes by itself. The
  home checks data series by their headers only and decodes index series.
- **RESCOPE** B1 to B7 are starting points tuned by T1 benchmarks.

### 1.4 Storage and codecs

- **S2 (as revised by M3, BQ4, R10)** On the wire, a series payload is only its data
  bytes. Type and width come from definitions. The codec choice is a 1-byte tag per
  vector inside the bytes. Supersedes: S2 series struct, S2 per-channel compression,
  S2 per-link re-encode.
- **S3 (R9-D4, r2 Q1)** Fixed-width series: `values[n]`. Fixed arrays: row-major.
  String, bytes, and lists: `ends[n]` (u32 end offsets) then data; lists nest. The
  sample count travels once per index group in the frame.
- **BQ4 + ADAPTIVE COMPRESSION (r10)** Per 1024-value vector, one stats pass gives exact
  sizes, and the encoder picks the smallest. Raw is always a candidate. The minimum
  saving is a fixed 1/8. No sampling and no hysteresis, except ALP's top-5 exponent
  pairs, refreshed about every 100 vectors. Codec set: integers raw, FFOR, delta
  (natural order), RLE; timestamps add stride; floats raw, ALP, fdelta, RLE; `max` mode
  adds pco. No zstd and no ALP_rd. Policy: `compression { select, mode = auto, raw, or
  max }`, default `auto`. The validator is the top fuzz target.
- **CODEC FORMAT V1 (#4)** A vector is a tag byte, a bit width byte, header fields,
  zeros to a multiple of the sample width, then a body padded the same way. Tags: 0
  raw; 1 FFOR (reference; body `sample - reference`); 2 delta (first, base; body
  `sample - previous - base` for each sample after the first); 3 RLE (`u16` run count;
  body the values, then `u16` lengths). Sample arithmetic is modulo 2^b, where b is
  the bit count of a sample. Packing is in natural order in every vector, least
  significant bit first. The person chose it ("Natural order") over FastLanes order
  for full vectors: a natural-order FFOR decode prototype took 197 ns per vector on an
  M3 Max, about 1.9% of a core at 100M samples/s. FastLanes order can come later as a
  new tag. Raw and RLE have bit width 0. Integers, `Stamp`, and `Span` use all four
  tags; other scalars use raw. Timestamp stride (BQ4) comes later as a new tag. The
  validator checks tags, bit widths, lengths, and run sums, not padding. `max_len` of
  the raw length (raw plus one raw header per vector) sizes the output, and the
  encoder makes one pass. `codec/src/vector.rs` is the full spec. `codec` is the one
  place that checks a series against its count (#359): `encode` refuses raw values
  that do not hold `count` samples, and `validate` refuses encoded bytes that do not
  parse as `count` samples. The bytes do not carry the count, so a wrong count passes
  when the vectors also parse at it: a vector with bit width 0 holds any count up to
  1024. A fixed array is the series of its `count * len` elements. A `String`,
  `Bytes`, or `List` series is the `u32` series of its ends, then the series of its
  elements (`u8` for `String` and `Bytes`). An end counts elements from the first, so
  ends never decrease, and a `List` sample holds at most `max` elements. In the raw
  form, zeros pad the ends to a multiple of the element width or 8, whichever is less
  (R9-D3). A frame series starts on 8 bytes, so the elements are then aligned. The
  encoded form has no padding. `codec` owns the check of the ends, raw and encoded,
  and a view of a raw variable series relies on it. `codec` does not check UTF-8 (the
  owner is #556). Vector numbers in errors count across the ends and the elements.
  `Decoder` decodes a scalar series one vector at a time, so a reader of a series from
  a peer needs room for only 1024 samples, whatever the count (#416).
- **S4 (r2 starting point, not locked)** Per shard: a preallocated write-ahead ring
  (CRC32C per record, one group-commit sync), then immutable columnar segments with one
  chunk group per index. Eviction deletes whole segments. No per-channel files. A failed
  fsync is fatal and never retried.
  Ring record (starting point): `[len: u32][crc32c: u32][kind: u8][body]`, starting
  on a 4096-byte boundary so a commit never rewrites a synced block, except the
  restart record of an open and the records after it, which may go over a restart or
  wrap record that no data record follows (#649). The CRC covers `len`, `kind`, and
  the body. It continues from the record before (a chain), so bytes of an earlier
  chain never read as the next record.
  Kinds: data (1), one per group commit; wrap (2), no body, the rest of the area is
  not used and the next record is at its start; restart (3), written at each open
  right after the last data record the walk reads, or at the tail when it reads none,
  its body is a random `u32` and the chain continues from that value. A record never
  crosses the end of the area. Kind 0 is never valid.
  Offsets count bytes since the ring was made and never wrap; the place in the area
  is the offset modulo the area length. The area is at least four times the largest
  record (#1276), so a ring that holds only its restart record takes any record (#637),
  and, when each commit and each open trims, a steady load of one record in each commit
  gets no `Full` (STORE TRIM). This supersedes the two times of #637. A ring whose head
  reaches the end of the offsets is full for good. A body is at most `u32::MAX` bytes
  and at least one block less the record header (4087 bytes): a record takes whole
  blocks, so a smaller one saves no disk and only holds less per commit.
  Data body: `[count: u32][count entry headers][bytes of entry 1][bytes of entry
  2]...`. An entry header is `index: u128, path: u8 (live 0, backfill 1), first:
  u64, len: u32, stored_at: i64, last: u8 + i64, tag: u8, bytes: u32`, 51 bytes,
  little-endian, fixed width; `last` is a presence byte (0 or 1) then the stamp,
  which is 0 and not read under presence 0. The entries' bytes follow the table in
  order, each `bytes` long, so one table block and the callers' blocks make one
  vectored write with no copy and no block per entry. A body holds at most 1023
  entries, so that write stays within `IOV_MAX`. A body that ends early, a count
  over 1023, an unknown path or presence byte, or bytes after the last entry is a
  wrong shape.
  For each path, memory holds one run per data record with an entry of it: the
  mark before the path's first entry in the record and the record's offset, oldest
  first, 24 bytes per record and path in a deque that doubles, so at most 48/51
  of the area. A mark is a seq and the count of entries with no samples at it
  already given, so a read resumes between two such entries. The recovery walk
  and each sync feed the runs in ring order; a read starts from them. A read does
  not check the record CRC: the open's walk checked each record, and a record
  this process wrote is read as written. A read's budget counts the pool bytes
  that its entries' blocks take (`block::footprint`), so an entry with no bytes
  still costs its block. A read drops a record's table block before it takes the
  blocks of the record's entries, so an entry of the pool's largest block reads
  (#968). A read holds no record while it waits for a file read, so a change that
  frees ring space must first hold the records of each read in progress (#510).
  Recovery walks from the tail to the first record that does not follow the chain. A
  block of kind 0 is no record and ends the walk, also when its CRC follows the chain:
  no version writes kind 0, and a zeroed block must end the walk for every chain value
  (decided by the architect, #1049:
  https://github.com/synnaxlabs/foundation/issues/1049#issuecomment-6031201904). A
  record that follows the chain but has an unknown kind or a wrong shape fails the open,
  and so does an entry whose `first` is below the tail of its path or whose
  `first + len` passes `u64::MAX`. The open syncs the ring before it reports a tail
  durable: a killed process may have written records that it never synced (#657). Before
  that sync, the open writes again, as read, the two header blocks and each window the
  walk reads before the one that ends the chain. A read can see, from the cache, writes
  that a failed sync of an earlier process in the same boot lost, and the cache can drop
  them between two reads. So an open writes again the header, 8 KiB, and the bytes it
  walks, at most the area, and the first 52 KiB of each record over one block twice
  (#698). Lost: a walk with direct I/O, which needs a new `env::files` read mode in each
  driver and in `sim`. An open that fails with `Invalid` wrote only bytes that it read,
  where it read them, and did not sync the ring: the ring reads as it did before the
  open. That is a statement about what a read gives, not about what is durable. On a
  disk that refuses a write, such an open can give `Files`. Lost: a first walk that only
  reads, which reads each record twice, and windows held until the walk ends, which
  takes memory up to the area. Decided by the architect (#1049,
  https://github.com/synnaxlabs/foundation/issues/1049#issuecomment-6030897567). A file
  with no checkpoint is no case of its own: the open makes it again (#1254), so the walk
  after the first checkpoint reads a zero area and ends. This supersedes the case "on a
  file with no header" of
  https://github.com/synnaxlabs/foundation/issues/1049#issuecomment-6031034971. Decided
  by the architect (#1286,
  https://github.com/synnaxlabs/foundation/pull/1286#issuecomment-6034555756,
  2026-10-07T08:58:30Z). Each statement about a record holds only when no CRC gives a
  false match. Decided by the architect (#1049,
  https://github.com/synnaxlabs/foundation/issues/1049#issuecomment-6031034971).
  `Unaligned` says that the header block that the open takes holds a tail off a block
  boundary, and gives that tail. An open that gives `Unaligned` also leaves the ring as
  read. The open does not check the tail of the other block. `Invalid` names a record
  only: the first record of a ring is at offset 0. Lost: the tail in `Unfit`, which is
  also the error of `Layout::new`, where a tail has no value. Decided by the architect
  (#1093, https://github.com/synnaxlabs/foundation/issues/1093#issuecomment-6031034712).
  The restart record needs one free block: an open of a full ring first frees its oldest
  records (STORE TRIM). The writer keeps in memory the boundary after each synced record
  past the tail (its offset and chain value, 16 bytes, at most one for each block of the
  area), so a trim finds its new tail with no read of the ring. The walk holds one pool
  block at a time and reads a longer record in pieces of the pool's largest block, so
  the pool puts no bound on `body_max`.
  An open with no such block free fails with `Pool`, and the next open recovers the
  record (#440, #572).
  Ring header: `[magic: 8][version: u16][area: u64][body_max: u32][tail offset:
  u64][tail chain: u32][seq: u64][crc32c: u32][zero padding]`, one 4096-byte block,
  magic `FNDNRING`, version 1. The CRC is at offset 42, right after the fields, and
  covers the rest of the first 512-byte sector, so a checkpoint is in one sector and a
  crash keeps it whole or old. The magic, the version, and the place of the CRC are the
  same in every version, and a later version puts its fields after the CRC in the same
  sector, so an older build reads a newer block and reports its version. A decode does
  not read bytes past the first sector. Two zero blocks are a ring made and not yet
  written; a zero first sector with other bytes in the block is not a ring. Two blocks
  at the start of the ring hold the last two checkpoints: checkpoint `n` goes to block
  `n mod 2`. The second block is for a fault that no crash makes (a bad sector, a stray
  write), not for a torn write. A crash in the first write can keep only block 1; the
  ring then has one copy of the checkpoint until checkpoint 2. Open takes the whole
  block whose `seq` comes after the other's, wrapped as the writer wraps it (on a tie,
  the first). No block with the magic: not a ring. Both with the magic and a wrong CRC:
  the ring is lost.
  The header with a new tail is durable
  before the writer releases the space, so the header's tail is at or before the
  writer's tail and the records between are whole. The layout comes from the header
  at open; configuration sets it at create, and a changed `body_max` takes effect at
  the next create. The open reports the effective layout, and the node shows it in
  status. `append` refuses a batch that no one record holds (over 1023 entries or
  parts, or a body over `body_max`) with `Large`, and never splits a batch over
  records. It also refuses with `Large` a batch with an entry whose parts, joined,
  pass the largest block of the shard's pool (`Limit::Block`), because a read gives
  each entry in one block (#968). An open fails with `Pool(TooLarge)` when a
  recovered entry passes that block, as after a restart with a smaller budget; a
  larger pool opens the ring. An open of a header with a smaller `body_max` fails
  with `Unfit` (#627).
  `Layout::entry_max` is the most bytes of parts that `append` takes in a batch of
  one entry, at least `Layout::ENTRY_MAX_MIN` (4032); a batch of more entries holds
  less. `Layout::check` gives the `Limit` that `append` would refuse a batch with,
  from its counts of entries, parts, and bytes, so the home checks a frame before it
  takes the blocks of its entries (#795). It does not check `Limit::Block`, which
  depends on the pool. An entry has no part, one, or two; `append` takes them owned
  and drops them when it fails (#582).
  `Layout::fit(len, body_max)` gives the largest ring whose file holds at most `len`
  bytes, or `Small { len, min }` in file bytes, so a caller never learns the area
  unit. Its `body_max` bounds panic, from the one check that `Layout::new` uses, and
  `body_max` comes from a constant of `node`, never a config value; if it ever does,
  the panic becomes an error first. Decided by the architect on #1166:
  https://github.com/synnaxlabs/foundation/issues/1166#issuecomment-6032394297.
  A new ring has the same block at `seq` 0 in both places, with the tail at offset 0 and
  a random chain value. A ring file with no checkpoint (an empty file, or two zero
  header blocks) holds no record, because an open syncs the first checkpoint before it
  writes a record. A crash before that sync leaves such a file. An open removes it and
  makes the ring again with `Config::layout`, then syncs the directories and writes the
  first checkpoint, so a ring with no checkpoint takes the layout of the open and a ring
  with one keeps its own (#1254). The open holds its write handle of the old file
  through the remove, so of two opens at once one gets `Busy`; an `os` open can still
  take a removed file, which `os` is to close (#1297). An open that finds no ring while
  other opens of the directory run can get an error from its create, and each commit
  stays: `Files(Length)` when another open made a ring with another layout, and
  `Files(Full)` on a disk with room for one ring when another open removed a ring with
  no checkpoint and has not yet synced the directory. The open does not look again,
  because no caller opens one directory two times at once. A dropped open can leave its
  remove in flight, and a later open of the same directory in the process can lose its
  ring (#1441). It syncs the directory before each create of a ring, because a disk
  gives the room of a removed file back only then, and a kill after the remove leaves
  such a file: the remake needs room for the larger of the two files, not for both.
  `Length` stays for a ring with a checkpoint whose length does not fit its header, and
  for a file that is not empty and ends inside its header blocks. A crash at any point
  of the remake leaves a ring that the next open takes, or no ring. Lost: fit the layout
  to the length of the file (a ring whose size no config gave), and keep the file when
  its length fits (two paths for one case). It does not wait for `File::resize` (#1238).
  Decided by `laptop.architect` (2026-10-07T07:47:16Z):
  https://github.com/synnaxlabs/foundation/issues/1254#issuecomment-6033434001. The held
  handle, the sync before the create, and the `Length` text: decided by
  `laptop.architect` (2026-10-07T09:08:16Z):
  https://github.com/synnaxlabs/foundation/pull/1286#issuecomment-6034721207. The
  deferral of the dropped open to #1310: decided by `laptop.architect`
  (2026-10-07T09:34:29Z):
  https://github.com/synnaxlabs/foundation/pull/1286#issuecomment-6035164599. The open
  that gets an error from its create: decided by `laptop.architect`
  (2026-10-07T10:59:02Z and 2026-10-07T11:18:03Z):
  https://github.com/synnaxlabs/foundation/pull/1286#issuecomment-6036483605 and
  https://github.com/synnaxlabs/foundation/pull/1286#issuecomment-6036799415.
  A `Commit` answers for the entries appended before its call, and nothing else. Held
  past the drop, it resolves once the task ended: with `Ok` when those entries are
  durable, else with the error that ended the task. A caller that needs each entry
  durable before the drop calls `committed` after its last append. Lost: the error of
  the task to each `Commit` held past the drop, a second meaning only after the drop.
  Decided by `laptop.architect` (#1234, 2026-10-07T07:06:07Z):
  https://github.com/synnaxlabs/foundation/issues/1234#issuecomment-6032824731.
- **INDEX FRAMES (#191)** The home makes one index frame for each present group with
  samples of a write: the writer's key set with only that group present, its range,
  and its encoded series. The home stores it, keeps it as the index's newest frame,
  and later gives it to readers. B7, the log, the seq, and reader positions are per
  index. A write with more than one present group pays one copy of its series into
  the index frames. Decided by the `write-path` builder; approved by the coordinator
  (#191). A group with no samples gets no index frame and no data entry. It still
  records its handoff (or keeps it waiting when it finds no room), renews its lease,
  spends a seq range of zero, and is applied, also when another group of a live write
  is lost or its own handoff finds no room in a live write. A backfill write with no
  room gets `Full` whole (B5). A write with no samples still reports a failed commit.
  Decided by the coordinator with the advisor at 2026-10-06T15:30:26Z (#885):
  https://github.com/synnaxlabs/foundation/issues/885#issuecomment-6019665440
  A group with no samples is confirmed with the entries appended before it. It appends
  no entry of its own and moves no stored mark, but a handoff that it records does. A
  lost range is durable only when a later live entry of its index, with samples or a
  handoff, is on disk. A restart before that continues the index at the lost range's
  first seq. Supersedes the confirm rule of
  https://github.com/synnaxlabs/foundation/issues/885#issuecomment-6019665440.
  Decided by `laptop.architect` (#1347), with the live write and `Full` text above,
  in three comments (2026-10-07T11:46:56Z, 11:47:17Z, and 11:52:25Z, in this order):
  https://github.com/synnaxlabs/foundation/pull/1347#issuecomment-6037240549
  https://github.com/synnaxlabs/foundation/pull/1347#issuecomment-6037245942
  https://github.com/synnaxlabs/foundation/pull/1347#issuecomment-6037325066
- **STORED BODY (#191)** The bytes of a data entry (S4) are `[count: u32]`, then
  `[channel: u128][kind: u8][element: u8][n: u32][end: u32]` for each present series
  of the index frame in entry order, then the frame's encoded series bytes,
  little-endian. `end` is as in FRAME LAYOUT. Kinds: scalar 0, array 1, list 2, string
  3, bytes 4, matrix 5. `n` is the array length, the list maximum, or `rows | columns
  << 16` of a matrix, else 0. `element` is the scalar (bool 0, i8 1, i16 2, i32 3, i64
  4, u8 5, u16 6, u32 7, u64 8, f32 9, f64 10, stamp 11, span 12, uuid 13), else 0. The
  type is the writer's type, so a reader decodes with it after an `apply` changes the
  channel's type (A15). The header is one pool block and the series bytes are a view of
  the frame's block, so a write copies no series byte. A slot or key set number is never
  stored. The layout is part of the disk format version (C9d), as in FRAME LAYOUT. Copy
  mode checks each stored body once where remote records enter (X43), and the read after
  it panics on a bad body. Decided by the `write-path` builder; approved by the
  coordinator (#191).
  Amended: kind 5, with both `u16` sides in `n`, so the descriptor stays 26 bytes and
  a body with no matrix is as before. Decided by `laptop.architect`
  (2026-10-07T17:30:55Z):
  https://github.com/synnaxlabs/foundation/issues/1341#issuecomment-6043244011.
  Supersedes the `columns` table of
  https://github.com/synnaxlabs/foundation/issues/1341#issuecomment-6042293625.
- **STORED BENCH (#1547, 2026-10-07)** The cargo feature `sim` of `home`, off by
  default, adds `#[doc(hidden)] pub mod bench`: `entry` calls `stored::entry`, and
  `read` calls `stored::read` and gives each series' channel, type, and bytes. Only the
  bench `benches/stored.rs` (`test = true`) uses it. `read` gives all three fields, so
  the compiler cannot skip a decode that production does, and the bench passes each item
  to `divan::black_box`. Run it with `cargo bench -p home --bench stored`. Lost: a copy
  of `stored` in the bench through `#[path]`, which breaks at its first `crate::` item,
  and a time of `Shard` writes and reads, which hides the cost of the body in the cost
  of the write. Decided by `laptop.architect` (2026-10-07T18:46:13Z):
  https://github.com/synnaxlabs/foundation/issues/1547#issuecomment-6044535576.
  `cargo bench -p home` turns on `sim` through a dev-dependency of `home` on itself,
  since the bench host runs no features. Decided by `laptop.architect`
  (2026-10-07T19:05:26Z):
  https://github.com/synnaxlabs/foundation/issues/1547#issuecomment-6044862850. Amended
  by `laptop.architect` (2026-10-08T01:01:28Z):
  https://github.com/synnaxlabs/foundation/pull/1568#issuecomment-6049989224. The
  feature is `sim`, not `bench`, since the feature says that the module is test-only,
  and the module keeps the name `bench`, since it says what the module serves.
  Supersedes the feature name of
  https://github.com/synnaxlabs/foundation/issues/1547#issuecomment-6044535576 and
  https://github.com/synnaxlabs/foundation/issues/1547#issuecomment-6044862850.
- **NODE BENCH (#1637, 2026-10-07)** The cargo feature `sim` of `node`, off by default
  (`node`'s dev-dependency on itself turns it on for the bench), adds `#[doc(hidden)]
  pub mod bench` with `Scope { new, spawn }` and its `Default` over `scope::Scope`. Only
  the bench `benches/scope.rs` (`test = true`) uses it. Its `env::tasks::Driver` keeps
  each task, `spawn` gives it, and the bench polls it by hand, so a time holds only
  `Spawned::poll` and the future's poll. A `bare` line polls the boxed future directly
  in the same binary, as the control. Lost: a time through `Node::spawn` on `sim` or
  Tokio, which hides a 0.3 ns change in the executor's cost, and a copy of the poll
  before `clone_from`, which #1627 decided and the `bare` control replaces. The
  `same_waker` time is the check on `clone_from` until #715 gates it with a baseline
  from the form with `clone_from`: an `Arc` waker clone allocates nothing, so no
  allocation count can. Decided by `laptop.architect-2` (2026-10-07 23:56 UTC):
  https://github.com/synnaxlabs/foundation/issues/1637#issuecomment-6049244976; the
  surface of `spawn`, in round 1 of #1666 (2026-10-08 00:11 UTC):
  https://github.com/synnaxlabs/foundation/pull/1666#issuecomment-6049426380. The
  bench's `Driver`, beside those of `os` and `sim`, is the #1632 clause, by
  `laptop.architect-2` (2026-10-08 00:18 UTC):
  https://github.com/synnaxlabs/foundation/issues/1632#issuecomment-6049501422. #1632
  applies its text to the doc of `env::tasks::Driver` and to ENV SEAMS. The feature is
  `sim`, since a hook that only a bench or a fuzz target uses is test-only (#1570), by
  `laptop.architect-2` (2026-10-08 00:56 UTC):
  https://github.com/synnaxlabs/foundation/pull/1666#issuecomment-6049939448. It
  supersedes
  https://github.com/synnaxlabs/foundation/issues/1637#issuecomment-6049244976 in its
  clause that the feature is `bench`.
- **HANDOFF RECORD (#191)** The home records each handoff that `Gate::handoff` gives
  (GATE RULES) as a buffer entry on the live path of the index, with tag `HANDOFF`,
  `len` 0, and `first` at the live tail. It records a handoff after the gate input that
  gave it and before the next input or frame. Its bytes are empty when no writer holds
  control, else `[authority: u8]` then the holder's subject as UTF-8; the entry length
  gives the subject's length. A restart or a failover starts the gate from the last
  record (X18): `Gate::recover` with its holder, or `Gate::new` when it names none.
  Trimming must keep the last record of each index (#406). Until it does, a trim
  (STORE TRIM) can free that record, and a holder whose record a trim freed gets no
  grace after a restart. Retention deletes nothing (decided by `laptop.architect`,
  2026-10-07T12:30:53Z and 2026-10-07T12:59:37Z:
  https://github.com/synnaxlabs/foundation/issues/1377#issuecomment-6037946637 and
  https://github.com/synnaxlabs/foundation/issues/1377#issuecomment-6038431739).
  Supersedes https://github.com/synnaxlabs/foundation/pull/402 in its clause that
  retention can remove that record. The layout is part of the disk format version (C9d).
  Copy mode checks each record once where remote records enter (X43), and the read after
  it panics on a bad record. Decided by the `write-path` builder; approved by the
  coordinator (#191).
- **ENTRY TAGS (#191)** Each entry of an index log has a tag (S4) that says what its
  bytes hold: `DATA` 0 (STORED BODY), `HANDOFF` 1 (HANDOFF RECORD). A new kind of
  record takes the next free value here. The buffer does not read the tag.
  Decided by the `write-path` builder; approved by the coordinator (#191).
- **LARGE FRAME (#191)** The home refuses a write whose bodies no record of the ring or
  no block of the shard's pool holds, on either path, with `Large`. The pool bound is on
  each entry's parts joined, the home's header part included (#968). No seq moves and
  the home stores no part of the frame. The waiting handoffs of the frame's indexes are
  still recorded (HANDOFF RECORD). The writer splits the frame by samples or by indexes
  and writes each part. The home never splits a frame, because a frame applies whole
  (B7). Each handoff goes in its own append, so a handoff never makes a frame large. The
  size is checked only when the bodies are appended, after the handoffs: a frame whose
  handoff finds no room is lost (live) or refused with `Full` (backfill) before its size
  is known. Decided by the `write-path` builder (#191). After a failed commit, the home
  gives `Disk` before it checks the size, so a frame that no record of the ring or no
  block of the pool holds gets `Disk`, not `Large`, on either path (#1260). Decided by
  `laptop.architect` (2026-10-07T09:21:07Z):
  https://github.com/synnaxlabs/foundation/issues/1260#issuecomment-6034929252.
- **HOME CLOCKS (#191)** A shard reads monotonic time and mesh time itself, from the
  `clock::Reader` in its `Config`, in each call that needs them. One
  `clock::Reader::now` gives both at one instant, so a control lease and a stamp check
  in one call see the same time, and a lease never compares readings of two clocks
  (approved by the coordinator on 2026-10-06, #964). Before the node first has mesh
  time, it opens no writer, with `writer::Error::Unsynced`. A write needs an open
  writer, so it never meets that case. A reader opens with no mesh time: its open and
  its close take no stamp (#1024; decided by the architect, #963). This is a patch: #523
  decides where samples wait before the first estimate (CLOCK PEER ANSWER), and removes
  or keeps `Unsynced`. Lost: time as arguments of each call, because each caller repeats
  the same two reads and can pass an old one. Approved by the coordinator on 2026-10-05
  (#191). Mesh time in the home (the ahead limit and the stamp of each entry) is the
  midpoint of the mesh time of `clock::Reader::now`, which never goes back. Lost: the
  latest edge, because it goes back when the error shrinks, and with an unknown error
  (OS CLOCK BOUND) it is 36500 days ahead, so the ahead limit stops nothing and one bad
  stamp makes each later true stamp `Backwards` (#952 review, 2026-10-06).
- **HOME SURFACE (#963)** The public surface of `home` names only `types`, `env`,
  `codec`, and `home` items, apart from two. `Config`, which only `node` builds, names
  `buffer` and `clock` types. `Shard::pool` gives a `block::Pool`, the pool of the
  shard's buffer. `block` is in the `hub` row. A writer's frames come from that pool,
  so `hub` takes no pool of its own and the two cannot differ (architect,
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6031955051). `Config`
  takes no pool: the shard uses `Buffer::pool()`. It takes one `clock: clock::Reader`
  for monotonic and mesh time. The shard is the only writer of the buffer in `Config`:
  the caller gives it with no entry that waits for a commit. The condition is stated,
  not checked: `node` appends nothing before `Shard::new`, and `Config` takes the
  buffer by value, so no later append can come from outside (architect,
  https://github.com/synnaxlabs/foundation/pull/1130#issuecomment-6033691871; lost: a
  check in `Shard::new`). `replica` (X13) and copy mode (X43) are out of the MVP. Their
  PR decides how `replica` gets to the buffer of a shard and what `committed` waits for.
  Until then, the shard is the only writer (architect,
  https://github.com/synnaxlabs/foundation/pull/1130#issuecomment-6034204295).
  `home::Error` holds only what `write` gives, and each other call has its own error.
  Conversions from `control` errors are private. The `hub` row stays as it is. `Shard`
  gives no stored seq until a caller needs one (architect review,
  https://github.com/synnaxlabs/foundation/pull/1130#issuecomment-6031908363). Lost:
  `control` and `delivery` in the `hub` row, because `hub` then knows how the home is
  built and a `control` change becomes a `hub` change. Lost: no call surface, with
  requests through a ring, because on one shard a call costs nothing and a message costs
  a copy and a wake, and it adds a second protocol beside `wire`. Lost: one reader key
  and a panic at a grant to a latest reader, because the precondition is not in the
  type. Lost: one `home::Error` for every call, because `write` would list `Unsynced`
  and `Lease`, which it never gives. The plan has the full text
  (https://github.com/synnaxlabs/foundation/issues/963#issuecomment-6022924709). Decided
  by the architect, #963
  (https://github.com/synnaxlabs/foundation/issues/963#issuecomment-6031464116).
- **HOME EVERY TYPE (#1145)** `Shard::open_writer` takes a key set with series of any
  `sample::Type`, and the home writes and reads a series of each: `codec` checks and
  encodes it as S3 says, and STORED BODY stores its type. `codec` does not check that
  a `String` sample is UTF-8 (#556). Neither `home` nor `hub` has a
  `writer::Error::Type`. Supersedes HOME TYPE REFUSAL (#963,
  https://github.com/synnaxlabs/foundation/issues/963#issuecomment-6031702785), the
  patch that refused a series of a type other than a scalar until this change. Lost:
  an allow list in `home` that grows one type per PR, because it copies the list that
  `codec` owns; a variant that no path gives, because it misleads each caller that
  matches on it. Decided by `laptop.architect` (2026-10-08T06:36:33Z:
  https://github.com/synnaxlabs/foundation/issues/1145#issuecomment-6053997861). The
  surface was approved by `laptop.architect` (2026-10-08T07:01:09Z:
  https://github.com/synnaxlabs/foundation/pull/1824#issuecomment-6054411394).
- **HUB SESSIONS (#1133)** `hub::reader::Reader::next` yields once after 128 frames in a
  row: it wakes its own task and returns `Pending`. So it yields under `sim` as under
  `os`, and `hub` does not depend on Tokio. Lost: the Tokio coop budget, which does
  nothing outside a Tokio runtime. A complete session that misses a frame (one that
  still waits for credit when the next commit with frames of its index is released,
  CREDIT RULES) gets no later frame, as there is no catch-up from the buffer yet. The
  director chose that `delivery` reports the miss and wakes the session, and that `next`
  then ends with an error (2026-10-07T06:01:39Z:
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6032004737). So
  `delivery::Readers::release` also names a session that missed a frame and has none
  waiting, and `Readers::take` gives `Next::Behind` after the frames before the miss.
  `home::Shard::take` gives `Next::Behind` until #274. `next` gives a waiting frame,
  then `Ended::Behind`, then `Ended::Buffer`. Lost: an error from `take`, which every
  caller, latest readers too, then handles; a `behind` list beside the woken keys, a
  second list to drain for an event that happens once per session. A `delivery` model
  property test and a 32-run `sim` test stand in for loom and shuttle: the wake never
  crosses a thread. Decided by `laptop.architect` (2026-10-07T06:36:57Z:
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6032442901). `take`
  gives `delivery::Next` (`Frame`, `Empty`, or `Behind`), and `Readers::behind` and
  `Shard::behind` go. Supersedes
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6032442901 in its lost
  design "an error from `take`": a third case is not an error, each caller of `take`
  uses one path for both modes, and #274 may give a gap from `take`. Lost: `woken` names
  a reader that is behind in a second list, which keeps the two steps and moves the
  state into the hub. Decided by `laptop.architect` (2026-10-08T01:43:19Z:
  https://github.com/synnaxlabs/foundation/issues/1718#issuecomment-6050444671). A
  waiting hub reader has given back every frame, as it grants at each `next` call, so no
  hub test reaches the wake of a session that missed a frame with none waiting. The
  `delivery` tests and the home `sim` test
  `names_a_complete_reader_once_when_it_misses_a_frame_with_none_waiting` reach it, and
  `does_not_name_a_complete_reader_that_misses_a_frame_while_one_waits` checks that a
  miss while a frame waits gives no wake. The hub `sim` test
  `gives_a_waiting_complete_reader_each_frame_of_a_commit_past_its_window` checks that
  one commit of about three windows wakes a waiting reader, which gets each frame, with
  no hang. The hub `sim` test
  `ends_a_complete_reader_after_its_waiting_frames_when_it_misses_a_frame` checks that a
  reader that takes no frame until a second commit past its window ends gets each frame
  before the miss, then `Ended::Behind`. Decided by `laptop.architect`
  (2026-10-08T07:23:30Z:
  https://github.com/synnaxlabs/foundation/issues/1170#issuecomment-6054827276).
  Supersedes https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6033084119.
  After a warmup, a write and `next` make no heap allocation, while frames wait, while a
  reader waits, and in the write that wakes a latest reader, which a counting allocator
  test binary checks (COUNTING ALLOCATOR); it does not count the commit task. `next`
  gives a `types::frame::View` of the reader's channels and their index (M2), never the
  frame. The view borrows the reader, which releases the frame at the next call, not at
  its first poll, and grants credit for it there (CREDIT RULES): `next` is a plain `fn`
  that returns a future. A caller that keeps data copies it. A session that ends gives
  `reader::Ended`. `Hub::define` stands.
  Decided by `laptop.architect` (2026-10-07T05:53:24Z:
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6031908575;
  2026-10-07T05:57:18Z:
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6031955051; and
  2026-10-07T07:12:40Z:
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6032912929). The
  surface was approved by `laptop.architect` (2026-10-07T14:53:11Z:
  https://github.com/synnaxlabs/foundation/pull/1133#issuecomment-6040585795).
  Amended (2026-10-08T00:13:26Z, #1625): `reader::Session` is the home's side of a
  reader, which `Reader` drives, and which `Hub::serve` (#1636) will drive. The split
  costs `latest next` +1 ns per frame (16 against 17 ns net on a quiet host), which
  adds 0.3% to the write of one frame. Accepted by laptop.architect:
  https://github.com/synnaxlabs/foundation/pull/1625#issuecomment-6049444882.
  A doc states what is true at its commit: `Reader` states no credit window, as a
  latest reader has none, and `Session` names only `Reader` as its driver. #1636 adds
  each stream of a remote reader when it adds that driver (laptop.architect,
  2026-10-08T01:01:26Z,
  https://github.com/synnaxlabs/foundation/pull/1625#issuecomment-6049988923).
- **HUB END (#585)** The hub's commit task holds the hub's state weakly, and keeps its
  waker in the state while it sleeps and while it waits for a commit. The state wakes
  it on drop, and the task ends at its first poll after that. Lost:
  `Hub::close(self) -> Commit`, which each caller must call, and which a clone or a
  live session defeats. Decided by `laptop.architect` (2026-10-07T18:07:55Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6043897000).
  The commit that the task waits for lives in the state, and the task polls it through
  the state. So the drop of the state drops the commit in the same call. Once the hub
  and each of its sessions drop, the hub holds no part of the home: no `Home`, no
  `Commit`, no `Reading`. A task that the hub spawns holds a part of the home only
  through the state or a session. `node` takes its own commit before it gives the home
  to the hub, drops the hub and each session, awaits the commit, which resolves once the
  buffer's task ended, drops it, and then lets go of the data directory lock. Lost: a
  future of the end of the task, one more step for each caller; and an order in `node`,
  which cannot know what the hub holds. Supersedes: "So the task ends, and drops the
  commit it waits for, at its first poll after the hub and each of its sessions drop"
  (https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6043897000).
  Decided by `laptop.architect` (2026-10-07T21:23:22Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6047128783).
- **BQ9** Re-index by changing `index` in the files. The old home seals the channel at
  its last accepted sample and records the seal with voters (which region: X39). The
  history "index A until T, index B from T" is runtime state in `mesh`; the spec keeps
  only the current index. Readers' `hub` joins the spans. No data moves. A connector
  rate change is not a re-index.
- **RE-INDEXING REQUIREMENT** A re-index keeps key and name. Stored data keeps its
  original timestamps. Readers see one continuous channel.

### 1.5 Memory and performance

- **P1** Targets: 100M numeric samples/s per node (disk buffer plus one complete
  reader); within 2x at 100k channels; latest-mode p99 under 250 us over one encrypted
  LAN hop; under 4 bytes per sample for typical sensor data, timestamps included;
  Raspberry Pi 4 with 1 GB: idle under 50 MB, start under 1 s. A regression over 5% on
  the dedicated machine needs a written judgment before merge. The judgment states how
  often the path runs (per sample, frame, session, or start), its absolute cost against
  the P1 budget, the noise of the machine, and what the change buys. The architect
  accepts or rejects it on those facts (the person, 2026-10-07: "YES"). The person
  decided on 2026-10-06 (#1047): "we need to make sure that we semantically understand
  benchmarks. A regression of 11% can be ok in the right contexts". Supersedes: a
  regression over 5% blocks a merge.
- **M1** Node-local u32 `channel::Slot`s. Each writer session gets an interned key set
  (slots, keys, and types, R9-D1). Frames point at the key set id. Supersedes: S1
  frame struct. Approved by the coordinator (#390).
- **M2 (revised 2026-10-06)** Readers get a view: the frame plus a mask cached per
  key set and reader. The home routes by key set. A mask holds the index of each
  channel it holds, so the series of a view make a frame. A mask is a sorted list of
  the entries it holds, or no list when it holds every entry. A view's walk grows with
  the smaller of its frame's series and its mask's entries (rule 11). A view borrows
  its frame and mask, so making one takes no reference count. Approved by the
  coordinator (#157). A view has no charge: a remote reader charges the frame it builds
  (FRAME LAYOUT), so `View::charge` and the list of the entries a mask leaves out,
  which only the charge used, are gone (architect, #1068:
  https://github.com/synnaxlabs/foundation/issues/1068#issuecomment-6032304827 and
  https://github.com/synnaxlabs/foundation/pull/1216#issuecomment-6032799083).
  Supersedes: the list of the entries left out (#755, the gate of PR #873).
- **M3 (revised 2026-10-05)** One pool block per frame: a header (key set key, form,
  path), a range for each present index group, a descriptor for each present series,
  and series bytes back to back. Ranges are sorted by group and descriptors by entry.
  An entry is present when it has a descriptor, so the cost of a frame grows with the
  series it holds, not with the width of its key set (rule 11). The "presence mask" of
  S7 and X30 is this list. A series is a slice. One refcount per frame. Connectors
  write straight into `hub.block(len)`. The person decided on 2026-10-05 ("Sorted
  lists"), #187, in place of a presence mask over entries. Supersedes: S2 per-series
  buffer.
- **M4** Each shard owns a pool that `node` injects. A release returns the block to the
  owner shard. No global pool.
- **M5** Blocks hold offsets, never pointers.
- **FRAME LAYOUT (refines M3, X8)** `types::frame`, little-endian, offsets from the
  start of the payload. A 16-byte header: key set key, range count, and descriptor count
  (each u32), then form and path (each u8), then zeros. The path is live 0 or backfill
  1, the path whose seq the ranges count on (A8).
  Then `{ group: u32, count: u32, seq: u64 }` for each present group, sorted by group,
  then `{ entry: u32, end: u32 }` for each present series, sorted by entry, then the
  series. `end` counts from the start of the series bytes. Each series starts at the
  end before it rounded up to 8, and the first at 0. A group is present when its index
  is, and a present series needs its index. A lookup by entry or group is a binary
  search, and a pass in entry order reads each descriptor once. The header holds no
  entry or group count: an entry or group past the key set is absent. A frame is at
  most `u32::MAX` bytes. The series bytes are stored and sent as they are (X35), so
  their order and padding are part of the disk and wire format version (C9d). A change
  to either needs a new version. `Layout::draft` writes zeros in the padding of a frame
  from lengths. A frame from ends gets its padding from `Draft::body_mut`, which the
  caller fills whole (decided by the architect, #1246, 2026-10-07T15:17:43Z:
  https://github.com/synnaxlabs/foundation/issues/1246#issuecomment-6040879844).
  Supersedes the zero padding of every draft in
  https://github.com/synnaxlabs/foundation/pull/1064#issuecomment-6031091642. No
  reader reads the padding, so `frame::check` does not check it. A frame from a peer may
  hold other bytes there, which `replica` stores and copy mode (X43) sends as they are
  (decided by the architect, #1064:
  https://github.com/synnaxlabs/foundation/pull/1064#issuecomment-6031091642, worded in
  https://github.com/synnaxlabs/foundation/pull/1064#issuecomment-6031226370). The
  padding is at most 7 bytes for each present series: at most 1% of encoded bytes at
  1024 samples, and up to 34% at 10 samples (measured on #317). `frame::split` cuts a
  body at its `(tag, end)` pairs and panics on ends that do not fit. Copy mode runs
  `frame::check` once where remote records enter (X43). Decided by the coordinator
  (#306). A frame from another node is built from its ends (HUB WIRE):
  `Layout::from_ends` checks them before a block is taken, and `Draft::body_mut` takes
  the body as it arrives. `frame::ends` gives the ends of series of given lengths, so
  the rule of 8 has one home. Both nodes charge such a frame with
  `frame::charge(series, body_len)`, one function on each side, so the charges are
  equal by construction; a `Layout::charge` would be a second way. Decided by the
  architect (#1068:
  https://github.com/synnaxlabs/foundation/issues/1068#issuecomment-6031655359 and
  https://github.com/synnaxlabs/foundation/issues/1068#issuecomment-6032304827).
- **MEMORY BOUNDS** A hard pool budget per node. Pools reserve address space, commit
  pages lazily, and purge after idle. Credits cap the blocks a reader can pin, apart
  from the frames of the last release of each index that wait for a grant (CREDIT
  RULES). A reader that falls behind is served from disk. When the pool is full, a live
  write records a gap and backfill waits. The current value of B4 pins one block per
  index that had a live frame, with no reader open and no cap. A smaller copy is a 5.3
  tunable. The person decided on 2026-10-05: "Accept it" (#139). The budget counts what
  stays resident. A purge of a block smaller than a page gives no page back, so it frees
  no budget. When an allocation finds no room, the pool gives back the whole carved
  range and budget of size classes whose carved blocks are all free, until the
  allocation fits. Under this pressure, at most two partial pages per class stay
  resident, and `Config::budget` states that slack. An idle class that no allocation
  presses keeps its pages until the purge after idle. A class that a reader keeps partly
  in use keeps its budget. The person accepted this (design H) on 2026-10-05 ("Ok
  fine"), #2, #270. Purges per block that give back every page they credit (design P)
  wait in a follow-up issue. When the system refuses to commit pages, the pool gives
  back one idle size at a time, in the order a purge for room in the budget uses,
  and tries the commit again; after the last idle size the allocation fails with
  `Error::Refused`, a separate error from a full pool (the person on 2026-10-05: "I
  approve the separate error"). The carve counts do not change, the sizes given back
  stay given back, and a later allocation may succeed (#475, #542).
- **R9-D9** Atomic refcount. `Unique` is writable; `Block` is immutable after freeze. No
  copy-on-write.
- **Performance rulebook** Rules 1 to 14 bind every implementing agent, the performance
  agent, and every adversarial reviewer.
- **C2 (2026-10-05)** One shard per core owns its indexes with no locks. Each shard
  runs one Tokio `LocalRuntime`. Vendor libraries run on dedicated threads. A
  connector's shard is chosen by its index (R12-8). Network data and vendor-thread
  frames reach the owning shard as one batch with one wake per batch. On Linux the
  parked wake adds 4 to 9 us per batch with no millisecond tail (#9, r1). The spin
  window stays a per-node setting with a default of 0; the sweep in 5.3 sets it. The
  person locked it on 2026-10-05 ("Ok I approve lcoking C2").

### 1.6 Time

- **C6 (as revised by R6 TIME LOCKED and TIME ADAPTERS)** Each node keeps a mesh clock:
  the OS clock plus a measured offset, with an error interval (earliest to latest).
  It never steers the OS clock unless that is opted in where privileged. All timestamps
  are in mesh time; connectors convert device time. Each node publishes
  `<node>.clock.offset` and `<node>.clock.error`. Zero time config by default: sources
  are detected, and the clock follows the smallest measured bound, with no fixed
  ranking. Supersedes: C6 fixed source choice (X36). Amended by ESTIMATE COMBINE: the
  clock follows more than half of the bounds, not the smallest one (#344).
- **R6 TIME LOCKED** Own sans-I/O estimator over our transport. The bound is half the
  round trip. Keep the fastest exchange per source, combine sources, widen the bound
  with drift, slew only. Sources are read directly: mesh peers, GPS, PPS with NMEA, the
  NIC hardware clock (kept by ptp4l), and the OS daemon. No PTP client in v1. Device
  clock fitting (DAQmx, LabJack) is a connector-library component that writes residual
  error to the index's error channel. Amended by MESH SLEW: mesh time also steps
  forward when it is more than 500 us behind every offset an estimate allows.
- **TIME ADAPTERS** Neutral model `Measurement { at: local monotonic, offset, error }`.
  The estimator never knows what a source is. Each source is an adapter with its own
  loop. `node` builds the source table. Adapters probe for hardware and privileges. The
  same estimator serves device clocks in the connector library. Amended by ESTIMATE FIT:
  a device clock gives `Overlap` readings with a low edge, a high edge, or both, not
  `Measurement`s.
- **ESTIMATE COMBINE (2026-10-04)** A `Measurement` is about one local clock (the node's
  monotonic clock, or a device's sample clock in nanoseconds, #84): its offset is mesh
  time minus the local reading at `at`, and its error is a half-width from 0 to 36500
  days. A bound grows by the drift bound times the time from `at`, in both directions.
  The drift bound is at most 10%; `Drift::UNDISCIPLINED` is 200 ppm. Each source keeps
  its last 8 measurements and offers the one with the smallest bound now. This reads R6
  TIME LOCKED's "keep the fastest exchange" with drift: an old fast exchange loses to a
  fresh slower one. `combine` takes one `Filter` per source and returns the hull of the
  offsets inside more than half of the bounds that vote. A known result holds the true
  offset when more than half of the bounds that vote hold it, whatever the other bounds
  are. It can be wider than the narrowest bound, so C6's "follows the smallest measured
  bound" no longer holds. Cost: PPS at ±100 ns beside two peers at ±1 ms, all centered
  on the true offset, gives ±1 ms, not ±100 ns. Lost: the hull of the offsets inside the
  most bounds (Marzullo), and NTP's selection, which first tries the offsets inside
  every bound. When one lying source of three put a small bound inside the honest
  overlap, each followed the liar. A threshold that also counts the sources with no
  measurement lost too: beside two of them, it needs all three bounds of that case. The
  person decided on 2026-10-05 ("a is fine"), #344. Amends C6 and X36. `combine` fails
  when no offset is inside the bounds of more than half of the sources that vote.
  Decided by the `time` builder (#49). Each source votes: a source with no measurement
  agrees with no offset, and it votes beside the known bounds, or beside the unknown
  bounds when no bound is known. So before its first estimate a clock waits until more
  than half of its sources agree, and one source that answers first cannot set mesh
  time. The person decided on 2026-10-05 ("clock question si approved at whatever path
  you think"), #488. A device's readings
  go to the oscillator fit (`Overlap`), never to `combine`. Node
  sources keep `Filter`, not `Overlap`: a network exchange puts the true offset at about
  the same place in each bracket, so an overlap gains little, and a broken drift bound
  would stay wrong for the life of an overlap, not for 8 exchanges. Decided by the
  coordinator (#84). An error that grows past 36500 days stops at 36500 days ("unknown")
  and never fails, so a lone Windows node gets OS time as OS CLOCK BOUND says, when it
  holds no known estimate (CLOCK HOLDOVER). An error over 36500 days fails only in a new
  measurement: `Measurement::new` gives `None`. The person decided on 2026-10-05 ("Ok
  that's fine"), #225. In an `Interval` from
  `Measurement::interval`, "unknown" is a half-width of 36500 days, and the true time
  can be outside it. Decided by the `time` builder (#142). `combine` uses each bound
  with its full growth, so an "unknown" bound never cuts another. A bound of 36500 days
  at `now`, given or grown by drift, votes only when no bound is known. A vote for it
  lost: it turned a peer split into a wide estimate that no peer gave. The person chose
  this (OS CLOCK BOUND); counting a grown bound is from the `time` builder, approved by
  the coordinator (#314). A known bound votes at any width, so a wide one (a Linux
  bound of 15 s) can still turn a peer split into the hull of both sides. #314
  showed this case before the person chose. When only unknown bounds vote, the estimate
  is unknown too, at the center of the same hull. Approved by the coordinator (#437),
  with the hull of #344. When drift grows unknown bounds so that this hull spans more
  than 73000 days, no unknown estimate holds it, and its center can miss an offset that
  every bound holds. The estimate is then at the center of the offsets inside the most
  bounds. Decided by the `time` builder (#344). `Measurement::unknown(at, offset)` gives
  the "unknown" error, so a source never writes 36500 days itself: 1 ns less is a known
  bound, and it votes until drift grows it to 36500 days. Approved by the coordinator
  (#144). An exchange with an error over 36500 days gives an unknown measurement,
  centered between its edges or at the nearest span, so no caller maps a failure to one.
  It cuts no known bound, because an unknown bound votes only when no bound is known. An
  unknown reading (`exchange::Reading::Unknown`) gives an unknown measurement, because
  two unknown readings sent as intervals whose centers move apart by more than the round
  trip, or one interval clamped at the end of the stamp range, can give a known bound
  (#930). An overlap whose readings allow an error over 36500 days before drift gives
  `None`, as an overlap with no edge does, because no caller needs an unknown device
  measurement yet. A device source can ask for one when it calls `Overlap::at`. Decided
  by the `time` builder (#258), and for the exchange approved by the coordinator (#903).
  Each function returns only the errors it can give: one `Error` per module (`overlap`,
  `combine`), and `Option` where a caller does the same for each cause
  (`Drift::from_ppb`, `Measurement::new`, `Overlap::at`). Decided by the coordinator
  (#272). `Exchange::measure` has one cause left, so it gives `Option` (#903).
- **BQ20** Wall time comes only from `clock`. Clippy `disallowed-methods` and the
  architecture agent enforce it.
- **R9-D13** The layer-2 crate is `clock`. `types::time` holds `Stamp`, `Span`, and
  `Range`. Supersedes: crate name `time`.
- **R9 keep list** `Stamp - Stamp = Span`; one format and parse grammar for spans,
  ranges, and ns ISO stamps.
- **TIME TEXT (#3)** A span is one number and one unit (`ns`, `us`, `ms`, `s`, `m`, `h`,
  `d`). Output uses the largest of `d`, `h`, `m` that divides the span, else the largest
  of `s`, `ms`, `us`, `ns` not more than the span, with a decimal fraction: `3d`, `90s`,
  `1.5s`, `250us`, `0s`. Input takes a decimal fraction and a leading `-` and rejects a
  value that is not a whole number of nanoseconds. A stamp is RFC 3339: output is UTC
  with nine fraction digits; input needs an offset, takes up to nine fraction digits,
  and rejects second 60. A range is the ISO 8601 interval `<start>/<end>`. A `Range`
  never ends before it starts (`Range::new` returns `None`), so its text always round
  trips; input rejects an end before the start. A byte size follows the span rules: one
  number and one unit with no space (`200GiB`), and a decimal fraction only when it
  gives whole bytes (`1.5GiB`). Its type lives in `types` beside `time::Span`, and the
  `document` reader is an adapter over it. The person decided on 2026-10-05 ("A yes I
  approve", #479). The units are `B`, `KiB`, `MiB`, `GiB`, and `TiB`, with exact case:
  `GB` and `Gb` are errors, because they mean other sizes. Input takes no sign. Output
  uses the largest unit that divides the size, with no fraction: `1.5GiB` is written
  `1536MiB`, and zero is `0B` (#505). The reader's `byte::Error` gives the data for a
  fix: where the unit starts, the unit a text likely means (`GiB` for `gib` or `GB`,
  none for `Gb`), and the largest size in the text's unit (#650).
- **ESTIMATE FIT (2026-10-04)** `Overlap` is the oscillator fit for one device clock. It
  keeps the offsets that every reading of that clock allows, each widened by drift, so
  it holds only the reading with the highest low edge and the one with the lowest high
  edge. Readings come in local time order. An older one returns `Backwards` (the device
  restarted), and one that shares no offset returns `Disjoint` (the clock jumped, or it
  drifts faster than its bound). Neither changes the overlap, and the caller starts a
  new one with a gap. Each reading holds true mesh time, with the node's own error in
  its bound, so the drift covers only the device oscillator. The drift is fixed for the
  life of an overlap, because a smaller drift would need readings that it dropped. This
  reads r6 Q5's lower-envelope fit with the rate bounded by `Drift`, not fitted. A line
  fit of offset and rate lost: it is honest only if the rate stays constant, and no
  datasheet bounds oscillator wander. A measured rate needs a signed rate in the model,
  not a smaller `Drift`. A reading gives a low edge, a high edge, or both, at one device
  time. A read return bounds the newest sample the host knows only from above. A low
  edge comes from a mesh stamp before the device acts (a start command or a request),
  from a device counter read between two mesh stamps, or from a latency that the
  hardware guarantees. The overlap gives a bound only when it has both a low edge and a
  high edge (`Overlap::at` gives `None` before that). Decided by the `time` builder; the
  person accepted it on 2026-10-05 ('#1 is fine'). The person accepted one-sided
  readings on 2026-10-05 ("Accept #133"). Supersedes: r6 Q5 method 1 (a fitted rate from
  read-return upper bounds).
- **CLOCK HOLDOVER (2026-10-05)** Before its first estimate, the clock is unsynced and
  a reader gets no mesh time. After it, when `combine` fails (no majority, or no sources
  after a remove), the clock holds over: it keeps its last estimate and its error grows
  by drift. It never follows the largest group or one side of a tie. `Reader::status`
  gives the status on any shard, and `node` publishes it. `push` and `remove` do not
  also return it: one value gets one way to read it (#634). The next majority ends the
  holdover, but after a known estimate only a known one does. Decided by the `time`
  builder (#142). The coordinator approved `Reader::status` within it (#598).
  `estimate::discipline` chooses what mesh time follows, and `clock` writes it, so the
  decision logic is in layer 1 (#635). An unknown estimate never replaces a known one:
  after a known estimate, when only unknown bounds agree, the clock holds over until a
  known estimate. The person decided on 2026-10-05 ("a is fine"), #489. Known is as
  `combine` sorts a bound: under 36500 days at the estimate's time. So when drift grows
  the held bound to 36500 days, the clock follows an unknown estimate. Decided by the
  `time` builder (#835).
- **MESH SLEW (2026-10-05)** After the first estimate, mesh time moves toward each new
  estimate at no more than 500 ppm (ntpd's maximum slew), in `estimate::Slew`. The part
  not yet applied goes into the error, so a slew of 1 s takes 2000 s and its error says
  so. When the offset served at `now` is more than 500 us (1 s of slew) below the
  earliest offset the new estimate allows at `now`, mesh time steps forward to that
  earliest offset. In every other case it slews. Mesh time never steps back. A clock in
  holdover keeps its slew. Cost: mesh time that is ahead still slews. After a stale
  first estimate that is ahead, or for an estimate with an unknown error (a Windows OS
  clock alone under OS CLOCK BOUND, whose earliest offset is 36500 days back), a
  correction of 1 h takes 83 days and one of 1 day about 5.5 years, with a true error
  the whole time. A majority of falsetickers more than 500 us ahead steps mesh time into
  the future, and it does not come back. That is outside the fault model. Lost: a
  frequency loop (a PLL, as in ntpd), because R6 bounds drift with an error that grows
  and a PLL can overshoot; the slew private in `clock`, because it is decision logic in
  layer 2; a step only when the estimate's whole interval is ahead of mesh time's,
  because when both bounds hold the intervals overlap and it never fires. Amends R6 TIME
  LOCKED ("slew only") and r6 Q3 item 6 ("Step forward only at startup"). The person
  decided on 2026-10-05 ("Ok 225 mesh slew approved"), with the forward step. The person
  changed the forward step on 2026-10-05 ("I think (b)"), because the first rule never
  fires when both bounds hold.
- **OS CLOCK BOUND (2026-10-05)** The OS wall clock is a source. `env::wall` gives the
  OS error bound with each reading where the OS has one (`adjtimex` on Linux,
  `ntp_gettime` on macOS). Where it has none (Windows), `env::wall` gives `None`, and
  `clock` reads that as `Measurement::unknown` (36500 days): a node alone with no known
  estimate (CLOCK HOLDOVER) still gets OS time, with an error that says "unknown", and
  beside a known bound the reading does not vote (ESTIMATE COMBINE). A fixed invented
  error lost: a wrong value gives a bound that is not true. Amends ENV SEAMS. The person
  decided on 2026-10-05 ("Use it, error 'unknown'"), #144. The split between `env::wall`
  and `clock` is from #172. When known
  peers split, `combine` fails, so the clock is unsynced before its first estimate and
  holds over after it (CLOCK HOLDOVER). A known OS bound still votes. Dropping the OS
  source in `clock` when a peer exists lost: it also drops a narrow OS bound (Linux,
  macOS). The person decided on 2026-10-05 ("314 should be (b)"), #314. `clock` gives
  the OS reading to the exchange as an interval, its time plus or minus its bound, so
  the error is never less than the OS bound. An error of 36500 days or more reads as
  unknown, the same as no bound (the coordinator, #144). So does an edge of the bound
  past the range of a stamp, centered at the reading as with no bound: a known bound
  with such an edge needs a reading after 2162 or before 1777, so it cannot hold a true
  time between those years. The coordinator approved it with the advisor, #910. Lost:
  the edge stopped at the range, because it narrows a bound of 36500 days or more into a
  known one; edges in `i128` through a new `estimate` input, because it keeps a false
  bound that votes (#314); `Measurement::widened`, a public item that keeps the OS
  reading a special path. Only `clock` and `node` call `clock::source::Wall::measure`; a
  lint denies it elsewhere (BQ20). On Linux the bound is the kernel's `maxerror`, and
  only chrony and ntpd compute it. `systemd-timesyncd` sets it to 0 at each update,
  while the clock can still be 0.4 s off. So a known OS bound on Linux needs chrony or
  ntpd, and the operator docs must say so. A host with timesyncd (the default on Debian)
  gives a false bound until its operator installs chrony. Lost: the Linux bound always
  unknown, because it also drops the good bound from chrony and ntpd; detecting
  timesyncd, because it reaches outside `env::wall` and is a guess. The person decided
  on 2026-10-06 ("A is still fine"), #689. `os` also gives `None` in clock state
  `TIME_ERROR`, and for a negative `maxerror` or one past the end of a `Span`, because
  root can set any value. `os::wall()` reads once and returns `Error::Wall` when the OS
  refuses the call, as a seccomp filter or systemd's `ProtectClock` can. A later
  refusal panics (#117).
- **CLOCK PEER ANSWER (2026-10-05)** A node with no mesh time answers a peer with its OS
  reading and its OS bound. Cold nodes then vote with each other's OS clocks, and each
  waits until more than half agree (ESTIMATE COMBINE). An answer with an unknown bound
  (an unknown estimate, or an OS clock with no bound) says "unknown" and carries its
  offset. The asking node measures it as `exchange::Reading::Unknown`, so the answer
  cannot narrow into a known bound (#930). A node answers from one read of its clock,
  sent as both intervals, because two reads can straddle a sync and pair a known
  interval with an unknown one. The read is after the request arrived and before the
  answer left, so it bounds both ends of the exchange. An unknown answer carries
  `Measurement::time`, because the midpoint of the interval moves after 2162 (#145). A
  peer that never answers counts against a majority, and `node` removes no source. Lost:
  a node with no time does not answer, because then a mesh that starts cold never syncs;
  an answer of "no time" that takes the source out of the vote, because a node with a
  bad OS clock then syncs on itself; `node` removes a silent source after a timeout, a
  patch that puts time policy in layer 4. The person decided on 2026-10-05 ("Yeah that's
  fine"), #145. So a node that starts while no peer answers stays unsynced, even with a
  good OS bound. Its samples keep their local monotonic reading, and the node stamps
  them in mesh time when the first estimate comes, with the error of that estimate at
  each reading (200 ppm: 0.72 s after 1 h). The buffer holds the samples until then, and
  a node that never syncs fills it. Lost: drop the samples, a patch that loses data;
  stamp them with OS time at once, a patch that writes a time the clock refused and
  cannot correct later. The person decided on 2026-10-05 ("(b)"), #145.
  `clock::Reader::first` gives that stamp: the first estimate at a reading. Later
  estimates never change it, so the stamps keep the order of their readings and are
  never after mesh time (#523). `clock::Reader::now` gives a `clock::Time`: a reading
  of the monotonic clock, and mesh time at that reading, from one read of the clock, so
  a sample with no mesh time keeps that reading. Lost: mesh time at a reading the
  caller made, which can go back while the clock slews down. Approved by the
  coordinator on 2026-10-06 (#964).
- **CLOCK SUSPEND (2026-10-05)** `env::clock` counts time asleep (`CLOCK_BOOTTIME` on
  Linux, `mach_continuous_time` on macOS). After a suspend, the error has grown by
  drift over the sleep, and `clock` needs no reset. A monotonic clock that stops in
  suspend lost: mesh time would fall behind by the time asleep, outside its bound.
  Amends ENV SEAMS. The person decided on 2026-10-05 ("Count time asleep"), #144. On
  Linux the read is `CLOCK_MONOTONIC_RAW` plus the time asleep (`CLOCK_BOOTTIME` minus
  `CLOCK_MONOTONIC`), because time daemons slew `CLOCK_BOOTTIME` faster than 200 ppm
  (chrony up to 83,333 ppm), and a bound that grows at 200 ppm then misses the true
  time. The driver in `os` keeps the largest time asleep it has read, so reads never go
  back (#117). Cost, Amazon Linux 2023: 73 ns against 24 to 29 ns for one
  `CLOCK_BOOTTIME` read on c7i.large (x86-64), and about 100 ns against 30 ns on
  c7g.medium (arm64, a noisy run); the shared maximum adds about 1 ns (M3 Max). macOS
  is not hit: no daemon slews `mach_continuous_time`. Lost: `CLOCK_BOOTTIME` with a
  rule that the daemon slews within a limit, because a node cannot check it;
  `CLOCK_BOOTTIME` with a bound that can fail in a fast slew. The person decided on
  2026-10-06 ("yes"), #688. On macOS `os` reads `CLOCK_MONOTONIC_RAW`, which is
  `mach_continuous_time`. Each `os::clock()` call starts a new clock at 0, so `node`
  calls it once. Tokio's timer stops in a suspend on macOS and slews on Linux, so `os`
  arms it for at most 1 s at a time: a sleep across a suspend completes up to 1 s late.
  The vDSO and the macOS commpage read the counter with no fence that waits for earlier
  stores or holds back later loads, so `os` adds them to give the order that
  `env::clock` requires (#458): `mfence; lfence` before the read and `lfence` after on
  x86-64, `dsb ish; isb` before and `isb` after on arm64. Other architectures do not
  build. Cost: 38 ns against 16 ns for one read without the fences (M3 Max) (#117).
- **CLOCK RUN (2026-10-05)** Within TIME ADAPTERS. `clock::Clock::run` runs all time
  sources of one clock in one task on the clock's shard. `node` builds the source table
  and passes it to `run`. Today the table is the OS clock. Each adapter keeps its own
  loop and decides when it measures: the OS clock at once, then one second after the
  last measurement, so once after a suspend. `run` adds a source for each adapter and
  pushes each measurement. `run` owns every source, so it panics on a clock that has a
  source already: nothing could push to that source. A reader gives the status
  (`Reader::status`, #598). Lost: a task for each adapter with a shared clock
  (`Rc<RefCell>` or a queue), because then the caller shares the clock; the loop in
  `node`, because the peer exchange adds and removes sources, and `node` would pass its
  events through. Decided by the `time` builder (#144, #600). The coordinator approved
  it on #144 and #600.
- **CLOCK REACH (2026-10-08)** `clock::Reader::reach(at) -> impl Future<Output = ()>`
  waits until the latest edge of mesh time is at or after `at`, and is never early: a
  read inside the wait gave such an edge. A later read can give an earlier edge, when
  the error shrinks. A drop cancels the wait. (1) One edge, the latest, as SUBJECT
  PROOF ends a hello; if #274 ends a hold on the earliest edge, it adds an edge
  argument through an interface change. (2) It reads mesh time, sleeps on the
  monotonic clock until the soonest reading at which the slew of that read moves the
  latest edge to `at`, and reads again. Under one slew it is late only by the timer;
  a new slew shows at the next read. (3) Before the first mesh time it reads again
  each second, the period of the OS source. (4) `estimate::Slew::reach(now, at,
  drift) -> Monotonic` gives that reading. It has no `None`: at the last reading the
  edge is at the end of a stamp's range. The edge is not monotonic (a downward slew
  moves it back at each tick), so it steps by the most the edge can rise: `j * (1 +
  rate) + 1` ns over `j` ns, with `rate` the larger of the drift and 500 ppm. Lost:
  an edge argument now; a wake from the writer on each change of the discipline (a
  wake for each waiter each second, and a waker list between shards); a cap on each
  sleep, such as 1 s (about 900 wakeups for each 15-minute hello on each link); a
  `Sleep` type with `reset` for a renewal; `Reader::when(at) -> Option<Monotonic>`,
  which keeps the loop in each caller; an accessor of the monotonic clock on
  `Reader`, and `hub::Config::clock`. Decided by `laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/issues/1870.

### 1.7 Transport

- **TRANSPORT SHAPE LOCKED** One session model: prioritized, cancellable streams plus
  optional datagrams. Carriers are adapters: QUIC (noq-proto), TLS over TCP, relay (TLS
  on 443 through designated nodes), and a one-way diode carrier. An adapter that lacks a
  feature emulates it. Reliability is per delivery mode: commands reliable and highest;
  latest drops stale frames by cancel or datagram and keeps `TCP_NOTSENT_LOWAT` small
  over TCP; complete is reliable and ordered with credits; catch-up is lowest. The
  default carrier per traffic class comes from measurement.
- **T1 seam** Foundation's own `Transport` trait sits in front of every carrier.
  Amended by SIM NETWORK: the trait is private to `transport`, and `sim` replaces the
  network below the carriers (`env::net`), not the transport.
- **R5 starting points** Addresses come from the mesh, not DNS. A TCP path is
  mandatory. Try direct UDP, then direct TCP, then a relay. Relays admit only known
  keys. No n0 infrastructure. iroh `Endpoint` is rejected; quinn-proto is the fallback
  core. The diode carrier is UDP, Noise K, RaptorQ, seq, and codec keyframes: best
  effort with recorded gaps; commands, Raft, and clock exchange cannot cross it.
- **A4 (wire part)** Each connection swaps keys for short numbers.
- **STREAM DISPATCH (2026-10-04)** `transport` is blind to protocols. The first
  message of each stream, and each datagram, starts with a header from `wire` that
  names the protocol (`clock`, `mesh`, `replica`, `blob`, `hub`). `node` holds the
  table from protocol to handler and runs one accept loop per session. A protocol
  that the table does not know comes from a peer, so the loop resets that stream with
  a code and goes on. Decided by the coordinator (network's review of #53).
- **PROTOCOL HEADER (#75)** The header of STREAM DISPATCH is 3 bytes: the wire
  version (`u16`, little-endian), then the protocol number (`u8`): clock 1, mesh 2,
  replica 3, blob 4, hub 5. On a stream, the header is the whole first message, so
  later messages carry no prefix. A datagram starts with it; its handler calls
  `Block::skip(wire::header::LEN)` on the rest (BLOCK VIEW). The
  version covers every message on that stream, encoded series included: each wire
  version fixes one codec version (wire 1 carries codec 1). The version comes first
  and is checked first, so a later version can change what follows it. A node reads
  only `wire::VERSION` until version 2 exists; then it also reads the version before
  it (C9d), and writers take the version from the format flag. `node` stops a stream
  whose header is not valid with code 1 (`wire::header::REJECTED`) and resets its
  reply half, if it has one, with the same code; a datagram whose header is not valid
  drops and counts in a status channel. Stop and reset codes 1 to 15 belong to the
  header; each protocol numbers its own from 16. A client session (`Peer::Client`)
  opens only hub streams, its hello stream is the first hub stream, and the node's
  `Challenge` is the first message on it (CLIENT HELLO); `node` refuses the other
  four protocols from a client. The stream and client rules are approved by the
  coordinator on #90. The hello stream was changed by `laptop.architect` at
  2026-10-08T09:53:58Z
  (https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6057298526).
  Rejected: a version agreed once per session
  (the format flag's flip reaches nodes at different times, so one session can carry
  streams of two versions) and a session per protocol (`transport` stays blind to
  protocols, and it costs five handshakes per peer pair).
- **CLOCK WIRE (#865)** After the header, a clock datagram is one `wire::clock`
  message: a kind byte, then little-endian fields of 8 bytes. A request (kind 1)
  carries `sent`, the monotonic reading of the node that asks. An answer echoes `sent`,
  so the node that asks keeps no open requests, and carries the peer's time (CLOCK
  PEER ANSWER). Kind 2 is a known bound, with an interval read after the request
  arrived and one read before the answer left. Kind 3 is an unknown bound, with the
  peer's best guess, read after the request arrived and before the answer left. The
  offset of CLOCK PEER ANSWER is this stamp less the asking node's own time, because the
  peer's offset has no meaning without the peer's monotonic clock. A message has 9, 41,
  or 17 bytes, and `decode` refuses each other length. `decode` does not check the order
  of an interval, because `estimate::exchange::Exchange::measure` refuses a crossed one.
  Lost: a request number, because the node that asks must then keep and remove open
  requests and still needs the send time of a late answer; each message 41 bytes, as the
  header has one length (a request then sends 32 zero bytes); `encode` into a
  `&mut [u8]` that returns a length (a short buffer then needs an error); a second byte
  for the kind of time (two checks where one kind byte does the work).
- **HUB WIRE (#561)** A remote reader session is one hub stream of class `Complete` or
  `Latest`. After the header, the reader's node sends `wire::hub::Open`: the mode and
  the number of channels, all on one index. A latest session gets the newest live frame
  before its commit. A complete session gets each live frame after its commit, and
  `Open` carries its first grant in bytes (CREDIT RULES). The home answers `Opened`; the
  reader sends `Credit`, its total grant since the open; the home sends each frame as a
  `Head` (path, seq, count, and the number of series). Every frame is encoded (X35), so
  `Head` has no form. The body holds only the series of the reader's view, the index
  series too, written from the frame's block as slices, and both ends charge the frame
  that the reader builds (CREDIT RULES, M2). A series has the place of its first listing
  in the open, from 0. The reader's `hub` lists the keys in the entry order of its own
  frame (its slot order), the index too, so a place is an entry of the reader's frame
  and a session has `channels` places. An open whose keys do not hold the index is not
  valid: the home's `hub` checks it and stops the session with `MALFORMED` (lost:
  `UNKNOWN`; the architect, 2026-10-07,
  https://github.com/synnaxlabs/foundation/pull/1236#issuecomment-6032902101). An open
  of no channel is not valid. Only the fixed part of `Open` and of `Head` is one
  message. The rest is one run of bytes, in messages of at most the peer's
  `message_bytes_max`, back to back with no prefix: after `Open`, the keys; after
  `Head`, the place and end of each series in the body, then the body. A message never
  splits a key or an end, so each side decodes each message as it arrives. The keys run
  holds exactly `channels` keys and the ends run exactly the head's number of series, so
  each side counts them to find where a run ends, and the body starts a new message. So
  no count of channels or series has a cap, and the reader fills one block of its
  frame's length: the header, the range, a descriptor for each series, and the body to
  the last end. A run message with more keys or ends than remain is not valid. A head of
  no series is not valid, since a frame holds its index. The home checks each key as it
  arrives and never allocates by the peer's count. A head with more series than places,
  or an end with a place the session does not have or that is not above the place before
  it, is not valid; `wire::hub::Reader` checks the head as it arrives and `types` checks
  the ends, so the reader holds no more ends than it has places. The ends and the body
  are in place order: the home writes the series of each place it has, from 0, each from
  the frame's block as a slice, with ends it computes in that order. It cuts each series
  from `Frame::body` by `frame::Places::lay`, which finds them with `View::bounds`
  (the architect, 2026-10-07T22:35:41Z,
  https://github.com/synnaxlabs/foundation/issues/1639#issuecomment-6048265226; lost:
  `View::ends`, which gives no start, and `Frame::bounds`, a search for each place).
  `View::bounds` is crate-private, as no crate outside `types` calls it (the architect,
  2026-10-08T00:49:22Z,
  https://github.com/synnaxlabs/foundation/pull/1668#issuecomment-6049855032; lost: a
  public `bounds`, a second way to lay a reader's frame beside `Places`). At
  the open it makes the list of each place and its home entry, sorted by place, and
  writes each ends message from it with `wire::hub::ends::encode`, which sizes the
  message by its buffer, so no scratch buffer holds the ends (the architect, #1146,
  https://github.com/synnaxlabs/foundation/issues/1146#issuecomment-6032284157). It
  takes exactly the ends the buffer holds and no more, so one iterator passed with
  `by_ref()` splits a run into messages; the caller owns the count of the run (the
  architect,
  https://github.com/synnaxlabs/foundation/pull/1258#issuecomment-6033667563). The first
  series starts at 0, and each other at the end before it rounded up to a multiple of 8.
  So the body is the series bytes of the reader's own frame (FRAME LAYOUT), and the
  reader builds that frame in one block: the header and descriptors that `types` writes,
  then the body as it arrives, with no copy of a series after the receive. An end below
  the start of its series is not valid; `types` refuses it, as `frame::check` does. The
  padding may hold any bytes (FRAME LAYOUT). Each direction has its own messages: the
  reader sends `Open`, then `Credit`; the home sends a `Reply`, `Opened`, `Head`, or
  `Behind`. The home sends `Behind` after the last frame before a miss of the session,
  then finishes its stream. The reader's `next` gives each frame before it, then
  `Ended::Behind`. `hub` builds both in #340 PR 4. A message after `Behind` is not
  valid (lost: a stop code, which can cut off the frames sent before it; the architect,
  2026-10-07T21:07:50Z,
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6046877541).
  `Behind` and `Credit` in a latest session are not valid (lost: accept them in either
  mode, which lets a remote latest reader give `Ended::Behind`; the architect,
  2026-10-07T21:34:58Z,
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6047310321). The
  check of the mode costs the decode of a `Credit` +0.28 ns. The `Ended` state and the
  mode flag of `Reader`, for `Behind`, cost a body message up to +0.37 ns and a frame up
  to +1 ns. Both are accepted with no code change; a `Credit` decode past +1 ns over
  `main` comes back to the architect (lost: `#[inline]` on `wire::hub::Home::decode`,
  which is not measured and grows each caller; the architect, 2026-10-07T22:18:40Z,
  https://github.com/synnaxlabs/foundation/pull/1631#issuecomment-6048002213, and
  2026-10-08T00:16:49Z,
  https://github.com/synnaxlabs/foundation/pull/1631#issuecomment-6049484466). Stop
  codes: 16 `UNKNOWN` (a channel the home does not know), 17 `NOT_HOME` (the node is not
  the home of the index), and 2 `wire::header::MALFORMED` (a message that does not
  decode, comes from the wrong side, or breaks a rule above), which every protocol may
  use. Lost: a `message_bytes_max` of at least the largest pool block (a client or a
  foreign peer can set 1472, and it ties `transport` to the pool); a cap of 91 channels
  a session, the most that fit in 1472 bytes; the index in its own field of `Open`,
  because the home knows its index and a second copy needs a check; the whole
  `Frame::body` (a reader gets only its view); an `UNSYNCED` code, because an unnamed
  open needs no mesh time (READER RULES), and a later named open can add one; grants for
  many sessions in one message, which wait until a link carries a second session; a
  public series count on `View`, for a home that writes the ends from `View::iter`,
  which gives the home's entry order and not place order (the architect,
  https://github.com/synnaxlabs/foundation/issues/1146#issuecomment-6032284157). The
  coordinator approved the messages (2026-10-05); the architect decided the rest (#561,
  2026-10-06) and the run, the index place, and `MALFORMED` on #1064
  (https://github.com/synnaxlabs/foundation/pull/1064#issuecomment-6030652085), then
  whole keys and ends and one message type for each direction
  (https://github.com/synnaxlabs/foundation/pull/1064#issuecomment-6030699163), then the
  open of no channel and the place checks in `hub`
  (https://github.com/synnaxlabs/foundation/pull/1064#issuecomment-6030906615). Amended
  (2026-10-07, #1068): the body follows the places, not the home's entry order, so an
  end whose place is not above the place before it is not valid; both ends charge the
  reader's frame. Lost: a copy of each series at the reader (one per sample at every
  remote reader at P1 rates, which the home's free order cannot justify,
  `docs/claude/performance.md` rule 10); a reader key set in the home's order (key sets
  are sorted by slot); a start in each descriptor (a disk and wire format change, C9d).
  Decided by the architect, #1068
  (https://github.com/synnaxlabs/foundation/issues/1068#issuecomment-6031655359). The
  byte form, little-endian: `Open` is kind 1 (latest) or 2 (complete, then `limit_bytes`
  `u64`), then `channels` `u32`; `Credit` is kind 3, then `limit_bytes` `u64`; `Reply`
  is kind 1 (opened), 2 (head: path `u8`, live 0 and backfill 1, seq `u64`, count
  `u32`, series `u32`), or 3 (behind, no fields, by the Behind rule: the architect,
  2026-10-07T21:07:50Z,
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6046877541); a key is
  a `u128`; an end is place and end, each `u32`. Amended (2026-10-07, #1196): the
  message order, the runs, and the head bound move from `hub` to two stateful decoders
  in `wire`, `hub::Home` at the home and `hub::Reader` at the reader's node, each with
  an exact error for each broken rule, so `hub` checks no wire rule. Decided by the
  architect
  (https://github.com/synnaxlabs/foundation/issues/1196#issuecomment-6032630529).
  Amended (2026-10-07T14:56:48Z, #1455): `Reader::decode` checks a message in three
  steps and gives the error of the first that fails: the bytes (its decode error), the
  order of the session (`Unopened` or `Reopen`, whatever the content), then the content
  against the session (`Places`, `Run`, `Body`). A head before `Opened` is not a head
  of this session yet, so its series count has no session to break. Lost: `Places`
  first. Decided by the architect
  (https://github.com/synnaxlabs/foundation/issues/1455#issuecomment-6040654132).
  Amended (2026-10-07T23:34:09Z, #1648): `types::frame::Places` holds the layout of a
  remote reader's frame for `delivery` and `serve`. `Places::lay` gives each series in
  place order, with its bounds in the home's `Frame::body` and its end in the reader's
  frame; `Places::charge` is the `Frame::charge` of that frame, in O(1) when the places
  name each entry of the key set, in any order: each block payload is a multiple of 8
  bytes, so the padding of the last series does not change the footprint (the architect,
  2026-10-08T00:25:50Z,
  https://github.com/synnaxlabs/foundation/pull/1668#issuecomment-6049589885. Supersedes
  "in entry order" in
  https://github.com/synnaxlabs/foundation/issues/1648#issuecomment-6048992122). Lost: a
  free function that lays one frame, with each caller keeping its own state for each key
  set, so `delivery` and `serve` each repeat it. Also lost: one `Places` for each remote
  session, whose layout `release` keeps with each frame for `serve`: each frame in the
  queue would hold its layout. So a remote session holds two. Decided by
  laptop.architect:
  https://github.com/synnaxlabs/foundation/issues/1648#issuecomment-6048992122.
  Supersedes: "At the open it makes the list of each place and its home entry, sorted by
  place" above; `Places` makes it at the first frame of each key set. `lay` walks the
  places for a frame with at least one series at the places for each 8 entries that
  they name, and sorts the series of a sparser frame. Lost: walk only (10 series of 100k
  places took 140 to 420 µs, not 0.5 to 0.7 µs), and sort only (a scattered frame of
  100k series took 3.9 to 6.0 ms, not 1.6 to 1.7 ms). The architect accepted the cost of
  the dense walk against 1e658b7a, up to the head numbers of #1695 (laptop.architect,
  2026-10-08T01:36:39Z,
  https://github.com/synnaxlabs/foundation/pull/1695#issuecomment-6050376022). The cut
  counts only the series at the places, and is 8. Lost: a count of each series of the
  frame (`outside_lay` 56 to 69 µs, not 0.1 µs), and a cut of 16 (a frame just over it
  cost 148 to 255 µs more than one just under). The architect also accepted
  `narrow_lay` at +3 to +4 ns per frame (laptop.architect, 2026-10-08T02:33:03Z,
  https://github.com/synnaxlabs/foundation/pull/1695#issuecomment-6050982066). Each
  dense frame first pushes ceil(m/8) series, for the m entries that the places name,
  then moves them into the walk: the architect accepted +5.6% at `cut_lay` 12,500 and
  +1.3% at `reversed_lay` 100k for -21.7% at `cut_lay` 12,499 (laptop.architect,
  2026-10-08T03:07:31Z,
  https://github.com/synnaxlabs/foundation/pull/1695#issuecomment-6051347440).
  Amended (#1631): after `Behind`, each message gives `Ended`, before the three steps
  and whatever its bytes, since the home sends nothing after `Behind`. Step 3 also
  gives `Latest` for a `Behind` in a latest session, since only a complete session
  falls behind. Decided by the architect (2026-10-08T01:04:51Z):
  https://github.com/synnaxlabs/foundation/issues/1689#issuecomment-6050026992.
- **ONE PORT PER NODE (2026-10-04)** A node listens on one UDP port and one TCP port on
  the same port number, however many shards it runs, so each site's firewall needs one
  known port per conduit. Each QUIC connection belongs to one shard, and every
  connection ID a node issues encodes that shard. A receive loop on one shard reads the
  UDP socket in batches and hands each batch to the owning shard over the C2 ring; every
  shard sends on the same socket. The TCP listener accepts and moves each stream to its
  shard. `env::net` therefore splits a UDP socket into a receive half with one owner and
  a send half that any shard may use, and `sim` models the split. Rejected: a port per
  shard (a port range in every firewall), kernel reuse-port hashing (routes by address,
  breaks on NAT rebinding), and one shard doing all network work. If the receive loop
  saturates on Linux, add a reuse-port group steered by the same connection ID. Decided
  by the design session under the architecture delegation (#53). The same port number
  (2026-10-06): with port 0, UDP takes a free port and TCP binds the same one. When TCP
  finds it in use, the node closes the UDP socket and tries a new port, up to 8 tries,
  then gives the last error: TCP and UDP have separate port spaces, and no OS call gives
  a port free in both. A fixed port that fails gives its error at once. Approved by the
  coordinator on #990.
- **TLS RANDOMNESS (2026-10-04)** All randomness inside TLS (key shares, client
  random, nonces) comes from aws-lc, not from `env`. rustls holds its random source
  as a `&'static` value, and aws-lc makes X25519 key shares with its own randomness,
  so neither can be injected without a leak per `Transport`. It changes bytes, never
  sizes or timing. Simulated runs still replay because nothing branches on those
  bytes; replay traces leave out ciphertext and handshake randoms, and a `sim` test
  runs one value twice and compares the traces. The person accepted it ("Accept TLS
  RANDOMNESS"), from `network`'s proposal on #54.
- **R14** Do not build on Zenoh; a Zenoh connector may come later. Measure QUIC against
  TLS over TCP on Linux early.
- **TRANSPORT SURFACE (#45, 2026-10-04)** One `Transport` per shard dials and accepts;
  the node's sockets and relays sit in one node-level part, `transport::Port` (ONE
  PORT PER NODE), which `node` binds once and splits into one part for each shard. A
  `Session` goes to one peer over one path, direct or relayed, fixed for its life, and
  runs every class on one carrier. A second carrier for some classes waits for the
  measurement in TRANSPORT SHAPE LOCKED, which must show that `Latest` p99 holds while
  `CatchUp` runs on the other carrier. Until then, QUIC is the one carrier (r19): it
  keeps `Latest` p99 low while bulk shares the connection, at 1.5 times the CPU per byte
  of TLS over TCP. The person decided on 2026-10-05: "QUIC" (#10). Streams carry whole
  messages in pool blocks, not bytes; the QUIC carrier benchmark decides whether decode
  reads chunks in place instead. A stream reaches the peer with its first message, and a
  `Sender` dropped without `finish` resets it. A `Sender` can also send without waiting
  (`try_send`): it gives the message back whole when the stream cannot take it now, and
  it never resets the stream. Each stream has a `Class` (`Command`, `Latest`,
  `Complete`, `CatchUp`) that sets its priority and preferred carrier. A peer is a node
  key or a `Client` (an SDK, proved by its signed hello above). Callers admit peers,
  dispatch streams (STREAM DISPATCH), and cancel stale latest frames. Builds on SIM
  NETWORK. Proposed by `network` in #45; approved by the coordinator on PR #53.
  `Transport::public_key` gives the key that the transport proves to each peer, so
  `Mesh::open` can check it against the public half of the private key of its config
  (MESH SURFACE). Decided by laptop.architect and laptop.architect-2 (#1587, 2026-10-07
  19:55 UTC):
  https://github.com/synnaxlabs/foundation/issues/1587#issuecomment-6045695196,
  https://github.com/synnaxlabs/foundation/issues/1587#issuecomment-6045706124.
- **STREAM WIRE (#55, 2026-10-05)** On QUIC, the side that opens a stream sends one
  class byte first in its own direction: 0 `Command`, 1 `Latest`, 2 `Complete`, 3
  `CatchUp`. The byte goes with the first message, so a stream reaches the peer with its
  first message. The peer queues a stream for accept at the first byte of its first
  message. A stream that ends or resets before that byte drops: the peer never accepts
  it, and resets the reply half of a two-way stream with code 0 (amended:
  https://github.com/synnaxlabs/foundation/pull/1380#issuecomment-6038160446). Each
  message is a QUIC varint length, then that many bytes, at most the receiver's
  `message_bytes_max`. A node accepts the waiting streams highest class first. Each
  stream sends at the QUIC priority of its class, `Command` first, and streams of one
  class share in turn. The priority is strict: a class sends nothing, resends too, while
  a higher class has bytes to send, so a steady higher class starves the lower ones. It
  orders only the bytes that QUIC holds. All classes share one QUIC send window, so a
  message can wait for bytes of a lower class to be acknowledged (#797). The QUIC send
  window, not the send budget, bounds what QUIC holds. A message that QUIC does not take
  in full waits its turn, by class, then oldest first. Only the first sender in turn
  writes, and only it wakes when QUIC has room. A write of a class ahead of every
  waiter goes first; any other write waits, and a `try_send` gives the message back.
  Stream credit is twice the connection window, so a stream never waits on its own
  credit while the connection has room. This relies on reader-granted credits (B3): a
  node takes every byte it granted credit for. A peer that gives less stalls only its
  own connection (#819). The turn goes `Command`, then `Latest` and `Complete` by share,
  then `CatchUp`. While streams of both `Latest` and `Complete` hold a message to send,
  QUIC takes 3 bytes of `Complete` for each byte of `Latest`, within about one window:
  `Complete` goes ahead while it is owed bytes. A class alone makes no debt and no
  credit, and pays off what it owes or is owed. Room that a stream got and its caller
  has not taken counts for neither class, and a message that `try_send` gave back is not
  held. The send budget gives room in the order of the turn. Room that a message of the
  owed class frees waits for that class's next message while the other class holds room,
  so neither class can take the share through the budget (#819). A change of this
  share changes the share bound of `transport/benches/send.rs` in the same PR. Decided
  by architect-2 (#977, 2026-10-07 17:15 UTC):
  https://github.com/synnaxlabs/foundation/issues/977#issuecomment-6042983190. Lost: a
  connection per class, because four handshakes and four congestion controllers
  compete on one path (#55). Settled by the advisor and the coordinator under the
  person's delegation (#789).
  A node resets a stream with the stop's code when the stop arrives, and frees
  the stream's room in the send budget and its turn (#1308). A stop that arrives after
  the peer acknowledged all the data of a finished stream, or this side's reset of the
  stream, has no effect, and the node does not check its code, because the carrier has
  freed the stream. Decided by architect-2 (#1445, 2026-10-07 17:48 UTC):
  https://github.com/synnaxlabs/foundation/issues/1445#issuecomment-6043565271.
  Supersedes
  https://github.com/synnaxlabs/foundation/issues/1445#issuecomment-6042123544. A peer
  breaks the protocol when it sends another class byte, ends a stream inside a message,
  sends a message over the limit, or resets or stops a stream with a code over 32 bits.
  The node then closes the connection with application code 2^32 and the reason as
  text, and the caller gets `Error::Broken`.
  Each connection keeps two budgets, which count the length of each message. A sender
  starts a message only when the messages it started and the streams have not taken in
  full stay within the peer's `window_bytes`; else the write waits for `Writable`. A
  message starts only when it fits and no stream of its class or a class ahead of it
  waits for room. Room that frees goes to the waiting streams by class in that order,
  then oldest first, until the next one does not fit, and only those streams wake
  (#611). A send that does not wait (`try_send`) starts a message only by the same rule
  and when, after a flush, the stream holds no part of an earlier one; else it gives the
  message back with no byte of it sent, and the stream does not wait for room (#597). A
  receiver takes room for a message by the same rule, highest class first, within
  `window_bytes` plus `message_bytes_max`; else the read waits for `Readable` (#611). It
  holds the bytes of a message outside the pool, and takes a block only when the message
  is whole. Decided by architect-2 (#1456:
  https://github.com/synnaxlabs/foundation/issues/1456#issuecomment-6041057673). No
  chunk of the carrier outlives the read that took it: a read that ends before its
  message has a block copies the bytes it holds into one buffer of the message's
  length, outside the pool. Decided by `laptop.architect-2` (#1456, 2026-10-07 17:05
  UTC: https://github.com/synnaxlabs/foundation/issues/1456#issuecomment-6042785777).
  Supersedes
  https://github.com/synnaxlabs/foundation/issues/1456#issuecomment-6042336187 and the
  copy cost of
  https://github.com/synnaxlabs/foundation/issues/1456#issuecomment-6041057673. A
  test asserts that each read leaves no view of a chunk, and that each reader holds
  at most one buffer, whose capacity is the length of its message (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/issues/1456#issuecomment-6043389350). So
  bytes that wait for a block never use up the credit that a started message needs,
  and a peer that breaks the send rule holds at most the receive budget and stops only
  its own connection. Each node's first one-way stream is its hello, with no class byte:
  (id, value) pairs, both QUIC varints, ids strictly increasing, then the stream end. Id
  0 is `window_bytes` and id 1 is `message_bytes_max`; both are required. A node ignores
  an id it does not know, so an advisory field needs no new ALPN; a field that the peer
  must understand needs one. The acceptor sends its hello at 0.5-RTT, once it has the
  whole ClientHello and so the peer's transport parameters, or at its `Connected` when a
  HelloRetryRequest holds them back. The dialer sends at its `Connected`. So the hello
  adds no round trip. The hello has its own one-way stream: a node lets the peer open
  `streams_max` + 1 one-way streams, and does not give back the credit of the peer's
  hello stream when it ends, so after the hello the peer has at most `streams_max` open.
  Until the peer's hello arrives, a node opens and accepts no stream; the caller bounds
  that wait, with its other limits before admission (#563). A sender obeys only the
  peer's values: each message is at most the peer's `message_bytes_max`, and the send
  budget is the peer's `window_bytes`. A value over what the node can count counts as
  the largest it can count. A peer breaks the protocol when its hello ends inside a
  pair, misses a required id, has an id out of order, is over 256 bytes, has a
  `message_bytes_max` below 1472 (architect, #1198:
  https://github.com/synnaxlabs/foundation/issues/1198) or a `window_bytes` below it, or
  resets. A peer whose QUIC transport parameters cannot take this node's whole hello at
  once (no one-way stream, or a stream or connection window under the hello) also breaks
  it, with the reason `a peer with no room for the hello`. A dial that breaks so gets
  `Error::Broken` with no `Connected` before it, and an accept gives the caller no
  event. Before the handshake is confirmed, QUIC gives the peer no reason, only
  APPLICATION_ERROR. A Foundation node always has room: `streams_max` is at least 1, and
  `window_bytes` is at least `message_bytes_max`, which is at least 1472. A compile-time
  assertion holds 1472 at or above the hello limit, so only a foreign peer gets this.
  Lost: send the hello later when credit comes, because `open` then needs a second gate
  and a state that only a foreign peer reaches. `Endpoint::write` gives
  `Error::TooLarge` for a message over the peer's limit; a caller that forwards a
  writer's frame gives the writer `Large`, and the writer splits the frame (LARGE
  FRAME). Proposed by `network` in #55; approved by the coordinator on PR #407. The
  budgets: proposed by `network` in #228. The room order: approved by the advisor on
  #611. The hello: proposed by `network` in #55; settled by the advisor and the
  coordinator under the person's delegation (#55). A sender can send one message from
  parts of one block (`send_parts`, `try_send_parts`), and the budgets count it as one
  message, of the sum of its parts. A `stream::Part` is a range of the block, then at
  most 255 zeros. The stream never sends a byte of the block outside the ranges, because
  those bytes can hold stale data of another channel; the padding is zeros, which `hub`
  computes from FRAME LAYOUT. Lost: a range that runs past the series, because it sends
  stale block bytes; a pad rule in the stream, because it puts the hub layout in
  `transport` and is wrong for a series split across messages (architect, #1197:
  https://github.com/synnaxlabs/foundation/issues/1197#issuecomment-6032606575, after
  HUB WIRE
  https://github.com/synnaxlabs/foundation/issues/1197#issuecomment-6032579333). The
  stream sends the zeros from one static constant of 255 zero bytes, and `Part` holds no
  invariant, so its fields are public. Lost: the cap of 7, because it is FRAME LAYOUT's
  alignment inside `transport` and adds a panic; private fields and a fallible
  constructor for that cap. `stream::Sender::bytes_max` gives the peer's
  `message_bytes_max`, which the hello gives before any stream opens, and `hub` cuts
  each run at it. Lost: a `send_parts` that cuts a run into messages, because the stream
  knows no key or end of HUB WIRE and `try_send_parts` could then send part of a run; a
  probe with `TooLarge`, a guess with one failed call for each session (architect,
  #1197: https://github.com/synnaxlabs/foundation/issues/1197#issuecomment-6033870280).
  A receiver can receive into its own buffer (`recv_into`). A message longer than the
  buffer gives `Error::TooLarge` and stays queued, and so does a message whose future
  drops; HUB WIRE makes that `TooLarge` a broken session, not a size probe. Lost: the
  `Message` type of the proposal, because it changes `send` and `try_send` for each
  caller and must own its ranges (architect, #1197:
  https://github.com/synnaxlabs/foundation/issues/1197#issuecomment-6032529738).
- **DATAGRAM WIRE (#55, 2026-10-05)** On QUIC, a datagram is one message in one QUIC
  DATAGRAM frame. `transport` adds no prefix: the frame carries the length, and the
  message itself starts with the STREAM DISPATCH header, which the caller writes. A node
  takes datagrams on every connection. It sends its `message_bytes_max`, at most 65535,
  as the QUIC `max_datagram_frame_size` parameter. noq-proto holds at most
  `message_bytes_max` bytes of datagrams until the node takes them, after each UDP
  packet, so `message_bytes_max` is at least 1472, the largest UDP payload a node takes
  (#610). A sender's largest datagram is the smaller of the path's limit and the peer's
  limit less the frame header (9 bytes). A peer that takes no datagrams, such as an SDK
  client, gets none: the limit is 0, and each send gives `Error::TooLarge`. A datagram
  over the receiver's `message_bytes_max` breaks the protocol: noq-proto closes the
  connection with PROTOCOL_VIOLATION, and the caller gets `Error::Broken`. Each
  connection queues at most 64 KiB of datagram bytes to send; when a new one does not
  fit, the oldest unsent ones drop. Small datagrams hold more pool than that, because
  each holds a block (#615). A node copies each datagram into a block from its pool when
  it arrives, and drops it when it gets no block (the pool or the system has no room).
  At most 64 wait untaken on one connection; a new one drops the oldest, and they free
  when the connection ends. #68 adds a count of each drop, with the counts of RECV
  WAITS. Proposed by `network` in #55; approved by the coordinator on #55, and the
  `datagram` doc on #565.
- **RECV WAITS (#581, 2026-10-05)** `stream::Receiver::recv` waits while it has no
  block, because the pool has no room or the system refused a commit. It gives the next
  message, `None` at the end, `Error::Reset` when the sender cancelled the stream, or
  the error that ended the session. A full pool and a refused commit get no error:
  `transport::Error` has no `Pool` variant, and the read path's "no room now"
  stays private (architect, #68:
  https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6032721674). Its doc
  says that it waits. The whole message keeps its room in the receive budget, and the
  budget holds the peer (STREAM WIRE). Decided by architect-2 (#1456:
  https://github.com/synnaxlabs/foundation/issues/1456#issuecomment-6041057673). TLS
  over TCP must do the same (TRANSPORT SHAPE LOCKED). One timer for each `Transport`
  retries all of its waiting reads, for both causes; each retry's `alloc` takes back the
  blocks returned since the last try. The retry interval is a `transport` constant that
  simulation tunes (5.3). The waiting reads of one `Transport` take blocks highest class
  first, then oldest first, so `CatchUp` reads cannot starve `Command` reads; other
  users of the shard pool (M4) are not in this order. A read waits for a block only
  with a whole message that holds its room, so it never waits for room in its place.
  `transport` counts the time that reads wait and each refused commit, and `node`
  publishes them on status channels (BQ11b). `Transport::status` gives
  `Status { waited, refusals }`, pulled, not pushed: `waited` is the time that at least
  one read waited, not the sum over reads (architect, #68:
  https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6032541901). A caller
  ends a wait when it drops the future; it can then call `stop`.
  `datagram::Receiver::recv` gives no such error either: a datagram with no block drops
  and is counted, and the read waits for the next one. `hub` writes no retry for a read.
  B5 on the remote hop: the writer's `hub` never waits on a live send. When the stream
  cannot take a live frame now, `hub` drops it and adds its samples and stamps to one
  pending gap for each index. When the stream can take a message again, `hub` sends the
  pending gap first, and the home records it and warns. The writer gets the same answer
  as for a frame the home dropped, and never resends it (B7). Backfill waits. The live
  send is `stream::Sender::try_send` (#597). `block` gets no wake when a block returns
  until simulation shows that the resume latency matters; then `memory` proposes one
  wake, which home backfill shares. Until then, home backfill also retries on a timer.
  Rejected: each caller retries (each caller writes the same timer, and the pool's
  states leak into `hub`), and the stream ends (memory pressure becomes stream churn and
  lost messages, and `Command` streams drop first). Decided by the advisor under the
  delivery and wire internals delegation.
- **DIAL ORDER (#68, 2026-10-07)** `Transport::dial` tries a peer's addresses UDP, then
  TCP, then relays, in the given order within a kind. It starts the next address 250 ms
  after the newest attempt started, or at once when the newest fails, and keeps the
  first session that completes; dropping the others closes them. Before it starts a
  carrier, it checks each address: port 0, an unspecified IP, or a kind with no carrier
  on this node is the cause `Error::Unroutable` in `Error::Unreachable`, so
  `Endpoint::connect` keeps its invariant panic. A broken socket ends the dial with
  `Error::Network`. Rejected: the carrier maps noq-proto's invalid address to an error
  (each later carrier would need its own check), and `Network` with neutral text (one
  variant with two meanings: a caller cannot tell a dead socket from a bad address). An
  attempt that connected before the break still wins, and its session ends with
  `Error::Network`, as `accept` gives such a session, so the result does not depend on
  the order of the attempts. The order and the stagger: proposed by `box2.builder-5`
  (https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6022920297), decided
  by the architect in review of #1067. `Unroutable`: decided by the architect
  (https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6030703879). The
  connected attempt: proposed by `box2.builder-5`, decided by the architect
  (https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6030913321).
- **CANCELLED SEND (#68, 2026-10-07)** A `stream::Sender::send` or `send_parts` future
  that drops after the stream sent a byte of its message (its header counts), and
  before it completes, resets the stream with `Code(0)`. One that drops before that
  sends nothing and changes nothing, and the stream stays open. That includes a drop
  before its first poll, while it waits behind an earlier message, while it waits for
  its turn, and while it waits for room in the send budget. The core takes the message
  out and gives back its room in the send budget and its place in the turn, as a
  completed write does. After a reset, each `send`, `try_send`, `send_parts`,
  `try_send_parts`, and `finish` on the sender gives `Error::Reset { code: Code(0) }`
  after the checks below, and the `Error::Reset` doc names both causes: the peer, or a
  dropped `send` future. A dropped future is a normal cancel in async code, such as a
  timeout in a select, so it must not panic; in both cases the caller opens a new
  stream. Rejected: a panic, as after `finish` (a timeout the caller handles would
  become a crash). Proposed by `box2.builder-5`, decided by the architect, #68
  (https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6030986313). Amended
  by the architect
  (https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6035156093): reset
  only when bytes of the message may have gone. Amended again
  (https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6035820204): the
  rule names the fact, a byte went, and not the proxy, the stream took it. `send`,
  `try_send`, `send_parts`, and `try_send_parts` check in this order: the range panic
  (`*_parts`), the panic after `finish`, `Error::TooLarge`, then the state errors
  (`Reset` after a dropped send future, `Stopped`, or the error that ended the
  session). The limit is fixed for the session, so a size defect shows in every state
  of the stream
  (https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6035220831).
- **NODE KEY TLS** Every carrier but the diode runs TLS 1.3 only. A node's certificate
  is self-signed from a fixed template: Ed25519 key, `CN=foundation`, serial 1, valid
  from 1970 to `99991231235959Z`. The same key always gives the same bytes. A peer is
  the Ed25519 key in the leaf certificate's `SubjectPublicKeyInfo`; names, dates, and
  issuer are not checked. A peer's chain is one certificate of at most 1 KiB; any other
  chain is refused, so a peer cannot make the node hold more for a session (#299). The
  limit is part of `foundation/1`: a certificate over it needs a new ALPN. The person
  approved it on 2026-10-05 ("approve"), #383. A node sends its certificate when it
  dials; an SDK client sends none and pins the node key the same way. ALPN is
  `foundation/1`, and a new session protocol gets a new name. During an upgrade, a node
  accepts its own ALPN name and the previous one, and offers its own name only after
  every node runs the release (C9d). A session that agrees no ALPN, or a name the node
  does not accept, ends on every carrier. The suites are AES-128-GCM, AES-256-GCM, and
  ChaCha20-Poly1305; the groups are X25519MLKEM768, X25519, P-256, and P-384. A dialing
  node offers them in that order, and the client's order decides, so nodes agree
  AES-128-GCM and X25519MLKEM768. The person chose "AES-128-GCM" first between nodes and
  "Hybrid first" on 2026-10-05. A node accepts any one suite and group, so an SDK may
  offer only one. Resumption and 0-RTT are off, so rustls gets a fixed time and never
  reads the OS clock. Randomness inside TLS comes from aws-lc (TLS RANDOMNESS). Decided
  by `network` in #54; the ALPN check, suites, and groups in #108. The person accepted
  it as a contract on 2026-10-05 ("yes to both"). The golden certificate, the ALPN name,
  and the suite and group lists are an oracle in `oracles/conformance/transport/`. A key
  of small order is not a node key: a signature for it passes with no private key, so
  every Ed25519 check refuses it (BQ12). `types::ed25519::PublicKey` refuses such a
  key when it is built, so no check site needs its own test. The person decided on
  2026-10-05 ("Yeah that's fine"), #227, #277. `types::ed25519::PublicKey` holds the
  Ed25519 public key of a node and of a subject. Decided by `laptop.architect` at
  2026-10-08T04:03:21Z
  (https://github.com/synnaxlabs/foundation/issues/1755#issuecomment-6051941741).
  `types::ed25519::PrivateKey` holds the Ed25519 private key of a node and of a
  subject (ruling above). It moved in its own mechanical PR before the PR that gives
  `hub::client` the private key of a subject. Ordered by `laptop.director` at
  2026-10-08T05:41:28Z
  (https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6053189498).
  `types::ed25519::Pair::new` is the one place that derives the public key from the
  private key (`Pair` ruling below). `PrivateKey::public` derives through it, for a
  caller that needs only the key, or needs it once at an open or a join: `transport`
  and `mesh` at open, `mesh::Ticket`, and tests. A holder that signs for each message
  or request keeps one `Pair`. No crate keeps a copy. So
  `types` depends on `aws-lc-rs`, as it owns the Ed25519 rule of the key. Cost: each
  crate that depends on `types` builds `aws-lc-rs` one time for each target directory.
  Lost: a `pub fn` in `transport`, a pass-through for a thing that is not transport;
  and the copies, which grow with each crate that needs the key. Decided by
  `laptop.architect` (2026-10-07T14:16:15Z):
  https://github.com/synnaxlabs/foundation/issues/1423#issuecomment-6039878050. The
  first sentence was changed by `laptop.architect` at 2026-10-08T09:12:15Z
  (https://github.com/synnaxlabs/foundation/pull/1843#issuecomment-6056619178).
  `types::ed25519::PublicKey::verify` is the one Ed25519 verify, and gives
  `BadSignature` for a signature that is not of the message by the key. A verify on
  `PublicKey` uses a key that is not of small order by construction. `mesh` and
  `access` call it. Lost: a `bool`, which a caller can invert or drop
  with no word from the compiler; a `Signature` type, as `[u8; 64]` already fixes the
  length; and a copy in `access`. Decided by `laptop.architect` at 2026-10-08T05:45:46Z
  (https://github.com/synnaxlabs/foundation/issues/1747#issuecomment-6053244858). The
  TLS CertificateVerify is the exception: rustls checks it with the Ed25519 of
  `aws-lc-rs`, as a step of the TLS 1.3 handshake, and `transport` takes the
  certificate's key only as a `PublicKey`, so a key of small order ends the handshake.
  If `PublicKey::verify` gets a check that `aws-lc-rs` does not make, both TLS verifiers
  call it. Lost: a call of `PublicKey::verify` in each TLS verifier, which moves the
  check of the scheme and of the signature out of rustls, a mature library that makes
  them, and adds no check that the handshake does not make. Decided by
  `laptop.architect` at 2026-10-08T08:04:32Z
  (https://github.com/synnaxlabs/foundation/pull/1812#issuecomment-6055539099).
  `types::ed25519::Pair` is the one Ed25519 sign. Each signer (`mesh::claim::Signer`,
  `mesh::card::Signed::sign`, `mesh::Ticket::admission`, `transport::Tls::new`, and
  `hub::client` in #1748) builds one `Pair` and keeps no other signing key. The TLS
  CertificateVerify is the exception: rustls signs it with the key of the PKCS#8
  document that `transport` builds, as a step of the TLS 1.3 handshake. Lost: a
  `PrivateKey` that owns the pair, which re-derives on each clone and changes each
  constructor; and a `PrivateKey::sign`, which costs a scalar multiplication for each
  claim and each request of `hub::client`. Decided by `laptop.architect` at
  2026-10-08T08:12:49Z
  (https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6055665349).

### 1.8 Consensus, regions, and the spec

- **S9 (as revised by R4 SETTLED)** `State { spec, runtime }`. Only `apply` changes the
  spec (see X28); only the mesh changes runtime state. `plan` compares files with the
  spec only. Raft holds each region's spec pointer (version and root hash) and runtime
  state. Fast state (status, health, clock error, control state, reader positions)
  stays out of Raft. Supersedes: S9 name-hierarchy tree, S9 gossip hints.
- **R4 SETTLED** Own sans-I/O Raft modeled on etcd/raft, PreVote and CheckQuorum on,
  with etcd scenarios and the TLA+ spec as oracles. No gossip. Each region's spec is one
  prolly tree keyed by full name, about 4 KiB chunks, BLAKE3. Each change record lists
  its new chunks. A region's voters sit on one LAN. A node fetches only the regions and
  ranges it uses.
- **SPEC CHANGE (#1083)** A `Spec` change record (kind 4) moves a region's spec
  pointer by compare-and-swap. It holds the base pointer, the new root, and the digests
  of the new tree's chunks, never their bytes, so `mesh` moves the pointer without the
  chunks. Byte form: the base version (8 bytes, little-endian), the base root (32), the
  new root (32), the chunk count (2 bytes, little-endian), then each digest (32), in
  strictly rising order. The new version is `base.version + 1`. Lost: a version in the
  record, which can disagree with the base. A record lists at most `CHUNKS_MAX` = 1024
  digests, about 32 KiB, so that one record fits in an append of 64 KiB, a node's
  limit; decode refuses a larger count. An entry over a member's limit is never sent,
  and `raft` sends it again with no end (#1361). `raft` bounds an `Append` by its count
  of entries, not by its bytes, so two records at the bound in one `Append` go over
  64 KiB. #1361 bounds it by bytes before a milestone applies a spec change to a region
  of more than one member. #1741
  decides how a change of more new chunks applies. Every member applies a change whose
  base is the pointer, and refuses one whose base is not (`Refused::Stale`), so of two
  changes from one base only the first applies. The state machine never reads chunks
  and never runs a check: a committed spec with problems moves the pointer, and the
  node keeps the last spec it used (#1741). The pointer before the first change is
  version 0 at the root of the tree of `Config::founding`. No BQ12 signature check on
  the change in this milestone (#1213). Trigger: `mesh::Pointer` moves to a layer 1
  crate in a refactor PR before a `wire` message carries it. `Mesh::open` runs no check
  of `Config::founding`: the founding is agreed region state, and a check at each open
  stops a node on a later build whose checks find more problems. The node that founds
  the region checks the founding with the `spec` function of #1841, and does not found
  a region whose founding has problems (#1744). A founding with problems at a later
  build follows the rule of a committed spec with problems (#1741). Decided by
  `laptop.architect`: chunks through `blob` and no BQ12 check, 2026-10-07T06:42:23Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6032512454); a
  spec with problems, 2026-10-07T07:03:20Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6032786065); the
  founding definitions, 2026-10-08T06:12:36Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6053614771); the
  kind, its byte form, the version from the base, `CHUNKS_MAX`, `Refused::Stale`, and
  the trigger for `Pointer`, 2026-10-08T08:22:08Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6055806836); no
  check of the founding at open, 2026-10-08T08:41:43Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056116151); the
  bound of an `Append` in bytes, 2026-10-08T08:44:55Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056167437), with
  "member" for "voter", 2026-10-08T08:46:16Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056189352).
- **SPEC APPLY (#1083)** `Mesh::apply(base, definitions)` makes the definitions, by
  tree key, the region's spec through the leader, as `set_home` does, and gives the new
  pointer. It first runs `spec::region::check` (REGION CHECK) at the region's prefix: a
  problem gives `Error::Problems`, which holds each problem as `check` gives it, and
  proposes nothing. `mesh` defines no problem of its own. It then builds the tree with
  `spec::region::tree`, and the change lists each chunk of the new tree. A tree of more
  than `CHUNKS_MAX` chunks gives `Error::Large { chunks, most }` and proposes nothing;
  #1741 decides how a change of more new chunks applies. A change that the state
  refuses gives `Error::Stale { base, pointer }`. A call learns the refusal of its own
  entry from `Applied`, which keeps the refusal of each applied entry above the lowest
  open floor of a try. Decided by `laptop.architect`, 2026-10-08T08:22:08Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6055806836).
  A refusal at the pointer that the call makes, after a lost answer, gives that
  pointer; a later pointer gives `Stale`. Decided by `laptop.architect`,
  2026-10-08T10:19:54Z
  (https://github.com/synnaxlabs/foundation/pull/1855#issuecomment-6057736427).
  The build with `spec::region::tree`, and the build of the root of
  `Config::founding` with it in `Mesh::open`, decided by `laptop.architect`,
  2026-10-08T08:41:43Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056116151).
  Supersedes the build with `spec::tree::apply` from `tree::empty()`
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6053614771).
- **RAFT SURFACE (#5, #91)** `raft::Raft::new(Config, Start)` builds a follower.
  `Config` holds the fixed inputs (key, tick counts). `Start` holds what the node had
  on disk: `hard`, `voters`, `entries` (the log from index 1), and `applied` (the
  last index the caller applied). `Hard` holds the term, the vote, the leader of the
  term (this node when it led), and the proof that moved the node to the term: its
  own pre-votes when it campaigned, else the proof of the message that moved it. A
  `Proof` is a `Grant` (pre-vote or vote), the candidate, and each voter's key with its
  `Signature`, the candidate included. It proves the term of the message or hard state
  that holds it. A granted `PreVoteReply` or `VoteReply` carries the voter's signature
  in its `Answer`, and the candidate copies it into its proof. `raft` counts the keys
  and carries the signatures as opaque bytes: it does no crypto. A signature attests
  a `Claim`: a `Grant` (the voter, the grant, the term, and the candidate) or a
  `Change` (the leader, the position of a configuration entry, and its voters; RAFT
  VOTERS). `raft` owns the rule that gives each signature its claim: a proof entry
  claims the proof's grant to its candidate in the term of the message or hard
  state, a granted reply claims its grant from the sender to the receiver in the
  message's term, and a configuration entry claims its change from the leader whose
  votes it holds. `Claim::signer` is the node whose signature a claim needs
  (architect, #881,
  https://github.com/synnaxlabs/foundation/pull/1187#issuecomment-6032591908).
  `raft` gives this node's own entries, grants, and changes with no signature
  (`None`).
  `Ready::sign` gives each `None` the signature that the caller's closure makes for
  its claim, in the hard proof, in each message, and in each change this node wrote
  (in `entries`, in `committed`, and in each append), before the write and the
  sends. `raft` keeps each claim that it makes unsigned, and a claim that it reads at
  start keeps its signature. So `Ready::sign` signs each copy of an unsigned claim that
  a `Ready` holds, and a resend again. Ed25519 gives each copy the same bytes. A data
  entry carries no claim, so the cost does not grow with the data rate. The most
  frequent case is a leader with a voter that does not answer: each heartbeat to it
  carries the votes, so the leader signs once for each tick (100 ms). A node that
  campaigned for its term signs each refusal of a lower term in the same way, because
  the refusal carries its own pre-votes. Lost: `raft` keeps the signed copy, which puts
  the signer in `raft` and changes the conformance oracle (architect, #1187,
  2026-10-07T15:25:42Z,
  https://github.com/synnaxlabs/foundation/pull/1187#issuecomment-6041049761).
  The caller checks each pair that `Raft::claims` gives before `step` and
  refuses a `None`: `step` keeps each signature as it came, so an unchecked `None`
  of another voter reaches `Ready::sign`. `Raft::claims` gives the claims `step`
  reads, in its order: the proof's grants, each link of the chain that `step` reads
  (its votes in the link's term, then the change), each change an append carries
  (its votes in the entry's term, then the change), then the sender's grant. It
  gives no claim of a message for a lower term or of a reply from a node that is not
  a peer, which `step` does not check. The list is the one `step` reads only when
  `step` gets the same message, with no call to the node between the two
  (architect, #881,
  https://github.com/synnaxlabs/foundation/pull/1187#issuecomment-6032381078,
  2026-10-07T06:32:04Z,
  https://github.com/synnaxlabs/foundation/issues/881#issuecomment-6030969579,
  2026-10-07T04:31:40Z, and
  https://github.com/synnaxlabs/foundation/pull/1488#issuecomment-6042831364,
  2026-10-07T17:08:02Z). Amended (approved by `laptop.architect`,
  2026-10-07T20:35:59Z:
  https://github.com/synnaxlabs/foundation/pull/1609#issuecomment-6046363822,
  2026-10-07T20:47:58Z:
  https://github.com/synnaxlabs/foundation/pull/1609#issuecomment-6046560645, and
  2026-10-07T20:51:39Z:
  https://github.com/synnaxlabs/foundation/pull/1609#issuecomment-6046617974,
  #1589): the rule is that a message `step` refuses or drops by its header gives no
  claim. So it also gives no claim of a message for another node, from this node, or
  from a second leader of this term (`Misrouted`, `Loopback`, `SecondLeader`). One
  predicate, `Raft::reads`, holds each refusal and drop by the header, and decides
  both. A grant or a proof that `step` reads past the header and then ignores is
  still a claim (#1613 holds the design that removes the class).
  `Entry::claims`, `Proof::claims(term)` and `Link::claims` give the claims of one
  entry, proof, or link in the same order, so `mesh` edits a message before `step`
  reads it (decided by `laptop.architect`, 2026-10-07T18:57:55Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6044730486, and
  2026-10-07T20:05:30Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6045861142).
  `Message.proof` carries one: a `Vote` carries the candidate's pre-votes; a leader's
  `Heartbeat` or `Append` carries its votes until the receiver answers an append, and
  again after the receiver is silent through a quorum check;
  an answer to a message of a lower term carries the sender's hard proof, and a node
  with no proof of its term sends no refusal. `step` checks a proof before anything
  changes. A `PreVote`, or a granted `PreVoteReply`, of a higher term needs none.
  Every other message of a higher term needs a proof that fits its body (a `Vote`
  the sender's pre-votes, a `Heartbeat` or `Append` the sender's votes, a reply any
  proof of the term) whose voters are a quorum of this node's configuration in
  force or last committed, or of a configuration that the message's chain proves;
  else `Error::Unproven`, and nothing changes or is sent. A leader claim in this
  node's own term follows RAFT LOG. A late pre-vote or vote of the term joins the
  proof its candidate carries. The chain: a message with a proof carries
  `Message.chain`, the configuration entries of the sender's log below the message's
  term, oldest first, each a `Link` (its position and its `Change`). A node whose
  configuration the proof is no quorum of reads the chain from its first link above
  its commit index, and stops at the first link whose configuration the proof is a
  quorum of. Each link it reads must have a term below the message's, rise from the
  position at the commit index or the last link read (the index rises, the term does
  not fall), hold `Vote` votes, hold a configuration with at least one incoming
  voter, and hold votes of a quorum of the configuration it trusts: the last link
  read of a lower term, else the node's last committed configuration entry of a
  lower term, else the configuration before its entries.
  The node keeps nothing from the chain: the leader's appends bring the entries. A
  voter that was down through a change so follows the leader that the change elected,
  and helps elect the next one (`raft/tests/it/behind.rs`). A node that took its term
  through another node's chain answers a stale message with its hard proof and its own
  chain, which can fall short of the sender's configuration: the sender then stays in
  its term until the leader's chain moves it, and the random runs check that a
  leader's heartbeat or append is never unproven (builder, #881, approved by the
  architect,
  https://github.com/synnaxlabs/foundation/pull/1488#issuecomment-6043521735,
  2026-10-07T17:45:39Z). Known gap: a node that a leave removed can reach, through
  pre-votes of the old configuration, a term that no configuration entry stands
  behind; a change that adds it back then needs its ack, and no node passes its term.
  The random runs reject such a run; the fix is #1485 (architect,
  https://github.com/synnaxlabs/foundation/issues/1485#issuecomment-6042768573,
  2026-10-07T17:05:00Z). A link is attested by its leader's signature and the votes of
  its term alone, so a voter that led a term can sign a configuration entry it never
  wrote, and prove any term with it: `raft` trusts its voters until #882, which gives
  a link the signed acks of a quorum, and `prove` counts them. The test
  `a_voter_that_led_a_term_can_forge_a_link_to_itself_and_prove_any_term` pins the gap
  (architect,
  https://github.com/synnaxlabs/foundation/pull/1488#issuecomment-6043096423,
  2026-10-07T17:22:17Z). The chain excludes a leader that a change the node missed
  made a voter (#1096). The log keeps the indexes of its configuration entries, so a
  chain costs their number, not the log's: the plan deferred the index until a
  measured scan on a large log (builder, #881,
  https://github.com/synnaxlabs/foundation/issues/881#issuecomment-6040930256,
  2026-10-07T15:19:54Z), and the review of PR 2 measured a leader of 7 voters with
  1,000,000 entries and 6 silent peers at 41 to 44 ms per tick with the scan
  (reviewer,
  https://github.com/synnaxlabs/foundation/pull/1488#issuecomment-6042889435,
  2026-10-07T17:11:00Z). With the index, the same tick, its `ready` included, takes
  0.33 µs (box1, Intel Xeon Platinum 8488C; `crates/raft/benches/chain.rs`). No
  test fails when `links` goes back to a scan: a scan allocates no more than the
  index, so no count sees it. The bench is the check until #715 gates it, and the
  gate's CI job must run it (builder, #881,
  https://github.com/synnaxlabs/foundation/issues/715#issuecomment-6044217325,
  2026-10-07T18:27:12Z; accepted by the architect,
  https://github.com/synnaxlabs/foundation/pull/1488#issuecomment-6044211313,
  2026-10-07T18:26:50Z).
  The advisor required a proof on every message and on each refusal, signatures
  only, and the proof in the hard state (#750, 2026-10-05). `mesh` signs and checks
  the signatures (MESH LOG).
  `Raft` takes `tick(random)`,
  `step(message)`, and `campaign()`, and gives `ready()`: a `Ready` with `hard` (only
  when it changed), `entries` to write, `committed` entries to apply, and `messages`
  to send. The caller writes, then sends, then applies, as etcd does: to apply first
  only delays the next round trip. A candidate counts its own vote at once because
  the write comes before the send. `hard()` stays a getter like `term()`. Randomness
  enters only through `tick`: a node draws its election timeout on the first tick
  after a reset. PreVote and CheckQuorum have no off switch. A PreVote answer, grant or
  refusal, shows the voter's state when it sent the answer. A grant that arrives after
  its voter got a lease back still counts, and costs one needless election; safety
  holds. etcd/raft counts such a grant too. Lost: a round number in `PreVote`, which
  changes the message format and closes only the case of two pre-campaigns. Decided by
  the advisor under the failover delegation on 2026-10-05 (#719). A node that may not
  campaign (RAFT VOTERS) still votes and follows.
  `step` does not check that a sender is a voter (a voter can learn late
  that a peer joined), so the caller authenticates the sender and decides which nodes
  may send. `step` drops a reply with no check when its sender is not in `voters()`,
  unless the configuration in force removed the sender and the node still sends to it:
  only `raft` knows whom it asked (#352). A node that may send can stop a group for good
  with one message in term `u64::MAX`: each node writes that term, and none can
  campaign. `raft` takes the term as it is. It trusts its voters: one that lies can
  already break safety, because a false `AppendReply` counts as held, so a bound on the
  term would guard nothing. No bound on a term jump spares an honest node that was down,
  either. Later: a sender proves a term jump by a signed term, which needs `mesh`
  (#750). The person decided on 2026-10-05 ("(a) is fine", #352 item 2). When the term
  of the last entry is above `hard.term`, `Raft::new` starts at that term with no vote.
  The node sends nothing before its write, so no peer counted a vote or an answer that a
  lost `hard` held. The caller writes `hard` and `entries` in any order, with no atomic
  write. Lost: the `Ready` doc requires `hard` before `entries`, a patch that each
  caller must keep and that shows only at a restart. The person decided on 2026-10-05
  ("I approve long term fix on 522"), #522. `Raft::removed` says whether a committed
  configuration removed a node: the configuration before the entries or a committed
  `Voters` entry held it, and the last committed configuration lacks it. `mesh` is to
  ask it at a refusal and keeps no copy of the configurations (#1105, #1762).
  `Voters::contains` and `Voters::nodes` are public. Decided by `laptop.architect`,
  2026-10-08T03:04:33Z:
  https://github.com/synnaxlabs/foundation/pull/1762#issuecomment-6051316777. After
  compaction, a snapshot also carries the nodes that the configurations it replaces
  held or removed, so the answer survives a trim (#253; `laptop.architect`,
  2026-10-08T03:41:50Z:
  https://github.com/synnaxlabs/foundation/pull/1775#issuecomment-6051704590).
  Supersedes the place of `held` in `mesh` in part 2 of
  https://github.com/synnaxlabs/foundation/issues/1105#issuecomment-6050855747 and
  finding 2 of
  https://github.com/synnaxlabs/foundation/pull/1762#issuecomment-6051260164.
- **RAFT LOG (#91)** A leader takes `propose(data)` and returns the entry's `Position`,
  or `Error::NotLeader { leader }` with the leader it knows. A new leader writes an
  empty entry of its term first, so it can commit what came before. It replicates with
  `Body::Append { prev, entries, commit }`, answered by `Body::AppendReply { last }`
  (the last index the follower holds of what was sent) or `Body::AppendReject { hint }`
  (its hint for the next `prev`). `step` checks a message against the log before it
  changes state: entries that do not follow `prev` are `Error::EntryOutOfOrder`, and a
  heartbeat's `commit`, an append reply's `last`, or an append reject's `hint` past the
  log is `Error::IndexPastLog`, unless `step` drops the reply (RAFT SURFACE). An
  append's `prev` and `commit` and a vote's `last` can be past the log of a node that is
  behind. An `Append` with an entry whose term is above the message's term is
  `Error::TermBehindLog`: no leader sends one, so the sender is faulty. The conformance
  oracle changed to match; the person decided on 2026-10-05 ("a is fine", #232). A
  heartbeat or an append of this node's term from a node other than the leader it knows
  is `Error::SecondLeader`: one term has one leader, and a node keeps the leader of its
  term until the term ends, through a step-down and a campaign. A node that knows no
  leader of its term takes the first that proves a quorum of its votes, else
  `Error::Unproven`; `Hard.leader` keeps it through a restart (#750). The person
  approved it on 2026-10-05 ("Yeah that's fine", #391). A bad message changes nothing.
  A voter that does not lead cannot make a node follow it: a leader claim needs a
  quorum of grants (RAFT SURFACE, #750), except a voter that led a term at or above
  the node's committed one, which can forge a link until #882 (RAFT SURFACE). After a
  restart, the committed term is the term at the applied index, because `Hard` holds no
  commit index. That term can be lower than the term at the commit index before the
  restart, so more past leaders can forge a link. Lost: the commit index in `Hard`. It
  costs one more durable write each time the commit index moves, for a gap that #882
  closes. Also lost: a bound of the highest term in the stable log. It refuses a real
  leader whose link has a lower term than an entry of the node that is not committed.
  Decided by `laptop.architect` (#1682, 2026-10-08T01:03:46Z):
  https://github.com/synnaxlabs/foundation/pull/1682#issuecomment-6050014758.
  A false `AppendReply` still counts as held (#882). Lost: a lease that drops a
  heartbeat or an `Append` of a higher term from a node that is not the leader. A
  reply of a higher term ends any node's lease, and a leader must step down on one;
  the lease also changed three etcd oracle tests. The
  coordinator decided on 2026-10-06 under the person's delegation (#391). The person
  may change it.
  `Body::Heartbeat { commit }` carries the commit index, capped at what that follower
  is known to hold. A leader commits an index only when a quorum holds it and its
  entry is of the leader's own term. A follower commits no further than the last
  entry the leader sent it. `Ready.committed` gives each entry once, after it is
  written. Batch size (64 entries) and the number of appends in flight per follower
  (8) are constants, not `Config` fields: nothing measured asks for a knob. `Message`
  and `Body` are `Clone`, not `Copy`, because an append carries entries.
- **RAFT DURABILITY (#352)** `raft` is safe only when a disk keeps what it synced. The
  disk owns that (`env::files`); `mesh` writes each `Ready` there. `raft` does not
  find a loss. When a follower's disk lost synced entries, and what it applied of
  them, the leader still counts them. While the leader's commit is below the
  follower's last entry, the follower follows, and the leader can commit an entry
  that fewer than a quorum hold. Once the commit passes that entry, each heartbeat
  gives the follower `Error::IndexPastLog`; an append to it fails with no error. A
  loss that keeps `applied` fails at `Raft::new` with `Error::AppliedPastLog`. `node`
  shows the error in its status (#648). Lost: the leader sends again from below what
  it counted, which lowers its count under a commit that a quorum may no longer
  hold. The person left the choice to the coordinator on 2026-10-05 ("your choice",
  #352 item 3), and the coordinator chose this. Later, at low priority: the leader
  learns the follower's real last index and stops counting lost entries (#663).
- **RAFT VOTERS (#193)** `Start.voters` is a `raft::Voters { incoming, outgoing }`,
  the etcd joint configuration: `incoming` is the voter set, and `outgoing` is the set a
  joint phase replaces, else empty. An election, a commit, and a leader's quorum check
  need a majority of each non-empty set. Each set is a `BTreeSet`, so a duplicate cannot
  exist and the order is fixed. An empty `incoming` with an `outgoing` is
  `Error::EmptyIncoming`; both empty is a node that only follows. etcd's quorum tables
  are the oracle for the quorum math (`oracles/conformance/raft/quorum/`). A node only
  in `outgoing` still campaigns, so a leader keeps its lead through its own removal. A
  configuration travels in the log: `Entry.data` is a `raft::Data`, one of `Empty` (a
  leader's first entry of its term), `Bytes` (a proposal), or `Voters(Change)`. A
  `Change` is the `voters`, the `votes` of the leader that wrote the entry (its election
  proof as it held it at the write: a vote that arrives later joins the leader's proof,
  not an entry it already wrote), and the leader's `signature` of the entry (`None`
  until `Ready::sign`). A node that missed the change checks the entry with them as a
  link of a chain before it counts a later proof against it, and refuses a link whose
  votes are not `Vote` (RAFT SURFACE; architect, #881,
  https://github.com/synnaxlabs/foundation/issues/881#issuecomment-6030969579,
  2026-10-07T04:31:40Z). A node uses the latest `Voters` entry in its log from the time
  it writes it; `Start.voters` is the configuration before `Start.entries`. A node that
  joins starts with the founding voters from the answer to its join (decided by the
  architect, #242:
  https://github.com/synnaxlabs/foundation/issues/242#issuecomment-6030855135). An empty
  `Start.voters` is a voter that an operator wiped. It takes any proof until it holds a
  `Voters` entry (#1004). Then its first `Voters` entry shows the configuration before
  the entries: a joint entry's outgoing set, or for a leave its own set (#928,
  coordinator, 2026-10-06). A log starts at index 1, so that entry is the joint entry of
  the group's first change, and the node checks proofs as a founder with the same log
  does, gaps included (#881, #1005). Lost: an empty committed set proves nothing (the
  node then refuses a leader that the outgoing set elects when the old leader fails
  before the joint entry commits); a joining node starts with the group's current
  configuration (the caller must know it, and it removes the operator's recovery of a
  wiped voter); the founding configuration as entry 1, as in etcd (a wider change that
  alone leaves the node open until it holds that entry). A `Voters` entry with an empty
  `incoming` set, in `Start.entries` or in an `Append`, is `Error::NoVoters`: a group
  with no voter can never commit or elect. A leader changes the voters with
  `Raft::propose_voters(set)`: it writes the joint configuration (`incoming` the new
  set, `outgoing` the current one) and, when that entry commits, the leave (`incoming`
  alone). One change at a time: while the last configuration entry is not committed, a
  proposal is `Error::ChangePending`. A node the change removed stays a peer of the
  leader, and gets appends up to the leave, or up to the leader's first entry when that
  is later, until it holds them and the leave is committed: then the leader sends it the
  commit in a heartbeat and releases it, so the node learns it is out and never
  campaigns. The leader's first entry replaces each entry that an older leader left past
  the leave on the node, such as a configuration that makes it a voter again. A removed
  node that answered nothing over a whole quorum check period is released at that check
  instead, and the next configuration releases any that is still a peer. A follower
  releases the removed nodes when the leave commits. A removed node that missed its
  release learns it from `mesh`, not `raft`: `mesh` admits a `raft` request only from a
  voter of the newest configuration that the node knows: the newest in its log, or a
  newer one that a link of the request's chain proves. A link proves a configuration
  when the votes it carries elected its leader under a configuration the node already
  knows, and the leader's signature holds (#881). A configuration entry binds the public
  key of each voter it adds, under the signature of the leader that writes it. A node
  answers `removed` only to a sender that a configuration in its own log held, when its
  committed configuration lacks the sender and the request proves no newer configuration
  that holds it. Each other sender that is not a voter gets `Error::NotVoter`, and does
  not stop. The person chose A, 2026-10-07T04:49:33Z
  (https://github.com/synnaxlabs/foundation/issues/1096#issuecomment-6031153921; the
  text, https://github.com/synnaxlabs/foundation/issues/1096#issuecomment-6031072285).
  Supersedes the first version, which the person approved on 2026-10-06
  (https://github.com/synnaxlabs/foundation/pull/647#issuecomment-6007546638). #1105
  builds the `removed` answer and the held rule, #1106 the key binding, and #1107 the
  configuration that a chain proves. Until #1107, a request proves no newer
  configuration. A node answers `removed` only to a sender that `Start.voters` or a
  committed `Voters` entry held, when the last committed configuration lacks it. A
  sender that only an uncommitted entry held gets `NotVoter`: the entry can still be
  truncated, and a node whose commit lags would stop a voter that no committed
  configuration removed, the failure of #1054 (decided by the architect,
  2026-10-08T02:21:15Z:
  https://github.com/synnaxlabs/foundation/issues/1105#issuecomment-6050855747). `mesh`
  asks `Raft::removed`, so the log that holds the configurations answers it (decided by
  `laptop.architect`, 2026-10-08T03:04:33Z:
  https://github.com/synnaxlabs/foundation/pull/1762#issuecomment-6051316777).
  `Raft::removed` counts a configuration entry only once the commit index covers it, so
  a node that opened again answers `NotVoter` until a leader gives it the commit index.
  A request is a PreVote, a Vote, a heartbeat, or an append. The rule covers requests
  only, and `raft` decides which replies count (RAFT SURFACE). The coordinator gives the
  person's words on the first version in its comment on #647, linked above. The removed
  node takes that answer only from a voter of its own region, and stops its `raft` group
  for that region. A voter of its region is a voter of the newest configuration in its
  log, and an answer from any other node drops the stream, as any refusal does (decided
  by `laptop.architect`, 2026-10-08T02:59:07Z:
  https://github.com/synnaxlabs/foundation/pull/1762#issuecomment-6051260164). `raft`
  sends such a node no entries, only answers. A voter with a lease drops its campaign or
  refuses it with a `PreVoteReply` of `Answer::Refused` at the voter's term. In `raft`
  alone, the node campaigns. While a voter has a lease, this has no effect. Once no
  voter has a lease, as after the leader fails, the voters can elect the node: it
  commits an entry of its term, which commits the leave, and steps down, and the voters
  follow it until their election timeout. That gap stays in `raft`, pinned by
  `it::change::the_voters_elect_a_removed_node_once_the_leader_fails`. In `mesh`, the
  admission check and the `removed` answer close it for each voter whose log holds the
  leave; a voter whose log lacks the leave entry admits the request until #1107. #483
  keeps the stop on applying a committed configuration without itself. Keeping readmit
  until then lost. The person decided on 2026-10-05 ("(a)"), #482. Readmit in `raft`
  (#414) lost: it sent the log to a sender that `raft` cannot check. The person decided
  on 2026-10-05 ("Ok B is fine", #193). A leader outside the committed final set sends
  the commit and steps down. A node may campaign when it is a voter, incoming or
  outgoing, of the configuration in force, or, while that configuration is not
  committed, of the configuration before it. No other node campaigns. The rule is exact:
  a leader appends a configuration entry only after the last one commits, so by Log
  Matching only the last configuration entry in a log can be truncated, and the one
  before it is committed. The fallback keeps two cases: a truncation gives the
  configuration before back, and a removed leader that lost its lead before the leave
  reached a peer is the only node that can win the election that commits it. A follower
  whose commit index lags lets the configuration before campaign for longer, which costs
  liveness, never safety. The `mesh` admission check stays beside this rule:
  `promotable` decides whether an honest node campaigns, and `mesh` checks a sender that
  may lie, because `raft` never checks senders (RAFT SURFACE). Neither is a second guard
  for the other. Decided by the coordinator and the advisor on 2026-10-06 (#659). After
  compaction a snapshot carries the configuration in force at its index, so the
  configuration before the last entry stays known (#253).
- **MESH LOG (#471)** `mesh` keeps the `raft` hard state and log of a region in the
  files `log-0`, `log-1`, and so on of one directory. A log also holds the file `lock`
  of that directory open for writing, from its open until it drops. A file call of a
  dropped write can end after the drop, and the lock does not cover it (#1375). The file
  has no bytes, and one with bytes fails the open (`Files(Length)`). So a second open of
  the directory gives `Error::Log` with `Busy` on the lock, at each time, whatever log
  file the first log holds (#1360, decided by `laptop.architect`, 2026-10-07T12:07:03Z:
  https://github.com/synnaxlabs/foundation/issues/1360#issuecomment-6037555764). Any
  other file there is `Error::Stray`. Supersedes
  https://github.com/synnaxlabs/foundation/pull/549 in its clause that each file but
  `log-<n>` is `Error::Stray`. One write of `raft` is one record: a header, then the
  body. The header is an 8-byte check of the rest of the header (the first bytes of
  `types::digest::Digest::of`), the format version (1, C9d), the record's number, the
  body length, and an 8-byte check of the body. The body holds the hard state, when it
  changed, and the entries, so one sync makes both durable; two slots for the hard state
  lost, because they need a second sync and a second torn-write rule. The hard state is
  the term, then the vote, the leader, and the proof, each behind a presence byte; the
  proof is a grant byte, the candidate, a count of voters, then each voter's key (16
  bytes) and signature (64 bytes) in rising key order (#750). A `raft` message on the
  wire carries its proof in the same form, after the term and before the body, then
  its chain: an 8-byte count of links, always present, then each link's position (8
  bytes of term, 8 of index) and its change in the entry form below, from the
  incoming keys to the signature. A link of a joint entry with 3 incoming, 3
  outgoing, and 3 signed votes is 457 bytes, and a chain of 2 such links is 922
  bytes with its count (architect, #881,
  https://github.com/synnaxlabs/foundation/issues/881#issuecomment-6030969579,
  2026-10-07T04:31:40Z). A
  granted `PreVoteReply` or `VoteReply` is the byte 1, then the signature; a refusal
  is the byte 0 alone. An entry is its term and index (8 bytes each), then a data
  byte: empty (0) alone; bytes (1), an 8-byte length, and the bytes; voters (2), the
  incoming keys, the outgoing keys (each an 8-byte count, then the keys in rising
  order), the votes in the proof form, and the leader's signature (64 bytes). No
  form holds a grant or a change with no signature: encode panics on one, because
  the caller signs before each write and send. `mesh::claim` signs each claim with
  the node's Ed25519 key. A grant signs `foundation/grant/1`, the voter (16 bytes,
  little endian), the grant byte (pre-vote 0, vote 1), the term (8 bytes, little
  endian), and the candidate (16 bytes, little endian). A change signs
  `foundation/voters/1`, the leader (16 bytes), the term and the index (8 bytes
  each), and the incoming and the outgoing keys, each with its count as in the
  entry, not the 4-byte count of the plan: one form for both (architect, #881,
  https://github.com/synnaxlabs/foundation/pull/1187#issuecomment-6032591908).
  The signer in the bytes keeps two members that share a key from sharing a
  signature. Grants name no region; a second region adds the region key under
  `foundation/grant/2`. The driver (#471) checks each
  claim of a message before each `step` against the key of its signer: the key of a
  member in the applied state, else the key that each join of that node in the log as
  `raft` holds it and not applied names, when all of them name one key (decided by
  `laptop.director`, 2026-10-07T12:48:00Z and 2026-10-07T13:10:50Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038235423 and
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038649429). An
  entry that replaces a join removes its key, at the step that replaces it. Two joins
  that name two keys: MESH DRIVER states the rule. The log
  is the one that `raft` reads its configuration from, so each `step` and each proposal
  syncs the keys from `Raft::unstable` before the write (decided by `laptop.architect`,
  2026-10-07T13:31:26Z:
  https://github.com/synnaxlabs/foundation/pull/1400#issuecomment-6039027801). The same
  key serves the check of the sender of a message (`Error::Spoofed`) and of the peer of
  a forwarded proposal (`Error::PeerNotVoter`) (decided by `laptop.architect`,
  2026-10-07T13:23:59Z:
  https://github.com/synnaxlabs/foundation/pull/1400#issuecomment-6038887227). The
  format version stays 1: no log has shipped. A later record replaces the
  entries from its first index. A file is 1 MiB, or the length of the record that the
  log made it for when that is more. A record that does not fit starts the next file.
  In a file with no record, it makes that file again, larger, so each file but the last
  holds a record. A file with no bytes, which a crash in a create can leave (ENV SEAMS,
  #1264), is a file with no record. After a stopped write that made a file, the next
  record starts that file. Decided by `laptop.architect` (2026-10-07T10:38:51Z, the
  last sentence at 2026-10-07T11:05:15Z, and the sentence on a file with no bytes at
  2026-10-07T18:39:26Z):
  https://github.com/synnaxlabs/foundation/pull/1284#issuecomment-6036182314,
  https://github.com/synnaxlabs/foundation/pull/1284#issuecomment-6036600297, and
  https://github.com/synnaxlabs/foundation/issues/1264#issuecomment-6044422561. A write
  puts its record in blocks, one block of the pool at a time and of 64 KiB at most, from
  the end of the record to its start, and then syncs one time. Each block but the one at
  the end of the record ends at a multiple of the block size in the file, so no two
  blocks share a sector. The block with the header is the last that it writes, so a
  write that the pool stops (`Error::Pool`) leaves no header: the log holds what it
  held, and the bytes of the stopped write stay after its end. Decided by
  `laptop.architect` (2026-10-07T09:12:02Z):
  https://github.com/synnaxlabs/foundation/pull/1284#issuecomment-6034784720. So a pool
  that opens holds each write. A write that a file call fails, or that its caller drops,
  poisons the log (`Error::Poisoned`), and a write that the pool stops does not. The
  next write puts zeros over the bytes of the stopped write and syncs, and only then
  writes its record: with one sync, a power cut can keep the record and not the zeros
  (SIM CRASH), and an open then reads the old bytes as a header (PR 1 of #1091, approved
  by the architect, 2026-10-07T05:29:41Z:
  https://github.com/synnaxlabs/foundation/issues/1091#issuecomment-6031627973, and the
  text of this rule, 2026-10-07T08:42:04Z:
  https://github.com/synnaxlabs/foundation/pull/1284#issuecomment-6034282653). A header
  never crosses a `SECTOR`: a record whose header would cross one starts at the next
  sector. A power cut keeps each sector whole or not at all (SIM CRASH), so a header is
  whole or absent. At a restart, zeros where a record should start, or a good header
  with a torn body, are the end of the log. Anything else is `Error::Corrupt`, and the
  node does not start. So is a record that starts right after a torn one, or at the
  start of the next file: the error is at the torn record, whatever the version of the
  record after it. A record of another version that is the first defect in file order
  is `Error::Version` (#1784, approved by the architect, 2026-10-08T04:26:06Z:
  https://github.com/synnaxlabs/foundation/pull/1776#issuecomment-6052206016). Open
  writes again, whole, the end file that it finds: the records as it read them, then
  zeros to the end of the file. So a torn record leaves nothing that a later open reads
  as a header. Each read and each write of the open is whole sectors, so a header gets
  one write. An open with a pool whose largest block is less than one sector gives
  `Error::Pool(TooLarge)` before it reads or makes a file. Then it syncs the end file,
  the directory, and its parent, because `raft` acts on what open gives and a crash can
  leave any of them with no sync. The write is there because a read sees, from the
  cache, the writes that a failed sync of this boot lost, and a later sync does not
  write them (SIM CRASH): an open that only syncs gives records, or keeps zeros, that
  the disk does not hold (#1066; the ring has the same rule, #698). Each file before the
  end file is durable, because a failed write poisons the log, and the next open has the
  file of that write as its end file or removes it. An open of a log that has a file
  thus writes and syncs 1 MiB or more, for each region. P1 gives a Raspberry Pi 4 under
  1 s to start, and no one has measured this cost there (#1140). Lost: zeros only after
  a torn end (the first shape), which is the defect; and a read with direct I/O, which
  not each driver can give: macOS does not promise a read that skips the cache (decided
  by the architect, #1128, 2026-10-07T05:37:30Z:
  https://github.com/synnaxlabs/foundation/issues/1128#issuecomment-6031715225). One
  check over the whole record lost: a damaged length then reads as a torn end, and the
  log drops the good records after it. Zeros over the header of a durable record, which
  only a disk fault makes, read as the end, and open drops the records after it in that
  file. A search past the end for a record lost: a body can hold the bytes of a record,
  so a power cut could then stop the node. Nothing trims the log until snapshots (#253).
  `mesh` depends on `block` for the blocks of its file calls. Decided by `consensus`.
  `mesh::testing::round_trip_log_record` and `seal_log_record`, behind the `sim`
  feature, give the fuzz target `mesh_log` the decode and encode of one record; the
  seal writes the length and both checks, with the log's own check (approved by the
  architect, 2026-10-08T01:49:20Z:
  https://github.com/synnaxlabs/foundation/issues/1711#issuecomment-6050509924; the
  doc of the seal that writes the length, 2026-10-08T03:38:57Z:
  https://github.com/synnaxlabs/foundation/pull/1740#issuecomment-6051674927. Supersedes
  https://github.com/synnaxlabs/foundation/issues/1711#issuecomment-6050509924 for the
  doc of the seal).
- **MESH WIRE (#471)** `mesh` encodes what two nodes of a region say on a stream of
  `wire::Protocol::Mesh`, behind the `wire` stream header: a `raft::Message`, a proposal
  that a follower forwards to the leader, and its two answers (the position of the
  entry, or "not the leader" with the leader the receiver knows). `wire` does not carry
  them: the Rust SDK reuses `wire`, a client never opens a mesh stream, and `wire` must
  not depend on `raft`. The encoding in `raft` lost: `raft` cannot see the format
  version. A `raft` message travels on a one-way stream. A member forwards a proposal as
  one two-way stream of `Class::Command`: the stream carries one proposal, the leader
  writes one answer on its reply half, and then both halves end. So a proposal and its
  answers name no sender and carry no request number (#779): a request number makes the
  node that asks keep and remove open requests and handle a late answer (CLOCK WIRE),
  and HUB WIRE already answers on the stream that asks. A stream breaks the protocol
  when a message is the byte form of no message, when a one-way stream carries a
  proposal or an answer, when a two-way stream does not start with a proposal, or when
  it carries a second message. The receiver stops the stream with code 2
  (`wire::header::MALFORMED`), the code that HUB WIRE gives for a broken protocol rule
  (decided by the architect, 2026-10-07T08:15:18Z:
  https://github.com/synnaxlabs/foundation/pull/1263#issuecomment-6033866025, and
  2026-10-07T09:42:17Z:
  https://github.com/synnaxlabs/foundation/pull/1263#issuecomment-6035294122, point 3).
  A second message comes after the answer, and a reset takes back an answer that the
  peer does not have yet. So there the receiver stops only the half that it reads. At
  each other break of a two-way stream, it stops the half that it reads and resets its
  reply half, each with code 2: a two-way stream that ends with no message is such a
  break (approved by the architect, 2026-10-07T12:52:00Z:
  https://github.com/synnaxlabs/foundation/pull/1386#issuecomment-6038303084). A message
  that the group refuses (MESH DRIVER) changes nothing, and the receiver stops the
  stream with code 16, the first code of the mesh protocol (PROTOCOL HEADER), and resets
  a reply half with the same code. A request from a node that a committed configuration
  removed, when `Start.voters` or a committed `Voters` entry held it (RAFT VOTERS), gets
  code 17 instead (`Error::Removed`). A group that stopped gives code 16 on a one-way
  stream. On a stream that goes both ways it gives no mesh code: it can stop in the
  write of the entry, which then applies after a new open. A `raft` message that finds
  no block in the pool is not a refusal: the receiver drops it, the stream goes on, and
  `raft` sends it again. The receiver holds no block while the group writes the entry:
  it drops the block of the proposal before it gives the change to the group, and takes
  the block of the answer after the answer. With no block for the answer, the peer gets
  no answer: the group can hold the entry of the proposal. The reply half ends with no
  answer and no mesh code. A reply half that ends with no answer and with no code 2 or
  16 says nothing about the change, and the peer forwards it again. Lost: the block of
  the answer first, because a block held while the group writes can take the room that
  the write needs, and only the end of the write frees it (decided by the architect,
  2026-10-07T13:07:00Z:
  https://github.com/synnaxlabs/foundation/pull/1386#issuecomment-6038576823). The
  sentences on a group that stopped and on an answer with no block are from a later
  ruling. Lost there: code 16 that says nothing about the change after a stop, because
  code 16 carries no cause, so the peer cannot tell a stop from a refusal (decided by
  the architect, 2026-10-07T14:17:49Z:
  https://github.com/synnaxlabs/foundation/pull/1386#issuecomment-6039908458).
  Supersedes, for a stream that goes both ways, point 2 of
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037318501, and the
  sentence "the group took the proposal" of
  https://github.com/synnaxlabs/foundation/pull/1386#issuecomment-6038576823. The
  receiver does not check the class of a stream: the class sets only the priority of the
  sender (approved by the architect, 2026-10-07T11:52:00Z:
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037318501). A
  message has one byte form, and a decode takes nothing else. The log (MESH LOG) and the
  messages share the byte form of an entry. Decided by `consensus`, approved by the
  coordinator (#471). `mesh::testing::round_trip_change`, behind the `sim` feature,
  gives the fuzz target `mesh_change` the decode and encode of a change record; no
  change type is public (decided by the architect, 2026-10-07T11:17:12Z:
  https://github.com/synnaxlabs/foundation/issues/1339#issuecomment-6036785855).
  `mesh::testing::round_trip_message` and `round_trip_entries` give the fuzz targets
  `mesh_message` and `mesh_entries` the decode and encode of a message and of entries
  one after another, in the same way (approved by the architect, 2026-10-08T01:06:45Z:
  https://github.com/synnaxlabs/foundation/issues/1470#issuecomment-6050048371). The
  module `change` holds the change records and their byte forms (`Change`, `Join`,
  `Malformed`, `Unknown`). The module `region` holds the state that they move (`State`,
  `Request`, `Refused`, `Unfit`). One module for both lost: `region::Unknown`, a change
  of no known kind, was not clear next to `region::Refused::Unknown`, a ticket that is
  not recorded (decided by the architect, 2026-10-07T16:24:54Z:
  https://github.com/synnaxlabs/foundation/issues/1051#issuecomment-6042136383).
- **MESH DRIVER (#471)** `mesh` runs the `raft` group of one region as one task, on the
  shard that opened it. The task waits for a tick or a `Ready`, and does each `Ready` in
  the order of RAFT SURFACE: sign, write and sync, queue the messages, apply. A ticker
  task and a writer task lost: they need a second waker. A tick is 100 ms, a heartbeat
  is 1 tick, and an election timeout is 10 ticks. A tick that comes due in a write is
  lost, so the group's time only slows. Before each `step`, `mesh` checks a message in
  this order: the peer holds the key of the member that the message names
  (`Error::Spoofed`), a request comes from a voter of this node's configuration
  (`Error::Removed` for a sender that a committed configuration removed, else
  `Error::NotVoter`), and each claim holds (`Error::Claim`), the claims being what
  `Raft::claims` gives, so a link that the node does not read is not checked. So a
  node with a configuration refuses a leader that is not a voter of that
  configuration, when a change that the node does not hold made that leader a voter.
  The node does not get the log from that leader (a known defect, #1096, that #1107
  fixes). A leader that stays a voter through the change passes the check, and its
  chain proves the change (RAFT SURFACE). A node with no
  configuration takes no request. Only a voter that an operator wiped is such a node
  (#881), because a node that joins opens with the founding voters from its join answer
  (decided by the architect, #242, 2026-10-07T04:20:40Z:
  https://github.com/synnaxlabs/foundation/issues/242#issuecomment-6030855135). A claim
  in the proof whose signer has no key at the node, or whose signature does not hold
  under a key from a join that is not applied, is removed before `step`. An append is
  cut before the first entry with such a claim, and the entries after the cut are not
  checked or stepped. A claim of an applied member with a bad signature refuses the
  whole message (`Error::Claim` with `claim::Error::Forged`) (decided by
  `laptop.director`, 2026-10-07T20:02:05Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6045806233. This
  supersedes rule 3 of
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6042828979, which
  superseded rule 3 of
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038235423). In
  the chain, such a vote of a link is removed, and the chain is cut before the first
  link whose change is such a claim. `mesh` makes each change before `Raft::claims`,
  and steps the message that it checked (decided by `laptop.director`,
  2026-10-07T17:18:49Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6043037608). A
  grant of a reply that does not hold under the key of its sender refuses the reply
  (`Error::Claim` with `claim::Error::Forged`), also when the key comes from a join
  that is not applied: the sender check proved that the peer holds that key
  (decided by `laptop.architect`, 2026-10-07T20:37:34Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6046390090).
  When the unapplied joins of a signer name two keys, its key is the key of the
  joins below the first configuration entry, in the log as `raft` holds it, whose
  incoming half names the signer, when those joins name one key, else none: the
  leader applied the real join before it wrote that entry, so Log Matching puts the
  real join below it in each log, and a join above it can be a forgery. The sender
  check and the claim check both use this lookup (decided by `laptop.director`,
  2026-10-07T20:44:24Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6046503082.
  Supersedes the two-keys sentence of
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038611630).
  The incoming half is enough: `raft` makes the outgoing half of an entry from the
  incoming half of the configuration in force, so a signer that only an outgoing
  half names is in the incoming half of an earlier entry, or of the applied
  configuration, and then its join is applied (decided by `laptop.architect`,
  2026-10-07T20:49:33Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6046585323).
  Triggers: a change kind that removes a member, or a change to how `raft` makes
  the outgoing half, states this rule again. A link of the chain proves the entry
  in the log of its sender, not the entries below its position in the log of the
  receiver, so the lookup never reads a link as a configuration entry: when the
  two joins are in the log and the entry that names the signer is in the chain
  only, the vote of that signer is removed. With voters that do not lie, the limit
  is in liveness only, and #1623 is the sound fix, designed with #336 (decided by
  `laptop.director`, 2026-10-07T21:03:30Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6046807570). A
  voter that lies can write a join with a key that it holds, and when that join is
  the only unapplied join of its node, the node takes that key, until #882 (decided
  by `laptop.architect`, 2026-10-08T00:52:26Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6049888903.
  Supersedes the sentence "the node never counts a wrong key" of
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6046807570).
  Two joins below that entry still strand a follower under a leader that the real
  node elected, until #336 builds the voter that checks a join before it stamps it.
  A hard proof that lost such a claim can be no quorum at a node with a newer
  configuration, which then learns the term from the leader. A follower answers a
  cut run with the last entry it kept, and the leader sends the rest from there.
  `propose_voters` refuses a set with a node that is not a member in the applied
  state of this node (`Error::NotMember`, the first such key), so each log
  that holds the `Voters` entry holds the join of each of its voters before it, and the
  join applies the same on each node (decided by `laptop.director`,
  2026-10-07T12:48:00Z and 2026-10-07T13:10:50Z, with the error of `laptop.architect`,
  2026-10-07T12:48:43Z and 2026-10-07T13:08:48Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038235423,
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038649429,
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038247134, and
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038611630).
  `propose` returns the position of its entry only after the write that holds the entry
  ends: a lone voter leads before its term is on disk, and after a power cut the same
  position can hold another change. A second call that waits for the write lost: no
  caller needs a position that is not on disk, and a caller that skips the wait gets
  that defect again (decided by the architect, 2026-10-07T08:18:51Z:
  https://github.com/synnaxlabs/foundation/pull/1263#issuecomment-6033920665). When the
  append of a new leader replaces the entry before a write holds it, `propose` gives
  "not the leader": the task tells each proposal whether the `Ready` that it wrote held
  the entry, by index and term, and the first `Ready` after the call decides (approved
  by the architect, 2026-10-07T10:17:54Z:
  https://github.com/synnaxlabs/foundation/pull/1263#issuecomment-6035860357). A node
  that gets a forwarded proposal (MESH WIRE) proposes the change, and its answer is the
  position, or "not the leader" with the leader that it knows. A proposal from a peer
  whose key no voter of this node's configuration holds is refused
  (`Error::PeerNotVoter`, with the key of the peer, because a forwarded change names no
  sender; `Error::NotVoter` names the sender of a message; decided by the architect,
  2026-10-07T10:38:49Z:
  https://github.com/synnaxlabs/foundation/pull/1263#issuecomment-6036181809); a
  member that is not a voter proposes with join (#336). The leader does not check the
  home of a forwarded change: `Error::NotMember` checks only the argument of a local
  caller, and the check of a home at apply on each node is #1273. A forwarded change
  applies at least one time: a member that got no answer forwards it again, and the
  leader then appends a second entry. A try of `set_home` that gives up resets its
  stream. A proposal that the network delivers late, before the reset, can still apply
  after a later call returned and set the older home, until #1273 refuses it (ruled by
  the architect, 2026-10-07T20:58:20Z:
  https://github.com/synnaxlabs/foundation/pull/1607#issuecomment-6046723849).
  Supersedes the sentence that a repeat of `Change::Home` gives the state of a call
  that took effect last:
  https://github.com/synnaxlabs/foundation/pull/1263#issuecomment-6033866025.
  `Change::Join` is safe to repeat while no
  change removes a member: a repeat finds its node a member and is refused
  (`Unfit::Duplicate`) before the ticket counts a use. The change that removes a member
  must keep a repeat of an older `Join` from admitting the node again, and needs a
  ruling before it lands (decided by `laptop.architect`, 2026-10-07T12:55:06Z:
  https://github.com/synnaxlabs/foundation/issues/336#issuecomment-6038355946). A later
  `Change` kind that is not safe to repeat needs a ruling before a member forwards it
  (decided by the architect, 2026-10-07T08:15:18Z:
  https://github.com/synnaxlabs/foundation/pull/1263#issuecomment-6033866025). The
  messages for one member wait in a queue of 64 that drops its oldest, because `raft`
  sends again. A write that finds the pool full (`block::Error::Exhausted`), or that the
  system refuses memory for (`Refused`), does not stop the group, because each may
  succeed later (MEMORY BOUNDS): the task writes the same `Ready` again at each tick,
  and until then no message leaves, nothing applies, and the group gets no tick. From
  the write that finds no block until the write ends, `propose` and `receive` give
  `Error::Pool` with the cause of the wait, so the group takes no proposal and no
  message, and what `raft` holds does not grow. A forwarded proposal that gets it did
  not reach the group. A leader that waits sends no heartbeat, so the other voters elect
  a new leader. A follower that waits answers no message and falls behind until its
  write ends (decided by the architect, #1091, 2026-10-07T05:29:41Z:
  https://github.com/synnaxlabs/foundation/issues/1091#issuecomment-6031627973; the doc
  text of the variant decided by the architect, 2026-10-07T14:17:49Z:
  https://github.com/synnaxlabs/foundation/pull/1386#issuecomment-6039908458, which
  supersedes the doc texts of
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037318501 and
  https://github.com/synnaxlabs/foundation/pull/1386#issuecomment-6038576823, and the
  Display text of the first (2026-10-07T11:52:00Z) stands; the text of the two cases by
  the architect, 2026-10-07T12:08:39Z:
  https://github.com/synnaxlabs/foundation/pull/1366#issuecomment-6037581525). The group
  checks the wait before each other check of a message or of a forwarded proposal, so
  each gets `Error::Pool` in a wait, also one that a check refuses with no wait. The
  other order lost: it gives the exact refusal, but the group drops each of them in a
  wait in both orders, and a node that is short of memory then also pays for the
  signature checks (decided by the architect, 2026-10-07T12:31:59Z:
  https://github.com/synnaxlabs/foundation/pull/1366#issuecomment-6037964937). A write
  holds one block of the pool at a time (MESH LOG), so no record is too large for a
  pool that opens, and a write does not wait for a block of its own (decided by the
  architect, 2026-10-07T08:42:04Z:
  https://github.com/synnaxlabs/foundation/pull/1284#issuecomment-6034282653). A free
  block of a size with a block in use keeps its budget (#291), so a write can wait while
  the budget has room for its block, until the other user of the pool drops its block
  (#1134). A pool whose largest block is less than one sector does not open (MESH LOG),
  so no write gives `TooLarge` and the group does not stop for it (decided by the
  architect, 2026-10-07T06:32:47Z:
  https://github.com/synnaxlabs/foundation/pull/1123#issuecomment-6032389760; the
  `Refused` wait decided by the architect, 2026-10-07T04:39:07Z:
  https://github.com/synnaxlabs/foundation/pull/1057#issuecomment-6031046531). A group
  stops when a write of the log fails, when a committed change has 0 bytes or a kind
  that this build does not know, or when each `Mesh` drops: this build cannot judge such
  an entry, and a newer build can. An entry with no change (the first entry of a leader)
  is not a change of 0 bytes. A committed entry of a known kind whose body does not
  decode is `Refused::Body` on every node, and the group goes on, so one voter that
  proposes bad bytes cannot halt the region. So a change to the body or to a cap of a
  known kind (the 64 status entries of a `Join`) takes a new kind, which writers use
  only after the format flag (C9d) allows it; a node of an older build stops at it and
  never applies it differently. Decided by `laptop.architect` (2026-10-07T10:55:00Z):
  https://github.com/synnaxlabs/foundation/pull/1328#issuecomment-6036422521. Each later
  call gives `Error::Stopped` with the first cause. A watch gives the `Stopped` itself,
  also after each `Mesh` drops (MESH SURFACE). `member` has no error (#562): it gives
  the record that the node holds, also after a stop (approved by the architect,
  2026-10-07T08:07:48Z:
  https://github.com/synnaxlabs/foundation/pull/1241#issuecomment-6033747689). A stopped
  group does not start again: the node opens the mesh again, and the open makes durable
  what it gives (MESH LOG). The task ends soon after the last `Mesh` drops, a write in
  progress ends first, and a write that waits for a block ends at the next tick; until
  then a new open gives `Error::Log`. Each open applies the log from index 1, until
  snapshots (#253). A watch does not keep the group running, and a dropped watch leaves
  no waker. `open` refuses a node or a voter that is not a member (`Error::NotMember`),
  and a private key that is not the key of this node's member (`Error::WrongKey`).
  `Config.members` is a list, and the region state holds each record under the key of
  its card, so the key of a member has one copy. `open` is the one check of a list for
  two records of one node: a decoder of a join answer passes its records on and does not
  check them again (decided by `laptop.architect`, 2026-10-07T08:07:47Z:
  https://github.com/synnaxlabs/foundation/issues/1259#issuecomment-6033747312, which
  reverses the map of the ruling below). The key of a member is the key that its card's
  signature covers, and `open` refuses a member that the region cannot hold, or two
  members with one key (`Error::Member`, with the `region::Unfit`). The signature does
  not show that the node owns its public key. The admission does, and `Join` (#336)
  refuses the `node::Key` of a member (decided by `laptop.architect`,
  2026-10-07T08:33:14Z:
  https://github.com/synnaxlabs/foundation/pull/1277#issuecomment-6034146773). `open`
  starts the tasks that send: one for each member, from the first message for it
  (approved by the architect, 2026-10-07T13:40:50Z:
  https://github.com/synnaxlabs/foundation/pull/1410#issuecomment-6039206881, which
  supersedes the start at open in the plan that
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037318501 approved),
  so a member that is slow holds only its own messages. `mesh` dials and `node` accepts:
  `node` gives each stream of `wire::Protocol::Mesh` to `serve`. A task dials a session
  to each member at the addresses of the member's card, as the group holds the card
  then, and sends each `raft` message as one message of one one-way stream of
  `Class::Command`, after the stream header (MESH WIRE). `mesh` never closes a session.
  A message that fails drops, with only the part that failed: the message when the pool
  has no block for it or when it is too large for the peer (#1361), the stream when the
  peer stopped it, and the handle of the session on each other error, also when no dial
  gives a session. The next message then opens a stream, or dials, again. Nothing sends
  the dropped message again, because `raft` does. A local pool error must not drop a
  session that other protocols use, and the one session for each pair of nodes is the
  job of `transport` (#1363) (approved by the architect, 2026-10-07T11:52:00Z:
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037318501). A
  message for a node of which the group has no record drops in the same way (approved by
  the architect, 2026-10-07T17:29:35Z:
  https://github.com/synnaxlabs/foundation/pull/1410#issuecomment-6043221155). The tasks
  end when the group stops or when each `Mesh` drops, also a task that waits in a dial
  or in a send. The task of a voter that a change removed, to which `raft` sends no more
  messages, ends only then (#1401) (approved by the architect, 2026-10-07T13:40:50Z:
  https://github.com/synnaxlabs/foundation/pull/1410#issuecomment-6039206881). The group
  holds the handle of the session to each member, and not the task, so that `set_home`
  (PR 4c-2 of #471) can open its stream on it: the task is the only one that dials (the
  plan of PR 4d,
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037266854, approved
  by the architect, 2026-10-07T11:52:00Z:
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037318501). A stop
  of the group drops each handle. When `Transport::dial` gives the one open session to a
  peer (#1363), a call can take its session from `dial`, and #1598 decides whether the
  handle goes back to the task. `set_home` makes a member the home of an index, and
  returns when this node applied an entry that sets it. A try starts with two checks on
  this node: the node is a voter (`Error::NoVote`), and the home is a member
  (`Error::NotMember`). Only the two and a stop of the group end the call with an error.
  A voter of one half of a joint configuration is a voter here, as in the leader's check
  of a peer (`Error::PeerNotVoter`). The try proposes on this node. When another node
  leads, the try sends the proposal on a new two-way stream of the session to the leader
  that the group holds, and reads one answer. The call does not dial: two dialers for
  one peer need a rule for which session stays, and a follower sends to its leader in
  each tick, so with no session the leader cannot be reached. A try gets no position
  when no leader is known, when the group has no session to the leader, when the leader
  refuses the proposal, when the stream fails, when the pool has no block for the
  proposal or this node's group gives `Error::Pool`, and when this node's `raft` names
  another leader or term before the answer. The call then waits one tick and starts the
  next try. A try that waits for the answer has no time limit: the leader answers each
  forwarded proposal or ends its stream, and a session that fails one way ends at the
  timeout of `transport`. `Group` wakes each call at each change of the leader or the
  term, and at a stop, so a stop ends the wait at once. A limit of one election timeout
  on the answer lost: on a link with a round trip above it, `raft` keeps its leader,
  and each try gave up after the leader took its proposal, so the call appended one
  entry for each try and never returned (decided by `laptop.architect`,
  2026-10-07T22:40:51Z:
  https://github.com/synnaxlabs/foundation/pull/1607#issuecomment-6048332653).
  Supersedes the limit of one election timeout in the plan that this approval took:
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037364407. It also
  supersedes point 3 of
  https://github.com/synnaxlabs/foundation/pull/1607#issuecomment-6046249552, a stop
  that ends a forward at most 11 ticks late. With a position,
  the call waits with no time limit until this node applied the entry of that term at
  that index, or until the log has a different entry there, and then it proposes again:
  a new leader commits an entry of its term, which decides each older position. A time
  limit lost: a slow group gets the change again at each timeout. A `request` number
  with a map of open requests lost: the stream is the request (MESH WIRE). `Applied`
  keeps the term of each applied entry only above the lowest floor of an open try, and
  the term of the last entry, so it holds one pair while no call waits (MEMORY BOUNDS).
  A dropped call leaves no waker and no floor, and its stream stops. A node that is not
  a voter gets `NoVote` and does not wait, because the leader gives it only code 16,
  which a full pool also gives (approved by the architect with two changes, `NoVote` and
  the bound of `Applied`, 2026-10-07T11:54:56Z:
  https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037364407). The
  `NoVote` check reads the configuration of this node's log, which changes when the node
  appends a change of voters, before the commit. A promoted node gets `NoVote` until it
  appends that change, and a new leader that replaces the entry changes the result back.
  `NotMember` reads what this node applied (ruled by the architect,
  2026-10-07T20:58:20Z:
  https://github.com/synnaxlabs/foundation/pull/1607#issuecomment-6046723849).
  Supersedes the sentence that a promoted node gets `NoVote` until it applies that
  change: https://github.com/synnaxlabs/foundation/issues/471#issuecomment-6037364407.
  `Ok`
  means that the entry at the position of the try applied. That is an entry that sets
  the home only while `State::apply` never refuses a home change. A change kind that
  lets `apply` refuse a home change (such as a removal of a member) must also make
  `set_home` tell a refused entry from one that set the home. The surface as built, the
  doc of `set_home`, and three points that the plan did not state (a joint
  configuration, `Error::Pool`, and `NoVote` before `NotMember`) are approved by the
  architect, 2026-10-07T20:29:07Z:
  https://github.com/synnaxlabs/foundation/pull/1607#issuecomment-6046249552. The
  ruling of 2026-10-07T20:58:20Z above adds the sentence on a late proposal to that
  doc. Its sentences on what `NoVote` and `NotMember` read supersede the last sentence
  of the doc text in that comment ("The first two read what this node applied").
  Proposed by box1.builder-3, decided by the architect (#471),
  2026-10-07T04:11:26Z:
  https://github.com/synnaxlabs/foundation/pull/1057#issuecomment-6030753391.
  Amended (2026-10-08, the `mesh` PR before PR 3b of #585): each task that sends ends
  at once after the group stops or the last `Mesh` drops. A dial or a send in
  progress stops, so it never holds `Mesh::ended` for a dial timeout. Decided by
  `laptop.architect`, 2026-10-08T04:00:49Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051912643.
  Amended (2026-10-08, PR 3b of #585): the mesh's directory is `mesh` in the data
  directory. Decided by `laptop.architect`, 2026-10-08T03:37:20Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051658475. `node`
  gives it as `mesh::Config::dir`. Decided by `laptop.architect-2`,
  2026-10-08T03:54:37Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051833866, and
  approved by `laptop.architect`, 2026-10-08T04:00:49Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051912643.
- **MESH SURFACE (#1051)** A crate outside `mesh` reads a region through `Mesh::watch`,
  `Watch::next`, and `Mesh::member` (#562). `Mesh::key` gives this node, the `key` of
  the `Config`, so a crate that holds a `Mesh` keeps no second copy of the key that can
  differ (#1664). Approved by `laptop.architect`, 2026-10-07T23:31:29Z:
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
  of `mesh`. The `Debug` text of a `Mesh` is `Mesh { .. }`, of an `Ended` is
  `Ended { .. }`, and of a `Watch` is its index only. The text of `Ended`: decided by
  `laptop.architect` (2026-10-08T04:42:48Z):
  https://github.com/synnaxlabs/foundation/pull/1791#issuecomment-6052417077. It
  supersedes the `#[derive(Debug)]` of `Ended` in
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051912643. A crate
  outside `mesh` opens a region with `Config` and `Mesh::open`, and gives it each stream
  of a peer with `Mesh::serve`. The three are public since the senders (#1410). `Error`,
  `claim::Error`, and `region::Unfit` are public with them, because `open` and `serve`
  give them. `claim::Error` is the `grant::Error` of the rulings: #1460 gave the module
  its new name. `Error` adds `raft::Error` and `transport::Error` to the types of other
  crates. `Config` and `serve` add types that the caller builds:
  `env::files::Files`, `env::clock::Clock`, `env::entropy::Entropy`,
  `env::tasks::Tasks`, `block::Pool`, `transport::Transport`,
  `transport::stream::Incoming`, `types::name::Prefix`, and
  `types::ed25519::PrivateKey`. So a crate that opens a region has `env`, `block`,
  and `transport` in its line of the crate map. `Config::founding` adds
  `spec::definition::Definition` and `types::name::Name`, and `Mesh::pointer` gives a
  `Pointer`, whose root is a `types::digest::Digest`. So a crate that opens a region
  also has `spec` in its line. Decided by `laptop.architect`: the founding definitions,
  2026-10-08T06:12:36Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6053614771); the
  pointer, 2026-10-08T08:22:08Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6055806836); this
  text, 2026-10-08T08:41:43Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056116151).
  `Config` has no `clock::Reader`, and `Error` has no `Unsynced` and no `Status`: no
  public call reads the one or gives the two. The join answer of #336 decides, with its
  caller, where a join that no voter stamps goes (MEMBER RECORD).
  Decided by `laptop.architect` (2026-10-07T22:33:29Z):
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
  https://github.com/synnaxlabs/foundation/pull/1607#issuecomment-6046249552.
  The line "Private still" of the plan names `set_home` (4c-2) as the first call that
  changes the region
  (https://github.com/synnaxlabs/foundation/issues/1051#issuecomment-6041243466), which
  the architect approved, 2026-10-07T16:24:54Z:
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
  three types.
  Amended (2026-10-08, the `mesh` PR before PR 3b of #585): `Config::dir` is the
  mesh's directory, relative to the data directory. The mesh makes it and syncs its
  parent, and the log goes in `log` in it. Its parent must be there and durable. `node`
  gives `mesh`. `Mesh::ended` gives `Ended`, a future that resolves once each task of
  the mesh has ended: the group's task and each task that sends. It holds no clone, so
  it does not keep the group running. Once it resolves, the mesh holds no file, and a
  new open of its directory can take the log. Decided by `laptop.architect`,
  2026-10-08T04:00:49Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051912643, after
  the ruling on PR 3b, 2026-10-08T03:37:20Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051658475, and the
  field over a `Files` call by `laptop.architect-2`, 2026-10-08T03:54:37Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051833866. Lost: a
  counting `Tasks` driver in `node`, because `node` then watches the tasks of another
  crate; `Mesh::close(self)`, because the hub holds a clone, so one clone cannot end
  the group; `env::files::Files::within`, because `env` then gives two ways to scope
  the files of a crate, beside `buffer::Config::dir`. A change that wants it later
  moves `buffer` and `mesh` together.
- **SPEC TREE (#6)** `spec::tree` is the prolly tree of one region. A key is a full
  name in byte order, so the descendants of one name are one range. A value is opaque
  bytes. A chunk is a level byte, then entries: a leaf entry is a key and a value, and
  an entry above is the last key of a child and its BLAKE3 hash. A chunk ends after an
  entry when a draw from the BLAKE3 hash of the level and the key is below
  `(end^4 - start^4) / 4096^4`, where `start` and `end` are the entry's byte offsets
  in the chunk (Weibull hazard, shape 4), or when the chunk reaches 16 KiB. A chunk
  above the leaves holds at least two entries, unless it is the last of its level, so
  a key of any size fits. The rule uses integers only. A chunk with one child is
  never a root, so the tree is a function of its entries. The empty tree has the root
  `tree::empty()` and no stored chunk. Chunks come from peers, so a reader checks each
  chunk that it reads: keys in order, each length in its shortest form, and a child at
  the level below with the last key that its parent gives and a first key above each key
  that comes before it in the chunks above (#684). A chunk that fails gives
  `Error::Corrupt(hash)`. A reader does not check the boundaries, and only `diff` checks
  that a leaf key is a name, so "a function of its entries" holds for trees that `apply`
  made. The tree does no I/O: the caller fills a `tree::Chunks`, and `get`, `apply`, and
  `diff` return `Error::Missing(hash)` for a chunk that is not there, so the caller
  fetches it and runs the operation again. Each run names one chunk, because a change
  record lists the chunks that it made and a caller fetches those first. `apply` takes a
  batch of `tree::Change` values, adds the new chunks to the `Chunks`, and returns the
  new root and their hashes. `diff` returns each changed entry with its old and new
  value, and the chunks that only the new tree has. A read of all entries below one name
  is a later function of `spec::tree`; it replaces the `spec::Tree::region` of X12. A
  chunk has no maximum size: one value is in one chunk, and the limit on a value belongs
  to the code that encodes definitions. A chunk's address is a `types::digest::Digest`,
  the same type that `wire` and `blob` carry. To change the chunk format or the boundary
  rule changes every root digest.
- **REGION CHECK (#1841)** `spec::region::check(prefix, definitions)` gives each
  problem of a region's definitions, by tree key, in tree key order: each
  `spec::channel::check` problem, each name that the region does not govern (X2), and
  each definition that is not at the tree key of its kind. `Mesh::apply`, #1741, the
  node state of BQ11b, and `plan` (#1082) call it. A channel's edges point only at
  channels of its own region, so a region checks its spec alone, also while cut off
  (K5), and a change in one region breaks no edge of another. Two channels with one key
  are `channel::Problem::Duplicate`, not a panic, as a committed spec comes from other
  nodes. Lost: an input of the keys of other regions, so that an edge may cross
  regions; a `spec::Region` that cannot hold a problem, as #1741 and BQ11b keep a
  committed spec with problems; and a module `spec::problem`. The reach of a policy
  (X26) is not in it yet: #1846 adds it, and `config` (#679) gives its diagnostic from
  that problem (`laptop.architect-2`, 2026-10-08T09:11:05Z,
  https://github.com/synnaxlabs/foundation/issues/1841#issuecomment-6056600033).
  `spec::region::tree(chunks, definitions)` builds the tree of a region's definitions,
  and cannot fail. `mesh` calls it at open and at apply. Lost: the function in
  `spec::tree`, which then points at the model above it; and the encode in the caller.
  `plan` (#1082) maps a key to its region with the function of `spec::region`, and
  keeps no copy (`laptop.architect-2`, 2026-10-08T09:09:16Z,
  https://github.com/synnaxlabs/foundation/pull/1844#issuecomment-6056571263). The
  check of the key form accepts a reserved label only for a subject and an access
  policy, the kinds of the founding definitions (FIRST ADMIN). Each other kind at a
  reserved label is `Misplaced`, so a region there makes no child region. A file
  still cannot hold a reserved label (`Kind::key`).
  Lost: a check that skips each reserved key, as a channel at `@admin.@subject` is
  then no problem and the check needs `spec::key::reserved`. Decided by
  `laptop.architect-2`, 2026-10-08T09:15:07Z
  (https://github.com/synnaxlabs/foundation/pull/1844#issuecomment-6056664804), and
  the kinds 2026-10-08T09:51:31Z
  (https://github.com/synnaxlabs/foundation/pull/1844#issuecomment-6057256316).
  Supersedes: the panic for two channels with one key (architect, #756,
  https://github.com/synnaxlabs/foundation/issues/756#issuecomment-6031836890).
  Decided by `laptop.architect-2`: the check, 2026-10-08T08:41:52Z
  (https://github.com/synnaxlabs/foundation/issues/1841#issuecomment-6056118794); the
  tree, 2026-10-08T08:47:57Z
  (https://github.com/synnaxlabs/foundation/issues/1841#issuecomment-6056217372). On
  the checks in `spec` of `laptop.architect`, change 2, 2026-10-08T08:22:08Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6055806836), and
  the tree of the founding definitions, 2026-10-08T08:41:43Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056116151). The
  supersede and the edge rule, agreed by `laptop.architect`, 2026-10-08T08:43:31Z
  (https://github.com/synnaxlabs/foundation/issues/1841#issuecomment-6056144931).
- **BLOB STORE (#1226)** `blob::Store` keeps chunks by `types::digest::Digest` on the
  node's disk through `env::files`. A put returns only after the chunk is durable. A get
  gives bytes only when they hash to the digest; a chunk that fails the check (a write
  torn by a crash, a bad sector) reads as absent, so the caller fetches it again as for
  any absent chunk, and the store counts each one in a crate-private count: an
  `interface` issue makes it public, with a noun for a name, when the first caller (the
  node's status of its disk) needs it. A get or a put holds at most one chunk in memory.
  `put` borrows its chunk (`&Block`). Layout: one flat directory, one file per chunk
  named by the 64 hex digits of its digest, with the chunk's bytes and nothing else, so
  the bytes are their own check and the layout needs no header, no check field, and no
  rename. A pack file with an index lost: it needs record headers, a scan of every byte
  at open, and compaction for removal. Removal of chunks that no kept root reaches is a
  follow-up. Decided by `laptop.architect` (2026-10-07T17:23:56Z):
  https://github.com/synnaxlabs/foundation/issues/1226#issuecomment-6043124789. A torn
  chunk reads as absent, never as a short chunk, because the bytes are their own check.
  A write to a second name and a rename (`File::rename`, #1503) lost: each chunk then
  has a second name, a crash leaves strays at that name, and the open needs a rule for
  them. Decided by `laptop.architect` (2026-10-07T20:47:57Z), item 1:
  https://github.com/synnaxlabs/foundation/issues/1226#issuecomment-6046560177.
  Supersedes the reason "`env::files` has no rename" of the rules in
  https://github.com/synnaxlabs/foundation/issues/1226 (2026-10-07T06:59:56Z). The open
  lists the directory and trusts no name: a get of a listed digest reads and checks its
  bytes, and a put of one writes it again, because a process crash leaves whole bytes in
  the cache that no sync covers, and a put that trusted a read of them would return
  before they are durable. A put refuses a chunk longer than the largest block of the
  pool before any file call. A put of a digest that a put stored since the open makes no
  file call. A put whose future is dropped stores nothing that a get gives unchecked:
  the next get of the digest reads and checks the file, and the next put writes it
  again. Decided by the builder (#1515,
  https://github.com/synnaxlabs/foundation/pull/1515) and `laptop.architect`
  (2026-10-07T17:47:50Z):
  https://github.com/synnaxlabs/foundation/pull/1515#issuecomment-6043557708. Supersedes
  the read on the first put of a listed digest in the plan
  (https://github.com/synnaxlabs/foundation/issues/1226#issuecomment-6042962010) and the
  sentence of item 1 of the 17:23:56Z ruling, "the next put or get of the digest reads
  the file first". Every open makes the directory and syncs its parent, because an
  earlier open can have stopped between the two. A put removes a file of another length
  at its name and writes the chunk. A create that gives `Full` syncs the directory one
  time and opens again, because `Files::remove` counts the room of a removed file as
  used until `sync_dir` on its directory ends. A second `Full` is the error of the put.
  Decided by `laptop.architect` (2026-10-07T20:47:57Z):
  https://github.com/synnaxlabs/foundation/issues/1226#issuecomment-6046560177.
- **BLOB WIRE (#1227)** The blob protocol (protocol 4 of the header) runs on one stream
  from a requester to the server that holds the store. After the header, the requester
  sends gets and puts, and the server sends replies. Fields are little-endian. A get is
  kind 1 and then one or more digests of 32 bytes; the message length gives the count,
  so a get has no count field and no run state, and a requester with more digests than
  one message holds sends more gets. A put is kind 2, the digest, and the length of the
  chunk (`u32`), 37 bytes; its body follows. A reply is kind 1 (chunk: the digest and
  the length, 37 bytes; its body follows), kind 2 (absent: the digest, 33 bytes), or
  kind 3 (stored: the digest, 33 bytes). A body is the bytes of the chunk as stream
  messages back to back with no prefix, each at most the peer's `message_bytes_max`, so
  a chunk larger than one message goes in parts. No message of a body is empty, the body
  starts a new message, and a chunk of 0 bytes has no body message. Each decoder
  (`wire::blob::Server` for the requester's messages, `wire::blob::Requester` for the
  server's) takes from its caller the most bytes a chunk may have, refuses a longer
  chunk at its length field, and refuses a body message longer than the rest of the
  body; `body` gives where in the chunk the next body message starts. The receiver, not
  `wire`, checks the digest over the whole chunk. Stop codes: 16 `MISMATCH` (the bytes
  do not hash to the digest), 17 `TOO_LARGE` (the chunk is longer than the largest block
  of the node), 18 `FULL` (a put would leave the disk under the free floor of the
  store). Each other break is `wire::header::MALFORMED`, and a stop ends each open
  request of the stream. Order is a rule: the server answers requests in order, the
  digests of a get in message order and a put after its body. The requester keeps a
  queue of its open requests, and a reply that does not answer the oldest open request
  is `MALFORMED`. Each reply names its digest. The check lives in `blob` (#1229), and
  `wire` keeps no queue. The sides are `Requester` and `Server`, and the decoded
  messages `FromRequester` and `FromServer`. Lost: a count field in the get (the length
  gives it); a run state for the get as the hub keys have (a get is one message); a
  length prefix on each body message (the stream frames it); a digest check in `wire`
  (the decoder sees parts, and the receiver has the whole chunk). Decided by
  `laptop.architect` (2026-10-07T20:43:10Z):
  https://github.com/synnaxlabs/foundation/issues/1227#issuecomment-6046483057.
  `wire::blob::Error::code` gives the stop code of each decode error: `TOO_LARGE` for
  `TooLarge`, and `MALFORMED` for each other, so `blob` holds no copy of the map.
  Decided by `laptop.architect` (2026-10-08T00:54:19Z):
  https://github.com/synnaxlabs/foundation/pull/1626#issuecomment-6049910210. A
  requester that refuses a chunk over its own limit stops the stream with `TOO_LARGE`,
  the true cause, and the server ends the open requests as after any stop. Supersedes,
  for a get, the reason "the sender goes to another peer" of `TOO_LARGE` in
  https://github.com/synnaxlabs/foundation/issues/1227#issuecomment-6046483057. A
  `TooLarge` or a `MISMATCH` on a get is the failure of that peer for that digest. A get
  of `blob` names one peer and gives back each digest that the peer did not give, with
  its cause: absent, `TooLarge` with the length and the limit, or `MISMATCH`.
  Supersedes, for a get, the words "the call gives the exact error" of the rules in
  https://github.com/synnaxlabs/foundation/issues/1229: the get gives that digest back
  with its exact error and goes on, the stream stops, and nothing is stored. `blob` asks
  for the other open digests again on a new stream to the same peer. `mesh`, which picks
  the peer, asks the next peer that holds the digest, each peer at most once for one
  fetch. When no peer remains, `mesh` fails the fetch with an exact error that names the
  digest and the last cause. The length in a chunk reply is a claim until the body
  hashes, and a member node can lie (`docs/security.md`), so one peer cannot deny a
  chunk. Lost: give up at the first `TooLarge` (one member decides it); a get of `blob`
  over a list of peers (the choice of peer needs the membership, which `mesh` has); a
  get that fails as a whole at the first refused chunk (`mesh` cannot tell which digests
  the peer still owes). Decided by `laptop.architect` (2026-10-08T06:24:02Z and
  2026-10-08T06:29:55Z):
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6053784863 and
  https://github.com/synnaxlabs/foundation/issues/1229#issuecomment-6053878785.
- **K5 + REGION LOCKED + K5 REVISION** There is one mesh. A region keeps changing its
  own definitions while cut off. A region changes its own voters. The parent only
  creates or removes a region, or forces a takeover (admin on the parent, `--force`,
  epoch bump; nodes reject commits from an old epoch). The parent cannot veto. Access
  across regions is ordinary access policy. A change that spans regions commits per
  region in dependency order. Supersedes: D7 linked meshes, K5 parent-owned voters.
- **REGION BLOCK (tunable syntax)** `region "site_a" { voters = [...] }` declares a
  region by name prefix. Regions nest like names. Supersedes: K5 voters policy. The
  prefix is a `types::name::Prefix`, which can be empty: the root prefix
  (`Prefix::ROOT`, text `""`) contains each name, so the root region holds each node.
  `mesh` holds it in `mesh::Config.region` and `region::State`, and checks each name
  against the region with `Prefix::contains`; `ticket::Options.prefix` stays a `Name`.
  Decided by `laptop.architect` (2026-10-07T12:47:19Z):
  https://github.com/synnaxlabs/foundation/issues/1383#issuecomment-6038223777. Each
  field that holds a region's prefix is a `Prefix`: also `ticket::Ticket`'s region (the
  region that the joining node opens with) and the `region` of `Unfit::Outside` and
  `Refused::Outside`, so a ticket for the root region exists. Decided by
  `laptop.architect` (2026-10-07T13:32:35Z):
  https://github.com/synnaxlabs/foundation/issues/1383#issuecomment-6039051758.
- **r4 reconciliation (SETTLED BY ME)** Definition references (index, quality, error,
  control) stay inside one region. Placement is not a key reference (rules in 1.9).
- **VOCABULARY + REGION LOCKED** "Region" names the governed part of the tree. Docs say
  "cloud region" for providers. The old term for the governed part is retired, and the
  word "branch" is reserved for a possible future copy-on-write mesh branching feature.
- **BQ1** Layer-2 order. `hub` pulls incoming connections through `Transport::accept`.
  `blob` is one content-addressed store with peer fetch for spec chunks and upgrade
  binaries.
- **BQ2** `types` holds values. `spec` holds definitions, the tree, hashes, diffs, and
  the one resolver `spec::resolve`. A connector's config is an opaque document that only
  its kind decodes.
- **BQ11a** Joining is an operation. An admin creates a join ticket (single-use or
  reusable, with an expiry, scoped to a region and name prefix). The node joins with it,
  voters record membership, and the join is logged on the changes channel. Files name
  nodes only where they matter (voters, placement). Ephemeral nodes are removed after a
  set time offline. Tickets are secrets.
- **MEMBER RECORD (#242)** The region's record of a node is a `mesh::Member`: a
  `card::Signed` (name, Ed25519 public key, seal key, addresses, and version, which the
  node signs over `foundation/card/1`, its `node::Key` (16 bytes), and the card's one
  byte form), the join ticket's signature over the first card, `ephemeral` (for an
  ephemeral node, the time offline after which the region removes it), and the key of
  each status channel (X27) by its name relative to the node's name (`clock.offset`,
  never the full name); `card.name` is the one copy of the node's name.
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
  that admits a join answers with the founding voters and their cards, and the node
  opens with them as `Start.voters` (RAFT VOTERS). Until snapshots (#253), a region
  whose founders all left cannot admit a node. `secret` finds no key itself: `ops` and
  `node` read the member and pass its seal key. A rotation, a new card, and `Remove`
  wait for a caller; a rotation that only the node signs lets a stolen key lock the node
  out. Lost: a record that only the admitting voter checks (a voter that lies admits any
  key, against BQ12). A `card::Signed` holds the `node::Key` that its signature covers
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
  already a member, a name that a member holds, a status key that a member holds or
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
- **S9 (changes log)** A built-in changes channel carries the small change records; seq
  is the Raft log index; any copy can serve it; readers resume from any source. There
  is one per region (X29).

### 1.9 Replication and failover

- **A1 (current part)** One home per index orders samples, keeps the buffer, and runs
  the gate. Reads may come from a copy. Supersedes: A1 channel home field, A1 standby in
  the mesh file.
- **S12 (placement part) + B7** Placement is a policy: `placement { select, home,
  standby, copies }`. Each node field is optional, but a placement names at least one
  node, and no node has two roles. When no placement selects the index, or the winning
  placement names no home, an index's home is the node of the connector that writes it
  (precedence in X22). Amended: the `home` field restores the recorded intent
  ("placement decides home", r8 Q8), which the bootstrap list left out (architect,
  #1150, https://github.com/synnaxlabs/foundation/issues/1150#issuecomment-6032212749).
  `config` refuses a node with two roles with `config.role-overlap` at the last value in
  source order that names it. A `copies` list is one value (architect, #1150,
  https://github.com/synnaxlabs/foundation/issues/1150#issuecomment-6037095151). A
  placement that names no node is `config.empty-placement`, at the `copies` value when
  the block has one, else at the block. `copies = []` next to a home or a standby is
  valid (architect, #1150,
  https://github.com/synnaxlabs/foundation/issues/1150#issuecomment-6037713864).
- **BQ6** Asynchronous replication. The `replica` component ships each index's log
  (stored bytes, reader positions, control handoffs, dedup marks) without touching the
  write path. Takeover is the home's crash recovery plus one fence check, inside `home`.
  Supersedes: r8 Q6 standby as a reader.
- **BQ7** Live writes never wait. The home publishes stored and replicated marks per
  index. Each writer picks its confirmation and resends unconfirmed frames after
  failover, deduplicated. Voters promote the standby when the home's lease lapses,
  however far behind it is. The old home's tail returns as deduplicated backfill.
  Failback is manual. By default the home sends to the standby after its own sync (an
  SSD is advised for critical homes; a Pi SD card loses about 1.6 s per crash).
- **BQ8 (FAILOVER DELEGATED)** Each node holds one lease, from its own region's
  voters. An index's holder record lives in that region. The standby must be in the
  home node's region. The name's region governs the definition. Supersedes: S9 lease
  wording, r8 Q8 lease per group.
- **BQ10 (FAILOVER DELEGATED)** One placement covers a connector and every index under
  its name. The standby connector starts cold behind the same fence as the home's
  writes. Failback is a planned move.
- **READ COPIES** Placement `copies = [node]` keeps a never-promoted copy of an index,
  fed by `replica`, in any region. Remote readers read and hold at the copy, so a weak
  link carries each sample once.
- **FAILOVER DELEGATED** The remaining r13 choices are parameters tuned by simulation.

### 1.10 Connectors and calculations

- **D4** The connector list is a parameter. Adding one is cheap: one small contract, a
  conformance kit and a protocol simulator per connector, and one compiled-in kind table
  (C-backed kinds behind build flags). First acceptance scenario: a remote test site
  (NI, LabJack, PLC over Starlink or cellular) streams to cloud stores and laptops with
  no loss, synced clocks, and commands back.
- **C3 + C5 SHAPE** One concept, the connector. Integrations merged into it, and the
  word "integration" is retired. A connector is named in the tree and governed by its
  region. Its changing state is channels under its name. Shared error set: retry,
  config, device.
- **GROUPS DROPPED + CONNECTOR = TASK** A connector is a task: one index for reading
  (one rate, one clock, one writer) or one reader for writing. There are no groups.
  Connectors that name the same endpoint share it through a library component that owns
  the handle once per node. The kind's checker rejects combinations the hardware cannot
  do. Supersedes: C3 REFINEMENT groups.
- **Commandable parameters (same lock)** A kind declares its parameters and which can
  change at runtime. The connector config chooses which are commandable. Each
  commandable parameter is a channel with an ack (A20). Access and authority decide who
  sets it. Files give only its starting value. The library gives every kind `running`.
  Supersedes: r8 Q14 group run channels.
- **BQ5** Each kind owns `async fn run(&self, ctx: Context)`. The supervisor only starts
  and cancels it. `ctx` gives hub sessions, status, run commands, secrets, and cancel.
  `hub` and `home` enforce the rules. `connector` is a library of components plus
  ready-made compositions built only from public parts. Supersedes: r8 Q5 actor with
  device hooks.
- **C5 + KINDS OWN THEIR CONFIG** Each kind owns parse, check, discover, and run, built
  on shared components. `config` never knows a kind's fields. A kind returns diagnostics
  with positions plus the channels it reads and writes. Calculations are a kind. The
  engine (powerful, Arc-style, in Rust: rule-based filtering, waveforms, FFT) is a
  separate later design. Locked guarantees: data only through hub sessions, a plan-time
  check, resource isolation (own threads with a budget, or another node), determinism
  (time only from samples and ctx), and outputs on the calculation's own index.
  Supersedes: r3 single-expression language, r3 first-input index, r8 JSON Schema
  check in `config`.
- **KIND TABLE** `kind::Kind` is typed: an associated `Config` and `impl Future`
  methods. `kind::Table` erases it inside `connector` with a private trait that takes
  the `Document` and parses again, so callers see one concrete type with no `Any` and
  no downcast. An unknown kind is the diagnostic `connector.unknown-kind`, since the
  name comes from a file. A run or a discovery fails with one of three classes:
  `Config` (stop until the spec changes), `Device`, and `Retry` (restart with
  backoff). Decided by the `connector` builder in the plan on #338, after
  `/eb-review`; approved by the coordinator (#338). `Table::check` takes where the file
  names the kind and puts `connector.unknown-kind` there; `discover` and `run` take
  their kind from the spec, which has no spans (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152,
  2026-10-08 03:02 UTC). `Table::check` also puts there each diagnostic of the kind
  with no span, since a `Document` has none to place a missing attribute
  (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/pull/1782#issuecomment-6051900967,
  2026-10-08 03:59 UTC).
- **READER SETTINGS** `connector::reader::read` is the one reader of the S10 settings of
  an out connector: the `select` attribute and one `reader` block with `name`, `mode`
  (`hub::reader::Mode`, as a string or a reference), and `hold`. With no block the
  reader is complete, has the connector's name, and holds nothing. A second `reader`
  block is `document.repeated-block`, and `read` reads only the first, where a label is
  `document.label-count`. A negative `hold` is `document.negative-span` (READER RULES,
  #94; `laptop.architect-2`, 2026-10-08T07:04:36Z,
  https://github.com/synnaxlabs/foundation/issues/1785#issuecomment-6054474145).
  Supersedes `config.repeated-block` and `config.label-count` of
  https://github.com/synnaxlabs/foundation/pull/1782#issuecomment-6051900967
  (2026-10-08T03:59:47Z), and `config.negative-span` for a `hold` of
  https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6037207886.
  A `hold` in `latest` mode is `connector.latest-hold`, since only a complete reader
  holds.
  `read(config, keys, blocks)` takes the kind's own attributes and blocks and gives
  `document.unknown-attribute` or `document.unknown-block` for each other key it does
  not read (DOCUMENT KEYS), so a kind's key list does not change when `read` reads a new
  key. Decided by `laptop.architect-2` on #1153
  (https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152,
  2026-10-08 03:02 UTC, and
  https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051327019,
  2026-10-08 03:05 UTC). A kind that lists `select` in its attributes or `reader` in its
  blocks is a defect in the kind, and `read` panics (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/pull/1782#issuecomment-6052200724, 2026-10-08
  04:25 UTC). Supersedes the `KEYS` part of
  https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152 and
  https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051327019
  (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/pull/1782#issuecomment-6051900967, 2026-10-08
  03:59 UTC).
  `Settings::name` is `None` for a reader with no `name`, and `None` is the
  connector's name. `kind::Context::reader` (#1731) gives that name when it opens the
  reader, and no other place does. Lost: `name: Name`, with the connector's name
  passed through `Kind::parse` of every kind for one value that only the reader needs.
  Proposed by `connector` on #1794
  (https://github.com/synnaxlabs/foundation/pull/1794#issuecomment-6052681089), and
  approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1794#issuecomment-6053214653,
  2026-10-08 05:43 UTC). Supersedes the ad hoc reader and `connector.unnamed-hold` of
  https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152
  (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/issues/1736#issuecomment-6052555898, item
  7, 2026-10-08 04:53 UTC).
- **SUPERVISOR** `supervisor::Supervisor::run` runs one connector and never starts a
  run before the last one returned, and none after a cancel. Each run gets a child of
  the caller's token. After `Device` or `Retry` it restarts with full jitter backoff
  (1 s first, 60 s cap, constants). The waits start again from 1 s after a run that
  lasted at least 60 s. `Ok` from `run` ends the connector.
  `Config` returns to the caller, which starts a new supervisor when the spec
  changes (R12-4). Restart errors reach the connector's status in #420. Decided by the
  `connector` builder in the plan on #338, after `/eb-review`; approved by the
  coordinator (#338), with the reset after a long run approved on #338 later.
- **BQ15** A set of devices that the driver acquires as one unit is one connector.
  Otherwise, separate connectors and indexes, never two writers.
- **R7 starting points** OPC UA: open62541 compiled in, with our own crypto plugin on
  aws-lc or compiled-in mbedTLS. Modbus, MQTT with Sparkplug B (pass the TCK), and Kafka
  (pure Rust on `kafka-protocol`, rdkafka behind a flag): built sans-I/O. DAQmx and LJM:
  runtime-loaded bindings, NI functions declared by hand. Codecs: built. Crypto: rustls
  with aws-lc-rs and blake3. Tooling: clap, schemars, toml_edit, tracing. Our own thin
  MCP server, Prometheus text output, and InfluxDB line protocol. FIPS build later.
  HTTP: one client for all connectors, on `hyper` (HTTP/1.1 and HTTP/2) over the `env`
  network seam with `rustls`, in the connector component library. InfluxDB, a general
  HTTP connector, alarms, webhooks, and remote write use it. No HTTP parser of our own.
  Every clock read and name lookup of the client goes through `env`, and no Tokio
  feature of `hyper` or `hyper-util` is on. TLS takes a configured CA, and no setting
  turns verification off. Lost: a sans-I/O HTTP/1.1 module in `connector-influx`. The
  person decided on 2026-10-06 ("Approved." "Adding a bunch of crates is fine. Making a
  binary larger is fine." "we should be careful about writing raw HTTP transports.",
  relayed by `advisor`; "Yes I approve", to the coordinator) (#341). #983 (an `httparse`
  reader) closed: the person told `connector` to use the `hyper` client on 2026-10-06.
  `httparse` comes in only as a dependency of `hyper`. The client is HTTP/1.1 only for
  now: `h2` 0.4 reads the OS clock to expire a reset stream, so HTTP/2 turns on only
  when `h2` takes its clock through `env`, by an upstream change. Decided by the
  coordinator with `advisor` on 2026-10-06 (#341). The client keeps one idle connection
  for each origin. It does not reuse one that is idle longer than 90 s (the `hyper-util`
  default), read on the `env` clock, and the next send closes it: the client sends no
  keep-alive, and a firewall or NAT may drop the state of an idle stream. Decided by the
  coordinator with `advisor` on 2026-10-06
  (https://github.com/synnaxlabs/foundation/issues/341#issuecomment-6022322924). A
  request that fails on a reused connection before its response goes once more on a new
  connection, when the connection did not write it, or when its method is idempotent and
  no byte of a response came (RFC 9112, as in Go). The pool key is the origin, and it
  keeps the host name, because a TLS connection is verified for one name and must never
  carry a request for another. Decided by the architect on #1111
  (https://github.com/synnaxlabs/foundation/pull/1111#issuecomment-6031412223). The key
  is the host name in lower case and the port. `influx.` and `influx` are two keys,
  because a resolver may expand a name with no final dot. The client takes only `http`
  today; with TLS, the key also holds the scheme. A new connection looks up the host
  through `env` and tries each address in order, as Go does: each address gets an equal
  share of the time left to the deadline, but at least 2 s or all that is left. A
  refused address moves the dial to the next at once, and no connect starts once the
  time is up. A reused connection does no lookup. Lost: Happy Eyeballs (RFC 8305), which
  needs more code and streams; a separate error variant for a failed lookup, which a
  caller handles as a failed connect; no limit for each address, where one that drops
  the SYN uses the whole timeout. Proposed by `connector` in the plan on #341
  (https://github.com/synnaxlabs/foundation/issues/341#issuecomment-6031334051) and in
  the review of #1135
  (https://github.com/synnaxlabs/foundation/pull/1135#issuecomment-6031807435,
  https://github.com/synnaxlabs/foundation/pull/1135#issuecomment-6031903363,
  https://github.com/synnaxlabs/foundation/pull/1135#issuecomment-6031982713). The key
  text after the #1111 link, the dial rule, and the `Error::Connect` doc: approved by
  `laptop.architect-2` on 2026-10-07
  (https://github.com/synnaxlabs/foundation/pull/1135#issuecomment-6032674524).
  A refused URI gives one error for each cause: `Scheme`, `UserInfo`, `Host`, and
  `Port`, checked in that order.
  Text after `]` is part of the host up to a `:`, so `http://[fd00::2]8086/` gives
  `Host` (https://github.com/synnaxlabs/foundation/issues/1179#issuecomment-6032542190).
  A `[` in a host that is not in brackets gives `Host`. A built URI with an empty path
  sends `/`. Lost: one `Uri` variant that holds the URI with its user info removed,
  because the message must name the cause; and a `Uri` whose `Display` drops the user
  info, because `Debug` and the field still hold the password. Decided by
  `laptop.architect-2` on #1159
  (https://github.com/synnaxlabs/foundation/issues/1159#issuecomment-6032370253).
  No error of a refused URI holds text from the URI: a `/` or `?` in a password ends
  the authority early and puts the password in the host or the port.
  `connector::http::uri` reads a config value as a URI that `send` takes, with no I/O.
  Each refusal, and a fragment, which `send` never sends, is `connector.bad-uri` at the
  value. Lost: a `check(&Uri)` and a code for each kind, which each HTTP kind repeats.
  Decided by `laptop.architect-2` on #1794
  (https://github.com/synnaxlabs/foundation/pull/1794#issuecomment-6052684931,
  2026-10-08 05:03 UTC). Supersedes the `Host` and `Port` fields and messages of
  https://github.com/synnaxlabs/foundation/issues/1159#issuecomment-6032370253.
- **INFLUX KIND** `connector_influx::Kind` reads `address` and the reader settings
  (READER SETTINGS). `address` is an `http::Uri`, since a `Name` is a mesh name. `parse`
  reads `address` through `connector::http::uri`, so a plan finds an address that
  `send` refuses before a run. `node` puts the kind in its table only in #1734, when
  `run` works, so until then a file with an influx connector gives
  `connector.unknown-kind` at plan. `check` gives no channels, and `discover` no
  documents. Until #1734, `run` fails with `Error::Config` and `influx.not-yet`.
  Decided by `laptop.architect-2` on #1153
  (https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152,
  2026-10-08 03:02 UTC). The kind also refuses a path other than empty or `/`, and a
  query, at the value: a path (a proxy prefix) can come later as a compatible change,
  and a refusal cannot. Each diagnostic of a kind names its document
  `connector::kind::NOUN` ("the connector"), as `reader::read` does. Decided by
  `laptop.architect-2` on #1794
  (https://github.com/synnaxlabs/foundation/pull/1794#issuecomment-6052684931,
  2026-10-08 05:03 UTC). The code of that refusal is `influx.bad-address`. Proposed by
  `connector` as its address code
  (https://github.com/synnaxlabs/foundation/pull/1794#issuecomment-6052532087,
  2026-10-08 04:51 UTC), and approved by item 4 of
  https://github.com/synnaxlabs/foundation/pull/1794#issuecomment-6052684931
  (2026-10-08 05:03 UTC).
- **INFLUX SEQ AND GAPS (#1151)** The InfluxDB out connector stores no seq. A stamp
  names one sample of an index on each path (X31), and InfluxDB keys a point by
  measurement, tag set, and time, so a resend stores each sample once. Each run of
  explicit gaps before a sample is one line,
  `foundation_gaps,connector=<connector>,index=<index>,path=<path> count=<n>i <stamp>`:
  `<path>` is `live` or `backfill` (amendment below), `<stamp>` is the stamp of the
  first sample after the gaps, and `count` is the number of seqs from the first trimmed
  seq up to that sample. The gap line goes in the request of that sample, and the
  position is acked only after InfluxDB confirms it (B3). The count is signed, because
  InfluxDB 1 OSS refuses `u`. The `connector` tag keeps two connectors that write one
  index to one database from replacing each other's gap lines. Until a later sample
  comes, the connector keeps one gap per index, and from #1270 one per index and path.
  After a restart the buffer reports the gap again (READER RULES), so a lost gap line is
  sent again. The measurement name is fixed. #1734 names the data measurement, and its
  kind check refuses `foundation_gaps` as one (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152,
  2026-10-08 03:02 UTC). Supersedes the clause of
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032215953 that the
  kind check (#1153) refuses a config that maps a data measurement to `foundation_gaps`.
  Fold rule (6032756428, which replaces the fold rule of 6032215953): `Lab::stored`
  reads each gap line as the seqs `[seq(stamp) - count, seq(stamp))`, where
  `seq(stamp)` is the seq of the data point at its stamp, in the data measurement of
  the same index (6039993275: stamps slew, so a stamp is not a key into the write
  record). Seqs rise with time: a point whose seq is not above the seq before it is a
  lab failure, and the message names both stamps and both seqs. Its gaps are the
  union of these ranges minus the stored seqs, as maximal runs. Each run is one gap:
  `after` is the count of stored samples before the run, and the count is the run's
  length. A gap line with no data point at its stamp, or whose range starts below the
  first written seq, is a lab failure (panic), not data. The property test
  also asserts no silent loss: each seq from the first written seq to the last stored
  seq is stored or in a gap range. After a lost confirmation, a resend, and a later
  trim, gap lines can overlap, so the sum of `count` in `foundation_gaps` is an upper
  bound on the loss from trims. Before #1270 (amendment below) that is the whole loss,
  and the exact loss is the union of the ranges minus the stored samples.
  Lost: a seq field (about 20 bytes a line), a seq tag (one series per sample), a gap
  point in the data measurement (a field type conflict), a configurable gap
  measurement, and a `first=<seq>i` field on each gap line. That field puts a seq,
  which is internal to the node, into each user's InfluxDB; the lab does not need
  it; a reader still cannot get the exact loss, as the stored samples hold no seq;
  and it makes each gap line longer when the store is under pressure. Decided by the
  architect (`laptop.architect-2`), #1151
  (https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032215953,
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032474515,
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032756428,
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032802085), and
  in the review of #1225
  (https://github.com/synnaxlabs/foundation/pull/1225#issuecomment-6032761284,
  https://github.com/synnaxlabs/foundation/pull/1225#issuecomment-6032817985).
  Amended on #1151: with #1270 (M2), the connector is a recording reader and writes the
  samples of both paths (A6, A8). Until then it reads the live path only, and the
  store-and-forward tests use the live path only. The `path` tag of each gap line is
  `live` until then, so the format does not change at M2. The `path` tag keeps a live
  gap line and a backfill gap line at one stamp as two points, so the sum of `count`
  stays an upper bound on the loss from trims. Data lines get no `path` tag, so a live
  sample and a backfill sample of one index at one stamp are one point, and the later
  write sets its fields. The earlier sample is a loss that no gap line counts. Until
  #1270, the fold reads the live path only, and a gap line of another path is a lab
  failure. #1270 amends the fold for two paths. From #1270, a separate test reads the
  points of the simulated InfluxDB and pins the overwrite. Lost: a `path` tag on data
  lines, which makes two series for each channel and puts the path, which is internal to
  the node, in each user's data schema. `foundation_gaps` is Foundation's own
  measurement, so its `path` tag costs the user's data nothing. Decided by
  `laptop.architect-2` on #1151 (2026-10-07T08:07:33Z:
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6033743748; amended
  2026-10-07T08:18:10Z:
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6033910167;
  corrected 2026-10-07T16:52:35Z:
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6042660446).
  Supersedes the gap line with no `path` tag of
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032474515 and of
  point 3 of 6033743748, and the loss bound of
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032756428.
  Data points in the lab: sample `k` of one `Lab::write`, from 0, has the value
  `k as f64`, which is exact below 2^53. `Lab::stored` takes a data point's seq from
  its value: `written.start + k`. It accepts a data point only when its fields are
  one float that is a whole number `k` in `+0..count`, where `count` is the number
  of written samples. Until #341 names the field key of a data line, the field may
  have any key; the #341 PR that names the key changes the check to that key.
  Decided by the architect (`laptop.architect-2`), #1151, on 2026-10-07T14:07:11Z
  (https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6039706938)
  and 2026-10-07T14:22:17Z
  (https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6039993275).
  Supersedes the Q2 check of
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6039706938, which
  compared each value with `seq - written.start`.
- **LINE TEXT (#1098)** `connector_influx::line::Measurement::new` accepts only text
  that InfluxDB 1, 2, and 3 each store as written, in each part (measurement name, tag
  key, tag value, field key). It refuses the rest at construction, so the error
  reaches the config diagnostic in place of a partial write that InfluxDB answers with
  204 or drops. One set of refused characters holds for every part: a backslash, a
  newline, a carriage return, a tab, NUL, U+FFFD, and each character outside the
  general categories L, M, N, P, and S other than U+0020. The last two are what
  InfluxDB 1 and 2 with `validate-keys` drop (`unicode.IsPrint` false, or
  `unicode.ReplacementChar`). The categories come from `unicode-properties` at Unicode
  17.0.0, which a test pins. InfluxDB reads them from the Unicode tables of the Go
  release that built each server (Unicode 15.0.0 for Go 1.26 today), so a code point
  assigned after that version passes here and that server drops it. No client closes
  this gap exactly. NUL in a tag value is refused, though InfluxDB 3 keeps it. No user
  needs it, and a user learns one rule, not four. Foundation names hold only ASCII
  letters, digits, `_`, `-`, `.`, and `@`, so the rule applies only to text that a user
  writes in the connector's config. Lost: a rule for each part. It keeps NUL in tags
  for no caller, and the set a user may write then depends on the part.
  Decided by the architect (`laptop.architect-2`) on 2026-10-07T06:26:41Z
  (https://github.com/synnaxlabs/foundation/issues/1098#issuecomment-6032314177); the
  class by the architect on 2026-10-07T20:31:16Z
  (https://github.com/synnaxlabs/foundation/issues/1098#issuecomment-6046284871), and
  the crate by the person on 2026-10-08T01:48:32Z
  (https://github.com/synnaxlabs/foundation/issues/1098#issuecomment-6050501586).
- **REDUCTION** Deadband is a policy, `reduction { select, deadband }`, unit-checked,
  most specific wins. Connectors read it through a library component and pass it to
  devices that support it. Frames carry only channels that moved. Swinging door is a
  calculation with its own index. Raw and reduced data live side by side, each index
  under its own retention policy.
- **SIM INFLUX (#1151)** `connector_influx::sim::Store` is a simulated InfluxDB, behind
  the cargo feature `sim`, off by default. It parses with `influxdb-line-protocol`,
  InfluxData's own parser, so it is independent of our writer. A point is named by its
  measurement, tag set, and time; a later write of the same point replaces the fields
  that it sets. It refuses a line that a writer must never write: a line that does not
  parse; no time, or a time outside `i64::MIN + 2 ..= i64::MAX - 1`; the key `time`, or
  a name or key that starts with `_`; a key more than once in tags and fields together;
  a float that parses to infinity; and a tag or field whose type differs from the type
  stored for that key in the measurement, where a tag is a type, as InfluxDB 3 gives
  each column one type, also across shards, where InfluxDB 1 checks each shard only.
  Each refusal is a typed `sim::Error` variant. `write` stores each valid line, also
  after a line that is not valid, and returns the first error; a body that is not UTF-8
  gives `Error::Utf8`, and nothing is stored. Where InfluxDB versions differ in a rule
  that it keeps, the store keeps the strictest one. It keeps no size limit (the series
  key length, 65535 bytes in InfluxDB 1 and 2, and the columns per table in InfluxDB 3),
  because each depends on the server's version or config, and the writer owns them
  (#1265). The InfluxDB 3 parser also refuses some lines that InfluxDB 1 stores, such as
  a tab in a measurement name; the writer refuses them too. It splits lines, and skips
  blank lines and comments, as InfluxDB 3 does, so a writer that writes a measurement
  name with a leading `#` loses that line with no error; a test that reads the points
  sees the loss. It stores a `u` integer, which InfluxDB 1 OSS refuses, until the writer
  stops writing `u` (#1210). Lost: a store that gives a time to a line with none, and
  one that takes a type conflict, as each hides a writer bug; and a test that a line is
  refused if and only if the writer refuses its input, as the writer also refuses some
  names that InfluxDB stores, such as a backslash or NUL, so the two sets differ by
  design. Decided by the architect (`laptop.architect-2`), #1151
  (https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032723969), and in
  the review of #1239
  (https://github.com/synnaxlabs/foundation/pull/1239#issuecomment-6032923332,
  https://github.com/synnaxlabs/foundation/pull/1239#issuecomment-6032970676,
  https://github.com/synnaxlabs/foundation/pull/1239#issuecomment-6033050251,
  https://github.com/synnaxlabs/foundation/pull/1239#issuecomment-6033093344,
  https://github.com/synnaxlabs/foundation/pull/1239#issuecomment-6033140752,
  https://github.com/synnaxlabs/foundation/pull/1239#issuecomment-6033409699,
  https://github.com/synnaxlabs/foundation/pull/1239#issuecomment-6033688614).
  Memory: each series keeps its points in chunks, one time column and one typed
  column for each field key, so the STORE AND FORWARD scenario holds about 6e7 points
  on a CI runner (#1149). `Point::fields` is a `Fields` view of the chunk.
  `tests/memory.rs` counts the heap bytes with `counting` and asserts at most 32 a
  point after 1e6 points of the lab's line. Lost: runs of points on a fixed time step,
  as mesh slew moves each time off any grid (MESH SLEW); and the resident set size
  (RSS) in place of a byte count, as RSS depends on the allocator and the OS. Decided
  by the architect (`laptop.architect-2`) on 2026-10-07T14:23:09Z, #1419
  (https://github.com/synnaxlabs/foundation/issues/1419#issuecomment-6040009661).
  Implementation, not a ruling: a chunk holds at most 4096 points. A column holds only
  the points that set its key, each as an index and a value. A point past the end of a
  full chunk goes into the next chunk when it has room, so appends in either time order
  fill each chunk. A full column grows by an eighth, not by double, and a split frees
  the spare room of both halves. A point with one float field takes about 19 heap
  bytes in a long series. Each series also has a fixed cost of about 1.6 KB, so 1000
  series of 200 points take about 27 bytes a point. Each chunk keeps a column for each
  key it holds, in a `Vec` sorted by key, so many sparse keys cost more: 255 keys, each
  set by every 255th point, take about 24 bytes a point, and about 29 when writes split
  each chunk into two halves near half full, as each half keeps a copy of each column.
  `tests/memory.rs` bounds 22 a point for one field, for 63 sparse keys, for appends
  newest first, for writes that split chunks, also with 63 and 65 sparse keys, and for
  one point of 255 fields among points of one field, and 32 for 255 sparse keys, for
  257 and 255 sparse keys with writes that split chunks, and for 1000 series of 200
  points.
  `connector_influx::sim::serve(listener, tasks, store, database)` is its HTTP front, on
  `connector::http::sim::serve` (HTTP SIM SERVER). `POST /write?db=` (InfluxDB 1) and
  `POST /api/v2/write?bucket=` (InfluxDB 2 and 3) give 204 when the store takes each
  line, and 400 with the text of the store's error when it refuses one. A missing or
  empty `db` or `bucket`, or on `/api/v2/write` a missing or empty `org` and `orgID`,
  gives 400; a `db` or `bucket` other than `database` gives 404, as InfluxDB gives for
  one that does not exist. `precision` is `ns` only, and a missing or empty one is
  `ns`; another gives 400, where InfluxDB scales it, because our writer writes
  nanoseconds only and a wrong precision must fail loud. Another path gives 404, and
  another method on a write path 405. A `content-encoding` that names a coding other
  than `identity` gets 415 and stores nothing, as the store decodes no body. The HTTP
  front checks the path, then the method, then the `content-encoding`, then the query,
  and gives the answer of the first check that fails. It checks no token. `database`
  stays out of `Store`: the 404 is an answer of the HTTP front. The front compares names
  as the query writes them, with no percent-decoding, until the first PR of
  `connector-influx` that writes a name into a query (#1530). Decided by architect-2
  (2026-10-07T16:33:22Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042291321,
  2026-10-07T16:41:29Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042446508,
  2026-10-07T17:22:22Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6043097777,
  2026-10-07T18:32:43Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6044310015).
  Supersedes: https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6043097777
  (the 415 sentence of item 5, by 6044310015).
- **HTTP SIM SERVER (#1151)** `connector::http::sim::serve(listener, tasks, answer)`,
  behind the `connector` cargo feature `sim`, off by default, is the one HTTP/1.1
  server of the protocol simulators of HTTP connectors. It runs `hyper`'s server on
  each stream, on its own task, with keep-alive, and gives `answer` each request with
  its whole body. A client that closes its write side after a whole request still gets
  the answer. A request that breaks HTTP gets 400, or 414 when its URI is too long and
  431 when its head is too long, and ends its stream; an HTTP/2 preface ends it
  with no answer. Each simulator answers a `content-encoding` in its own route. It
  returns the listener's error, so a test server that cannot accept fails loud. When the
  caller drops the future, the server accepts no more streams, and each stream that it
  accepted continues to run. `hyper`'s server reads OS wall time on each poll (hyper
  1.12.0, `common/date.rs`) only for the `date` header, which is off. A timer reads its
  own `Instant` to arm the header read timeout, which is off too
  (`header_read_timeout(None)`). The person approved the server on the condition that it
  gets no timer. Lost: our own server on `httparse`, which the person refused; and a
  copy of the server in each kind crate.
  Decided by architect-2 (2026-10-07T16:33:22Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042291321,
  2026-10-07T16:41:29Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042446508,
  2026-10-07T17:08:29Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042839816,
  2026-10-07T17:14:34Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042958763,
  2026-10-07T17:22:22Z
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6043097777) and the
  person (2026-10-07T17:04:29Z
  https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6042756353).
  Supersedes: https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042446508
  (items 1 and 4, by the person's ruling and 6042839816; item 9, by 6043097777 item 5),
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042839816 (its 400
  and 415 sentences, by 6043097777 items 4 and 5; its `# Errors` section, by
  6042958763),
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042291321 (the doc
  of finding 1, by 6042446508 item 1 and 6042839816).
- **QUARANTINE** An out connector that gets a permanent rejection moves the frame to its
  quarantine (a hold on the original data plus an error record) and moves on.
  Operations list, retry, and drop it. Its size is a status channel. It is a library
  component.
- **DEATH RECORDS** When a writer session ends without closing, the home writes a
  "source lost" quality sample. A clean close writes nothing. Scope: X19.
- **R12 catalog (proposal, partly adopted)** Components (cancel, pace (see PACE),
  clock stamping, retry, endpoint, link, drive, thread, queue, cycle, status, run, out,
  calc align) and compositions (polled, clocked, pushed, cyclic, out, calc). The
  kind's `&self` holds process-lifetime parts that `node` injects; `ctx` holds one
  run's capabilities. Group-based parts need revision (X5).
- **ENDPOINT REGISTRY** `endpoint::Registry<K, S, T>` keeps at most one open
  endpoint per key on a node. `acquire(key, settings, open)` shares the open endpoint,
  or calls `open` when none is open. Opens and closes of one key run one at a time;
  other keys do not wait. Unequal settings on an open key give `Error::Config`
  (`connector.endpoint-settings`). The endpoint closes when the last `Lease` drops.
  `node` makes one registry per kind that needs it. A FIFO lock (`endpoint::Shared`)
  composes as `T` later. A `Lease` is not `Clone`, and the close runs after the
  registry's lock is released. Decided by the `connector` builder in the plan on #422,
  after `/eb-review`; approved by the coordinator (#422).
- **PACE (2026-10-05)** `pace::Timer` ticks on a grid of deadlines at `start + n /
  rate`, from a `types::time::Rate`, and skips and counts the ticks a stall missed.
  It has one async `tick(&cancel::Token)`, with no blocking wait and no sleep, hybrid,
  or spin mode: precision belongs to the clock driver in `os` (#379). Decided by the
  `connector` builder in the plan on #237, after `/eb-review`; approved by the
  coordinator (#237). Supersedes: r12 A.3 `pace` modes and blocking wait.

### 1.11 Config as code

- **K1** The boundary is a syntax-neutral Document (blocks, attributes, values, and a
  source position on every value). Each syntax is a front end that reads and writes it:
  `config-hcl` first, HCL the default; YAML read-only until a format-preserving Rust
  editor exists. `node` builds the front-end table keyed by file extension. Files hold
  data only (no loops, variables, or modules). SDK code may produce a Document directly.
  Never shrink the model to the weakest syntax. Supersedes: r3 plain HCL.
- **DOCUMENT MODEL (2026-10-04)** A Document is attributes in a map sorted by key
  (keys unique) plus blocks in order. Values: bool, integer (`i128`), finite float,
  string, reference (`types::name::Name`), list, map, and call. No null and no
  expressions. Refines K1: a front end gives every key, keyword, label, function
  name, and value a span (byte offset, then line and column in Unicode scalar values,
  from 0); SDK and spec documents have none (section 2.1, kind config). `==` never
  reads spans, so a Document from a file equals the same Document from the spec.
  Decided by the `config` builder; approved by the coordinator and `consensus` (#42).
- **DOCUMENT ENCODING (2026-10-04)** `document::encoding` gives each Document exactly
  one byte string, with no spans: a version byte, then tagged values, blocks in the
  producer's order, keys in byte order, and fixed-width little-endian integers (`u64`
  counts and lengths, `i128` integers, and `f64` floats as their bits). `decode`
  refuses every byte string that `Checked::encode` cannot write. Only a `Checked`
  Document encodes: `Checked::new` refuses nesting past 64 levels with `TooDeep`, so
  `Checked::encode` cannot fail, and `decode` gives a `Checked` or an `Error`. Front
  ends refuse files that nest deeper. `spec` holds a connector config as a `Checked`,
  and `config-hcl` `write` and `update` take one. Lost: a depth on each tree type,
  which makes each producer of a tree pay for a rule that only the writers (the
  encoding and `config-hcl`) need. `spec` stores and hashes these bytes. Pinned bytes
  are an oracle in `oracles/conformance/document/`. A new format takes a new version
  byte. Decided by the `config` builder; approved by the coordinator (#62). `Checked`
  decided by the architect (#828,
  https://github.com/synnaxlabs/foundation/issues/828#issuecomment-6030763787, and
  for `write` and `update`,
  https://github.com/synnaxlabs/foundation/issues/828#issuecomment-6030891911).
- **HCL READER (2026-10-04)** `config-hcl` reads HCL with its own lexer and
  recursive-descent parser for the data-only subset (K1, DOCUMENT MODEL), not with
  `hcl-edit`. Evidence on #85: a 2 KB file of 500 nested lists overflowed the stack and
  ended the process, `hcl-primitives` read `-18446744073709551615` as 1, and its errors
  had no fix-it hints. The reader refuses nesting past the Document limit, reads a
  number written with digits only as an exact integer and any other number as a
  float, refuses a float that an `f64` cannot hold (past the largest, or
  rounded to zero from digits that are not all zero), refuses an object key that is a
  number with a fraction, an exponent, or more than 154 digits (HCL can change such a
  key when it makes a string of it), and gives each unsupported HCL form an error with
  a fix-it hint. r3
  section 2 names this fallback. The person chose "Own reader". Supersedes: `hcl-edit`
  in `docs/dependencies.md`.
- **HCL IDENTIFIERS (2026-10-05)** The reader accepts identifiers outside ASCII as HCL
  does (Unicode `XID_Start` and `XID_Continue`, through `unicode-ident`), so
  `température = 1` reads. A new error for each such identifier lost: a valid HCL file
  would fail. The person decided on 2026-10-05 ("go with yes"), with low priority, #263.
  A reference outside ASCII is still an `Error::Name`, because names are ASCII (A3).
  Measured against HCL v2.25.0, two differences remain. HCL reads the 23 compatibility
  characters in `ID_Start` but not in `XID_Start` (U+037A, U+0E33, and others). The
  reader refuses them at the start of an identifier, and 19 of them after it. The
  reader follows the Unicode version of `unicode-ident` in `Cargo.lock`, which can be
  newer than HCL's, so it accepts characters that HCL does not know yet. Lost: a
  hand-kept list of the 23; own tables generated from HCL's Unicode version; and
  `unicode-id-start`, a second table crate that follows the changes JavaScript makes to
  `ID_Start` and `ID_Continue`.
- **HCL REFERENCES (2026-10-05)** The reader reads a reference part by part, as HCL
  reads a traversal: identifiers joined by `.`, with spaces around each `.` and new
  lines inside `[` and `(`. A first part `true`, `false`, or `null` is a value, so
  `true.x` is an index. After a `.`, a number is an index (`site_a.1` is `Form::Index`),
  `*` is a splat, and any other token is a syntax error. An index that is a string with
  no template, quoted or heredoc, is one more segment: `plc["40001"]` is `plc.40001`,
  `a["b"]` is `a.b`, and `plc["a.b"]` is `plc.a.b`. Any other index is `Form::Index`.
  `write` gives each later segment that is not an identifier as a string index
  (`plc["40001"]`, `site_a["@changes"]`). A first segment that does not start with a
  letter or `_`, or that is `true`, `false`, or `null`, has no reference form, and
  `write` refuses it with `Unwritable::Reference`. A file writes such a name as a string
  where a kind takes a name: a kind reads a string or a reference as the same `Name`,
  through `document::read::name` (#474). `read::names` reads one name or a list, in
  order with repeats, and `read::label` reads a block label (architect, #1150,
  [ruling](https://github.com/synnaxlabs/foundation/issues/1150#issuecomment-6037095151)).
  `value::Kind::text` gives the text of a string or of a reference, so each place that
  reads the two as the same text matches them once (`laptop.architect-2`, #1702,
  2026-10-08T06:05:28Z,
  https://github.com/synnaxlabs/foundation/issues/1702#issuecomment-6053513102).
  `export` and `discover` write every name as a string (`"site_a.pt_1"`): they need no
  HCL rule, and a generated file reads back as exactly the Document it came from. This
  replaces the #363 ruling that a file writes a reserved name only as a string. The
  advisor decided (names and architecture delegations, 2026-10-05), #536 and #701. Lost:
  a reserved call `name("40001.x")`, which reserves a function name and adds an error
  for names that a string already carries; it can be added later without breaking a
  file. Lost: bare names in generated files, which changes only how a file looks. Lost:
  `export` and `discover` write only such a name as a string, which copies HCL's
  identifier rule into `config` and layer 3. Lost: `write` gives such a reference as a
  string, which reads back as a `String` and changes the spec hash. Lost: A3 segments
  that start with a letter or `_`, which shrinks the name model to fit one file format.
  The person decided on 2026-10-05 ("a is fine"), #519. Lost: a new `Expected` variant
  for a name after `.`, a public change when the error already names what may come at
  the `.`. #363.
- **DOCUMENT KEYS** `document::read::unknown` reports each attribute and each block of a
  body that its reader does not take, with the attribute keys and the block keywords
  apart, so a key that names an attribute never passes as a block. One function holds
  both checks, so a kind cannot forget one half. When a body takes blocks and no
  attribute, as a file does, the fix of an attribute is to move it into one of those
  blocks. `read::missing` reports a body with none of some keys, and panics through
  `one_of` on an empty list, which is a defect of the caller. `read::required` reads one
  key or gives that diagnostic. `config` uses them, also at the top level of a file, and
  so does each kind, so one mistake has one code: `document.unknown-attribute`,
  `document.unknown-block`, and `document.missing-attribute`. Decided by
  `laptop.architect-2` on #1153
  (https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051327019,
  2026-10-08 03:05 UTC) and on #1772
  (https://github.com/synnaxlabs/foundation/pull/1772#issuecomment-6051559819,
  2026-10-08 03:27 UTC, and
  https://github.com/synnaxlabs/foundation/pull/1772#issuecomment-6051578111, 2026-10-08
  03:29 UTC). Lost: a `Body` value that records each key read and reports the rest at
  `finish`, which drops the diagnostics when a caller returns early;
  `unknown_attributes` and `unknown_blocks` as two functions; a public
  `UNKNOWN_ATTRIBUTE` code for a caller to match on. `read::one_of` lists words in
  backticks for a fix, such as "`a`, `b`, or `c`", and panics on an empty list. It is
  public for the `config.bad-action` fix, so no copy goes into `config`. Decided by
  `laptop.architect-2` at 2026-10-08T03:54:12Z
  (https://github.com/synnaxlabs/foundation/pull/1781#issuecomment-6051829474).
  `read::labels::<N>` checks that a block has `N` labels (`document.label-count`),
  `read::repeated` reports each block of a keyword that takes one after the first
  (`document.repeated-block`), and `read::span` refuses a span below zero
  (`document.negative-span`), since each attribute that reads a span needs zero or
  more. The first attribute that takes a negative span adds its own reader, named for
  its meaning, such as an offset. `config` and `connector::reader` use them, and the
  `config.*` codes for these went. Lost: `read::duration` beside `read::span`, since
  A9 names `Span` of any sign a duration; `unknown` with a count for each block, which
  changes each caller of `unknown` for one caller of `repeated`. Decided by
  `laptop.architect-2` at 2026-10-08T07:04:36Z
  (https://github.com/synnaxlabs/foundation/issues/1785#issuecomment-6054474145).
  Supersedes the clause "A negative span reads, and each caller owns its bound" of
  https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6037207886, and its
  fix for `time::Error::Long`, "Use a span from "-106751d" to "106751d"": that fix is
  now "Use a span from "0s" to "106751d"", since each span it names reads. Decided by
  `laptop.architect-2` at 2026-10-08T07:36:01Z
  (https://github.com/synnaxlabs/foundation/pull/1828#issuecomment-6055037900).
- **HCL VERDICTS (2026-10-05)** `oracles/conformance/hcl/` holds HCL texts, each with
  the verdict of a pinned HCL version: accepted or refused. For each accepted text, a
  small Go program next to the texts lists the diagnostic code that `read` gives for
  each form outside data in it, such as `hcl.null`. A test checks that `read` accepts
  exactly the accepted texts with no code, refuses each other accepted text only with
  `Error::Form` of the codes listed for it, and refuses each refused text.
  `differences.txt` lists each text where `read` differs from HCL on purpose, with its
  outcome and the decision behind it, and the test checks that outcome instead. For
  each text that reads, `write` must give the bytes of a text in the directory that is
  accepted with no code and is not in `differences.txt`, and those bytes must read as
  the same Document. The program records the HCL
  version. A person runs it by hand when the texts change; CI does not run it and
  needs no Go. It is the only Go code in the repo. The person decided on 2026-10-05
  ("Yeah that's fine", #460); the coordinator approved the plan on #460.
  The program also writes the values HCL reads from each text with only data, in a
  small text form. For each such text that reads and is not in `differences.txt`, the
  test prints the Document in the same form, and the two must be equal. So the test
  checks which numbers are integers (HCL READER), and it compares the bits of each
  float with the `f64` nearest to the written number. HCL holds a 512-bit value,
  and a second rounding to `f64` can miss the nearest one. Lost: cty JSON, which has
  no value for a reference, a call, or a block, and gives a number as a 512-bit
  decimal; the shortest decimal of a float, which Go and Rust write differently for
  some floats; Rust that reads the form into a Document, which is more code than a
  printer; and Go that writes `document::encoding`, a second implementation of the
  encoding. Decided by the `config` builder (#497).
- **HCL UPDATE (2026-10-05)** `config_hcl::update` changes a file so that it reads as
  a new Document. Each attribute and block that keeps its value and its place keeps its
  bytes, comments, and blank lines. A changed value and a changed block on one line
  are written again, without the comments in them. A removed item is cut with the
  comment lines directly above it, up to a blank line. A new attribute goes after the
  kept attribute before it in key order, and a new block after the kept block before
  it. The k-th block of a keyword and labels pairs with the k-th new one, and the
  longest run of pairs in the same order stays, so a moved block is cut and written
  again. New text takes the file's line end. Lost: an edit list by span, which puts
  the diff on each caller; returning edits, which each caller must apply; a lossless
  syntax tree with comments as trivia, which needs a second tree type in the reader;
  writing the whole file with comments attached to items, which loses the layout; and
  moving the bytes of a moved block, which a caller that changes the Document it read
  never needs. Decided by the `config` builder; approved by the coordinator (#249).
- **HCL ERRORS (2026-10-05)** Each function of `config-hcl` gives only the errors it
  can have. `read` gives a list of `Error`, `write` a list of `Unwritable`, and
  `update` a `Refusal`: the problems in the old text, or else the parts of the new
  Document that HCL text cannot hold. Nesting past the depth limit is
  `Error::TooDeep` from `read`, with `document`'s diagnostic; `write` and `update`
  take a `Checked` Document, so they cannot meet it. Lost: one `Error` for all three,
  so each caller of `read` handled a variant that `read` never gives; a `write` that
  takes a plain Document and clones it into a `Checked`, which copies each tree only
  to check its depth and keeps `Unwritable::TooDeep`; and an `update` that takes the
  Document that `read` gave for the text, so it gives only `Unwritable`, but writes
  wrong text with no error when a caller gives another Document. Decided by the
  `config` builder; approved by the coordinator (#330). `write` and `update` take a
  `Checked`, and `read` does not change: decided by the architect (#828,
  https://github.com/synnaxlabs/foundation/issues/828#issuecomment-6030891911).
  Trigger for `read` to give a `Checked`: the first production code that calls
  `Checked::new` on a `read` result, or writes one back with `write`, files an
  `interface` issue that names it. Until then no code checks the depth twice, and a
  checked whole Document does not make a checked connector body. Decided by the
  architect (#1089,
  https://github.com/synnaxlabs/foundation/pull/1089#issuecomment-6031438972).
- **DIAGNOSTICS (2026-10-05)** A problem that a person or an agent fixes in a
  Document or its file is a `document::diagnostic::Diagnostic`: a stable `Code`, a
  span, a message, a fix, and notes (other places that explain it). The span is `None`
  only for a Document with no spans; a problem with a whole file has an empty span at
  the start of the file. The message and the fix have no final period, and each
  producer's tests pin both. A code is `<producer>.<problem>`: each part is lower-case
  ASCII letters and digits, starts with a letter, and may join words with single `-`
  (`hcl.syntax`, `document.duplicate-key`). The producer is a name it owns: the syntax
  of a front end, a core crate, or a kind. No two producers share a name. A producer
  declares each code as a `const` item, so a bad code fails the build. A code never
  changes between releases. Each producer maps its own errors with `From<&Error>`
  beside them, so `config`, `ops`, and `node` never match a producer's variants. An
  error from a crate below `document` that a producer shows as a diagnostic gives its
  message with `Display` and its fix with `fix()`; the producer adds the code and the
  span. A fix that shows a value in a Document shows it as the file writes it, so
  `document.bad-size` quotes the size for `Syntax` and `Range` (`Use at most
  "16777215TiB"`), while `byte::Error::fix` stays bare for a flag (architect,
  https://github.com/synnaxlabs/foundation/issues/1070#issuecomment-6032077046).
  `size_fix` stays in `document` (#650): it works on the text the reader read, and its
  only caller is the reader. It moves to `types` when a second reader of sizes needs it
  (same ruling).
  `Diagnostic` is `#[non_exhaustive]`, so a new field with a default in `new`
  breaks no producer. No severity field: the warnings in K2 and R13-10 belong to plan
  output.
  `ops` operation error codes use `Code` too, so the grammar has one home. A code
  crosses the wire as text, and no reader makes a `Code` from it. Lost: a `Diagnose`
  trait behind `Box<dyn>` (not `Clone`, and a fix is optional); number codes (a
  central registry, and unreadable); one span only (the first producer has two
  places). Codes go into `oracles/conformance/document/` at the first stable release;
  the person decided on 2026-10-05 ("At the first release"). Decided by the `config`
  builder; approved by the coordinator (#137).
  A message or a fix quotes text from a file with `types::text::Quoted`: U+0020 to
  U+007E as written, except `\"`, `\\`, and `$` or `%` for a `$` or `%`
  before `{`; each other character as `\u` and four upper-case hex digits, or `\U` and
  eight above U+FFFF. HCL, YAML, and TOML read the form back as the text, and a
  look-alike shows. The `config-hcl` writer keeps its own rule, because a person edits
  what it writes. Lost: Rust's `Debug` form, which no file reads; `$$` and `%%`, which
  only HCL reads. Decided by the architect (#941).
  Until `types::text::Quoted` is on `main` (#941), a producer quotes text from a file
  with `{:?}`, and #941 changes each such quote to `Quoted`. Decided by the architect
  at 2026-10-08T05:49:29Z
  (https://github.com/synnaxlabs/foundation/issues/941#issuecomment-6053293087).
- **K2 (tunable)** The core knows only full names and regions. `plan` groups changes by
  region. One directory per region is the default layout that `init`, `discover`, and
  `export` write; `plan` warns on a mismatch. Full names everywhere, no imports.
  `discover` proposes diffs and never overwrites.
- **K3** Files in Git are the desired mesh; the spec is the running mesh. Commands:
  `discover`, `plan`, `apply <plan>` (commits exactly the reviewed plan; refuses if the
  spec changed; compare-and-swap on the pointer), `explain`, and `export`. Each has
  `--json` with stable change kinds.
- **A2** The files list every channel. Hardware channels enter through `discover`. A
  file may open a name folder to one program, where the first write creates the
  channel; `plan` lists such channels until they are added to files or deleted.
  Mechanism: X28.
- **S12 + C8 amendment** Settings are policies that select names. One `Selector`
  (patterns with `*`, `**`, and `!` exclusions) serves subscriptions, readers,
  connectors, policies, and access. Most specific pattern wins; equal specificity is a
  plan error; `explain` shows each effective value and its source. Equal specificity
  means a tie between the most specific policies, per budget for node settings
  (SPECIFICITY). A rename can move a channel under other policies, and `plan` shows it.
  Current policy kinds: retention, placement, transmission, compression, reduction,
  time, access, secret store, and node settings (NODE SETTINGS). Targets and combination
  rules: X25, X26. Specificity: SPECIFICITY (#3).
- **NODE SETTINGS (2026-10-05)** A node's disk budget and pool budget are a policy
  that selects node names: `node_settings "<name>" { select, disk, pool }`, such as
  `select = "site_a.*"` and `disk = "200GiB"`. Each budget is optional and above zero.
  A node that no policy selects computes a default from its free disk and memory at
  start, so a mesh with no policy works. Before it reads the spec, a node uses the last
  budget it applied, which it keeps in its data directory; the first start uses the
  default. A policy that sets no budget is a user mistake, refused as normal
  validation with a fix (DIAGNOSTICS, #869, #1000). The data directory is node-local:
  a start argument of `foundation`, with a default, because the spec is stored in it.
  Node-local config for the budgets lost: `plan` cannot show it and `apply` cannot
  change it. Proposed by `ops`; the person decided on 2026-10-05 ("Yeah mesh node"),
  #342. The `config` builder added the label and the bound above zero (#474).
  Each shard's part of the pool budget must hold the largest block its buffer takes;
  a smaller part stops the node at start with `Error::Buffer`. `config` cannot check
  it, because the shard count belongs to the node, so the buffer is the one place
  that refuses it. Decided by the architect on #1062:
  https://github.com/synnaxlabs/foundation/pull/1062#issuecomment-6030791343.
- **SHARD DISK (2026-10-07)** Until segments exist, `node` gives each shard's ring
  `disk / n` of the node's disk budget (`node::Config::disk`, a `types::byte::Size`),
  and shard 0 also gets the remainder, as SHARD POOLS does. The budget bounds each new
  ring file: `node` takes the largest ring whose file fits each part
  (`buffer::Layout::fit`), so the format stays in `buffer`. When a part holds no ring,
  no shard starts, and `join` gives `Error::Disk` with the budget, the shard count, and
  the least budget (`n` times the least ring that `fit` gives, capped at the largest
  `Size`); `config` cannot check it, as for the pool part (NODE SETTINGS). The ring is
  the whole store. A ring with a checkpoint keeps its size, which can be more than its
  part, until `Buffer::resize` exists (#451). A ring with none is made again at its part
  (#1254). So oldest first (B1) holds per shard, not per node. This is a patch. The
  long-term path is small rings for commits, then segments that draw from one node-wide
  allowance (#1081). The 5.5 lab sizes the budget for the shard that holds the index.
  With `Buffer::resize`, `node` computes the `Layout` with `fit` and calls
  `Buffer::resize`, nothing more: `buffer` sets the file length itself, so `node` never
  extends or cuts the ring file and does not learn the format (after
  https://github.com/synnaxlabs/foundation/issues/451#issuecomment-6032821843).
  Decided by the architect, #342:
  https://github.com/synnaxlabs/foundation/issues/342#issuecomment-6030837040,
  https://github.com/synnaxlabs/foundation/issues/342#issuecomment-6032845187,
  https://github.com/synnaxlabs/foundation/pull/1180#issuecomment-6033305257,
  https://github.com/synnaxlabs/foundation/pull/1180#issuecomment-6033404672,
  https://github.com/synnaxlabs/foundation/pull/1180#issuecomment-6033884447, and, for
  a ring with no checkpoint (2026-10-07T08:52:34Z),
  https://github.com/synnaxlabs/foundation/pull/1286#issuecomment-6034449677.
- **POLICY NAMES (2026-10-05)** The label of a policy is a name (A3), unique among the
  policies of its kind. Its tree key `<label>.@<kind>` is a name too, so a label holds
  at most 255 bytes less the suffix (240 for `node_settings`). A policy name can equal a
  channel name. A policy belongs to the region that governs its name (X2: the longest
  region prefix that contains it), and it may select only names in that region and its
  descendants (X26). When a `region` block is added or removed, `plan` checks X26 again
  for each policy whose region changes, lists each policy that moves to other voters,
  and refuses one whose reach fails. Lost: the region from the selector (a wider pattern
  would move the policy to other voters silently, and X26 could never fail), and the
  region from the directory (K2 makes the layout a default only; r3 rejected a
  `region =` attribute). The advisor approved it on 2026-10-05, #474.
  The `<kind>` segment of each kind is its HCL keyword: `@access`, `@region`,
  `@node_settings`, `@compression` (compression section), `@placement` (S12),
  `@retention` (#895), `@subject`, and `@time`. No time keyword was on record (C6 shows
  `[[time]]`, and X36 replaced its content), so the architect decided `time`.
  `laptop.architect-2` decided `@subject` at 2026-10-08T03:15:41Z
  (https://github.com/synnaxlabs/foundation/issues/1755#issuecomment-6051435217).
  A connector has no segment: it is at its own name, and its channels are its children
  (#758, 2.2, C8). A channel has no segment either: it is at its own name (#756,
  https://github.com/synnaxlabs/foundation/issues/756#issuecomment-6031378098). The `@`
  check still applies to both names. A region record is at `<prefix>.@region` in the
  parent's tree (#758). This is not an exception to X2: the region that holds the record
  is the longest region prefix that contains `<prefix>`, other than `<prefix>` itself.
  The root region has no record and no key: no parent records it (X3), and its voters
  live only in its Raft config. The one place that maps a key to its region applies
  this, so no caller tests for `@region`. Decided by the architect, #1001
  (https://github.com/synnaxlabs/foundation/issues/1001#issuecomment-6031305302; #758
  for the connector and the region). The kind is `spec::definition::Kind`, and the
  module `spec::key` holds the whole key rule: the segments, the `@` rule, the bound,
  `Kind::key`, and `key::Error`. Lost: a module `spec::kind`, because in `spec` "kind"
  also names a connector's driver. Decided by the architect, #1109
  (https://github.com/synnaxlabs/foundation/pull/1109#issuecomment-6031286198 and
  https://github.com/synnaxlabs/foundation/pull/1109#issuecomment-6031290037).
  `Kind::key` takes the label as text and checks it in this order: the bound
  (`key::Error::Long`, the one length error for every kind, with `Name::MAX_BYTES` for a
  connector), then the name (`key::Error::Name`), then the `@` rule. So the user gets
  the true bound in one round. Lost: a `&Name` label, whose parse gives its own length
  error with the wrong bound. Decided by the architect, #1109
  (https://github.com/synnaxlabs/foundation/pull/1109#issuecomment-6031559597).
- **CHECK ORDER (2026-10-07)** `config::check` gives the same entries for each order
  of the Documents, or problems in each order, so the meaning of a mesh's files does
  not depend on the order that a tool reads them. The problems can differ. Decided by
  architect-2 (#1444, 2026-10-07T17:08:05Z,
  https://github.com/synnaxlabs/foundation/pull/1444#issuecomment-6042832407).
- **CHANNEL BLOCK (2026-10-08)** `channel "<name>" { kind, ... }` defines one channel
  (S5) at its own name. `kind` is `"index"` or `"data"`, and `"data"` is the default.
  An index takes `error` and `control`. A data channel takes `index` and `data_type`,
  which it needs, and `quality` and `unit`. Each value is a string or a reference.
  `config::check` gives `config::Definition::Channel`, a `spec::channel::Kind<Name>`
  whose edges are names until `plan` gives each channel its key. `Definition::Spec`
  holds each other definition. Each edge must name a channel that a `channel` block of
  the Documents defines, or `check` gives `config.unknown-channel`, at the span of the
  edge, in source order. An edge to a channel that only the stored spec has (X28) gives
  it too, until #1082. Lost: `spec::definition::Definition<C = Channel>`, because `plan`
  would then wrap each of the eight variants again to change one. Decided by
  `laptop.architect-2` (#1152, 2026-10-07T11:17:44Z,
  https://github.com/synnaxlabs/foundation/issues/1152#issuecomment-6036793927, and
  2026-10-08T00:51:39Z,
  https://github.com/synnaxlabs/foundation/issues/1152#issuecomment-6049880294).
  After a bad `kind`, `check` gives `document.unknown-attribute` for each attribute that
  no kind knows, and leaves each other attribute, the edges too: each belongs to one
  kind, so its problem depends on the kind. Decided by `laptop.architect-2`
  (2026-10-08T05:54:24Z,
  https://github.com/synnaxlabs/foundation/pull/1806#issuecomment-6053360334).
  Supersedes clause 1 of the #1758 ruling, "the edges, as now" (2026-10-08T02:41:50Z,
  https://github.com/synnaxlabs/foundation/issues/1758).
- **ACCESS BLOCK (2026-10-08)** `access "<name>" { subjects, select, allow, authority }`
  (C8) gives a `spec::access::Policy` at `<name>.@access`. `subjects` and `select` are
  selectors. `allow` is one action or a list of actions, each a string or a bare word,
  so `["read", "write"]` and `[read, write]` read the same; a repeat is one action, and
  an empty list is `config.empty-allow`. A word that is not an action is
  `config.bad-action`. `authority` is optional, an integer from 0 to 255
  (`config.bad-authority`). With no `authority`, a write is capped at `Authority(0)`,
  the least, as default deny gives the least. Lost: an `authority` that `write` makes
  required, a rule that C8 does not have. The action words are a table in `config` until
  a second reader needs them, such as the `plan` output of access; then they move to
  `spec` as `Action::as_str`. Decided by `laptop.architect-2` (2026-10-08T02:41:38Z,
  https://github.com/synnaxlabs/foundation/issues/1017#issuecomment-6051076121).
  The gate: "The gate gives authority 0 no special meaning: such a writer outranks no
  writer and follows GATE RULES, so it takes control when it opens on an index that no
  writer holds." `control` and `home` must not read `Authority(0)` as "may not write" or
  "may not take control". A change to that is a change to GATE RULES, and it goes to
  `laptop.architect`. The quoted sentence supersedes the sentence on the gate in
  https://github.com/synnaxlabs/foundation/issues/1017#issuecomment-6051076121.
  `laptop.architect` decided it and approved the default cap (2026-10-08T05:21:56Z,
  https://github.com/synnaxlabs/foundation/issues/1017#issuecomment-6052936198).
  An `authority` with no `write` in an `allow` that reads is
  `config.authority-without-write`, also `authority = 0`: only a write uses an
  authority, so the value is a mistake. `Policy::new` still sets the authority of a
  policy with no `write` to zero. Lost: no diagnostic, which hides the mistake. Decided
  by `laptop.architect-2` at 2026-10-08T04:00:34Z
  (https://github.com/synnaxlabs/foundation/pull/1781#issuecomment-6051909712).
  It reads two attributes together, so it runs only when each attribute of the block is
  known and reads, as `config::check` states for a whole definition. Decided by
  `laptop.architect-2` at 2026-10-08T04:24:33Z
  (https://github.com/synnaxlabs/foundation/pull/1781#issuecomment-6052187547).
  Supersedes the silent `authority` of
  https://github.com/synnaxlabs/foundation/issues/1017#issuecomment-6051076121.
- **CONNECTOR BLOCK (2026-10-08)** `connector "<name>" { kind, node, ... }` (X22)
  gives a `spec::connector::Connector` at its own name, which is unique in any case
  among the keys of every block. `kind` and `node` are names, and each is required. The
  config is the body without `kind` and `node`. `config::check` takes a
  `connector::kind::Table`, and the kind that `kind` names checks the config through
  `Table::check`, with `at` the span of the `kind` value. A kind that the table does
  not have is `connector.unknown-kind` there. Decided by `laptop.architect-2` on #1153
  (https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152,
  2026-10-08 03:02 UTC).

### 1.12 Access, identity, and secrets

- **C8** A subject is anything that reads or writes (person, agent, program, connector),
  named in the tree and governed by its region. People, agents, and programs
  authenticate with keys; a node vouches for its connectors. Access is allow-only,
  default deny, with no conflicts:
  `access "<name>" { subjects, select, allow, authority }`. Every access policy has a
  name: it is unique among access policies, it decides the governing region, and the
  tree key is `<name>.@access` (for example `site_a.operators.@access`). The person
  approved the name on 2026-10-06 ("Yes I confirm", #729). Actions: read, write, plan,
  apply, secret, admin. No groups or roles; a group is a selector over subject names. A
  connector may write channels under its own name by default. The connector default
  caps authority at ABSOLUTE. Decided by the advisor on 2026-10-06, #455. `plan` lists
  access changes separately. SSO comes later.
- **SUBJECT KEYS (2026-10-08)** A person, an agent, or a program is a
  `spec::subject::Subject` at `<name>.@subject`, which holds its Ed25519 public keys
  (`types::ed25519::PublicKey`): at least one, each distinct, sorted by their bytes so
  the order of a file does not change the definition. The `subjects` selector of an
  access policy matches `<name>`, not the tree key, and #1747 makes `access::admit` read
  `<name>.@subject`. A connector has no subject definition: it stays at its own name
  with no keys. The encoding is tag 10, a count, and 32 bytes for each key in ascending
  order; `decode` checks the count against the bytes left before it allocates, and
  refuses an empty list, keys out of order or equal, and a key of small order. Lost: the
  subject at its plain name, which takes that name from a channel or a connector and
  allows no children. Decided by `laptop.architect-2` at 2026-10-08T03:15:41Z
  (https://github.com/synnaxlabs/foundation/issues/1755#issuecomment-6051435217).
  `access::Rules` keeps each subject by its tree key, and `admit` and `verify` build
  that key from the hello's subject with `spec::definition::Kind::key`, so no caller
  builds it and only `spec` holds the key form. A subject that makes no key gives
  `Error::Unknown`. Lost: a public `Kind::label` in `spec`, which only `access` calls.
  The first ruling kept each subject by `<name>` (`laptop.architect`,
  2026-10-08T06:56:19Z,
  https://github.com/synnaxlabs/foundation/issues/1747#issuecomment-6054321636). The
  tree key was decided by `laptop.architect` at 2026-10-08T08:10:33Z
  (https://github.com/synnaxlabs/foundation/pull/1834#issuecomment-6055629911), which
  supersedes the `<name>` of the first.
- **SUBJECT PROOF (2026-10-08)** `access::Rules::admit` checks a signed
  `types::hello::Hello` and gives an `access::proof::Admitted`, which no other code
  builds. The owner keeps it for the connection, and `Rules::verify` takes it with each
  request. `verify` finds the key in the spec again and checks the time again, so a
  spec that removes the key stops the next request, and no caller writes its own
  expiry check. The signed bytes are a contract for each SDK; each integer is
  little-endian:
  - hello: the 18 bytes `foundation/hello/1`, the subject length (1 byte), the
    subject, the key (32), `via` as a `u128` (16, the reverse of the byte order of its
    UUID text), the connection key (16), the nonce (16), and `expires` in nanoseconds
    (8);
  - request or session open: the 20 bytes `foundation/request/1`, the connection key
    (16), and the exact bytes that the program sent.

  `types::hello::Hello::encode` writes the fields of the hello after the tag, so
  `access` and `wire` share one encoder of the field run, and a test in `types` pins
  its exact bytes (`laptop.architect`, 2026-10-08T10:15:03Z,
  https://github.com/synnaxlabs/foundation/pull/1854#issuecomment-6057658762).

  The tags take the MESH LOG form, so each SDK reads one form, and no tag is a prefix
  of another (`laptop.architect`, 2026-10-08T07:39:12Z,
  https://github.com/synnaxlabs/foundation/issues/1747#issuecomment-6055096691). A
  request is checked with the key of the hello over its connection key, so a request
  signed with another key or for another connection gives `Error::Signature`, and no
  connection check exists. `peer` is the node that carried the hello: this node when the
  program connected to it, else the node whose transport session forwarded it. A hello
  is live while the latest mesh time is before `expires`, and `expires` may be at most
  `proof::CAP` (15 minutes) past the earliest mesh time. Lost: `access` decodes the
  signed bytes of the hello itself (design B), because `access` then owns a decoder of
  outside input and the hello's wire form, which HUB WIRE gives to `wire`; a free
  `verify` of any `&Hello`, which accepts a key that the program picked when a caller
  skips `admit`; and `Error::Connection`, which the signature makes needless. `admit`
  does not check `nonce`: the node that `via` names checks that it is the challenge
  that it sent (#1748; `laptop.architect`, 2026-10-08T08:10:33Z,
  https://github.com/synnaxlabs/foundation/pull/1834#issuecomment-6055629911). A
  hosted proof waits on #1832. Decided by `laptop.architect` at
  2026-10-08T07:35:46Z
  (https://github.com/synnaxlabs/foundation/issues/1747#issuecomment-6055033237).
- **CLIENT HELLO (2026-10-08)** A program's session with the node it connects to
  has a hello stream and request streams (`wire::hub::client`, kinds 4 and 5). Hub
  kinds are one space: the first byte of a hub stream picks its decoder
  (`laptop.architect`, 2026-10-08T10:19:16Z,
  https://github.com/synnaxlabs/foundation/pull/1854#issuecomment-6057726181). The
  hello stream is the first hub stream and lives as long as the session. The node sends
  a `Challenge` (a fresh nonce and its mesh time) first and after each admitted hello;
  the program sends a `Signed` hello that echoes it, first and to renew at half its
  life. The wire hello is kind 4, the signed bytes of the hello with no tag
  (`Hello::encode`), and the signature, so an SDK writes one form (`laptop.architect`,
  2026-10-08T10:15:03Z,
  https://github.com/synnaxlabs/foundation/pull/1854#issuecomment-6057658762). Each
  refusal on the hello stream ends the session. A request stream carries one
  `Request` and its body, then one `Response` and its body; each body is a run of at
  most `BODY_BYTES_MAX` (16 MiB), which the SDK checks before it sends. A body over the
  cap at the node gets `MALFORMED`. `access::Rules::renew` checks a renewal as `admit`
  checks a first hello, after `Error::Changed` for another subject, key, `via`, or
  connection, and takes the carrier from the `Admitted`: a move to another node is a
  new connection. `Changed` names the first field that differs, an
  `access::proof::Field` (`laptop.architect`, 2026-10-08T10:19:16Z,
  https://github.com/synnaxlabs/foundation/pull/1854#issuecomment-6057726181).
  Stop codes: `REFUSED` 20 for an unknown subject, an unlisted key, or
  a bad signature, which tell about the spec and so share one code (`Signature` too:
  `admit` checks it only for a listed key); each other step its own code, `UNSYNCED`
  21, `STALE` 22, `VIA` 23, `EXPIRED` 24, `CAPPED` 25, `CHANGED` 26. `hub` maps each
  `access::proof::Error` to its code in one exhaustive `match`, and `serve` returns the
  exact error for the node's log. `connection::Key` writes as UUID text in byte order.
  Lost: one `REFUSED` for each refusal, which hides an unsynced node from a program;
  and a verify before the spec lookup, so that each refusal costs the same, which costs
  a verify for each hello from an unknown client. Decided by `laptop.architect` at
  2026-10-08T09:53:58Z
  (https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6057298526),
  which extends the refusal ruling of #1744
  (https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6053329389).
- **REGION PREFIX** `access::Rules::new` takes the definitions of each region tree,
  with the region as a `types::name::Prefix`; `Prefix::ROOT` is the root region. Access
  picks out the policies, connectors, and subjects itself. A policy reaches a name when
  `Prefix::contains` holds, so no caller writes the root case. Decided by
  `laptop.architect` on 2026-10-07T12:47:19Z
  ([#1383](https://github.com/synnaxlabs/foundation/issues/1383#issuecomment-6038223777));
  applied in #1402. The trees in place of the policies: `laptop.architect`,
  2026-10-08T03:01:36Z
  ([#810](https://github.com/synnaxlabs/foundation/issues/810#issuecomment-6051285927)).
  The subjects: `laptop.architect`, 2026-10-08T06:56:19Z
  (https://github.com/synnaxlabs/foundation/issues/1747#issuecomment-6054321636), by
  their tree key at 2026-10-08T08:10:33Z
  (https://github.com/synnaxlabs/foundation/pull/1834#issuecomment-6055629911).
- **K4** Config refers to secrets by name only. Values never appear in files, plans, or
  output. Secrets are write-only (`secret set`, `secret delete`). `plan` checks that
  every reference resolves. Agents wire references but never see values.
- **BQ16** A secret value is encrypted to each eligible node (the placement's nodes,
  standbys included). Voters store ciphertexts outside the spec. Only `secret set` and
  `secret delete` change them. On a key rotation or a new standby, a node that can
  decrypt re-encrypts. `discover` takes credentials for one call only.
- **SECRET STORES AS ADAPTERS** `ctx.secret(name)` is the only path for kinds. Adapters:
  the built-in sealed store (default, works offline), a node environment variable or
  file, HashiCorp Vault, AWS Secrets Manager, Azure Key Vault, GCP Secret Manager, and
  Kubernetes Secrets. A policy picks the store per name. External adapters authenticate
  with the node key and may cache values sealed to it (this delays revocation).
- **BQ12 (locked 2026-10-04)** Identity across forwarding nodes. The person adopted
  r15 in full (sections 4 and 6). A remote subject signs a short-lived hello and each
  session open, the gateway forwards the signatures, and the owner verifies them
  against the subject's keys in the spec. A node acts only as itself or as connectors
  placed on it. Node-to-node traffic is authorized by role. r15 decisions 2 to 10 hold
  as written there (session opens signed, a key list per subject in OpenSSH format,
  `apply` signs the plan hash, every node checks every change record, audit records
  the subject and the forwarding node, MCP runs beside the agent, the caller seals
  secret values, no end-to-end frame integrity in v1). A secret write is a
  `mesh.changes` record, so every node checks its subject signature and the `secret`
  action on the name against the spec. Applies r15 decisions 4, 5, and 9; approved by
  the coordinator (#409).
- **FIRST ADMIN (2026-10-08)** A node that starts a new mesh has an empty spec, and
  under BQ12 only a key that the spec names can sign an apply. So the first `foundation
  start` of that node, on an empty data directory, creates the spec with one admin
  subject. It writes the admin's private key into the data directory, and only the user
  who started the node can read the key. The CLI on the same host signs with that key,
  so the first `apply` needs no key step. A node that joins by ticket (BQ11a) joins a
  mesh that has a spec, so it creates none. BQ12 holds as written: each node checks each
  apply, the first one too, against a key in the spec. Lost: the first apply from any
  local process, because any local user could then take the node. Also lost: an admin
  public key given before the first start, a step before the first use. Decided by the
  person ("Yes, I approve."), relayed by `laptop.monitor` at 2026-10-08T02:43:43Z:
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6051096981. The
  #1744 plan names the subject, its access policy, and the key file, as
  `laptop.architect-2` and `laptop.architect` decided (2026-10-08T02:46:51Z,
  https://github.com/synnaxlabs/foundation/pull/1759#issuecomment-6051130026). The
  question was about a node whose spec is empty. So `laptop.architect` decided the limit
  to a node that starts a new mesh, and the sentence on a node that joins
  (2026-10-08T02:57:01Z,
  https://github.com/synnaxlabs/foundation/pull/1760#issuecomment-6051238643). It also
  decided that the #1744 plan names how a first start tells a new mesh from a join
  (2026-10-08T02:59:40Z,
  https://github.com/synnaxlabs/foundation/pull/1760#issuecomment-6051265693).

### 1.13 Operations, agents, and the factory

- **BENCH SPEND (2026-10-04, replaced by the test budget in 5.5 on 2026-10-05)** Linux
  benchmarks that need real machines run on rented AWS machines. The person: "you're
  welcome to provision AWS machines. SET STRICT COST LIMITS. I don't want more than $100
  spent". The limit is 100 USD in total, across all benchmarks, until the person raises
  it. Only the coordinator provisions, by the procedure in `docs/coordination.md`.
- **C7** One operation table (typed input and output, error codes, read-only and
  destructive flags) generates the CLI (`--json`), annotated MCP tools, and docs
  embedded in the binary. Every error has a stable code and a fix-it hint. Status is
  channels; there is no status API. `foundation init` writes an agent guide.
  `plan --simulate` runs a plan on a simulated mesh.
- **C7 + D12 rejected** Each SDK has a generated base layer and a hand-written native
  layer. Each SDK's data path is hand-written as the fastest idiomatic code for its
  language. Guard: the Rust codec is the reference; golden vectors from it run in every
  SDK's CI; differential fuzzing runs both ways; the drift agent watches it.
- **D5** Developers extend Foundation through SDKs (Python and Rust first), not in-node
  plugins. A plugin system is open.
- **BQ17** The `ops` crate holds the operation table and generates the CLI, MCP tools,
  and docs.
- **BQ19** `sim` ships in the binary behind a feature that product builds turn on. Its
  size is measured against P1.
- **BQ11b** `node` pulls each crate's values and writes status channels through `hub`.
  Rebalancing is an outside controller that reads status channels and acts through
  plan and apply.
- **STATUS CHANNELS (2026-10-06)** `node::status::TABLE` is the fixed set of a node's
  status channels: `clock.status` (`U8`: 0 unsynced, 1 synced, 2 holdover),
  `clock.offset` (`Span`), and `clock.error` (`Span`; an unknown error is the bound of
  `estimate::Measurement::unknown`). An unsynced clock gives no offset or error. Each
  table entry maps the pulled status to its value. A pure `Collector` pulls each value
  from a reader that its crate gives; no crate calls `node`. Lost: each crate pushes
  status events to a sink (BQ11b locks pull). Decided in #728.
- **OWN REPO (revises C9a)** Foundation lives in its own private repository,
  `synnaxlabs/foundation`, with one Cargo workspace: `crates/` (crate list in section
  4), `xtask/`, `oracles/`, and later `sdk/` and `bench/`. Every PR runs the layer
  check (`cargo xtask layers`).
- **C9b** Work loop: a planning session splits a phase into tasks that own crates
  (amended by MILESTONES: builders file the issues on the milestone path); one agent per
  task in its own worktree; machine gates (build, lints, layer and stand-alone checks,
  unit and property tests, thousands of simulation runs, short fuzz, the 5% benchmark
  check (P1), mutation testing on the diff); fresh adversarial reviewers (amended by
  REVIEW TIERS); the merge queue (amended by MERGE QUEUE).
- **C9c** Oracles are enforced by visibility. A script writes an oracle section at the
  top of each PR summary and flags weakening. Each flagged change gets its own
  adversarial reviewer. PRs merge through the merge queue (MERGE QUEUE). Supersedes: T2
  enforcement level.
- **AGENT REQUIREMENT** Every task must be easy to do with agents. C7 carries it.
- **R16-1 (2026-10-04)** Release builds keep integer overflow checks
  (`overflow-checks = true`), so R9-D10 holds in release too. An intended wrap uses
  `wrapping_*`. The P1 benchmark check measures the cost. Decided by the advisor under
  the quality delegation.
- **R16-2 (2026-10-04)** Release builds set `panic = "abort"`. A broken invariant
  crashes the node, and crash recovery restarts it. Tests keep unwinding. Confirmed by
  the person on 2026-10-04.
- **R16-3 (2026-10-04)** A lint exception is `#[expect(lint, reason = "...")]`, never
  `#[allow]`. Clippy denies `allow_attributes`. Decided by the advisor under the
  quality delegation.
- **R16-4 (2026-10-04)** The workspace turns on the r16 lints that check rules the
  repo already has: `Debug` on public types, one path per item, one unsafe operation
  per block, exact error assertions, no discarded results, determinism, bounded
  loops, and names. The lists are in the root `Cargo.toml` and `clippy.toml`;
  `docs/claude/rust.md` gives the rules. Decided by the advisor under the quality
  delegation.
- **R16-5 (2026-10-04)** The strict decoder lints (`indexing_slicing`,
  `arithmetic_side_effects`, `as_conversions`, `string_slice`) apply only in crates
  that decode outside input: `codec`, `wire`, `document`, `config-hcl`, and each
  protocol parser in a `connector-<kind>`. Layer 1 decision crates (`raft`,
  `control`, `delivery`, `access`, `estimate`) deny `wildcard_enum_match_arm`.
  Decided by the advisor under the quality delegation.
- **R16-6 (2026-10-04)** Each crate keeps one public `Error` enum, or one per
  sub-boundary module. Microsoft's canonical error structs and
  `clippy::error_impl_error` are rejected: an enum lets a test pin the variant, and
  backtrace capture costs time on hot paths. Decided by the advisor under the quality
  delegation.
- **MILESTONES (2026-10-06)** The unit of work is the next acceptance scenario, one
  milestone. Only issues on its path are admitted, a WIP limit stops breadth work while
  it is open, and a new public item needs a caller on the path. Three factories, one per
  machine, each a workstream of crates that change together: box1 (`foundation-factory`)
  the slice core, box2 (`foundation-factory-2`) the edges, and the laptop the
  composition (`node`, `config`, `ops`). The split is in `docs/factory.md`. The person:
  "Yes, I agree, but we'll have three factories."
- **ENGINEERS (2026-10-06)** Each engineer runs one machine's sessions on their own
  Claude account and approves that machine's PRs to the risk crates: `raft`, `buffer`,
  `delivery`, `block`, `ring`, `codec`, `wire`, `home`, `replica`, and `transport`. The
  person: "Yes, I agree."
- **TWO LANES (2026-10-06)** A watched day lane for open questions, public surfaces,
  decisions, and risk-crate code, with flexible hours. An unwatched night lane takes
  only issues marked ready during the day (the contract on `main` and compiling, the
  acceptance tests named and present, no open decision, one crate of the session's
  machine), plus simulation, fuzz, and mutants. The person: "Yes". Supersedes: Factory
  constraint.
- **MERGE QUEUE (2026-10-06)** A ruleset on `main` requires the CI checks, a merge
  queue, and code-owner review, with zero other approvals. Agents act as one GitHub App,
  `synnax-foundation-factory`, with one private key per machine; branches start with the
  machine. The person owns `oracles/`, the decisions, `.github/`, `CLAUDE.md`,
  `.claude/`, and each `public-api.txt`; the engineers own the risk crates. The person:
  "Yes, I agree"; one App: "Yes please". Supersedes: C9c merge exception, MERGE RULE.
- **MESSAGES (2026-10-06, a trial)** Same machine: `SendMessage`. Across machines: the
  factory Claude Code mod over MQTT on AWS IoT Core, which wakes an idle session and
  acks without model tokens. No session polls. GitHub stays the record. The person: "I'm
  willing to give it a try", after a local prototype; IoT Core: "Yes I approve. Let's
  get things set up". Supersedes: REMOTE CONTROL.
- **AWS CEILING (2026-10-06)** 4,000 USD a month for Foundation on AWS, with alerts at
  50, 80, and 100% of the forecast and a budget action that stops both boxes and the ARM
  runners at 100% actual. Each resource stops 72 h after its last renewal; the person
  renews every 3 days (ledger #163). A new resource needs the person's yes with its
  exact price. The person: "Yes, that's fine. I want 3 days deadlines though".
  Supersedes: FACTORY HOST.
- **FACTORY ROLES (2026-10-06)** Fifteen Opus sessions (`docs/factory.md`). Laptop: a
  thin coordinator (board, milestone, ready issues, routing), the architect (crate map,
  boundaries, contracts; takes over the advisor role), two integrators, and the monitor.
  box1: four builders and a red-team. box2: three builders, a connector builder mostly
  on the night lane, and a red-team. `verify`, the daily crew, and the `audit` and `ux`
  routines are retired; `code-quality` and `drift` run weekly. The person: "If you think
  this is the right architecutre i'm ok with it". Supersedes: MULTI-SESSION FACTORY,
  NINE BUILDERS, C9b2, QUALITY SESSIONS, CLOUD ROUTINES.
- **REVIEW TIERS (2026-10-06)** `reviewer` on every PR; `architecture` and `breaker` on
  every code PR; `performance` on hot paths, with measured numbers. A second round runs
  `reviewer` and `breaker` again on the fix commits only, with the earlier findings. A
  deferral in a risk crate needs the architect's explicit OK (the person, 2026-10-07:
  "YES"). 4 of the 5 worst escaped defects came in through a fix or a deferral that
  nothing checked again. Decided by the advisor under the quality delegation.
  Supersedes: BREAKER REVIEW.
- **REVIEW CHECK (2026-10-07)** The required status `review` (`cargo xtask review`,
  `.github/workflows/review.yaml`) passes a PR only when its review is done. It reads
  only round comments by the factory bot, in the format of `/review`, "Round comment".
  Each round comment parses, also an earlier one, and has its `Deferred:`,
  `Public surface:`, and `Hot path:` lines. Each round names the reviewers REVIEW TIERS
  requires, and `performance` when the first word of its `Hot path:` value is not
  `none`. Stated by the issue that the director's audits filed,
  https://github.com/synnaxlabs/foundation/issues/1467 (2026-10-07T15:20:38Z).
  The end lines are the last paragraph of the comment, in that order, as `/review`,
  "Round comment", writes them, each at the start of its line, so an indented quote of
  them or a line in a code block is not them. Each end line may wrap onto the lines
  after it, and a paragraph after them fails. The first word of a value, with its
  backticks and one final comma, period, or semicolon removed, is the word that is
  checked. Decided by the director at 2026-10-08T02:57:36Z
  (https://github.com/synnaxlabs/foundation/issues/1467#issuecomment-6051244793). The
  hand rule for code fences, as REVIEW CHECK stated it at `48101724`, meets that
  ruling. Decided by the director at 2026-10-08T04:01:43Z
  (https://github.com/synnaxlabs/foundation/pull/1752#issuecomment-6051923239). The
  check ends a line at `\n`, `\r\n`, or a lone `\r`, as that ruling covers (decided by
  the director at 2026-10-08T04:42:33Z,
  https://github.com/synnaxlabs/foundation/pull/1752#issuecomment-6052414062), and
  reads a code block so: a fence of three or more backticks or tildes, after at most
  three spaces, opens it, and a like fence closes it, or it runs to the end of the
  comment. It does not see an HTML block or HTML comment, or a fence after a list marker
  or a quote mark. In an old round, it does not see a `Hot path:` line, or a
  `Reviewers:` line of a round that does not parse, with four or more spaces of indent
  or a tab in its indent, where GitHub shows the line as text: for example, a line
  that continues a paragraph, or a paragraph in a list item or a footnote. Such a
  `Hot path:` line does not ask for `performance`. On 2026-10-08, the 58 old rounds of
  the 12 open PRs that had one (#1245, #1487, #1554, #1561, #1600, #1626, #1636, #1643,
  #1650, #1691, #1739, #1752) hit none of these cases. Decided by the director at
  2026-10-08T05:13:45Z
  (https://github.com/synnaxlabs/foundation/pull/1752#issuecomment-6052814147),
  2026-10-08T05:31:31Z
  (https://github.com/synnaxlabs/foundation/pull/1752#issuecomment-6053059328), and
  2026-10-08T05:43:00Z
  (https://github.com/synnaxlabs/foundation/pull/1752#issuecomment-6053208426).
  https://github.com/synnaxlabs/foundation/issues/1783 reads the comment as GitHub
  does. A round comment posted before the cutoff `CUTOFF` in
  `xtask/src/review.rs` (2026-10-08T03:00:00Z) is checked as before: an earlier
  free-form round passes, and it needs no end lines. A `Hot path:` line anywhere in its
  text that names a function still needs `performance`. Decided by the director at
  2026-10-08T02:44:00Z
  (https://github.com/synnaxlabs/foundation/pull/1752#issuecomment-6051099968).
  Supersedes the reviewers of a later round in ruling 2 of
  https://github.com/synnaxlabs/foundation/issues/1169#issuecomment-6040439732
  (2026-10-07T06:32:32Z). For this rule, an old round that parses names
  `performance` in its `Reviewers:` field, read as before. One that does not parse
  names it in any `Reviewers:` line of its text. A `Hot path:` line counts anywhere in
  its text. Each `Reviewers:` line of a round that does not parse, and each `Hot path:`
  line, has at most three spaces of indent and no tab. Decided by the director at
  2026-10-08T04:42:33Z
  (https://github.com/synnaxlabs/foundation/pull/1752#issuecomment-6052414062).
  The last round finds none and ends at the head, or at a commit that reaches the head
  through clean merges of the base (`git merge-tree`). A merge of the base is not clean
  when the base moves a path that the PR changed since their merge base, and that is not
  code, to a code path, by the rename detection of the merge. A base move of a path that
  the PR did not change stays clean. Decided by the director at 2026-10-07T17:21:14Z
  (https://github.com/synnaxlabs/foundation/issues/1496#issuecomment-6043078385). When
  the last round is a later round with `Breaker: skipped`, it fails if its range changes
  code: a `.rs` line that, trimmed, is not blank and does not start with `//` (a doctest
  line is a comment), or any `Cargo.toml` or `Cargo.lock` line. Each line of a moved
  file counts as removed and added. A merge of the base in the range counts only by its
  resolution: a conflict that `git merge-tree` finds between its parents in a `.rs`,
  `Cargo.toml`, or `Cargo.lock` file is a code change. The rest of the range is read
  from the tree that `git merge-tree` makes of its start and the newest base commit that
  its end holds, not from its start: the base's code does not count, and text that the
  range changes and the base moves into a code file does. So does a path that is not
  code, that the start changes since its merge base with that base commit, as the merge
  reads it, and that this merge moves into a code file, by the merge's own rename
  detection. A conflict in this tree in a code file is a code change, also one that
  leaves no markers, and so is an end that holds more than one newest base commit. In
  the range, a base move counts only through this tree. Decided by the director at
  2026-10-07T18:27:30Z
  (https://github.com/synnaxlabs/foundation/issues/1496#issuecomment-6044222273).
  Supersedes the range sentence of
  https://github.com/synnaxlabs/foundation/issues/1496#issuecomment-6043078385. Found
  by the director at 2026-10-07T14:50:33Z
  (https://github.com/synnaxlabs/foundation/pull/1193#issuecomment-6040535575), fixed by
  #1451. An earlier round's skip is taken as written, since a rebase can drop its range
  from the clone. An earlier round in the fixed format that does not parse fails. A
  red-team `oracle` PR also needs ``Director: approved at `<sha>` `` at the head. The
  status is `success` on `merge_group`. Decided by the director on #1169
  (https://github.com/synnaxlabs/foundation/issues/1169#issuecomment-6032179989) and in
  messages on #1193.
- **FACTORY MODELS (2026-10-06)** Opus 5.5 for every session and reviewer. Fable only on
  an issue that the person or the architect labels `model:fable`. Sonnet for
  `code-quality` and `drift`, Haiku for search. Decided by the advisor under the
  delegation. Supersedes: MODELS.
- **SMALL CHANGES (2026-10-08)** A change of under about 50 lines (a fix, a test pin, a
  doc fix, a rename, or a record) goes into the PR that its session builds in its crate
  or its file, as its own commit, never a PR of its own. A review finding with such a
  fix in a crate or a file that the PR changes is fixed in that PR. Else it is an item
  of an open issue in its crate, by preference one whose PR has had no review round. It
  goes alone only when no open issue in its crate fits, when it fixes a broken `main`,
  or when other work waits on it. A small mechanical change, and a small refactor that a
  fix needs, follow the same rule, as their own commit before the fix; a larger one
  ships alone. Each architect, red-team, and `laptop.monitor` keeps one PR open for its
  own small changes, sent to review at most once a day, or at once when other work waits
  on it (`docs/coordination.md`, "Small changes"). Of the 252 PRs that merged in the 24
  h to 2026-10-08T03:05Z, 65 changed 50 lines or fewer, and each paid the full fixed
  cost of CI, review rounds, an audit, and a queue slot (#1705: 8 lines, two review
  rounds, and an audit). The person decided to fold small fixes into open PRs, relayed
  by `laptop.monitor`
  (https://github.com/synnaxlabs/foundation/issues/462#issuecomment-6051321753,
  2026-10-08T03:05:03Z): "We should batch small optimizations/fixes into single pull
  requests. One set of test runs, one set of reviews. Less context and less
  infrastructure cost", and, on a proposed batch branch, "these batch branches could
  hold up progress on the next piece. Instead they should preferrably be folded into
  current or existing larger PRs". The rest was decided by the director at
  2026-10-08T03:23:34Z
  (https://github.com/synnaxlabs/foundation/pull/1706#issuecomment-6051516851), with the
  link of its item 7 corrected at 2026-10-08T03:31:23Z
  (https://github.com/synnaxlabs/foundation/pull/1706#issuecomment-6051596894) and its
  item 2 widened to a file at 2026-10-08T03:39:52Z
  (https://github.com/synnaxlabs/foundation/pull/1706#issuecomment-6051684071).
  Supersedes the mechanical-change and refactor sentences of `CLAUDE.md` Rule 2
  (https://github.com/synnaxlabs/foundation/blob/8f6a0596/CLAUDE.md#L200-L202).
- **COST TRIALS (2026-10-08)** Until 2026-10-09T04:00Z, `box1.builder-1`,
  `box1.builder-2`, `box1.builder-4`, and `box2.builder-7` run the `reviewer` of a
  second round that does not skip `breaker` on Sonnet. After the end time,
  `laptop.monitor` compares the groups and reports to the person, and a new decision
  keeps or removes the trial. The Sonnet audit trial waits until the trail checks that
  a script can make are in `cargo xtask review` (#1467, #1211). Until then the `audit`
  agent runs on Opus. When both issues close, a new decision starts that trial and
  sets its end and its measure. The person (2026-10-08T01:13Z): "Let's try all 3 of
  these and see what we get". The person dropped change 1, which closes a round with
  commits by `Text fixes:` (2026-10-08T01:17Z, on the decline by `laptop.director`,
  https://github.com/synnaxlabs/foundation/pull/1601#issuecomment-6050149418): "Ok
  fine". Both are recorded in https://github.com/synnaxlabs/foundation/issues/1703.
  Supersedes FACTORY MODELS for these runs.
- **SELF MERGE (2026-10-07)** No person approves a PR to a crate. The builder merges its
  own PR through the queue when the gate, the review rounds, and CI pass; agents may run
  `gh pr merge`. The person owns only `oracles/`, `.github/`, `CLAUDE.md`, and
  `.claude/`. The person: "Great, make the fucking changes and do your fucking job
  shipping software". Fuzz inputs in `oracles/fuzz/` need no approval either: they only
  add tests. The person: "Yes". Supersedes: the approvals in ENGINEERS and MERGE QUEUE.

### 1.14 Testing

- **TESTING IS BEDROCK + T1** Injection rule: every component gets clock, network, disk,
  and randomness as inputs. Layers: (1) unit and property tests on every commit; (2)
  coverage-guided fuzzing of every decoder of outside input, short per merge and
  continuous nightly, crashes kept as regression inputs; (3) deterministic simulation of
  a whole mesh, thousands of runs per merge and millions nightly; (4) unit benchmarks
  and (5) component benchmarks on the dedicated machine with a 5% check; (6) end-to-end
  performance against P1 on shared infrastructure, nightly and per release; (7) a
  protocol simulator per connector on every merge; (8) Synnax HITL runners with real NI,
  LabJack, and PLC hardware, nightly and per release. Mutation testing runs on the diff
  (C9b).
- **T2** Oracles are person-owned: simulation invariants, P1 targets and baselines,
  conformance suites, fuzz inputs (agents add, never remove). Agents write most tests.
- **BENCH BASELINES (2026-10-06)** The committed baselines and the CI bench job with
  the 5% check (P1) come with #715. Until then, a PR that touches a hot path gives its
  benchmark results, with the machine named. When a result is near 5%, the
  coordinator runs it again on a quiet Linux host; a result still over 5% needs the P1
  judgment. Once a day, the coordinator runs the hot-path benchmarks on a quiet Linux
  host against a fixed commit, which finds a slowdown that no PR expected. Patch; #715
  is the long-term fix. The person decided on 2026-10-06 ("I am ok with deferring
  #715").
- **CANONICAL LIBRARY RULE (testing part)** Protocol simulators and HITL test C-backed
  connectors. Simulation replaces any connector through `hub`.
- **R13 invariants (oracles)** The eight invariants in r13 section 9 become simulation
  invariants in `oracles/invariants/`.
- **R16-7 (2026-10-04)** `clippy.toml` bans `std::collections::HashMap`, `HashSet`, and
  `std::hash::RandomState`. Code uses `types::hash::Map` and `Set`, which have a fixed
  hasher, so a simulated run replays. Decided by the advisor under the quality
  delegation. The fixed hasher is the Fx hasher of `rustc-hash` (`FxBuildHasher`), not
  SipHash with fixed keys: SipHash cost the `transport` write 8.5 ns of 131 ns per 64 B
  message (Xeon 8488C), and its public keys stop no flood. Iteration order never decides
  behavior, so a test that breaks on the new order shows a defect in the code. Decided
  by `laptop.architect` (2026-10-07T09:59:29Z):
  https://github.com/synnaxlabs/foundation/issues/1321
  A map whose keys a party outside the node picks is a `BTreeMap`, whose lookup is
  O(log n) compares for each set of keys, unless the node limits the keys that the party
  puts in the map to a small count, as `streams_max` limits the streams of one session.
  A keyed hasher, with its key from `env` randomness, comes only when a benchmark shows
  that such a `BTreeMap` is too slow. Decided by `laptop.architect`
  (2026-10-07T17:36:18Z):
  https://github.com/synnaxlabs/foundation/pull/1434#issuecomment-6043338861. It
  supersedes "A map keyed by outside input will get a keyed hasher with its key from
  `env` randomness." The first PR that gives `Interner::intern` a subset of channels
  that a party outside the node picks makes `Interner.sets` a `BTreeMap` and limits the
  key sets of outside sessions (#1513).
  "Outside input" is a value that a party outside the node chooses freely. A QUIC stream
  ID is not: a peer must use its stream IDs in order, and `streams_max` limits how many
  are open, so a set of keys that collide costs the peer many streams and a lookup in a
  map of one session at most that many compares. A map that holds the streams of many
  sessions has no such bound (#1506). Decided by `laptop.architect`
  (2026-10-07T14:37:40Z):
  https://github.com/synnaxlabs/foundation/pull/1434#issuecomment-6040288730
  `laptop.architect` limited the bound to a map of one session and filed #1506
  (2026-10-07T17:29:27Z):
  https://github.com/synnaxlabs/foundation/pull/1493#issuecomment-6043218811
  Supersedes: the bound for each map of 2026-10-07T14:37:40Z.
- **R16-8 (2026-10-04)** `thread_local!` state is banned like every other mutable
  global. `clippy.toml` denies the macro. Decided by the advisor under the quality
  delegation.
- **R16-9 (2026-10-04)** Miri and cargo-fuzz run on one pinned nightly toolchain that
  only those gates use. The workspace toolchain stays stable. Decided by the advisor
  under the quality delegation.
- **R16-10 (#340)** One exception to one path per item (r16 2): `hub` re-exports each
  item of another layer-2 crate that its public surface names, at the same path under
  a module named for that crate (`hub::home::Error` for `home::Error`). Layer 3 names
  a layer-2 item only through `hub` (X44), so it has no other path. Only `hub`
  re-exports, and only items its own signatures use. Layer 2 and `node` name the item
  at its home. Copies of the types lost: each change in `home` needs a change in
  `hub`. Decided by the architect (#340).
- **ENV SEAMS (2026-10-04)** Each `env` seam is a concrete handle over a small driver
  trait that only `os` and `sim` implement. `clock::Clock`: monotonic time as
  `types::time::Monotonic`, and a `Sleep` future that resets without an allocation.
  `wall::Wall`: the OS wall clock, which only `clock` reads (a lint).
  `entropy::Entropy`: random bytes from the OS, or from the run's seed in simulation.
  `rng::Rng` is concrete (xoshiro256++ seeded from `Entropy`), so simulation replays it.
  `shards::Shards`, held only by `node`: the core count, and one thread per shard with
  its own executor. A shard's core is an index below the count, never an OS CPU
  number. `Shards::start` panics past the count: only `node` picks cores, so a bad
  index is a bug. `os` maps index `i` to the `i`-th CPU of its affinity set, which it
  reads once, so the count never changes (#116).
  `tasks::Tasks`: spawns `!Send` tasks on the current shard.
  `threads::Threads`: dedicated threads for blocking code. Each runs one future, and it
  waits for an event only by awaiting a future, so simulation controls every wait. A
  lint denies the std blocking waits (`park`, `Condvar`, `Barrier`, `mpsc` receive).
  `thread::Handle` and `thread::Error`, which `Shards` and `Threads` both return (#129).
  A `thread::Error` comes from a start, and a `thread::Panicked` from a join (#153).
  When a shard's main future completes, the shard drops its other tasks. A panic in any
  task ends its shard, and its `Handle::join` returns `thread::Panicked`. A dropped
  `Handle` would leave its thread running, so it is `#[must_use]`. On `os`, a shard is a
  Tokio `LocalRuntime` and `spawn_local` runs `Tasks`; on `sim`, the deterministic
  scheduler runs them. No other crate calls Tokio's timers or spawn. `env::files`
  (#37) gives files under one data directory, with owned blocks and a sync that
  poisons the file on failure (S4). One handle at a time holds a file open to write,
  until it drops and its calls end; another write open fails with `Busy` (#392).
  `File::close` ends after the calls of its handle end; a drop closes without a wait
  (#516). Each `os` platform picks its own mechanism (#121). `env::net` (#44) gives UDP
  sockets that move GSO and GRO batches with ECN and the local address, TCP streams, and
  listeners. `env::serial` (#431) gives serial ports that move bytes at the line rate,
  with 8 data bits, a parity, and stop bits. Framing belongs to the protocol: a USB
  adapter hides the gap between frames, so a seam that split frames would act
  differently on `os` and `sim`. A socket, listener, or port may move to another thread
  before its first poll. The first poll binds it to its thread, and a poll on another
  thread panics. Amended (2026-10-07, #995): `env::net` also gives name lookups.
  `Net::resolve` gives an IP literal, also an IPv6 address in brackets, with no
  lookup, and keeps no cache. `NotFound` is final; `Io` is a failed lookup that a
  retry may fix, and a caller matches the variant, not the code. On `os`,
  `getaddrinfo` maps `EAI_NONAME` and `EAI_NODATA` to `NotFound`, `EAI_SYSTEM` to
  `Io` with `errno`, `EAI_AGAIN` to `Io` with `EAGAIN`, `EAI_MEMORY` to `Io` with
  `ENOMEM`, and each other code to `Io` with `EIO` (#1095). Decided by the
  architect, #995
  (https://github.com/synnaxlabs/foundation/issues/995#issuecomment-6030922608).
  From the review of #1018: the bracketed IPv6 literal, and what `NotFound` and `Io`
  mean to a caller. Amended (2026-10-07, #1117): `Mode::Create` makes a missing file
  with `len` zeroed bytes. It treats an empty file that is there as missing and
  allocates it, because a crash between the create and the allocation leaves one. It
  opens any other file that is there as it is. A create that gives `Full` leaves no
  file at the path and keeps no blocks. Another error can leave an empty file at the
  path, as a crash can. `os` and `sim` both do this. Lost: an atomic create through a
  temporary name and a rename, so that the path never shows an empty file; the
  temporary file would show in `list` and need a sweep after a crash. Decided by the
  architect, #1117
  (https://github.com/synnaxlabs/foundation/issues/1117#issuecomment-6031488357).
  Amended (2026-10-07, #1112): on `os`, a write open can lock a new empty file before
  its create does. The create gives `Busy`, the empty file stays, and the next create
  allocates it. A caller that opens with `Create` only never meets it. Lost: Linux
  `O_TMPFILE` with `linkat`; macOS has no equivalent, so the two platforms would
  differ in this rule. Decided by the architect, #1112
  (https://github.com/synnaxlabs/foundation/pull/1112#issuecomment-6031672142). The
  text of the failure rule: the architect, #1117
  (https://github.com/synnaxlabs/foundation/issues/1117#issuecomment-6031721563).
  Text of the failure rule amended by the architect, #1112
  (https://github.com/synnaxlabs/foundation/pull/1112#issuecomment-6032450864): only
  `Full` promises no file; a flock, stat, or name check error after `openat` can leave
  the empty file that the create made. Lost: a promise that any failed create leaves no
  file it made.
  Amended (2026-10-07, #1310): a call of `Files` whose future drops can still run. A
  remove left so removes what the path names when it ends. Count the room of a
  removed file as used until `sync_dir` on its directory ends, and while a handle holds
  the file (#1301). Decided by `laptop.architect-2`, #1310, 2026-10-07T14:55:45Z
  (https://github.com/synnaxlabs/foundation/issues/1310#issuecomment-6040635245).
  Supersedes
  https://github.com/synnaxlabs/foundation/issues/1310#issuecomment-6035200491. The
  sentence on a dropped call: `laptop.architect-2`, 2026-10-07T16:23:52Z
  (https://github.com/synnaxlabs/foundation/issues/1310#issuecomment-6042117395).
  Supersedes the drop sentence of
  https://github.com/synnaxlabs/foundation/issues/1310#issuecomment-6040635245. Lost:
  "a drop does not stop the remove", which `os` breaks when its I/O queue is full.
  Amended (2026-10-07, #1264): a crash before a `Mode::Create` open ends can leave the
  file that it makes with no bytes, and `Create` makes a file with no bytes `len` zeroed
  bytes. `sim` makes that file at a crash. Lost: an atomic create in `os` through a
  temporary name and a rename; it leaves a temporary file after a crash, which needs a
  sweep, and a caller already learns from its own header whether a file holds data.
  Decided by `laptop.architect-2` (2026-10-07T18:31:29Z):
  https://github.com/synnaxlabs/foundation/issues/1264#issuecomment-6044288692.
  Amended (2026-10-07, #1604): `File::remove(self)` removes the file of a write handle,
  then closes the handle as `File::close`. It removes the file of the handle, by device
  and inode with no follow of a link, as FILE RENAME does: `NotFound { path }` when the
  path no longer names it, and nothing is removed. Until the remove ends, also after a
  drop of its future, a write open of the path gives `Busy`; on `os` the descriptor
  closes after the unlink, so the lock holds across processes until then. The race
  sentence of FILE RENAME holds for it too. A drop of the future can stop the remove
  before it starts, as for `Files::remove`; the file then stays, and the handle closes.
  The removal is not durable until `sync_dir` on its directory ends. A poisoned handle
  gives `Poisoned` and closes: a dropped rename can still move the file, so the path of
  the handle may be stale. A read handle panics. The caller is `mesh::log` (#1314),
  which removes a file with no record and later makes one at its path (MESH LOG). Lost:
  a spare name in `mesh` only, which adds a second kind of file to the directory of a
  log, a sweep of it in `Log::open`, and a change to the `Stray` rule of MESH LOG.
  Supersedes the "Lost: `File::remove`" sentence of
  https://github.com/synnaxlabs/foundation/issues/1310#issuecomment-6040635245. Decided
  by `laptop.architect-2`, #1604, 2026-10-07T20:24:12Z
  (https://github.com/synnaxlabs/foundation/issues/1604#issuecomment-6046168932). The
  caller sentence: `laptop.architect`, 2026-10-08T02:36:07Z
  (https://github.com/synnaxlabs/foundation/pull/1745#issuecomment-6051016964). The
  sentence that a drop can stop the remove: `laptop.architect-2`, 2026-10-08T02:32:41Z
  (https://github.com/synnaxlabs/foundation/pull/1745#issuecomment-6050977855). The
  `Poisoned` sentence: `laptop.architect-2`, 2026-10-08T02:41:37Z
  (https://github.com/synnaxlabs/foundation/pull/1745#issuecomment-6051075955).
- **SHARD PIN (#718, 2026-10-05)** `Shards::pinnable()` says whether a shard can pin
  to a core: `true` on Linux, `false` on other OSes, and `true` in `sim` unless the
  node config says `unpinnable`. `node` sets no core when it is `false`, and logs that
  once at start. `Shards::start` panics on a core then, as on a core past the count:
  the answer never changes, so a core there is a bug in `node`. `Error::Pin` means
  only a real fault, such as a CPU that went offline after the read, and carries the
  cause as a `reason`. This is the advisor's choice A, narrowed from the set of cores
  that can pin to a bool: the index map of ENV SEAMS makes that set always
  `0..cores()` or empty. Lost: `Error::Pin` for a core on a node that cannot pin, which
  gives two contracts for the same kind of bug, and a caller tells the bug from a
  fault only by its `reason` text; each driver checks the core itself, which puts one
  rule in each driver. Windows pinning waits for the person (#477). Amends ENV SEAMS.
- **SIM NETWORK (2026-10-04)** `sim` replaces only the network, not the transport.
  The production carriers (QUIC through `noq-proto`, TLS over TCP, relays) run
  unchanged under simulation, which is why r5 rejected iroh. The network seam lives
  in `env` (`env::net`): `os` implements real sockets, and `sim` implements the
  simulated network with loss, delay, reorder, duplication, and partitions. `sim` does
  not depend on `transport`. `transport` owns the carriers and the session model, and
  its `Transport` trait is private. `Clock::epoch` gives the `Instant` at
  `Monotonic(0)` for libraries that take a std `Instant`. Decided by the design
  session under the architecture delegation. `Node::fail_udp` makes a UDP socket fail
  as when the OS breaks it, until the socket drops: each receive gives `EIO`, the
  datagrams that arrive at it are lost, and a send still works. Approved by the
  coordinator on #907. Built by `simulation` in #926. Amended (2026-10-06, #943):
  `link::Config::rate` limits a link to that many bytes per second, counted as IP
  packets with their IP and UDP or TCP headers. Each direction of a link sends one
  packet at a time: a packet starts when it is sent or when the packet before it has
  left, whichever is later, and leaves after its bytes at the rate. Packets sent back
  to back at one rate leave at the rate of their total bytes, so the rounding of each
  to a nanosecond does not add up. A packet sent after the rate is removed still waits
  for the packets before it. Then it takes the delay and the jitter. A power cut drops
  the packets of the node that wait to leave. With no rate, a link adds no events and
  no draws, so the digest of a run does not change. A UDP datagram takes its length
  plus 768 bytes of its socket's send buffer until it leaves its link or a power cut
  drops it. As on Linux, a send goes whole while the send buffer is empty or takes
  less than `send_buffer_bytes`, and is pending from then. When a datagram leaves and
  the send buffer is no longer full, each send that waits wakes in the same step.
  Approved by the coordinator. Amended (2026-10-07, #995): `Net::resolve` gives the
  addresses of a host name. An IP literal gives its one address with no lookup, and
  no lookup is cached. `Sim::name` sets the answer to each lookup of a name in the
  run, on any node: its addresses in order, none (`NotFound`), or a failure (`Io`
  with `EAGAIN`), after a delay on the clock of the node. A lookup reads the answer
  at its first poll and sends no packet, so a partition does not stop it. Decided by
  the architect, #995
  (https://github.com/synnaxlabs/foundation/issues/995#issuecomment-6030922608).
  From the review of #1018: a name matches in any ASCII case and with or without one
  final dot, as in DNS. An IP literal as a name panics, because no lookup reads it.
  A lookup that would end past the end of the clock never answers. The match in any
  case and with a final dot was confirmed by the architect on #1018, in place of its
  earlier exact match
  (https://github.com/synnaxlabs/foundation/pull/1018#issuecomment-6031438649). Amended
  (2026-10-07, #1255): a receive of a failed UDP socket first gives the datagrams queued
  before the fault, then `EIO`. A broken socket still holds its receive queue, so the
  queue stays readable. A pulled serial adapter takes its buffer with it, so
  `Node::fail_serial` loses its unread bytes. Decided by `laptop.architect-2`, #1255
  (https://github.com/synnaxlabs/foundation/issues/1255#issuecomment-6033324472).
  Amended (2026-10-07, #1473): `Node::fail_listener` makes a TCP listener fail until
  it drops: each accept gives the streams already in its backlog, then `EIO`. A
  connect after the fault is refused, and the streams it accepted still work. Decided
  by `laptop.architect-2` at 2026-10-07T17:07:34Z
  (https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042821949).
  Amended (2026-10-07, #1532): a connect is refused when its SYN arrives after the
  fault. A connect whose SYN the listener took, but not its ACK, before the fault
  ends `Ok`, and its stream is reset when the RST of the fault arrives, so a write
  before it is taken. An accept error of `env` leaves the listener usable, unless the
  listener is broken: then each later accept fails too. Decided by
  `laptop.architect-2` at 2026-10-07T18:01:16Z
  (https://github.com/synnaxlabs/foundation/issues/1532#issuecomment-6043781710),
  with the connect sentences at 2026-10-07T18:05:20Z
  (https://github.com/synnaxlabs/foundation/issues/1532#issuecomment-6043854311).
  Supersedes the connect sentence of
  https://github.com/synnaxlabs/foundation/pull/1473#issuecomment-6042821949.
- **SECTOR (2026-10-05)** `env::files::SECTOR` (512) is the length of the sector that
  a crash keeps or loses whole in a write that is not yet durable. It is a constant,
  so that a store format asserts against it when it compiles. A length read from the
  device at run time lost (#569).
- **FILE RENAME (2026-10-07)** `File::rename(&mut self, to: &Path)` moves an open file
  to `to`, in the same directory, with no replace. It first syncs the file, so that no
  crash leaves the new name with bytes that were not durable (#1441). After `Ok`, the
  handle names `to` in its errors, and a write open of `to` is `Busy` until the handle
  closes. It renames the file that the handle opened, not whatever path now holds its
  old name: when the old path is gone or holds another file (a remove and a create
  since the open), it gives `NotFound { path: old }` and changes nothing. When `to` is
  there, it gives `Exists { path: to }` and changes nothing; the handle stays usable. A
  read handle, or a `to` that is not a name in the directory of the file (another
  directory, empty, `.`, or ending in `/` or `/.`), is a defect and panics. A trailing
  slash gives `ENOTDIR` on Linux and `ENOENT` on macOS, so no error can name it the
  same way on both. It poisons the file when its sync fails or when it is dropped
  before it ends, as any other call. The rename can still end after the drop, and
  then the file is at `to`. `os` checks that the old path still names the file by
  device and inode, with no follow of a link, then renames with `RENAME_NOREPLACE`;
  the I/O thread of a shard runs its calls in order, one shard writes each name
  (SHARD BUFFERS), and one node uses a data directory (DATA DIRECTORY LOCK), so
  nothing in Foundation changes the path between the check and the rename. Lost:
  `Files::rename(from, to)` on paths, which cannot tell the file of the handle from a
  new file at its path; a link then an unlink, which leaves two names at a crash; a
  replacing rename or a `replace: bool`, which no caller wants and which hides a
  defect that `Exists` reports; and a bare-name `rename(&mut self, name: &OsStr)`: an
  `OsStr` can hold a `/`, so it needs the same check, and it would be the one call
  that takes a name in place of a path in the data directory (#1449, decided by
  `laptop.architect-2`, 2026-10-07 14:55 UTC:
  https://github.com/synnaxlabs/foundation/issues/1449#issuecomment-6040629508; the
  panic list and the bare-name reason, 2026-10-07 17:35 UTC:
  https://github.com/synnaxlabs/foundation/pull/1503#issuecomment-6043326214). Also
  lost: a rename that also syncs its directory: it would be the one directory change
  that is durable when it ends, several changes could no longer share one
  `sync_dir`, and a failed directory sync would be a third result, a rename that took
  effect and is not durable (#1503, decided by `laptop.architect-2`, 2026-10-07
  19:11 UTC: https://github.com/synnaxlabs/foundation/pull/1503#issuecomment-6044972392;
  the race sentence, 2026-10-07 19:12 UTC:
  https://github.com/synnaxlabs/foundation/pull/1503#issuecomment-6044987221).
- **SIM CRASH (2026-10-05)** `Sim::crash(&node, Crash)` ends each thread of a node
  between runs; a test restarts the node with new threads on the same disk. A `Process`
  crash keeps each file call that ended, and ends each call in flight at the crash, so a
  restart finds no file held (#392), not even by a leaked handle (#535). The blocks of
  each file call of the node go back to their pools, those of a leaked call too (#763).
  A crash of either kind closes each serial port of the node, a leaked one too, and a
  socket or serial port from before the crash panics when it polls. A `Power` crash
  keeps, for each 512-byte sector, its durable bytes or the bytes of any one write since
  then, a write in flight too. A `sync` makes durable the writes that ended before it
  started. A failed `sync` makes each sector keep its durable bytes or those of one such
  write, at random. Where writes in flight at once overlap, a power cut or a failed
  `sync` can keep a part of one of them in a sector (#580). A `sync_dir` makes durable
  the entries at its end. A removed file takes space until the removal is durable. The
  monotonic clock starts again and the wall runs on. `join` on a thread that a crash
  ended panics, because no process joins its own threads after it dies. Built by
  `simulation` in #114, #535, #580, and #763. Amended (2026-10-06, #876): a failed
  `sync` covers each write up to the last one that ended before it started, in the
  order of the writes. As on Linux, these writes stay in the cache, clean: a read sees
  them, and a later write goes over them. A power cut drops them, and at each read or
  write of their sector the cache may drop them, by a coin. A sector with a write that
  no `sync` covered is dirty, and the cache keeps it. Amended (2026-10-07, #1449): a
  `Power` crash keeps the durable entries of each directory, as for a create or a
  remove, so it undoes each rename since the last `sync_dir` of the directory, a rename
  in flight too. A `Process` crash applies a rename in flight, as for other calls.
  Decided by `laptop.architect-2`, #1449, 2026-10-07 14:55 UTC:
  https://github.com/synnaxlabs/foundation/issues/1449#issuecomment-6040629508; the
  text, 2026-10-07 17:35 UTC:
  https://github.com/synnaxlabs/foundation/pull/1503#issuecomment-6043326214.
  Amended (2026-10-07, #1264): a `Mode::Create` open in flight at a crash that makes a
  file draws its state. After a `Process` crash the file is whole or has no bytes. After
  a `Power` crash there is no file, or the file with no bytes or whole, with the
  entries of its directory durable, as when the file system commits its journal by
  itself or the `fsync` of the open commits it. The commit acts as a `sync_dir` of the
  directory, so it also keeps each earlier change there, a rename too, and the digest
  holds the drawn state. Lost: a create in two calls, one that makes the entry and one
  that allocates; it doubles the calls of each create, changes the stream of each run,
  and adds a step that `env` does not have.
  Decided by `laptop.architect-2` (2026-10-07T18:31:29Z, the entries of the
  directory at 2026-10-07T19:22:31Z):
  https://github.com/synnaxlabs/foundation/issues/1264#issuecomment-6044288692 and
  https://github.com/synnaxlabs/foundation/pull/1553#issuecomment-6045160531.
  Amended (2026-10-07, #1551): the disk keeps one log, in the order that the calls
  ended, of the creates, removes, and renames that no `sync_dir` of their directory
  covered. A rename is one change. A `Power` crash keeps a prefix of the log. It draws
  the prefix from the files stream only when the log is not empty, and the digest holds
  its length. Each file call in flight takes effect as for `Process`, and the prefix
  decides whether its change stays, except a `sync` or `sync_dir` in flight, which has
  no effect. A `sync_dir` makes durable only the changes of its directory. A journaled
  file system can commit more; `sim` does not, so a missing `sync_dir` shows. A file
  takes space while an entry, a durable entry, a change in the log, or a handle names
  it. Supersedes
  https://github.com/synnaxlabs/foundation/issues/1449#issuecomment-6040629508: a
  `Power` crash undoes each rename since the last `sync_dir`, a rename in flight too.
  Supersedes
  https://github.com/synnaxlabs/foundation/issues/1264#issuecomment-6044288692: the
  commit of a create in flight. Supersedes
  https://github.com/synnaxlabs/foundation/pull/1553#issuecomment-6045160531: a commit
  makes the entries of the directory durable. A create that a `Power` crash cuts is
  whole or has no bytes, and the prefix decides whether its entry stays. A cut gives a
  state that a journaled file system can reach, or a state that only a missing
  `sync_dir` reaches. Lost: a log for each directory, which gives states that need no
  missing `sync_dir`.
  Decided by `laptop.architect-2` (2026-10-07T19:20:28Z):
  https://github.com/synnaxlabs/foundation/issues/1551#issuecomment-6045125302; the
  calls in flight, 2026-10-08T02:39:46Z:
  https://github.com/synnaxlabs/foundation/pull/1743#issuecomment-6051056642; the text,
  2026-10-08T02:27:20Z:
  https://github.com/synnaxlabs/foundation/pull/1743#issuecomment-6050920514; the order
  that the calls ended, 2026-10-08T02:43:34Z:
  https://github.com/synnaxlabs/foundation/pull/1743#issuecomment-6051095403.
- **SIM SERIAL (2026-10-05)** `Sim::line` joins two node ports with a serial line.
  Bytes go at the sender's `Settings::rate`, and an end with other settings gets
  random bytes. Each line draws its faults (loss, a flipped bit) and its random bytes
  from its own stream as each byte is sent, so a change of the line acts only on the
  bytes sent after it. A flip with parity on is lost. Each port holds 4 KiB to send
  and 4 KiB to read, as a Linux TTY does. An open ends at once. `Node::fail_serial`
  makes a port fail as a pulled USB adapter does: each read and write gives `EIO`
  until the port drops. Built by `simulation` in #431 and #690.
- **SIM PANICS (2026-10-05)** A panic in a poll or in the drop of a future ends the
  thread and the run with `Error::Panicked`, and the thread's other futures drop. Each
  future drops in its own `catch_unwind`, so a second panic never aborts the process.
  The error gives every panic, the first one first: a drop that panics is a defect of
  its own, even when an earlier panic caused the drop. A panic in the drop of a panic
  payload is one more panic. At most 16 payloads of one chain drop, and the payload
  past them is forgotten, so that a drop that always panics cannot hang the run. `os`
  drops panic payloads with the same bound. `Sim::crash` panics with the same
  messages after the crash ends. At a crash, the start of each thread that has not
  run drops the same way, after the futures. A thread that a drop starts on the
  crashing node ends in the crash and never runs. Built by `simulation` in #548, #666,
  and #870.
- **SIM TCP (2026-10-05)** `sim` models TCP segments on the same links as UDP. A segment
  is never lost or duplicated. It arrives after the delay and a jitter draw of its link,
  and never before an earlier segment in its direction, so each direction keeps its
  order. Each segment carries the key of its stream, and only the end of that stream
  takes it, so a late segment of an older stream on the same pair meets a closed port. A
  connect is ready after one round trip and its accept after one and a half. The receive
  buffer sets the window, the send buffer holds the bytes that the peer has not
  received, and a write waits while `unsent_bytes_max` bytes are not sent. A drop before
  close, or with bytes unread, sends an RST; a drop after close sends the bytes and the
  FIN. A stream is done when an RST arrived, or each FIN arrived and its own is acked. A
  drop of it sends nothing. An end that is done leaves its pair, as a Linux socket
  leaves its table: a segment to the pair then meets a closed port, a SYN opens a new
  stream, and a connect may take its port, also while a driver holds the old end. A
  process crash drops each stream. A power cut sends nothing, so the peer gets an RST
  only when it sends. A case that `sim` does not model panics with "sim does not
  simulate ... yet": a link with loss, `delayed` sends, a connect to an address with no
  node, a full backlog, and a SYN to a live stream. Rejected: retransmission over a
  lossy link (a full TCP state machine to test before a carrier needs it), and a pipe of
  bytes with no segments (no window, so no test of a writer that a slow reader stops).
  Built by `simulation` in #113 and #944. Amended (2026-10-06, #874): a stream that is
  done sends no RST at its drop and leaves its pair.
- **SIM DROP (2026-10-06)** The drop of a `Sim` drops each live future in its own
  `catch_unwind`. If any panicked, it then panics once with every message, the first
  one first, but only when the thread is not already panicking. This is the one
  exception to the rust.md rule "`Drop` never panics": to print the messages and not
  fail would hide a defect. The person said: "An exception for the simualtor is fine"
  (#555).
- **BLOCK MEMORY (2026-10-04)** A `block::Pool` gets its address space through
  `block::Memory`, a small `unsafe` trait in `block`, because `block` sits below
  `env`. `os` implements it over `mmap` (reserve, commit, purge); `block::Heap`
  implements it over `std::alloc` for tests, Miri, and `sim`. `block` makes no OS
  call. `reclaim` takes back returned blocks on each loop turn; `purge` gives idle
  pages back on a timer that the shard owns (#2). The first 64 bytes of a `Memory`
  are usable from the start: they hold the pool's header, so `Pool::new` makes no
  commit that can fail. A purged page stops counting against the memory the system
  can commit. On Linux with strict overcommit, `madvise` and `mprotect` keep that
  charge, so `os` purges with a `MAP_FIXED` remap (#475). `os::memory::Memory`
  reserves `PROT_NONE` pages, which take no charge, and commits with `mprotect`;
  `ENOMEM` gives `Refused`, and a refused commit can leave part of its range
  committed and charged until a purge or the drop. A failed purge remap panics, and
  the drop then leaks the reserve: on Linux the remap can leave a hole that another
  mapping fills, and an unmap would remove that mapping. `os::memory` builds on Linux
  and macOS only; Windows waits for #477, and `node` adds no cfg for it. On Linux each
  reserved or purged page has no huge pages (`MADV_NOHUGEPAGE`): the first touch of
  a huge page takes 2 MiB, and a purge of part of one gives memory back only later.
  A read and write `MAP_NORESERVE` reserve with a commit that does nothing lost: strict
  overcommit and Windows charge it in full, and it never refuses (#66). The person
  approved `unsafe` in `os::memory`, checked by tests on the real OS and not by Miri, on
  2026-10-05 ("Yeah taht's fine"), #461. `block::testing::{Scarce, Switch}`, behind
  the `sim` feature, is heap memory whose commits a test makes refuse, so a crate
  above `block` tests a refused commit through its production path (#591).
  `Pool::heap(config)` makes a pool on a `Heap` of `Config::reservation` bytes, so a
  caller that wants heap memory does not size it. `Pool::new` stays for injected
  memory, such as `os::memory::Memory` in `node` and `testing::Scarce` in a test
  (decided by the architect, 2026-10-07T08:55:57Z, #1294:
  https://github.com/synnaxlabs/foundation/issues/1294#issuecomment-6034509040).
- **SHARD POOLS (2026-10-06)** `Node::start` makes one `block::Pool` for each shard
  and moves it into the shard, which drops it (M4). Each of `n` shards gets
  `budget / n`, and shard 0 also gets the remainder, so the parts add up to the node's
  budget (MEMORY BOUNDS). The memory comes from `node::Config::memory`, a closure that
  `node` calls once per shard, in order of core: production passes
  `os::memory::Memory::new`, and `sim` tests pass `block::Heap`. A shard with no memory
  is a start failure: later shards do not start, the node stops, and `join` gives
  `Error::Memory` with the core and the `os::memory::Error`. Lost: making the pool on
  the shard's thread, which needs a second path for the error and a `Send + Sync`
  seam. The purge timer and `reclaim` on each loop turn land with the first PR that
  allocates from a pool, since no test can see either before then (#410). Proposed
  by `ops` in #410; approved by the coordinator on #806.
- **SHARD BUFFERS (2026-10-07)** Each shard opens its write-ahead ring in directory
  `shard-<i>` of the node's data directory and keeps it until the node stops.
  `node::Config::files` is a maker with no core that `node` calls on the start
  thread, in order of core, for each shard that gets its memory, just before its
  start; the shard runs the function it gives on its own thread, because `Files` is
  `Rc`, and a shard that does not start drops it unrun. A caller on the real OS makes
  each shard's disk with `os::files` before the start and joins its I/O thread after
  `join`. A `Fn` that each shard calls on its own thread lost: it fits `os::files` only
  with a lock around a queue of disks. A fallible maker like `memory`, with a `node`
  error for it, lost: `node` would then own I/O thread handles, which `sim` does not
  have. Decided by the architect on #1062 (#1173):
  https://github.com/synnaxlabs/foundation/pull/1062#issuecomment-6032037030.
  `node` alone names `shard-<i>`. `node::Config::entropy` gives the shards
  randomness. A ring that does not open stops the node, and `join` gives
  `Error::Buffer` with the core, after `Start` and `Memory` and before `Panicked`. A
  data directory made for another shard count, more or fewer, is refused before any
  buffer opens (#1076); a reshard at start is the long-term path (#1077). Running the
  stored count on another core count lost: it bends C2. Decided by the architect on
  #1062:
  https://github.com/synnaxlabs/foundation/pull/1062#issuecomment-6030791343. The
  count is an empty directory `shards-<n>` in the data directory, made and synced
  before `shard-0`, so a crash leaves it whole or absent. Shard 0 claims it at the
  head of the interner handoff. Another count gives `Error::Shards`, and a failed
  file call `Error::Directory`. Any record of another count fails the start, also
  next to `shards-<cores>`, and `stored` is the smallest such count, so the error
  does not hang on the order of the list. A name counts only when the rest after
  `shard-` or `shards-` is plain decimal that fits a `usize`: above zero for a
  record, and below `usize::MAX` for a ring. Any other name (`shards-03`,
  `shards-+3`, `shards-0`, `shard-<usize::MAX>`) is one the claim does not know, and
  it ignores it, because no core count makes the node write it. A count that no host
  has, such as `shards-<usize::MAX>`, is still a record, and the claim refuses the
  start (decided by `laptop.architect-2`, 2026-10-07T08:16:03Z, #1214:
  https://github.com/synnaxlabs/foundation/issues/1214#issuecomment-6033877711, with
  "node" for "claim" at 2026-10-07T16:47:47Z:
  https://github.com/synnaxlabs/foundation/issues/1214#issuecomment-6042568803).
  Supersedes the reason in
  https://github.com/synnaxlabs/foundation/issues/1214#issuecomment-6032550887. The
  claim reads names only, so a file with such a name counts as a directory would.
  Decided by the architect, #1214:
  https://github.com/synnaxlabs/foundation/issues/1214#issuecomment-6032550887, as on
  #1110 for `shards-0` and `shard-<usize::MAX>`:
  https://github.com/synnaxlabs/foundation/pull/1110#issuecomment-6032339719. With
  no record, rings up to `shard-<k>` are a record of `k + 1`, so a data directory
  whose record a copy dropped is checked too; a crash cannot leave a ring with no
  record. Each start syncs the data directory before `shard-0`, also when the record
  is there, because a process crash can leave it unsynced. A one-sector file lost: it
  needs a block, a write, two syncs, and a decode. Decided by the architect, #1076:
  https://github.com/synnaxlabs/foundation/issues/1076#issuecomment-6031257049.
  The rule of rings with no record stays, decided by the architect on #1178:
  https://github.com/synnaxlabs/foundation/issues/1178#issuecomment-6032340796, with
  the reasons at
  https://github.com/synnaxlabs/foundation/pull/1110#issuecomment-6032339719. After a
  stop, a shard starts no disk step: shard 0 checks the stop before the claim, and
  each shard before its open. A started step runs to its end. A skipped step drops its
  handoff, so each later shard skips too. A stop is not a failure, so `join` gives
  `Ok` when no shard failed. Any failure stops the node, so a claim or open that has
  not started does not start; `join` gives `Start` or `Memory`, else `Shards` or
  `Directory`, else `Buffer` by core, else `Panicked` by core. Decided by the
  architect on #1062 (#1174):
  https://github.com/synnaxlabs/foundation/pull/1062#issuecomment-6032037030. One
  shard writes each name in the data directory: shard `i` writes `shard-<i>` and each
  name in it, and shard 0 also writes `lock`, `shards-<n>`, and, with a region, `mesh`
  and each name in it (#585, by `laptop.architect`, 2026-10-08 03:37 UTC:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051658475). A change
  that gives a name a second writer first changes the check of FILE RENAME, which relies
  on this (#1503, decided by `laptop.architect-2`, 2026-10-07 19:12 UTC:
  https://github.com/synnaxlabs/foundation/pull/1503#issuecomment-6044987221).
- **DATA DIRECTORY LOCK (2026-10-07)** One node at a time uses a data directory. Before
  the claim reads a name, shard 0 opens the file `lock` in the data directory to write
  (`Mode::Create { len: 0 }`), and drops it after each shard of the node has closed its
  ring and each task of the mesh has ended (#585, by `laptop.architect`, 2026-10-08
  03:37 UTC:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051658475). `Busy`
  on `lock` stops the start with `Error::Directory`, before any name is read. The node
  never removes `lock`, so an open cannot race with a remove. A crash frees the lock
  (`env::files`, #392). Lost: no lock, with the `Busy` of each ring only, because two
  nodes with two core counts can each record a count, and the loser's record then
  refuses every later start. Also lost: an atomic claim with no lock, because an
  exclusive create guards one name, and two counts are two names.
  Decided by the architect, #1297:
  https://github.com/synnaxlabs/foundation/issues/1297#issuecomment-6034758419 (#1300).
- **SHARD HOMES (2026-10-07)** Each shard builds its `home::Shard` over its buffer
  once the buffer opens, with the node's `clock::Reader`, and keeps the home until the
  node stops. Its number is its core. It carries no index until the hub picks them
  (#585). The stamp limits (A5) are a patch until #1285 makes them settings: earliest
  2000-01-01T00:00:00Z, which refuses a clock that reads near 1970 but not one that
  resets to 2000-01-01, and refuses backfill from before 2000; ahead 10 s, ten times
  the MVP time error target of 1 s. A field of `node::Config` lost, because a setting
  comes from the spec (NODE SETTINGS), not from the caller of `Node::start`.
  Decided by the architect on #1287:
  https://github.com/synnaxlabs/foundation/pull/1287#issuecomment-6034425115.
- **NODE SPAWN (2026-10-07)** `Node::spawn(task)` calls `task` with the node's one hub,
  on shard 0, once each shard has opened its buffer, then runs its future. It has the
  shape and the rules of `env::tasks::Tasks::spawn`: no handle, `Output = ()`, and a
  panic ends shard 0 and fails the node (`Error::Panicked`), unless the transport
  stopped first, which gives `Error::Transport`. Shard 0 calls the tasks
  with the hub in the order of their calls, so their closure bodies run in that order;
  their futures run in no set order. A task that is given before the hub exists waits
  for it. A node that stops or fails before shard 0 calls a task drops it uncalled, and
  a stop drops each running future. A future that completes drops at once. The task runs
  on shard 0's thread, so it may hold values that are not `Send`, such as sessions.
  `node` depends on `hub`, and `Node::interner` goes away: shard 0 builds the hub with
  the interner when it comes back from the last shard. Decided by `laptop.architect-2`
  (2026-10-07T18:04:23Z):
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6043838411.
  `hub::Config` stays as it is, one interner by value for one shard, and sessions on the
  home of each shard wait for #1566. Decided by `laptop.architect`
  (2026-10-07T18:02:03Z):
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6043797070, with the
  director's OK for the deferral (2026-10-07T18:15:18Z):
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6044020168.
  The call order and no result decided by `laptop.architect-2` (2026-10-07T19:49:12Z):
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6045597072.
  Supersedes the start order of
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6043838411. When a
  caller outside the tests of `node` builds a result channel, file an `interface` issue
  for a result from `spawn`. `Error::Transport` over a panic after the transport stopped
  decided by `laptop.architect-2` (2026-10-08T03:21:42Z):
  https://github.com/synnaxlabs/foundation/pull/1769#issuecomment-6051497737.
  Supersedes the panic error of
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6043838411.
- **NODE PORT (2026-10-07)** `Node::start` binds the node's one port at `Config::listen`
  on `Config::net` before any shard starts; a failed bind starts no shard, and
  `Node::join` gives `Error::Port`. The port's one part (#77) moves to shard 0, which
  builds the transport with `Config::private_key` once the last shard has opened its
  buffer (X42), so the node takes no session before that. Its limits are patches until
  #1662 makes them settings, as LIMITS of SHARD HOMES is: window 1 MiB, 64 streams of
  each kind, idle 30 s, and messages of the smaller of 64 KiB and the pool's largest
  block. Each session runs in its own future, and each stream of it reads its header in
  its own future, so a late header delays no other stream. One exhaustive `match` on
  `wire::Protocol` in `node` routes each stream; until a protocol has a server, its arm
  stops the stream with `Code(wire::header::REJECTED)` and resets the reply half with
  the same code, as for a header that does not decode, or for a first message with bytes
  after the header. The node reads no datagram until the first protocol that takes
  datagrams has a server (#1661). `Config::private_key` is a patch until `Node::start`
  reads the key from its data directory (#1660). The node admits every peer that
  completes the handshake; the mesh checks each message of a mesh stream (NODE MESH).
  At the stop, each session and stream future drops, then the transport. The bound on
  the wait for a header is #1628.
  Decided by `laptop.architect-2` (2026-10-07 21:09 UTC):
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6046900669, on the
  plan https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6046861267;
  datagrams and the key, by `laptop.architect-2` (2026-10-07 23:26 UTC):
  https://github.com/synnaxlabs/foundation/pull/1649#issuecomment-6048898047.
  Amended (2026-10-07, #1649 round 1, a finding of `performance` that
  `laptop.integrator-1` deferred, 23:41 UTC): the window caps a session at the window
  over the round trip, about 21 MB/s at 50 ms, until #1662 sizes it from the
  bandwidth-delay product. The number of sessions has no bound until #1628.
  https://github.com/synnaxlabs/foundation/pull/1649#issuecomment-6049077609.
  Amended (2026-10-08, #1647, by `laptop.architect-2`, 00:05 UTC): a transport that
  stops with an error stops the node, and `Node::join` gives `Error::Transport`. The
  node does not rebind the port:
  https://github.com/synnaxlabs/foundation/issues/1647#issuecomment-6049354544.
  Supersedes the deferral of
  https://github.com/synnaxlabs/foundation/pull/1649#issuecomment-6048464411
  (2026-10-07 22:50 UTC), under which the node ran on with no port until #1647.
  Amended (2026-10-08, PR 3b of #585, by `laptop.architect`, 03:37 UTC): shard 0 opens
  the mesh before the first session. `route` gives each `Mesh` stream of a
  `Peer::Node` session to `Mesh::serve` with that key, never a key from a header or a
  message. A `Mesh` stream of a `Peer::Client` session stops with
  `Code(wire::header::REJECTED)`, and its reply half resets with the same code. For
  `Mesh` streams, the admission rule is the mesh's check of each message: a peer that
  is not the member it names gets `Spoofed`, and the stream stops at that message. The
  `Hub` rule comes with PR 4. At the stop, each session and stream future drops, then
  the mesh, and shard 0 waits for the mesh's task to end before it drops `lock` (DATA
  DIRECTORY LOCK) and before `Node::join` returns, so a restart at once opens the log:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051658475. This
  supersedes the stop order of
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6046900669.
  Amended (2026-10-08, #1830, by `laptop.architect-2`, 07:26 UTC): with a mesh, each
  session and stream future drops, then the mesh, and the transport drops when the
  last task of the mesh ends, before `lock` drops:
  https://github.com/synnaxlabs/foundation/pull/1830#issuecomment-6054871235.
- **NODE MESH (#585, 2026-10-08)** `Config::key` is the node's key, beside
  `Config::private_key`; both are patches until #1660 moves them to node-local disk.
  `Config::region: Option<Region>` gives the region that the node is a member of: its
  prefix, its members (one card has `Config::key`), and the voters before the first
  entry of the log. The caller gives the same region at each start: the node keeps no
  copy of it. `None` opens no mesh. The `Option` is a dark patch: the `None` stays in
  `node`, and no lower crate gets an `Option` of the mesh. PR 4 of #585, which gives the
  mesh to the hub, makes the region required, unless #1660 and #1744 have already taken
  it out of `Config`. The long-term path takes it out of `Config`: the node keeps its
  membership in its data directory when it founds or joins, and reads it at each start.
  With a region, shard 0 opens `mesh::Mesh` on the node's transport after the last shard
  has opened its buffer and before it takes the first session. Its directory is `mesh`
  in the data directory (`mesh::Config::dir`; the directory by `laptop.architect`,
  2026-10-08 03:37 UTC:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051658475; the field
  by `laptop.architect-2`, 03:54 UTC:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051833866, and by
  `laptop.architect`, 04:00 UTC:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051912643). Shard 0
  waits for `Mesh::ended` before it drops `lock` (DATA DIRECTORY LOCK). Each clone of
  the mesh, also one inside the hub, lives in a future that shard 0 drops at the stop.
  PR 4 of #585 keeps this (`laptop.architect-2`, 2026-10-08 07:26 UTC:
  https://github.com/synnaxlabs/foundation/pull/1830#issuecomment-6054871235). A mesh
  that does not open stops the node, and `Node::join` gives `Error::Mesh`, ranked with
  `Error::Buffer` and below `Error::Transport`. Each `wire::Protocol::Mesh` stream of a
  peer that proved a node key goes to `Mesh::serve`, which checks each message against
  the region; the error of `serve` ends only its stream. A mesh stream of a client, or
  of a node with no region, is rejected as NODE PORT says. Shard 0 sets no home yet (PR
  4 of #585). Shard 0 opens the mesh with no founding definitions
  (`mesh::Config::founding`). From PR 1 of #1744, it gives the root region the
  definitions that `spec::founding::create` gives, and each other region an empty map.
  Decided by `laptop.architect` at 2026-10-08T06:11:30Z
  (https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6053599101).
  `node::Region` copies three fields of `mesh::Config`. One `mesh` value of what a node
  knows of its region at open replaces it (#1859) when the first of PR 1 of #1744 and
  the join answer of #336 lands, because each needs all four fields. Decided by
  `laptop.architect` at 2026-10-08T10:23:48Z
  (https://github.com/synnaxlabs/foundation/pull/1857#issuecomment-6057800438). A mesh
  that stops does not stop the node until #1780, before PR 4 gives the mesh to the hub.
  Lost: `Node::found(region)` at run time, which needs a second open path and a node
  that runs with no region before it; the key in `Region`, because a node's identity is
  not region data, and PR 4 needs it with no region. Decided by `laptop.architect-2`
  (2026-10-08 03:37 UTC):
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051655452, on the
  plan https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6051630943.
- **BLOCK VIEW (#110)** `Block::skip(self, count)` is a view of the same buffer that
  starts `count` bytes later, with no copy and no count change. `Block` is
  `{ header, start: u32, len: u32 }`, 16 bytes, so the largest block holds 2 GiB; a
  budget above that gives more blocks, not larger ones. A pool has at most 96 size
  classes, four per doubling, so a payload is at most 64 bytes or a quarter above its
  length (#188; 26 power-of-two classes wasted up to 100%). `slice(&self, range)`
  lost: it clones the count for every view, and nothing needs a range yet. Decided
  by `memory`.
  Amended (2026-10-07, #1068): `block::footprint(len)` gives `usize::MAX` when `len`
  passes the largest payload, in place of a panic. No pool holds such a block, so
  every budget refuses it. `frame::charge` of ends from a hostile peer gives
  `u64::MAX`, and `Layout::draft` refuses it with `block::Error::TooLarge { .. }`
  (`frame::Error::Pool` at the reader) and takes no block. A reader drafts before it
  spends, so a spend adds only a charge that a pool holds, and a plain add never
  overflows. Lost: an exported largest payload with a new `Error` variant, a second
  check of a limit that `block` owns; a saturating spend, a second guard.
  Decided by the architect, #1068
  (https://github.com/synnaxlabs/foundation/issues/1068#issuecomment-6032386156,
  corrected in
  https://github.com/synnaxlabs/foundation/pull/1216#issuecomment-6032799083 and
  https://github.com/synnaxlabs/foundation/pull/1504#issuecomment-6043266054, the
  latter at 2026-10-07T17:32:11Z).
  Amended (2026-10-07, #1504): a const assertion in `block` checks the 16 bytes of a
  handle, so `block`, and each crate that depends on it, builds only where a pointer is
  8 bytes. So `frame::charge` maps no `usize::MAX` of a narrower target to `u64::MAX`,
  and no crate that depends on `block` checks the pointer width (`usize::BITS` or
  `target_pointer_width`); #1396 removes the last such check, in `os`. A 32-bit target
  first needs a new handle, and the choice of targets is the person's (CPU BASELINE);
  `charge_of` in `types` changes with that handle. Decided by `laptop.architect`
  (2026-10-07T18:30:48Z):
  https://github.com/synnaxlabs/foundation/pull/1504#issuecomment-6044276677
- **POOL COPY (#1599)** `Pool::copy(&self, bytes: &[u8]) -> Result<Block, Error>` gives
  a frozen block that holds a copy of `bytes`, with the errors of `alloc` for
  `bytes.len()`. Callers repeated `alloc`, `copy_from_slice`, and `freeze`. Lost:
  `alloc` with a closure that writes in place, because each caller already holds its
  bytes as a slice. Decided by `laptop.architect` (2026-10-07T20:43:02Z):
  https://github.com/synnaxlabs/foundation/issues/1599#issuecomment-6046480683
- **COUNTING ALLOCATOR (2026-10-04)** The person allowed one exception to "no mutable
  globals": "Allow in test binaries". A test or benchmark binary may hold one
  counting `#[global_allocator]` `static` with an atomic count, because Rust has no
  other way to count allocations. Never in a library or the `node` binary. The
  `xtask globals` check allows only this case. The static also holds the state of
  `Allocator::freed_holding` (#349): a phase with a count of the frees that scan, the
  caller's needle while a call runs, and a found count, because Rust has no other way
  to see a freed block. `freed_holding` is the one exception to "Safe code is sound
  for every input": the person said "#481 I approve A" on 2026-10-05. A freed block
  can hold bytes the program never wrote, such as padding or the spare capacity of a
  `Vec`. Rust defines no read of such a byte on any target, so no sound read exists,
  and Miri stops at one. This is a patch. The long-term fix is a freeze read (Rust RFC
  3605); when Rust has one, `freed_holding` uses it and the exception goes. A binary
  that bounds the memory of a structure holds `counting::Bytes`, one atomic count of
  the bytes it holds; a binary holds one counting allocator, `Allocator` or `Bytes`.
  `Allocator` does not keep that count: a benchmark must not pay for a count that only
  a test reads, or its baseline moves with no product change, as
  `transport/benches/send.rs` did (+4.2% to +11.2% at p50). Lost: `Allocator` keeps
  `held` (that cost in each counting binary); a `bool` at construction (a branch on
  each allocation and free, and a `held` that must panic when it is false);
  `Allocator<const HELD: bool>` (no branch, but `Allocator<true>` says nothing at the
  call site, and no caller needs both counts in one binary). Decided by
  `laptop.architect` on 2026-10-07T16:28:07Z
  (https://github.com/synnaxlabs/foundation/pull/1440#issuecomment-6042194242).
  Supersedes the `held` part of
  https://github.com/synnaxlabs/foundation/issues/1437#issuecomment-6040199190.
  A counting allocator runs code as `System` runs it, apart from its count: each
  `GlobalAlloc` method calls the `System` method of the same name, and keeps the
  trait's own body only when the allocator's contract needs it, with a comment that
  names that contract. So `Bytes::realloc` calls `System.realloc` and changes `held` by
  the difference of the two sizes in one atomic step, and `Allocator::realloc` keeps
  the trait's own body, because the scan of a free reads the old block. A count that
  no test can tell apart is not a reason to keep the trait's own body: under it, a
  `realloc` that doubles 64 B to 1 MiB took 15.4 µs, not 1.6 µs (Apple M3 Max).
  Decided by `laptop.architect` on 2026-10-07T17:59:58Z
  (https://github.com/synnaxlabs/foundation/pull/1440#issuecomment-6043758919, #1536).
  Supersedes the reason of 5d23e00e
  (https://github.com/synnaxlabs/foundation/pull/1440#issuecomment-6042860057).
- **ARM RUNNER (2026-10-04)** CI runs every test on aarch64 too, because a wake protocol
  can pass on x86 and fail on ARM (r11 4.1). The person chose "AWS runner always on" and
  said "I have tons of AWS credits". Three runners (`foundation-arm-a`, `-b`, `-c`)
  share one AWS m7g.2xlarge (8 vCPU, 32 GiB) in us-east-1 with no inbound ports, tagged
  `project=foundation-ci`, outside BENCH SPEND. One runner queued 9 runs while its host
  used about 30% CPU, so the person asked: "can we have multiple runners on a single
  machine?" ARM skips docs-only changes. The coordinator owns it. On 2026-10-05 the
  host ran at 80 to 86% CPU with 14 runs queued, so a second host, an m7g.4xlarge (16
  vCPU, 300 GB) with six runners (`foundation-arm-d` to `-i`), joined it. The person
  chose "m7g.4xlarge, 6 runners". On 2026-10-06, with 55 runs queued, a third host
  joined with three runners (`foundation-arm-j` to `-l`): one spot machine from an EC2
  Fleet over six Graviton types and six zones (launch template `foundation-arm-spot`),
  because AWS took back a single-type spot machine after 30 minutes. Limits: a spot
  price cap of 0.20 USD/h, and a hard stop on 2026-10-08 at 03:00 UTC. With it, the
  hosts and the factory host cost at most 99.73 USD a day (#15). The person said:
  "Once you are sure of costs provision and set strict limits on whatever you need
  please". With 51 runs still queued, a fourth host joined with twelve runners
  (`foundation-arm-m` to `-x`): one 32-vCPU spot machine from a fleet over eight
  Graviton types and five zones (launch template `foundation-arm-spot-32`). Limits: a
  spot price cap of 0.60 USD/h, a hard stop with the factory host on 2026-10-07 at
  07:03 UTC, and a cap of 18 USD (#15). Until that stop, the daily cap is 115 USD; then
  it is 100 USD again. The person said: "Yes thats fine".
- **LINUX CI (2026-10-05)** For the alpha, tests run only on Linux (x86-64 and ARM).
  No CI job runs on macOS or Windows. The design stays cross-OS: each C9d target must
  still be a valid build, so OS-specific code goes only in `os`. The person said: "As
  long as our systems are designed to cross compile i'm ok wiht only testing against
  linux for an alpha. as long as the system is designed for cross os deployment"
  (#574).
- **ROOT TESTS (2026-10-07)** A test that needs `sudo` (to mount a small filesystem)
  goes in its own `[[test]]` target with `test = false`, so `cargo test`, also with
  `--all-targets`, does not run it. One step of the x86 `check` job in `ci.yaml` lints
  and runs it on a GitHub-hosted runner, which is discarded after the job. No other
  host runs it: box1, box2, and the self-hosted runners keep their state, and root
  there is a security change that only the person can make. The ARM mutants job does
  not run it, so the code that only such a test pins sits in one small function that
  `.cargo/mutants.toml` excludes, with the name of the test. First user: the `root`
  target of `os` (#1100). Decided by the architect, #1100
  (https://github.com/synnaxlabs/foundation/issues/1100#issuecomment-6031260669).
- **CI PACE (2026-10-06)** The ARM pool must not hold up the agents. The ARM workflow
  runs no loom step: loom is a software model, so the x86 `loom` job gives the same
  result. A PR run is cancelled by a newer push. A run on main is never cancelled while
  it runs; of the commits that merge during it, only the newest runs next. Each runner
  keeps its build in `$HOME/target/<runner>`, outside the workspace, and deletes it
  past 25 GiB. Dependencies build at opt-level 2 in the dev profile; workspace crates
  stay at opt-level 0 with their checks. A PR tests only the changed crates and their
  reverse dependencies (`cargo xtask affected`); main tests the whole workspace.
  Decided by the advisor (#782). The person said: "We need to make the agentic
  engineering the bottleneck, not CI".

### 1.15 Releases

- **C9d** Releases follow RFC 0058: dispatch from main or `release/X.Y`; tag `vX.Y.Z` in
  this repository; `-rc.N` never counts as shipped. One binary per target (Linux x86-64
  and ARM, macOS, Windows), each tested on real hardware. `foundation upgrade <version>`
  is an ordinary operation, rolling one node at a time; nodes fetch the signed binary by
  hash from a nearby peer. Each wire and disk format has one integer version; a node
  reads its own and the previous one; new formats turn on only after every node runs the
  release. Compatibility is owed only to stable releases. Until the first stable
  release, each wire and disk format stays at version 1, and a breaking change does not
  add a version. The person decided on 2026-10-05 ("keep version 1. we should only make
  breaking changes until we release v1"), #374.
- **BQ18** The desired version is in the spec. Nodes report versions with lease
  renewals. A rollout lock upgrades one node at a time. Finalize when all report.
  Multi-region scope: 5.1.
- **CPU BASELINE (2026-10-05)** Builds assume x86-64-v2 on x86-64 (every CPU since
  2009) and the CRC instruction on aarch64 Linux (Raspberry Pi 3 and later, Graviton;
  Apple chips have it already). `.cargo/config.toml` sets both, so tests, benchmarks,
  and releases build the same code. A binary fails on an older CPU. Without the flags,
  the `crc32c` kernels ran 2x slower (#140). The person decided on 2026-10-05 ("Raise
  the minimum").
- **LOCAL PATCHES (2026-10-05)** A dependency that we patch lives in this repository as
  an unchanged copy of its release in `patches/<crate>/`, outside the workspace, with
  `[patch.crates-io]` in the root `Cargo.toml`. One PR adds the copy alone; a second
  PR makes our change on it, with its tests, so the change is reviewed here. For each
  new release that we take, the copy is replaced and our change made again. Lost: a
  fork in `synnaxlabs` patched by git URL (each build depends on a second repository,
  and the change is reviewed outside this one); for the first patch, a workaround in
  `transport` that never stops a stream (the peer sends the rest of the stream, and a
  cancel no longer reaches the sender, against STREAM WIRE). The person decided on
  2026-10-05 ("Ok I guess we need to do #2"), #620.

### 1.16 Retired entries

| Retired entry | Replaced by |
| --- | --- |
| A1 sketch: channel `home` field, epoch and seq pair, standby in the mesh file | S5, S12, A8 |
| Rule 3 of #1382 (6038235423): `mesh` removes a claim whose signer has no key at the node, and a bad signature of a known signer refuses the message | "Rule 3 becomes" (6042828979): the append is cut before the first entry with a claim of a signer with no key, and the chain too (6043037608) |
| Two-keys sentence of 6038611630: two joins of one node that name two keys give none until the apply decides | MESH DRIVER (6046503082): the joins below the first configuration entry that names the node decide, when they name one key |
| "The node never counts a wrong key" of 6046807570 | MESH DRIVER (6049888903): a voter that lies can write a join with a key it holds, and when that join is the only unapplied join of its node, the node takes that key, until #882 |
| Item 3 of rule 3 of 6042828979: a claim of a known signer with a bad signature refuses the whole message | MESH DRIVER (6045806233): a claim that does not hold under a key from a join that is not applied is removed, or cuts the append or the chain; only a bad signature of an applied member refuses |
| A1 "control is a lease" (for every holder) | S11 (optional writer setting) |
| A2 and A15 "mesh file" and placeholder commands | K1, K3 |
| A5 tie rule (ties ordered by seq) | S6 strict increase |
| A6 seq counts batches | A8 |
| A10 columnar struct series | S7 |
| A14 validity bits | S7 + QUALITY DECISIONS |
| A15 struct fingerprints | S7 |
| A18 quality side array | S13, BQ13 |
| A20 channel retention, quality codes on acks | S12, S13 |
| B1 durable reader, B2 durable and ad-hoc readers | S10 |
| r12 A.3 `pace` modes (sleep, hybrid, spin) and blocking wait | PACE |
| B3 one cumulative position per index | READER RULES |
| Retention trims a held sample: the trim clauses of #895 (6032219156), of the READER RULES floor (#89), and of HANDOFF RECORD (#402) | RETENTION, READER RULES, HANDOFF RECORD, STORE TRIM |
| `set_floor` takes `keep`, and the floor is past each sample stored more than `keep` ago: #1377 (6037946637, parts 1 and 2, and part 3 before the first estimate or while a store time of the path is at or after the cutoff; the floor sentence of 6038431739) and #1080 (6037950577) | READER RULES (the cutoff) |
| BENCH SPEND; the coordinator as the session that rents and ends the ARM RUNNER hosts | Test budget (5.5) |
| C1 and C9a crate lists | Section 4 |
| C3 REFINEMENT groups | GROUPS DROPPED |
| C4 integration contract | C3 |
| C6 fixed time source | R6 TIME LOCKED, TIME ADAPTERS |
| D6 path lock for agents | T2, C9c |
| D7 linked meshes, Raft library, bootstrap peers in the file, any node relays | K5, R4 SETTLED, BQ11a, TRANSPORT SHAPE |
| K5 voters policy with a selector, parent-owned voters | REGION BLOCK, K5 REVISION |
| r3 "one language", single-expression calculations, first-input index | K1, C5 + KINDS OWN |
| r8 Q5 actor with device hooks | BQ5 |
| r8 Q6 standby as a reader, positions as a channel | BQ6 |
| r8 Q8 lease per group | BQ8 |
| r8 Q11 `connector-status` kind | BQ11b |
| r8 Q14 group run channels and `stopped` | Commandable parameters (`running`) |
| r8 section 1.14 JSON Schema check in `config` | KINDS OWN THEIR CONFIG |
| S1 frame struct; S2 series struct, struct layout, per-channel compression, per-link re-encode | M1, M3, S7, R10, BQ4 |
| S8 node as a spec definition | BQ11a |
| S9 name-hierarchy tree, gossip hints | R4 SETTLED |
| T2 enforcement level as a parameter | C9c |
| Crate name `time` | `clock` (R9-D13) |
| HOME SPLIT placement of `control` and `delivery` in layer 2 | SRP PASS layer-1 rule (X17) |
| The old term for a region | "region" (REGION LOCKED) |
| Factory constraint (two people, attended sessions only) | ENGINEERS, TWO LANES |
| MULTI-SESSION FACTORY, NINE BUILDERS, C9b2 crew | FACTORY ROLES |
| QUALITY SESSIONS (`verify`, `audit`, `ux`), CLOUD ROUTINES | FACTORY ROLES |
| MODELS | FACTORY MODELS |
| BREAKER REVIEW | REVIEW TIERS |
| MERGE RULE, C9c "a person merges every PR" | MERGE QUEUE |
| REMOTE CONTROL, `inbox:<name>` issues | MESSAGES |
| FACTORY HOST (daily renewal by the coordinator) | AWS CEILING |
| 5.5 and STORE AND FORWARD one-hour cut (#1072) | STORE AND FORWARD amendment (2026-10-07) |
| R16-7 "a map keyed by outside input will get a keyed hasher" | R16-7 `BTreeMap` rule (2026-10-07T17:36:18Z) |
| HUB END: the task drops the commit it waits for at its first poll after the hub drops | HUB END: the commit lives in the state (#1633) |
| NODE PORT deferral of #1649 (6048464411): a transport that stops ends the routing and the node runs on with no port | NODE PORT amendment (#1647, 6049354544) |
| HOME TYPE REFUSAL (#963): `open_writer` refuses a series of a type the home does not write | HOME EVERY TYPE |
| FIRST SLICE order, for ONE NODE work only; the order "after FIRST SLICE" of 6050540089 | FIRST SLICE amendment (2026-10-08) |

---

## 2. Where things are defined

Storage classes used in the table:

- **Files**: definition files, read through the Document model.
- **Spec**: the stored spec. One prolly tree per region in `blob`; the pointer (version
  and root hash) is in that region's Raft state.
- **Region state**: runtime state agreed by one region's voters through Raft, outside
  the spec.
- **Index log**: records in an index's log in `buffer` at the home, copied by `replica`.
- **Channel**: values published as samples.
- **Memory**: in memory on one node; not durable.
- **Node-local**: on one node's disk or local config; not agreed.
- **Kind-owned**: an opaque Document inside the spec that only the kind decodes.
- **Binary**: compiled into the binary.

### 2.1 Definitions

| Concept | Defined or stored | Written by | Read by | Owner crate |
| --- | --- | --- | --- | --- |
| Channel | Files, then Spec as `spec::channel::Channel { key, kind }`, keyed by its name (architect, #756: https://github.com/synnaxlabs/foundation/issues/756#issuecomment-6031378098). Sources of channels: X33 | People or agents in files; `discover` and `export` write files; `apply` commits | Every node through its spec snapshot; `home`, `hub`; kinds through `hub.spec()` | `spec` (type, edge checks: `channel::check` over the channels keyed by name; an index's control channel is on another index, X18), `config` (calls it on the planned set, where a new name gets a provisional key that never shows) and `mesh` (calls `region::check`, which runs it; commits). Two channels with one key are `channel::Problem::Duplicate`, not a panic (REGION CHECK; it supersedes the panic of the architect, #756: https://github.com/synnaxlabs/foundation/issues/756#issuecomment-6031836890) |
| Index | Spec: `Kind::Index { error, control }`. Its settings come only from policies | As channel | `home`, `delivery`, `hub`, `buffer` | `spec` |
| Data channel | Spec: `Kind::Data(Data)`, where `Data::new(index, quality, data_type, unit)` refuses a unit on a type that holds no number. The `index` edge is defined here only (X23) | As channel | As index | `spec` |
| `channel::Key` | Spec (name to key map), wire setup, disk footers, stored bodies (STORED BODY). Never in files | `apply`, the first time a name appears | Everyone | `types` (value), `mesh` (assignment) |
| `node::Key` | Region state (membership record) | Voters at join | `hub`, `mesh`, `access` | `types` (value), `mesh` |
| `channel::Slot` | Memory, node-wide; never on the wire or disk | The node's slot table (`channel::Slots`) when the node learns a channel (owner: X42) | `hub`, `home`, `delivery`, `buffer` | `types` (value) |
| Key set | Memory, one per writer session: sorted slots, with each entry's key and type | The interner at writer open | `home` (routing), `delivery` (masks), `hub` | `types::frame` |
| Path (live or backfill) | A value, `frame::Path` (A6, A8). Each frame carries one in its header | Whoever freezes the frame: the home on a write, from its label after the B7 check; a decoder or catch-up, from the path the frame came with | `home`, `buffer`, `wire`, `delivery` | `types::frame` |
| Label (a path or resend) | A value, `frame::Label` (B7), on each write: the `hub` writer call and the wire write message. The only source of a write's path; none means live. Not in the frame block | The writer | `hub`, `wire`, `home` | `types::frame` |
| Per-connection short numbers | Memory, per connection | The `wire` encoder at setup | The `wire` decoder | `wire` |
| Data type | Spec, on each data channel (byte layout); interned per key set in memory | Files, then `apply` | `codec`, home checks, SDKs | `types` (layout), `spec` (`spec::data_type`, meaning) |
| Enum and flags definitions | Files, then Spec as named types with fingerprints | People, `discover` | Sinks, SDK code generation, `plan` | `spec` |
| Struct template | Files. `config` expands it into one channel per field. Stored form is open (5.1) | People, `discover` | `config` (expand, plan), SDK code generation, `export` | `spec`, `config` |
| Unit | Files, on a primitive channel or a struct field; Spec on the data channel. The unit table and standard codes are in the binary | People, `discover` | Unit checks at plan, sinks, reduction checks | `spec` (`spec::unit`) |
| Quality channel | Spec: a data channel of type `Quality` that data channels point at (`Data.quality`). Own index or the data's index | Values: the writer of its index; the home writes death records (X19) | Sinks (as-of), calculations | `spec`; values through `home` |
| Error channel | Spec: `Index.error` pointer | Values: the connector that writes the index (clock fit residual plus mesh bound) | Readers, sinks | `spec` |
| Control channel | Spec: `Index.control` pointer, placed with its index | Values: only the home, one sample per handoff (a published copy) | People, agents, auditors, new subscribers | `spec`; values through `home` |
| Region | Files: `region "<prefix>" { voters }`. The parent's spec holds the delegation record `{ prefix, epoch, initial voters }`; the region's own Raft config holds current voters (X3) | Parent voters create, remove, or force takeover; the region changes its own voters | `mesh`, `plan`, every node | `spec` (definition), `mesh` (groups) |
| Voters | Desired: the region block. Actual: Raft membership of the region's group | The region's own commits (joint consensus) | `raft`, `mesh` | `mesh`, `raft` |
| Policies (all kinds) | Files, then Spec | People, agents | `spec::resolve` (settings) or `access` (access) | `spec`, `config` (check), `access` |
| Retention policy | Spec; selects indexes: `{ select, keep }` (architect, #895: https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6032219156) | Files | `home` (gives the cutoff to `buffer.set_floor`), `buffer` (caps holds by store time; a trim follows STORE TRIM) | `spec` |
| Placement policy | Spec; selects connectors and indexes: `{ select, home, standby, copies }` | Files | `mesh`, supervisor, `replica`, `plan` | `spec` |
| Transmission policy | Spec; selects indexes (link side open, 5.1) | Files | `transport`, `hub` | `spec` |
| Compression policy | Spec; selects indexes; `mode` auto, raw, or max. The actual codec is a 1-byte tag per vector in the encoded bytes | Files | `codec` at the encoder (the home, or the writer's `hub`) | `spec`, `codec` |
| Reduction policy | Spec; selects data channels; deadband checked against the channel's unit | Files | Connector library component through `hub.spec()` | `spec`, `connector` |
| Time policy | Spec; selects node names; lists candidate peer nodes (default: the region's voters) | Files | `clock` | `spec`, `clock` |
| Access policy | Spec; `{ subjects, select, allow, authority }` | Files | `access`, called by the owners (`home`, `mesh`) | `spec`, `access` |
| Secret store policy | Spec; selects secret names | Files | The secret resolver | `spec` |
| Connector | Files, then Spec as `spec::connector::Connector { kind, node, config }`, keyed by its name | People, `discover` | Supervisor on the placed node, the kind | `spec` (shell) |
| Kind config | Kind-owned: an opaque Document in the spec (canonical form, no source positions, so hashes stay stable) | Files | The kind's check at plan, `ctx.config()` at run | `connector-<kind>` |
| Calculation | A connector of kind `calc`; program text is kind-owned; outputs on its own index | Files | `connector-calc` | `connector-calc` |
| Open folder (A2) | Files, then Spec (mechanism: X28) | People | `hub`, `mesh` | `spec`, `mesh` |

### 2.2 Runtime agreed state (region state)

| Concept | Defined or stored | Written by | Read by | Owner crate |
| --- | --- | --- | --- | --- |
| Node | Region state: membership record `{ key, card { name, public key, seal key, addresses, version } signed by the node, admission, ephemeral, status keys by name }` (MEMBER RECORD) in the region that holds the node's name. Private key: node-local. Files only name nodes | Voters at join (ticket); removal operation; removal of an ephemeral node after its time offline | `mesh`, `hub` (authentication), `access`, `plan` (name checks) | `mesh` (record), `node` (key material) |
| Membership | Region state: node records plus each region's voter set | Voters | Everyone | `mesh` |
| Node lease | Region state of the node's own region | The node renews; a renewal carries its version and seq block requests | Voters (promotion), `home` (fence, with the clock bound) | `mesh`, `home` |
| Actual home of an index | Region state of the home node's region: `{ home node, holder, seq block }` | Voters (promotion), `apply` (planned moves) | `hub` routing through `mesh` watches | `mesh` |
| Seq blocks | Region state of the home node's region | The home, through lease renewals | A new home after promotion | `mesh` |
| Index history (re-index) | Region state: spans and seals. The spec keeps only the current index. Which region: X39 | The old home proposes the seal; voters seal at lease end if it is down | `hub` joins spans for readers | `mesh` |
| Secret ciphertexts | Region state, outside the spec, one per eligible node (region of the secret: X40), with a version per name in the associated data. Every node takes a write or a delete only at the newest version plus one, and a re-seal only at the newest version, from and to nodes of the secret's placement. A delete is a version with no value. The newest version of a name is never compacted away, also after the spec removes the secret | `secret set` and `secret delete` (`ops` calls `secret::seal`) | The node that runs the connector opens it in `secret::store::Sealed`, which refuses a value that does not open at its version | `mesh` (record), `secret` (seal and open) |
| Join ticket record | Region state: options and use count. The ticket itself is a secret, never in files | Admin through `ops` | Voters at join | `mesh`, `ops` |
| Delegation record | The parent region's spec: `{ prefix, epoch, initial voters }` | Parent voters | Nodes (epoch fencing) | `mesh` |
| Spec pointer | Region state: `{ version, root hash }` | `apply` (compare-and-swap) | Every node that follows the region | `mesh` |
| Spec tree | Prolly tree chunks in the `blob` store on each node's disk | `apply` writes chunks | Nodes fetch the ranges they use | `spec` (tree), `blob` (chunks) |
| Changes channel | The region's Raft log presented as a channel; seq is the log index; one per region (X29) | Voters | Any node, `plan`, agents | `mesh` (served through `hub`) |
| Desired version, rollout lock, format flag | Desired version in the spec; lock and flag in region state (multi-region scope: 5.1) | `ops upgrade`; voters | `node` (binary swap); `codec`, `wire`, `buffer` get the flag injected | `mesh`, `ops`, `node` |

### 2.3 Per-index state at the home

| Concept | Defined or stored | Written by | Read by | Owner crate |
| --- | --- | --- | --- | --- |
| Encoded samples | Index log (write-ahead ring, then segments), as stored bodies (STORED BODY) | `home` and `replica` through `buffer.append` | Complete readers (catch-up), `replica`, crash recovery | `buffer`, `home` (stored body) |
| Seq counters (live, backfill) | Memory at the home; durable through the index log | `home` | `delivery`, `wire` (prediction) | `home` |
| Control state | Memory in `control` at the home; handoff records in the index log (HANDOFF RECORD; truth, copied by `replica`); control channel (published copy) | `control` decides, `home` records | New home at takeover (from the log, X18) | `control`, `home` |
| Control lease | A writer session setting; state in `control` | The writer at open | `control` | `control` |
| Reader positions | Truth: `delivery` state at the home, written as index log records and copied by `replica`. A connected reader's `hub` keeps its own position. Status channels publish copies | `delivery`; `replica` copies; `node` publishes | `home` after failover; `hub` on resume | `delivery`, `buffer`, `replica` |
| Holds and floors | `delivery` (hold per reader and index); floor = lowest held position per path, handed by `home` to `buffer.set_floor` with the retention cutoff | `delivery` | `home`, `buffer` | `delivery`, `buffer` |
| Backfill dedup marks | Index log records | `home` | `replica`, a new home | `home` |
| Gaps | Index log records (explicit gap with a count) | `home`, `buffer` | Complete readers | `home`, `buffer` |
| Stored and replicated marks | Memory at the home (the replicated mark is the standby's position in `delivery`); published on status channels | `home`, `delivery` | Writers (confirmation), `node` collector | `home`, `delivery` |
| Latest mailbox | Memory: depth 1 per latest reader per index | `delivery` | The reader session | `delivery` |
| Current value | Memory: the index's newest live frame, one pinned pool block per index (B4, MEMORY BOUNDS) | `delivery` | A new latest reader | `delivery` |
| Credits | Memory per session per index; credit messages on the wire | The reader's `hub` grants; `delivery` spends when the home releases a frame | `delivery` | `delivery`, `wire` |
| Live frames for complete readers | Memory: the index's live frames not yet on disk, and the frames released to each complete session and not taken, as refcount clones (B1, CREDIT RULES, MEMORY BOUNDS) | `delivery`: the home queues each stored live frame and releases them after a commit | The reader session | `delivery` |
| Masks and routes | Memory: mask per key set and reader; route per key set | `delivery` | The home's fan-out | `types` (mask), `delivery` |
| Death records | Quality channel samples (X19) | `home` | Sinks | `home` |
| Read copy data | The copy node's index log | `replica` | The copy's readers, served by `home` in copy mode (X43) | `replica`, `home` |

### 2.4 Sessions and values in memory

| Concept | Defined or stored | Written by | Read by | Owner crate |
| --- | --- | --- | --- | --- |
| Frame | Memory: one pool block (FRAME LAYOUT): a header (key set key, form, path), a range `{ group, count, seq }` for each present index group (X8), and a descriptor `{ entry, end }` for each present series, each list sorted. Wire form per connection. Never stored as a frame on disk | Writers, through `hub.block` or the frame builder; `home` (INDEX FRAMES) | `delivery` views, `hub`, `codec` | `types` (layout), `block` (memory) |
| Series | Memory: a slice of the frame's block. Encoded: tagged 1024-value vectors | Writers; `codec` | Readers | `types`, `codec` |
| Block | Memory: per-shard pools that `node` injects | Writers fill a `Unique`, then freeze it | Every holder, by refcount | `block` |
| View | Memory: none; borrows a frame and a mask | `delivery` | The reader session | `types` (value), `delivery` |
| Reader session | Memory: `hub` session (selector expansion, max-age check); per-index state in `delivery` at the home or copy | The reader (SDK, CLI, connector) | `hub`, `delivery` | `hub`, `delivery` |
| Writer session | Memory: `hub` session (key set, routing, confirmation); gate in `control`; seq and dedup in `home` | The writer | `hub`, `control`, `home` | `hub`, `control`, `home` |
| Subscription | The selector of a reader session, kept live in `hub` against `mesh` watches | The reader | `hub` | `hub` |
| Effective settings | Memory: a per-node cache of `spec::resolve` results | `mesh` | `home`, `transport`, `clock`, supervisor | `mesh` |
| Document | Memory: made by a front end from files, or by SDK code | Front ends | `config`, kinds | `document` (X21) |
| Diagnostic | Memory: made from a producer's error | Front ends, kinds, `document` | `config`, `ops` (text, `--json`, MCP) | `document` (DIAGNOSTICS) |
| Selector | A value inside policies, readers, connectors, and access, kept as written. Equality compares the texts in order, so equal selectors encode to equal bytes (#836) | Files, sessions | Every matcher | `types` (one matcher) |
| Plan | A JSON artifact with stable change kinds | `ops plan` | `ops apply` (commits exactly it) | `config`, `ops` |

### 2.5 Connectors, time, status, and node-local state

| Concept | Defined or stored | Written by | Read by | Owner crate |
| --- | --- | --- | --- | --- |
| Kind | Binary: one literal table built in `node` | The build | `config` (check), `ops` (discover), supervisor (run) | `connector` (contract), `connector-<kind>` |
| Commandable parameter | The kind declares it; the connector config marks it commandable; value = a channel plus an ack under the connector's name; files give the starting value (index layout: X24) | Subjects with access and authority | The kind, through `ctx` | `connector` (library), the kind |
| Run state | The `running` commandable parameter | As above | As above | `connector` |
| Shared endpoint | Memory: an endpoint registry on the kind value (process lifetime) | The kind | Connectors of that kind on the node | `connector` (endpoint component) |
| Connector status channels | Channels under the connector's name | The kind through `ctx.status()` | People, agents, tools | `connector` |
| Node status channels | Channels under the node's name (definitions: X27) | `node`'s collector through `hub`, from each crate's pulled values | People, agents, tools, rebalancers | `node` |
| Quarantine | Per out connector: a hold on the original data plus an error record (samples on a channel under the connector's name); size on a status channel | The kind, through a library component | `ops` list, retry, drop | `connector` |
| Secret value | Never in files, plans, or output. Built-in store: region state ciphertexts. External stores through adapters. References by name in kind config | `secret set` (person or CI) | `ctx.secret()` on the connector's node | `secret` (seal and open; `ops` seals, `node` opens), resolver (X40) |
| Time sources | Binary: a source table built in `node`; adapters probe for hardware | Adapters feed measurements | The estimator | `clock` (adapters), estimator crate (X11) |
| Mesh clock state | Memory per node; any shard reads its time and status (`Reader::now`, `Reader::status`); published as `<node>.clock.offset`, `.clock.error`, and the status | `clock`; `node` publishes | `hub.now()`, `home` (fence, stamp limits) | `clock` |
| Operation table | Binary | The build | CLI, MCP, embedded docs | `ops` |
| Node key material | Node-local disk | `node` at join | `transport`, `node` | `node` |
| Per-node settings (disk budget, pool budget, data directory) | Budgets: a policy in the spec; data directory: a start argument (NODE SETTINGS) | `apply`; whoever starts the node | `buffer`, `block` | `node` |
| SDK guide, JSON schemas for editors | Generated from kinds and the operation table | `ops`, `init` | Agents, editors | `ops` |

---

## 3. Contradictions and resolutions

Each item names the conflict, the resolution, and its basis. Every resolution is
decided (2026-10-04). Most follow from a later locked entry; the rest fall under a
delegation.

### 3.1 The checks the user asked for

**X1. Node in the spec vs node membership as runtime state.**
Conflict: S8 defines `Node { key, name, public_key }`, and BQ2 lists `spec::Node`.
BQ11a makes joining an operation and membership runtime state. r3's example layout has
a `nodes.fdn` file.
Resolution: delete `spec::Node`. A node is a membership record in the region state of
the region that holds its name (`mesh`). The spec refers to nodes by name only (region
voters, placement, connector `node`, time policy). `plan` checks those names against
membership and warns, but does not fail, on a node that has not joined yet. Basis:
BQ11a (later, user-locked).

**X2. K5 voters policy vs the `region` block.**
Conflict: K5 locks `[[voters]] select = "site_a.**"`, the MODEL MAP says "Voters ->
regions via selector", r8 lists `Policy::Voters`, and C6 and r4 assume a voters
policy. REGION BLOCK replaces it with `region "<prefix>" { voters }`.
Resolution: a region is a definition, not a policy. Remove `Voters` from the policy
kinds and from `spec::resolve`. Governance is "longest region prefix that contains the
name". A root `region ""` block (or an implied root) holds the root voters. Basis:
REGION BLOCK (later).

**X3. One `voters` list in the region block vs two owners of the voter set.**
Conflict: the region block shows one `voters` list. K5 REVISION says the region changes
its own voters and the parent only creates, removes, or forces takeover. r4 stores
initial voters and the epoch in the parent.
Resolution: creating or removing a region block is a parent commit that writes
`{ prefix, epoch, initial voters }`. A later change to `voters` is the region's own
commit (joint consensus). The current voter set lives only in the region's Raft
config. `plan` sorts each region-block change to the right region. A forced takeover
bumps the epoch in the parent. Basis: K5 REVISION.

**X4. Per-kind schema validation in `config` vs kinds owning their config.**
Conflict: C3 ("typed config with schema"), the AGENT REQUIREMENT ("JSON Schema for the
config language"), BQ2 ("checks it against its schema"), and r8 section 1.14
(`config::load(files, kinds: &Schemas)`) put a schema check in `config`. KINDS OWN
THEIR CONFIG says `config` never knows a kind's fields.
Resolution: each kind parses and checks its own Document and returns diagnostics with
positions plus the channels it reads, writes, and defines. A schema is only an output,
generated from the kind's own types, for docs, editors, and agent guides. `config`
never runs a generic schema validator. Basis: KINDS OWN THEIR CONFIG (later).

**X5. C3 groups vs connector = task.**
Conflict: C3 REFINEMENT, r8 traces (k) and (l), and the r12 catalog use groups:
`compose::groups`, `run::Commands` per group, `ctx.writer(&InGroup)`, and a `Device`
error handler at group scope. GROUPS DROPPED removes groups.
Resolution: one connector is one task with one primary session. Remove
`compose::groups`. `ctx.writer()` and `ctx.reader()` take no group argument. The
`Device` error class is handled by the composition for the whole connector (stop, back
off, reopen). A re-index moves a channel between connectors, never between groups of
one connector (r8 trace (k) "variant" path). The r12 note "choose the shard by
connector" now has no ambiguity, because a connector writes one index. Basis: GROUPS
DROPPED.

**X6. Per-group run channels vs commandable parameters.**
Conflict: r8 Q14 proposes a run command channel and ack per group plus a `stopped`
boolean in the spec. r12 has `run::Commands` per group. GROUPS DROPPED gives every kind
a `running` commandable parameter.
Resolution: `running` is the only run state. It is a commandable parameter channel with
an ack under the connector's name, gated by access and control authority. The files
give its starting value; after that, the latest command decides, so arming never
drifts from the files. Rename the r12 component from `run` to `params`. Basis: GROUPS
DROPPED.

**X7. Calculation outputs: first input's index vs own index; one output vs several.**
Conflict: r3 puts the output on the first input's index (a second writer on that
index). C5 locks "outputs on the calc's own index". The MODEL MAP says "Calculation ->
one output channel"; the C5 lock says "outputs".
Resolution: a calculation writes one index of its own, with one or more output
channels on it. Update the model map edge to "Calculation -> inputs, -> its own output
index". Trade: an output stores its timestamps again, so raw counts plus calculated
scaling meets P1's byte target only for the wire, not for the receiver's disk. Basis:
C5 + KINDS OWN (later).

**X8. S1 and S2 vs M1 and M3.**
Conflict: S1 defines `Frame { keys: Vec<channel::Key>, series: Vec<Series> }`. S2
defines `Series { seq, data: Buffer }` and "compression known per channel". M1 and M3
replace both. M3 also keeps a `seq` in each series descriptor, but A8 counts seq per
index.
Resolution: M1 and M3 define the frame in memory. The S1 principle still governs the
wire. `Buffer` is now `block::Block`. Under the memory delegation, move `seq`
from the series descriptor to the index-group entry in the frame header, next to the
sample count, because all series on one index in a frame share it; descriptors become
`{ offset, len }`. Name the three numbering spaces: `channel::Key` (identity),
`channel::Slot` (node-local), and per-connection short numbers (`wire`). Basis: M1, M3,
R10, MEMORY/PERF DELEGATED.

**X9. References inside one region vs free placement.**
Conflict: r4 Q5 requires every key reference, including `home` and `standby`, to stay
inside one region. BQ8 and READ COPIES let homes and copies sit elsewhere. "r4
reconciliation" calls placement "free", but BQ8 still binds the standby.
Resolution: definition references (index, quality, error, control) must stay inside
one region; `plan` fails otherwise. Placement names nodes, not keys: the home node may
be in any region; the standby must be in the home node's region (BQ8); copies may be in
any region. Basis: r4 reconciliation, BQ8, READ COPIES.

**X10. S13 quality on its own index vs BQ13.**
Conflict: S13 says a quality channel has its own index and is written only on change.
BQ13 allows the data's own index and per-sample writes. r8 Q13 found that an ack and
its quality on two indexes can arrive out of order.
Resolution: BQ13 holds. "Written on change" applies to a quality channel on its own
index. A quality channel on the data's index appears in the frames where its writer
includes it (A7 subset rule). Under the quality delegation, an ack's quality
channel shares the ack's index, so an ack and its failure code arrive in one frame.
Basis: BQ13, quality delegation.

**X11. The `time` crate vs `clock`.**
Conflict: C1, BQ1, HOME SPLIT, C9a, r8, and r12 say `time`. R9-D13 renamed the layer-2
crate to `clock` and put `Stamp`, `Span`, and `Range` in `types::time`. The r12
catalog also has a connector module named `clock` (`clock::Software`,
`clock::Window`, `clock::Fit`). TIME ADAPTERS says the same estimator also serves
device clocks in the connector library, but layer 3 cannot use a layer-2 crate other
than `hub`.
Resolution: the layer-2 crate is `clock` (it drives sources and the peer exchange, and
serves `now()`). The pure estimator, `Measurement`, the exchange state machine, and
the oscillator fit move to a layer-1 crate (`estimate`), used by both
`clock` and the connector library. Rename the connector module to `stamp`
(`stamp::Midpoint`, `stamp::Window`, `stamp::Fit`). Basis: R9-D13, TIME ADAPTERS, the
SRP PASS layer-1 rule, BQ21 (names).
Amended (2026-10-05, #143): there is no exchange state machine. The request carries
`sent` and the peer echoes it, so `estimate::exchange::Exchange` is plain data, and
`Exchange::measure` turns one round trip into a `Measurement`. `clock` sends requests
on a fixed timer and keeps no state for each one: a late answer is still an exchange,
and a lost one needs no timeout. The person approved it on 2026-10-05 ("Yeah I
approve").

**X12. The old term vs "region".**
Conflict: S9, K5, the MODEL MAP, C3, C6, C8, BQ8, R4 RESULTS, r4, r8 (including its
tree method for one prefix), and r13 use the old word for the governed part of the
tree.
Resolution: read every such use as "region". The tree method becomes
`spec::Tree::region`. Keep the old word only for Git and for the possible future mesh
branching feature. Basis: REGION LOCKED.

**X13. A standby as a hub reader vs the `replica` component; and what `replica` may
call.**
Conflict: r8 Q6 and its simplification list make the standby a complete reader through
`hub`. BQ6 makes `replica` a layer-2 component "using only delivery raw subscription,
buffer append_at, transport". But `delivery` is a pure state machine whose per-index
instances live inside `home`, so `replica` cannot reach the send side without calling
`home`.
Resolution: the send side is a raw cursor in the home's `delivery` state (one more
holding cursor), served over the network by `hub`'s server loop, which already serves
every incoming session (BQ1). `replica` is the receive side only: it dials the home,
receives log records, and stores them with `buffer::append_at`. It never touches the
home's write path. Dependencies: `transport`, `wire`, `buffer`, `mesh`. Basis: BQ6,
BQ1, r13 section 5.3 ("the standby pulls").

**X14. Reader positions: a channel vs log records.**
Conflict: S9 says "reader positions at the home"; S10 says positions are kept per
reader and index and "current readers and their holds are status channels"; r8 Q6 puts
positions on a channel. BQ6 says positions travel in the log.
Resolution: one truth with copies. `delivery` owns positions at the home; it writes them
as index log records, so `replica` copies them in order with the data. A connected
reader's `hub` presents its own position when it resumes at a new home. Status channels
show positions for visibility only. Basis: BQ6, BQ11b.

**X15. Status written by a connector kind vs by `node`.**
Conflict: r8 Q11 adds a `connector-status` kind, and r13 section 6.1 repeats it. BQ11b
and r12 I1 put the collector in `node`.
Resolution: no `connector-status` crate. `node`'s collector pulls each layer-2 crate's
values and writes `<node>.*` channels through a `hub` writer session. A connector writes
its own status through `ctx.status()`. The home writes only its companion samples
(control channel, death records). Basis: BQ11b (later, user-locked).

**X16. One language for definitions and calculations vs calculation strings with their
own grammar.**
Conflict: r3 Q4 says one language because HCL has expressions. K1 replaces that with a
small calculation grammar written as a string and "checked at plan", next to "config
checks only the Document". C5 + KINDS OWN then make the calculation engine a separate,
powerful, later design owned by the calc kind.
Resolution: the calc kind owns its language, its parser, and its plan-time check.
`config` never parses calculation text. Files carry the program as a string attribute
in every syntax (K1). Whether a large program may live in its own file is open (5.1).
r3's single-expression and no-loop limits are gone. Basis: C5 + KINDS OWN (latest).

**X17. The crate list and layers after the SRP splits, `replica`, `ops`, `clock`, and
the layer-1 rule.**
Conflict: C1 and C9a list `types` and `codec` in layer 1 and `cli` and `mcp` as
crates. r8 adds `env`, `blob`, `ops`, `connector-status`. HOME SPLIT puts `control` and
`delivery` in layer 2 as leaves. SRP PASS adds `access`, `raft`, `wire`, and `block`
and defines layer 1 as "pure logic, no I/O" and layer 2 as "drives disk, network,
clock", while keeping the BQ1 order. R9-D13 keeps a `block` module in `types` although
SRP PASS made `block` a crate. r12 already calls `control` and `access` layer 1.
Resolution: every pure crate is in layer 1: `block`, `types`, `env`, `document`,
`raft`, `estimate`, `control`, `codec`, `wire`, `spec`, `access`, `delivery`. Layer 2 is
`transport`, `buffer`, `clock`, `blob`, `mesh`, `home`, `replica`, `hub`, and `sim`. No
`cli`, `mcp`, or `connector-status` crates. `env` (from r8, never locked) is adopted
because T1 needs one home for the injected seams. Later additions under the
architecture delegation: `ring` (layer 1) splits the cross-shard rings and wake
protocol from `block`, and `os` (layer 2) holds the real implementations of the `env`
seams, so no other crate touches the OS directly. Full map in section 4. Basis: SRP
PASS, BQ6, BQ17, BQ19, BQ11b, R9-D13.

### 3.2 Data structures defined in two places

**X18. Control state after failover: read from the channel vs internal records.**
Conflict: r13 section 6.2 starts the new gate from "the copied control channel". BQ11b
forbids core decisions that read channels back. BQ6 lists "control handoffs" among the
log records.
Resolution: the new home's `control` takes its starting state (last holder, "held, not
connected" for one lease period) from the handoff records in the index's log. The
control channel is a published copy that the home writes. The control
channel sits on its own small index placed with the controlled index, because
home-written samples on the controlled index would break S6 strict increase against
the writer's device timestamps. Basis: BQ6, BQ11b, failover delegation.

**X19. Death records vs one writer per index.**
Conflict: QUALITY DECISIONS has the home write "source lost" to "each affected index's
quality channel(s)". A quality channel can be shared by many channels and sit on an
index homed elsewhere and written by another session (S13). The home would then write
another node's index. A18's "Foundation never judges quality" also reads against it.
Resolution (quality delegation): when a writer session ends without closing, each home
writes "source lost" only to the quality channels that the lost session itself was
writing, on indexes that home homes. The sample is stamped with mesh time. Later data
from the writer that is older than that sample must arrive as backfill (A6). This is a
recorded fact, not a judgment of values. Basis: quality delegation.

**X20. "One writer per index" used two ways.**
Conflict: A1 allows many writer sessions with one in control. A7 says "one writer in
control". r12 enforces "one writer per index" at `open_writer`. BQ15 says "never two
writers".
Resolution: two rules with two names. Runtime: at most one writer in control per index
at a time (the gate). Plan: at most one connector writes an index. The home itself may
add companion samples (control channel, death records) on indexes it homes. Basis: A1,
S11, BQ15.

**X21. The Document type and shared parsing parts have no layer-1 home.**
Conflict: K1 puts Document checks in `config` (layer 4). BQ2 stores connector config as
an opaque document in `spec` (layer 1). KINDS OWN THEIR CONFIG gives kinds (layer 3) a
shared Document reader with positions, name and unit parsing, and diagnostics. KINDS
OWN also says "`config` parses files to Documents", but K1 says front ends parse.
Resolution: a layer-1 crate `document` holds the Document, source positions,
diagnostics, and readers for durations, rates, byte sizes, names, and selectors.
Channel unit names live in `spec::unit`, name syntax in `types::name`. Front ends
(`config-hcl`) parse files; `config` reads only Documents. Basis: K1, BQ2, KINDS OWN,
"decide the best architecture".

**X22. Where a connector runs: the connector's `node` vs placement.**
Conflict: C3 and C5 SHAPE give each connector a `node` attribute. BQ10 says a placement
covers a connector and every index under its name. B7 gives indexes a default home on
the connector's node. A placement selecting the same connector could name another node.
Resolution: the connector's `node` is its required primary node (it is
attached to a device, and `discover` writes it). A placement that selects a connector
may add `standby` and `copies`, and may name only the connector's `node` as `home`;
`plan` fails otherwise.
An index's home, in order: a placement that selects the index, then the node of the
connector that writes it (B7), then a plan error. The placement resolves as a whole
policy (X25): when the winning placement names no home, a less specific one does not
give it (architect, #1150,
https://github.com/synnaxlabs/foundation/issues/1150#issuecomment-6032212749).
The order is `spec::placement::place`. Decided by architect-2 (#1150,
https://github.com/synnaxlabs/foundation/issues/1150#issuecomment-6039572310). It
names each placement by its tree key (architect-2,
https://github.com/synnaxlabs/foundation/issues/1150#issuecomment-6039872584).
Basis: BQ10, B7, C5 SHAPE.

**X23. The `index` edge stated twice.**
Conflict: S5 puts `index` on the data channel, and BQ9 re-indexes "by changing `index`
in the file". C3 REFINEMENT has the connector name its index and channels.
Resolution: the data channel definition owns the `index` edge. A connector's config
names the index it writes; its checker confirms that every channel it writes points at
that index. A struct template instance names one index for all its fields. Basis: S5,
BQ9, KINDS OWN.

**X24. Placement of companion channels.**
Conflict: Q11 (r8) requires the control channel on the same home as its index; r13 Q10
puts "command indexes and their control channels" under one placement; BQ13 lets a
quality channel share the data index. No entry says which index the ack, parameter,
error, and status channels use.
Resolution: an out connector writes acks and its status on one small index
of its own; parameter commands sit on one per-connector parameter index (one control
gate for all parameters, homed on the connector's node so arming works while cut off,
as r8 Q14 advised); the error channel is written by the connector that writes the
index. `plan` fails if a companion would be homed away from the index it serves. Basis:
BQ10, r8 Q14, BQ13.

**X25. Access combination vs the one resolver.**
Conflict: BQ2 makes `spec::resolve` "the ONE policy resolver" with most-specific-wins,
and S12 lists access as one of those policies. C8 makes access allow-only with no
conflicts (a union of allows).
Resolution: `spec::resolve` applies most-specific-wins to setting policies (retention,
placement, transmission, compression, reduction, time, secret store, node settings).
For node settings, each budget resolves on its own: a policy that leaves a budget unset
gives that budget to a less specific policy. A tie between the most specific policies
that set one budget for one node is a plan error, and a tie below them decides nothing
(SPECIFICITY); two policies that set different budgets do not conflict. Per-budget
resolution holds only because `disk` and `pool` are independent. It does not extend to
kinds whose fields go together (such as placement), where values from different policies
could make a combination nobody wrote. Access is evaluated only in `access`, as the
union of matching allows; the authority cap is the highest authority among matching
allows that grant `write`. Both use the one selector matcher in `types`. Basis: C8, SRP
PASS (`access` split).

**X26. Policy targets and reach.**
Conflict: S12 says "policies apply to whole indexes; data channels follow". Reduction
selects data channels (it is checked against a channel's unit). Access selects any
name and subjects. Time selects node names. Secret store selects secret names. BQ10
makes placement select connectors. r3 K2 forbids a policy from selecting outside its
region; r4 lets a root policy apply inside child regions.
Resolution: each policy kind states its target: retention, transmission, and
compression select indexes; placement selects connectors and indexes; reduction selects
data channels; time and node settings select nodes; access selects names (plus
subjects anywhere); secret store selects secret names. A policy may select only names
in its own region and that region's descendants; a descendant applies it as of the
last parent version it saw. Basis: S12, REDUCTION, C8, C6, r4 Q5.

**X27. Built-in channels have no spec definitions.**
Conflict: S8 puts node status under the node's name, and S9 adds the changes channel.
Nodes are runtime membership (BQ11a). Keys come from `apply` (M1/M2 answer), and only
`apply` changes the spec.
Resolution: built-in channels are defined by the binary, not by files. A
node's status channels are a fixed set per release, recorded with the membership record
at join, with keys assigned then. A region's changes channel is created with the
region. `hub` resolves names under a node name through membership. `plan` shows these
channels as read-only. Basis: BQ11a, BQ11b.

**X28. Runtime channel creation (A2) vs "only apply changes the spec" and "anything that
creates a channel is an explicit definition".**
Conflict: A2 lets the first write create a channel in an open folder, committed by
voters. K3 says only `apply` changes the spec. The REDUCTION rule requires an explicit
definition for every new channel.
Resolution: the open folder is itself an explicit definition in files
(not a policy). Inside it, a first write is a create operation (access action `write`
on the folder) that the folder's region voters commit to the spec and log on the
changes channel. `plan` lists such channels until files adopt or delete them. K3 then
reads: the spec changes only through `apply` and open-folder creation, and both are
logged. Basis: A2, K3, REDUCTION.

**X29. The changes channel: one for the mesh vs one log per region; its name.**
Conflict: S9 names one `mesh.changes` channel with seq equal to the Raft log index. K5
and R4 give each region its own Raft log. The name `mesh.changes` also does not use the
reserved `@` prefix (A3) and collides with user names that start with `mesh`.
Resolution (names delegation): one changes channel per region, named with the reserved
segment, for example `site_a.@changes` (root: `@changes`). `mesh` serves it; `hub`
routes subscriptions to `mesh`. Basis: K5, R4, A3, BQ21.

**X30. Catch-up merging vs presence per frame.**
Conflict: B6 says catch-up merges consecutive frames because "frame boundaries carry no
meaning". QUALITY DECISIONS makes optional-field presence per frame.
Resolution (memory delegation): catch-up merges only consecutive frames with the same
key set and presence mask. On disk, presence comes from the seq runs each chunk
records. Struct views use presence per sample range. Basis: QUALITY DECISIONS.

**X31. Timestamp ties: A5 vs S6.**
Conflict: A5 allows equal timestamps ordered by seq. S6, r8, and BQ4 require strict
increase per path.
Resolution: strict increase per index per path. A5's tie rule is retired, and the
InfluxDB tie concern goes with it. Basis: S6 (later).

**X32. The max-age check at the home vs at the reader.**
Conflict: B4 has the home skip a newest frame older than `max_age` for a new reader.
r8 trace (c) checks only at the reader's `hub` ("no second guard").
Resolution: one check, at the reader's `hub`, against the mesh time interval. Basis:
root "no defense in depth".

**X33. Where channels come from.**
Conflict: S5 and A2 say files list every channel. r3 lets a calculation infer and
define its output channel. GROUPS DROPPED creates `running` and parameter channels for
every connector. S7 creates field channels from a struct instance. C3 puts status
under each connector.
Resolution: a channel exists only when a definition in files creates it,
directly (a channel or index block) or by implication (a struct instance's fields; a
connector's status, parameter, and ack channels; a calculation's output index and
channels). A kind reports the channels its definition implies at check time. `plan`
lists every channel with the definition that made it, and `explain` shows it. This
keeps the REDUCTION rule, because a policy never implies a channel. Basis: REDUCTION,
KINDS OWN, S7.

**X34. Compression on links vs per-vector tags.**
Conflict: S2 says "compression known per channel" and "re-encode only for links with
different compression". B6 lists "compression level" as a per-link transmission
setting. R10 and BQ4 use a tag in every vector, encode once, and a `compression` policy
per index.
Resolution: no per-link re-encode and no compression field in transmission. The
compression policy decides at the encoder. Whether `max` belongs on thin links is a
measurement question (R10-D7). Basis: BQ4 (later).

**X35. "One byte format" for memory, wire, and disk.**
Conflict: S2 wants one byte format. r2 splits it: plain layout in memory, compressed
bytes on disk and in catch-up, stateful framing on the live wire. BQ4 encodes once at
entry.
Resolution: encoded series bytes (tagged vectors) are produced once and shared by disk,
catch-up, and live wire. Only framing (key set numbers, predicted seq and counts) is
per-connection state in `wire`. Memory holds raw bytes for local writes until the home
encodes. Basis: BQ4, R10-D3.

**X36. A fixed time source vs following the smallest bound.**
Conflict: C6 shows `[[time]] select = ... source = "site_a.gps_1"`. R6 TIME LOCKED
follows the smallest measured bound with no fixed ranking and detects sources.
Resolution: the time policy lists only the peer nodes a node may use as mesh
references (default: its region's voters). Local hardware sources are found
automatically. The estimator always follows the smallest bound. Basis: R6 TIME LOCKED,
TIME ADAPTERS. Amended by ESTIMATE COMBINE: the estimator follows more than half of the
bounds, not the smallest one (#344).

**X37. Bootstrap peers and relays.**
Conflict: D7 puts bootstrap peers in the file and lets any public node relay. R5 drops
"every node can relay" and puts relays in designated nodes "chosen by policy". BQ11a
tickets carry voter addresses.
Resolution: no peer list in files; join tickets carry the first addresses, and after
that addresses come from the mesh. Relays are designated by a
`relay` policy that selects node names, like the time policy. Basis: BQ11a, TRANSPORT
SHAPE.

**X38. The spec tree shape and gossip.**
Conflict: S9 describes a Git-like tree that follows the name hierarchy and gossip for
hints. R4 SETTLED uses one prolly tree per region keyed by full name and no gossip.
Resolution: R4 SETTLED. Basis: later entry.

**X39. The region of the index history.**
Conflict: r8 Q9 keeps the history in "the group that governs the channel's name". BQ9
says the old home records the seal with "its region's voters", and BQ8 keeps an index's
runtime records in the home node's region. The two differ when a cloud node homes a
site's index.
Resolution: keep the whole history in the channel name's region,
so a reader on any node finds it by name, and so the region that commits the re-index
also holds it. The old home proposes its seal there. If the old home is unreachable,
that region seals at the old home's lease end, which it reads from the home node's
region. Basis: r8 Q9, BQ9.

**X40. Secret names and which region holds ciphertexts.**
Conflict: K4 uses flat names (`secret = "influx_token"`). C8 and the SIMPLICITY
DIRECTIVE put everything in one name tree, governed by regions. BQ16 says "voters store
ciphertexts" without saying which region.
Resolution: secrets are full names in the tree (for example
`site_a.secrets.influx_token`). The region that holds the name stores its ciphertexts.
Access (`secret` action) and the secret store policy select secrets by name. Basis: C8,
SIMPLICITY DIRECTIVE.

**X41. Writer-side buffering.**
Conflict: A1 and A20 say "writes to the home are never buffered". BQ7 and r13 let a
writer keep unconfirmed frames and resend them after failover.
Resolution: live delivery is never queued for retry. A writer may keep unconfirmed
frames in a bounded memory window only to resend them, labeled `resend`, after
failover (B7). Basis: BQ7, B7.

**X42. The owner of the slot and key set tables.**
Conflict: M1 needs one node-wide slot table and key set interner. `hub` opens sessions,
but `home` (below `hub`) routes by key set and writes companion samples, so a
`hub`-owned table would point upward.
Resolution (memory delegation): both tables are layer-1 data structures, the slot table
(`types::channel::Slots`) and the interner (`types::frame::key_set::Interner`). `node`
constructs one interner per node, which owns the slot table, and passes that table to
`Buffer::open`. `node` injects the interner into `hub` and `home`. Interning happens
at session open; each shard reads a snapshot. `buffer` keys its in-memory tails,
floors, and read cursors by slot, and keeps the key on disk. `Buffer::open` assigns a
slot to each index it recovers; `node` opens every buffer before it opens sessions
(#219, 2026-10-05). Approved by the coordinator on PR #449. The shards open their
buffers one after another, in order of core, and pass the interner along; a failed
open does not pass it on, so no later shard opens. Start time is the sum of the
opens. When that is too slow, the exit is a two-step `Buffer::open`: recover in
parallel with no slots, then assign slots in one short step. Decided by the architect
on #1062:
https://github.com/synnaxlabs/foundation/pull/1062#issuecomment-6030791343.
Basis: M1, root principle on injected registries.

**X43. Which crate serves readers at a read copy.**
Conflict: READ COPIES has remote readers read and hold at the copy, but no entry says
which crate serves them. `replica` only receives.
Resolution (failover delegation): `home` opens a copied index in copy mode (the
crash-recovery open without a write path, gate, or seq) and serves readers through its
`delivery`. Holds at the copy are local to the copy. Basis: BQ6, READ COPIES.

### 3.3 Layer rules

**X44. C1 rule 1 ("layer 3 uses only `hub`") vs what connectors need.**
Conflict: connectors also need `types`, `block`, `spec`, `document`, the estimator,
and `env` seams.
Resolution: layer 3 may depend on any layer-1 crate and, from layer 2, only on `hub`.
`ctx` hands out the `env` seams. Basis: C1, BQ3, BQ5.

**X45. C1 rule 2 ("only layer 2 does I/O") vs connectors, front ends, `node`, and
secret adapters.**
Conflict: connectors open sockets and call vendor libraries; front ends and `node`
read files; secret adapters call Vault and cloud services.
Resolution: layer 1 does no I/O. Layer 2 does mesh I/O (peers, the disk buffer, the
clock). Layer 3 does outside-system I/O only through injected dialers, links, and
dedicated vendor threads. Layer 4 does process, file, and OS I/O. All of it enters
through injected seams, so simulation can replace it. Basis: T1, SRP PASS.

**X46. Where access is enforced.**
Conflict: BQ5's lock text says "hub and home enforce the rules (... access)". r8 Q12
and r12 enforce access only at the owner. BQ12 found that "authenticate at `hub`,
authorize at the owner" lets a forwarding node impersonate a subject.
Resolution: enforcement only at owners (`home` for data; region voters for apply,
secret, admin), with no check in `hub` or `ctx`. The owner learns the true subject
from the signatures it verifies (BQ12). Basis: root "no defense in depth", r12 table.

### 3.4 Terms used two ways

**X47. "Lease".** It means a node lease (failover), a control lease (writer setting),
and r12's `endpoint::Lease<T>`. Resolution: in prose, always "node lease" or "control
lease". Rename `endpoint::Lease<T>` to `endpoint::Handle<T>`. (Names delegation.)

**X48. "Slot".** It means `channel::Slot`, the latest-value slot (r8, B4), a kind
table slot (C5 SHAPE), and a memtable slot (r2). Resolution: "slot" means only
`channel::Slot`. Use "latest mailbox", "table entry", and "memtable entry". (Names
delegation.)

**X49. "Kind".** It means the channel enum (`Kind::Index`, `Kind::Data`), connector
kinds (`kind = "opcua"`), and policy kinds. Resolution: user-facing "kind" means a
connector kind only. Prose says "index channel" and "data channel", never "channel
kind". The internal `spec::channel::Kind` stays namespaced. (Names delegation.)

**X50. "Block".** It means a Document block (HCL) and a pool buffer (`block::Block`).
Resolution: keep both, namespaced (`document::Block`, `block::Block`). Prose says
"config block" and "pool block". (Names delegation; flagged in the log, BQ21.)

**X51. "Integration", "sink", "durable reader", "mesh file".** These retired terms still
appear in A11, A16, A19, B1, S10, K4, C8, and the reports. Resolution: read
"integration" and "sink" as "out connector", "durable reader" as "named reader with a
hold", and "mesh file" as "definition files".

**X52. Decision IDs.** r9 numbers its decisions D1 to D14, which collide with the log's
D1 to D7; r10 uses D1 to D8; r12 and r13 number theirs too. Resolution: cite them as
R9-Dn, R10-Dn, R12-n, and R13-n (as this record does).

**X53. D6 path lock vs C9c.** D6 says agents "cannot edit" contract and oracle paths.
T2 calls that too strict, and C9c enforces oracles by visibility only. Resolution: C9c.
People still own contracts and oracles; agents may edit them, and every weakening gets
an adversarial reviewer and a person's merge.

**X54. "Clock".** It means `env::clock::Clock`, the monotonic clock of one node, and
the `clock` crate, which serves mesh time. Resolution: in prose, "monotonic clock" for
the `env` seam and "mesh clock" for what the `clock` crate serves.

Count: 54 items (X1 to X54).

---

## 4. Final crate map

Rules:

1. A crate depends only on crates earlier in its own layer or on lower layers.
2. Layer 1 is pure: no I/O, no clock reads, no threads, no globals. It decides.
3. Layer 2 drives the mesh's own disk, peer network, and clock. It does.
4. Layer 3 may depend on any layer-1 crate and, from layer 2, only on `hub`.
5. Layer 4 may depend on anything below it.
6. Upward flow goes only through values the upper crate pulls (watches, streams).
   Seams that lower crates define and upper crates implement (`env` traits) are
   injected downward.
7. Only `os` touches the real clock, files, network, serial ports, randomness, and
   threads, through the `env` seams it implements. Only `clock` reads wall time
   through `env`; everyone else asks `clock`. Only `node` builds real seams, and only
   `sim` builds simulated ones. Below `hub`, only `home` writes channels, and only its
   companion samples.
8. Tests follow the same rules, with these extra dev-dependencies only: any crate may
   take `sim` and `counting`, `connector-ni` may take `daqmx-stub`, `hub` may take
   `buffer`, so its tests build a real `home::Shard`, and `access` may take `document`,
   so its tests build a `spec::connector::Connector` (`laptop.architect`,
   2026-10-08T03:01:36Z:
   https://github.com/synnaxlabs/foundation/issues/810#issuecomment-6051285927), and
   `config` may take `config-hcl` and `connector-influx`, so its tests check a real file
   with a real kind (`laptop.architect-2`, 2026-10-08T03:02Z:
   https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152). A crate
   may also take itself, so its tests and benches build with its own `sim` feature
   (STORED BENCH; `laptop.architect`, 2026-10-08T01:01:28Z:
   https://github.com/synnaxlabs/foundation/pull/1568#issuecomment-6049989224). The
   `hub` edge was decided by the architect (#340). Lost: `buffer` in the `hub` row (hub
   code could call the ring), the hub tests in `node`, and a second way to build a shard
   in `home`.

Order: layer 1 (`block`, `ring`, `counting`) -> `types` -> (`env`, `document`, `raft`,
`estimate`, `control`, `delivery`) -> `codec` -> `wire` -> `spec` -> `access`; layer 2
`os` -> (`transport`, `buffer`) -> (`clock`, `blob`, `sim`) -> `mesh` -> (`home`,
`replica`) -> `hub`; layer 3 `secret` -> `connector` -> `connector-<kind>`; layer 4
(`config-hcl`, `config`) -> `ops` -> `node`.

| Layer | Crate | Job (one sentence) | Allowed dependencies |
| --- | --- | --- | --- |
| 1 | `block` | Owns pools of preallocated, aligned buffers (`Pool`, `Unique`, `Block`, one refcount per frame, offsets only) and their unsafe memory code. | none |
| 1 | `ring` | Carries handles between shards through bounded single-producer, single-consumer rings, owns the wake protocol (loom-checked) and the `latest` cell that one shard writes and every shard reads, and holds its own unsafe slot code (memory delegation, 2026-10-04). A consumer parks at once: the shard idle loop owns the spin window through `try_pop` (#46). | none |
| 1 | `counting` | Counts heap allocations so tests and benchmarks can assert that code does not allocate, counts the heap bytes held so tests can bound the memory of a structure, finds freed blocks that hold given bytes so tests can assert that code erases a secret, and holds the `unsafe impl GlobalAlloc` of every crate after `block`, which keeps its own. A dev-dependency only. | none |
| 1 | `types` | Defines byte-level values: time, byte sizes, sample types, series, frames, key sets, masks, views, keys, slots, quality, names, node keys, Ed25519 public and private keys, with the derive, the sign, and the verify, subject hellos, connection keys, control authority, content digests, the one selector matcher, and the one quote form for text in diagnostics. | `block` |
| 1 | `env` | Defines the injected seams for monotonic time, the OS wall clock (read only by `clock`), files, the network, serial ports, randomness, shards, dedicated threads, and task spawning. | `types`, `block` |
| 1 | `document` | Defines the syntax-neutral Document with source positions, diagnostics, shared value readers, and its canonical encoding. | `types` |
| 1 | `raft` | Runs a sans-I/O replicated log (etcd model, PreVote, CheckQuorum) that knows nothing about specs. | `types` |
| 1 | `estimate` | Computes clock offset and error bounds from measurements, the peer exchange, and device oscillator fits, slews mesh time, and chooses what mesh time follows. | `types` |
| 1 | `control` | Decides who holds control of an index: authority, ties, control leases, handoffs, start state after failover. | `types` |
| 1 | `delivery` | Keeps each reader's state per index: positions, credits, live frames for complete readers, latest mailbox, holds, floors, position records, masks. | `types`, `block` |
| 1 | `codec` | Compresses and checks one series: per-vector selection, codecs, header validation, format version. | `types`, `block` |
| 1 | `wire` | Defines every message between two nodes, or between a program and the node it connects to, except the bodies of the mesh protocol, which `mesh` encodes (MESH WIRE): per-connection short numbers, predicted seq and counts, session, credit, and replication messages, format version. | `types`, `block`, `codec` |
| 1 | `spec` | Defines the definitions (channels, types, units, connectors with opaque config, regions, policies, open folders), the prolly tree, hashes, diffs, and `spec::resolve`. | `types`, `document` |
| 1 | `access` | Decides whether a proof is of its subject (signed hellos and requests), and whether a subject may do an action on a name: union of allows, authority cap. | `types`, `spec`; `document` as a dev-dependency only |
| 2 | `os` | Implements the `env` seams and `block::Memory` on the real operating system: monotonic and wall clocks, files, sockets, serial ports, memory, randomness, and threads. The only crate allowed to call them. Holds its own unsafe memory code in `os::memory` (BLOCK MEMORY), and the OS calls of its clock and wall clock in `os::clock` and `os::wall` (#117). On macOS, `os::allocate` holds one `fcntl(F_PREALLOCATE)` call, because `rustix` can allocate only part of a new file (architect, #931, https://github.com/synnaxlabs/foundation/issues/931#issuecomment-6030986099). | `env`, `types`, `block` |
| 2 | `transport` | Carries sessions of prioritized, cancellable streams and datagrams over QUIC, TLS over TCP, relays, and diodes on the `env::net` seam; never calls up. | `env`, `types`, `block` |
| 2 | `buffer` | Stores each index's log durably within the disk budget (write-ahead ring, segments, trimming, floors, `append`) through a per-OS driver. | `env`, `types`, `block`, `codec` |
| 2 | `clock` | Runs time source adapters and the peer exchange, feeds `estimate`, and serves mesh time as an interval. | `ring`, `env`, `types`, `estimate`, `wire`, `transport` |
| 2 | `blob` | Stores content by hash and fetches it from peers (spec chunks, binaries). | `env`, `types`, `block`, `wire`, `transport` |
| 2 | `sim` | Simulates the `env` seams (time, randomness, scheduling, files, network, serial lines) with a deterministic scheduler and fault injection; ships behind a feature. | `env`, `types`, `block` |
| 2 | `mesh` | Agrees per region, through `raft`, on spec pointers, delegations, and runtime state (membership, node leases, homes, seq blocks, index history, secret ciphertexts, tickets, versions, rollout lock, format flag); serves snapshots, watches, effective settings, and the changes channels. | `env`, `types`, `block`, `raft`, `spec`, `access`, `wire`, `transport`, `clock`, `blob` |
| 2 | `home` | Runs the per-index write path (time checks, seq, fence, control, storage, fan-out), crash-recovery and copy-mode opens, and companion writes. | `env`, `types`, `block`, `ring`, `control`, `delivery`, `codec`, `spec`, `access`, `buffer`, `clock`, `mesh` |
| 2 | `replica` | Receives an index's log from its home on a standby or copy node and stores it with `append`. | `env`, `types`, `block`, `wire`, `transport`, `buffer`, `mesh` |
| 2 | `hub` | Is the one path for every read and write: sessions across homes, routing, live selectors, the server loop, authentication, encode and decode once, raw cursors for replicas, re-index stitching, and the layer-3 window. | `env`, `types`, `block`, `ring`, `codec`, `wire`, `spec`, `transport`, `clock`, `mesh`, `home`; `buffer` as a dev-dependency only |
| 3 | `secret` | Resolves a named secret on the node that runs a connector, through store adapters chosen by policy; `node` hands it the sealed ciphertexts it pulls from `mesh`. Seals a value to a node's seal key, and opens it. | layer 1 |
| 3 | `connector` | Defines the kind contract (parse, check, discover, run), the thin supervisor, `ctx`, the component library, and the compositions. | layer 1, `hub`, `secret` |
| 3 | `connector-<kind>` | Translates one protocol, device family, store, or the calculation engine into channels. | layer 1, `hub`, `connector`; vendor libraries behind build flags, except a library loaded at run time, which links nothing |
| 3 | `daqmx-stub` | Stands in for NI's `libnidaqmx.so` in the tests of `connector-ni`, built as a shared library and as a Rust library. A dev-dependency of `connector-ni` only. | none |
| 4 | `config-hcl` | Reads and writes HCL files as Documents. | `types`, `document` |
| 4 | `config` | Checks core definitions in Documents, expands templates, hands connector blocks to kinds, and computes plans, explains, and exports. | layer 1, `connector`; `config-hcl` and `connector-influx` as dev-dependencies only |
| 4 | `ops` | Holds the operation table and handlers, generates the CLI, MCP tools, and docs, and runs each operation on the node that must run it. | `config`, `connector`, `hub`, `mesh`, `blob`, `sim`, layer 1 |
| 4 | `acceptance` | Runs the MVP acceptance scenarios against whole meshes built from `node`. Test-only. | all crates |
| 4 | `node` | Is the composition root: real seams, pools and shards, all tables (kinds, front ends, time sources, secret stores), the status collector, process lifecycle, and upgrades. | all crates |

Outside the binary: the Rust SDK reuses `block`, `types`, `codec`, and `wire`; other
SDKs hand-write their data path against golden vectors (D12).

---

## 5. Open items

### 5.1 Still open

Shapes that need the person:

1. **Calculation engine design.** Language, windows, state, placement, quality
   propagation, and whether a large program lives in its own file. Its guarantees are
   locked (C5 + KINDS OWN).

Measured before they lock:

2. **OPC UA crypto plugin.** Our own plugin on aws-lc, or compiled-in mbedTLS.

Parameters and later choices, recorded and not asked:

3. Struct template storage: whether the spec stores templates and instance records for
   SDK code generation and `export`.
4. The transmission policy target: links, indexes, or both (B6).
5. Upgrades across regions: which region holds the desired version and the format
   flag, and how finalization waits for every region (BQ18, C9d).
6. R12-4: a spec change restarts `run` in v1; commandable parameters are the runtime
   path.
7. A20: whether a channel may carry a default max age.
8. A3: partial-segment wildcards.
9. A13: bounded lists.
10. D3: license, free tier, monetization.
11. D5: a plugin system.

### 5.2 Settled under a delegation

On 2026-10-05 the person gave every open decision to the advisor and the
coordinator: "Don't block any decisiosn on me. consult with the advisor and come toa
conclusion together". Each one is listed below.

- Quality: X10 (ack quality on the ack's index), X19 (death record scope), R16-1 and
  R16-3 to R16-9 (r16 Rust guides).
- Memory and performance: X8 (seq per index group), X30 (merge rule), X42 (interner),
  S4 disk format starting point and its ring sizing (#637), `Layout::new` refuses a
  `body_max` under one block less the record header (#627), r12 I4 (`buffer`
  driven, not self-running), `ring` holds its own unsafe slot code (section 4).
- Failover: X18 (gate start from log records, R13-5 "held, not connected" grace), X43
  (copy mode), R13-10 (three voters for failover; `plan` warns with fewer), R13-6 (send
  after sync vs on receipt), #719 ("A PreVote answer, grant or refusal, shows the
  voter's state when it sent the answer."), #352 item 1 (a reply from a node that is not
  a peer).
- Names: X11 (`estimate`, `stamp`), X12, X29 (`@changes`), X47 to X50, X52, the
  tree key `<label>.@<kind>` of a policy (#729), `frame::split`, which cuts a frame
  body at its ends and gives each part (#632), HCL REFERENCES first segment (#536),
  generated names as strings (#701), and POLICY NAMES (#474).
- Delivery and wire internals: RECV WAITS (#581), the STREAM WIRE room order (#611),
  the STREAM WIRE hello (#55), a reader session key type per mode and the drop of a
  late reader call (#725).
- Architecture: X17 and section 4 (`env`, `document`, `estimate`, `secret` crates), X21,
  X44, X45; R12-3 error classes without groups; R12-7 vendor code only in dedicated,
  never-detached threads; R12-13 no always-on scan loop; R12-14 one cycle engine per
  connector; SHARD PIN (#718), the advisor's choice A narrowed to a bool; an error
  below `document` that a producer shows as a diagnostic (DIAGNOSTICS) has `Display`
  and `fix()` and no `Code`, and the grammar of a value has one home, in `types`
  (advisor, #328).

### 5.3 Parameters for experiment

- Delivery and wire: group commit interval, credit window (bytes, from link BDP), batch
  size, linger, max packet size, latest-over-TCP send buffer, priority mapping, catch-up
  merge size.
- Storage: write-ahead ring size, ring record alignment, segment flush size and age,
  chunk sizes, memtable cost per channel (estimate 100 to 200 bytes), eviction timing.
- Codecs: ALP refresh interval and skip rule (R10-D8), natural-order delta decode speed
  on a Pi 4 (R10-D5), fdelta on recorded plant data (R10-D4), when to build `max`
  (R10-D7), short-vector packing.
- Memory: allocator (mimalloc 3 off the hot path), pool size classes and budget, commit
  and purge policy, queue kinds and capacities, spin windows (0 on a Pi), latest
  mailbox mechanics, a compact copy of the current value (B4), cache-line padding.
- Replication: seq block size, node lease length, check-in period, fence margin, gate
  grace, standby send point (after sync or on receipt), SSD rule for Pi homes.
- Consensus and spec: Raft timeouts, prolly chunk size (~4 KiB) and chunker quality,
  root GC depth (last N roots).
- Time: exchange period, source discovery period, drift rate for bound widening
  (starts at 200 ppm, ESTIMATE COMBINE), stamp limits near 1970 and far future (A5).
- Transport: default carrier per traffic class (QUIC vs TLS over TCP, measured on
  Linux), GSO and GRO, ChaCha20 vs AES by platform, relay selection, the retry
  interval of a read that waits for a block (RECV WAITS), the `Complete` share of the
  turn (3 to 1, `LATEST_COST`).
- Compression and reduction defaults; retention defaults; disk budget defaults.
- Benchmark reruns owed: r1 handoff, r10 codecs, r11 memory on Linux x86-64 (pinned)
  and Raspberry Pi 4; `sim` binary size against P1 (BQ19); binary size and idle memory
  of R7 dependencies.

### 5.4 Deferred features

- Cross-company sharing (K5).
- SSO mapping a login to a subject (C8).
- Consumer groups for named readers (S10).
- Configurable latest-mode depth (B4).
- Hot standby connectors (r13 Q10).
- Synchronous replication or per-placement durability (BQ7 says none for now).
- Shared-memory SDK transport (M5).
- Unions (A14), f16, 128-bit integers, decimals (A9).
- Own PTP client; Starlink dish, White Rabbit, and timing card sources (R6, TIME
  ADAPTERS).
- OS clock steering where privileged (opt-in, C6).
- More front ends (TOML and others); YAML write support (K1).
- A Zenoh connector (R14).
- A FIPS build (R7).
- A plugin system (D5).
- Copy-on-write mesh branching, the reason the word "branch" is reserved (VOCABULARY).

### 5.5 MVP

Decided with the person on 2026-10-05. The MVP is an edge-to-cloud mesh that survives
a bad link:

- Two or more nodes (an edge node and a cloud node), joined by ticket, in one region,
  with Raft for the spec and membership.
- Inbound connectors, each with commands back to the device: OPC UA client, Modbus
  TCP and RTU, and NI DAQmx. Outbound: InfluxDB. The person cut LabJack, MQTT with
  Sparkplug B, and Kafka from the MVP ("eliminate 3 of those"). CI tests the NI
  connector against a stub `libnidaqmx.so` (R7 loads the library at run time). NI's
  simulated devices run only on the factory host, if NI's driver builds for its kernel.
- Store-and-forward: an edge node writes 1M samples/s (1% of P1) while its link to the
  cloud is cut for one minute. With a disk budget that covers the minute, the InfluxDB
  out connector (a named reader whose hold covers the cut) receives every sample, in
  seq order. With a budget that covers 30 seconds, it receives exactly one gap, whose
  count equals the trimmed samples. The `acceptance` tests run both. The cut was one
  hour until 2026-10-07 (STORE AND FORWARD, amendment).
- A time error bound on every sample. The bound must hold the true offset, and the
  MVP target is at most 1 s. A tighter target waits for the x86 and Pi 4 run (#260).
  The person accepted on 2026-10-05 ("as long as you've evaluated the performance
  costs of your decision against correctness then I'm ok with this").
- Command authority and audit (D2).
- The mesh as code: `plan` and `apply` from HCL, operated through the JSON CLI and MCP.
- Robust means: simulation-tested, fuzzed, and chaos-tested on real AWS links.

Out of the MVP: standby failover (`replica`), more than one region, the calculation
engine, and performance work past the P1 targets.

**Test budget (2026-10-05).** The person approved 1000 USD for AWS testing, and it
replaces BENCH SPEND: a nightly chaos lab (about 2 USD a day), a spot simulation swarm
of four c7i.8xlarge for four hours (about 9 USD at spot, with its ledger cap by "Cloud
machines" step 2 in `docs/coordination.md`), a nightly P1 benchmark on a c7i.metal-24xl
(about 4 USD), and benchmarks for hot-path PRs (about 10 USD). Hard cap: 100 USD a day
("test budget should be capped at $100 a day"). Every launch goes in the ledger (#15)
with its cap and an automatic shutdown first. Only `laptop.monitor` rents and ends
machines, by "Cloud machines" in `docs/coordination.md`, and no other session holds AWS
credentials (the person,
https://github.com/synnaxlabs/foundation/issues/15#issuecomment-6042582552,
2026-10-07T16:48:27Z). Supersedes: BENCH SPEND, and the coordinator as the session that
rents and ends the ARM RUNNER hosts. Those hosts stay under AWS CEILING, outside the
test budget, its limits, and "Cloud machines" step 3. Each launch and end of one still
gets its line on #15, with its 72-hour renewal stop as its end time. Step 4 checks each
instance by itself, because those hosts have no `issue` tag.

### 5.6 First phase

The first wave builds the riskiest pieces in parallel: `block` and `ring` (`memory`),
`types`, `codec`, and `wire` (`data-path`), `raft` and `spec` (`consensus`), and
`env`, `os`, and `sim` plus the QUIC against TLS over TCP benchmark on Linux
(`simulation`). The second wave adds `control`, `delivery`, `access`, then `buffer`,
`home`, and `replica` for a single-node write path measured against P1, then
`transport`, `mesh`, `clock`, and `hub`.

**FIRST SLICE (2026-10-05)** Before more features, one thin slice runs end to end: two
nodes in `sim` on the real `transport`, a writer on node A writes one channel, its home
stores it, and a reader on node B gets the same values in the same order. It goes
through a minimal `mesh` (the members and the home of one index, no snapshots) and a
minimal `hub` (one writer and one reader session). Access, config files, and failover
wait until its acceptance scenario passes. The plan and owners are on #462. The person
decided on 2026-10-05 ("Yes, let's do that", relayed by `advisor`): slower is fine, if
the system is solid.

Amendment (2026-10-08): ONE NODE work goes on beside FIRST SLICE, which keeps priority.
The ONE NODE entry states its scope. FIRST SLICE focuses on the internals, and ONE NODE
on the developer APIs and connectors. Supersedes, for ONE NODE work only, the order of
this entry (the person's decision of 2026-10-05, which has no link), and the order
"after FIRST SLICE" of the person's approval of ONE NODE
(https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6050540089). For
ONE NODE work, features, access, and config files do not wait until the acceptance
scenario of FIRST SLICE passes (#462). The person decided ("Yes, that's fine. I really
think that first slice should try to focus on the 'guts' the internals while ONE NODE
work should be focused on developer APIs and connectors."), relayed by `laptop.monitor`
at 2026-10-08T02:45:18Z:
https://github.com/synnaxlabs/foundation/issues/1737#issuecomment-6051113411.

**STORE AND FORWARD (2026-10-06)** The second milestone is the store-and-forward
scenario of 5.5: an edge node writes 1M samples/s while its link to the cloud is cut
for one hour (one minute since the amendment below), and the `acceptance` tests run
both disk budgets. It runs beside FIRST SLICE, which keeps priority. The person decided
on 2026-10-06 ("Yes that is fine I approve", relayed by `monitor`).

Amendment (2026-10-07): the cut is one minute, not one hour, at the same rate.
Supersedes the one-hour cut of 5.5 and of this entry (#1072). The hour writes 3.6e9
samples, and its edge buffer alone does not fit in `sim` on a CI runner
(https://github.com/synnaxlabs/foundation/issues/1149#issuecomment-6034058219). The
simulated InfluxDB store gets a compact form first (#1419). No scheduled run on a
rented host runs the hour. The person decided ("Let's do a smaller scenario. It can
still prove a significant amount of the behavior." and "Copy, yes I can agree with
that"), relayed by `laptop.monitor` at 2026-10-07T14:10:57Z:
https://github.com/synnaxlabs/foundation/issues/1149#issuecomment-6039778221.

5.5 sets no drain rate. Before the two tests lose `#[ignore]`, the lab runs a drain span
after the heal in place of `OUTAGE`: two times the sum of the delay before the drain
starts and the time to send `WRITTEN` at the drain rate measured in the lab.
`laptop.architect-2` decided this at 2026-10-07T16:31:12Z (#1477:
https://github.com/synnaxlabs/foundation/issues/1477#issuecomment-6042249280).

**ONE NODE (2026-10-08)** A milestone beside FIRST SLICE, which keeps priority: one real
node moves OPC UA samples to InfluxDB. The `foundation` binary starts a node from a
config, on a real disk and network, reads an OPC UA server through `connector-opcua`,
and pushes the samples to InfluxDB through `connector-influx`. Its acceptance is a
simulated OPC UA server, the node, and a simulated InfluxDB. A first version may run
with no OPC UA security, so the crypto plugin (5.1 item 2) does not block it. The
developer experience on one node is part of the goal, and #1737 breaks it into tests.
When it and STORE AND FORWARD both have ready issues, ONE NODE goes first. The FIRST
SLICE amendment of 2026-10-08 gives the split of the work and its "Supersedes". The plan
is on #1737. The person decided, relayed by `laptop.monitor`: the milestone ("Yes",
2026-10-08T01:52:14Z,
https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6050540089), the order
("Yes, let's do one node first. We should really prioritize a working devx that feels
relatively good with one node. and an influxdb to opc ua connector is prime for that",
2026-10-08T01:55:05Z,
https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6050570677), and the
work beside FIRST SLICE ("Yes, that's fine. I really think that first slice should try
to focus on the 'guts' the internals while ONE NODE work should be focused on developer
APIs and connectors.", 2026-10-08T02:45:18Z,
https://github.com/synnaxlabs/foundation/issues/1737#issuecomment-6051113411).
