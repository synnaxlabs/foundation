# Rust rules

"(r16 N)" names rule N in `docs/research/r16-rust-guides.md`, which gives its source.

## Toolchain

- Stable Rust, pinned in `rust-toolchain.toml`. Edition 2024.
- Miri and cargo-fuzz need nightly. They run on one pinned nightly that only those
  gates use. Builds and tests stay on stable (r16 64).
- One Cargo workspace. Crates live in `crates/<name>`, with `publish = false`. The
  package name is the bare module name (`types`, `hub`), so code reads `types::Frame`.
- Dev tools live in `xtask` and run as `cargo xtask <task>`.
- Features are additive: each feature builds alone (r16 16). CI runs Clippy and the
  tests with `--all-features`, so code and tests behind a feature run on every PR;
  the ARM job tests the defaults.

## Commands

```sh
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo xtask layers
cargo xtask globals
cargo xtask oracles
cargo test --workspace --all-features
cargo bench -p <crate>
```

## Build profiles

- Release keeps integer overflow checks, so an internal overflow panics in every
  build. Use `wrapping_*` where a wrap is intended and `checked_*` on outside values
  (r16 32).
- Release sets `panic = "abort"`. A broken invariant stops the node, and crash
  recovery restarts it. Tests and benchmarks still unwind (r16 35).
- Release also sets `codegen-units = 1`, `lto = "fat"`, and
  `debug = "line-tables-only"` (r16 34).
- Never override `debug-assertions` or `overflow-checks` in the `dev` or `test`
  profile (r16 33).
- `dev` and `test` also keep only line tables, to save disk. For a debugger, set
  `debug = true` in your own build, never in the committed `Cargo.toml`.

## Lints

Workspace lints are in the root `Cargo.toml`. Every crate sets `[lints] workspace =
true`. CI denies warnings. r16 gives the reason for each lint.

- A lint exception is `#[expect(lint, reason = "...")]`, never `#[allow]`. Put it on
  the smallest item. Remove it when the compiler reports it unfulfilled (r16 15, 62).
- `missing_docs` covers every public item. A public item is a contract.
- `clippy::pedantic` is on.
- `clippy.toml` forbids calls to the OS (clock, sleep, threads, environment, arguments,
  files, sockets, `process::exit`), the std blocking waits (`park`, `Condvar`,
  `Barrier`, `mpsc` receive), std `HashMap`, `HashSet`, `RandomState`, `thread_local!`,
  and `env::wall::Wall::now` outside `clock`. Only `os` implements the `env` seams and
  calls the OS clock, files, sockets, randomness, and threads. Only `node` reads
  arguments and exits. Each such call carries one `#[expect]`.
- `clippy.toml` also forbids the rustls calls that read the OS clock, a key log file,
  or the process-wide provider. Only `transport::tls` builds rustls configs, with
  `builder_with_details`, its own provider, and a fixed time.
- `clippy.toml` also forbids the noq-proto constructors and types that draw OS
  randomness or read the OS clock. Only `transport::quic::settings` builds noq-proto
  configs and endpoints. It sets every random value from `Entropy`.
- A crate's `[lints]` table cannot add to the workspace set, so stricter lints go at
  the top of `lib.rs` as `#![deny(...)]` (r16 63):
  - Decoders of outside input (`codec`, `wire`, `document`, `config-hcl`,
    `spec::tree`, and each protocol parser in a `connector-<kind>`) deny
    `indexing_slicing`, `arithmetic_side_effects`, `as_conversions`, and
    `string_slice`. Bad input returns an error. It never panics or wraps.
  - Layer 1 decision crates (`raft`, `control`, `delivery`, `access`, `estimate`)
    deny `wildcard_enum_match_arm`, so a new variant breaks every match.
- `clippy::todo` and `let_underscore_untyped` turn on for a crate when its stubs are
  gone.

## Style and API

- Every public type implements `Debug`, and the output is never empty (r16 1).
- Each public item has one path. Do not `pub use` an item that is public at its home
  (r16 2).
- Fields are all private, or all public on plain data with no invariant (r16 4).
- A type with an invariant has a fallible constructor and no `From` that can panic
  (r16 5).
- A `bool` or `Option` argument that callers pass as a literal becomes two functions
  or an enum (r16 6).
- Put preconditions in types. Push `if`s up to the caller and loops down to the
  callee (r16 7).
- Take `&[T]`, `&str`, and `Option<&T>`, not `&Vec<T>`, `&String`, or `&Option<T>`
  (r16 8).
- A function has at most 70 lines (r16 9).
- A raw integer with a unit names the unit last: `latency_ns_max`. Prefer typed
  units such as `time::Span` (r16 10).
- Wire, disk, and shared-memory fields use fixed-width integers. `usize` is only for
  in-memory lengths and indexes (r16 11).
- Byte order is explicit. Never `to_ne_bytes` or `from_ne_bytes` (r16 12).
- Pass options that change behavior at the call site: atomic `Ordering`, socket
  options, fsync mode. Never rely on a library default (r16 13).
- Doc comments are full sentences (r16 14).

## Errors

- Each crate has one public `Error` enum, or one per module when a module is a clear
  sub-boundary. Variants carry the data a caller needs to act or to show a fix. Do
  not use error structs with a hidden kind (r16 17).
- Every error a user can see has a stable code and a fix-it hint.
- Convert errors with `From` and `?`. Never drop the cause (r16 18).
- Never ignore a `Result`: no `.ok();` and no `let _ = fallible();` (r16 19). Never
  `unwrap()` outside tests. Use `expect("invariant: ...")` only for an internal
  invariant.
- An internal invariant that breaks panics. Bad outside input never panics: it returns
  an error. Panic and assert messages state what broke and the values (r16 20).
- `Drop` never panics. It never blocks unless the type also gives a call that does
  not block (r16 23).

## Unsafe

- `unsafe_code` is denied. Only the crates the crate map names may hold `unsafe`:
  `block`, `ring`, `counting`, and later FFI connectors. Such a module uses
  `#[expect(unsafe_code, reason = "...")]` and runs under Miri (r16 24).
- Each `unsafe` block holds one unsafe operation and a `// SAFETY:` comment. The
  comment relies only on earlier checks, type invariants, and well-formed inputs
  (r16 25).
- Inside an `unsafe fn`, each unsafe operation has its own `unsafe` block. Each
  `unsafe fn` and `unsafe trait` has a `# Safety` section (r16 26).
- `unsafe` marks a risk of undefined behavior, never logic that is only dangerous
  (r16 27).
- Each `unsafe impl Send` or `Sync` has a `// SAFETY:` comment that names the owner
  thread and what crosses threads (r16 28).
- Safe code is sound for every input, including a bad `Hash`, `Ord`, or `Drop`, and a
  panic mid-operation. Miri and loom check it (r16 29).
- Prefer an audited safe byte cast to `transmute`. A new crate for it goes through
  `docs/dependencies.md` (r16 30).
- `unsafe` for speed needs a benchmark that shows the safe form is slower. Slice
  first or assert lengths first; use `get_unchecked` last (r16 31).
- Authors and reviewers of `block` and `ring` read the Rustonomicon.

## Performance

`docs/claude/performance.md` is the rulebook. These rules add to it:

- Assert lengths once before a hot loop, or slice first, so the compiler removes the
  bounds checks (r16 36).
- Clone an `Arc` as `Arc::clone(&x)`, so each reference count change is visible
  (r16 37).
- Never put a `Mutex` around an integer or a bool. Use an atomic (r16 38).
- Every loop and queue has a bound. A loop that never ends returns `!` (r16 39).
- Keep stack frames small enough for a Pi 4 (r16 40).
- Put a hot loop in a small function that takes primitives or slices, not `&self`
  (r16 42).

## Determinism

- Use `types::hash::Map` and `Set`, never std `HashMap` or `HashSet`. Their order and
  hashes are the same in every run, so a simulated run replays. A map keyed by
  outside input needs a keyed hasher with its key from `env` randomness (r16 43).
- Hash iteration order never decides behavior. Sort, or use a `BTreeMap` (r16 44).
- Never print a pointer. Addresses change from run to run (r16 45).
- No globals: no `thread_local!` and no `static` item; a constant is a `const`
  (r16 46, #645). The one exception is `#[global_allocator] static ALLOCATOR:
  counting::Allocator` in a test or benchmark binary, never in a library or the
  `node` binary. Clippy refuses `global_allocator`, and only a crate-level
  `#![expect(clippy::disallowed_macros)]` lifts it. `cargo xtask globals` refuses
  that lift outside the root file of a test or benchmark target, and refuses each
  `static` that does not come right after `#[global_allocator]`, in every Rust file
  but `xtask`.

## Async and threads

- Layer 1 has no async runtime, no threads, and no I/O. `ring` may expose futures
  for wakeups.
- Each shard owns its state and runs its own loop. Do not share state between shards
  through locks. Send handles through bounded rings.
- Blocking vendor libraries run on their own threads, owned by their connector, never
  on a shard.

## Naming

- The module carries the context: `channel::Key`, `control::Gate`, `delivery::Credit`.
  `clippy::module_name_repetitions` checks it (r16 3).
- Identifiers are keys: `node::Key`, never `NodeId`.
- One concept per file. Unit tests are co-located in `#[cfg(test)] mod tests`.

## Docs

Doc comments speak to the caller. Use `# Errors`, `# Panics`, and `# Safety` sections
when they apply. State which thread may call a function and whether it blocks, when
that matters for correctness. Keep each doc comment short.

Write the doc comment of a public item before its body. When it cannot be short, the
abstraction is wrong: fix the design, not the comment.
