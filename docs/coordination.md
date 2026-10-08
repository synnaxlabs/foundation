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
  decisions, and the six performance answers on a hot path. For a red-team PR and the
  director's rule PR, `laptop.monitor` marks it ready and queues it (`docs/factory.md`,
  "Merge path").
- **Merge:** `gh pr merge <n> --auto` puts it in the merge queue (`docs/factory.md`).

## Small changes

Each PR pays a fixed cost: CI, its review rounds, an audit, and a slot in the merge
queue. So a small change goes into a larger PR, never a PR of its own (SMALL CHANGES in
`docs/decisions.md`).

- **Small change:** a fix, a test pin, a doc or comment fix, a rename, or a record, of
  under about 50 lines.
- **Fold it in:** when the PR that you build changes its crate, put it there as its own
  commit. Do it before that PR's first review round where you can, so that it shares
  that round.
- **Found by review:** a finding whose fix is a small change in a crate or a file that
  the PR changes is fixed in that PR, not deferred. Another one is an item, as below.
- **Else, an item:** a small change that you cannot fold in (from an audit, a weekly
  pass, or a crate that you do not build) is an item of an open issue in its crate: a
  comment that states the change, the test that pins it, and its source. Choose the
  issue whose PR has had no review round, by preference one in progress, else the next
  one in that crate. Its builder folds the item into its PR and lists it in the PR body.
- **Outside a crate:** a small change to a file in no crate, which no PR that you
  build changes, goes into the records PR of the file's owner (below): the architect
  of the crate that a decision covers (else `laptop.architect`) for
  `docs/decisions.md`, the red-team for `docs/security.md` and fuzz inputs,
  `laptop.monitor` for `docs/factory.md` and `docs/coordination.md`, and the director
  for the rules in `CLAUDE.md` and `.claude/`.
- **Alone:** a small change gets its own issue and PR only when no open issue in its
  crate fits and no records PR above takes it, when it fixes a broken `main`, or when
  other work waits on it.
- **Records:** each architect, red-team, and `laptop.monitor` keeps one PR open for its
  own small changes (decisions, threat model notes, fuzz inputs, factory docs), and
  sends it to review at most once a day, or at once when other work waits on it. A
  ruling that a code PR needs ships in that PR (`docs/factory.md`, "GitHub is the
  record"). The director's rule PR keeps its own pace (`/direct`, "The bar").

## Interface changes

Builders never edit another crate's public surface. To change one, or to add a crate
dependency:

1. Open an issue labeled `interface` with the proposed signature and the reason. Send
   the link to the crate's architect (`docs/factory.md`).
2. The architect decides. A change inside the locked decisions becomes a change from
   the crate's builder (a small one as "Small changes" says). A change to a locked
   decision, a contract, or an oracle goes to the person first, with the architect's
   recommendation.
3. After the merge, the architect files an issue for each crate that must follow the
   change, or an item of an open issue in that crate when its change is small ("Small
   changes").

A builder may change anything private inside the crates of its issue without asking.

Two cases skip the interface issue:

- A crate with no public surface yet gets one in its builder's first PR. The architect
  reviews that surface before the PR merges.
- An additive change to a builder's own crate that a locked decision already requires.
  The architect approves it in a comment on the issue, and the builder makes it in its
  PR.

## Cloud machines

Only `laptop.monitor` rents and ends machines. Test machines stay within the test budget
(`docs/decisions.md` 5.5): 1000 USD in total and at most 100 USD a day, and at most 15
USD a day for #1139. The ARM RUNNER hosts stay under AWS CEILING, outside the test
budget, its limits, and step 3. Step 4 checks each by its instance, because they have no
`issue` tag. No other session holds AWS credentials. The person decided this
(https://github.com/synnaxlabs/foundation/issues/15#issuecomment-6042582552,
2026-10-07T16:48:27Z).

1. A session that needs one asks `laptop.monitor` on its issue, then sends the link:
   the purpose, instance types, count, and hours. A request outside the budget goes to
   the person.
2. Before launch, `laptop.monitor` posts the cap on the spend ledger issue (#15), with
   the types, the issue, and the session that asked. The cap is the price per hour
   times count times the lifetime. For an on-demand host the price is the on-demand
   price. For a spot host the price is its `MaxPrice` plus 0.03 USD an hour for its
   disk and public address, because AWS never bills a spot host above its `MaxPrice`,
   and the lifetime gets 10 more minutes (`laptop.monitor`,
   https://github.com/synnaxlabs/foundation/issues/15#issuecomment-6042917805,
   2026-10-07T17:12:22Z). The sum of caps stays inside each limit: the total, the day,
   and the #1139 day. At launch, it posts on #15 one line for each instance: the
   instance, type, issue, session that asked, cap, and end time. It sends the asking
   session the address of each and how to reach it.
3. Every instance has the tags `project=foundation-bench` (or `foundation-test`) and
   `issue=<n>`, shutdown behavior `terminate`, a root volume that is deleted on
   termination, and user data that runs `shutdown -h +<minutes>` at boot. The lifetime
   is at most 240 minutes.
4. When the run ends, the asking session says so on its issue, then sends the link to
   `laptop.monitor`. `laptop.monitor` then terminates the instances, checks that no
   instance tagged `issue=<n>` still runs, and posts the actual hours on the ledger.
5. For #1139, `laptop.monitor` runs the bench script of #1487 itself, and
   `box2.red-team` keeps the script. Each host ends itself within 120 minutes
   (https://github.com/synnaxlabs/foundation/issues/15#issuecomment-6042420655,
   2026-10-07T16:40:04Z). The script ends the host and posts its end line, so step 4
   needs no message. When the script stops with no end line, `laptop.monitor` does
   step 4 itself.
6. The spend watch of `laptop.monitor` alerts on each instance that is not on #15.

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
