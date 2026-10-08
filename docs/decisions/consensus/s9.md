- **S9 (as revised by R4 SETTLED)** `State { spec, runtime }`. Only `apply` changes the
  spec (see X28); only the mesh changes runtime state. `plan` compares files with the
  spec only. Raft holds each region's spec pointer (version and root hash) and runtime
  state. Fast state (status, health, clock error, control state, reader positions)
  stays out of Raft. Supersedes: S9 name-hierarchy tree, S9 gossip hints.
  Until peers get chunks, only a region with one voter applies a change (SPEC
  CHANGE); #1231 ends this. Decided by `laptop.architect`, 2026-10-08T11:03:36Z
  (https://github.com/synnaxlabs/foundation/issues/1741#issuecomment-6058455178).
