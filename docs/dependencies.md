# Dependencies

Every third-party crate needs a person's approval and an entry here. Prefer the
canonical, production-grade implementation in any language, compiled into the binary.
Evidence: `docs/research/r7-dependencies.md`.

Declare each crate once in `[workspace.dependencies]` in the root `Cargo.toml`, and
use it with `workspace = true`. The version column is the latest release on the day of
approval; pin the version you build against there.

## Runtime

| Crate | Used by | Why | License | Version | Approved |
| --- | --- | --- | --- | --- | --- |
| `blake3` | `spec`, `blob` | Hashes of spec chunks and blobs (R4 SETTLED, r7 area 7) | CC0-1.0 or Apache-2.0 | 1.8.7 | 2026-10-04 |
| `rustix` | `block`, `os` | Reserve, commit, and purge pool pages; OS calls behind `env` | Apache-2.0 with LLVM exception, Apache-2.0, or MIT | 1.1.5 | 2026-10-04 |
| `tokio` | `os`, `transport`, `hub`, `node`, benchmarks | One `LocalRuntime` per shard (C2) | MIT | 1.53.2 | 2026-10-04 |
| `rustls` | `transport` | TLS 1.3 for the TCP and relay carriers (r7 area 7) | Apache-2.0, ISC, or MIT | 0.23.x stable | 2026-10-04 |
| `aws-lc-rs` | `transport`, signing | The only crypto provider (r7 area 7) | ISC and (Apache-2.0 or ISC) | 1.18.1 | 2026-10-04 |
| `noq-proto` | `transport` | Sans-I/O QUIC core (TRANSPORT SHAPE LOCKED, r5) | MIT or Apache-2.0 | 1.3.0 | 2026-10-04 |
| `hcl-edit` | `config-hcl` | Parse HCL and keep its formatting (r3 section 2); our own checker compiles the tree | MIT or Apache-2.0 | 0.9.7 | 2026-10-04 |

## Tests, benchmarks, and tools

These never ship in the binary.

| Crate | Used by | Why | License | Version | Approved |
| --- | --- | --- | --- | --- | --- |
| `serde_json` | `xtask` | Read `cargo metadata` output for the layer check | MIT or Apache-2.0 | | Bootstrap |
| `proptest` | All crates (dev) | Property tests (testing layer 1) | MIT or Apache-2.0 | 1.11.0 | 2026-10-04 |
| `loom` | `ring`, `block` (dev) | Exhaustive checks of wake protocols and atomics | MIT | 0.7.2 | 2026-10-04 |
| `shuttle` | `ring`, `block` (dev) | Randomized (PCT) checks of larger concurrent models | Apache-2.0 | 0.9.5 | 2026-10-04 |
| `divan` | Benchmarks | Wall-time benchmarks that also count allocations | MIT or Apache-2.0 | 0.1.21 | 2026-10-04 |
| `libfuzzer-sys` | Fuzz targets | Coverage-guided fuzzing with `cargo-fuzz` (testing layer 2) | (MIT or Apache-2.0) and NCSA | 0.4.13 | 2026-10-04 |
| `hdrhistogram` | Benchmarks | Latency percentiles | MIT or Apache-2.0 | 7.6.0 | 2026-10-04 |
| `core_affinity` | Benchmarks | Pin benchmark threads to cores on Linux | MIT or Apache-2.0 | 0.8.3 | 2026-10-04 |
