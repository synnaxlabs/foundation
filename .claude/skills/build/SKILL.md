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

1. `git fetch origin`. Work only in your own worktree. Remove a subagent worktree only
   by the exact path that its task notification gives, other than after a crash or a
   restart (`docs/coordination.md`, "Worktrees"). Never loop over `.claude/worktrees` or
   `git worktree list`: the worktrees of other sessions are there too.
2. Run the waits script. It prints each `blocked` issue with its owner and its state.
   For each with your name or `-`, `ended` is a wait that has ended (Rules). At
   `dropped`, an issue or PR that it waits on closed with no merge or as not done:
   write the line again from what the issue and the closed one state, and keep
   `blocked`. Give each of yours with `no line` its line (Rules). Its exits mean what
   they mean in step 4. Then an open issue labeled `owner:$FACTORY_NAME` that is not
   `blocked` comes first, one with no open PR before one with an open PR: resume it
   from its last state comment. Run "Wait once" in the background for the PR of each
   other one that has auto-merge on.

   ```sh
   w=$(git show origin/main:.claude/skills/build/waits.sh) || exit 3
   sh -c "$w" waits.sh
   ```
3. Give back each issue labeled `owner:$FACTORY_NAME` that you do not resume and that
   no open PR closes or has in its branch: comment its state, and remove your `owner:`
   label. Add `ready` to each that is not `blocked` and that the path table of the
   milestone's plan issue names (on #1737, "Path after the demo"). The state comment
   of each other one that is not `blocked` says that it waits for the path. You then
   hold at most one owned issue with no open PR.
4. With none to resume, take the first issue that the free script prints (on the night
   lane, `sh -c "$s" free.sh night`): the oldest `ready` issue whose crates no issue
   with an `owner:` label holds. An owned issue holds its crates while an open PR
   closes it or has its number in its branch, or while its owner builds it, which is
   while it is not `blocked`. Exit 3: git could not get the script. Another exit that
   is not 0: fix what gh printed, and run it again.

   ```sh
   s=$(git show origin/main:.claude/skills/build/free.sh) || exit 3
   sh -c "$s" free.sh
   ```
5. Claim it: `gh issue edit <n> --add-label "owner:$FACTORY_NAME" --remove-label ready`
   (the first time, `gh label create "owner:$FACTORY_NAME"`). If it then has a second
   `owner:` label, remove yours and take the next one.
6. Day lane: when fewer than two `ready` issues remain, file the next ones on the
   milestone path (Rules).
7. An issue labeled `model:fable`: ask your engineer to switch with `/model` first.

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
   public surface, a change that removes or changes a public item of a crate of your
   issue that a crate outside the issue uses, or a new crate dependency is an
   `interface` issue (`docs/coordination.md`, "Interface changes").
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
8. `/review <pr>`. It runs the reviewers by tier and the later rounds, at most 3
   (`/review`, "Round cap"). Fix every finding of a round in one batch of commits, with
   each merge of `main` that the PR needs, then ask for the next round. After round 3,
   fix only each defect, with a test that fails without the fix, add each input that
   `docs/claude/testing.md`, "Fuzzing", asks for, in a commit that changes nothing else,
   add the tests that kill the mutants that each run of the required `mutants` check
   reports on code of the PR, in one commit for each run that changes only tests, apply
   each `Text fixes:` item of round 3, link each approval, OK, and link that "Done" asks
   for, and defer the rest. A design flaw is deferred only with the OK of its design
   approver (`/review`, "Round cap").
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
    - Exit 0 (merged): comment the final state on the issue. While you build another
      issue, go on with it. Else call `mcp__factory__next`, and end your turn. On the
      night lane, take the next `night` issue in this context instead.
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
cargo xtask layers && cargo xtask globals && cargo xtask skips && cargo xtask oracles
cargo test -p <crates> --all-features
cargo hack check -p <crates> --each-feature --no-dev-deps
p=$(mktemp) && git diff origin/main...HEAD > "$p"
env -u CARGO_TARGET_DIR cargo mutants --in-diff "$p" --jobs 4 --jobserver-tasks 4
```

- Each missed mutant is a missing test. Exit 3 with an empty `mutants.out/missed.txt` is
  a pass: a timeout means a test caught the mutant.
- On a box, run `cargo mutants`, and each `cargo test` of a mutant made by hand,
  through `scripts/capped.sh` (`docs/claude/testing.md`, "Runs on a box"). That section
  also says when a hand mutant counts as killed.
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
  `#[ignore = "waits on #<n>"]`, and a method of its test bench that cannot is
  `todo!("waits on #<n>")`. The list names each open issue that states a step it needs,
  and no other. Never delete or weaken a scenario to make it pass.
- A new third-party dependency needs the person's approval and an entry in
  `docs/dependencies.md`.
- Ask the person in a comment on the issue or PR, then send `laptop.coordinator` the
  link to that comment at once (`docs/coordination.md`, "Messages"). A comment alone
  reaches no one. When no part of the issue is left to build, add `blocked` and take
  other work (Take an issue). A `blocked` issue names each wait in one line of its body,
  `Waits on #<n>, #<m>.`, for issues and PRs, with `the person` for a question
  (`Waits on #12 and the person.`). When the answer comes, take `the person` out of the
  line. Delete the line when nothing is left in it. A wait has ended when you delete
  the line, or when each issue that it names closed as completed, each PR merged, and
  it names no `the person`, as `ended` of the waits script says (step 2). Then remove
  `blocked` if it is there. If the issue has your `owner:` label and no open PR closes
  it or has its number in its branch, also give it back (step 3). With no `owner:`
  label, send its link to `laptop.coordinator`, which admits it (`/coordinate`,
  "Admit"; `docs/coordination.md`, "Messages", while it is paused). If you build no
  other issue, take one (Take an issue).
- A key choice (`CLAUDE.md`, "The person decides the design"): send it to
  `laptop.monitor` when you make it, before work builds on it, then go on. Never add
  `blocked` for it.
- A design choice that other crates need goes in the PR's Shape decisions; send the link
  to the crate's architect (`docs/factory.md`).
- When the architect rules in a comment on your issue, act on it at once. Add the ruling
  to the crate's records in `docs/decisions/` in your PR, with who decided and the
  comment link.
- Send the architect only a change to the public surface or to the meaning of a ruling.
  A fix of wording or links in a ruling needs no new approval.
- Each issue that you file (step 6, or for your issue: its later steps, a deferral of a
  finding of your PR, or an issue that your PR waits on) states its goal, crates, tests
  that must pass, and decisions section, with a `crate:` label for each of its crates.
  Send its link to `laptop.coordinator` when you file it, which admits it
  (`/coordinate`, "Admit"; `docs/coordination.md`, "Messages", while it is paused).
  Claim it (step 5) only when step 4 takes it. Your next state comment on your issue
  links each one that you file for it.
- Before you stop, comment your state on the issue: done, next step, open questions.
- A change to `free.sh` or `waits.sh` passes its test first:
  `sh .claude/skills/build/free_test.sh` or `sh .claude/skills/build/waits_test.sh`.
