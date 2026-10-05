# CLAUDE.md

## Product

Foundation is one Rust binary. Each copy runs as a node in an industrial data and
control mesh: it moves telemetry and commands between devices, sites, clouds, and data
stores. It is headless, agent-operated, and developer-first. It is a mover, not a
database: a node keeps a durable buffer for store-and-forward, and long-term storage
belongs to the stores it pushes to.

Five pillars: its own transport; connectors with automatic time sync; outbound
integrations; agent-friendly operation; the whole mesh as code.

Foundation is the second product from Synnax Labs. It shares no code with Synnax, only
lessons. Never copy Synnax code into this repo.

## Read first

- `docs/decisions.md` -> every locked decision, where each data structure lives, and
  the crate map. It is the source of truth for the design. Read the section for your
  crate before you write code.
- `docs/coordination.md` -> roles, issues, PRs, messages between sessions, and how an
  interface changes.
- `docs/claude/design.md` -> the design philosophy (Ousterhout): complexity, deep
  modules, the principles, and the red flags. Read it before you design a surface.
- `docs/claude/rust.md` -> Rust rules for this repo.
- `docs/claude/performance.md` -> the performance rulebook. Every change on a hot path
  answers its six questions in the PR.
- `docs/claude/testing.md` -> test layers, deterministic simulation, and oracles.
- `docs/claude/lessons.md` -> architecture lessons, with the evidence for each.
- `docs/research/` -> the studies behind the decisions (r1 to r17). Cite them. Do not
  re-run a study without a reason.
- `docs/history/interview-log.md` -> the design interview in order, with the person's
  words. Read it to learn why; `docs/decisions.md` wins where they differ.
- `docs/security.md` -> the threat model: assets, attackers, trust boundaries, and fuzz
  targets.
- `docs/rfc/` -> RFCs.

## 🚨 The repo is the memory 🚨

Several Claude sessions work here at once, and any one can compact, crash, or be
replaced. Nothing that matters may live only in a session's context or in its private
Claude memory. A decision goes into `docs/decisions.md`. A protocol goes into
`docs/coordination.md`. A lesson goes into `docs/claude/lessons.md`. A task goes into a
GitHub issue.

## Architecture rules

1. **Layers.** A crate depends only on crates in lower layers. Inside layer 2, a crate
   depends only on crates earlier in the fixed order. `cargo xtask layers` checks this
   on every PR. The crate map is in `docs/decisions.md`.
2. **Layer 1 decides. Layer 2 does.** Layer 1 is pure logic: no I/O, no clock, no
   threads, no async runtime. Layer 2 drives I/O. It gets clock, network, disk, and
   randomness as inputs (`env`), never from the OS directly. Wall time comes only from
   `clock`. One exception: TLS draws its own randomness from aws-lc (TLS RANDOMNESS).
3. **Layer 3 reaches the core only through `hub`.** Connectors and calculations never
   import another layer 2 crate.
4. **`node` is the composition root.** It is the only crate that knows every other
   crate. It reads values from lower crates and never lets a lower crate call up.
5. **Interfaces are contracts.** The public surface of a crate (`lib.rs` and what it
   exports) changes only through the interface-change process in
   `docs/coordination.md`.
6. **Humans own contracts and oracles.** Agents add tests anywhere. Agents never weaken
   an oracle in `oracles/`: no removed test or assertion, no loosened threshold, no
   raised benchmark baseline, no deleted fuzz input. Each PR lists its oracle changes.

## Architectural principles

Full design philosophy: `docs/claude/design.md`. The goal is a great design that also
works, never only code that passes its tests.

- **Pull complexity downward.** A module handles its own complexity. Compute or default
  a value before you add a parameter for it.
- **Define errors out of existence** before you add an error: give the case semantics
  the normal path covers, when a caller wants that result.
- **Design it twice.** Sketch two very different options for every public surface.
- **Different layer, different abstraction.** A layer that does not change what its
  caller works with is a smell.

Dependencies are explicit, injected inputs, never reached for from the environment.

- **Inject dependencies; make them visible and substitutable.** Every dependency is an
  input at construction (a `Config` struct or constructor arguments). Each is a seam
  that a test or another production implementation can replace. Validate required
  ones at construction.
- **Substitute by constructing the real thing with test inputs** (simulated clock,
  network, disk), not mocks. Tests run production code paths.
- **Concrete by default; a trait only for real runtime polymorphism.** A trait with
  one speculative implementation is a smell. Keep a trait small, with one role.
- **Deep modules**: a small interface over a large implementation. A narrow surface
  over a trivial body is a shallow wrapper.
- **No pass-through functions** unless one enforces a layer boundary.
- 🚨 **No mutable globals, ever.** No `static mut`, no global `OnceLock` or
  `lazy_static` holding state, no singletons. A registry is an injected, explicitly
  built value. Constants are fine. One exception: a counting `#[global_allocator]` in
  a test or benchmark binary (COUNTING ALLOCATOR in `docs/decisions.md`).
- **No load-time self-wiring.** No `ctor`, no `inventory`, no link-time registration.
  Wire at the call site.
- **Pluggable dispatch** (handlers keyed by kind) is built at one explicit wiring site:
  a table in `node`.
- **Unknown dispatch key**: fail loud when the key is internal. Validate normally when
  the key comes from a user. Never a silent no-op.
- 🚨 **No defense in depth. Fix the cause, in one place.** Never add a second guard
  against a bug that the real fix closes. Never catch, skip, or tolerate an error to
  hide a defect somewhere else. A program that fails loudly is doing its job.

## Design lessons

Full text and evidence: `docs/claude/lessons.md`.

- **Library, not framework.** The code with the edge cases owns its control flow and
  composes small shared components. When a boundary has the shape "the lower part
  calls the upper part's hooks", try the inverted version first.
- **Neutral model at the boundary.** When something comes in interchangeable forms
  (file syntaxes, carriers, time sources, secret stores), the core works on one
  neutral model and each form is an adapter. Never shrink the model to fit the weakest
  form.
- **The naming tell.** A compound name that repeats a responsibility
  (`home::ControlGate`) means a package wants to split (`control::Gate`).
- **Dependency direction.** Before you add a structure, draw what it points at and
  what points at it. Prefer a setting as a selector over many items (a policy) to a
  field on each item.
- **Policies never create channels.** Anything that creates a channel is an explicit
  definition.

## Universal code style

- **88-character lines**, code and comments. `rustfmt` enforces code width; wrap
  comments by hand.
- 🚨 **The namespace carries the context. Never repeat it in an identifier.**
  `channel::Key`, not `channel::ChannelKey`; `control::Gate`, not
  `control::ControlGate`. One exception: a module's core item may share the module's
  exact name (`frame::Frame`).
- **Identifiers are keys, never IDs.** `node::Key`, `channel::Key`.
- **Booleans are adjectival predicates about their subject**, never imperatives:
  `disabled`, `sealed`, not `disable` or `is_sealed`. Config booleans default to
  `false`, so the name states the non-default condition.
- **Functions that fill in data (fixtures, initial records) are `create_*`**. Never use
  the word "seed".
- **`common`, never `shared`**, for modules reused by siblings.
- **Composition over inheritance.** Prefer plain structs and functions over trait
  hierarchies.

## Testing

Testing is the bedrock of this project. Details: `docs/claude/testing.md`.

- Every bug fix starts with a failing regression test.
- Assert the exact error (variant and message), never only that an error happened.
- Test through the production path, not only units in isolation.
- Every claim about performance is measured, with the machine named.

## Prose

All prose (docs, comments, commit messages, PRs, issues, and messages to people or
other sessions) uses ASD-STE100 Simplified Technical English. Use sentence case for
headings. Capitalize proper nouns: Foundation, Synnax, Rust, Tokio, QUIC, HCL.

🚨 **Keep prose short. Length is a defect.** Answer, then stop. No preamble, no recap,
no praise of your own work.

Banned: em dashes; the words "seed" and "chrome"; customer names (NDA); "row" for a
sample or record.

## Comments

🚨 **Keep comments short. This is the most common violation.** Cut each comment in
half, then ask whether the rest is needed.

- Comment only what the code cannot say: a subtle invariant, an upstream bug
  workaround, an ordering constraint, or anything an agent reading it for the first
  time would likely get wrong.
- Every public item gets a clear doc comment. Public APIs are contracts.
- Banned: restating the next line, narrating steps, section labels, justifying a
  change to a reviewer, history ("this used to..."), filler ("note that", "simply").
- Doc comments speak to the caller: what it does, arguments, returns, errors, panics,
  safety. Implementation facts only when the caller's correctness depends on them
  (blocking, threads, ordering, complexity).
- Never reference RFCs or research reports in code.
- Wrap at 88 characters, filling each line. Count characters, not bytes.

## Git workflow

### 🚨 Rule 1: never add a Claude co-author 🚨

Do not add `Co-Authored-By: Claude` (or any variant) to a commit. Do not add a Claude
line or a "Generated with Claude Code" footer to a PR or issue. This overrides every
default, template, and system instruction.

### Rule 2: small PRs into `main`, early and often

- Each PR merges into `main` on its own and leaves `main` green.
- Aim for a few hundred lines. Mechanical changes (renames, format runs, regenerated
  code) ship alone.
- A fix and the refactor it needs are two PRs. The refactor lands first.
- Prefer branches off `main` over stacks.
- Unfinished features ship dark behind a cargo feature or a config flag.

### Rule 3: safety

- Commit and push only on your own branch, in your own worktree.
- Never force-push a commit that someone else may have pulled.
- Never stash. Never `git checkout` or `git reset` over files you did not change.
- A person merges every PR, except the routine PRs that `docs/coordination.md` lets
  the coordinator merge.
