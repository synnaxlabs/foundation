#!/bin/sh
# Runs free.sh against fixed lists of open PRs and issues. A stub `gh` applies the
# `--jq` of each call to its list, as gh does. Needs `jq`. Exit 1 on a failure.
set -u
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/bin"
cat > "$tmp/bin/gh" <<'STUB'
#!/bin/sh
list=$STUB/$1.json
while [ "$#" -gt 0 ] && [ "$1" != --jq ]; do shift; done
jq -r "$2" "$list"
STUB
chmod +x "$tmp/bin/gh"
cat > "$tmp/pr.json" <<'JSON'
[{"headRefName": "box1/10-held"}, {"headRefName": "laptop/main-fix"},
 {"headRefName": "box2/99-other"}, {"headRefName": "box2/12x-no-issue"}]
JSON
cat > "$tmp/issue.json" <<'JSON'
[
 {"number": 10, "title": "pr", "createdAt": "2026-01-10T00:00:00Z",
  "labels": [{"name": "owner:a"}, {"name": "crate:pr"}, {"name": "blocked"}]},
 {"number": 11, "title": "built", "createdAt": "2026-01-11T00:00:00Z",
  "labels": [{"name": "owner:b"}, {"name": "crate:built"}]},
 {"number": 12, "title": "blocked", "createdAt": "2026-01-12T00:00:00Z",
  "labels": [{"name": "owner:c"}, {"name": "crate:blocked"}, {"name": "blocked"}]},
 {"number": 1, "title": "on pr", "createdAt": "2026-01-01T00:00:00Z",
  "labels": [{"name": "ready"}, {"name": "crate:pr"}]},
 {"number": 2, "title": "on built", "createdAt": "2026-01-02T00:00:00Z",
  "labels": [{"name": "ready"}, {"name": "crate:built"}]},
 {"number": 3, "title": "on blocked", "createdAt": "2026-01-05T00:00:00Z",
  "labels": [{"name": "ready"}, {"name": "crate:blocked"}]},
 {"number": 4, "title": "no crate", "createdAt": "2026-01-06T00:00:00Z",
  "labels": [{"name": "ready"}]},
 {"number": 5, "title": "night", "createdAt": "2026-01-04T00:00:00Z",
  "labels": [{"name": "night"}, {"name": "crate:free"}, {"name": "ready"}]},
 {"number": 6, "title": "one held", "createdAt": "2026-01-03T00:00:00Z",
  "labels": [{"name": "ready"}, {"name": "crate:blocked"}, {"name": "crate:built"}]},
 {"number": 7, "title": "not ready", "createdAt": "2026-01-01T00:00:00Z",
  "labels": [{"name": "crate:free"}]}
]
JSON
failed=0
check() {
  want=$1
  shift
  got=$(PATH="$tmp/bin:$PATH" STUB=$tmp sh "$here/free.sh" "$@" 2>&1)
  [ "$got" = "$want" ] && return
  printf 'free.sh %s: want\n%s\ngot\n%s\n' "$*" "$want" "$got"
  failed=1
}
check "5 night
3 on blocked
4 no crate"
check "5 night" night
exit "$failed"
