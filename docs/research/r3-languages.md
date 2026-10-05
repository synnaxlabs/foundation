# Foundation research 3: definition language, file layout, calculation language

Fork 3 of 8. Covers K1 (definition language), K2 (file layout and composition), C5
(calculation language), and whether K1 and C5 can be one language. Written 2026-10-04.

## 0. Summary

| Question | Recommendation |
|---|---|
| K1 | HCL syntax with Foundation's own semantics: data-shaped files, no loops, one checker. |
| K2 | Directories mirror branches of the name tree; a file defines only names in its directory's branch; full names everywhere; no imports. |
| C5 | One typed, unit-checked expression per calculation, with built-in window and as-of functions, compiled to a column-at-a-time evaluator over S2 buffers. No loops, no user functions. |
| One language? | Yes. Calculation expressions are HCL expressions in the same files, checked by the same checker. It holds only while calculations stay single expressions. |

## 1. The decisive constraint for K1: round-trip

K3 locked `discover` (writes definitions into files) and `export` (writes a running
spec into files). Both need a language that a machine can write and later edit without
destroying human edits. That splits the candidates in two:

- **Data-shaped languages** (TOML, HCL, KDL, YAML, JSON5) round-trip. Format-preserving
  editors exist in Rust: `toml_edit` for TOML, `hcl-edit` for HCL, `kdl-rs` for KDL.
- **Programmable languages** (Starlark, Jsonnet, Nickel, CUE, Pkl, KCL, Dhall, SDK code)
  don't. A loop or a function call can't be regenerated from its expanded output.
  Terraform shows the same limit: `terraform plan -generate-config-out` can't produce
  `for_each` blocks ([Terraform import docs][tf-import]; single source, unverified
  beyond HashiCorp's docs).

So the definition language must be data-shaped. Repetition is handled by struct
templates (S7) and by `discover`, never by loops.

## 2. K1: the definition language

**Recommendation:** use HCL syntax, and give it Foundation's own semantics. Parse it
with `hcl-edit`, which preserves formatting, and compile the syntax tree with our own
checker. Never use HCL's evaluation library.

### Evidence

- **HCL is the proven middle ground.** HashiCorp built it because JSON has no comments
  and is verbose, YAML confused beginners about structure, and full languages such as
  Ruby allow too much ([hashicorp/hcl README][hcl-readme], [Packt summary][hcl-packt]).
- **The grammar fits.** The native syntax has blocks, attributes, and expressions, with
  arithmetic, comparison, logic, conditionals, and function calls, but no user-defined
  functions. Functions come only from the host's table ([HCL native syntax
  spec][hcl-spec]). That is exactly what calculations need (section 4).
- **Rust support exists.** `hcl-rs` 0.19 parses, formats, and evaluates HCL
  ([docs.rs][hcl-rs], [lib.rs][hcl-rs-lib]). `hcl-edit` keeps whitespace, comments, and
  spans, and is "to HCL what toml_edit is to TOML" ([docs.rs][hcl-edit],
  [lib.rs][hcl-edit-lib]). Its own docs say the API is still young and may break.
- **A telemetry pipeline already took this path.** Grafana built River (now the Alloy
  syntax), an HCL-like language for Grafana Agent ([Agent docs][river-docs]). Their RFC
  says they first used HCL, then left its *evaluation* library (gohcl and cty) because
  it needed boilerplate and couldn't evaluate expressions continuously at runtime
  ([River RFC 0005][river-rfc]). Foundation avoids that problem by using only HCL's
  syntax tree and compiling it with its own checker.
- **Agents fail on knowledge, not syntax.** In IaC-Eval, GPT-4 passed only 19.4% of
  Terraform tasks while scoring 86.6% on Python ([IaC-Eval, NeurIPS 2024][iaceval]). A
  follow-up study raised overall success from 27.1% to 62.6% by injecting configuration
  knowledge ([arXiv 2512.14792][iac-taxonomy]). For Foundation, embedded docs, schemas,
  and `plan` errors (C7) matter more than syntax, but a familiar syntax still avoids the
  low-resource penalty below.
- **Unfamiliar languages cost adoption.** LLMs score much lower on low-resource
  languages ([survey, arXiv 2410.03981][lowres]). Dagger dropped its CUE SDK because
  users' main complaint was having to learn CUE, and CUE bugs went unfixed upstream
  ([Dagger blog][dagger-cue]). Grammar prompting helps LLMs with new DSLs but doesn't
  remove the gap ([NeurIPS 2023][grammar-prompting]).

### Example

```hcl
# types.fdn (root branch)
enum "Mode" {
  type   = u8
  values = { off = 0, manual = 1, auto = 2, fault = 255 }
}

struct "MotorState" {
  speed   = f64("rpm")
  current = f32("A")
  mode    = Mode
  fault   = optional(u16)
}
```

```hcl
# site_a/plc_7.fdn (written by: foundation discover opc.tcp://10.0.0.7:4840)
index "site_a.plc_7.time" {
  error = site_a.gateway_1.clock.error
}

channel "site_a.plc_7.quality" {
  type = quality
}

channel "site_a.plc_7.pt_101" {
  type    = f64
  unit    = "kPa"
  index   = site_a.plc_7.time
  quality = site_a.plc_7.quality
}

channel "site_a.plc_7.pt_102" {
  type    = f64
  unit    = "kPa"
  index   = site_a.plc_7.time
  quality = site_a.plc_7.quality
}

channel "site_a.plc_7.valve_1.cmd" {
  type = bool # no index: gets a private one (A7)
}

channel "site_a.plc_7.motor_1" {
  type  = MotorState # expands to motor_1.speed, .current, .mode, .fault (S7)
  index = site_a.plc_7.time
}

connector "opcua" "site_a.plc_7" {
  node     = site_a.gateway_1
  url      = "opc.tcp://10.0.0.7:4840"
  password = secret("plc_7_password") # K4

  in  { channel = site_a.plc_7.pt_101,      address = "ns=2;s=PT101" }
  in  { channel = site_a.plc_7.pt_102,      address = "ns=2;s=PT102" }
  in  { channel = site_a.plc_7.motor_1,     address = "ns=2;s=Motor1" }
  out { channel = site_a.plc_7.valve_1.cmd, address = "ns=2;s=Valve1" }
}
```

```hcl
# site_a/branch.fdn
voters {
  nodes = [site_a.gateway_1, site_a.gateway_2, site_a.gateway_3]
}

retention {
  select = ["site_a.**"]
  keep   = "3d"
}

retention {
  select = ["site_a.**.cmd"]
  keep   = "none"
}
```

Semantics on top of the syntax:
- Every reference is a full name (`site_a.plc_7.pt_101`), which is a valid HCL
  traversal. Selectors with wildcards are strings, because `**` isn't an expression.
- Types are bare names (`f64`, `MotorState`). Units, durations, and secrets use host
  functions or strings.
- `for` expressions, `count`, `for_each`, modules, and variables don't exist. The
  checker rejects them with a fix-it hint ("use a struct type, or run discover").
- `foundation fmt` gives one canonical layout, so diffs stay clean.

### Rejected alternatives

| Candidate | Downsides that rule it out |
|---|---|
| TOML | No expressions, so calculations become unchecked strings and there is no single language. Nested repeated tables (`[[connector.in]]`) get awkward. References are strings. Its real advantage is that taplo provides schema-aware completion through JSON Schema for free ([taplo][taplo]). |
| YAML | No expressions, indentation errors, and implicit typing. `serde_yaml` is deprecated, and the `serde_yml` fork was archived after an unsoundness advisory ([RUSTSEC-2025-0068][rustsec-yml], [users.rust-lang.org][yaml-dep]). |
| JSON5 | No expressions, verbose, and few people write it by hand. |
| KDL | A data language with format-preserving `kdl-rs` and a v2 spec ([kdl-rs][kdl]), but no expressions and little LLM familiarity. |
| CUE | Only a Go implementation. `cue-rs` statically links the Go runtime through cgo ([docs.rs cue-rs][cue-rs]), which complicates cross-compiling one binary for every target. It's programmable, so it can't round-trip, and Dagger dropped it (above). |
| Pkl | `libpkl` is about 100 MB when statically linked ([libpkl docs][libpkl]), which breaks P1's footprint target. Rust bindings talk to a separate process ([pklrust][pklrust]). |
| KCL | A Rust core, but it's a full compiler toolchain that uses LLVM ([KCL intro][kcl]). It's programmable and niche. |
| Nickel | Rust, with contracts, 1.0 since 2023 ([Tweag][nickel]). It's programmable, has little adoption, and has a learning curve (the Dagger lesson). |
| Dhall | The Rust crate's last release was 0.12.0 in August 2022 ([lib.rs dhall][dhall]). |
| Starlark | Deterministic and hermetic, with a mature Rust implementation used by Buck2 ([Wikipedia][starlark-wiki], [starlark-rust][starlark-rs]). But it's code, so `discover` and `export` can't write it. |
| Jsonnet | Fast Rust implementation ([jrsonnet][jrsonnet]), but it's programmable and has the same round-trip problem. |
| A fully own syntax | The nicest type syntax (`struct MotorState { speed f64 rpm }`), but unfamiliar to agents (low-resource gap) and expensive. Synnax's Arc is about 45,000 hand-written lines of Go plus an 18,000-line generated parser, with a 5,145-line LSP. Oracle is about 57,000 lines. |
| SDK code (Pulumi, CDK) | Not deterministic and needs a language runtime. HashiCorp archived CDK for Terraform on 2025-12-10 for lack of product-market fit ([terraform-cdk][cdktf], [Pulumi][cdktf-pulumi]). SDK programs can still write definition files through the formatter library. |

### Risks

- **Terraform habits.** Users will try `for_each`, `count`, and modules. The checker
  must reject them with clear fix-it messages.
- **`hcl-edit` is young.** If it limits us, we write our own parser for the same
  grammar, as Grafana did with River. The files don't change.
- **Name rules must match HCL identifiers.** A segment must start with a letter or
  `_`. HCL also allows `-` inside identifiers ([HCL spec][hcl-spec]), so `a-b` lexes as
  one name. A3's name rules should forbid `-`, and `fmt` should always space operators.
- **We must build the LSP.** Schema-aware completion for connector config and channel
  names needs our checker. Arc's LSP (5,145 lines of Go) is the in-house size
  reference.

**Decision for the user:** use HCL syntax with Foundation's own semantics (data-shaped,
no loops, one checker), instead of TOML?

## 3. K2: file layout and composition

**Recommendation:** a directory is a branch of the name tree. A file defines only names
in its directory's branch. Every reference is a full name, and there are no imports.

```
mesh/                  root branch ("**")
  mesh.fdn             root voters, time policy
  types.fdn            Mode, MotorState
  site_a/              branch "site_a."; only names under site_a.
    branch.fdn         voters, retention, access for site_a
    nodes.fdn
    plc_7.fdn          written by discover, then edited by people
    calcs.fdn
  cloud/
    influx.fdn
```

- **Directories map to K5's voters.** A `voters` block in `site_a/` governs
  `site_a.**`. `plan` and `apply` work per directory, so a branch commits on its own
  (K5), and Git code owners per directory match branch owners.
- **Policies can't reach outside their branch.** A selector in `site_a/` that matches
  names outside `site_a.` is a `plan` error.
- **A small mesh stays flat.** A branch gets its own directory only when it needs its
  own voters or reviewers.
- **Full names everywhere.** An agent can grep `site_a.plc_7.pt_101` across files, plans,
  logs, and channels and find every use. Relative names would break that.
- **No imports.** Every file under the root loads together. References resolve by full
  name, and types are visible everywhere.
- **One kind of file.** `discover` writes an ordinary file, one per connector. Running it
  again proposes a diff of that file and preserves human edits through `hcl-edit`. It
  never overwrites. `export` writes this same layout.

### Rejected alternatives

- **One big file**: merge conflicts, and no per-branch ownership in review.
- **Any layout, with a `branch =` attribute**: ownership becomes invisible in review,
  and code owners can't map to it.
- **Relative names inside a directory**: shorter, but not greppable.
- **Imports or modules** (Terraform modules, CUE packages): one more concept, with
  nothing gained once names are global.
- **Separate generated files** (`*.gen.fdn`): two kinds of file, and edits are lost when
  they're regenerated.

### Risks

- Moving a channel to another branch means moving it to another file and another set of
  voters. `plan` must show that as one change.
- The `.fdn` extension is a placeholder name.

**Decision for the user:** directories mirror branches of the name tree, files define
only names in their branch, full names everywhere, and no imports?

## 4. C5: the calculation language

**Recommendation:** a calculation is one typed, unit-checked expression, written in the
same HCL expression syntax. It has built-in window and as-of functions and is compiled
to a column-at-a-time evaluator over S2 buffers. There are no loops and no user
functions.

```hcl
# site_a/calcs.fdn
connector "calc" "site_a.plc_7.pt_101_avg" {
  node = site_a.gateway_1
  expr = avg(site_a.plc_7.pt_101, "1s") # output: f64 kPa on pt_101's index
}

connector "calc" "site_a.plc_7.dp" {
  node = site_a.gateway_1
  expr = site_a.plc_7.pt_101 - site_a.plc_7.pt_102 # same unit, same index
}

connector "calc" "site_a.plc_7.pt_103" {
  node = site_a.gateway_1
  unit = "kPa"
  expr = site_a.plc_7.pt_103_raw * 0.0305 + 1.2 # dimensionless result takes the unit
}
```

### Semantics

- **The calculation defines its output channel.** The output's type, unit, and index are
  inferred from `expr`, which removes one block per calculation.
- **Types and units are checked against channel definitions** at `plan`, following
  Arc's dimensional analysis (`arc/docs/spec.md:139-190`). Adding values requires
  compatible units, `convert(x, "psi")` changes units, and a dimensionless result can
  take the declared output unit (raw scaling).
- **Built-in functions provide state and time.** Rolling windows (`avg`, `min`, `max`,
  `sum` over a duration or a count), `derivative`, `integral`, and downsampling
  (`every("1s", avg(x))`, which creates its own index). They mirror Kepware's Average,
  Min, Max, and Derived tags ([PTC][kepware], [Software Toolbox][kepware-stb]) and
  Ignition expression tags ([Ignition docs][ignition]).
- **Inputs on different indexes align as-of.** The output uses the first input's index,
  or an explicit `index`, and every other input contributes its last value at or before
  each timestamp. This is kdb+'s `aj`, and `wj` covers window joins ([kx joins][kx],
  [TimeStored][timestored]).
- **Quality propagates** as S13 requires: the worst input quality, per sample, through
  the same as-of rule.

### Why this design

- **Speed comes from evaluating a column at a time.** A per-sample interpreter can't
  reach P1's 100M samples/s. Rhai runs 1 million loop iterations in 0.14 s on one core
  ([Rhai benchmarks][rhai]; single source), which is about 7M operations per second.
  Evaluating one operator over a whole buffer spreads the interpretation cost across
  every value in it. MonetDB/X100 showed that a tuple-at-a-time engine spent 50 to 80%
  of CPU time on interpretation ([X100][x100], [ClickHouse][ch-vec]). Throughput for our
  operators is an estimate until the C2 benchmark measures it.
- **No loops means bounded cost.** Cost grows linearly with input size, so a calculation
  is safe to run even if an agent or an outside user wrote it. VRL makes the same choice
  and is checked at compile time ([vrl.dev][vrl], [Vector blog][vrl-blog]).
- **Deterministic for simulation (T1).** Time comes only from sample timestamps,
  there's no I/O, and reductions run in a fixed order, so floating-point sums are
  identical on every run.

### Rejected alternatives

| Candidate | Downsides that rule it out |
|---|---|
| Arc or another WASM target | Strong sandbox, and deterministic with NaN canonicalization and fuel ([wasmtime][wasmtime-det], [interrupts][wasmtime-int]). But each batch is copied into WASM linear memory, which conflicts with S2's no-copy rule, and Arc's sequences and stages are control logic, which D2 excludes. Kept as the candidate for user plugins (D5). Ideas taken: units, series operations, compile-time error taxonomy. |
| Lua, Rhai, Starlark at runtime | Interpreted per sample (see Rhai above), loops make cost unbounded, and types can't be checked against channel definitions. |
| Streaming SQL (Arroyo, RisingWave, Materialize) | A query planner and a state backend are heavy for a Raspberry Pi, and tables and joins don't match per-channel expressions. Arroyo is a Rust engine and is now part of Cloudflare ([Cloudflare][arroyo]). Its window semantics (tumbling, hopping) are worth borrowing. |
| InfluxDB Flux | A cautionary example. InfluxData stopped developing it, citing demand for SQL, a v3 engine Flux couldn't move to, and lower Flux performance on that engine ([future of Flux][flux]). |
| kdb+/q | As-of and window semantics are borrowed. Its terse syntax and commercial license are not. |

### Risks

- **The expression ceiling.** Multi-step logic with local variables or state machines
  doesn't fit. Logic beyond one expression goes to SDK programs, or later to WASM
  plugins. A `locals` block inside a calculation could be added later without breaking
  anything.
- **As-of alignment can hide staleness.** A slow input repeats its last value. This is
  the same problem as the S7 optional-field gap, and the same fix should cover both
  (fork 2's topic).

**Decision for the user:** calculations are single typed, unit-checked expressions with
built-in window and as-of functions, compiled to a column-at-a-time evaluator, with no
loops or user functions?

## 5. One language for definitions and calculations?

**Recommendation:** yes. That means one syntax, one parser, one formatter, one LSP, and
one checker, with two evaluation modes. Definition attributes are checked once at
`plan`. A calculation's `expr` is compiled to the streaming evaluator.

- **The single checker is the real gain.** A calculation that references
  `site_a.plc_7.pt_101` is resolved and unit-checked against that channel's definition
  in the same pass, so a typo or unit mismatch is a `plan` error, not a runtime one.
- **It works because the two halves share a shape.** Both are declarative, have no
  loops, and get their functions from the host. HCL's grammar already has exactly that
  shape ([HCL spec][hcl-spec]).
- **It holds only while calculations stay single expressions.** If calculations ever
  need statements, loops, or user functions, they should become a separate plugin
  surface (WASM, D5), not grow inside the definition language. Growing one language
  toward both jobs would recreate Flux.
- **One honest cost.** Some attributes are evaluated once (`keep = "3d"`), and one
  (`expr`) runs on every series. Docs and the LSP must make `expr` visibly different.

**Decision for the user:** one language: HCL syntax for definitions, with calculation
expressions written in the same files and checked by the same checker?

## 6. Sources

In-repo (read 2026-10-04):
- `arc/docs/spec.md:139-190` (units and dimensional analysis), `:420-433` (stateful
  variables), `:754-766` (restrictions), `:793-797` (WASM target).
- Arc size: `arc/go` hand-written about 45,000 lines of Go (parser 18,303 generated),
  LSP 5,145, formatter 1,576. Oracle: about 56,752 lines of Go.
- `schemas/synnax/channel.oracle:20-63`, `schemas/x/control.oracle:23-30` (Oracle's own
  struct and enum syntax).

External:

[tf-import]: https://developer.hashicorp.com/terraform/language/import
[hcl-readme]: https://github.com/hashicorp/hcl
[hcl-packt]: https://hub.packtpub.com/what-is-hcl-hashicorp-configuration-language-how-does-it-relate-to-terraform-and-why-is-it-growing-in-popularity
[hcl-spec]: https://github.com/hashicorp/hcl/blob/main/hclsyntax/spec.md
[hcl-rs]: https://docs.rs/crate/hcl-rs/latest
[hcl-rs-lib]: https://lib.rs/crates/hcl-rs
[hcl-edit]: https://docs.rs/hcl-edit
[hcl-edit-lib]: https://lib.rs/crates/hcl-edit
[river-docs]: https://grafana.com/docs/agent/latest/flow/concepts/config-language/
[river-rfc]: https://github.com/grafana/agent/blob/97a55d0d908b26dbb1126cc08b6dcc18f6e30087/docs/rfcs/0005-river.md
[iaceval]: https://proceedings.neurips.cc/paper_files/paper/2024/hash/f26b29298ae8acd94bd7e839688e329b-Abstract.html
[iac-taxonomy]: https://arxiv.org/abs/2512.14792
[lowres]: https://arxiv.org/html/2410.03981v3
[dagger-cue]: https://dagger.io/blog/ending-cue-support
[grammar-prompting]: https://proceedings.neurips.cc/paper_files/paper/2023/hash/cd40d0d65bfebb894ccc9ea822b47fa8-Abstract-Conference.html
[taplo]: https://github.com/tamasfe/taplo
[rustsec-yml]: https://rustsec.org/advisories/RUSTSEC-2025-0068
[yaml-dep]: https://users.rust-lang.org/t/serde-yaml-deprecation-alternatives/108868
[kdl]: https://github.com/kdl-org/kdl-rs
[cue-rs]: https://docs.rs/cue-rs
[libpkl]: https://pkl-lang.org/main/latest/libpkl/index.html
[pklrust]: https://docs.rs/crate/pklrust/0.9.0
[kcl]: https://www.kcl-lang.io/docs/user_docs/getting-started/intro
[nickel]: https://www.tweag.io/blog/2023-05-17-nickel-1.0-release
[dhall]: https://lib.rs/crates/dhall
[starlark-wiki]: https://en.wikipedia.org/wiki/Starlark
[starlark-rs]: https://github.com/facebook/starlark-rust
[jrsonnet]: https://lib.rs/crates/jrsonnet
[cdktf]: https://github.com/hashicorp/terraform-cdk
[cdktf-pulumi]: https://pulumi.com/docs/iac/comparisons/cdktf/
[kepware]: https://www.ptc.com/en/store/kepware/advanced-plug-ins/advanced-tags
[kepware-stb]: https://help.softwaretoolbox.com/faq/2513
[ignition]: https://docs.inductiveautomation.com/docs/8.1/appendix/expression-functions/advanced/runScript
[kx]: https://code.kx.com/q/basics/joins
[timestored]: https://timestored.com/kdb-guides/asof-time-joins-aj-wj
[rhai]: https://rhai.rs/book/about/benchmarks.html
[x100]: https://ir.cwi.nl/pub/16510/16510A.pdf
[ch-vec]: https://clickhouse.com/resources/engineering/vectorised-query-execution
[vrl]: https://vrl.dev
[vrl-blog]: https://vector.dev/highlights/2021-02-16-vector-remap-language/
[wasmtime-det]: https://docs.wasmtime.dev/examples-deterministic-wasm-execution.html
[wasmtime-int]: https://docs.wasmtime.dev/examples-interrupting-wasm.html
[arroyo]: https://blog.cloudflare.com/cloudflare-acquires-arroyo-pipelines-streaming-ingestion-beta/
[flux]: https://docs.influxdata.com/flux/v0/future-of-flux

Unverified or single-source claims: Terraform's `-generate-config-out` and `for_each`
limit (HashiCorp docs only); Rhai's throughput (Rhai's own benchmark only); the
`hcl-edit` maturity statement (the crate's own docs, echoed by lib.rs); column-at-a-time
throughput for Foundation's operators (an estimate until the C2 benchmark).
