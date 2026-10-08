- **C5 + KINDS OWN THEIR CONFIG** Each kind owns parse, check, discover, and run, built
  on shared components. `config` never knows a kind's fields. A kind returns diagnostics
  with positions plus the channels it reads and writes. Calculations are a kind. The
  engine (powerful, Arc-style, in Rust: rule-based filtering, waveforms, FFT) is a
  separate later design. Locked guarantees: data only through hub sessions, a plan-time
  check, resource isolation (own threads with a budget, or another node), determinism
  (time only from samples and ctx), and outputs on the calculation's own index.
  Supersedes: r3 single-expression language, r3 first-input index, r8 JSON Schema
  check in `config`. The channels of a kind's `check` are from the mesh's side:
  `reads` are the channels it reads from the mesh (commands for the device, or samples
  it sends out), and `writes` the channels it writes to the mesh (samples from the
  device) (`laptop.architect-2`, 2026-10-07T15:17:11Z:
  https://github.com/synnaxlabs/foundation/issues/1082#issuecomment-6040866688; item on
  #1731, 2026-10-08T06:24:16Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6053789750).
