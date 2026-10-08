- **SMALL CHANGES (2026-10-08)** A change of under about 50 lines (a fix, a test pin, a
  doc fix, a rename, or a record) goes into the PR that its session builds in its crate
  or its file, as its own commit, never a PR of its own. A review finding with such a
  fix in a crate or a file that the PR changes is fixed in that PR. Else it is an item
  of an open issue in its crate, by preference one whose PR has had no review round. It
  goes alone only when no open issue in its crate fits, when it fixes a broken `main`,
  or when other work waits on it. A small mechanical change, and a small refactor that a
  fix needs, follow the same rule, as their own commit before the fix; a larger one
  ships alone. Each architect, red-team, and `laptop.monitor` keeps one PR open for its
  own small changes, sent to review at most once a day, or at once when other work waits
  on it (`docs/coordination.md`, "Small changes"). Of the 252 PRs that merged in the 24
  h to 2026-10-08T03:05Z, 65 changed 50 lines or fewer, and each paid the full fixed
  cost of CI, review rounds, an audit, and a queue slot (#1705: 8 lines, two review
  rounds, and an audit). The person decided to fold small fixes into open PRs, relayed
  by `laptop.monitor`
  (https://github.com/synnaxlabs/foundation/issues/462#issuecomment-6051321753,
  2026-10-08T03:05:03Z): "We should batch small optimizations/fixes into single pull
  requests. One set of test runs, one set of reviews. Less context and less
  infrastructure cost", and, on a proposed batch branch, "these batch branches could
  hold up progress on the next piece. Instead they should preferrably be folded into
  current or existing larger PRs". The rest was decided by the director at
  2026-10-08T03:23:34Z
  (https://github.com/synnaxlabs/foundation/pull/1706#issuecomment-6051516851), with the
  link of its item 7 corrected at 2026-10-08T03:31:23Z
  (https://github.com/synnaxlabs/foundation/pull/1706#issuecomment-6051596894) and its
  item 2 widened to a file at 2026-10-08T03:39:52Z
  (https://github.com/synnaxlabs/foundation/pull/1706#issuecomment-6051684071).
  Supersedes the mechanical-change and refactor sentences of `CLAUDE.md` Rule 2
  (https://github.com/synnaxlabs/foundation/blob/8f6a0596/CLAUDE.md#L200-L202).
