#!/bin/sh
# Prints each open `blocked` issue, by number, as `<n> <owner> <state>`, with `-` for no
# `owner:` label. The state is `ended` when its lines `Waits on #<n>, #<m>.` name only
# closed issues and PRs, and `no line` when it has no such line. A line that also names
# `the person` prints nothing. Exits non-zero, with no list, when a `gh` call fails.
set -eu

# The limits are above the counts of open PRs and issues, so each list is whole.
prs=$(gh pr list --state open --limit 1000 --json number --jq '[.[].number]')
waits=$(gh issue list --state open --limit 10000 --json number,body,labels \
  --jq "$prs"' as $prs | ($prs + [.[].number]) as $open
  | .[] | select(any(.labels[]; .name == "blocked"))
  | [.body | split("\n")[] | rtrimstr("\r") | select(test(
      "^Waits on (#[0-9]+|the person)((,| and|, and) (#[0-9]+|the person))*\\.?$"))]
    as $lines
  | [$lines[] | scan("#([0-9]+)")[0] | tonumber] as $on
  | ([.labels[].name | select(startswith("owner:")) | ltrimstr("owner:")][0] // "-")
    as $owner
  | if $lines == [] then "\(.number) \($owner) no line"
    elif any($lines[]; test("the person"))
      or any($on[]; . as $n | any($open[]; . == $n))
    then empty
    else "\(.number) \($owner) ended" end')
printf '%s' "$waits" | sort -n
