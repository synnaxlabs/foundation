#!/bin/sh
# Prints each open `blocked` issue whose body has a line `Waits on #<n>, #<m>.` that
# names only closed issues and PRs, by number, as `<n> <owner> <title>`, with `-` for no
# `owner:` label. Exits non-zero, with no list, when a `gh` call fails.
set -eu

# The limits are above the counts of open PRs and issues, so each list is whole.
prs=$(gh pr list --state open --limit 1000 --json number --jq '[.[].number]')
waits=$(gh issue list --state open --limit 10000 --json number,title,body,labels \
  --jq "$prs"' as $prs | ($prs + [.[].number]) as $open
  | .[] | select(any(.labels[]; .name == "blocked"))
  | [.body | split("\n")[] | rtrimstr("\r")
    | select(test("^Waits on #[0-9]+((,| and|, and) #[0-9]+)*\\.?$"))
    | scan("#([0-9]+)")[0] | tonumber] as $on
  | select($on != [] and all($on[]; . as $n | any($open[]; . == $n) | not))
  | ([.labels[].name | select(startswith("owner:")) | ltrimstr("owner:")][0] // "-")
    as $owner
  | "\(.number) \($owner) \(.title)"')
printf '%s' "$waits" | sort -n
