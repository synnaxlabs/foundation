#!/bin/sh
# Prints each open `blocked` issue, by number, as `<n> <owner> <state>`, with `-` for no
# `owner:` label. Its lines `Waits on #<n>, #<m>.` give the state: `ended` when each
# issue that they name closed as completed and each PR merged, `dropped` when one closed
# another way and none is open, and `no line` with no such line. A line that names an
# open one or `the person` prints nothing. Exits non-zero, with no list, when a `gh`
# call fails, also for a number that names no issue or PR.
set -eu

# The limit is above the count of open `blocked` issues, so the list is whole. The first
# line is the GraphQL fields for the numbers that the lines name, the second the waits.
out=$(gh issue list --state open --label blocked --limit 1000 \
  --json number,body,labels --jq '[.[]
    | [.body | split("\n")[] | rtrimstr("\r") | select(test(
        "^Waits on (#[0-9]+|the person)((,| and|, and) (#[0-9]+|the person))*\\.?$"))]
      as $lines
    | {n: .number, line: ($lines != []), person: any($lines[]; test("the person")),
       on: [$lines[] | scan("#([0-9]+)")[0]],
       owner: ([.labels[].name | select(startswith("owner:")) | ltrimstr("owner:")][0]
         // "-")}] as $w
  | ([$w[].on[]] | unique | map("n\(.): issueOrPullRequest(number: \(.)) {"
      + " ... on Issue { state stateReason } ... on PullRequest { state } }")
      | join(" ")), ($w | tojson)')
fields=$(printf '%s\n' "$out" | sed -n 1p)
waits=$(printf '%s\n' "$out" | sed -n 2p)
waits=$(gh api graphql -F owner='{owner}' -F name='{repo}' -f query='
  query($owner: String!, $name: String!) {
    repository(owner: $owner, name: $name) { __typename '"$fields"' } }' \
  --jq "$waits"' as $w | .data.repository as $r | $w[]
  | [.on[] | $r["n\(.)"] | if .state == "OPEN" then "open"
      elif .state == "MERGED" or .stateReason == "COMPLETED" then "done"
      else "dropped" end] as $on
  | if .line | not then "\(.n) \(.owner) no line"
    elif .person or any($on[]; . == "open") then empty
    elif any($on[]; . == "dropped") then "\(.n) \(.owner) dropped"
    else "\(.n) \(.owner) ended" end')
printf '%s' "$waits" | sort -n
