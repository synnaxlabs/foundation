---
name: eb-review
description:
  A builder's quality audit of its own work: a plan before it is built, or a diff
  before its PR opens. Asks whether this is good software, not whether it works. Runs
  nine fixed lenses: complexity cost, naming, anti-patterns, hacks, structural
  avoidance, performance, new patterns, test pinning, and robustness. Use from the
  build skill, or when asked to check whether an approach holds up.
---

# EB review

This review asks whether the work is good software. It does not hunt for bugs: tests,
CI, and `/review` own correctness, and a clean EB review never claims that the code
works. It carries the person's architectural judgment, so apply it with their
standards, not a lower bar.

## The two subjects

1. **A plan**, before any code: the issue's approach and the exact public surface it
   adds or changes.
2. **A diff**, after the local gates pass and before the PR opens.

The same nine lenses apply to both. Against a plan, the line count is an estimate,
the public surface change is stated exactly, and lens 8 asks what test will pin the
behavior and whether it can be written.

## The nine lenses

Run every one, in order.

1. **Complexity cost.** Count the lines added and removed and the public items added
   and removed. Then ask whether the growth is justified: is each module deep, does the
   common case stay simple, and did complexity move down into the module or up onto
   its callers? For each new public item,
   field, and parameter, ask why it deserves to exist and to be independent. When
   callers repeat the same steps, the surface asked them for the wrong thing: fix the
   surface, do not add a helper.
2. **Naming.** The namespace carries the context: `channel::Key`, not
   `channel::ChannelKey`. A compound name that repeats a responsibility
   (`home::ControlGate`) means a module wants to split; propose the split and the
   simpler names. Keys, never IDs. Booleans are adjectives.
3. **Anti-patterns and standards.** Read `docs/claude/design.md`, `docs/claude/rust.md`,
   `docs/claude/testing.md`, and `docs/claude/lessons.md` before this lens. Check the
   work against each rule it touches, and check each of the 14 red flags in
   `docs/claude/design.md` by name.
4. **Hacks.** Type erasure (`dyn Any`, downcasts, `Box<dyn>` where an enum fits),
   `as` casts that can truncate, `clone()` to quiet the borrow checker, needless
   `unsafe`, an `#[expect]` that hides a real problem, strings where a type belongs,
   needless control flow, and anything that works only by accident.
5. **Structural avoidance.** Is the work a workaround for a deeper problem? Name the
   problem and say whether to fix it instead. Fix the cause in one place; never add a
   second guard. When two pieces of logic look alike, ask why the shared logic must
   exist at all: often the case it defends against cannot happen, and both go away.
6. **Performance.** What did the work add to a path that a frame or sample crosses?
   Count allocations, copies, locks, syscalls, and wakeups per frame. On a hot path,
   answer the six questions in `docs/claude/performance.md` with measured numbers.
7. **New patterns.** Does the work add a pattern the codebase does not have: a trait
   with one implementation, a registry, a new kind of channel between shards, a new
   dependency? Justify it in full or drop it. When an existing mechanism covers the
   case, use it. The absence of a pattern is itself a decision.
8. **Test pinning.** Can a test pin the new behavior, and does one? Each test must fail
   when the behavior breaks. Errors are asserted by exact variant and message. Pure
   logic gets property tests; anything with I/O gets a simulation test.
9. **Robustness.** Is this the production-grade path? Name the alternatives you
   considered, at least one of them very different, and why each lost. Take the best
   architecture, not the cheapest change that passes, and do not use a tactical fix as
   a step toward it.

## What to do with a finding

Nobody waits for a person's decision here. The line is how far the change reaches.

- **One right answer, inside your crates.** Fix it now.
- **A shape choice inside your crates.** Choose the best architecture by the
  principles in `CLAUDE.md`, apply it, and record it under "Shape decisions" in the
  PR: the choice, the alternatives, and why they lost.
- **A change to another crate's public surface.** Follow "Interface changes" in
  `docs/coordination.md`.
- **A change to a locked decision, a contract, or an oracle.** Message
  the crate's architect (`docs/factory.md`), who takes it to the person.

A plan is cheap to change: revise it directly, shape included, and post the revised
plan on the issue.

## Effort ceiling

- Read the touched files in full and the rules for lens 3. Grep for callers, prior
  art, and the nearest existing analog.
- No subagents. Fresh reviewers belong to `/review`.
- Run focused tests for the touched crates. The full gates run in the build skill.
- After a rename or a signature change, grep every caller before you call it done.

## Output

No chat report. A plan's review goes into the issue as the revised plan. A diff's
review goes into the PR body:

- **Complexity**: lines added and removed, and public items added and removed, from
  the actual diff. Always present, even when nothing is wrong.
- **Shape decisions**: each choice from the second bullet above. Write "None" if there
  were none.

Fix every other finding before the PR opens. The person reads these two sections to
judge whether the work kept their standards.
