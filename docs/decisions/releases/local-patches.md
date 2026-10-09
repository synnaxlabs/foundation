- **LOCAL PATCHES (2026-10-05)** A dependency that we patch lives in this repository as
  an unchanged copy of its release in `patches/<crate>/`, outside the workspace, with
  `[patch.crates-io]` in the root `Cargo.toml`. One PR adds the copy alone; a second
  PR makes our change on it, with its tests, so the change is reviewed here. For each
  new release that we take, the copy is replaced and our change made again. Lost: a
  fork in `synnaxlabs` patched by git URL (each build depends on a second repository,
  and the change is reviewed outside this one); for the first patch, a workaround in
  `transport` that never stops a stream (the peer sends the rest of the stream, and a
  cancel no longer reaches the sender, against STREAM WIRE). The person decided on
  2026-10-05 ("Ok I guess we need to do #2"), #620. A C library that we patch
  (open62541) is copied by one command: each release file that our build compiles or
  includes, unchanged, plus the files that its build generates. Its `build.rs` reads
  the copy, with no `[patch.crates-io]`. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6057554572,
  2026-10-08 10:08 UTC). `fuzz/` is a workspace of its own, so `fuzz/Cargo.toml` holds
  the `[patch.crates-io]` table of the root `Cargo.toml` (#1864). Lost: one
  `[patch.crates-io]` table in `.cargo/config.toml` for both workspaces. Cargo reads
  config from the working directory, so a run from outside the repository with
  `--manifest-path` builds the registry release with no error (`laptop.architect-2`,
  2026-10-08T14:16:19Z,
  https://github.com/synnaxlabs/foundation/pull/1864#issuecomment-6061840714).
  `cargo xtask fuzz` fails when `fuzz/` builds a copy that the root does not build, or
  when a package of the `fuzz` graph has an edge to another package of the name of a
  copy that the root builds, and a requirement on crates.io, of a kind and target of
  the edge, that both the copy and that package meet. A requirement that resolves to
  another package always gives such an edge, so the check has no false pass. Its cost
  is a false refusal of a requirement of the kind and target of one that resolves to
  another package, also when it resolves to the copy or to nothing, such as an
  optional dependency that is off. `fuzz/Cargo.lock` has one package of each copy's
  name, so no such case exists now (`laptop.architect`, 2026-10-09T06:40:13Z,
  https://github.com/synnaxlabs/foundation/pull/2099#issuecomment-6075796048).
  Supersedes the pair by edge name of
  https://github.com/synnaxlabs/foundation/pull/2099#issuecomment-6074529391 and
  https://github.com/synnaxlabs/foundation/pull/2099#issuecomment-6074614166, and the
  two limits of
  https://github.com/synnaxlabs/foundation/issues/1867#issuecomment-6074424462.
  The task checks the graphs of `cargo metadata --locked`, not the text of the two
  tables: a patch that `fuzz/` does not use changes no code that it tests
  (`laptop.architect-2`, 2026-10-09T04:27:21Z,
  https://github.com/synnaxlabs/foundation/issues/1867#issuecomment-6074258921). The PR
  that changes a copy of a Rust crate lists its mutants as `docs/dependencies.md`,
  "Local patches", states (`laptop.architect-2`, 2026-10-08T11:36:09Z,
  https://github.com/synnaxlabs/foundation/pull/1864#issuecomment-6058989337, and
  2026-10-08T12:05:01Z for hand mutants,
  https://github.com/synnaxlabs/foundation/pull/1864#issuecomment-6059458510). That rule
  does not cover a C copy. Trigger: the PR that first changes a file in a C copy states
  how a test outside the copy checks each changed line, for the approval of the
  architect of `connector-opcua` (#435; `laptop.architect-2`, 2026-10-08T11:24:06Z,
  https://github.com/synnaxlabs/foundation/pull/1864#issuecomment-6058789517).
  A change of a file that the copy command of a C copy generates (the thread-local
  block of the open62541 `config.h`) is made again after each run of that command, as
  a change of a release file is, and a test fails when it is lost (`laptop.architect-2`,
  2026-10-08 20:03 UTC,
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6068041912; this
  text approved by `laptop.architect` at 2026-10-08 21:42 UTC,
  https://github.com/synnaxlabs/foundation/pull/1995#issuecomment-6069607884, and
  `laptop.architect-2` at 2026-10-08 21:24 UTC,
  https://github.com/synnaxlabs/foundation/pull/1995#issuecomment-6069329793).
  Supersedes, for that block, "unchanged, plus the files that its build generates" of
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6057554572. The
  same holds for the branch of that `config.h` that sets `UA_FLOAT_LITTLE_ENDIAN` on
  little-endian 64-bit Arm, which Clang needs (`laptop.architect-2`, 2026-10-08 23:26
  UTC, https://github.com/synnaxlabs/foundation/pull/1995#issuecomment-6071050895).
