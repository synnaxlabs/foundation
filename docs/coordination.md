# Coordination

Several Claude sessions build Foundation at the same time. This file is how they work
together. When this file and a message disagree, this file wins.

## Roles

**The person** owns contracts and oracles, merges every PR, and answers escalations.

**The coordinator** (session `coordinator`) owns:

- the interface skeleton: the public surface of every crate;
- `docs/decisions.md`;
- the issue board: it turns RFC phases into issues and assigns them;
- the merge queue: it checks each PR's gates and asks the person to merge.

The coordinator does not build crates.

**Builders** (sessions named for their area, such as `data-path`) each own a set of
crates. A builder takes issues for its crates, writes code and tests, opens PRs, and
runs adversarial review on them.

**The advisor** (session `advisor`) is the design session that ran the interview. It
answers "why did we decide this" questions. It does not write code in this repo.

**The crew** are subagents defined in `.claude/agents/`. Any session runs them for
review. The coordinator runs the daily quality pass with `/crew`.

## Models

- **Fable 5.1** where a subtle mistake is expensive and hard to find later: the
  `memory` and `consensus` builders, and reviewers for `raft`, `mesh`, `block`,
  `ring`, lock-free code, and wake protocols. Start those sessions with
  `--model fable`.
- **Opus 5.5** for the coordinator, the other builders, and other reviewers.
- **Sonnet 5.5** for mechanical work: format runs, renames, regenerated code,
  CI-only fixes, and the `code-quality` and `drift` crew agents. A builder hands a
  mechanical task to a subagent with `model: "sonnet"`.

Fable uses plan limits faster. Widen or narrow its use from what the limits show.

## Tokens

Most of the cost is context size per turn, so keep each context small:

- `.claude/settings.json` compacts a session when its context reaches 300k tokens.
- Read only the sections of `docs/decisions.md` and `docs/research/` you need.
- Send reading, searching, and reviews to subagents; keep their results, not their
  file dumps.
- Finish each issue with its state comment, so compaction or `/clear` loses nothing.

## Current sessions

| Session | Model | Owns |
| --- | --- | --- |
| `coordinator` | Opus | Interfaces, `docs/decisions.md`, issues, merge queue |
| `memory` | Fable | `block`, `ring` |
| `data-path` | Opus | `types`, `codec`, `wire`, then `document` |
| `consensus` | Fable | `raft`, `spec`, then `mesh`, `blob` |
| `simulation` | Opus | `env`, `os`, `sim`, the QUIC against TLS over TCP benchmark, then `transport` |
| `advisor` | Opus | Answers design questions; writes no code here |

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

No two open tasks own the same crate.

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
- **Merge:** the coordinator adds `ready` and messages the person. The person merges
  with a squash.

## Interface changes

Builders never edit another crate's public surface. To change one:

1. Open an issue labeled `interface` with the proposed signature and the reason.
   Message the coordinator with the link.
2. The coordinator decides. A change inside the locked decisions becomes a small PR
   from the coordinator. A change to a locked decision, a contract, or an oracle goes
   to the person first.
3. After the merge, the coordinator messages the owner of every crate that uses the
   changed surface. Each one rebases.

A builder may change anything private inside its own crates without asking.

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

## Escalate to the person when

- a change touches a locked decision, a contract, or an oracle;
- two sessions still disagree after one exchange;
- a PR adds a third-party dependency (record it in `docs/dependencies.md`);
- work would spend money: cloud resources or paid services.
