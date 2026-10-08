- **REVIEW CHECK (2026-10-07)** The required status `review` (`cargo xtask review`,
  `.github/workflows/review.yaml`) passes a PR only when its review is done. It reads
  only round comments by the factory bot, in the format of `/review`, "Round comment".
  Each round comment parses, also an earlier one, and has its `Deferred:`,
  `Public surface:`, and `Hot path:` lines. Each round names the reviewers REVIEW TIERS
  requires, and `performance` when the first word of its `Hot path:` value is not
  `none`. Stated by the issue that the director's audits filed,
  https://github.com/synnaxlabs/foundation/issues/1467 (2026-10-07T15:20:38Z).
  The end lines are the last paragraph of the comment, in that order, as `/review`,
  "Round comment", writes them, each at the start of a line as GitHub shows it. So a
  line in a code block is not one, and a line that continues the paragraph with an
  indent is. Each end line may wrap onto the lines after it, and a paragraph after
  them fails. The first word of a value, with its backticks and one final comma,
  period, or semicolon removed, is the word that is checked. Decided by the director at
  2026-10-08T02:57:36Z
  (https://github.com/synnaxlabs/foundation/issues/1467#issuecomment-6051244793). The
  check reads the comment as GitHub does: as CommonMark with each GitHub extension
  (tables, footnotes, strikethrough, task lists, and quote kinds), by `pulldown-cmark`,
  with the spaces and tabs at the end of each line removed. A line ends at `\n`,
  `\r\n`, or a lone `\r` (decided by the director at 2026-10-08T04:42:33Z,
  https://github.com/synnaxlabs/foundation/pull/1752#issuecomment-6052414062). The
  round heading is the first top-level `## Review round <n>` heading. The fields are
  the first top-level block after it, and the end lines are the last one, each when it
  is a paragraph. A code block, an HTML block, a list, a quote, or a table is not a
  paragraph, so a list after the `Hot path:` line fails. A round comment posted after
  the cutoff fails when it holds raw HTML outside a code span or a code block: an HTML
  block or inline HTML as `pulldown-cmark` reads it, or a line of text whose source
  starts with `<` and a letter, `!`, `/`, or `?` and that is not an autolink. The
  message names the line. GitHub reads some of these lines in a different way, and an
  open `<!--` or `<details>` hides the text after it. A round comment that fails by
  this rule gets an edit that puts the line in a code span, and the cutoff stays. In
  an old round, a `Hot path:` line, or a `Reviewers:` line of a round that does not
  parse, counts where GitHub shows it as a line of text of a paragraph, at any depth
  and any indent. A line of a code block or an HTML block does not count. Changed by
  https://github.com/synnaxlabs/foundation/issues/1783, with the rulings of the
  director at 2026-10-08T21:47:24Z
  (https://github.com/synnaxlabs/foundation/pull/1999#issuecomment-6069679983),
  approved by the director at DIRECTOR.
  Supersedes the code block and indent rules of
  https://github.com/synnaxlabs/foundation/pull/1752#issuecomment-6051923239,
  https://github.com/synnaxlabs/foundation/pull/1752#issuecomment-6052414062,
  https://github.com/synnaxlabs/foundation/pull/1752#issuecomment-6052814147,
  https://github.com/synnaxlabs/foundation/pull/1752#issuecomment-6053059328, and
  https://github.com/synnaxlabs/foundation/pull/1752#issuecomment-6053208426. A round
  comment posted before the cutoff `CUTOFF` in
  `xtask/src/review.rs` (2026-10-08T03:00:00Z) is checked as before: an earlier
  free-form round passes, and it needs no end lines. A `Hot path:` line anywhere in its
  text that names a function still needs `performance`. Decided by the director at
  2026-10-08T02:44:00Z
  (https://github.com/synnaxlabs/foundation/pull/1752#issuecomment-6051099968).
  Supersedes the reviewers of a later round in ruling 2 of
  https://github.com/synnaxlabs/foundation/issues/1169#issuecomment-6040439732
  (2026-10-07T06:32:32Z). For this rule, an old round that parses names
  `performance` in its `Reviewers:` field, read as before. One that does not parse
  names it in any `Reviewers:` line of its text. A `Hot path:` line counts anywhere in
  its text. Decided by the director at
  2026-10-08T04:42:33Z
  (https://github.com/synnaxlabs/foundation/pull/1752#issuecomment-6052414062).
  The last round finds none and ends at the head, or at a commit that reaches the head
  through clean merges of the base (`git merge-tree`). A merge of the base is not clean
  when the base moves a path that the PR changed since their merge base, and that is not
  code, to a code path, by the rename detection of the merge. A base move of a path that
  the PR did not change stays clean. Decided by the director at 2026-10-07T17:21:14Z
  (https://github.com/synnaxlabs/foundation/issues/1496#issuecomment-6043078385). When
  the last round is a later round with `Breaker: skipped`, it fails if its range changes
  code: a `.rs` line that, trimmed, is not blank and does not start with `//` (a doctest
  line is a comment), or any `Cargo.toml` or `Cargo.lock` line. Each line of a moved
  file counts as removed and added. A merge of the base in the range counts only by its
  resolution: a conflict that `git merge-tree` finds between its parents in a `.rs`,
  `Cargo.toml`, or `Cargo.lock` file is a code change. The rest of the range is read
  from the tree that `git merge-tree` makes of its start and the newest base commit that
  its end holds, not from its start: the base's code does not count, and text that the
  range changes and the base moves into a code file does. So does a path that is not
  code, that the start changes since its merge base with that base commit, as the merge
  reads it, and that this merge moves into a code file, by the merge's own rename
  detection. A conflict in this tree in a code file is a code change, also one that
  leaves no markers, and so is an end that holds more than one newest base commit. In
  the range, a base move counts only through this tree. Decided by the director at
  2026-10-07T18:27:30Z
  (https://github.com/synnaxlabs/foundation/issues/1496#issuecomment-6044222273).
  Supersedes the range sentence of
  https://github.com/synnaxlabs/foundation/issues/1496#issuecomment-6043078385. Found
  by the director at 2026-10-07T14:50:33Z
  (https://github.com/synnaxlabs/foundation/pull/1193#issuecomment-6040535575), fixed by
  #1451. An earlier round's skip is taken as written, since a rebase can drop its range
  from the clone. An earlier round in the fixed format that does not parse fails. A
  red-team `oracle` PR also needs ``Director: approved at `<sha>` `` at the head. The
  status is `success` on `merge_group`. Decided by the director on #1169
  (https://github.com/synnaxlabs/foundation/issues/1169#issuecomment-6032179989) and in
  messages on #1193.
