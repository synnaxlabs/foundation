#!/bin/sh
# Prints one line for each PR that merges into main from now on: "#<n> +<add> -<del>
# <title>". Run it with Monitor; it never exits.
set -u
# The default order is by creation, which drops an old PR that merges late. An edit on
# an old PR moves it up the update order, so list only the PRs merged since the start.
start=$(date -u +%Y-%m-%dT%H:%M:%SZ)
list() {
  gh pr list --state merged --limit 30 \
    --search "base:main merged:>=$start sort:updated-desc" \
    --json number,additions,deletions,title \
    --jq '.[] | "#\(.number) +\(.additions) -\(.deletions) \(.title)"'
}
seen=$(mktemp)
# awk reads the seen keys only from a file that is not empty, so start with a key that
# no PR has.
{ echo '#0'; list | cut -d' ' -f1; } > "$seen"
while :; do
  sleep 90
  now=$(list 2>/dev/null) || continue
  [ -n "$now" ] || continue
  # Compare only the first field: a title can name a PR that merged before.
  echo "$now" | awk 'NR==FNR { s[$1]; next } !($1 in s)' "$seen" -
  echo "$now" | cut -d' ' -f1 >> "$seen"
done
