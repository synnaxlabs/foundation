# Connector conformance

The scenarios that each connector kind passes, on two sim nodes: the plant, with the
rig of `connector::testing` and the connector, and the device, with the kind's
simulator. The scenarios use only public items: the rig of `connector::testing`,
`hub::testing::Env`, the sim, `env`, `document`, `types`, and the `kind`, `status`,
and `cancel` modules of `connector`. The record CONNECTOR CONFORMANCE
(`docs/decisions/connectors/connector-conformance.md`) gives each scenario.

`scenarios.rs` holds the scenarios, the input type `Suite`, and `suite!`, which expands
to one `#[test]` for each scenario. `<kind>/main.rs` is the root of the test binary of
one kind: its `Suite` and one `suite!` line. Each kind crate runs its suite as its
`conformance` test. The `[[test]]` entry in the `Cargo.toml` of each kind is part of
this oracle: to remove it is to weaken the oracle.

```sh
cargo test -p connector-opcua --all-features --test conformance
cargo test -p connector-influx --all-features --test conformance
```

## Rules

- People own the scenarios. A kind that fails one has a defect, or the person changes
  the scenario.
- A new scenario goes into `suite!`, so it reaches each kind at once.
- A kind gives plain inputs: its table, a setup, its simulator, the name of its
  progress count, its backoff form, and the configs that its check refuses. A kind
  that needs a hook to pass a scenario has a scenario that is wrong.
- A new kind adds `<kind>/main.rs` and its `[[test]]` entry.
