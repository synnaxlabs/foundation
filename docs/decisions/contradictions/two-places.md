# Data structures defined in two places

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
covers a connector and every index that it writes to the mesh (`laptop.director`,
2026-10-08T18:36:00Z,
https://github.com/synnaxlabs/foundation/pull/1901#issuecomment-6066565970). B7 gives
indexes a default home on the connector's node. A placement selecting the same connector
could name another node.
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
