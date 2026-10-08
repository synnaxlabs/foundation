# Definitions

| Concept | Defined or stored | Written by | Read by | Owner crate |
| --- | --- | --- | --- | --- |
| Channel | Files, then Spec as `spec::channel::Channel { key, kind }`, keyed by its name (architect, #756: https://github.com/synnaxlabs/foundation/issues/756#issuecomment-6031378098). Sources of channels: X33 | People or agents in files; `discover` and `export` write files; `apply` commits | Every node through its spec snapshot; `home`, `hub`; kinds through `hub.spec()` | `spec` (type, edge checks: `channel::check` over the channels keyed by name; an index's control channel is on another index, X18), `config` (calls it on the planned set, where a new name gets a provisional key that never shows) and `mesh` (calls `region::check`, which runs it; commits). Two channels with one key are `channel::Problem::Duplicate`, not a panic (REGION CHECK; it supersedes the panic of the architect, #756: https://github.com/synnaxlabs/foundation/issues/756#issuecomment-6031836890, and the `Problem::Shared` that replaced it: https://github.com/synnaxlabs/foundation/issues/756#issuecomment-6032581487, superseded by `laptop.architect-2`, 2026-10-08T11:11:18Z: https://github.com/synnaxlabs/foundation/pull/1844#issuecomment-6058584682) |
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
| Struct template | Files. `config` expands it into one channel per field. Stored form is open (`docs/decisions/open/still-open.md`) | People, `discover` | `config` (expand, plan), SDK code generation, `export` | `spec`, `config` |
| Unit | Files, on a primitive channel or a struct field; Spec on the data channel. The unit table and standard codes are in the binary | People, `discover` | Unit checks at plan, sinks, reduction checks | `spec` (`spec::unit`) |
| Quality channel | Spec: a data channel of type `Quality` that data channels point at (`Data.quality`). Own index or the data's index | Values: the writer of its index; the home writes death records (X19) | Sinks (as-of), calculations | `spec`; values through `home` |
| Error channel | Spec: `Index.error` pointer | Values: the connector that writes the index (clock fit residual plus mesh bound) | Readers, sinks | `spec` |
| Control channel | Spec: `Index.control` pointer, placed with its index | Values: only the home, one sample per handoff (a published copy) | People, agents, auditors, new subscribers | `spec`; values through `home` |
| Region | Files: `region "<prefix>" { voters }`. The parent's spec holds the delegation record `{ prefix, epoch, initial voters }`; the region's own Raft config holds current voters (X3) | Parent voters create, remove, or force takeover; the region changes its own voters | `mesh`, `plan`, every node | `spec` (definition), `mesh` (groups) |
| Voters | Desired: the region block. Actual: Raft membership of the region's group | The region's own commits (joint consensus) | `raft`, `mesh` | `mesh`, `raft` |
| Policies (all kinds) | Files, then Spec | People, agents | `spec::resolve` (settings) or `access` (access) | `spec`, `config` (check), `access` |
| Retention policy | Spec; selects indexes: `{ select, keep }` (architect, #895: https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6032219156) | Files | `home` (gives the cutoff to `buffer.set_floor`), `buffer` (caps holds by store time; a trim follows STORE TRIM) | `spec` |
| Placement policy | Spec; selects connectors and indexes: `{ select, home, standby, copies }` | Files | `mesh`, supervisor, `replica`, `plan` | `spec` |
| Transmission policy | Spec; selects indexes (link side open, `docs/decisions/open/still-open.md`) | Files | `transport`, `hub` | `spec` |
| Compression policy | Spec; selects indexes; `mode` auto, raw, or max. The actual codec is a 1-byte tag per vector in the encoded bytes | Files | `codec` at the encoder (the home, or the writer's `hub`) | `spec`, `codec` |
| Reduction policy | Spec; selects data channels; deadband checked against the channel's unit | Files | Connector library component through `hub.spec()` | `spec`, `connector` |
| Time policy | Spec; selects node names; lists candidate peer nodes (default: the region's voters) | Files | `clock` | `spec`, `clock` |
| Access policy | Spec; `{ subjects, select, allow, authority }` | Files | `access`, called by the owners (`home`, `mesh`) | `spec`, `access` |
| Subject | Files as `subject "<name>" { keys }`, then Spec as `spec::subject::Subject` at `<name>.@subject` | People, agents | `access::admit` (#1747), `plan` (#1082) | `spec` (definition), `config` (OpenSSH read) |
| Secret store policy | Spec; selects secret names | Files | The secret resolver | `spec` |
| Connector | Files, then Spec as `spec::connector::Connector { kind, node, config }`, keyed by its name | People, `discover` | Supervisor on the placed node, the kind | `spec` (shell) |
| Kind config | Kind-owned: an opaque Document in the spec (canonical form, no source positions, so hashes stay stable) | Files | The kind's check at plan, `ctx.config()` at run | `connector-<kind>` |
| Calculation | A connector of kind `calc`; program text is kind-owned; outputs on its own index | Files | `connector-calc` | `connector-calc` |
| Open folder (A2) | Files, then Spec (mechanism: X28) | People | `hub`, `mesh` | `spec`, `mesh` |
