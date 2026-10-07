#!/bin/sh
# Prints one line for each PR that merges into main from now on: "#<n> +<add> -<del>
# <title>". Run it with Monitor; it never exits.
set -u
list() {
  gh pr list --state merged --limit 30 --json number,additions,deletions,title \
    --jq '.[] | "#\(.number) +\(.additions) -\(.deletions) \(.title)"'
}
seen=$(mktemp)
list | cut -d' ' -f1 > "$seen"
while :; do
  sleep 90
  now=$(list 2>/dev/null) || continue
  echo "$now" | grep -v -F -w -f "$seen" || true
  echo "$now" | cut -d' ' -f1 >> "$seen"
done
