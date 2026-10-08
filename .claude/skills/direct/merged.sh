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
# macOS grep prints nothing for an empty pattern file, so start with a key no PR has.
{ echo '#0'; list | cut -d' ' -f1; } > "$seen"
while :; do
  sleep 90
  now=$(list 2>/dev/null) || continue
  [ -n "$now" ] || continue
  echo "$now" | grep -v -F -w -f "$seen" || true
  echo "$now" | cut -d' ' -f1 >> "$seen"
done
