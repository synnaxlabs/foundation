# Coordination

Several Claude sessions build Foundation at the same time. This file is how they work
together. When this file and a message disagree, this file wins.

## Roles

**The person** owns contracts and oracles, merges every PR that is not routine
(below), and answers escalations.

**The coordinator** (session `coordinator`) owns:

- the interface skeleton: the public surface of every crate;
- `docs/decisions.md`;
- the issue board: it assigns crates to builders and checks that no two open issues
  own one crate;
- the merge queue: it checks each PR's gates, asks the person to merge, and merges
  routine PRs that wait (below).

The coordinator does not build crates.

**Builders** (sessions named for their area, such as `data-path`) each own a set of
crates. A builder files and takes issues for its crates from `docs/decisions.md`,
writes code and tests, opens PRs, and runs adversarial review on them.

**The advisor** (session `advisor`) is the design session that ran the interview. It
answers "why did we decide this" questions. It does not write code in this repo.

**The crew** are subagents defined in `.claude/agents/`. Any session runs them for
review. The coordinator runs the daily quality pass with `/crew`.

## Models

- **Fable 5.1** where a subtle mistake is expensive and hard to find later: the
  `memory`, `consensus`, and `storage` builders, and reviewers for `raft`, `mesh`,
  `block`, `ring`, `buffer`, crash recovery, lock-free code, and wake protocols. Start
  those sessions with `--model fable`.
- **Opus 5.5** for the coordinator, the other builders, and other reviewers.
- **Sonnet 5.5** for mechanical work: format runs, renames, regenerated code,
  CI-only fixes, and the `code-quality` and `drift` crew agents. A builder hands a
  mechanical task to a subagent with `model: "sonnet"`.

Fable uses plan limits faster. Widen or narrow its use from what the limits show.

## Tokens

Most of the cost is context size per turn, so keep each context small:

- `.claude/settings.json` compacts a session near 200k tokens, keeps the prompt cache
  five minutes (99% of calls come sooner), and turns off plugins we never use.
- Every token in context is read again on every later call until compaction. Read the
  lines you need (`grep -n`, then `sed -n` or Read with a range), never a whole file
  or log; look at `--stat` or `--name-only` before a diff; and cut long output with
  `tail`.
- Wait for CI with one background `gh pr checks <n> --watch`, not repeated checks. A
  Monitor must filter to events you act on.
- Never fork from a large context. Brief a fresh subagent instead.
- The person: a `/login` that switches organizations flushes every session's cache.
- Read only the sections of `docs/decisions.md` and `docs/research/` you need.
- Send reading, searching, and reviews to subagents; keep their results, not their
  file dumps.
- Finish each issue with its state comment, so compaction or `/clear` loses nothing.
- On usage credits, the prompt cache lives five minutes. A session that sleeps longer
  reads its whole context again at full price, so `/clear` before a long wait.

## Current sessions

| Session | Model | Owns |
| --- | --- | --- |
| **Laptop** | | |
| `coordinator` | Opus | Interfaces, `docs/decisions.md`, issues, merge queue |
| `memory` | Fable | `block`, `ring` |
| `data-path` | Opus | `types`, `codec`, `wire` |
| `consensus` | Fable | `raft`, `spec`, then `mesh`, `blob` |
| `simulation` | Opus | `env`, `os`, `sim`, the QUIC against TLS over TCP benchmark |
| `write-path` | Opus | `control`, `delivery`, then `home` |
| `storage` | Fable | `buffer`, then `replica` |
| `time` | Opus | `estimate`, then `clock` |
| `config` | Opus | `document`, `config-hcl`, then `config` |
| `network` | Opus | `transport` |
| `advisor` | Opus | Answers design questions; writes no code here |
| **Factory host** | | |
| `hub` | Fable | `hub`; starts after `home`'s write path and `mesh`'s snapshot and watch |
| `connector` | Opus | `connector` (the kind contract, supervisor, `ctx`, components) |
| `access` | Opus | `secret`, then `access` after `spec` (#43) |
| `ops` | Opus | `node` first as a walking skeleton for `verify`, then `ops` |
| `opcua` | Opus | `connector-opcua` |
| `modbus` | Opus | `connector-modbus` |
| `ni` | Opus | `connector-ni` |
| `influx` | Opus | `connector-influx` |
| `verify` | Opus | `acceptance` (MVP tests, test-only), the chaos lab |
| `red-team` | Fable | `fuzz/`, additions under `oracles/`, simulation swarms |
| **Cloud routines** | | |
| `audit` | Opus | Architecture, practices, and performance, per merged PR |
| `ux` | Opus | The end user's experience, per merged PR that a user touches |
| `red-team` attack | Fable | Attacks each merged PR in layer 1 and layer 2 crates |

A connector kind starts with its protocol codec and its device simulator, which need
only layer 1. It moves onto the `connector` contract when that surface merges.

Sessions on the two machines talk through Remote Control. Turn it on for every new
session in `/config` ("Enable Remote Control for all sessions"), or run
`/remote-control` in a running one.

Two first surfaces have a named reviewer besides the coordinator: `consensus` reviews
`document`, because `spec` uses it; `simulation` reviews `transport`, because `sim`
simulates it.

The coordinator updates this table when sessions or ownership change.

## Starting a session

1. Start Claude with the role as its name: `claude -n data-path`.
2. Builders work in their own worktree (below). The coordinator works in the main
   checkout.
3. Run the role's skill: `/coordinate` or `/build`.

## Running continuously

A session works without a person between tasks in one of two ways:

- **A loop** runs a skill again and again, and the session picks its own wait between
  runs. Builders run `/loop /build`. The coordinator runs `/loop /coordinate`.
- **A goal** keeps a session working until a condition holds, for one known
  deliverable. A small model judges
  the condition from the transcript only, so the builder prints the evidence at the
  end of each turn. Set it after `/build`:
  ```
  /goal Issues #12 and #13 are closed or have an open PR labeled ready. The last
  output in the transcript is `gh issue view 12` and `gh issue view 13`.
  ```
  A small model judges the condition from the transcript only, so name the evidence.
  `/goal` shows its status. `/goal clear` removes it.

Messages from other sessions wake an idle session in both cases. Start long runs with
`--permission-mode auto` so routine commands do not wait for a person.

## Worktrees

The main checkout is `~/Desktop/synnaxlabs/foundation`. Each builder has one long-lived
worktree:

```sh
git -C ~/Desktop/synnaxlabs/foundation worktree add \
  ~/Desktop/synnaxlabs/foundation-wt/<name> origin/main --detach
```

A builder makes a branch per issue inside its worktree. Never work in another session's
worktree.

Never share `CARGO_TARGET_DIR` between worktrees. Cargo gives a path crate the same hash
in each, so a stale build of another worktree's code can pass or fail a gate.

## Issues

Every task is a GitHub issue. An issue states its goal, the crates it owns, the tests
that must pass, and the section of `docs/decisions.md` it builds.

Labels:

- `owner:<session>` -> which session has the task.
- `crate:<name>` -> which crates it changes.
- `interface` -> a request to change a public surface.
- `oracle` -> it changes `oracles/`.
- `blocked` -> waiting on another issue, linked in the body.
- `ready` -> a PR that passed its gates and review, waiting for the person.

Each builder files the issues for its own crates from `docs/decisions.md` and the RFC
phases, with the `owner:` and `crate:` labels. The coordinator files only issues that
cross crates or owners. One task is in progress per crate. A builder may file the
next issue for a crate early, labeled `blocked` with a link to the open one.

## Pull requests

- **Branch:** `<session>/<issue>-<short-name>`, for example
  `data-path/12-key-set`.
- **Title:** `<crate>: Sentence case description`, for example
  `types: Add interned key sets`.
- **Body:** the template in `.github/pull_request_template.md`. It links the issue,
  lists oracle changes, and answers the six performance questions when the PR touches
  a hot path.
- **Gates:** CI passes (format, Clippy, layer check, tests).
- **Review:** the author runs `/review <pr>`. Two fresh adversarial reviewers check
  the diff, and their findings go on the PR as comments. The author fixes each finding
  or answers it on the PR. When the PR changes how a crate is used, the author also
  asks the owners of the crates that use it.
- **Ready:** the author adds `ready` when every check on the PR's current head passed,
  the PR has no conflict with `main`, every review finding is fixed or answered, and
  the body is complete: oracle changes, Complexity, Shape decisions, and the six
  performance answers on a hot path. The exception is a PR that changes a public
  surface, a locked decision, or an oracle: the author messages the coordinator
  instead, and only the coordinator adds `ready`.
- **Merge:** the coordinator tells the person about each new `ready` PR, one line
  each. The person merges with a squash.
- **Routine merge:** when the person has not merged a routine PR 30 minutes after it
  got `ready`, the coordinator reads every check on its current head again and merges
  it the way the person does, then tells the person. A PR is routine when it adds,
  removes, or changes no `pub` item, has no `interface` label, and touches nothing in
  `oracles/`, `docs/decisions.md`, `docs/coordination.md`, `CLAUDE.md`, `.github/`,
  `.claude/`, `.cargo/`, `xtask/`, `clippy.toml`, or any `Cargo.toml`.

## Interface changes

Builders never edit another crate's public surface. To change one:

1. Open an issue labeled `interface` with the proposed signature and the reason.
   Message the coordinator with the link.
2. The coordinator decides. A change inside the locked decisions becomes a small PR:
   from the owner when no other crate uses the surface yet, else from the
   coordinator. A change to a locked decision, a contract, or an oracle goes
   to the person first.
3. After the merge, the coordinator messages the owner of every crate that uses the
   changed surface. Each one rebases.

A builder may change anything private inside its own crates without asking.

Two cases skip the interface issue:

- A crate with no public surface yet gets one in its owner's first PR. The
  coordinator reviews that surface as an interface before it adds `ready`.
- An additive change to a builder's own crate that a locked decision already requires.
  The coordinator approves it in a comment on the issue, and the builder makes it in
  its PR.

## Cloud machines

The coordinator, `verify`, and `red-team` rent machines within the test budget
(`docs/decisions.md` 5.5): 1000 USD in total and at most 100 USD a day.

1. The builder asks on its issue: instance types, count, and hours.
2. Before launch, the renting session posts the cap on the spend ledger issue (#15):
   on-demand price per hour times count times lifetime. The sum of caps stays inside
   the limit.
3. Every instance has the tags `project=foundation-bench` (or `foundation-test`) and
   `issue=<n>`, shutdown behavior `terminate`, a root volume that is deleted on
   termination, and user data that runs `shutdown -h +<minutes>` at boot. The lifetime
   is at most 240 minutes.
4. The renting session terminates the instances when the run ends and posts the
   actual hours on the ledger. The coordinator checks for running tagged
   instances on each loop.

## Messages

Sessions message each other with `SendMessage`, by name. Find names with
`ListAgents`.

- Use messages for questions, review requests, and notices. Keep them short.
- **A message is not a record.** Messages are lost when a session restarts, and a held
  message expires after five minutes. Write the decision into the issue, the PR, or the
  docs first, then send the link.
- An idle session wakes when a message arrives. Do not send a message to check if a
  session is alive.

## Before a session stops

Comment on each open issue you own: what is done, the next step, and open questions. A
new session starts from that comment.

## Outside projects

Never open an issue, PR, or comment on a project outside `synnaxlabs`. It publishes
from the person's account. Fix a dependency with a local patch
(`docs/dependencies.md`).

## Escalate to the person when

- a change touches a locked decision, a contract, or an oracle;
- two sessions still disagree after one exchange;
- a PR adds a third-party dependency (record it in `docs/dependencies.md`);
- work would spend money: cloud resources or paid services, except rented machines
  within the test budget.
