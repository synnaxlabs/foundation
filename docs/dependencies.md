# Dependencies

Every third-party crate needs a person's approval and an entry here. Prefer the
canonical, production-grade implementation in any language, compiled into the binary.
Evidence: `docs/research/r7-dependencies.md`.

| Crate | Used by | Why | License |
| --- | --- | --- | --- |
| `serde_json` | `xtask` only | Read `cargo metadata` output for the layer check | MIT or Apache-2.0 |
