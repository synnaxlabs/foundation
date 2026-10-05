# Design philosophy

Foundation follows John Ousterhout's _A Philosophy of Software Design_ (2nd edition).
This file states his principles as rules for this repo, and settles the few places
where they meet our other rules. The study behind it is
`docs/research/r17-ousterhout.md`.

## Complexity is the enemy

Complexity is anything in the structure of the system that makes it hard to understand
or change. It shows up three ways:

- **Change amplification**: a simple change needs edits in many places.
- **Cognitive load**: a developer must know a lot to make a change.
- **Unknown unknowns**: it is not clear what code to change or what you must know. This
  is the worst one.

It has two causes: **dependencies** (code that cannot be understood or changed alone)
and **obscurity** (important information that is not obvious). Complexity grows in
small steps, so no step is too small to matter.

## Work strategically

Working code is not enough. The goal is a great design that also works. Tactical work
gets a feature done fast and leaves each shortcut for someone else. An agent that
patches until the tests pass is the tactical case at full speed.

- Every change leaves the design as if the change had been planned from the start.
- When you find a design problem, fix it. Do not patch around it.
- Spend real effort on design in every issue: the plan step and `/eb-review` exist for
  this.

## Principles

1. **Modules should be deep.** A lot of function behind a small interface.
2. **Make the common case simple.** The most common use needs the least knowledge from
   the caller.
3. **A simple interface beats a simple implementation.** The module's author suffers
   so that its users do not.
4. **Somewhat general-purpose.** Build only the functions needed now, but shape the
   interface so it does not encode today's one caller. General interfaces hide more.
5. **Separate general-purpose and special-purpose code.** Push special cases up to the
   caller or down into a clean mechanism; never mix them into the general one.
6. **Different layer, different abstraction.** Two adjacent layers with the same
   abstraction are a smell. A layer must change what the caller works with.
7. **Pull complexity downward.** Handle complexity inside the module. Each config
   parameter pushes a decision onto the user: compute the value or default it, and add
   a parameter only when the user can choose better than the code.
8. **Define errors out of existence.** First try to change the semantics so the normal
   path covers the case: removing an absent item succeeds, a range past the end clamps.
   Then mask expected conditions low (a lost packet is resent), aggregate the rest into
   one handler, and crash on broken invariants.
9. **Design it twice.** Before a public surface or a major structure, sketch at least
   two very different options, and record why the losers lost.
10. **Comments describe what the code cannot.** A comment at the code's own level
    repeats it. Good comments add precision (units, bounds, ownership) or intuition
    (why, and the larger shape).
11. **Design for reading, not writing.** "Obvious" is decided by the reader. If a
    reviewer says it is not obvious, it is not.
12. **Increments are abstractions, not features.** Design a whole abstraction at once;
    do not grow it one feature at a time.
13. **Decide what matters.** Make what matters obvious and hide what does not.
14. **Consistency.** Follow the existing convention. A better idea is not enough to
    break it; change it everywhere or not at all.
15. **Simple designs are fast.** Deep modules cross fewer layers. Measure before you
    optimize. For the critical path, write the minimum code for the common case, then
    find a clean structure close to it, with special cases off the path.

## Red flags

Each one is a finding in `/eb-review` and in review.

| Red flag | Sign |
| --- | --- |
| Shallow module | The interface is not much simpler than the implementation. |
| Information leakage | One design decision shows up in more than one crate or module. |
| Temporal decomposition | Code is split by execution order, not by what it hides. |
| Overexposure | Callers of the common case must know about rare features. |
| Pass-through function | A function only forwards to one with a similar signature. |
| Repetition | A nontrivial piece of code appears more than once. |
| Special-general mixture | Special-case code is inside a general mechanism. |
| Conjoined functions | You cannot understand one without reading the other. |
| Comment repeats code | The comment says what the code beside it already says. |
| Implementation in the interface | A doc comment tells callers details they do not need. |
| Vague name | The name is too broad to say what the thing is. |
| Hard to pick a name | No precise name fits: the design is not clean. |
| Hard to describe | A complete doc comment must be long: the abstraction is wrong. |
| Nonobvious code | A quick read does not show what the code does. |

## Where this meets our other rules

- **Tests come after design, before code.** Ousterhout opposes test-driven design,
  where tiny test cycles grow the design one feature at a time. We do not do that. The
  surface is designed first (plan, design it twice, `/eb-review`). Then the tests for
  that whole abstraction are written against it, before the body exists, so an agent
  cannot write tests that agree with its own bug. A bug fix starts with a failing test,
  which he also asks for.
- **Comments are short and complete.** Every public item has a doc comment
  (`missing_docs`), and it is short. Write it before the body: when it cannot be short,
  the abstraction is wrong (hard to describe). Private code gets a comment only for what
  the code cannot say.
- **Defining an error away is not hiding it.** New semantics that make a case normal
  are fine when a caller wants that result and the doc comment says so. Skipping or
  swallowing an error to hide a defect is still forbidden. Important errors are exposed.
- **Pass-through at a layer boundary.** We allow one that keeps the dependency
  direction intact, but the boundary layer should still change the abstraction when it
  can (`hub` turns many homes into one session).
- **Function length.** Length alone is not a reason to split. Split when each part can
  be read alone. Merge functions that are shallow or conjoined.
- **Context objects.** Ousterhout's context object matches our injected `Config` and
  `ctx`: passed at construction, small, and immutable. A context that grows into a grab
  bag is a mutable global in disguise.
