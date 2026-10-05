# Rust rules

## Toolchain

- Stable Rust, pinned in `rust-toolchain.toml`. Edition 2024.
- One Cargo workspace. Crates live in `crates/<name>`, with `publish = false`. The
  package name is the bare module name (`types`, `hub`), so code reads `types::Frame`.
- Dev tools live in `xtask` and run as `cargo xtask <task>`.

## Commands

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask layers
cargo test --workspace
cargo bench -p <crate>
```

## Lints

Workspace lints are in the root `Cargo.toml`. Every crate sets `[lints] workspace =
true`. CI denies warnings.

- `unsafe_code` is denied. A module that needs `unsafe` (pools, codecs, FFI) allows it
  at the module with `#[allow(unsafe_code)]`. Each `unsafe` block has a `// SAFETY:`
  comment, and the module runs under Miri.
- `missing_docs` covers every public item. A public item is a contract.
- `clippy::pedantic` is on. Allow a pedantic lint only at the item, with a reason.
- `clippy.toml` forbids reading the clock, sleeping, spawning threads, and reading
  environment variables. Those come from `env`. Only the real adapters allow them.

## Errors

- Each crate has one public `Error` enum, or one per module when a module is a clear
  sub-boundary. Variants carry the data a caller needs to act or to show a fix.
- Every error a user can see has a stable code and a fix-it hint.
- Never ignore a `Result`. Never `unwrap()` outside tests. Use
  `expect("invariant: ...")` only for an internal invariant.
- An internal invariant that breaks panics. Bad outside input never panics: it returns
  an error.

## Async and threads

- Layer 1 has no async runtime, no threads, and no I/O. `ring` may expose futures
  for wakeups.
- Each shard owns its state and runs its own loop. Do not share state between shards
  through locks. Send handles through bounded rings.
- Blocking vendor libraries run on their own threads, owned by their connector, never
  on a shard.

## Naming

- The module carries the context: `channel::Key`, `control::Gate`, `delivery::Credit`.
- Identifiers are keys: `node::Key`, never `NodeId`.
- One concept per file. Tests are co-located in `#[cfg(test)] mod tests`.

## Docs

Doc comments speak to the caller. Use `# Errors`, `# Panics`, and `# Safety` sections
when they apply. State which thread may call a function and whether it blocks, when
that matters for correctness. Keep each doc comment short.
