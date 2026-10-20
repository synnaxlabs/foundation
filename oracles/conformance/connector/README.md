# Connector conformance

The scenarios that each connector kind passes, on two sim nodes: the plant, with the
rig of `connector::testing` and the connector, and the device, with the kind's
simulator. The scenarios use only public items: the rig of `connector::testing`,
`hub::testing::Env`, the sim, `env`, `document`, `types`, and the `kind`, `status`,
`reader`, and `cancel` modules of `connector`. The record CONNECTOR CONFORMANCE
(`docs/decisions/connectors/connector-conformance.md`) gives each scenario.

`scenarios.rs` holds the scenarios, the input type `Suite`, and `suite!`, which expands
to one `#[test]` for each scenario, and two that check that a scenario that is not done
at `RING` fails. Its `tests` module tests how a run of scenario 6 picks and replays
its seeds. `<kind>/main.rs` is the root of the test binary of one kind: its
`Suite`, one `suite!` line, and each test of that kind alone, which uses
`scenarios::run` and the `pub(crate)` items of `scenarios.rs`. Each kind crate runs its
suite as its `conformance` test.
The `[[test]]` entry in the `Cargo.toml` of each kind is part of this oracle: to remove
it is to weaken the oracle.

```sh
cargo test -p connector-modbus --all-features --test conformance
cargo test -p connector-opcua --all-features --test conformance
cargo test -p connector-influx --all-features --test conformance
```

A failed run of `heals_after_random_faults` prints the command that replays it, such as
`CONFORMANCE_SEED=0x2a cargo test -p connector-modbus --all-features --test conformance
heals_after_random_faults`. A range `<start>..<end>` in hex, the end excluded, such as
`CONFORMANCE_SEED=0x0..0xf4240`, runs more seeds than the 1000 of each merge.

## Rules

- People own the scenarios. A kind that fails one has a defect, or the person changes
  the scenario.
- A new scenario goes into `suite!`, so it reaches each kind at once.
- A kind gives plain inputs: its table, a setup, its simulator, the name of its
  progress count, its backoff form, the configs that its check refuses, a form of its
  config that reads each secret, and an echo simulator. A kind that needs a hook to
  pass a scenario has a scenario that is wrong.
- The simulator of a kind that subscribes changes its data at least every 100 ms.
- A new kind adds `<kind>/main.rs` and its `[[test]]` entry.
