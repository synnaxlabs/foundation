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
  https://github.com/synnaxlabs/foundation/pull/1864#issuecomment-6061840714). The PR
  that changes a copy of a Rust crate lists its mutants as `docs/dependencies.md`,
  "Local patches", states (`laptop.architect-2`, 2026-10-08T11:36:09Z,
  https://github.com/synnaxlabs/foundation/pull/1864#issuecomment-6058989337, and
  2026-10-08T12:05:01Z for hand mutants,
  https://github.com/synnaxlabs/foundation/pull/1864#issuecomment-6059458510). That rule
  does not cover a C copy. Trigger: the PR that first changes a file in a C copy states
  how a test outside the copy checks each changed line, for the approval of the
  architect of `connector-opcua` (#435; `laptop.architect-2`, 2026-10-08T11:24:06Z,
  https://github.com/synnaxlabs/foundation/pull/1864#issuecomment-6058789517).
  #2106 did so for `cmpDue`, the split key, and the rename of the time parameter of
  `UA_Timer_process` in `arch/common/timer.c` of open62541, at `652692759`: a hand
  mutant of each changed line fails a test or the build, except the release form of
  a `UA_assert` line, which builds to the same code with `UA_DEBUG` off (approved by
  `laptop.architect-2`, 2026-10-09T04:52:13Z,
  https://github.com/synnaxlabs/foundation/pull/2106#issuecomment-6074512538).
  It did so for the swapped results of `cmpBatchWindow` at `17a75f322` (approved by
  `laptop.architect-2`, 2026-10-09T05:27:21Z,
  https://github.com/synnaxlabs/foundation/pull/2106#issuecomment-6074900634), for
  its tests and its row in `docs/dependencies.md` at `7a0913317` (approved by
  `laptop.architect-2`, 2026-10-09T06:23:46Z,
  https://github.com/synnaxlabs/foundation/pull/2106#issuecomment-6075586406), and
  for its table at `7a0913317` (approved by `laptop.architect-2`,
  2026-10-09T06:28:29Z,
  https://github.com/synnaxlabs/foundation/pull/2106#issuecomment-6075645938).
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
