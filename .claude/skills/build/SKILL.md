---
name: build
description:
  The builder loop: take the next ready issue in any crate, build it, review it, and
  merge it through the queue, then clear the context and take the next one. Use when a
  builder, integrator, or connector session starts, with the argument `night` on the
  night lane.
---

# Build

You are `$FACTORY_NAME` (`echo $FACTORY_NAME`). You build in any crate: work goes to
whoever is idle, so all accounts spend their budget. You work one issue per context:
when its PR merges, `mcp__factory__next` clears it and starts `/build` again.

## Take an issue

1. `git fetch origin`. Work only in your own worktree.
2. An open issue labeled `owner:$FACTORY_NAME` comes first: resume it from its last
   state comment. Skip one labeled `blocked` while an issue that it waits on is open or
   a question on it has no answer. When neither holds, remove `blocked` and resume it.
   With none to resume, take the oldest `ready` issue whose crates no other open issue
   with an `owner:` label holds: `gh issue list --label ready --search
   "sort:created-asc"`. On the night lane, take only issues that also have `night`.
3. Claim it: `gh issue edit <n> --add-label "owner:$FACTORY_NAME" --remove-label ready`
   (the first time, `gh label create "owner:$FACTORY_NAME"`). If it then has a second
   `owner:` label, remove yours and take the next one.
4. Day lane: when fewer than two `ready` issues remain, file the next ones on the
   milestone path (goal, crates, tests that must pass, decisions section)
   and send the links to `laptop.coordinator`, which adds `ready`.
5. An issue labeled `model:fable`: ask your engineer to switch with `/model` first.

Read only this before you plan: the issue and its comments, the records of
`docs/decisions/` for your crate (the folder of its topic, and `grep -rl` for its name),
`docs/claude/rust.md`, and `docs/claude/testing.md`. `/eb-review` adds the design docs
it names; add `docs/claude/performance.md` for a hot path. Send wide searches to an
`Explore` subagent with `model: "haiku"`. Read diffs and logs through `--stat`, `tail`,
or a line range.

## Build it

1. Branch from `origin/main`, machine first:
   `git switch -c "${FACTORY_NAME%%.*}/<issue>-<slug>" origin/main`, for example
   `box1/462-hub-route`.
2. **Plan.** On the issue, write the approach, the exact public surface change with the
   doc comment of each new public item, and a very different design that lost. Run
   `/eb-review` on the plan and post the revised plan. A change to another crate's
   public surface or a new crate dependency is an `interface` issue
   (`docs/coordination.md`).
3. **Tests first.** Write the behavior from the issue and its decisions section as
   failing tests: property tests for codecs and pure logic, simulation tests for I/O.
4. **Implement** until they pass. Keep the PR to a few hundred lines. When a second idea
   appears, split. Handle each small change as `docs/coordination.md`, "Small
   changes", says: one in this PR's crate or in a file that this PR changes goes into
   this PR as its own commit. Before the first review round, fold in each item that the
   issue's comments add, one commit each, and list each item in the PR body.
5. **Local gate** (below). Fix every failure.
6. Run `/eb-review` on the diff. Put its Complexity and Shape decisions in the PR body.
7. `gh pr create --draft`, filling the template. Never add a Claude co-author or footer.
8. `/review <pr>`. It runs the reviewers by tier and the second round.
9. If review changed code, run the gate again. When review is done (`/review`,
   "Done"), run `gh pr ready <n>` and `gh pr merge <n> --auto`. The merge queue takes
   it when the checks pass.
10. **Wait once.** Run the wait script as it is on `main` (the copy on a branch can be
    older), with `run_in_background`:

    ```sh
    git fetch -q origin main &&
      s=$(git show origin/main:.claude/skills/build/wait.sh) || exit 3
    sh -c "$s" wait.sh <n>
    ```

    Never check by hand, `/loop`, or `ScheduleWakeup`. A message or the script's exit
    wakes you.
    - Exit 0 (merged): comment the final state on the issue, call
      `mcp__factory__next`, and end your turn. On the night lane, take the next
      `night` issue in this context instead.
    - Exit 1: read the cause it prints (`gh pr checks <n>`,
      `gh run view <id> --log-failed | tail -60`, or the review). Fix it, run the gate
      on what changed, `gh pr merge <n> --auto`, and wait again.
      "cannot be read" is three failed `gh` calls in a row: fix what gh printed (a
      login, a wrong PR number), or wait again once the network is back.
    - Exit 3: git could not get the script. Fix what git printed, and run it again.
    - Exit 2 or any other exit: read what it printed, fix the cause, and run it again.

## Local gate

Until `cargo xtask gate` exists, run CI's PR job set on the crates you changed plus the
crates that use a public item you changed (`-p <a> -p <b>`):

```sh
cargo fmt --check
cargo clippy -p <crates> --all-targets --all-features -- -D warnings
cargo xtask layers && cargo xtask globals && cargo xtask oracles
cargo test -p <crates> --all-features
cargo hack check -p <crates> --each-feature --no-dev-deps
p=$(mktemp) && git diff origin/main...HEAD > "$p"
cargo mutants --in-diff "$p" --jobs 4
```

- Each missed mutant is a missing test. Exit 3 with an empty `mutants.out/missed.txt` is
  a pass: a timeout means a test caught the mutant.
- On a box, run `cargo mutants` in the memory cgroup that `docs/claude/testing.md` gives
  (mutation testing).
- A changed `Cargo.toml` or `Cargo.lock`: also
  `cargo deny check advisories bans licenses sources`.
- A change under a `models` path in `.github/workflows/ci.yaml`: also `cargo xtask
  loom`, `cargo xtask shuttle`, and `cargo xtask miri`, except on the laptop.
- On the laptop: never `--workspace`, and run `cargo mutants` under the heavy lock
  (`docs/coordination.md`, "Heavy runs on the laptop").

## Night lane

Never change a public surface, a decision, or another crate. When the work needs a
person or a decision, ask on the issue, send the link, and add `blocked`, as Rules say.
A PR that needs the person (`oracles/`, `.github/`, `.claude/`) waits for the morning;
take the next issue meanwhile.

## Rules

- Never weaken an oracle. Add tests freely.
- Never sit idle on a wait. When your work needs a PR that is in the merge queue,
  build on its head and rebase when it merges. While a ruling is pending, build the
  parts it does not touch.
- In `acceptance`, and in a test of an acceptance scenario in `crates/node/tests/it/`
  (`docs/claude/testing.md`, "No `#[ignore]`"), a scenario that cannot run yet is
  `#[ignore = "waits on #<n>"]`, and a `Lab` method that cannot is
  `todo!("waits on #<n>")`. The list names each open issue that states a step it needs,
  and no other. Never delete or weaken a scenario to make it pass.
- A new third-party dependency needs the person's approval and an entry in
  `docs/dependencies.md`.
- Ask the person in a comment on the issue or PR, then send `laptop.coordinator` the
  link to that comment at once (`docs/coordination.md`, "Messages"). A comment alone
  reaches no one. When no part of the issue is left to build, add `blocked` and take
  other work (Take an issue). Remove `blocked` when the answer comes.
- A design choice that other crates need goes in the PR's Shape decisions; send the link
  to the crate's architect (`docs/factory.md`).
- When the architect rules in a comment on your issue, act on it at once. Add the ruling
  to the crate's records in `docs/decisions/` in your PR, with who decided and the
  comment link.
- Send the architect only a change to the public surface or to the meaning of a ruling.
  A fix of wording or links in a ruling needs no new approval.
- Before you stop, comment your state on the issue: done, next step, open questions.
