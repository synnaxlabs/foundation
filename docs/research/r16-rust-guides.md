# R16: Rust guides to bring into Foundation

Date: 2026-10-04. Toolchain checked: Rust 1.98.1, Clippy 0.1.98 (Homebrew).

## Method

- Read the current rules: `CLAUDE.md`, `docs/claude/{rust,testing,performance,lessons}.md`,
  root `Cargo.toml`, `clippy.toml`, `rustfmt.toml`, `rust-toolchain.toml`, and the
  testing and crate-map parts of `docs/decisions.md`.
- Checked every URL below with an HTTP request (all 200 unless marked). Read the
  guides marked "read". Took licenses from each repository's license files or the
  GitHub API.
- Ran every lint and `clippy.toml` key below against a copy of the repository plus a
  probe crate, on Clippy 0.1.98. Counts are from the 1,282 lines of code that exist
  now (most bodies are `todo!()`). Lint names that Clippy 0.1.98 does not know are
  marked.
- The coordinator stopped the research early. Sources marked "URL only" were not read
  in this pass. Rules that come from them are marked "unverified".

## Sources

### Style and API

**Rust API Guidelines** (read)
- URL: https://rust-lang.github.io/api-guidelines/ (checklist:
  https://rust-lang.github.io/api-guidelines/checklist.html)
- License: MIT or Apache-2.0. Maintainer: Rust library API team (rust-lang org).
  Last push 2025-07.
- Why: the base vocabulary for names (`as_`/`to_`/`into_`, `iter`), conversions
  (`From`, `AsRef`), `Debug` on every public type, private fields, newtypes, and
  `# Errors`/`# Panics`/`# Safety` docs. Written for published crates; skip the
  crates.io items (C-METADATA, C-RELNOTES, C-SEMVER parts).

**Rust Style Guide** (read)
- URL: https://doc.rust-lang.org/stable/style-guide/
- License: MIT or Apache-2.0. Maintainer: Rust style team.
- Why: the default that `rustfmt` implements. The repo already runs `rustfmt` at 88
  columns, which overrides the guide's 100. Nothing more to adopt by hand.

**Microsoft Pragmatic Rust Guidelines** (read: checklist, universal, correctness,
resilience, UX, AI pages)
- URL: https://microsoft.github.io/rust-guidelines/ (source:
  https://github.com/microsoft/rust-guidelines)
- License: MIT. Maintainer: Microsoft. Version 2026.6, active (push 2026-09).
- Why: the most current, agent-aware guide. It has a written lint set
  (M-STATIC-VERIFICATION), `#[expect]` over `#[allow]`, panic means stop, mockable
  syscalls, strong types that guard invariants, and an AI section (one path per item,
  no tautological tests). Its error rule (canonical structs with backtraces) conflicts
  with the repo; see Errors.

**rust-analyzer style guide** (read)
- URL: https://rust-analyzer.github.io/book/contributing/style.html (source:
  `docs/book/src/contributing/style.md` in rust-lang/rust-analyzer)
- License: MIT or Apache-2.0 (rust-lang org). Maintainer: rust-analyzer team; written
  by Alex Kladov (matklad).
- Why: a large, fast Rust codebase's house rules. Good items: preconditions in
  types, push control flow to the caller, split a `bool` parameter into two
  functions, no `#[should_panic]`, no `#[ignore]`, coverage marks, few dependencies.
  Skip `anyhow` everywhere and the "avoid monomorphization with `dyn`" rule (hot path).

**matklad's testing and build posts** (read: "How to Test"; others URL only)
- https://matklad.github.io/2021/05/31/how-to-test.html (read)
- https://matklad.github.io/2023/11/15/push-ifs-up-and-fors-down.html (URL only)
- https://matklad.github.io/2021/02/27/delete-cargo-integration-tests.html (URL only)
- https://matklad.github.io/2022/07/04/unit-and-integration-tests.html (URL only)
- https://matklad.github.io/2021/09/04/fast-rust-builds.html (URL only)
- License: MIT or Apache-2.0 (blog repository). Maintainer: Alex Kladov.
- Why: the best short case for sans-I/O tests, a `check` function per feature,
  data-driven and expect (snapshot) tests, coverage marks, tests per layer, and
  "test features, not code". Foundation's layer 1 is already sans-I/O, so this fits.

**TigerBeetle TIGER_STYLE** (read in full)
- URL: https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md
- License: Apache-2.0. Maintainer: TigerBeetle Inc. Written for Zig.
- Why: the closest match to Foundation's goals: limits on everything, assertion
  density, pair assertions, positive and negative space, no allocation after
  start-up, 70-line functions, batching, control plane against data plane, units
  last in names. Some rules need Rust translation (`usize`, static allocation).

### Unsafe

**The Rustonomicon** (read: intro)
- URL: https://doc.rust-lang.org/nomicon/
- License: MIT or Apache-2.0. Maintainer: Rust project. Self-described as incomplete.
- Why: required reading for `block` and `ring` authors and reviewers (variance, drop
  check, uninitialized memory, atomics, exception safety). It is a reference, not a
  rule list.

**Unsafe Code Guidelines reference** (read: intro)
- URL: https://rust-lang.github.io/unsafe-code-guidelines/
- License: MIT or Apache-2.0. Maintainer: the former UCG working group.
- Why: skip as a rule source. Its own intro says it is "largely abandoned" and not
  normative; only the glossary is maintained. Use the Reference's "behavior
  considered undefined" page and Miri instead:
  https://doc.rust-lang.org/reference/behavior-considered-undefined.html (URL only).

**Standard library safety-comment policy** (read)
- URL: https://std-dev-guide.rust-lang.org/policy/safety-comments.html
- License: MIT or Apache-2.0. Maintainer: Rust library team.
- Why: the exact form for `// SAFETY:` and `# Safety`. A `SAFETY:` comment may rely
  only on checks before the block, type invariants, and well-formed inputs.

**ANSSI Secure Rust Guidelines** (read: rule list and key chapters)
- URL: https://anssi-fr.github.io/rust-guide/
- License: Licence Ouverte / Open Licence 2.0. Maintainer: ANSSI (French national
  cybersecurity agency).
- Why: a security agency's rules with IDs. Useful items: never override
  `overflow-checks` or `debug-assertions` in dev and test profiles (DENV-CARGO-OPTS),
  explicit arithmetic modes (LANG-ARITH), `get` over indexing on untrusted input
  (LANG-ARRINDEXING), justify every `Send`/`Sync`/`Drop` impl, no panic in `Drop`.

### Performance

**The Rust Performance Book** (read: build configuration, bounds checks, contents)
- URL: https://nnethercote.github.io/perf-book/
- License: MIT or Apache-2.0. Maintainer: Nicholas Nethercote and others.
- Why: the build profile settings (`codegen-units = 1`, `lto = "fat"`,
  `panic = "abort"`), allocator choice, bounds-check removal by slicing before a loop
  or asserting lengths, and type sizes. Short and current.

### Lints

**Clippy lint list and configuration** (read: lint index for groups, configuration
reference for keys; all names tested locally)
- URLs: https://rust-lang.github.io/rust-clippy/master/index.html and
  https://github.com/rust-lang/rust-clippy/blob/master/book/src/lint_configuration.md
- License: MIT or Apache-2.0. Maintainer: Clippy team.
- Why: the tool that enforces most rules below. The stable index lists 847 lints:
  135 restriction, 52 nursery, 145 pedantic.

### Testing and tools (URL and license verified; content from tool docs not
re-read in this pass unless marked)

| Tool | URL | License | Maintainer | Why |
| --- | --- | --- | --- | --- |
| cargo-nextest | https://nexte.st/ | MIT or Apache-2.0 | nextest-rs (Rain) | Process per test, retries off, JUnit, partitions for CI. No doctests. |
| proptest | https://proptest-rs.github.io/proptest/ | MIT or Apache-2.0 | proptest-rs | Property tests with shrinking and saved failure files. Already chosen. |
| loom | https://github.com/tokio-rs/loom | MIT | Tokio project | Exhaustive model check of atomics and wake protocols under C11 memory model. |
| shuttle | https://github.com/awslabs/shuttle | Apache-2.0 | AWS Labs | Randomized and PCT schedules; scales to larger tests than loom. |
| cargo-mutants | https://mutants.rs/ | MIT | Martin Pool | Mutation testing; `--in-diff` for PRs. Already chosen. |
| cargo-deny | https://embarkstudios.github.io/cargo-deny/ | MIT or Apache-2.0 | Embark Studios | Licenses, advisories (RustSec), banned and duplicate crates, sources. |
| Miri | https://github.com/rust-lang/miri | MIT or Apache-2.0 | Rust project | Detects undefined behavior in `unsafe`; nightly only. |
| cargo-fuzz | https://rust-fuzz.github.io/book/cargo-fuzz.html | MIT or Apache-2.0 | rust-fuzz | libFuzzer driver; nightly only. Already chosen. |
| cargo-hack | https://github.com/taiki-e/cargo-hack | MIT or Apache-2.0 (GitHub API: Apache-2.0) | Taiki Endo | Checks each feature alone, so features stay additive (`sim`). |
| cargo-vet | https://mozilla.github.io/cargo-vet/ | Apache-2.0 (GitHub API) | Mozilla | Records who audited each dependency. Optional with `docs/dependencies.md`. |
| expect-test | https://docs.rs/expect-test/latest/expect_test/ | Apache-2.0 per GitHub API (dual likely) | rust-analyzer | Inline snapshots that update in place. |
| insta | https://docs.rs/insta/latest/insta/ | URL only, license unverified | Armin Ronacher | File snapshots; heavier than expect-test. |
| cov-mark | https://docs.rs/cov-mark/latest/cov_mark/ | MIT or Apache-2.0 | matklad | Coverage marks: a test proves a branch ran. |
| iai-callgrind | https://github.com/iai-callgrind/iai-callgrind | Apache-2.0 (GitHub API) | iai-callgrind | Instruction-count benchmarks; stable numbers on shared CI. Linux only. |
| divan | https://docs.rs/divan/latest/divan/ | Apache-2.0 (GitHub API) | Nikolai Vazquez | Wall-time micro-benchmarks with allocation counting. |
| criterion | https://github.com/bheisler/criterion.rs | Apache-2.0 (GitHub API) | criterion-rs | Wall-time benchmarks with statistics and saved baselines. |
| Kani | https://github.com/model-checking/kani | Apache-2.0 (GitHub API) | AWS | Bounded proofs for small pure functions (codec headers, varints). Optional. |
| bolero | https://docs.rs/bolero/latest/bolero/ | MIT (GitHub API) | Cameron Bytheway | One harness for fuzz, property, and Kani. Optional. |

### Deterministic simulation (URL only; rules from these are marked unverified)

- TigerBeetle VOPR: https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/internals/vopr.md
  (Apache-2.0) and "Simulation testing for liveness":
  https://tigerbeetle.com/blog/2023-07-06-simulation-testing-for-liveness/
- FoundationDB testing: https://apple.github.io/foundationdb/testing.html (Apple).
- sled simulation notes: https://sled.rs/simulation.html.
- S2 deterministic simulation for async Rust: https://s2.dev/blog/dst.
- turmoil (MIT, Tokio project): https://github.com/tokio-rs/turmoil. madsim:
  https://github.com/madsim-rs/madsim (license not checked).
- Antithesis docs: https://antithesis.com/docs/ (commercial service).
- Why: Foundation's `sim` crate is its own. These show the failure modes: hidden
  OS randomness, hash order, wall time, thread scheduling, and missing liveness
  checks. Do not adopt turmoil or madsim; `env` and `sim` already own the seams.

### Considered and skipped

- **Google Comprehensive Rust** (https://google.github.io/comprehensive-rust/,
  Apache-2.0 and CC-BY-4.0): a training course, not a rule set.
- **Rust for Rustaceans** (https://rust-for-rustaceans.com/, Jon Gjengset, No
  Starch Press, not openly licensed): good reading on API design, unsafe, and testing.
  Cite as reading; no rules taken. Not read in this pass.
- **Mozilla Rust style page**: the expected Firefox source-docs URL returned 404. Not
  found in this pass. Mozilla's useful output is cargo-vet.
- **cargo-semver-checks** (https://github.com/obi1kenobi/cargo-semver-checks): all
  crates are `publish = false`. Interface changes go through `docs/coordination.md`.
- **cargo-udeps** (https://github.com/est31/cargo-udeps): nightly only. The repo has
  almost no dependencies; `cargo-deny` and review cover it.

## Rules to adopt

Format: rule. Source. Conflict, if any, and the recommendation. "Tool" means a lint
or CI step can check it; "Review" means an agent or reviewer checks it.

### Style and API

1. Every public type implements `Debug`, and its output is never empty. Tool:
   `missing_debug_implementations`. Source: API C-DEBUG, MS M-PUBLIC-DEBUG. No
   conflict. Today it flags 5 types (`Pool`, `Unique`, `Block`, `Producer`,
   `Consumer`).
2. Each public item has one path. No `pub use` re-export of an item that is also
   public at its home. Review, and `unreachable_pub` (tool) for leaks. Source: MS
   M-SINGLE-ITEM-PATH. No conflict; it strengthens "the namespace carries the
   context". Today `unreachable_pub` flags 7 items.
3. An item name does not repeat its module name; the module's core item may equal it.
   Tool: `clippy::module_name_repetitions` (restriction since it left pedantic). Tested:
   it flags `frame::FrameKey` and passes `frame::Frame` with the default
   `allow-exact-repetitions = true`. Source: repo rule, Clippy. No conflict.
4. Struct fields are private unless the struct is plain data with no invariant. Never
   mix public and private fields. Tool: `partial_pub_fields`. Source: API
   C-STRUCT-PRIVATE. No conflict.
5. A newtype with an invariant has a fallible constructor and no infallible `From`.
   Tool: `fallible_impl_from` (nursery). Source: MS M-STRONG-TYPES-GUARD. No conflict.
6. A function that takes a `bool` or `Option` that callers pass as a literal becomes
   two functions, or takes an enum. Tool: `fn_params_excessive_bools` (pedantic, on).
   Source: rust-analyzer, API C-CUSTOM-TYPE. No conflict.
7. Preconditions live in types, not in checks inside the callee. Push `if`s to the
   caller and loops to the callee. Review. Source: rust-analyzer, matklad. No conflict.
8. Prefer `&[T]`, `&str`, `Option<&T>` over `&Vec<T>`, `&String`, `&Option<T>`. Tool:
   `ptr_arg` (style, on), `ref_option` (pedantic, on). Source: rust-analyzer.
9. Functions have at most 70 lines. Tool: `too_many_lines` with
   `too-many-lines-threshold = 70` (default 100). Source: TIGER_STYLE. No conflict.
10. Raw integers that carry a unit name it last: `latency_ns_max`, not
    `max_latency_ns`. Prefer typed units (`time::Span`, `Rate`) over raw integers.
    Review. Source: TIGER_STYLE. No conflict.
11. Wire, disk, and shared-memory fields use fixed-width integers (`u32`, `u64`).
    `usize` is only for in-memory lengths and indexes. Review. Source: TIGER_STYLE
    (adapted; Rust indexing needs `usize`). Conflict: none in the repo; TIGER_STYLE
    says "avoid usize" everywhere, which does not fit Rust. Adopt the adapted form.
12. Byte order is explicit. Never `to_ne_bytes`/`from_ne_bytes`. Tool:
    `host_endian_bytes`. Source: Clippy; supports performance rule 9 (memory shared
    with SDKs). No conflict.
13. Pass options to library calls at the call site; do not rely on library defaults
    for behavior that matters (atomics `Ordering`, socket options, fsync modes).
    Review. Source: TIGER_STYLE. No conflict.
14. Comments are full sentences. Tool: `doc_paragraphs_missing_punctuation` (doc
    comments only). Source: TIGER_STYLE, rust-analyzer. No conflict; 0 findings today.
15. Lint exceptions use `#[expect(lint, reason = "...")]`, never `#[allow]`. Tool:
    `allow_attributes`, `allow_attributes_without_reason`. Source: MS
    M-LINT-OVERRIDE-EXPECT. Conflict: `rust.md` says "allow a pedantic lint only at
    the item, with a reason" and "allows it at the module with
    `#[allow(unsafe_code)]`". Recommend: change both to `#[expect]`. Today 2 sites.
16. Features are additive. Tool: `cargo hack check --each-feature`. Source: MS
    M-FEATURES-ADDITIVE. No conflict; it guards the `sim` feature.

### Errors

17. Keep one public `Error` enum per crate (or per sub-boundary module), with data in
    variants. Source: repo `rust.md`. Conflict: MS M-ERRORS-CANONICAL-STRUCTS wants a
    struct with a private kind, `is_xxx()` methods, and a captured `Backtrace`.
    Recommend: keep the repo rule. Enums let tests pin the variant, and backtrace
    capture costs time and memory on paths that may be hot. Do not enable
    `clippy::error_impl_error`: it flags every `pub enum Error` (4 hits today).
18. Convert errors with `From` impls and `?`, not repeated `map_err`. Never drop the
    cause. Tool: `map_err_ignore`. Source: MS M-FROM-ERROR. No conflict.
19. Never discard a `Result` silently: no `.ok();`, no `let _ = fallible();`. Tool:
    `unused_result_ok`, `let_underscore_must_use`, `unused_must_use = "deny"`
    (present). Source: TIGER_STYLE "all errors must be handled", ANSSI. No conflict.
20. A detected programming bug panics; bad outside input returns an error. Panic
    messages state what broke and the values: `expect("invariant: frame len {n} >
    block len {cap}")` style. Tool: `missing_assert_message` for `assert!`. Source:
    MS M-PANIC-ON-BUG, M-PANIC-MESSAGE. No conflict; matches `rust.md`.
21. Tests never assert only `is_err()` or `is_ok()`. Tool:
    `assertions_on_result_states` (tested: flags `assert!(r.is_err())`). Source: repo
    testing rule "pin the exact error". No conflict; this makes the rule checkable.
22. No `#[should_panic]` without `expected = "..."`. Tool: `should_panic_without_expect`
    (pedantic, on). Source: rust-analyzer (it bans `should_panic`). Recommend the
    pedantic form, since some invariant panics need a test.
23. `Drop` never panics and never blocks without an alternative. Review. Source:
    ANSSI LANG-DROP-NO-PANIC, API C-DTOR-FAIL, C-DTOR-BLOCK. No conflict; matters for
    `Block` drop on foreign threads.

### Unsafe

24. `unsafe` lives only in crates the crate map names (`block` today; FFI connectors
    later). Each such module uses `#[expect(unsafe_code, reason = "...")]`. Tool:
    `unsafe_code = "deny"` (present). Source: MS M-UNSAFE. Conflict: `rust.md` says
    `#[allow(unsafe_code)]`; use `#[expect]` (rule 15).
25. Every `unsafe` block has one unsafe operation and a `// SAFETY:` comment that
    relies only on prior checks and type invariants. Tool:
    `undocumented_unsafe_blocks` (present), `multiple_unsafe_ops_per_block`,
    `unnecessary_safety_comment`. Source: std-dev-guide safety-comment policy. No
    conflict.
26. Every operation in an `unsafe fn` is in its own `unsafe` block. Every `unsafe fn`
    and `unsafe trait` has a `# Safety` section. Tool: `unsafe_op_in_unsafe_fn =
    "deny"`, `missing_safety_doc` (style, on), `unnecessary_safety_doc`. Source:
    std-dev-guide. No conflict.
27. `unsafe` marks only undefined-behavior risk, never "dangerous" logic. Review.
    Source: MS M-UNSAFE-IMPLIES-UB.
28. Every `unsafe impl Send` or `Sync` has a `// SAFETY:` comment that names the
    owner thread and what crosses threads. Tool: `non_send_fields_in_send_ty`
    (nursery) plus rule 25. Source: ANSSI LANG-SYNC-TRAITS. No conflict.
29. Safe code is sound: no safe function can cause undefined behavior for any input,
    including a misbehaving `Hash`, `Ord`, `Drop`, or a panic mid-operation. Review,
    Miri, and loom. Source: MS M-UNSOUND, Rustonomicon (exception safety).
30. Prefer audited safe abstractions for byte casts (`zerocopy`-style derives) over
    hand `transmute`. Tool: `transmute_undefined_repr` (nursery). Source: Clippy;
    zerocopy (https://github.com/google/zerocopy, URL only). Adding `zerocopy` needs
    the dependency process. Unverified fit; decide in `block` and `types` design.
31. `unsafe` for speed needs a benchmark that shows the safe form is slower. Review.
    Source: MS M-UNSAFE, perf book (bounds checks: slice first, assert lengths, then
    `get_unchecked` last). No conflict; matches performance rule 10's spirit.

### Performance

32. Release builds keep integer overflow checks. Set `overflow-checks = true` in
    `[profile.release]`. Source: decisions R9-D10 ("panic on internal overflow"),
    ANSSI LANG-ARITH. Conflict: none written, but without this setting, release
    builds wrap silently and R9-D10 holds only in debug builds. Recommend: turn it on
    and measure the cost with the 5% gate; use `wrapping_*` where wrap is intended.
33. Never override `debug-assertions` or `overflow-checks` in `[profile.dev]` or
    `[profile.test]`. Source: ANSSI DENV-CARGO-OPTS. No conflict.
34. Release profile: `codegen-units = 1`, `lto = "fat"`, `debug =
    "line-tables-only"` (profiles stay readable). Source: perf book. No conflict.
    Measure build time; `bench` inherits `release`.
35. `panic = "abort"` in release is a person's decision. Pro: perf book (smaller,
    slightly faster), MS M-PANIC-IS-STOP, and the repo's "fail loudly". Con: a panic
    in a connector's vendor thread then kills the node. Tests and benchmarks ignore
    this setting. Recommend abort, with connector isolation by process if needed later.
36. Assert lengths once before a hot loop, or slice first, so the compiler drops the
    bounds checks. Tool: `missing_asserts_for_indexing` (0 hits today). Source: perf
    book bounds-checks chapter. No conflict.
37. Clone an `Arc` as `Arc::clone(&x)` so each refcount bump is visible. Tool:
    `clone_on_ref_ptr`. Source: MS M-STATIC-VERIFICATION; supports performance rules
    2 and 3. No conflict.
38. No `Mutex` around an integer or bool; use an atomic, padded to a cache line when
    hot. Tool: `mutex_integer`, `mutex_atomic`. Source: Clippy; performance rules 5
    and 7. No conflict.
39. Every loop and queue has a bound; a loop that never ends returns `!`. Tool:
    `infinite_loop` (tested: flags `loop {}` in a `fn() -> ()`). Source: TIGER_STYLE
    "put a limit on everything". No conflict; matches performance rule 8.
40. Large stack frames fail the lint at a threshold that suits a Pi 4. Tool:
    `large_stack_frames` (nursery), `stack-size-threshold = 65536`. Source: Clippy.
    Unverified threshold; tune after the first shard loop exists.
41. Do a back-of-envelope sketch (network, disk, memory, CPU; bandwidth and
    latency) before writing a hot path. Put it in the PR beside the six questions.
    Review. Source: TIGER_STYLE. No conflict; it extends the six questions.
42. Hot loops go in small functions that take primitives or slices, not `&self`.
    Review. Source: TIGER_STYLE ("extract hot loops"). No conflict.

### Testing

43. Nothing in a simulated run reads OS randomness, OS time, or hash-order
    randomness. `std::collections::HashMap` and `HashSet` with `RandomState` are
    banned; use one deterministic alias in `types`. Tool: `disallowed-types` and
    `disallowed-methods` (tested: flags `HashMap::new()`, `HashMap<K, V>` in a
    signature, and `collect::<HashSet<_>>()`; passes a
    `type Map<K, V> = HashMap<K, V, BuildHasherDefault<DefaultHasher>>` alias whose
    definition carries one `#[expect]`). Source: repo injection rule; simulation
    sources (unverified). Conflict: none. Trade: `DefaultHasher::new()` uses fixed
    keys, so maps keyed by outside input lose HashDoS resistance. Recommend a keyed
    hasher whose key comes from `env` randomness for those maps.
44. Iteration over a hash map never decides behavior. Sort, or use an ordered map.
    Tool: `iter_over_hash_type` (tested). Source: Clippy; simulation replay. No
    conflict.
45. Logs and output never print pointers, since addresses change run to run. Tool:
    `pointer_format`. Source: Clippy; replay. No conflict.
46. No mutable globals, including `thread_local!`. Tool: `disallowed-macros =
    std::thread_local` (tested). A `static` with interior mutability
    (`Mutex`, `OnceLock`, `LazyLock`, atomics, `Cell`) has no Clippy lint; add an
    `xtask globals` grep check. Source: repo rule, MS M-AVOID-STATICS. No conflict.
47. Production code never checks `cfg(test)` to change behavior. Tool: `cfg_not_test`.
    Source: repo "test through the production path". No conflict.
48. Unit tests live in `#[cfg(test)] mod tests`; test names state behavior, with no
    `test_` prefix. Tool: `tests_outside_test_module`, `redundant_test_prefix`
    (both tested). Source: repo testing rules. No conflict.
49. Tests that use only a crate's public API and cross crates go in one integration
    binary per crate: `tests/it/main.rs` with modules, never many `tests/*.rs` files.
    Source: matklad (one binary links faster); MS M-INTEGRATION-TESTS. Conflict: the
    repo says tests are co-located. Recommend: keep co-location for unit tests; allow
    one `tests/it` binary for production-path tests. Update `testing.md`.
50. Write a `check` helper per feature under test: inputs as data, expected output as
    data. A signature change then edits one helper. Review. Source: matklad. No
    conflict.
51. Use inline snapshot (expect) tests for text outputs: `plan`, diagnostics,
    formatted HCL, `Display` of errors. Source: matklad, expect-test. Needs a
    dependency approval in `docs/dependencies.md`. No conflict.
52. Use coverage marks to prove that a test reached a branch (gap recording, credit
    exhaustion, fence). Source: rust-analyzer, cov-mark. Needs a dependency approval
    or a 20-line local macro in a test-only module. No conflict.
53. No `#[ignore]`. A known bug is a test that asserts today's wrong result, with a
    comment and an issue link. Source: rust-analyzer. No conflict; it keeps oracles
    honest.
54. Test both spaces: valid input, invalid input, and data that becomes invalid
    (truncated frames, bad offsets, stale fences). Source: TIGER_STYLE. No conflict.
55. Pair assertions: check data before it goes to disk or the wire, and again after
    it comes back. Review. Source: TIGER_STYLE. No conflict.
56. Tests must not repeat the implementation's formula or assert constants equal
    themselves. Assert properties (monotonic, round trip, bounds). Review, plus
    cargo-mutants. Source: MS M-TAUTOLOGICAL-TESTS. No conflict.
57. Test-only constructors and inspection hooks sit behind one feature. Source: MS
    M-TEST-UTIL (`test-util`). Conflict: `sim` already "ships behind a feature" with
    no name fixed. Recommend one name, `sim`, for simulated seams, and never a second
    test feature per crate.
58. Proptest failure files (`proptest-regressions/`) are committed and are oracles:
    agents add, never delete. Source: proptest docs (unverified detail). No conflict;
    extend `oracles/README.md`.
59. Each simulated run prints its replay value on failure, and CI reruns the failing
    value once to prove the failure replays. Source: simulation sources (unverified).
    No conflict; `testing.md` layer 3 says this in short.
60. Simulation checks liveness as well as safety: after faults stop, the mesh must
    converge within a bound. Source: TigerBeetle liveness post (title verified;
    content unverified). No conflict; add to `oracles/invariants/`.
61. Wake protocols get two checks: loom for exhaustive small models, shuttle (PCT) for
    larger ones. Gate std types behind `cfg(loom)` in `ring` only. Source: loom and
    shuttle READMEs (unverified details). No conflict.

### Tooling and lints

62. Use `#[expect]` everywhere (rule 15). Remove each exception when the code changes.
63. Per-crate stricter lints go in `lib.rs` as `#![deny(...)]`, since a crate's
    `[lints]` table with `workspace = true` cannot add entries. Unverified Cargo
    behavior; confirm before relying on it. See "Crate-level lints" below.
64. Pin `rust-toolchain.toml` to stable for builds. Allow a pinned nightly only for
    Miri and cargo-fuzz jobs. Conflict: `rust.md` says "stable Rust". Recommend a
    second file, `xtask`-managed, for the nightly date. Source: Miri and cargo-fuzz
    need nightly.
65. Fix a repo inconsistency: `rust.md` and `testing.md` say "only the real adapters
    in `env` and `clock`" may call the OS; `decisions.md` says `os` is the only crate
    allowed. `decisions.md` wins. Recommend editing `rust.md` and `testing.md`.

## Exact config

### `[workspace.lints.rust]` additions

All tested on rustc 1.98.1. "Today" is the hit count on the current code.

```toml
unsafe_op_in_unsafe_fn = "deny"        # each unsafe op in its own block (std policy)
missing_debug_implementations = "warn" # every public type is Debug; today 5
unreachable_pub = "warn"               # one path per item; today 7
non_ascii_idents = "deny"              # no look-alike identifiers
trivial_casts = "warn"                 # casts that do nothing hide intent
trivial_numeric_casts = "warn"         # same, for numbers
let_underscore_drop = "warn"           # `let _ = guard;` drops at once; today 2 (stubs)
redundant_lifetimes = "warn"           # MS static verification set
unused_lifetimes = "warn"              # same
elided_lifetimes_in_paths = "warn"     # show borrows in types: `Formatter<'_>`
meta_variable_misuse = "warn"          # macro bugs
unused_macro_rules = "warn"            # dead macro arms
ffi_unwind_calls = "warn"              # unwinding across FFI is UB-prone (connectors)
ambiguous_negative_literals = "warn"   # `-1.pow(2)` traps
redundant_imports = "warn"             # one path per item
unnameable_types = "warn"              # public API leaks a private type
unexpected_cfgs = { level = "warn", check-cfg = ["cfg(loom)", "cfg(shuttle)", "cfg(fuzzing)"] }
```

Rejected after the test run:

- `unused_results` and `missing_copy_implementations`: noisy, low value.
- `unused_crate_dependencies`: false positives with dev-dependencies.
- `single_use_lifetimes` and `variant_size_differences`: 0 hits now. Optional;
  `variant_size_differences` helps frame enums but is noisy with error enums.
- `implicit_provenance_casts` and `unqualified_local_imports`: unknown on stable 1.98
  (feature-gated). Do not add.

### `[workspace.lints.clippy]` additions

```toml
# Lint exceptions
allow_attributes = "deny"                 # use #[expect]; today 2
allow_attributes_without_reason = "deny"  # every exception says why

# Unsafe
multiple_unsafe_ops_per_block = "deny"    # one op, one SAFETY comment
unnecessary_safety_comment = "warn"       # no SAFETY on safe code
unnecessary_safety_doc = "warn"           # no # Safety on safe fns
non_send_fields_in_send_ty = "warn"       # nursery; unsafe Send impls
transmute_undefined_repr = "warn"         # nursery; UB in transmute
as_ptr_cast_mut = "warn"                  # nursery; mutable alias from &
as_pointer_underscore = "deny"            # pointer casts name their type
fn_to_numeric_cast_any = "deny"           # no fn pointer to integer casts

# Errors and panics
missing_assert_message = "warn"           # asserts say what broke
assertions_on_result_states = "deny"      # pin the exact error
unused_result_ok = "deny"                 # no `.ok();`
let_underscore_must_use = "deny"          # no `let _ = fallible();`
map_err_ignore = "deny"                   # keep the cause
unwrap_in_result = "warn"                 # a Result fn returns, not panics
get_unwrap = "deny"                       # use [] or handle None
fallible_impl_from = "warn"               # nursery; From never panics
unimplemented = "deny"                    # say todo!() while stubbing
exit = "deny"                             # only node exits; add one #[expect]

# Determinism and globals
iter_over_hash_type = "deny"              # hash order breaks replay
pointer_format = "deny"                   # addresses break replay
cfg_not_test = "deny"                     # tests run production paths
host_endian_bytes = "deny"                # byte order is explicit

# Performance
clone_on_ref_ptr = "warn"                 # refcount bumps are visible
mutex_integer = "deny"                    # use an atomic
mutex_atomic = "deny"                     # same
rc_buffer = "warn"                        # Rc<Vec> double indirection
missing_asserts_for_indexing = "warn"     # assert lengths, drop bounds checks
infinite_loop = "deny"                    # endless loops return `!`
large_stack_frames = "warn"               # nursery; Pi 4 stacks

# Names and structure
module_name_repetitions = "warn"          # namespace carries the context
partial_pub_fields = "deny"               # all fields public, or none
tests_outside_test_module = "deny"        # tests co-located in mod tests
redundant_test_prefix = "warn"            # test names state behavior
doc_paragraphs_missing_punctuation = "warn" # comments are sentences
wildcard_dependencies = "deny"            # no `*` versions
```

Enable later (CI denies warnings, and these fire on stubs today):

```toml
todo = "warn"                    # 35 todo!() stubs today; enable per crate when done
let_underscore_untyped = "warn"  # 20 stub `let _ = x;` today
```

Use only in some crates (see next section): `indexing_slicing`,
`arithmetic_side_effects`, `as_conversions`, `string_slice`,
`wildcard_enum_match_arm`.

Rejected, with reason:

- `error_impl_error`: flags every `pub enum Error`; conflicts with the repo's error
  rule.
- `expect_used`, `panic`: the repo allows `expect("invariant: ...")` for internal
  invariants.
- `missing_inline_in_public_items`: 64 hits on 1.2k lines. Use LTO instead.
- `std_instead_of_core`, `min_ident_chars`, `shadow_unrelated`,
  `default_numeric_fallback`, `else_if_without_else`, `integer_division`: noisy for
  this code. `min_ident_chars` fires on `f` in every `fmt`.
- `redundant_pub_crate` (nursery): fights `unreachable_pub`.
- `future_not_send` (nursery): shard futures are `!Send` by design.
- `redundant_clone`, `significant_drop_tightening` (nursery): known false positives.
- `multiple_crate_versions` (cargo group): cargo-deny does this better.
- `renamed_function_params`: 3 hits, low value.

### Crate-level lints

Add at the top of `lib.rs`:

- Decoders of outside input (`codec`, `wire`, `document`, `config-hcl`, every
  protocol parser in `connector-<kind>`):
  `#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects,
  clippy::as_conversions, clippy::string_slice)]`. Reason: bad outside input must
  return an error, never panic or wrap (`rust.md`, R9-D10, ANSSI LANG-ARITH and
  LANG-ARRINDEXING). Noisy elsewhere: they flag every `+` and `[]`.
- Layer 1 decision crates (`raft`, `control`, `delivery`, `access`, `estimate`):
  `#![deny(clippy::wildcard_enum_match_arm)]`. Reason: a new state or message
  variant must break the build at every match. Noisy on foreign enums such as
  `io::ErrorKind`.

### `clippy.toml` additions

```toml
too-many-lines-threshold = 70           # TIGER_STYLE function limit
allow-indexing-slicing-in-tests = true  # decoder lints stay out of tests
stack-size-threshold = 65536            # large_stack_frames; tune on a Pi 4
check-private-items = true              # # Safety/# Panics/# Errors docs on private fns
                                        # too (block internals); 2 hits today, in xtask

disallowed-methods = [
  # existing entries stay
  { path = "std::time::Instant::elapsed", reason = "reads the OS clock" },
  { path = "std::time::SystemTime::elapsed", reason = "reads the OS clock" },
  { path = "std::thread::Builder::spawn", reason = "threads come from env" },
  { path = "std::thread::available_parallelism", reason = "core count comes from env" },
  { path = "std::env::vars", reason = "configuration is an input" },
  { path = "std::env::args", reason = "only node reads arguments" },
  { path = "std::fs::read", reason = "disk comes from env" },
  { path = "std::fs::File::open", reason = "disk comes from env" },
  { path = "std::fs::File::create", reason = "disk comes from env" },
  { path = "std::fs::OpenOptions::open", reason = "disk comes from env" },
  { path = "std::net::TcpStream::connect", reason = "network comes from env" },
  { path = "std::net::TcpListener::bind", reason = "network comes from env" },
  { path = "std::net::UdpSocket::bind", reason = "network comes from env" },
  { path = "std::process::exit", reason = "only node exits" },
  { path = "std::collections::HashMap::new", reason = "RandomState breaks replay" },
  { path = "std::collections::HashMap::with_capacity", reason = "RandomState breaks replay" },
  { path = "std::collections::HashSet::new", reason = "RandomState breaks replay" },
  { path = "std::collections::HashSet::with_capacity", reason = "RandomState breaks replay" },
  { path = "std::hash::RandomState::new", reason = "OS randomness breaks replay" },
]
disallowed-types = [
  { path = "std::collections::HashMap", reason = "use the deterministic map in types" },
  { path = "std::collections::HashSet", reason = "use the deterministic set in types" },
  { path = "std::hash::RandomState", reason = "OS randomness breaks replay" },
]
disallowed-macros = [
  { path = "std::thread_local", reason = "no mutable globals" },
]
```

All paths above resolved on Clippy 0.1.98 (an unknown path is a hard error). The
`std::fs` list is not complete: Clippy has no wildcard paths. `xtask` already calls
`std::env::args`; give it one `#[expect]`. Note: Clippy reads the nearest
`clippy.toml` and does not merge files, so per-crate files would copy the root file.
Keep one file.

## Dev tools and CI gates

Install: `cargo install --locked cargo-nextest cargo-deny cargo-mutants cargo-fuzz
cargo-hack`. Miri: `rustup +nightly-<date> component add miri`. loom and shuttle are
dev-dependencies of `ring` (and `block` if it gets atomics); proptest is a workspace
dev-dependency. Each needs an entry in `docs/dependencies.md`. Command flags below are
from memory and are unverified against current docs.

| Gate | When | Command | Fails on |
| --- | --- | --- | --- |
| Format, lint, layers | Every PR | `cargo fmt --check`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo xtask layers`; `cargo xtask globals` (new) | Any warning, layer break, mutable static |
| Features | Every PR | `cargo hack check --workspace --each-feature --no-dev-deps` | A feature that does not build alone |
| Unit and property | Every PR | `cargo nextest run --workspace --profile ci` (retries 0, fail-fast off); `cargo test --doc --workspace` (nextest skips doctests) | Any failure; a retry pass counts as a fail |
| Dependencies | Every PR and nightly | `cargo deny check advisories bans licenses sources` | RustSec advisory, license outside the allow list, wildcard or git source, banned crate |
| Mutation | Every PR | `git diff origin/main... > diff.patch; cargo mutants --in-diff diff.patch --test-tool nextest` | Any missed mutant in changed code |
| Miri | PRs that touch `unsafe`; nightly for `block`, `ring` | `MIRIFLAGS="-Zmiri-strict-provenance" cargo +nightly-<date> miri nextest run -p block -p ring`; a second run with `-Zmiri-tree-borrows` | Any UB report |
| loom | PRs that touch `ring` or atomics | `RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=3 cargo test -p ring --release loom` | Any interleaving failure |
| shuttle | Same, and nightly at high iteration counts | `cargo test -p ring --features shuttle shuttle` (PCT schedules) | Any schedule failure; print the schedule to replay |
| Fuzz | Short per merge; continuous nightly | `cargo +nightly-<date> fuzz run <target> -- -max_total_time=60` per target; corpus in `oracles/fuzz/` | Crash or timeout; the input joins the corpus |
| Simulation | Thousands per merge, millions nightly | `cargo xtask sim --runs N` (repo tool) | Invariant break; print the replay value |
| Benchmarks | Every merge on the dedicated machine | divan or criterion against `oracles/baselines/` | Over 5% regression |

Two benchmark notes. First, iai-callgrind counts instructions, so its numbers are
stable on shared CI runners; it can catch regressions before the dedicated machine
runs. It is Linux only. Second, choose one wall-time harness; divan also counts
allocations, which could serve the "no hot-path allocation" check. Both are
unverified fits; decide in the first benchmark PR.

`deny.toml` start point: `[licenses] allow = ["MIT", "Apache-2.0", "BSD-3-Clause",
"ISC", "Unicode-3.0", "Zlib"]`; `[bans] multiple-versions = "warn"`, `wildcards =
"deny"`; `[sources] unknown-registry = "deny"`, `unknown-git = "deny"`;
`[advisories]` default (deny vulnerabilities, warn unmaintained). Tie each `[bans]`
entry to `docs/dependencies.md`.

`.config/nextest.toml` start point: a `ci` profile with `retries = 0`,
`fail-fast = false`, `slow-timeout = { period = "30s", terminate-after = 4 }`, and
JUnit output for the triage agent.

## Changes to repo docs this implies

- `rust.md`: `#[expect]` instead of `#[allow]` (rules 15, 24); the `os` crate owns
  OS calls (rule 65); nightly only for Miri and fuzz (rule 64); overflow checks in
  release (rule 32).
- `testing.md`: one `tests/it` binary per crate (rule 49); no `#[ignore]` (rule 53);
  committed proptest failure files are oracles (rule 58); liveness invariants (rule 60).
- `performance.md`: add the back-of-envelope sketch to the six questions (rule 41).
- `oracles/README.md`: list `proptest-regressions/` beside `fuzz/`.
