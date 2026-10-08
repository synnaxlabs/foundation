# MVP


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
