- **OWN REPO (revises C9a)** Foundation lives in its own private repository,
  `synnaxlabs/foundation`, with one Cargo workspace: `crates/` (crate list in section
  4), `xtask/`, `oracles/`, and later `sdk/` and `bench/`. Every PR runs the layer
  check (`cargo xtask layers`).
