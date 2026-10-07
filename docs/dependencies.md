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
| `blake3` | `types` (`types::digest`, for `spec` and `blob`) | Hashes of spec chunks and blobs (R4 SETTLED, r7 area 7) | CC0-1.0 or Apache-2.0 | 1.8.7 | 2026-10-04 |
| `rustix` | `os` | Reserve, commit, and purge pool pages; OS calls behind `env` | Apache-2.0 with LLVM exception, Apache-2.0, or MIT | 1.1.5 | 2026-10-04 |
| `tokio` | `os`, `transport`, `node`, benchmarks | One `LocalRuntime` per shard (C2) | MIT | 1.53.2 | 2026-10-04 |
| `rustls` | `transport`, `bench/carrier` | TLS 1.3 for the TCP and relay carriers (r7 area 7) | Apache-2.0, ISC, or MIT | 0.23.x stable | 2026-10-04 |
| `aws-lc-rs` | `transport`, signing, `mesh` (grants), `secret` (sealing) | The only crypto provider (r7 area 7) | ISC and (Apache-2.0 or ISC) | 1.18.1 | 2026-10-04 |
| `noq-proto` | `transport` | Sans-I/O QUIC core (TRANSPORT SHAPE LOCKED, r5) | MIT or Apache-2.0 | 1.3.0 | 2026-10-04 |
| `crc32c` | `buffer`, the `buffer_open` fuzz target | Hardware CRC32C for write-ahead records (S4, r2 Q4, #48) | Apache-2.0 or MIT | 0.6.8 | 2026-10-04 |
| `bytes` | `transport`, `connector` (`http`), `connector-influx` (feature `sim`) | The buffer type of `noq-proto`'s stream and datagram calls (#55), and of the body of each `connector` HTTP request and response, as `hyper` takes it (R7), also in the simulated HTTP servers | MIT | 1.12.1 | 2026-10-04 |
| `clap` | `ops` | The command line, generated from the operation table (C7, r7 area 8) | MIT or Apache-2.0 | 4.6.7 | 2026-10-05 |
| `schemars` | `ops` | JSON Schemas of operation inputs for MCP tools (C7, r7 area 8) | MIT | 1.2.2 | 2026-10-05 |
| `serde` | `ops` | Typed operation input and output for `--json` and MCP | MIT or Apache-2.0 | 1.0.229 | 2026-10-05 |
| `serde_json` | `ops` | JSON for `--json` and MCP | MIT or Apache-2.0 | 1.0.151 | 2026-10-05 |
| `unicode-ident` | `config-hcl` | Identifiers outside ASCII, as HCL reads them (HCL IDENTIFIERS, #263) | (MIT or Apache-2.0) and Unicode-3.0 | 1.0.26 | 2026-10-05 |
| `zeroize` | `secret` | Overwrite a secret value when it drops; a plain write may be optimized away, and a volatile write needs `unsafe` (#230) | Apache-2.0 or MIT | 1.9.0 | 2026-10-05 |
| `libloading` | `connector-ni` | Loads NI's DAQmx driver at run time, so the binary runs on hosts without it (R7). The person: "Ok libloading is fine" (#436) | ISC | 0.9.0 | 2026-10-06 |
| `libc` | `os` | `clock_gettime`, `adjtimex`, and `ntp_gettime` for `env::clock` and `env::wall` (CLOCK SUSPEND, OS CLOCK BOUND), and `fcntl(F_PREALLOCATE)` on macOS for `env::files` create (architect, #931, https://github.com/synnaxlabs/foundation/issues/931#issuecomment-6030986099); `rustix` has no `adjtimex`, no `ntp_gettime`, and no raw clock on macOS. The person: "Yes" (#117) | MIT or Apache-2.0 | 0.2.190 | 2026-10-06 |
| `getrandom` | `os` | Random bytes from the OS for `env::entropy`; it handles short reads and the 256-byte limit of `getentropy`. The person: "Yes" (#117) | MIT or Apache-2.0 | 0.4.3 | 2026-10-06 |
| `hyper` | `connector` (`http`) | The one HTTP client of every connector, with features `client` and `http1` only (R7, #341). The feature `server` only under the cargo feature `sim` of `connector`, for the simulated HTTP servers; the product build stays `client` and `http1`. The server gets no timer, so the clock value it reads changes nothing. The person approved it on #1151 (https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6039824839, https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6042756353). Its tree: `httparse`, `want`, `try-lock`, `atomic-waker`, `smallvec`, `pin-project-lite`, `futures-core`, `itoa`, and `tokio` with `sync` only; `server` adds `httpdate`. The person: "Yes, R7 stands" (#213, https://github.com/synnaxlabs/foundation/issues/213#issuecomment-6022143908). Approved on #341 (https://github.com/synnaxlabs/foundation/issues/341#issuecomment-6021382466) | MIT | 1.12.0 | 2026-10-06 |
| `http` | `connector` (`http`); `connector-influx` (feature `sim`) | The request and response types of `hyper`, which the client's surface uses (R7, #341, https://github.com/synnaxlabs/foundation/issues/341#issuecomment-6021382466), and of the simulated HTTP servers | MIT or Apache-2.0 | 1.5.0 | 2026-10-06 |
| `http-body` | `connector` (`http`) | The body trait of `hyper`, for the request body and to read the response (R7, #341, https://github.com/synnaxlabs/foundation/issues/341#issuecomment-6021382466) | MIT | 1.1.0 | 2026-10-06 |

One exception to "`aws-lc-rs` is the only crypto provider": `noq-proto`'s `rustls`
feature pulls RustCrypto's `aes-gcm`, used only for the QUIC Retry integrity tag, whose
key is public (RFC 9001 section 5.8). The person accepted it on 2026-10-04 until a
local patch of `noq-proto` uses the `aws-lc-rs` AEAD for that tag: "no opening github
issues on other peoples projects. we should do a local patch instead". Remove the
exception when the patch lands (#55).

## Local patches

We never open issues or PRs on projects outside `synnaxlabs`. To change a dependency,
carry a local patch through `[patch.crates-io]` in the root `Cargo.toml`, keep the
change small, and list it here with its reason. The patched copy lives in
`patches/<crate>/` (LOCAL PATCHES in `docs/decisions.md`).

## Tests, benchmarks, and tools

These never ship in the binary.

| Crate | Used by | Why | License | Version | Approved |
| --- | --- | --- | --- | --- | --- |
| `serde_json` | `xtask` | Read `cargo metadata` output for the layer check | MIT or Apache-2.0 | | Bootstrap |
| `proptest` | All crates (dev) | Property tests (testing layer 1) | MIT or Apache-2.0 | 1.11.0 | 2026-10-04 |
| `loom` | `ring`, `block` (`cfg(loom)`) | Exhaustive checks of wake protocols and atomics | MIT | 0.7.2 | 2026-10-04 |
| `shuttle` | `ring`, `block` (dev) | Randomized (PCT) checks of larger concurrent models | Apache-2.0 | 0.9.5 | 2026-10-04 |
| `divan` | Benchmarks | Wall-time benchmarks that also count allocations | MIT or Apache-2.0 | 0.1.21 | 2026-10-04 |
| `libfuzzer-sys` | Fuzz targets | Coverage-guided fuzzing with `cargo-fuzz` (testing layer 2) | (MIT or Apache-2.0) and NCSA | 0.4.13 | 2026-10-04 |
| `influxdb-line-protocol` | `connector-influx` (feature `sim`, off by default) | The parser of the simulated InfluxDB store. It is InfluxData's own parser (InfluxDB 3), so it checks our writer independently. Its tree: `bytes`, `log`, `nom` 7, `smallvec`, `snafu` 0.7 (with `syn` 1); no I/O and no clock. Architecture approved by `laptop.architect-2` (https://github.com/synnaxlabs/foundation/issues/1151#issuecomment-6032723969) | MIT or Apache-2.0 | 2.0.0 | Pending (#1151) |
| `hdrhistogram` | Benchmarks | Latency percentiles | MIT or Apache-2.0 | 7.6.0 | 2026-10-04 |
| `github.com/hashicorp/hcl/v2` (Go module, and the modules in its `go.sum`) | `oracles/conformance/hcl/main.go`, run by hand | The verdicts of HCL on the texts of the HCL oracle | MPL-2.0; the `go.sum` modules: MIT, Apache-2.0, BSD-3-Clause | 2.25.0 | 2026-10-05 (#460) |
| `github.com/zclconf/go-cty` (Go module, required by `hcl/v2`) | `oracles/conformance/hcl/main.go`, run by hand | The type of an HCL literal, to find a number | MIT | 1.19.0 | 2026-10-05 (#460) |
| `core_affinity` | Benchmarks | Pin benchmark threads to cores on Linux | MIT or Apache-2.0 | 0.8.3 | 2026-10-04 |
| `noq` | `bench/carrier` only | QUIC endpoint on Tokio for the carrier benchmark (#10). No product crate depends on it. Its `noq-proto` brings `aes-gcm` for Retry tags only; packets use aws-lc-rs | MIT or Apache-2.0 | 1.3.0 | 2026-10-04 |
| `tokio-rustls` | `bench/carrier` only | TLS over TCP on Tokio for the carrier benchmark (#10) | MIT or Apache-2.0 | 0.26.6 | 2026-10-04 |
