# R13: replication, failover, and reader positions (trade study)

Research fork 13, 2026-10-04. Scope: boundary questions Q6, Q7, Q8, and Q10 from r8, with
a real comparison of alternatives. Inputs: the decision log (A1-A20, B1-B7, P1, T1,
S1-S13, K5, C1-C9, BQ1-BQ5, SRP pass), r1, r2, r4, r5, r8. Model and results:
`scratchpad/r13/` (`model.py`, `results.md`, `fsync_bench.py`).

Marks: **[1]** one source only, **[U]** unverified or my estimate, **[M]** produced by
the model in this fork. Claims with no mark have two or more sources, or come from a
locked decision.

---

## 0. Summary

**Recommendation.** Keep the shape of the current proposal, with five changes. The home
copies each index to its standby asynchronously. The copy path is its own component.
Takeover is the home's ordinary crash-recovery path plus one fence check.

1. **Copy (Q6).** The standby receives each index's log records as stored bytes, after the
   home's sync. The records are data, reader positions, control handoffs, and backfill
   dedup marks, in index order. The copy reuses the complete-reader parts (`delivery`
   cursor, credits, hold, catch-up from disk). It is not a `hub` reader session, and it
   never decodes.
2. **Durability (Q7).** Live writes never wait (B5). The home publishes two watermarks
   per index: *stored* (on its disk) and *replicated* (on the standby's disk). The writer
   chooses which one confirms its frame. A remote SDK writer keeps frames until they are
   replicated, within a memory bound. The voters promote the standby when the home's lease
   lapses, however far behind it is. A returning old home sends its durable tail as
   deduplicated backfill. No per-placement durability setting, no in-sync set.
3. **Reader positions.** `delivery` owns positions and writes them as log records that
   travel in the copy stream, so they never get ahead of the data. A connected reader's
   `hub` presents its own position when it resumes at a new home. Status channels show
   positions for visibility only.
4. **Leases (Q8).** One lease per node, in the group of the node's own branch. The
   runtime record "index I is homed on X, failed over to S" lives in that group too.
   The standby must be in the home node's branch. Names stay free of placement.
5. **Connectors (Q10).** One placement covers a connector and every index under its
   name. The standby connector starts cold, only after the voters commit the move. The
   old node stops its connectors at the same fence where it stops accepting writes.

**Failure-mode headline [M].** On a LAN with NVMe disks, every alternative loses about
the same data when a healthy pair fails: under 3 ms at p99. The alternatives differ only
when the standby is behind or cut off when the home dies:

| Standby state when the home node is destroyed | Async (recommended) | Kafka ISR | Raft, 3 copies |
|---|---|---|---|
| 2 s behind | 2 s lost (local writer); 0 (remote writer) | 2 s lost, then unavailable until an operator acts | 0.5 ms |
| Cut off for 20 s | 20 s lost (local); 10 s (remote, hold bound) | 20 s lost (19 s of it confirmed), then unavailable | 0.5 ms |

Only a third data copy protects a local writer from a standby that is behind. An edge
site with two gateways cannot afford one. The recommended design makes that exposure
visible (the replicated watermark is a status channel) instead of hiding it.

**The second finding.** Sending to the standby after the home's sync costs little on
NVMe (2.2 ms against 0.5 ms lost per crash) [M]. On a Raspberry Pi 4 SD card it costs
1.6 s of data per crash, because the SD card's sync takes about 250 ms [1] [M]. Decision
6 covers it.

---

## 1. What constrains the answer

From locked decisions:

- **B4:** latest readers get frames before the disk sync. So no replication scheme sits on
  the latest-mode path, and the 250 us p99 target (P1) does not depend on this choice,
  as long as replication work stays off the latest fan-out on the shard.
- **B5:** live writes never wait. A home never blocks or refuses a live frame because of
  replication state. This rules out every scheme where acceptance needs a quorum.
- **A8:** one `u64` seq per index. A home with a standby reserves seq blocks from the
  voters. A new home starts at the next block. A restart continues from the last seq on
  disk, because complete readers get only durable frames.
- **B7:** backfill uploads carry frame numbers, and the home drops repeats. A writer may
  resend unconfirmed live data as backfill.
- **B3:** delivery is at-least-once. Duplicates are inside the contract.
- **S9 and r4:** fast state stays out of Raft. Voter groups sit in one network locality.
  Votes never cross Starlink.
- **S11:** the home's gate is the only control check. Unknown control state means "not
  in control".
- **T1:** every component takes clock, network, disk, and randomness as inputs.
- **K5 simplicity directive:** express new needs with existing concepts.

Two structural facts frame the study:

1. **f+1 data copies or 2f+1.** To survive one failure, a quorum protocol (Raft, VSR,
   Paxos) needs three data copies. Primary-backup with an outside configuration master
   needs two data copies plus a small voter group. Kafka's design notes say the same: a
   majority vote "requires three copies of the data" to tolerate one failure, and they cite
   PacificA as their closest academic relative. Vertical Paxos (Lamport, Malkhi, Zhou,
   PODC 2009) proves that primary-backup with a configuration master is a correct Paxos
   variant. Foundation's mesh already has the voters (S9).
2. **Time-series data has no conflicting writes.** One writer per index, append only, so
   two copies can only differ by missing ranges. A copy's extra suffix can always merge
   back as backfill (A6, B7). In Kafka or Raft, an "unclean" election loses the old
   leader's suffix, because those systems truncate it. In Foundation the same election
   only delays that suffix, if the old home's disk survives. GitHub's 2018 outage is the
   counter-example in a database with updates: 43 seconds of unreplicated writes needed
   manual reconciliation, still in progress when GitHub published its analysis nine days
   later.

---

## 2. The alternatives

Each block: how it maps onto Foundation, evidence, the criteria, and seating. Seating
splits the **copy path** (how bytes reach a second disk) from the **takeover path** (who
decides a new home, fencing, seq, gate, positions, routing).

### A1. Current proposal: the standby is a complete reader with a hold, async

**How it works.** The standby subscribes like a complete reader. It gets frames after
the home's sync. The writer's confirmation means "on the home's disk". Positions are a
channel. Failover is a lease lapse. The old home's tail returns as backfill.

**Evidence.** Kafka followers "consume messages from the leader like a Kafka consumer
would" (Kafka design docs). Kafka "does not require that crashed nodes recover with all
their data intact" and relies on other replicas instead of an fsync per write.
Foundation syncs before it confirms (r2), so each Foundation copy is safer than a Kafka
acks=1 copy.
PostgreSQL's async streaming: "the amount of data loss is proportional to the replication
delay at the time of failover" (PostgreSQL docs).

**Criteria.**
- P1: one more complete-reader stream per index. P1 already budgets "disk buffer + one
  complete reader", so the standby doubles that stream's egress: about 3.2 Gbit/s at
  100M samples/s and 4 bytes per sample [U]. Off the latest path.
- Pi 4: no extra disk at the home; one outgoing stream. The Pi's 1 Gbit NIC caps a copy
  at about 30M samples/s at 4 bytes per sample [U].
- Losses: see section 3. Healthy pair: 2.2 ms per crash on NVMe, 1.6 s on SD [M].
- Split brain: lease fence (r8 trace e). The home stops at lease end minus the clock error
  bound; the voters wait until lease end plus the bound. Leases need a bounded clock
  rate, not synchronized clocks (Gray and Cheriton, SOSP 1989, not fetched here).
- Gate: rebuilt from reconnects. The equal-authority tie rule breaks across failover
  (whoever reconnects first wins).
- B7 and A8: blocks work as designed. Backfill dedup state is not copied, so a resent
  backfill frame after failover is a duplicate.
- Weak links: tolerant (catch-up from disk; never stalls the home).
- DST: small state space.
- Simplicity: no new concept.
- **Seating.** The proposal says "a complete reader", which suggests a layer-3 component
  on `hub`. That does not work, for four reasons:
  1. `hub` decodes for readers (BQ4 proposal), so the standby re-encodes before storing.
     That breaks S2 ("store as received").
  2. A layer-3 component cannot call `buffer::append` (C1 rule 1). Storing replica bytes
     needs a new hole in `hub`.
  3. Backfill dedup marks are not visible to readers.
  4. A position channel needs its own index. Order across indexes is not guaranteed
     (B3), so a copied position can be ahead of the copied data. Kafka needed KIP-320 to
     detect exactly that.

  Copy path: separable in layer 2, not in layer 3. Takeover: woven (fence, seq block,
  gate, routing).

**Verdict.** Right direction. Fix the seating, the confirmation semantics, positions, and
the gate rebuild. That is A1R, the recommendation.

### A1R. Recommended: async copy with a replicated watermark

**How it works.** Same data path as A1, with these changes:

- The copy carries the index's log records, not reader output.
- The home publishes *stored* and *replicated* watermarks. The replicated watermark is
  the standby's acked position, which `delivery` already tracks for every holding
  reader.
- The writer picks the confirmation it waits for. The SDK default for a remote writer:
  keep frames until replicated, up to a memory bound.
- Promotion never checks the standby's lag. The returning tail fills the hole as
  backfill, numbered by its original live seq, so readers drop what they already saw.
- No automatic failback.

**Evidence.** Per-writer durability over one replication mechanism is how PostgreSQL
("`synchronous_commit` can be set ... dynamically by applications ... on a
per-transaction basis") and Kafka (producer `acks`) mix durability levels. Neither runs two
replication protocols. The "unclean promotion, re-merge later" rule has an industrial
precedent: Ignition's backup gateway in Partial history mode caches history during a
failover, and when the master returns "only the data that was collected while the master
was truly down is forwarded". Ignition states the root problem plainly: "it is never
possible for the backup node to know whether the master is truly down or simply
unreachable."

**Criteria.** As A1, plus:
- Confirmation p99 for a writer that waits for *replicated*: 4.3 ms on NVMe, 17 ms on a
  laptop SSD, about 2 s on a Pi SD card [M]. Complete readers stay at the stored
  watermark (2.2 ms on NVMe) [M]. Nothing else waits.
- Remote writer: zero loss whenever its hold covers the standby's lag. Local writer:
  same as A1, but every loss is visible, because nothing was confirmed as replicated.
- Gate: the new home reads the last holder from the copied control channel (section 6).
- B7: dedup marks travel in the copy stream, so dedup survives failover. A8 unchanged.
- DST: the copy is a cursor and a credit window (sans-I/O). Takeover reuses the restart
  path, which DST must cover anyway.
- Simplicity: one new idea, a second confirmation level. No ISR, no per-placement mode.
- **Seating.**
  - Copy path: a separate layer-2 component (`replica`) built only on the public
    surfaces of `delivery` (send side), `buffer` (`append` on the receive side), and
    `transport`. It never calls `home` and never sees the gate.
  - Takeover path: woven, but small:
    1. One fence check in `home`'s write path. A8 needs it anyway.
    2. `home::open(index, block)`, which is crash recovery. Promotion is a restart on
       another disk.
    3. The promotion rule in `mesh`.
    4. Routing in `hub`, which already follows `mesh::watch_homes`.

**Verdict.** Recommended. Trade: a local writer's data is unprotected for as long as the
standby is behind. The watermark shows that window; it does not close it.

### A2. Raft (or VSR) per index or per index group

**How it works.** Each index (or a group of indexes on one home) is a Raft log with three
replicas. The leader is the home. Commit means a majority synced. Seq could be the Raft
index. Leadership replaces leases for data.

**Evidence.**
- Redpanda: "Every topic partition forms a Raft group." Commit is acknowledged "when a
  majority (quorum) of responses have been received".
- CockroachDB runs Raft per range. It moved from per-range leases to epoch-based leases
  on node liveness, then to leader leases, because per-range lease upkeep costs CPU. Leader
  leases cut lease CPU "by up to 85%" [1].
- NATS JetStream runs a Raft group per stream and per consumer, plus a meta group.
  Synadia warns that 1,000 or more replicated assets on a server degrade the cluster [1].
- Jepsen, NATS 2.12.1: JetStream lost 131,418 of 930,005 acknowledged messages in
  simulated power failures, because it flushed every two minutes instead of before the
  ack. Raft is only as durable as its sync rule.
- Redpanda 24.1 added "write caching", a relaxed acks=all that acks before the sync. That
  is a sign the sync per commit is the main cost.
- Aeron Cluster is Raft: services consume the log "once a majority of the cluster members
  have safely recorded" it [1].
- TigerBeetle (VSR) uses flexible quorums: six replicas, commit with three, view change
  with four.

**Criteria.**
- P1: three data copies, so twice the leader's egress (about 6.4 Gbit/s at P1). One QUIC
  connection carries 2.4 to 8.2 Gbit/s on one core (r5). Confirmation p99 is close to
  A1's on a LAN (2.25 ms against 2.19 on NVMe) [M], because the leader sends before its
  own sync (etcd/raft supports async storage writes; r4 lists its
  `async_storage_writes_append_aba_race` scenario).
- Pi 4 and edge: a third data copy at every site. A two-gateway site cannot run it
  without a third data node. A vote-only witness does not help, because Raft appends go to
  every voter. MongoDB documents the same pain for primary-secondary-arbiter sets: with a
  lagging secondary, majority writes stall.
- Losses: about 0.5 ms in every single failure, including a lagging or cut-off follower
  [M]. Best of all options.
- B5: violated. A leader without a quorum steps down (CheckQuorum, required by r4), and
  the index stops accepting live writes. Keeping acceptance with no quorum means
  "accept now, commit later", which is A1R with extra machinery.
- Split brain: terms and quorum. Strong.
- Gate: exact, because the gate is replicated state.
- B7 and A8: the Raft index could replace seq blocks, which revises A8.
- Weak links: r4 forbids votes across Starlink, so all three replicas must share one
  locality.
- DST: etcd's scenario files apply. But thousands of groups per node need heartbeat
  batching and quiescence, the multi-raft machinery r4 rejected.
- Simplicity: adds terms, quorums, and followers per index, beside the mesh's own Raft,
  so there are two leader mechanisms.
- **Seating.** Woven into the center. The Raft log becomes `home`'s write path and
  `buffer`'s log format. The gate and positions become replicated state. Leadership
  replaces node leases for data. The copy and takeover paths cannot be separated.

**Verdict.** Rejected as the default. It is the only option that protects a local writer
from a lagging copy, but it needs three data copies, breaks B5 under quorum loss, and
rewires the core.

### A3. Kafka ISR (acks, min.insync.replicas, unclean election, KIP-966)

**How it works.** Commit means all in-sync replicas synced. The home drops a lagging
standby from the in-sync set through the voters before it confirms without it. A3 uses
min.insync = 1 (B5 forbids rejecting writes) and no unclean election.

**Evidence.**
- Kafka: "A write to a Kafka partition is not considered committed until all in-sync
  replicas have received the write." A committed message is not lost "as long as there
  is at least one in-sync replica alive". With f+1 replicas, f failures are tolerated.
- Since 0.11.0.0 Kafka waits for a consistent replica instead of an unclean election.
- Elasticsearch, also PacificA-based: "Only once removal of the shard has been
  acknowledged by the master does the primary acknowledge the operation." Elasticsearch
  also notes "A single slow shard can slow down the entire replication group".
- KIP-966 (Eligible Leader Replicas, default on new clusters from Kafka 4.1) exists
  because an ISR of one that loses its page cache becomes leader again and makes the
  other replicas truncate committed data (Vanlightly).

**Criteria.**
- P1: as A1R, but complete readers wait for the high watermark: 2.7 ms p99 on NVMe, and
  152 ms when the standby is across Starlink [M].
- Losses: when the standby has dropped out and the home dies, data confirmed by the home
  alone is lost, and the index waits for an operator [M]. Re-merge makes that strictly
  worse than A1R, because A1R promotes and loses the same data but stays available.
- Flapping: a flapping home-to-standby link at 3 s up and 2 s down costs 144 voter
  commits and 96 s of stalled confirmations in 10 minutes with a 1 s lag bound, or 240 s
  stalled with a 10 s bound [M]. Every stall also stalls complete readers.
- Seating: the copy path is separable like A1R. Takeover is deeper: the in-sync set
  lives in `mesh`, the high-watermark gate in `delivery`, and truncation or re-merge in
  `buffer`.

**Verdict.** Rejected. The in-sync set only buys "refuse to promote a stale standby".
With re-merge, that refusal trades availability for nothing.

### A3U. Kafka acks=1 with unclean election

Async like A1, but the returning old leader truncates its divergent suffix. Same numbers
as A1, except the delayed tail becomes lost [M]. It is the baseline that shows what
re-merge buys. Rejected.

### A4. Source-side fan-out (PI n-way buffering, Ignition store-and-forward)

**How it works.** The writer sends every frame to the home and to the standby itself and
keeps a queue per member. The members are independent.

**Evidence.**
- PI: "All interfaces write time-series data directly to members of the collective,
  buffering data temporarily for those unable to receive it" (tdworld, 2009) [1].
- The PI buffering service "queues data independently to each server in a Data Archive
  collective" (AVEVA docs). Only configuration replicates from the primary.
- Apache BookKeeper is the database form: the writer writes each entry to Qw bookies and
  waits for Qa acks.
- Facebook LogDevice: a sequencer writes each record to a copyset of storage nodes.

**Criteria.**
- A remote writer has zero write outage and zero loss in every single failure [M],
  because there is nothing to fail over.
- A local writer (the common case, B7) keeps its standby queue on the home node, so it is
  no better than A1R (20 s lost in the cut-off case) [M].
- The writer sends twice: a Pi connector pays double egress and double encryption.
- Two independent members means no single seq and no single gate. A8, S11, and the
  "one home" rule (A1) all need a leader, so control channels need a home anyway. That
  makes two mechanisms.
- Members can drift. AVEVA warns that collective members can hold different records
  because of server-side compression [1].
- **Seating.** The copy path moves into `hub` writer sessions (every writer knows every
  member). Seq moves to the writer. Each member needs dedup. Takeover is avoided for data,
  but the gate still needs a home.

**Verdict.** Rejected as the replication mechanism. Kept as a pattern: a remote writer
keeping frames until they are replicated is A4's good half, and A1R adopts it.

### A5. Chain replication and CRAQ

**How it works.** Writes enter at the head, flow down the chain, and commit at the tail.
A master (here, the voters) relinks the chain after a failure. CRAQ lets any member serve
reads of clean objects.

**Evidence.** Van Renesse and Schneider (OSDI 2004): a fail-stop design with a separate
master. Terrace and Freedman (USENIX ATC 2009): CRAQ.

**Criteria.**
- Each node sends once, so egress per node stays at one copy.
- Latency grows with chain length: 2.8 ms p99 for three nodes on NVMe, 152 ms when a link
  crosses Starlink [M].
- A slow member slows every confirmation (2 s of the local writer's data lost in the lag
  case) [M], and the chain blocks until the master removes the member.
- B5: a blocked chain must either buffer without bound or refuse writes.
- CRAQ's read benefit is already in the design: A1 says reads are served from any copy.
- Seating: woven. Every member is a receiver and a forwarder in the write path, and the
  master relinks the chain.

**Verdict.** Rejected. It brings no gain for a two-copy edge pair, and the chain latency
grows over weak links.

### A6. Synchronous primary-backup (Ignition redundancy, Aeron Cluster, semi-sync)

**How it works.** The confirmation waits for the standby's ack. If the standby stays
silent past a timeout, the primary falls back to async (MySQL semi-sync: "the source
reverts to asynchronous replication", default timeout 10 s).

**Evidence.**
- PostgreSQL synchronous commits "may never be completed if any one of the synchronous
  standbys should crash".
- Ignition redundancy shares runtime state "on a differential basis so that the backup
  can take over with the same state that the master had".
- Aeron Cluster is quorum-based, so it belongs with A2.

**Criteria.**
- Steady state is like A3 [M].
- The silent fallback is the danger. After the timeout, writers get local-only
  confirmations without knowing it. In the cut-off case that is 10 s of confirmed data
  lost [M].
- Without the fallback, B5 breaks.
- Seating: the copy path is separable. The confirmation gate is woven into `home` and
  `delivery`, with a fallback timer.

**Verdict.** Rejected. A1R gives the same writer guarantee with no fallback state:
the writer sees the replicated watermark stop moving.

### A7. Leaderless quorum writes with seq dedup (Dynamo, Cassandra)

**How it works.** The writer sends each frame to N replicas and waits for W acks.
Writer-assigned seq makes merges idempotent. Read repair and anti-entropy fill gaps.
Sloppy quorums with hinted handoff keep writes available.

**Evidence.**
- Dynamo (SOSP 2007) is "always writeable" through sloppy quorums and hinted handoff.
- Cassandra uses last-write-wins on timestamps. Jepsen showed lost updates and a
  vulnerability to clock skew. Seq dedup would avoid that for single-writer indexes.

**Criteria.**
- Best loss numbers with a remote writer (zero) [M].
- Three data copies, and the writer sends two or three copies.
- A latest reader must trust one replica or contact several, which threatens the 250 us
  target.
- No single seq or gate (as A4).
- Anti-entropy, hinted handoff, and read repair are three new components with large state
  spaces.
- Seating: replaces `home`, the center of the design.

**Verdict.** Rejected. It solves a multi-writer conflict problem Foundation does not
have, at the cost of the one-home model.

### A8. Per-placement durability setting (async default, quorum option)

**How it works.** A placement policy picks async (A1R) or quorum (A2) per index.

**Criteria.** The union of A1R and A2. Two copy paths, two takeover paths, two sets of
DST invariants, and an explanation of which setting gave which guarantee. The quorum
mode still breaks B5 on quorum loss. Kafka and PostgreSQL mix levels per writer over one
mechanism instead. Seating: both A1R's and A2's.

**Verdict.** Rejected for v1. A1R's per-writer confirmation gives most of A8's
flexibility with one mechanism. If a real need for a third copy appears, the narrow path
is a second standby under A1R (the replicated watermark becomes "on k copies",
PostgreSQL's `ANY k`), not a second protocol.

---

## 3. The model

`scratchpad/r13/model.py` is a Monte Carlo model, not a protocol simulator. For each
alternative, it encodes four rules:

1. when a frame is confirmed to its writer
2. when a complete reader may see it
3. which copies hold it when a failure starts
4. what takeover does with copies that disagree

Random inputs:
- sync latency per disk profile: NVMe with power-loss protection, about 0.1 ms [U]; this
  machine's SSD, `F_FULLFSYNC` p50 4.0 ms and p99 4.2 to 6.0 ms, measured with
  `fsync_bench.py`; Pi 4 SD card, about 250 ms [1]
- round-trip time: LAN 0.2 ms median [U]; Starlink 45 ms median with outliers over
  100 ms (r4)
- the failure's phase inside the group-commit cycle

Every alternative uses the same 3 s detection time, so the results compare protocol
structure, not vendor defaults. Group commit runs every 2 ms (B1). A remote writer keeps
up to 10 s of unconfirmed frames. Every alternative except A3U re-merges a returning old
home's durable suffix, because Foundation can add re-merge to any protocol.

**Limits.** The model does not simulate packets, credits, CPU, or real Raft elections.
It ignores catch-up time after a partition heals, and it uses single-source disk
numbers for the Pi. Its job is to rank the alternatives and size the windows. The
benchmark suite (T1 layers 5 and 6) must measure the real numbers.

### 3.1 Steady state [M]

Confirmation p99 and complete-reader p99 in ms. Writer on the home node. For A1R, the
first number is the *replicated* confirmation; *stored* equals A1.

| Alternative | NVMe, LAN | Laptop SSD, LAN | Pi 4 SD, LAN | NVMe, standby over Starlink |
|---|---|---|---|---|
| A1 async reader | 2.2 / 2.2 | 9.5 / 9.5 | 1283 / 1283 | 2.2 / 2.2 |
| A1R async + watermark | 4.3 / 2.2 | 17.4 / 9.5 | 1969 / 1283 | 153 / 2.2 |
| A2 Raft, 3 copies | 2.3 / 2.3 | 9.5 / 9.5 | 1288 / 1288 | 89 / 89 |
| A3 ISR acks=all | 2.7 / 2.7 | 10.0 / 10.0 | 1445 / 1445 | 152 / 152 |
| A4 source fan-out | 2.7 / 2.2 | 10.0 / 9.5 | 1445 / 1283 | 152 / 2.2 |
| A5 chain, 3 | 2.8 / 2.8 | 10.4 / 10.4 | 1552 / 1552 | 152 / 152 |
| A6 sync primary-backup | 2.7 / 2.7 | 10.0 / 10.0 | 1445 / 1445 | 152 / 152 |
| A7 leaderless W2/N3 | 2.2 / 2.2 | 8.6 / 9.5 | 863 / 1283 | 89 / 2.2 |

Reading:
- On a LAN, synchronous options cost about 0.5 ms at p99 over async. The cost is not in
  latency; it is in B5, copy count, and seating.
- Over a weak link, synchronous options put the link's tail on every complete reader.
  A1R and A4 keep complete readers on the home's disk.
- A1R's *replicated* confirmation is the slowest on fast disks, because it waits for two
  syncs in series. Only writers that ask for it pay.

### 3.2 Failure-mode matrix [M]

Rows: alternatives. Columns: failures. Each cell: data lost (p99, as ms of the index's
data), then time with no accepting home (p50). "conf" marks data that was confirmed to the
writer. "late" marks data that only waits for the old home to return. Local writer
(connector on the home node, dies with it), NVMe, LAN, 3 s lease.

| Alternative | Home crash, disk survives | Home destroyed | Home cut from voters, reaches standby | Home cut from standby 20 s, then destroyed | Standby 2 s behind, then home destroyed | Home and standby down 60 s |
|---|---|---|---|---|---|---|
| A1 | 2.2 ms, 0.5 ms late; 3.1 s | 2.4 ms (0.5 conf); 3.1 s | 0, 0.5 ms late; 0.1 s gap | 20 s conf; 3.1 s | 2 s conf; 3.1 s | 2.2 ms; 60 s |
| A1R | 2.2 ms, 0.5 ms late; 3.1 s | 2.4 ms; 3.1 s | 0, 0.5 ms late; 0.1 s gap | 20 s; 3.1 s | 2 s; 3.1 s | 2.2 ms; 60 s |
| A2 Raft | 0.5 ms; 3.1 s | 0.5 ms; 3.1 s | 0; none | 0.5 ms; 3.1 s | 0.5 ms; 3.1 s | 0.5 ms; 60 s |
| A3 ISR | 0.5 ms; 3.1 s | 0.5 ms; 3.1 s | 0; 0.1 s gap | 20 s (19 s conf); until operator | 2 s (1 s conf); until operator | 2.2 ms; 60 s |
| A3U | 2.3 ms (0.5 conf); 3.1 s | 2.4 ms (0.5 conf); 3.1 s | 0.5 ms; 0.1 s gap | 20 s conf; 3.1 s | 2 s conf; 3.1 s | 2.2 ms; 60 s |
| A4 fan-out | 0.5 ms; 3.1 s | 0.5 ms; 3.1 s | 0; none | 20 s; 3.1 s | 2 s; 3.1 s | 2.2 ms; 60 s |
| A5 chain | 0.5 ms; 3.1 s | 0.5 ms; 3.1 s | 0; 0.1 s gap | 0.5 ms; 3.1 s | 2 s; 3.1 s | 2.2 ms; 60 s |
| A6 sync PB | 0.5 ms; 3.1 s | 0.5 ms; 3.1 s | 0; 0.1 s gap | 20 s (10 s conf); 3.1 s | 2 s; 3.1 s | 2.2 ms; 60 s |
| A7 leaderless | 0.5 ms; 3.1 s | 0.5 ms; 3.1 s | 0; none | 0.5 ms; 3.1 s | 0.5 ms; 3.1 s | 0.5 ms; 60 s |

Remote writer (an SDK on another machine; keeps up to 10 s unconfirmed). Only the cells
that change:

- A1: 0.5 ms conf (destroyed), 20 s conf (cut off), 2 s conf (behind).
- A1R: **zero in every column** except "cut off 20 s": 10 s lost, none confirmed as
  replicated, limited by the writer's hold.
- A2, A5, A7: zero everywhere. A4 and A7 also have no write outage, because the writer
  keeps writing to the other member.
- A3: 19 s conf (cut off), 1 s conf (behind), both until an operator acts.
- A6: 10 s conf (cut off).

Pi 4 SD card, local writer: the async options lose about 1.6 s per crash. Options that
send on receipt lose about 0.5 ms (A2, A3, A4, A5, A6, A7). Everything else matches the
NVMe table.

**Flapping home-to-voters link [M]** (600 s, mean of 50 runs; lease renewed every
lease/3). With automatic failback, home moves ping-pong:

| Link up / down mean | Lease | Failovers, no failback | Failovers, automatic failback |
|---|---|---|---|
| 20 s / 0.5 s | 1 s | 1.0 | 11.6 |
| 20 s / 0.5 s | 3 s | 0.1 | 0.5 |
| 3 s / 2 s | 3 s | 1.0 | 68.7 |
| 15 s / 1.5 s | 3 s | 1.0 | 13.3 |
| 15 s / 1.5 s | 10 s | 0.2 | 0.3 |

Each failover costs one seq block, one gate rebuild, and about 0.1 s of gap. Without
failback, an incident costs at most one move. The ISR churn numbers are in A3.

**Both down.** Every alternative is unavailable until a node returns. Two-copy schemes
lose the home's unsynced window when a local writer dies with it. Recovery needs no
operator, because the returning home still holds its lease record and its disk.

---

## 4. Comparison

"Copies" means data copies needed to survive one node failure. "B5" means "can live
writes keep flowing in every failure where some node is alive?".

| Alternative | Copies | Confirm p99, NVMe / Pi SD | Worst single-failure loss, local writer | B5 | Gate after failover | Concepts added | Seating (copy / takeover) | Verdict |
|---|---|---|---|---|---|---|---|---|
| A1 proposal | 2 + voters | 2.2 / 1283 ms | lag window, confirmed | yes | race on ties | none | separable in layer 2 (not via `hub`) / woven | fix, becomes A1R |
| **A1R** | 2 + voters | 2.2 stored, 4.3 replicated / 1283, 1969 ms | lag window, never confirmed as replicated | yes | from the copied control channel | one ack level | **separate `replica` component / crash-recovery path + fence** | **recommend** |
| A2 Raft | 3 | 2.3 / 1288 ms | 0.5 ms | no (quorum loss) | exact | terms, quorum, followers per index | one woven unit; rewires `home` and `buffer` | reject |
| A3 ISR | 2 + voters | 2.7 / 1445 ms | lag, confirmed, then unavailable | yes (min.insync 1) | from channel | in-sync set, high watermark | separable / deeper (`mesh`, `delivery`, `buffer`) | reject |
| A3U | 2 + voters | 2.2 / 1283 ms | lag, confirmed, truncated | yes | from channel | unclean flag | separable / woven + truncation | reject |
| A4 fan-out | 2 | 2.7 / 1445 ms | lag (local queue) | yes | needs a separate leader | per-member queues, writer seq | moves into `hub` writers / none for data, leader for gate | reject; keep "hold until replicated" |
| A5 chain | 3 (or 2) | 2.8 / 1552 ms | lag window | no (blocked chain) | from channel | chain order, relink | woven into the write path | reject |
| A6 sync PB | 2 + voters | 2.7 / 1445 ms | lag, part confirmed after fallback | only with fallback | from channel | wait, fallback timer | separable / commit gate woven | reject |
| A7 leaderless | 3 | 2.2 / 863 ms | 0.5 ms | no (W unreachable) | needs a separate leader | N/W/R, hinted handoff, read repair, anti-entropy | replaces `home` | reject |
| A8 mixed | 2 or 3 | either | either | partly | either | both protocols | both | reject for v1 |

---

## 5. Seating: can replication stand apart from the core?

The coordinator asked how deep replication must reach and whether it can sit behind a
clean boundary. Short answer: **the copy path can; the takeover path cannot, but it can
shrink to the crash-recovery path the home needs anyway.**

### 5.1 What each path must touch

| Internal | Copy path needs it? | Takeover path needs it? |
|---|---|---|
| Seq assignment | No. Seq travels in the stored bytes. | Yes: a new home starts at a new block (A8). |
| Seq blocks | No. | Yes: reserved in `mesh` at promotion. |
| Control gate | No. The control channel is copied like any channel. | Yes: the new gate starts from the last holder. |
| Disk buffer | Yes: `append` on the standby, `read` on the home. | Yes: `home::open` reads it. |
| Fencing and leases | No. | Yes: one check in `home`'s write path, the decision in `mesh`. |
| Reader positions | Yes: they must travel with the data. | Yes: `delivery` loads them at open. |
| Routing in `hub` | No. | Yes: `hub` follows `mesh::watch_homes` (it does already). |
| Backfill dedup marks | Yes: they travel with the data. | Yes: loaded at open. |

### 5.2 Precedents for replication outside the core

- **Litestream** ships SQLite's WAL from a separate process. It is "intended as a
  single-node, disaster recovery tool"; it "cannot replicate data to other live servers
  and it does not support automatic failover" (LiteFS FAQ). To get failover, Fly built
  LiteFS, which sits in the write path through FUSE. That limits write throughput "to
  about 100 transactions per second" and needs a Consul lease. **Lesson:** an outside
  copy is easy; takeover had to move inside, and doing it through an interception layer
  cost performance.
- **PostgreSQL logical replication** runs through output plugins on the public decoding
  API. Physical streaming replication is in the core. Logical replication does not carry
  sequences ("Sequence data is not replicated") or DDL. Until PostgreSQL 17, a failover
  lost the subscribers' slot positions, because slots existed only on the primary.
  PostgreSQL 17 syncs logical slots through the physical (inside) path. **Lesson:** an
  outside copy misses internal counters (Foundation's seq blocks and dedup marks) and
  reader positions, and positions had to be carried inside to survive failover.
- **Kafka MirrorMaker 2** is a Kafka Connect connector. It cannot keep offsets the same
  across clusters, so it translates them through checkpoints. Consumers on the target can
  lag by up to 100 offsets and see re-delivery (Aiven known issues [1]; KIP-382).
  **Lesson:** an outside copy changes positions.
- **Debezium and Kafka Connect** keep source offsets in a Connect topic, flushed
  periodically, so a crash re-emits events (at-least-once). **Lesson:** positions kept
  apart from the data lag the data.
- **NATS JetStream mirrors** are built on the consumer path. They are read-only and
  one-way, and the publisher is acked "before the mirror has it". They serve disaster
  recovery, not automatic failover.
- **Kafka followers** use the consumer Fetch protocol for the copy (outside-shaped). The
  takeover machinery (ISR in the controller, leader epochs, high watermark, truncation)
  is inside the broker. **This is the split the recommendation copies.**
- **Ignition and PI** both keep copy and takeover apart:
  - Ignition syncs configuration and runtime state between the gateways, but its backup
    decides takeover on its own.
  - PI copies data through the interfaces' n-way buffers. Interface failover (UniInt)
    is a separate mechanism with hot, warm, and cold modes; warm and cold "might" lose
    data.

### 5.3 Verdict for the recommended design

- **Copy path: a separate component.** `replica` sits in layer 2, beside `home`, and
  depends only on `delivery`, `buffer`, `transport`, and `wire`. `home` does not know it
  exists: on the sending side the standby is one more holding cursor in `delivery`, and
  on the receiving side it is a caller of `buffer::append`. Dependency direction: the
  standby pulls (as Kafka followers fetch), so `replica` depends on the home's public
  stream, never the reverse.
- **Takeover path: woven, at three narrow points.**
  1. `home` checks the fence on each write. A8 already needs this.
  2. `home::open(index, block)` is crash recovery. Promotion is "restart on another
     disk", so takeover needs no separate code path.
  3. `mesh` decides promotion from the lease.
  `control` and `delivery` receive their starting state (last holder, positions) as
  inputs at open. They never learn that a failover happened.
- **What a fully outside design (a standby as a layer-3 `hub` reader) would cost:**
  - one decode and one re-encode per frame (against S2)
  - a new `hub` hole to store replica bytes
  - backfill dedup lost across failover
  - positions on another index that can be ahead of the data
  - promotion would still need the three woven points above

  Correctness and performance both lose, for no gain in separation.

---

## 6. Reader positions and the gate

### 6.1 Where positions live

| Option | Re-delivery after failover | Cost | Seating | Precedent |
|---|---|---|---|---|
| In the voters' Raft state | none if every ack commits | one Raft write per ack; breaks S9 "fast state out of Raft" | `delivery` depends on `mesh` per ack | Kafka offsets in ZooKeeper, abandoned in 0.8.2 because "ZooKeeper writes are expensive" [1] |
| A channel on another index | up to the position lag; can be ahead of the data | a home-written index per reader set | `delivery` writes channels (upward, the Q11 problem) | Kafka `__consumer_offsets`, which needed KIP-320 |
| A Raft group per reader | none | one group per named reader | a new group type | JetStream consumers; Synadia warns at 1,000+ assets [1] |
| On the reader side only | none for connected readers | the home cannot hold data for an absent reader | `hub` only | Kafka Connect source offsets, idempotent sinks |
| **Records in the index's log, plus the reader's `hub` presents its position on resume** | **none for connected readers; up to the coalescing interval after a reader restart** | **one small record per reader per interval** | **`delivery` owns, `buffer` stores, `replica` copies** | Kafka: an in-memory fetch position across leader moves, committed offsets for restarts |

**Recommendation:** the last row. The two copies hold different facts. The reader's
`hub` knows where its session is. The home knows what the reader committed, which it
needs for holds and floors (S10) and for reader restarts. Position records ride the copy
stream in index order, so a standby never has a position beyond its data. A new home's
floors can only be lower than the old home's, which is the safe direction: it holds
more. Status channels show positions, written by `connector-status` from values
`delivery` exposes (Q11).

### 6.2 Control gate after failover

- **Raft per index** keeps the gate exact. That is A2's one real control advantage.
- **Rebuild by reconnects** (A1) fails closed (S11), but it breaks "a tie keeps the
  first holder": whoever reconnects first wins.
- **Recommendation:** the control channel is copied like any channel (r8 Q11 already
  requires it on the same home as its index). At open, `control` takes the last recorded
  holder as input, in a "held, not connected" state:
  - No command passes until that subject reopens its writer (fail closed).
  - During a grace period equal to the lease, other writers at the same or lower
    authority are refused. Higher authority can take control, as always (S11).
  - After the grace period, the gate is empty.
  - The home writes each change to the control channel.

  No new concept: the grace period is the lease the mesh already uses.

---

## 7. Recommended answers

### Q6. Standby replication and reader positions

**Answer.** A standby is a replica fed by the home's per-index log. The log records are:
- data (stored bytes)
- reader positions
- control handoffs
- backfill dedup marks
- seq block markers

The home sends them after its sync. The `replica` component reuses `delivery`'s cursor,
credit flow, catch-up from disk, and hold, and stores through `buffer::append` with no
decode. Positions are log records owned by `delivery`. A connected reader's `hub`
presents its position on resume.

**Trade.**
- A standby that is behind holds data on the home, capped by retention (S10's rule).
- Sending after the sync loses the home's unsynced window on a crash: about 2 ms on NVMe,
  about 1.6 s on a Pi SD card [M]. Decision 6 covers it.

### Q7. Failover durability

**Answer.**
- Async acceptance (B5).
- Two watermarks; the writer picks its confirmation.
- Promotion on lease lapse, whatever the lag.
- The old home's durable tail returns as backfill. Each returned frame is numbered by its
  original live seq, so readers and the new home drop what they already have (B7's rule,
  with the old home as the writer).
- The seq hole between the standby's last seq and the new block is marked as a pending
  gap. It becomes "filled" when the tail arrives, or "lost" if the old node is removed.
- No automatic failback; moving back is a planned move (r8 trace f).
- No per-placement durability setting.

**Trade.**
- A local writer (the common case) loses whatever the standby lacked when the home was
  destroyed. Raft per index would avoid that only with a third data copy and by breaking
  B5.
- The replicated watermark lag is a status channel, so operators see the exposure.
- An off-site copy comes from complete readers with holds (store-and-forward to the
  cloud), not from a second standby.

### Q8. Leases across branch groups

**Answer.** Failover authority follows the home's node, not the channel's name.
- Each node holds exactly one lease, in the group of its own branch (S9 as written).
- The runtime record for an index lives in that same group: home node, current holder,
  seq block.
- The standby must be in the home node's branch, so the same voters can promote it.
- A node claims its placements in its own group. A site `apply` never commits into the
  cloud's group.
- Readers find a home from the spec's placement (which node), then that node's group
  (which holder).

**Options compared.**
- **r8 Q8, a lease in every group whose indexes the node homes.** A cloud node homing
  `site_a.pt_101` would renew a lease with site voters across Starlink. That brings back
  the cross-link dependency r4 removed. In the model, a link that drops for 1.5 s about
  every 15 s forces a failover within 10 minutes with a 3 s lease (section 3.2). The
  home it moves away from is healthy.
- **r4's rule, home and standby inside the index's branch.** Simple, but it forces
  names to follow placement and breaks A17: a calculation placed past a weak link cannot
  keep the site name.
- **The recommended rule.** One lease per node, and names stay free.

Precedent: CockroachDB's epoch-based leases tie every range lease to one liveness
record per node.

**Trade.**
- An index's definition (governed by its name's branch) and its failover (governed by
  the home node's branch) can sit in different groups.
- A site cut off from the cloud cannot fail over a cloud-homed site channel. It cannot
  reach that channel's home either, so nothing is lost.

**Note on small sites.** Failover needs three voters. A two-gateway site needs a third,
vote-only node. A Pi is enough, because voters store only the spec and runtime state.
This is MongoDB's arbiter pattern. Here it costs nothing per frame, because voters see
lease renewals, not data.

### Q10. Connector failover

**Answer.**
- One placement covers a connector and every index under its name: its in-group
  indexes, command indexes, and their control channels. `plan` rejects a split.
- The unit of failover is the connector and its indexes, because the connector runs on
  the shard of its indexes (r1).
- The standby connector starts **cold**, only after the voters commit the move.
- The old node stops its connectors at the same fence where it stops accepting writes.
  Two nodes never drive one device.
- A kind reports whether a config is relocatable (network endpoint) or attached (USB or
  PCIe DAQ). `plan` rejects a standby for an attached connector.
- Device data during the move is an explicit gap. A kind with a device buffer (OPC UA
  history, PLC buffers) may backfill it after start.
- Commands are never replayed (D2), because the new connector's out groups are latest
  readers with a max age.

**Evidence.**
- OPC UA Part 4 defines warm failover for systems where "the underlying devices are
  limited to a single connection". Cold failover means the client "may need to wait for
  the redundant Server".
- PI UniInt failover: "With hot failover, no data loss occurs" and warm or cold "some
  data loss might occur".
- Kafka Connect moves tasks between workers on failure.

**Trade.** Cold start adds the device connect time to the gap: seconds for an OPC UA
session [U]. Hot standby connectors (both read, one writes) lose nothing but double the
device load and break single-connection devices. They can come later as an option for
kinds that allow them.

---

## 8. Decisions for the user

1. **Copy and takeover seating.** Should the copy path be a separate layer-2 component
   (`replica`, on `delivery`, `buffer`, and `transport` only), with takeover as the home's
   crash-recovery path plus one fence check?
   *Recommendation: yes.* A fully outside standby on `hub` costs a decode and re-encode per
   frame and loses dedup and position state (Litestream, PostgreSQL logical replication,
   and MirrorMaker 2 show the same limits).
2. **Replication mode.** Should copies be async, with *stored* and *replicated*
   watermarks and the writer choosing its confirmation, instead of Raft per index, ISR,
   or a per-placement setting?
   *Recommendation: yes.* It is the only option that keeps B5 in every failure with two
   copies. The trade: a local writer's data is exposed while the standby is behind.
3. **Promotion rule.** Should the voters promote the standby on lease lapse regardless of
   its lag, with the old home's tail returning as backfill numbered by its original seq?
   *Recommendation: yes.* Refusing a stale standby (ISR) loses the same data and adds an
   outage that needs an operator.
4. **Reader positions.** Should positions be log records owned by `delivery` and copied
   with the data, with a connected reader's `hub` presenting its position on resume?
   *Recommendation: yes.* It is the only option with no ordering hazard, no Raft cost per
   ack, and no re-delivery for connected readers.
5. **Gate after failover.** Should the new home start the gate from the last copied
   holder, held but not connected for one lease period, failing closed until that holder
   reopens?
   *Recommendation: yes.* It keeps "a tie keeps the first holder" across failover, with no
   new concept.
6. **When the standby gets data.** After the home's sync (simplest; keeps A8's
   restart rule) or on receipt (about 0.5 ms lost per crash instead of up to 1.6 s on a
   Pi SD card, but a restarted home must jump to a new seq block and take back the
   standby's extra suffix)?
   *Recommendation: after the sync for now, and require an SSD on Pi homes that have a
   standby.* Revisit with Pi measurements from the benchmark suite.
7. **Leases (Q8).** Should failover authority follow the home's node (one lease per node
   in its own branch's group; standby in the same branch as the home node)?
   *Recommendation: yes.* It keeps leases off weak links without forcing names to follow
   placement.
8. **Failback.** Should a home that moved stay moved, with moving back only as a planned
   move?
   *Recommendation: yes.* In the model, with a 3 s lease, automatic failback turned a
   flapping link into 13 to 69 moves in 10 minutes. Without it there was at most one.
9. **Connector failover (Q10).** One placement for a connector and every index under its
   name, cold standby connectors started after the move commits, the same fence for
   connectors and writes, and `plan` rejecting standbys for attached devices?
   *Recommendation: yes.*
10. **Small sites.** Should a site that wants failover run three voters, where the
    third may be a vote-only small node?
    *Recommendation: yes.* `plan` should warn when a placement names a standby but the
    branch has fewer than three voters.

---

## 9. Simulation invariants for T1 (proposed oracles)

1. At most one node accepts live writes for an index at any mesh-time instant, given
   clock-rate error inside the configured bound.
2. Every seq is assigned at most once per index. No seq block is used twice.
3. Every frame that was durable on any copy is eventually on the current home, or its
   range is reported as a lost gap. Never silently missing.
4. A frame confirmed as *replicated* is on the current home after any single failure.
5. A standby never holds a position record beyond its last data record for that index.
6. A complete reader never receives the same live seq twice from one home. Duplicates
   occur only across failover, as backfill, and carry their original seq.
7. No command executes while the gate's holder is unknown.
8. Two connectors for the same endpoint never run at overlapping mesh-time instants.

---

## 10. Sources

Replication protocols and systems:

- Kafka replication design (Confluent):
  https://docs.confluent.io/kafka/design/replication.md
- Apache Kafka design (PacificA, majority vote copies):
  https://kafka.apache.org/43/design/design/
- Eligible Leader Replicas: https://kafka.apache.org/43/operations/eligible-leader-replicas/;
  Vanlightly on KIP-966:
  https://jack-vanlightly.com/blog/2023/8/17/kafka-kip-966-fixing-the-last-replica-standing-issue
- KIP-320:
  https://cwiki.apache.org/confluence/display/KAFKA/KIP-320:+Allow+fetchers+to+detect+and+handle+log+truncation
- Kafka 0.8.2 offset storage [1]:
  https://www.confluent.io/blog/whats-coming-in-apache-kafka-0-8-2/
- MirrorMaker 2: https://cwiki.apache.org/confluence/display/KAFKA/KIP-382%3A+MirrorMaker+2.0;
  offset sync lag [1]:
  https://aiven.io/docs/products/kafka/kafka-mirrormaker/reference/known-issues
- Debezium offset storage:
  https://debezium.io/documentation/reference/configuration/storage.html
- Redpanda architecture: https://docs.redpanda.com/current/get-started/architecture/;
  write caching:
  https://redpanda.com/blog/redpanda-24-1-general-availability-write-caching;
  Jepsen Redpanda 21.10.1: https://jepsen.io/blog/2022-04-29-redpanda-21.10.1
- CockroachDB replication layer:
  https://www.cockroachlabs.com/docs/v21.1/architecture/replication-layer;
  leader leases [1 for the 85% figure]:
  https://cockroachlabs.com/blog/distributed-database-leader-leases
- NATS Raft groups: https://docs.nats.io/learn/clustering/raft-and-leaders;
  high HA assets [1]: https://www.synadia.com/insights/checks/nats-high-ha-assets;
  Jepsen NATS 2.12.1: https://jepsen.io/analyses/nats-2.12.1;
  mirrors: https://docs.nats.io/nats-concepts/jetstream/source_and_mirror
- TigerBeetle VSR: https://docs.tigerbeetle.com/about/internals/vsr
- Aeron Cluster [1]: https://github.com/real-logic/aeron/wiki/Cluster-Tutorial
- Elasticsearch replication model:
  https://www.elastic.co/guide/en/elasticsearch/reference/current/docs-replication.html
- PacificA:
  https://www.microsoft.com/en-us/research/publication/pacifica-replication-in-log-based-distributed-storage-systems/;
  summary: https://dsrg.pdos.csail.mit.edu/2013/06/06/pacifica/
- Vertical Paxos:
  https://www.microsoft.com/en-us/research/publication/vertical-paxos-and-primary-backup-replication/
- Chain replication:
  https://www.usenix.org/conference/osdi-04/chain-replication-supporting-high-throughput-and-availability;
  CRAQ:
  https://www.usenix.org/conference/usenix-09/object-storage-craq-high-throughput-chain-replication-read-mostly-workloads
- BookKeeper protocol: https://bookkeeper.apache.org/docs/development/protocol
- LogDevice:
  https://engineering.fb.com/2017/08/31/core-data/logdevice-a-distributed-data-store-for-logs/
- Dynamo (SOSP 2007):
  https://www.allthingsdistributed.com/files/amazon-dynamo-sosp2007.pdf;
  Jepsen Cassandra: https://aphyr.com/posts/294-jepsen-cassandra
- PostgreSQL standby and sync replication:
  https://www.postgresql.org/docs/current/warm-standby.html;
  logical replication restrictions:
  https://www.postgresql.org/docs/current/logical-replication-restrictions.html;
  PostgreSQL 17 failover slots:
  https://www.morling.dev/blog/failover-replication-slots-with-postgres-17/
- MySQL semi-sync: https://dev.mysql.com/doc/refman/8.4/en/replication-semisync.html;
  timeout default:
  https://dev.mysql.com/doc/refman/8.4/en/replication-semisync-interface.html
- GitHub 2018 incident: https://github.blog/2018-10-30-oct21-post-incident-analysis/
- MongoDB arbiter: https://www.mongodb.com/docs/manual/core/replica-set-arbiter/
- Fencing tokens (cited in r4):
  https://martin.kleppmann.com/2016/02/08/how-to-do-distributed-locking.html

Industrial:

- PI high availability [1]:
  https://www.tdworld.com/smart-utility/article/20958556/osisoft-releases-high-availability-version-of-the-pi-system;
  PI buffering: https://docs.aveva.com/bundle/pi-server-s-buf-ha/page/1020154.html;
  UniInt failover:
  https://docs.aveva.com/bundle/pi-universal-interface-uniint-framework/page/1026193.html
- Ignition redundancy:
  https://docs.inductiveautomation.com/docs/8.1/platform/ignition-redundancy;
  store and forward:
  https://docs.inductiveautomation.com/docs/8.1/platform/database-connections/store-and-forward
- OPC UA Part 4, server redundancy: https://reference.opcfoundation.org/Core/Part4/v104/docs/6.6

Seating precedents:

- Litestream: https://litestream.io/how-it-works/; LiteFS FAQ: https://docs.fly.io/litefs/faq

Hardware:

- Pi 4 SD card fsync, about 240 to 260 ms [1]: https://wiki.kewl.org/boards:rpi4:sdb_fio
- This machine's sync latency: measured, `scratchpad/r13/fsync_bench.py` (Apple M3 Max,
  macOS 27.0.1, `F_FULLFSYNC` p50 4.0 ms, p99 4.2 to 6.0 ms; plain `fsync` 0.03 ms, which
  does not flush the drive cache)

## 11. Single-source and unverified claims

- [1] CockroachDB's 85% lease CPU cut; Synadia's 1,000-asset threshold; the Pi SD card
  fsync figure; Aeron's majority-before-consume wording; MirrorMaker 2's 100-offset lag;
  PI's "write directly to members" wording (2009 press article); Kafka's ZooKeeper
  offset cost.
- [U] NVMe-with-PLP sync of about 0.1 ms; LAN RTT of 0.2 ms; 4 bytes per sample at P1
  (P1's target, not a measurement); the Pi's per-copy ceiling; OPC UA session connect
  time; ChaCha20 on a Pi 4 (one search summary reported about 306 MiB/s on a 2.3 GHz
  Cortex-A72; a Pi 4 core runs slower).
- Not fetched in this fork: Gray and Cheriton on leases (SOSP 1989), and Raft's
  parallel leader write (taken from etcd/raft's async storage writes, listed in r4).
- [M] Every number marked [M] comes from a model with the limits listed in section 3,
  not from a running system.
