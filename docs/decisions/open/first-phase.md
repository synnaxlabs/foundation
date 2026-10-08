# First phase

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
scenario of `docs/decisions/open/mvp.md`: an edge node writes 1M samples/s while its
link to the cloud is cut for one hour (one minute since the amendment below), and the
`acceptance` tests run both disk budgets. It runs beside FIRST SLICE, which keeps
priority. The person decided on 2026-10-06 ("Yes that is fine I approve", relayed by
`monitor`).

Amendment (2026-10-07): the cut is one minute, not one hour, at the same rate.
Supersedes the one-hour cut of `docs/decisions/open/mvp.md` and of this entry (#1072).
The hour writes 3.6e9 samples, and its edge buffer alone does not fit in `sim` on a CI
runner (https://github.com/synnaxlabs/foundation/issues/1149#issuecomment-6034058219).
The simulated InfluxDB store gets a compact form first (#1419). No scheduled run on a
rented host runs the hour. The person decided ("Let's do a smaller scenario. It can
still prove a significant amount of the behavior." and "Copy, yes I can agree with
that"), relayed by `laptop.monitor` at 2026-10-07T14:10:57Z:
https://github.com/synnaxlabs/foundation/issues/1149#issuecomment-6039778221.

`docs/decisions/open/mvp.md` sets no drain rate. Before the two tests lose `#[ignore]`,
the lab runs a drain span after the heal in place of `OUTAGE`: two times the sum of the
delay before the drain starts and the time to send `WRITTEN` at the drain rate measured
in the lab. `laptop.architect-2` decided this at 2026-10-07T16:31:12Z (#1477:
https://github.com/synnaxlabs/foundation/issues/1477#issuecomment-6042249280).

**ONE NODE (2026-10-08)** A milestone beside FIRST SLICE, which keeps priority: one real
node moves OPC UA samples to InfluxDB. The `foundation` binary starts a node from a
config, on a real disk and network, reads an OPC UA server through `connector-opcua`,
and pushes the samples to InfluxDB through `connector-influx`. Its acceptance is a
simulated OPC UA server, the node, and a simulated InfluxDB. A first version may run
with no OPC UA security, so the crypto plugin (`docs/decisions/open/still-open.md` item
2) does not block it. The developer experience on one node is part of the goal, and
#1737 breaks it into tests. When it and STORE AND FORWARD both have ready issues, ONE
NODE goes first. The FIRST SLICE amendment of 2026-10-08 gives the split of the work and
its "Supersedes". The plan is on #1737. The person decided, relayed by `laptop.monitor`:
the milestone ("Yes", 2026-10-08T01:52:14Z,
https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6050540089), the order
("Yes, let's do one node first. We should really prioritize a working devx that feels
relatively good with one node. and an influxdb to opc ua connector is prime for that",
2026-10-08T01:55:05Z,
https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6050570677), and the
work beside FIRST SLICE ("Yes, that's fine. I really think that first slice should try
to focus on the 'guts' the internals while ONE NODE work should be focused on developer
APIs and connectors.", 2026-10-08T02:45:18Z,
https://github.com/synnaxlabs/foundation/issues/1737#issuecomment-6051113411).
