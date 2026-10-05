# R4: consensus, gossip, and spec distribution (S9, K5)

Research fork 4 of 8, 2026-10-04. Scope: the consensus algorithm, build vs adopt, the need
for gossip, the content-addressed spec tree, and per-branch delegation. Every load-bearing
claim cites two sources or is marked UNVERIFIED.

## Summary

| Question | Recommendation |
| --- | --- |
| Q1 algorithm | Raft, one group per delegated branch, with PreVote and CheckQuorum always on. Each group's voters sit in one network locality, so no vote crosses Starlink. |
| Q2 build vs adopt | Build our own sans-I/O Raft core, ported from etcd/raft's design. Use etcd's scenario tests and TLA+ trace validation as oracles. raft-rs is the fallback. |
| Q3 gossip | No gossip. Leases, the transport, status channels, and `mesh.changes` already cover every job gossip would do. |
| Q4 spec tree | One prolly tree (content-defined chunked B-tree) per delegated branch, keyed by full dot name, with 4 KiB chunks and BLAKE3 hashes. Each `mesh.changes` record lists its new chunks. |
| Q5 delegation | Cross-branch changes commit per branch, ordered by dependency. Key references (index, quality, error, control, home, standby) must stay inside one delegated branch. A branch changes its own voters. A parent takeover of a cut-off branch is an explicit forced action, fenced by an epoch. |

Two of these revise locked decisions:
- Q5 revises K5's trade "changing which voters own a branch needs the parent branch's voters". With this change, a branch changes its own voter set. The parent is needed only to create or remove a delegation, or to force a takeover.
- Q5 adds a rule to S5, S11, and S12: key references must not cross a delegated branch boundary.

## Q1: consensus algorithm

**Recommendation:** use Raft for every voter group. PreVote and CheckQuorum are always on.
Each group's voters must sit in one network locality, such as one LAN or one cloud region.

**What the workload needs.** Writes are small and rare: a spec pointer, plus homes and leases.
Throughput does not matter. Correctness, simple reasoning, and liveness on flaky links do.
There is one group per delegated branch (K5), so a mesh has tens to hundreds of groups, and
each node votes in one to three of them.

**Evidence**

- Raft and Multi-Paxos differ only in leader election. "Both Paxos and Raft take a very
  similar approach to distributed consensus, differing only in their approach to leader
  election" (Howard and Mortier, PaPoC 2020, https://arxiv.org/pdf/2004.05074). A review
  reaches the same conclusion: https://emptysqua.re/blog/paxos-vs-raft/. So choosing Raft
  costs nothing in capability. Raft has the most reference material and test oracles (Q2).
- Leaderless protocols are a poor fit. EPaxos only helps median latency when many sites write
  at once. A re-evaluation found that EPaxos has "much worse tail latency than previously
  reported (more than 4x worse than Multi-Paxos)", and that performance is highly sensitive to
  workload (Tollman, Park, Ousterhout, NSDI 2021,
  https://www.usenix.org/conference/nsdi21/presentation/tollman). Our writes are rare, so they
  have no conflict pattern for EPaxos to exploit.
- Viewstamped Replication (TigerBeetle) is equivalent in power. TigerBeetle chose it for
  deterministic view changes, and because it treats consensus and storage as one concern
  (https://jack-vanlightly.com/analyses/2022/12/20/vr-revisited-an-analysis-with-tlaplus,
  https://amplifypartners.com/blog-posts/why-tigerbeetle-is-the-most-interesting-database-in-the-world).
  We keep its key lesson, protocol-aware recovery from disk faults (see Risks), without
  switching algorithms.
- Multi-raft machinery is built for a different scale. CockroachDB's MultiRaft coalesces
  heartbeats and quiesces idle ranges because "each node may be participating in hundreds of
  thousands of consensus groups" (https://www.cockroachlabs.com/blog/scaling-raft/). TiKV has
  the same problem with regions. Our nodes sit in one to three groups. We only need one
  transport connection and one log store shared by the groups on a node, not batching
  machinery.
- Starlink is hostile to cross-link voting. Starlink reconfigures every 15 s, on a globally
  synchronized schedule, which causes latency and throughput drops at sub-second granularity.
  Median RTT is about 40 to 50 ms, with outliers over 100 ms (Mohan et al., "A Multifaceted
  Look at Starlink Performance", WWW 2024, https://arxiv.org/pdf/2310.09242; APNIC summary
  https://blog.apnic.net/2024/07/11/a-multifaceted-look-at-starlink-performance-the-good-the-bad-and-the-ugly/).
  Measured loss is 0.4% light and 1.5 to 2% under load. Raft needs broadcast time to be much
  smaller than the election timeout. A voter group that spans a Starlink link must either use
  multi-second election timeouts or suffer spurious elections every 15 s.
- PreVote and CheckQuorum are required for liveness on partial or asymmetric links. The
  November 2020 Cloudflare outage lasted 6.5 hours, and it was traced to etcd Raft behavior
  under a partial network fault that PreVote would have prevented
  (https://talks.cam.ac.uk/talk/index/159961, https://dev.to/tarantool/raft-notalmighty-how-to-make-it-more-robust-3a11).
  Relayed iroh paths are asymmetric by nature.

**The locality rule.** K5 already puts a site's voters at the site. The rule finishes the
job: every group's voters share one locality, so no vote crosses a slow link. Nodes outside
a group follow its `mesh.changes` as readers. That traffic tolerates latency and resumes after
outages (B3). The root group lives in one cloud region, and sites never vote in it.
`plan` warns when a voter set spans localities. Locality can be inferred from measured RTT,
so this needs no new config term.

**Rejected**

- Multi-Paxos: equivalent power, but fewer reference implementations and test oracles than
  Raft.
- VSR: equivalent power. Its advantages, deterministic view change and storage-aware design,
  can be adopted inside Raft (PAR, checksums). Few public scenario test suites exist to port.
- EPaxos and other leaderless protocols: tail latency more than 4x worse than Multi-Paxos;
  complex; no benefit for rare writes.
- CockroachDB or TiKV style multi-raft: solves 100k groups per node. We have one to three.
- One mesh-wide group with voters on several continents: every Starlink reconfiguration
  risks an election, and a cut-off site cannot change its own config (this is why K5 exists).

**Risks**

- Storage faults. Raft as specified assumes the disk is correct. "Protocol-Aware Recovery for
  Consensus-Based Storage" (Alagappan et al., FAST 2018 best paper,
  https://www.usenix.org/conference/fast18/presentation/alagappan, summary
  https://blog.acolyer.org/2018/02/27/protocol-aware-recovery-for-consensus-based-storage/)
  shows that deployed RSM systems lose data or become unavailable on a single corrupted log
  entry. Mitigation: checksum every log entry and recover a corrupt entry from peers instead of
  truncating it. Inject torn writes and corruption in DST (turmoil `unstable-fs` supports
  both, per research ledger section 9).
- Small sites. A site with two nodes cannot form a fault-tolerant group. Recommend one voter
  (autonomy without fault tolerance) or three. `plan` warns on even voter counts.

**Decision for the user:** do you agree that each voter group sits in one network locality
(one site LAN or one cloud region), so that no consensus traffic ever crosses Starlink, and
other nodes only follow the group's changes?

## Q2: build vs adopt

**Recommendation:** build our own sans-I/O Raft core in Rust, ported from etcd/raft's design.
etcd/raft's scenario tests and its TLA+ trace validation become our oracles. raft-rs is the
fallback.

**Candidates**

| Option | Shape | Evidence | Problem |
| --- | --- | --- | --- |
| openraft 0.10 | Async framework that drives storage and network through its own traits | 0.10.0-alpha.36, 2026-09-29; stable line 0.9.25 gets fixes only (crates.io API). README: "OpenRaft API is not stable yet. Before 1.0.0, an upgrade may contain incompatible changes." Changelog tags on-disk format changes `data-change:`. Strong testing: a turmoil simulation fuzzer on every CI build, and a Jepsen suite with 8 nemesis scenarios on every push to main. Runtime and randomness are injectable through `AsyncRuntime` (`openraft/src/engine/engine_config.rs` calls `AsyncRuntimeOf::<C>::thread_rng()`; engine tests inject a fixed generator, `openraft/src/engine/tests/elect_test.rs`). Users: Databend, CnosDB, RobustMQ. (https://github.com/databendlabs/openraft) | Not sans-I/O: it owns the control flow and calls our storage and network. 36 alpha releases with API and disk format churn. We would own a dependency whose disk format we do not control. |
| raft-rs 0.7 | Sans-I/O, tick-driven etcd/raft port (`Ready` loop) | Consensus module only; "you will need to build your own Log, State Machine and Transport" (https://github.com/tikv/raft-rs). Last crates.io release 0.7.0 on 2023-03-07, but master is active (commits 2026-02 to 2026-05). TiKV depends on git master, not a release (`tikv/Cargo.toml:207`, `raft = { git = "https://github.com/tikv/raft-rs", branch = "master" }`). | Randomness is not injectable: `src/raft.rs:2857` calls `rand::rng().random_range(...)` for election timeouts, which breaks T1 unless patched. Default features pull protobuf and slog (`Cargo.toml:19-38`). Etcd-shaped API. No release in 3.5 years. |
| Own sans-I/O core | Pure state machine: messages, ticks, and an injected RNG in; messages, log writes, and commits out | etcd/raft core is about 5,600 lines of non-test Go: `raft.go` 2,162, `log.go` 576, `log_unstable.go` 240, `rawnode.go` 557, `storage.go` 326, `read_only.go` 101, `tracker/` 780, `quorum/` 357, `confchange/` 574 (counted from github.com/etcd-io/raft, 2026-10-04). etcd/raft ships 28 data-driven scenario files in `testdata/`, including `prevote.txt`, `checkquorum.txt`, `prevote_checkquorum.txt`, `confchange_v2_replace_leader.txt`, and `async_storage_writes_append_aba_race.txt`. It also ships a TLA+ spec with trace validation (`tla/etcdraft.tla`, `tla/Traceetcdraft.tla`, `state_trace.go`). | We write it, and correctness is on us. Estimated 4,000 to 6,000 lines of Rust plus tests. |

**Why build.** The need is small: rare writes, a few groups per node, and snapshots that are
just a root hash (Q4). The deciding factors:

- **Sans-I/O.** A pure core passes T1 by construction, and it fits whatever C2 decides about
  threads.
- **Disk format.** It is ours, versioned with one integer like every other format (C9d).
- **Oracles.** Strong ones exist that an agent cannot argue with: etcd's 28 scenario files,
  ported as data-driven tests, plus TLA+ trace validation. The ledger notes that LLM-written
  TLA+ matches code only about 41 to 46% of the time without trace validation (section 6), so
  validating against an existing trace spec is the strong form.

This matches the library rule: architecture first, and building our own Raft is acceptable.

**Fallback.** If the factory cannot pass the oracles within a phase budget, adopt raft-rs from
a pinned git revision, as TiKV does, and patch the RNG call at `src/raft.rs:2857` to take an
injected generator.

**Rejected**

- openraft: not sans-I/O, alpha API, and disk format churn. Its testing is the best of the
  three, and its Jepsen scenarios are a model for our own.
- raft-rs adopted as-is: the RNG breaks T1, the dependencies are heavy, and it has no release.

**Risks**

- Membership changes are where Raft implementations break. Port the etcd `confchange_v1` and
  `confchange_v2` scenarios first, and require joint consensus (Raft dissertation chapter 4).
- Building our own takes factory time. Mitigate with a fixed phase budget and the raft-rs
  fallback.

**Decision for the user:** do you agree to build our own sans-I/O Raft core, with etcd/raft's
scenario tests and TLA+ trace validation as oracles, and raft-rs pinned to a git revision as
the fallback?

## Q3: gossip

**Recommendation:** no gossip. Every job gossip would do already has an owner in the locked
design.

| Job gossip usually does | Owner in Foundation |
| --- | --- |
| Membership (who is in the mesh) | The spec (S9), through consensus |
| Authoritative failure detection | Per-node leases in the node's branch group (S9) |
| Local liveness for routing | Connection keepalive in the transport |
| Addresses | Transport discovery (iroh dials by key, through relays and address lookup) |
| Load and health hints | Status channels under the node's name (S8), read by ordinary subscription |
| Config dissemination | `mesh.changes` followers plus chunk fetch from any node (Q4) |

**The Synnax lesson (Aspen).** Aspen is a homegrown, eventually consistent KV store that runs
on SI and SIR gossip. Its README says: "The gossip protocol lacks three essential features:
failure detection, failure recovery, and efficient propagation guarantees", and also "Aspen
is in active development and is not yet ready for production use" (`aspen/README.md`). The
code confirms it. Node states `StateSuspect` and `StateDead` are defined at
`aspen/internal/node/node.go:61-62` and re-exported at `aspen/db.go:77-78`, but no non-test
code ever assigns them. Peer selection only filters on `StateHealthy`
(`aspen/internal/cluster/gossip/gossip.go:151-153`). In practice, Aspen's gossip was a slow
broadcast with no failure detection. The lessons:
- correctness-critical metadata needs agreement, not eventual consistency
- gossip without failure detection buys little
- node IDs from a quorum "pledge" (`aspen/internal/cluster/pledge/pledge.go`) encode ownership
  into keys, which the ledger already rejects

**If gossip is ever needed** (much larger meshes, or finding which peer holds a chunk), there
are three options:

- **foca 2.0 (MPL-2.0, 2026-08-02).** A `no_std` sans-I/O SWIM with the infection and
  suspicion extensions. The user schedules its timers: "Foca will tell you 'Send these bytes
  to member M', how that happens is not its business" (https://docs.rs/foca). It passes T1.
  No production users are documented (UNVERIFIED).
- **iroh-gossip 0.101 (HyParView + PlumTree).** It has a sans-I/O `proto` module, "a state
  machine without any IO" (https://docs.rs/iroh-gossip). But it is pre-1.0, with a breaking
  release about every three weeks (0.99.0 on 2026-05-08, 0.100.0 on 2026-05-27, 0.101.0 on
  2026-06-15, from the crates.io API).
- **chitchat 0.13 (Quickwit; scuttlebutt with phi-accrual detection).** Used in production by
  Quickwit (https://github.com/quickwit-oss/chitchat). It does its own UDP I/O on Tokio.
  Injectable transport is UNVERIFIED.

SWIM needs Lifeguard-style local health checks to avoid false positives. HashiCorp measured
a 50x reduction (https://www.hashicorp.com/blog/making-gossip-more-robust-with-lifeguard,
https://arxiv.com/abs/1707.00788). This is one more reason not to let gossip decide anything.

**Rejected:** gossip for membership or failure detection (consensus and leases own those), a
homegrown gossip (Aspen's outcome), and memberlist ports (the Rust `memberlist` crate has 43k
total downloads, against foca's 233k and chitchat's 249k).

**Risk:** with no gossip, finding a nearby peer for a large chunk or binary fetch (C9d) needs
a rule. Proposed rule: ask the node's own branch voters first, because they hold the whole
branch. Then ask any node that follows the branch.

**Decision for the user:** do you agree to ship with no gossip, with leases, the transport,
status channels, and `mesh.changes` covering its jobs?

## Q4: the content-addressed spec tree

**Recommendation:** give each delegated branch one prolly tree: a content-defined chunked
B-tree keyed by full dot name.
- Chunks target about 4 KiB, using Dolt's size-dependent, key-only chunker.
- Chunks are addressed by BLAKE3 hash and fetched from any node.
- Each `mesh.changes` record carries the new root hash and the list of new chunk hashes, so a
  follower gets a change in one round trip.

**Why not a tree that copies the name hierarchy.** A Git-style tree with one node per name
segment is unbalanced. A branch with 20,000 channels under one prefix (S7 templates make this
normal) becomes one large node, about 1 MB at 50 bytes per entry, and every change rewrites
it. IPFS hit exactly this problem. Kubo converts a directory to a HAMT once it passes 256 KiB
(https://specs.ipfs.tech/ipips/ipip-0499, https://github.com/ipfs/kubo/pull/3042).

**Why the hierarchy is not lost.** Dot names sort so that every branch, such as
`site_a.stand_3.`, is one contiguous key range. A partial fetch of one branch is a range walk
over the tree. It touches only the chunks that overlap the range, plus their parents.

**Why a prolly tree**
- **Bounded chunks.** Dolt targets about 4 KB chunks with 20-byte addresses
  (https://www.dolthub.com/blog/2022-06-27-prolly-chunker/,
  https://docs.dolthub.com/architecture/storage-engine/block-store). Its improved chunker
  hashes keys only, and raises the boundary probability as a chunk grows. That fixes the
  geometric size spread and the "massive chunks" that rolling hashes produce on sorted keys
  (same source).
- **History independence.** The same set of definitions always produces the same root hash,
  whatever the edit order. That makes the root a reliable pointer, and it makes `plan` a
  cheap tree diff. Merkle Search Trees (Auvolat and Taïani, SRDS 2019) share this property,
  and AT Protocol uses them for every Bluesky repository (https://atproto.com/specs/repository).
- **Why not an MST:** AT Protocol's fanout of about 4 (two zero bits per layer) produces many
  small nodes, so a tree is deeper and needs more round trips than one with 4 KiB chunks.

**Size and fetch estimates.** Inputs: about 100 bytes per definition (name about 40 bytes
before prefix compression, key 16, index 16, type and unit about 8, quality 16, encoding
overhead). An internal entry is about 52 bytes (a 20-byte separator plus a 32-byte hash), so
about 78 children fit in a 4 KiB chunk.

| Definitions | Raw size | Leaf chunks | Internal chunks | Height | New data for one change |
| --- | --- | --- | --- | --- | --- |
| 50k (one site) | 5 MB | ~1.2k | ~16 | 3 | ~12 KiB |
| 1M | 100 MB | ~25k | ~325 | 4 | ~16 KiB |
| 10M | 1 GB | ~250k | ~3.3k | 4 to 5 | ~20 KiB |

- **One change** rewrites one leaf and its path to the root: four or five chunks. When the
  `mesh.changes` record lists those chunk hashes, a follower fetches them in one batched
  request, one RTT (about 45 ms on Starlink). Without the list, it walks down the tree, one
  RTT per level (about 200 ms on Starlink at height 4). Both are fine. The list removes the
  depth dependence.
- **A bulk change** such as a 1,000-motor template expansion (20,000 definitions, about 2 MB)
  rewrites about 500 contiguous leaves and about 10 internal chunks.
- **A gateway's first sync** of its own 50k-definition branch is about 5 MB raw. Chunk-level
  zstd should cut that roughly in half or better (UNVERIFIED ratio; names compress well, UUIDv7
  keys do not).
- **10M definitions** is 1 GB per full replica. That is why S9's rule matters: a node fetches
  only the branches and ranges it uses. Voters of the root branch hold the root branch, not
  every site's branch (Q5).

**Transport and hashing.** Chunks are small, so per-chunk hash checks are enough. BLAKE3
verified streaming (bao) only pays off for large blobs (iroh-blobs groups 16 KiB of chunks
per verification checkpoint, at about 6% overhead with 1 KiB groups;
https://www.iroh.computer/design/content-addressing,
https://iroh.computer/blog/blake3-hazmat-api). Use a plain batched "get chunks by hash"
request on our own transport. Keep iroh-blobs out of the dependency set: it is pre-1.0, with
breaking releases every three weeks (0.101 to 0.103 between 2026-05-08 and 2026-06-15). A
signed binary (C9d) can be stored the same way, as a chunk list.

**Garbage collection.** Each node keeps the chunks reachable from the last N roots of every
branch it follows, which allows rollback and plan diffs against recent versions. Unreachable
chunks are dropped. N is a tunable parameter.

**Rejected**

- A Git-style tree that copies the name hierarchy: unbalanced, with large rewrites per change
  on wide prefixes.
- IPFS UnixFS and HAMT: a heavy stack (DHT, CID profiles) for a problem with known peers.
  HAMT also breaks key order, so range fetch is lost.
- iroh-blobs as the tree: blobs are opaque byte strings with no key-range structure, and the
  crate is pre-1.0.
- AT Protocol style MST with fanout 4: deeper trees and more round trips.

**Risks**

- Chunker quality decides the worst case. Use Dolt's improved chunker design, and benchmark the
  chunk size distribution on real name sets (P1 suite).
- Hash choice and certification: BLAKE3 is fast but not FIPS-approved. If FIPS becomes a
  requirement (an open question in the notes), the hash is a one-integer format version
  change (C9d).

**Decision for the user:** do you agree to one prolly tree per delegated branch, keyed by full
name, with about 4 KiB chunks, where each `mesh.changes` record lists its new chunks?

## Q5: per-branch pointers and delegation

**Recommendation:**

- **What the parent holds.** A parent stores a delegation record for each child branch:
  `{ branch, initial voters, epoch }`. It never stores the child's root hash, so child changes
  never touch the parent.
- **Voters.** A child changes its own voter set through ordinary Raft joint consensus.
- **Cross-branch changes** commit as separate per-branch commits, ordered by dependency.
- **Key references stay inside one branch.** References by key (`index`, `quality`, `error`,
  `control`, placement `home` and `standby`) must stay inside one delegated branch. Links
  across branches go through selectors only.
- **Forced takeover.** If a cut-off branch is reassigned, the parent bumps the epoch, and nodes
  reject the old epoch's commits.

**Cross-branch commits.** There are three options:

1. **Two-phase commit across groups**, as Spanner does: "If a transaction involves more than
   one Paxos group, those groups' leaders coordinate to perform two-phase commit" (Corbett et
   al., OSDI 2012). It blocks or aborts when one group is unreachable. That is exactly the
   cut-off site case K5 exists for. Rejected.
2. **Independent per-branch commits with no rules.** A key reference in one branch can point
   at an item another branch has not created yet, or has already deleted. Rejected.
3. **Independent commits with two rules.** This is the recommendation:
   - Key references never cross a delegated branch. `plan` reports an error otherwise. Most
     key references already stay local: a channel's index, quality, error, and control
     channels live under the same device or template name. Requiring a standby to be in the
     same branch also fits S11 and S9, because the branch's own group decides failover for
     its nodes (S9 leases per node).
   - Selectors are late-bound: they match by name when evaluated (S12). So a cross-branch
     selector that matches nothing yet is harmless. `plan` shows it as matching zero channels.
     `apply` then orders the per-branch commits like Terraform's dependency graph: creates go
     dependee first, deletes go referrer first. The two-step commit from K5 can never leave a
     dangling key reference.

**Delegation and re-delegation.**

- **Creating a delegation** is a parent commit: `[[voters]] select = "site_a.**" nodes = [...]`
  lives in the parent's spec. It writes `{ branch: "site_a", voters, epoch: 1 }`. The named
  voters start the child group with an empty tree, or with the parent's current definitions
  under that prefix moved into the new tree.
- **Changing a child's voters** is the child's own business: a joint-consensus membership
  change inside the child group, and the child's spec records the change.
  This revises K5. A site that loses a gateway while cut off from the cloud must still be able
  to replace that voter. Under K5 as locked, it could not, and one more failure would freeze
  the site's config permanently. The parent's record keeps only the initial voters and the
  epoch.
- **Taking a branch back, or reassigning it** while the child is reachable, is a handoff. The
  child commits a final entry that names its last root. The parent commits the new delegation
  at epoch + 1, which points at that root. The new voters start from it.
- **Forced takeover while the child is cut off** is an explicit `--force` action by a subject
  with `admin` on the parent. The parent commits epoch + 1, starting from the last child root
  it knows. Every node rejects child commits that carry an older epoch. This is the
  fencing-token pattern (Kleppmann, "How to do distributed locking", 2016). When the old group
  reconnects, its commits after the fork are shown as a conflict for a person to review in
  `plan`. They are never silently merged.

**While a branch is cut off:**

- Inside the branch, everything keeps working with a quorum of its voters: config changes,
  failover, leases, and voter replacement.
- Parent policies that select into the branch (for example, a root `[[retention]] select =
  "**"`) apply as of the last root version the branch saw. They are stale but consistent, and
  they are re-evaluated on reconnect.
- Cross-branch readers stall and then catch up (B1, B3). Cross-branch key references cannot
  exist, so nothing dangles.
- The parent cannot change the child's delegation except by forced takeover.

**DNS comparison.** DNS delegation (RFC 1034) also stores only the delegation (NS records) in
the parent, never the child zone's contents. The child changes its zone freely. Our epoch
fencing is stronger than DNS's TTL-based convergence, because a forced takeover must not let
two groups commit at once.

**Rejected**

- Two-phase commit across branches: blocks on the cut-off case.
- The parent stores the child's root hash: every site change would rewrite and commit in the
  root group, and a site could not commit while cut off.
- Parent-owned voter membership (K5 as locked): a cut-off site cannot replace a failed voter.
- Cross-branch key references with ordering only: the parent-first and child-first orders
  conflict for references in both directions, and a cut-off branch leaves references
  dangling.

**Risks**

- Forced takeover loses the old group's divergent commits. This is acceptable only because it
  is explicit and visible in `plan`.
- Same-branch placement means a site's standby must be at the site. A cloud standby for a
  site index needs a branch-spanning failover protocol, which is out of scope.

**Decisions for the user (two, in order):**

1. Do you agree that a branch changes its own voters, and that the parent is needed only to
   create or remove a delegation, or to force a takeover? This revises K5.
2. Do you agree that key references (index, quality, error, control, home, standby) must
   stay inside one delegated branch, with cross-branch links only through selectors?

## Unverified claims

- foca's production users: none documented.
- Whether chitchat's transport can be injected: the fetch summary claimed it but quoted no
  code.
- zstd compression ratio on spec chunks: an estimate, to be measured.
- Per-definition size of about 100 bytes: an estimate from the S5 fields, to be measured.
