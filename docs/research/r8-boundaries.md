# Foundation boundary map (research fork 8)

Scope: every crate in C9a, ten end-to-end traces, the problems they expose, and the open
boundary questions in keystone order. Inputs: every locked decision in
`project_foundation_design.md`, the root CLAUDE.md architectural principles, and the
user's principles from the interview (dependency direction, structural simplification,
minimal vocabulary, performance first, the T1 injection rule).

Note for the parent: the memory file has a corrupted tail on line 703 ("...a one-by-one
boundaries interview from that map., arrays/optionals/unions, type definition and
evolution, ...") and stale entries that later decisions replaced (A1 "(epoch, seq)",
A2 "mesh file", A10 columnar structs, A14 validity bits, A15 struct fingerprints, A18
side array, D7 "linked meshes", B2 durable vs ad-hoc). They should be marked superseded
so a later session does not re-propose them.

---

## 0. Summary of the shape this map recommends

```
layer 1  values     types          (keys, DataType, Frame, Series, Selector matcher)
         defs       spec           (definitions, the content-addressed tree, policy resolution)  NEW
         codec      codec          (encode/decode, sans-I/O)
         env        env            (Clock, Fs, Rng traits; injected I/O)                         NEW
layer 2  leaves     time, transport, buffer, blob                                               blob NEW
         agreement  mesh           (Raft groups, spec pointer, runtime: homes, leases, members)
         owner      home           (per-index order, seq, gate, access, latest slot, reader state)
         waist      hub            (the one path: routing, authn, sessions, layer-3 window, server)
layer 3  edges      connector      (kind contract, actor, supervisor), connector-<kind>, connector-calc,
                    connector-status                                                            NEW kind
layer 4  surfaces   config         (files -> plan; templates; validation)
                    ops            (operation table + handlers; generates cli, mcp, docs)       NEW
                    node           (composition root, process lifecycle, kind table)
test/product        sim            (simulated env + transport; see Q19)
```

Layer 2 internal order (a crate depends only on crates to its left or in layer 1):
`time, transport, buffer, blob -> mesh -> home -> hub`.

---

## 1. Crates

Each entry: the one sentence it protects, the state it owns (one owner per piece of
state), a public surface sketch, its dependencies, its invariants.

### 1.1 `types` (layer 1)

Protects: the byte-level meaning of every sample, so that every crate reads the same
bytes the same way.

Owns: no runtime state. Owns the definitions of value-level vocabulary.

```rust
pub mod channel { pub struct Key(Uuid); }          // UUIDv7, A4
pub mod node    { pub struct Key(Uuid); }          // S8
pub struct Name(SmallStr);                          // dot-separated, A3 rules
pub enum DataType { Bool, I8, .., F64, Timestamp, Duration, Uuid, Str, Bytes,
                    Array(Box<DataType>, u32), Enum(EnumRef), Flags(FlagsRef),
                    List(Box<DataType>, u32) /* [R] S3 */ , Quality }
pub struct Series { pub seq: u64, pub data: Block }   // S2; Block = pooled bytes (see Q21)
pub struct Frame  { pub keys: Vec<channel::Key>, pub series: Vec<Series> }  // S1
pub struct Selector { include: Vec<Pattern>, exclude: Vec<Pattern> }       // S12 + C8
impl Selector { pub fn matches(&self, name: &Name) -> bool; }
pub struct Pool;   // fixed-size block allocator; the instance is owned by `node`
```

Depends on: nothing in Foundation.

Invariants: no I/O, no clock, no allocation on the frame hot path except through `Pool`.
`Frame` never carries anything both ends know from definitions (S1 principle).

### 1.2 `spec` (layer 1, NEW, see Q2)

Protects: one precise, hashable statement of what the mesh is defined to be, and one
answer to "which setting applies to this name".

Owns: no runtime state. Owns the definition types and pure functions over them.

```rust
pub struct Tree;                                   // content-addressed, follows the name tree (S9)
impl Tree {
    pub fn hash(&self) -> Hash;
    pub fn branch(&self, prefix: &Name) -> Option<Hash>;
    pub fn channel(&self, name: &Name) -> Option<&Channel>;
    pub fn diff(&self, other: &Tree) -> Diff;       // used by config::plan and mesh
}
pub struct Channel { key, name, kind: Kind }        // S5
pub enum Kind { Index { error: Option<channel::Key>, control: Option<channel::Key> },
                Data  { index, quality: Option<channel::Key>, data_type, unit: Option<Unit> } }
pub struct Connector { name: Name, kind: KindName, config: Document /* opaque */ }  // C3
pub enum Policy { Retention(..), Placement(..), Transmission(..), Access(..),
                  Voters(..), Time(..) }                                            // S12, C8, K5, C6
pub struct Effective { retention, placement, transmission, access, time }
pub fn resolve(tree: &Tree, name: &Name) -> Result<Effective, Conflict>;  // most specific wins
```

Depends on: `types`.

Invariants: `resolve` is the only implementation of policy precedence in the codebase.
Connector configs are opaque `Document`s here: only the connector kind decodes them,
and `config` validates them against the kind's schema. This keeps layer 1 from
depending on layer 3.

### 1.3 `codec` (layer 1)

Protects: every byte on the wire and on disk decodes to exactly what was encoded, at the
P1 size and speed targets.

Owns: no runtime state. Defines the per-connection state machine whose instances the
connection owns (key-to-short-number tables, predicted seq, A4, A8).

```rust
pub struct Encoder { /* per stream */ }
pub struct Decoder { /* per stream */ }
impl Encoder { pub fn series(&mut self, t: &DataType, s: &Series, out: &mut Block); }
impl Decoder { pub fn series(&mut self, t: &DataType, bytes: &[u8]) -> Result<Series, Error>;
               pub fn validate(&mut self, t: &DataType, bytes: &[u8]) -> Result<Count, Error>; }
pub const VERSION: u16;   // C9d: reads VERSION and VERSION - 1
```

Depends on: `types`.

Invariants: sans-I/O and fuzzable. `validate` checks a payload without materializing it,
so the home can accept encoded bytes from a remote writer cheaply (Q4).

### 1.4 `env` (layer 1, NEW, see Q20)

Protects: every source of nondeterminism enters through one injected seam (T1).

```rust
pub trait Clock { fn monotonic(&self) -> Instant; fn os_wall(&self) -> i64; fn sleep(..); }
pub trait Fs    { fn open(..); fn sync(..); fn sync_dir(..); /* durable vs pending */ }
pub trait Rng   { fn fill(&self, buf: &mut [u8]); }
```

Depends on: nothing. `node` passes real implementations; `sim` passes simulated ones.
The network seam is `transport::Transport`, not here, because its real implementation
lives in `transport`.

Invariant: no crate outside `node` and `sim` calls `std::time`, `std::fs`, `std::net`,
`tokio::net`, or an OS RNG. Enforced by clippy `disallowed-methods` and
`disallowed-types` in the workspace `clippy.toml`, plus the architecture agent (C9b2).

### 1.5 `time` (layer 2)

Protects: every timestamp in the mesh is in one time base with a known error interval.

Owns: offset estimates, error bounds, source selection, GPS and PTP device handles.

```rust
pub struct MeshClock;
impl MeshClock {
    pub fn now(&self) -> Interval;            // earliest..latest, TrueTime style (C6)
    pub fn to_mesh(&self, os_wall: i64) -> Interval;
    pub fn observe(&self) -> Watch<Status>;   // offset, error, source; read by connector-status
}
```

Depends on: `env`, `transport` (offset exchanges), `types`.

Invariants: the only producer of wall time. Never writes channels itself (Q11). Never
steers the OS clock unless the opt-in policy says so.

### 1.6 `transport` (layer 2)

Protects: authenticated, encrypted delivery between node keys, through NAT, with the
packing and priority the transmission policy asks for.

Owns: connections, streams, per-connection codec state, send queues and smart batching
(B6), liveness.

```rust
pub trait Transport {                        // real polymorphism: iroh vs sim (T1)
    fn dial(&self, peer: PublicKey) -> Result<Connection>;
    fn accept(&self) -> Incoming;            // yields (Connection, peer PublicKey)
}
pub struct Connection;                        // streams + datagrams; peer() -> PublicKey
```

Depends on: `env`, `codec`, `types`.

Invariants: knows keys, never names or subjects. Never calls up into `home` or `hub`:
incoming connections are pulled by `hub` through `accept` (Q1).

### 1.7 `buffer` (layer 2)

Protects: durable, ordered storage of each index's encoded samples within the node's
disk budget.

Owns: segment files, per-index logs (live and backfill paths), the disk budget, group
commit, trimming. Also persists the durable reader positions that `home` hands it (or
none, see Q6).

```rust
pub struct Buffer;
impl Buffer {
    pub fn open(cfg: Config { fs, clock, budget, dir }) -> Result<Self>;
    pub fn append(&mut self, index: channel::Key, path: Path, bytes: Block) -> Ticket;
    pub fn durable(&self) -> Watch<Durable>;            // per index: highest synced seq
    pub fn read(&self, index: channel::Key, path: Path, from: u64) -> Cursor;
    pub fn set_floor(&mut self, index: channel::Key, floor: u64);   // from home: holds + retention
}
```

Depends on: `env`, `codec` (segment framing only), `types`.

Invariants: bytes are stored exactly as handed in (S2: memory, wire, disk share bytes).
Trims oldest first and records an explicit gap (B1). Never decides who holds data; the
floor comes from `home`.

### 1.8 `blob` (layer 2, NEW)

Protects: any content fetched from any peer is exactly what its hash says.

Owns: the local content-addressed store and peer fetching.

```rust
pub struct Store;
impl Store {
    pub fn get(&self, hash: Hash) -> Result<Block>;          // local or fetched from peers
    pub fn put(&self, bytes: Block) -> Hash;
    pub fn pin(&self, hash: Hash); pub fn unpin(&self, hash: Hash);
}
```

Depends on: `env`, `transport`, `types`.

Used by: `mesh` (spec branches, S9), `node` (upgrade binaries, C9d). One mechanism for
both, instead of two fetch paths.

### 1.9 `mesh` (layer 2)

Protects: one agreed answer, per branch of the name tree, to "what is the spec" and "who
homes what".

Owns: one Raft group per voter branch (K5), each group's log and snapshots; per branch
the spec pointer (version + hash); runtime state per branch: current home of each index,
leases, seq block reservations (A8), node membership (Q11), node versions (Q18), the
mesh version flag (C9d), the rollout lock, and each re-indexed channel's index history
(Q9); the local spec snapshot of the branches this node uses; the materialized
effective settings for local consumers.

```rust
pub struct Mesh;
impl Mesh {
    pub fn spec(&self) -> Arc<spec::Tree>;                         // lock-free snapshot swap
    pub fn watch_spec(&self, sel: &Selector) -> Watch<SpecChange>;
    pub fn effective(&self, name: &Name) -> Effective;             // cached spec::resolve
    pub fn home_of(&self, index: channel::Key) -> Option<node::Key>;
    pub fn watch_homes(&self) -> Watch<HomeChange>;
    pub fn reserve_seq(&self, index: channel::Key) -> Result<Range<u64>>;
    pub fn history(&self, channel: channel::Key) -> History;      // epochs: index + seal
    pub fn propose(&self, change: Change) -> Result<Committed, Rejected>;  // CAS on pointers
    pub fn lease(&self) -> LeaseState;                              // this node's leases
}
```

Depends on: `env`, `time` (lease bounds), `transport`, `blob`, `spec`, `types`.

Invariants: never reads or writes data channels (Q11, Q18). Fast state stays out of Raft
(S9). Spec pointers change only through `propose(Apply)` (K3). The local snapshot swap
is atomic, so readers never see half a spec.

### 1.10 `home` (layer 2)

Protects: one authoritative order, sequence, control decision, and access decision per
index.

Owns, per index it homes: the live and backfill seq counters, the current seq block, the
control gate (holder, waiting writers, leases), the latest slot (B4), reader
subscriptions with their credits, positions, and holds (B2, B3, S10), backfill dedup
state (B7).

```rust
pub struct Home;
impl Home {
    pub fn open_writer(&self, subject: &Subject, w: WriterSpec) -> Result<WriterHandle, Denied>;
    pub fn write(&self, h: &WriterHandle, frame: FramePart) -> Result<Accepted, Rejected>;
    pub fn subscribe(&self, subject: &Subject, s: SubSpec) -> Result<Subscription, Denied>;
    pub fn ack(&self, sub: &Subscription, index: channel::Key, seq: u64);
}
```

Depends on: `buffer`, `mesh`, `time`, `codec` (validate), `spec`, `types`, `env`.

Invariants: the only place control is enforced (S11) and the only place data access is
authorized (Q12). Complete subscribers get a frame only after `buffer` reports it
durable (A8); latest subscribers get it before (B4). Live writes never wait (B5).
Stops accepting writes before its lease can have expired at the voters (fencing, uses
the `time` error interval). When a channel leaves an index it homes, it seals the
channel at the last accepted sample, refuses the channel after that, and is the only
proposer of the seal to `mesh` (Q9). Under C2's proposal, each index lives on exactly one shard
thread, so this state needs no locks.

### 1.11 `hub` (layer 2, the narrow waist)

Protects: one path for every read and write, local or remote, so routing, authentication,
and session behavior exist exactly once.

Owns: reader and writer sessions (S10) that span many indexes and homes; live selector
expansion (B2); the routing table from index to current home (fed by `mesh`); the
server loop for incoming connections; the layer-3 window.

```rust
pub struct Hub;
impl Hub {
    // layer-3 window (C1 rule 1); also served to remote SDKs over transport
    pub fn reader(&self, who: &Subject, r: Reader) -> Result<ReaderSession>;     // S10
    pub fn writer(&self, who: &Subject, w: Writer) -> Result<WriterSession>;     // S10, S11
    pub fn spec(&self) -> Arc<spec::Tree>;                                       // read-only (Q3)
    pub fn watch(&self, sel: &Selector) -> Watch<SpecChange>;
    pub fn now(&self) -> Interval;                                               // mesh time (Q3)
    pub fn block(&self, len: usize) -> Block;                                    // pooled (S2)
    // server side
    pub async fn serve(&self, transport: &dyn Transport);
}
```

Depends on: `home`, `mesh`, `transport`, `time`, `codec`, `spec`, `types`.

Invariants: authenticates every remote peer (peer key to subject through the spec) and
passes the subject to the owner; never authorizes (Q12). Checks max age at the reader's
side with the mesh time interval (trace c). Encodes for remote homes and decodes for
local readers of remote data, exactly once each (Q4). Presents a re-indexed channel as
one stream by following its `mesh` history (Q9). Not a pass-through: it adds
routing, fan-out across homes, live patterns, reconnection, and authentication. The
local call path into `home` is the one boundary-enforcing forward C1 sanctions.

### 1.12 `connector` (layer 3)

Protects: one small contract for every edge kind, and one actor that runs every
connector the same way.

C3 refinement shape: a connector is the device (there is no user-facing device or task).
Its `in` entries are grouped by index: one group = one clock = one index = one rate =
one writer session. `rate` is the hardware clock for a DAQ, the poll rate for Modbus,
and the requested sampling interval for OPC UA. Each `out` group is one reader session
with its own reader settings (mode, max age, hold).

Owns: the supervisor (start, stop, reconfigure actors placed on this node) and, per
connector, one actor: the device handle, one writer session per in group, one reader
session per out group, pacing for polled kinds, restarts with backoff, each group's run
state (Q14), and the re-index handover between its groups (Q9).

```rust
pub trait Kind: Send + Sync {                        // one per protocol; small
    type Config: DeserializeOwned + JsonSchema;      // whole connector, groups included
    type Device: Device;
    fn name(&self) -> &'static str;
    fn open(&self, cfg: &Self::Config, secrets: &Secrets) -> Result<Self::Device, Error>;
    fn discover(&self, target: &Target, creds: Creds) -> Result<Definitions, Error> { Unsupported }
}
pub trait Device {                                   // the one endpoint; one actor owns it
    fn start(&mut self, g: &InGroup) -> Result<(), Error>;     // NI: one DAQmx task per group
    fn read(&mut self, g: GroupId, into: &mut FrameBuilder) -> Result<(), Error>;
    fn write(&mut self, g: GroupId, frame: &Frame) -> Result<(), Error>;   // out groups
    fn stop(&mut self, g: GroupId);
}
pub enum Error { Retry(..), Config(..), Device(..) }        // C3 shared set
pub struct Actor;                                              // generic over Device
pub struct Supervisor;                                         // watches hub.watch(placed here)
pub struct Table(BTreeMap<&'static str, Box<dyn ErasedKind>>); // built in `node`, injected
```

`read` blocks on the device's own clock for hardware-clocked and push kinds (DAQmx
sample clock, an OPC UA subscription queue); for polled kinds the actor's timer calls it
at `rate`. Blocking versus async waits on C2 (fork 1). Prior art for a framework-owned
loop: the Synnax driver's `pipeline::Acquisition` calls `Source::read`
(`driver/pipeline/acquisition.h:39-50`); Kafka Connect's worker owns offsets, retries,
and task lifecycle while a `SourceTask` only implements `poll()`; Telegraf's agent owns
the interval while an input only implements `Gather`.

Depends on: `hub`, `types`, `spec` (definitions it reads through `hub`).

Invariants: one connector, one endpoint, one actor (C3). Kinds never open sessions,
never sleep to pace, never retry: the actor does all three once. A kind name from the
spec that is missing from the table is a config error at plan time (user input); a
missing kind at run time after a passed plan is a composition bug and panics (root
CLAUDE.md dispatch rule).

### 1.13 Connector kinds: `connector-<protocol>`, `connector-calc`, `connector-status`

Each protects the translation between one outside protocol (or expression, or the node's
own observables) and channels. Each owns its device or library handle and its in-flight
protocol state.

- `connector-calc` (C5 shape): the same contract with no special case. Its inputs are
  an out group (channels into the "device"), its outputs an in group (the device's
  results into channels); the expression state is the `Device`. Language is [R].
- `connector-status` (NEW kind, Q11): one per node, created automatically. Reads
  `Watch` values that `time`, `buffer`, `transport`, `mesh`, and `home` expose, and
  writes them as `<node>.*` channels through `hub`. Layer-2 crates then never need to
  write channels upward.

Depends on: `connector`, `hub`, and the protocol's own library (behind build flags for
C-backed libraries, D4).

### 1.14 `config` (layer 4)

Protects: files and spec agree only through a reviewed plan.

Owns: parsing the definition language (K1 [R]), template expansion (S7), validation,
plan computation, format-preserving file edits for `discover` and `export`.

```rust
pub fn load(files: &Files, kinds: &Schemas) -> Result<spec::Tree, Diagnostics>;
pub fn plan(desired: &spec::Tree, current: &Current, runtime: &RuntimeView) -> Plan;
pub fn write_definitions(files: &mut Files, defs: Definitions) -> Edit;
```

Depends on: `spec`, `types`. Receives kind schemas as an argument (from the table built
in `node`), so it does not depend on connector crates.

Invariants: pure apart from file reads and writes it is handed. `plan` lists access
changes on their own (C8) and renames as renames (A4).

### 1.15 `ops` (layer 4, NEW, see Q17)

Protects: every operation exists once and behaves the same from the CLI, MCP, and SDKs.

Owns: the operation table (C7) and the handlers. Generates the `cli` and `mcp` modules
and the embedded docs.

```rust
pub enum Operation { Plan, Apply, Explain, Export, Discover, SecretSet, SecretDelete,
                     Upgrade, JoinTicket, Read, Write, .. }
pub struct Handlers { mesh, hub, kinds, .. }   // injected
impl Handlers { pub async fn run(&self, who: &Subject, op: Operation) -> Result<Output, OpError>; }
```

Depends on: `config`, `connector` (table, discover), `hub`, `mesh`, `types`, `spec`.

Invariant: an operation runs on the node that must run it (Q17): `discover` on the
connector's node, `apply` at the branch's voters, `plan` wherever the files are.

### 1.16 `node` (layer 4)

Protects: the process: one place where everything is constructed and wired.

Owns: process lifecycle, the real `env` implementations, the `Pool`, the kind `Table`,
service install, binary replacement during upgrades.

Depends on: everything. Nothing depends on it.

Invariants: the only composition root. No `init()`-style self-registration (root
CLAUDE.md); the kind table is one literal list here.

### 1.17 `sim` (test support, product question in Q19)

Protects: any run of any mesh can be replayed exactly from its recorded random value.

Owns: simulated `Clock`, `Fs`, `Rng`, a simulated `Transport`, a deterministic scheduler,
fault injection, and the protocol simulators' hosting.

Depends on: `env`, `transport` (to implement the trait), `types`.

---

## 2. Traces

Notation: `crate::call`. Under C2's shard proposal, "home" means "the shard thread that
owns that index"; `hub` hands work to it through a lock-free queue.

### (a) A connector writes a frame to a local home

1. The actor of connector `site_a.plc_7` (kind `opcua`) runs in group `fast` (one index,
   one rate, one writer session). It calls `Device::read`, which waits on the
   subscription queue and fills pooled blocks from `hub::block`: one series per channel
   in the group plus the index series. Device time becomes mesh time through `hub::now`
   and `time::MeshClock::to_mesh` (S6).
2. The actor calls `hub::WriterSession::write(frame)`. The session resolved keys to the
   group's index and its home at open time. The home is local, so `hub` queues the frame
   to the owning shard. No encoding yet.
3. `home::write`: the gate checks the holder and lease (S11); timestamps strictly
   increase on the path and fall inside the A5 limits (`time` for "far future"); assign
   seq from the current block.
4. `home` places the raw frame in the latest slot and wakes local latest readers by
   reference, before any disk work (B4).
5. `home` encodes once with `codec::Encoder` (Q4) and calls `buffer::append`. Remote
   latest readers get the encoded bytes now.
6. `buffer` group-commits; `buffer::durable` advances; `home` releases the frame to
   complete subscribers (A8).
7. If the disk queue is full, step 5 records a gap instead and `connector-status` shows
   the warning (B5).

Exposes: who encodes and when (Q4); `hub` must expose time and pooled blocks (Q3).

### (b) A complete reader on another node catches up after an outage

1. On node R, an out connector (InfluxDB) calls `hub::reader(Reader { name:
   "influx_main", select: "site_a.**", mode: complete, from: resume, hold: 7d })`.
2. `hub` expands the selector against `mesh::spec`, groups indexes by `mesh::home_of`,
   and for remote home H calls `transport::dial(H)` and sends one subscribe per index
   group with the reader name.
3. On H, `hub::serve` accepts the connection, maps the peer key to a subject through the
   spec, and calls `home::subscribe(subject, ..)`.
4. `home` checks access (C8), loads the reader's stored position for each index, opens
   `buffer::read(index, path, position)`, and streams encoded bytes under credit flow
   (B3), merging consecutive frames (B6).
5. R's `hub` decodes once (Q4), hands raw series to the connector, and the connector
   acks after InfluxDB accepts. `home::ack` advances the position; `buffer::set_floor`
   receives min(held positions, retention).
6. Outage: the link drops. R's `hub` retries with backoff. H keeps the position and the
   data for up to `hold`, capped by retention (S10).
7. Reconnect: `hub` re-checks `mesh::home_of`, because H may have failed over meanwhile
   (trace e). Resume continues; a trimmed range arrives as an explicit gap.

Exposes: reader positions die with H unless they are replicated (Q6); `hub` must learn
home moves through `mesh` even while partitioned from the voters.

### (c) A latest reader feeding a control loop

1. A control program (SDK over the wire, or local) opens `hub::reader(.. mode: latest,
   max_age: 5ms)`.
2. `home::subscribe` sends the newest frame at once (B4).
3. Each new frame replaces the reader's one waiting slot. `transport` sends it at high
   priority (B6, wire internals).
4. The reader's `hub` drops any frame whose timestamp interval is older than `max_age`
   against `hub::now`. The home does not also check (no second guard).

Exposes: where the max-age check lives (reader side, once).

### (d) A command and its ack through the control gate

1. The controller calls `hub::writer(Writer { subject, authority: 200, lease: 50ms,
   path: live, channels: ["site_a.plc_7.valve_1.cmd"] })`. `hub` routes the open to the
   home of the command's index.
2. `home::open_writer`: access check (allowed `write`, authority within the cap, C8);
   the gate grants or queues by authority (A1). On a handoff, `home` appends the holder
   to the index's control channel (S11) in the same shard step.
3. `hub::WriterSession::write(cmd)`. `home::write` checks the holder and lease, assigns
   seq, and fills the latest slot.
4. The actor of `site_a.plc_7` receives the command on its out group (one reader
   session, latest mode with `max_age`; its `hub` checks age), calls `Device::write`,
   then writes the applied value to `valve_1.ack` through the writer session of the
   in group that holds the ack. On failure it writes the quality
   channel instead (A20, S13).
5. The controller's SDK "write and wait for ack" is a latest reader on the ack channel,
   waiting for the next sample after its write.

Exposes: if the ack's quality channel has its own index, the SDK can see the ack sample
before the "bad" quality sample, because order holds per index only (B3). A race (Q13).
Also: the control channel is written by `home`, so its index must live on the same home
(Q11).

### (e) Home failover to a standby

1. Node X homes index I; placement names standby S. S has been replicating I (Q6).
2. X dies. Its lease in the group that governs I's name is not renewed.
3. X fences itself if it is merely partitioned: it stops accepting writes at
   `lease_end - error bound` by its own `time` interval, before the voters can reassign.
4. The voters' leader sees the lease expire and proposes `homes[I] = S` plus a fresh seq
   block for I (A8). `mesh.changes` carries the record.
5. S's `mesh` watch fires; S's `home` opens I from its replicated `buffer` at the new
   block, with an empty gate.
6. Writers' `hub` sessions see their connection to X fail, re-resolve `mesh::home_of`,
   and reopen at S. The gate is rebuilt by reconnects; the highest authority wins again.
   The control channel records "no holder", then the new holder.
7. Readers' `hub` sessions reopen at S and resume (positions: Q6).
8. X returns: its `mesh` learns it no longer homes I. Its unreplicated tail (seq after
   S's last replicated sample, before S's new block) goes to S as backfill and is
   deduplicated (A6, A8, B7). Nothing is lost and nothing is applied twice (Q7).

Exposes: standby replication is not designed yet (Q6); the durability contract on
failover (Q7); which group holds X's lease when X homes indexes in several branches (Q8);
connectors placed on X have no failover (Q10).

### (f) Apply

1. `ops::Plan` runs where the files are: `config::load` parses files, expands templates
   (S7), validates connector configs against kind schemas (from the table, passed in),
   fetches the current branch pointers and needed branches through any node (`blob`),
   and calls `config::plan`. Affected readers come from status channels, read through
   `hub` (runtime view, not spec).
2. `ops::Apply(plan)` goes to the voters of each affected branch. The voters check the
   subject's `apply` permission (Q12). `mesh` puts the new branch blobs in `blob` and
   proposes a compare-and-swap of each branch pointer. Cross-branch plans commit one
   branch at a time, parent first, and report a partial result (K5 trade).
3. Every node following the branch: `mesh` fetches the changed subtrees it uses, checks
   hashes, swaps its snapshot, recomputes `Effective` for changed names, and notifies:
   - `home`: retention, access caps, new or removed channels for indexes it homes
   - `transport`: transmission settings
   - `connector::Supervisor` (through `hub::watch`): start, stop, or reconfigure
     connectors placed here
   - `hub`: live selectors gain or lose channels
   - `time`: time source policy
4. A placement change for an existing index is a planned home move: the new home starts
   as a standby, catches up, then `mesh` swaps `homes[I]` and the old home hands off
   (trace e without the lease lapse).

Exposes: authorization for apply lives at the voters; the planned-move path should reuse
failover (Q6).

### (g) Discover

1. `ops::Discover { kind: "opcua", target, node: "site_a.gateway_1" }` from a laptop goes
   over the wire to that node, because only it can reach the device.
2. The node's `ops` handler looks the kind up in the table (a missing user-provided kind
   name is a validation error) and calls `Kind::discover(target, creds)`.
3. Credentials: the connector does not exist yet, so no secret is sealed to this node
   (K4). Recommendation: `discover` takes credentials from the caller's environment for
   that one call and never stores them (Q16).
4. The result (types, templates, channels, connector with `in` and `out` mappings) returns
   to the laptop; `config::write_definitions` edits the files; the person reviews the diff,
   then plans and applies.

### (h) Upgrade

1. `ops::Upgrade("1.4.0")` sets the desired version (Q18) through `mesh::propose`.
2. Each node's `node` crate fetches the signed binary from `blob` by hash, from a nearby
   peer, and checks the release signature.
3. `mesh` runtime holds a rollout lock; one node at a time takes it, replaces its binary,
   restarts, and reports its version in its next lease renewal. Voters upgrade one at a
   time to keep quorum.
4. When every member reports the new version, the leader sets the mesh version flag and
   new formats turn on (C9d).

Exposes: `mesh` must learn versions without reading data channels (Q18).

### (i) A node joins

1. A subject with `admin` on `site_a` runs `ops::JoinTicket { name: "site_a.gateway_3" }`.
   The voters issue a signed ticket: mesh identity, voter addresses, the allowed name,
   and an expiry.
2. The new machine runs `foundation join <ticket>`. `node` creates its Ed25519 key,
   `transport` dials a peer from the ticket, and presents the ticket and public key.
3. The governing group commits the member: `Node { key, name, public_key }`.
4. The node fetches the branches it uses, starts its lease, and its supervisor starts any
   connectors placed on it.

Exposes: step 3 changes state outside `apply`. If nodes are spec, every join is drift
(Q11).

### (j) Secret set and delivery

1. `ops::SecretSet("influx_token")` reads the value from stdin.
2. The handler finds every connector that references the secret and every node that
   placement may run it on (primary and standbys).
3. It seals the value to each of those nodes' public keys and sends the ciphertexts to
   the governing branch's voters, who store them outside the spec (Q16).
4. A node starting the connector decrypts its ciphertext locally. Voters and other nodes
   cannot read it.
5. If placement later adds a node, or a node key rotates (S8), `plan` reports that the
   secret must be set again, because only a holder of the plaintext can seal it.

### (k) Re-index a channel

Case: `site_a.daq_1.pt_101` moves from in group `fast` (index A, 1 kHz) to a new in
group `slow` (index B, 10 Hz) on the same connector. Key and name stay.

1. A person or an agent moves `pt_101` between `[[connector.in]]` groups in the files.
   `config::load` keeps the key (A4), so `config::plan` reports a re-index, not a delete
   plus a create. The plan also shows: new samples go to B's home (placement follows
   the index, S12); the history stays on A's home under A's retention; companions that
   share A (a quality channel under Q13, the control channel of a command) move too or
   the plan fails; A and B sit in one branch (fork 4's proposed rule).
2. `ops::Apply`: the voters compare-and-swap the branch pointer (trace f).
3. On the connector's node, `mesh` swaps its snapshot and the actor sees the change
   through `hub::watch`. The actor owns both groups, so it orders the handover itself:
   1. `WriterSession(fast)::release(pt_101)`: `hub` sends an end mark in-band after the
      last frame that holds `pt_101`.
   2. A's `home` seals `pt_101` at its last accepted (seq, ts), refuses `pt_101` in
      later frames, and proposes `Seal { channel, index: A, seq, ts }` to `mesh`.
   3. `mesh` commits the seal in the group that governs the channel's name. The
      channel's history gains a closed epoch on A and an open epoch on B.
   4. The actor adds `pt_101` to the `slow` writer session. B's `home` accepts it only
      after the seal, and only with ts after the sealed ts (A5's strict increase
      carries across the boundary). Samples the device makes during steps 1 to 4 wait
      in the actor; past a bound they become a gap (B5).
4. `buffer` moves nothing. Stored samples keep their timestamps under A (the disk
   format records which index each stored range used: fork 2).
5. Readers: `hub` resolves `pt_101` through `mesh::history`. A complete reader reads A
   to the seal, then B; `hub` holds B's `pt_101` frames until its position on A passes
   the seal, so order holds across the boundary. A latest reader switches at once. A
   time-range read splits at the sealed ts and can touch two homes.
6. Retention trims the old samples with the rest of A. When A holds none, `mesh` drops
   the epoch. Deleting A while an epoch points at it: `plan` shows the data loss.

Variants:

- The rate of a whole group changes: no re-index. The index keeps its key and its
  timestamps change spacing.
- The channel moves to another connector (a sensor rewired to another DAQ): the old
  actor releases, and the new actor waits for the seal through `hub::watch`. If A's home
  is down, its lease lapses and the voters seal at the lease end. Fencing (trace e)
  makes that later than every sample A could accept. The seq fills in when A returns.

Owner of the index history: `mesh` runtime state, in the group that governs the
channel's name. The old home is the only proposer; `hub` is the reader. Not `spec`,
which holds only the current index (the desired state). Not a directory in `buffer`,
because a reader on a third node must find both homes without asking every home.

Exposes: problems 26 to 28 (Q9).

### (l) Arm a group before a test

1. In the spec, group `fast` has `stopped = true`. The actor opens the device and the
   group's writer session but does not call `Device::start`.
2. A test sequence (SDK, CLI, MCP, or a calc) takes control of the group's run command
   channel at authority 200 and writes `true`.
3. The command's `home` checks the gate (S11) and access (C8). The actor's run reader
   (an implicit out group) receives it, calls `Device::start(fast)`, and writes `true`
   to the run ack channel (A20 pattern).
4. Writing `false` stops the group the same way. No apply and no drift: the files still
   say `stopped = true`, and the run channels record every start and stop.

Exposes: problem 30 (Q14).

---

## 3. Problems the traces expose

Boundary and direction problems:

1. **Layer 2 has no internal order.** C1 says "lower layers only" but layer 2 holds six
   crates. Without an order, `transport` delivering incoming frames to `home` points
   upward. Fix: `time, transport, buffer, blob -> mesh -> home -> hub`; `hub` pulls
   incoming connections through `Transport::accept` and serves them (Q1).
2. **Spec types have no home.** `home`, `transport`, `mesh`, `hub`, and layer 3 all read
   definitions, but `config` (layer 4) produces them. The definitions must sit in layer
   1. Connector configs are typed in layer 3, so the spec must hold them as opaque
   documents (Q2).
3. **Layer 3 needs more than frames.** Connectors also need definitions (types to decode,
   their own config), mesh time (S6 conversion), and pooled blocks (S2). Under C1 rule 1,
   all of it must come through `hub` (Q3).
4. **Encoding has no single owner.** S2 says memory, wire, and disk share bytes, but a
   local connector hands raw values, a remote writer sends encoded bytes, and readers
   want raw values. Without a rule, encoding happens twice or validation is skipped
   (Q4).
5. **Standby replication is undesigned**, and reader positions die with the home (Q6).
6. **The failover durability contract is undecided**: what happens to frames the old
   home stored but never replicated (Q7).
7. **"Leases per node" (S9) conflicts with K5's branches** when a node homes indexes in
   more than one branch, such as a cloud calculation writing `site_a.pt_101` (Q8).
8. **Connectors cannot fail over.** C3 gives a connector one `node`; trace (e) moves the
   home but the connector dies with its node (Q10).
9. **Status channels have no writer.** S8, C6, S10, and B1 put status in channels, but
   the producers (`time`, `buffer`, `mesh`, `home`) are below `hub`, so they cannot write
   channels without pointing upward (Q11).
10. **`mesh.changes` has no home**, so `hub` would need a special case to route
    subscriptions to it (Q11).
11. **The control channel can be placed away from its index.** `home` writes it at each
    handoff, so it must be co-placed with the index (Q11).
12. **Access enforcement location is unstated.** S11 makes the home's gate the only
    control check, but read/write/apply permissions (C8) could be checked at the
    caller's node, the owner, or both. Both is defense in depth (Q12).
13. **Ack and quality can arrive out of order** when the quality channel has its own
    index (S13), because order holds per index only (Q13).
14. **Secrets are a third kind of state**: neither spec (changed only by apply) nor runtime
    (changed only by the mesh). Sealing to one node also breaks connector failover and
    `discover` before a connector exists (Q16).
15. **Nodes in the spec make every join drift**, because only `apply` changes the spec
    (K3) (Q11).
16. **Upgrade needs node versions in `mesh`**, but reading status channels would make
    `mesh` read the data plane (Q18).
17. **`sim` is test-only (C9a) but `plan --simulate` (C7) ships it** in the product, and
    protocol simulators have no stated home (Q19).
18. **Two notions of time.** The injected `Clock` (OS time, monotonic) and `time`'s mesh
    clock both exist; a stray `os_wall()` stamp would be wrong data, not an error (Q20).
19. **The operation table has no crate.** C7 defines it; C9a lists `cli` and `mcp` but not
    the table or its handlers, and operations must run on specific nodes (Q17).
20. **The kind table's home is unstated.** It must be built in `node` (the composition
    root) and injected into the supervisor, `config`, and `ops`.
21. **The connector contract leaks the runtime.** `async fn run` assumes Tokio, which C2
    has not decided. Blocking vendor libraries (DAQmx, LJM) need a dedicated thread either
    way.
22. **Naming collisions.** The crate `buffer` (the disk buffer) collides with S2's
    `Buffer` (pooled bytes). `types` is too generic to pass the "this crate protects X"
    test once `spec` splits out (Q21).
23. **T1 breaks** in: iroh's internal timers and RNG; Tokio's own clock; vendor libraries'
    threads and timers (DAQmx, LJM, librdkafka); rustls's OS RNG; any `std::time` call.
    Fixes: the `Transport` trait, turmoil's simulated Tokio, protocol simulators in place
    of vendor libraries, mad-turmoil RNG interposition, and the clippy
    `disallowed-methods` lint (Q20).
24. **Templates and the spec size.** S7 says the runtime knows only channels, but typed
    SDK views need the templates. The spec should store both: expanded channels (used by
    the runtime) and templates (used only by codegen and `plan`).
25. **Writer sessions across homes** can be partly accepted (S1 trade). `hub` must report
    acceptance per index, and the SDK must expose it.
26. **Index history has no owner.** The spec holds the current index (desired state) and
    storage holds samples per index, but nothing holds the directory and the boundary a
    reader needs to stitch one channel across two indexes (Q9).
27. **A re-index can split one channel across two homes**, because placement and
    retention follow the index (S12). Time-range reads touch both homes, and deleting
    the old index drops the channel's history (`plan` must show it).
28. **Companions must move together.** A command's control channel, and a quality
    channel that shares the index (Q13), cannot stay behind; `plan` checks it.
29. **A per-kind `run` duplicates the actor.** With groups, every kind's own loop would
    re-implement sessions, pacing, restarts, re-index handover, and run state. C3's
    `run` should become per-group device hooks under one actor (Q5).
30. **Group run state has no owner.** Arming acquisition before a test is a runtime
    change, but only apply changes the spec (K3). In the spec, every arm is drift
    (Q14).
31. **A shared hardware clock across devices** conflicts with one writer per index and
    one endpoint per connector (Q15).

Simplifications the traces suggest (S7 and S13 spirit):

- **A standby is a complete reader with a hold.** Kafka followers replicate by sending
  the same fetch requests consumers send. No separate replication subsystem (Q6).
- **Failover repair is backfill.** The old home's unreplicated tail returns as backfill
  with dedup (A6, A8, B7). No reconciliation subsystem (Q7).
- **A planned home move is a standby promotion.** Placement changes reuse failover with a
  graceful handoff instead of a lease lapse.
- **Node status is a connector.** `connector-status` writes `<node>.*` channels from
  values the layer-2 crates expose. No layer-2 crate writes channels (Q11).
- **Connector placement is the placement policy.** Connectors live in the name tree, so
  `[[placement]]` with a standby already covers connector failover (Q10).
- **Nodes are runtime membership**, like Kubernetes nodes that register themselves. The
  spec only refers to node names (Q11).
- **One blob store** serves spec branches and upgrade binaries.
- **`mesh.changes` is homed at the Raft leader.** Leadership change is a home move, and
  the seq is the Raft index, so continuity is free (Q11).
- **Upgrade is a spec change plus reconciliation**: desired version in the spec, a
  rollout lock in runtime. No separate upgrade orchestrator (Q18).
- **A whole-group rate change keeps its index.** Only moving channels between groups
  re-indexes (trace k).
- **Group run state is a command channel**, like Synnax's `sy_task_cmd`. No runtime
  operation and no new concept (Q14).
- **Calculations need no special case** in the connector contract: inputs are an out
  group, outputs an in group.
- **A synchronized device set is one connector** when its driver acquires it as one
  unit (Q15).

---

## 4. Open boundary questions, keystone first

Each is one decision with a recommendation and its evidence.

**Q1. Layer 2 order and the server loop.** Recommend the internal order
`time, transport, buffer, blob -> mesh -> home -> hub`, with `hub` serving incoming
connections by pulling them from `Transport::accept`. Evidence: trace (b) step 3 needs
an incoming path; without an order, `transport` calls `home` upward. Every other trace
depends on this answer.

**Q2. Split layer 1: `types` for values, `spec` for definitions.** `spec` holds the tree,
hashing, diff, and the one policy resolver; connector configs are opaque documents that
only their kind decodes. Evidence: problem 2; Rule 4's naming tell (`spec::Channel`,
`spec::resolve` read better than `types::ChannelDefinition`); one resolver means no crate
re-implements "most specific wins".

**Q3. `hub` is the whole layer-3 window.** It exposes reader and writer sessions,
read-only spec access and watches, mesh time, and pooled blocks. Evidence: traces (a)
step 1 and (d) step 4; C1 rule 1. The alternative, letting layer 3 reach `mesh` or
`time`, breaks C1 for the first connector.

**Q4. Encode once, validate at the owner.** A local write is encoded once by its home; a
remote write is encoded by the writer's `hub` and validated (not re-encoded) by the
home with `codec::validate`; readers decode once in their own `hub`. The home is the only
place that decides bytes are valid. Evidence: S2 (one byte format), problem 4, and the
fuzzing layer (T1) needs one trusted decode boundary.

**Q5. The actor owns the loop; kinds implement device hooks.** Replace C3's per-kind
`run` with `Kind::open` plus a `Device` with `start`, `read`, `write`, and `stop` per
group. The actor in `connector` owns the sessions (one writer per in group, one reader
per out group), pacing for polled kinds, restarts, status, run state, and the re-index
handover. Evidence: problem 29; the Synnax driver's `pipeline::Acquisition` already owns
the loop around `Source::read` (`driver/pipeline/acquisition.h:39-50`); the Kafka
Connect worker owns offsets, retries, and lifecycle around `SourceTask.poll()`; the
Telegraf agent owns the interval around an input's `Gather`. Deep module: one actor
hides the hard parts, and a kind is protocol code only. Trade: a push protocol that
wants its own event loop must block in `read` on its queue instead.

**Q6. A standby is a complete reader with a hold, and reader positions travel with the
data.** The standby subscribes to the home like any reader; the home writes reader
positions to a channel on the same index group, so the standby has them after failover.
Evidence: Kafka followers fetch like consumers; S13's "changing state is a channel"
pattern; trace (e) step 7.

**Q7. Failover is asynchronous, and the old home's tail returns as backfill.** Writes are
confirmed after the home's own disk sync. On failover, the new home starts at a fresh seq
block; when the old home returns, it sends its unreplicated tail as backfill, deduplicated.
Evidence: A6, A8, B7 already give every piece; Kafka with `acks=1` truncates that tail
instead, which loses data. Trade: during the outage, recent frames are missing until the
old home returns; synchronous replication can be a placement option later.

**Q8. Names decide governance; placement decides the home; leases follow both.** A node
holds a lease in every group whose indexes it homes. Evidence: A17 places calculations
past a weak link (a cloud node computing `site_a.pt_101`); "one lease per node" (S9)
cannot express that. CockroachDB holds one lease per range across many Raft groups.
Alternative: forbid homing outside the node's own branch, which is simpler but forces
names to follow placement.

**Q9. Index history is runtime state in `mesh`, sealed by the old home.** The spec
keeps only the current index. When a channel leaves an index, that index's home seals
it at the last accepted sample and proposes the seal. `mesh` stores the channel's epochs
(index plus seal) in the group that governs its name. Readers' `hub` stitches the epochs
into one stream. Data never moves. If the old home is down, the voters seal at its lease
end. A whole-group rate change is not a re-index. Evidence: trace (k) and problems 26
to 28; Apache Iceberg partition evolution leaves old data files in their old partition
spec, keeps every spec in table metadata, and splits query planning per spec, with no
rewrite. One owner, because a reader on any node must find every home that holds the
channel. Trade: one small piece of runtime state per re-index, and a channel's history
can sit on two homes until retention ends.

**Q10. Placement policy covers connectors.** A connector's `node` becomes a default
placement; a `[[placement]]` that selects the connector's name can add a standby, and
the supervisor on the standby starts it when the primary's lease lapses. Evidence: trace
(e) step 1; Kafka Connect moves connectors between workers on failure; connectors are
already in the name tree (C3).

**Q11. Nodes are runtime membership; status is a connector; owners co-locate what they
write.** Three linked rules:
- `join` registers a member in runtime state; the spec only refers to node names
  (Kubernetes kubelets register their own Node objects).
- `connector-status` writes every `<node>.*` channel from values that layer-2 crates
  expose; `mesh` never reads or writes data channels.
- The only channels an owner writes directly are its companions: `home` writes an
  index's control channel, which must be co-placed with the index; `mesh.changes` is
  homed at the group's Raft leader, with seq equal to the Raft index.
Evidence: problems 9, 10, 11, 15.

**Q12. Authenticate at the waist, authorize at the owner.** `hub` maps a peer key to a
subject; `home` authorizes read and write on its indexes; the branch's voters authorize
`apply`, `secret`, and `admin`. No other place checks. Evidence: S11 already puts control
at the home; root CLAUDE.md "no defense in depth"; a compromised node's `hub` cannot
bypass a check that runs at the owner.

**Q13. A quality channel may share its data's index.** When it does, a frame carries the
ack and its quality together, so they are atomic and ordered. S13's "own index" becomes
"any index". Evidence: trace (d) step 4 and B3's per-index order. A20's "write and wait
for ack" is unreliable without this.

**Q14. A group starts and stops through a channel, not an apply.** Each group gets a
run command channel and its ack under the connector's name (A20 pattern), gated by
control authority (S11). The spec's `stopped` boolean (default false) sets only the
state when the group is created; after that the latest command decides, so arming never
drifts from the files. Authority lets a test sequence hold arming so nothing else starts
acquisition. Evidence: trace (l) and problem 30; Synnax starts and stops tasks through
the `sy_task_cmd` channel (`core/pkg/service/task/service.go:197`); `kubectl scale`
writes the Kubernetes spec and fights GitOps, so Argo CD users add `ignoreDifferences`
on `/spec/replicas`; S13 ("changing state is a channel"). Detail for the interview: the
two channels' names and their index. Recommend a small per-connector index homed on the
connector's node, so arming works while the node is cut off from the voters.

**Q15. A synchronized device set is one connector; otherwise, two indexes.** When the
vendor driver acquires several devices as one unit, the connector's endpoint is that
set and each group maps to one acquisition. NI-DAQmx channel expansion puts modules
from several chassis in one task and synchronizes them automatically, for TSN cDAQ
chassis (cDAQ-9185 and cDAQ-9189 over IEEE 802.1AS) or chassis synced through the NI
9469. EtherCAT already works this way: one master, many slaves on distributed clocks.
One group, one writer, one index still holds. When the devices cannot be one acquisition
(different hosts or drivers), each gets its own connector and index with a shared start
trigger; timestamps then agree within each index's error bound, and readers join as-of
(S13). Never two writers on one index: per-index order, the gate, and positional series
alignment all assume one writer (A1, S11). Evidence: problem 31; NI's "Channel
Expansion Explained" and "C Series Multidevice Tasks" pages; the Synnax LabJack handle
race behind C3's one owner per endpoint. Trade: `devices` becomes a list for kinds that
support it.

**Q16. Secrets are their own state, sealed to every eligible node.** Ciphertexts live in
the governing group, outside the spec, changed only by `secret` operations. A value is
sealed to every node placement may run the connector on, standbys included. `discover`
takes credentials from the caller for one call and never stores them. Evidence: trace
(j) and problem 14; K4's write-only rule stays intact.

**Q17. Add an `ops` crate.** It holds the operation table and handlers, generates the CLI,
MCP tools, and docs, and runs each operation on the node that must run it. `cli` and `mcp`
become generated modules, not crates with their own logic. Evidence: C7, problem 19, and
traces (f), (g), (i).

**Q18. Versions travel with lease renewals; the desired version is in the spec.** A
rollout lock in runtime upgrades one node at a time, and finalization follows when every
member reports the version. Evidence: trace (h) and problem 16; it keeps `mesh` out of the
data plane (Q11).

**Q19. `sim` ships in the binary.** `plan --simulate` (C7) needs the simulated
environment and the protocol simulators at run time. Each protocol simulator lives in its
connector crate behind a feature that the product build turns on. Evidence: problem 17.
Trade: a larger binary; measure it against P1's footprint target.

**Q20. Wall time comes only from `time`; `env::Clock` gives only monotonic time and the OS
wall time that `time` itself reads.** Enforced by clippy `disallowed-methods` and the
architecture agent. Evidence: problems 18 and 23.

**Q21. Naming.** Rename S2's pooled bytes type to `Block` so `buffer` keeps the user's
"disk buffer" word, and decide whether `types` keeps its name once `spec` splits out
(`value` is the candidate). `hub` stays a parameter (C1). Evidence: problem 22 and the
namespace rule.

Questions that wait on research forks: the actor's thread model and whether `read`
blocks (C2, fork 1), `codec::validate` cost and the disk format's per-index range
records (S3 and S4, fork 2; that format must not grow a per-channel directory, which
Q9 gives to `mesh`), the definition language (K1, fork 3), Raft groups per branch and
their library (fork 4), and the `Transport` trait shape (fork 5).

Sources for Q15:
[NI-DAQmx channel expansion](https://www.ni.com/en/support/documentation/supplemental/15/channel-expansion-explained.html),
[C Series multidevice tasks](https://www.ni.com/docs/en-US/bundle/ni-daqmx/page/cdaqmultidevice.html),
[multi-chassis delta-sigma sync](https://digital.ni.com/public.nsf/allkb/54A717B91E8557D586257DF900244149).
