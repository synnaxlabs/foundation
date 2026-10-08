# The checks the user asked for

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
in every syntax (K1). Whether a large program may live in its own file is open
(`docs/decisions/open/still-open.md`). r3's single-expression and no-loop limits are
gone. Basis: C5 + KINDS OWN (latest).

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
seams, so no other crate touches the OS directly. Full map in
`docs/decisions/crate-map.md`. Basis: SRP PASS, BQ6, BQ17, BQ19, BQ11b, R9-D13.
