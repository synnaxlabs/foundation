# Retired entries

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
| BENCH SPEND; the coordinator as the session that rents and ends the ARM RUNNER hosts | Test budget (`docs/decisions/open/mvp.md`) |
| C1 and C9a crate lists | `docs/decisions/crate-map.md` |
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
| `docs/decisions/open/mvp.md` and STORE AND FORWARD one-hour cut (#1072) | STORE AND FORWARD amendment (2026-10-07) |
| R16-7 "a map keyed by outside input will get a keyed hasher" | R16-7 `BTreeMap` rule (2026-10-07T17:36:18Z) |
| HUB END: the task drops the commit it waits for at its first poll after the hub drops | HUB END: the commit lives in the state (#1633) |
| NODE PORT deferral of #1649 (6048464411): a transport that stops ends the routing and the node runs on with no port | NODE PORT amendment (#1647, 6049354544) |
| NODE PORT amendment of #1830 (6054871235): the transport drops when the last task of the mesh ends | NODE PORT second amendment (#1962, 6068113010): the transport drops with the last of the port's future and the tasks of the mesh |
| NODE PORT second amendment (#1962, 6068113010): a node whose mesh stops keeps serving until #1780 stops it | NODE PORT second amendment, new words (#1962, 6069443456): a group stop of the mesh stops the node (NODE MESH) |
| NODE PORT, test of the amendment (#1962, 6069568312): the port is free once `lock` is | NODE PORT (#1962, 6069829972, 6070247529, and 6070577797): under `sim` the port is free once `lock` is; under `os` the carrier's task can hold the socket until the runtime of shard 0 drops, until #2017 |
| HOME TYPE REFUSAL (#963): `open_writer` refuses a series of a type the home does not write | HOME EVERY TYPE |
| FIRST SLICE order, for ONE NODE work only; the order "after FIRST SLICE" of 6050540089 | FIRST SLICE amendment (2026-10-08) |
| HUB SESSIONS (#340, 6066821273): `reader::Error::Remote` gives a reader of an index at another node | HUB SESSIONS amendment (#340 PR 4d-b, 6069259471): the reader reads from that home over one hub stream |
| `hub::Config::mesh` of 6067438821 | HUB SESSIONS amendment (#340 PR 4d-b, 6068108715): `hub::Config::region` |
| `hub::Config::transport` of 6048960511 | HUB SESSIONS amendment (#340 PR 4d-b, 6068108715): `hub::Config::region` |
| `reader::Error::Refused(transport::Code)` and `reader::Ended::Refused(transport::Code)` of 6048960511 | HUB SESSIONS (6069259471): `Refused(wire::hub::Refusal)` |
| HUB SESSIONS plan of 6048861311: a session for each home | HUB SESSIONS (6069259471): the dial at each open |
| Code 18 of #340 6047300641: "the home's buffer failed" | HUB WIRE (6048960511) |
| Code 19 of #340 6047519084: "the home had no memory for a reply" | HUB WIRE (6048960511; the too-large exception 6071260886) |
| Code 19 in item 3 of #1946 6069496483: "the node had no memory for a reply or a request body" | HUB WIRE (6071260886), until #2012 (6071577074) |
