#!/bin/sh
# Prints the `ready` issues that `/build` may take, oldest first, as `<n> <title>`. An
# argument such as `night` keeps only the issues with that label too. An issue with an
# `owner:` label holds its crates while an open PR closes it or has the branch
# `<machine>/<n>-<slug>`, or while it is not `blocked`. Exits non-zero, with no list,
# when a `gh` call fails.
set -eu

label=${1:-ready}

prs=$(gh pr list --state open --limit 1000 --json headRefName,closingIssuesReferences \
  --jq '[.[] | (.headRefName | capture("^[^/]+/(?<n>[0-9]+)-")? | .n | tonumber),
    .closingIssuesReferences[].number]')

# The limit is above the count of open issues, so the list is whole.
free=$(gh issue list --state open --limit 10000 --json number,title,createdAt,labels \
  --jq "
  $prs as \$prs
  | [.[] | {number, title, createdAt, labels: [.labels[].name]}
    | .crates = [.labels[] | select(startswith(\"crate:\"))]] as \$all
  | [\$all[] | select(any(.labels[]; startswith(\"owner:\")))
    | select(.number as \$n | any(\$prs[]; . == \$n)
      or (any(.labels[]; . == \"blocked\") | not))
    | .crates[]] as \$held
  | \$all[] | select(any(.labels[]; . == \"ready\"))
  | select(any(.labels[]; . == \"$label\"))
  | select(any(.crates[]; . as \$c | any(\$held[]; . == \$c)) | not)
  | [.createdAt, .number, .title] | @tsv")
tab=$(printf '\t')
printf '%s' "$free" | sort -t "$tab" -k1,1 -k2,2n | cut -f2- | tr '\t' ' '
