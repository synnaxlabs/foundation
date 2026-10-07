# Coordination

Several Claude sessions build Foundation at the same time. `docs/factory.md` lists the
sessions, the lanes, and the merge path. This file is how the sessions work together.
When this file and a message disagree, this file wins.

## Tokens

Most of the cost is context size per turn, so keep each context small:

- `.claude/settings.json` compacts a session near 200k tokens, keeps the prompt cache
  five minutes (99% of calls come sooner), and turns off plugins we never use.
- Every token in context is read again on every later call until compaction. Read the
  lines you need (`grep -n`, then `sed -n` or Read with a range), never a whole file
  or log; look at `--stat` or `--name-only` before a diff; and cut long output with
  `tail`.
- Wait for a PR with one background command (`/build`), never repeated checks. A
  Monitor must filter to events you act on.
- Never fork from a large context. Brief a fresh subagent instead.
- The person: a `/login` that switches organizations flushes every session's cache.
- Read only the sections of `docs/decisions.md` and `docs/research/` you need.
- Send reading, searching, and reviews to subagents; keep their results, not their
  file dumps.
- Finish each issue with its state comment, so compaction or `/clear` loses nothing.
- On usage credits, the prompt cache lives five minutes. A session that sleeps longer
  reads its whole context again at full price, so `/clear` before a long wait.

## Worktrees

The main checkout is `~/Desktop/synnaxlabs/foundation`. The launcher gives each session
one long-lived worktree, `~/Desktop/synnaxlabs/foundation-wt/<role>`
(`docs/factory.md`). A builder makes a branch per issue inside its worktree. Never work
in another session's worktree.

Never share `CARGO_TARGET_DIR` between worktrees. Cargo gives a path crate the same hash
in each, so a stale build of another worktree's code can pass or fail a gate.

Delete a scratch copy of the repo, with its `target`, when its job ends: a review copy,
a breaker worktree, or a cargo-mutants copy. Each takes 1 to 9 GiB. On 2026-10-05 stale
copies filled the disk, and Bash failed in every local session.

## Heavy runs on the laptop

The laptop sessions share 16 cores. A benchmark, a stress loop, or a local
`cargo mutants` run takes one lock, so only one runs at a time. Run it in the
background, because it waits for the lock:

```sh
lockf -k ~/.cache/foundation-heavy.lock <command>
```

A benchmark starts only when `uptime` shows a load under 8, and its PR names the load.
Builds and the PR gates do not take the lock. On 2026-10-05 the load reached 170, and a
stress run and two benchmarks gave results that no one could use.

The sessions also share 48 GiB of RAM. A local build or test names its crates with
`-p`: the crates you changed, plus the crates that use a public item you changed.
Never run `--workspace`, `cargo xtask miri`, `loom`, or `shuttle` on the laptop; CI
runs them. On 2026-10-05 the person said: "You agents need to be careful about how they
use memory" (RAM).

## Issues

Every task is a GitHub issue. An issue states its goal, the crates it changes, the tests
that must pass, and the section of `docs/decisions.md` it builds.

Labels:

- `owner:<name>` -> the session that took the task.
- `crate:<name>` -> which crates it changes.
- `ready` -> on the milestone path, complete, and not blocked, so a builder may take it.
  Only the coordinator adds it.
- `night` -> ready for the night lane. Only the architect adds it.
- `model:fable` -> runs on Fable. Only the person or the architect adds it.
- `interface` -> a request to change a public surface or a crate dependency.
- `oracle` -> it changes `oracles/`.
- `blocked` -> waiting on another issue, linked in the body.
- `security` -> a security finding.

Builders file the next issues on the milestone path from `docs/decisions.md`, and the
coordinator admits them. Any builder takes any crate. One task is in progress per crate.

## Pull requests

- **Branch:** `<machine>/<issue>-<slug>`, for example `box1/462-hub-route`.
- **Title:** `<crate>: Sentence case description`, for example
  `types: Add interned key sets`.
- **Body:** the template in `.github/pull_request_template.md`. It links the issue,
  lists oracle changes, and answers the six performance questions when the PR touches
  a hot path.
- **Gate:** before `ready`, the author runs CI's PR job set on the affected crates
  (`/build`). CI then confirms instead of catching.
- **Review:** the author runs `/review <pr>`. Fresh reviewers check the diff by tier,
  and a second round checks the fix commits. Their findings go on the PR as comments.
  The author fixes each finding or answers it on the PR.
- **Ready:** the author runs `gh pr ready` when the gate passed, every review finding is
  fixed or answered, and the body is complete: oracle changes, Complexity, Shape
  decisions, and the six performance answers on a hot path.
- **Merge:** `gh pr merge <n> --auto` puts it in the merge queue (`docs/factory.md`).

## Interface changes

Builders never edit another crate's public surface. To change one, or to add a crate
dependency:

1. Open an issue labeled `interface` with the proposed signature and the reason. Send
   the link to the crate's architect (`docs/factory.md`).
2. The architect decides. A change inside the locked decisions becomes a small PR from
   the crate's builder. A change to a locked decision, a contract, or an oracle goes to
   the person first, with the architect's recommendation.
3. After the merge, the architect files an issue for each crate that must follow the
   change.

A builder may change anything private inside the crates of its issue without asking.

Two cases skip the interface issue:

- A crate with no public surface yet gets one in its builder's first PR. The architect
  reviews that surface before the PR merges.
- An additive change to a builder's own crate that a locked decision already requires.
  The architect approves it in a comment on the issue, and the builder makes it in its
  PR.

## Cloud machines

Only the red-team sessions rent machines, within the test budget (`docs/decisions.md`
5.5): 1000 USD in total and at most 100 USD a day.

1. A builder that needs one asks a red-team on its issue: instance types, count, and
   hours.
2. Before launch, the renting session posts the cap on the spend ledger issue (#15):
   on-demand price per hour times count times lifetime. The sum of caps stays inside
   the limit.
3. Every instance has the tags `project=foundation-bench` (or `foundation-test`) and
   `issue=<n>`, shutdown behavior `terminate`, a root volume that is deleted on
   termination, and user data that runs `shutdown -h +<minutes>` at boot. The lifetime
   is at most 240 minutes.
4. The renting session terminates the instances when the run ends, checks that none of
   its tagged instances still run, and posts the actual hours on the ledger.

## Messages

`docs/factory.md` says how sessions reach each other.

- Use messages for questions, review requests, and notices. Keep them short.
- **A message is not a record.** Write the decision into the issue, the PR, or the docs
  first, then send the link.
- Do not send a message to check if a session is alive.
- **A relay of the person counts as the person.** When the coordinator or the monitor
  posts the person's decision on an issue or PR from the factory account, it is the
  person's own OK, approval, or waiver (also as box engineer). A builder acts on it at
  once. The person never has to comment on GitHub.
- **The person reviews only what only the person can decide.** The architect decides
  everything inside the decisions the person made, in a comment on the issue. The
  builder adds the ruling to `docs/decisions.md` in the code PR, so the record and the
  code merge together. Public surfaces (`public-api.txt`) need the architect's approval,
  not the person's. Only four things go to the person: a change to a decision the person
  made, the next milestone, new spend, and a security or license risk. The coordinator
  sends them in one batch a day, except one that blocks the critical path.
- **A stuck session tells the coordinator at once.** When a permission check refuses
  a call, or work waits on the person, send `coordinator` the refused command, the
  reason text, and the issue or PR. Then stop and wait. The coordinator takes it to the
  person. Never try to get around a refusal.
- **A question for the person** states the problem, the fix, its cost, and a
  recommendation. For each option, it says whether it is a patch or the long-term
  path; for a patch, it names the long-term fix. The person decided on 2026-10-05:
  "whenever you present thes, you need to tell me hwether its a patch and not a long
  term fix or the long term path".

## Before a session stops

Comment on each open issue you own: what is done, the next step, and open questions. A
new session starts from that comment.

## Outside projects

Never open an issue, PR, or comment on a project outside `synnaxlabs`. It publishes
from the person's account. Fix a dependency with a local patch
(`docs/dependencies.md`).

## Escalate to the person when

Ask in a few short, plain sentences: what breaks, why, the fix, and your default. Never
use a multi-select dropdown.

- a change touches a locked decision, a contract, or an oracle;
- a contract disagreement that the architect cannot settle inside the locked decisions;
- a PR adds a third-party dependency (record it in `docs/dependencies.md`);
- work would spend money: cloud resources or paid services, except rented machines
  within the test budget.
