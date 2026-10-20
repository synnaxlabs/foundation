#!/bin/sh
# Runs free.sh against fixed lists of PRs and issues. A stub `gh` acts as gh does: it
# keeps the open items of its list, or all for `--state all`, cuts the list to `--limit`
# (30 when not given), and applies `--jq`. When `$STUB/fail.<pr|issue>` exists, it
# prints an error and exits 1. Needs `jq`. Exit 1 on a failure.
set -u
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/bin"
cat > "$tmp/bin/gh" <<'STUB'
#!/bin/sh
kind=$1 state=open limit=30 expr=.
while [ "$#" -gt 0 ]; do
  case $1 in
    --state) state=$2 ;;
    --limit) limit=$2 ;;
    --jq) expr=$2 ;;
  esac
  shift
done
[ ! -f "$STUB/fail.$kind" ] || { echo "HTTP 502: Bad Gateway" >&2; exit 1; }
jq -r --arg s "$state" --argjson l "$limit" \
  "[.[] | select(\$s == \"all\" or .state == \"OPEN\") | del(.state)] | .[:\$l]
  | $expr" "$STUB/$kind.json"
STUB
chmod +x "$tmp/bin/gh"
# 30 items before the ones that count, so a list cut to 30 misses them.
jq -n '[range(30) | {state: "OPEN", headRefName: "x/main-\(.)",
  closingIssuesReferences: []}]' > "$tmp/fill.pr.json"
jq -n '[range(30) | {state: "OPEN", number: (100 + .), title: "fill",
  createdAt: "2025-01-01T00:00:00Z", labels: []}]' > "$tmp/fill.issue.json"
jq -s '.[0] + .[1]' "$tmp/fill.pr.json" - > "$tmp/pr.json" <<'JSON'
[{"state": "OPEN", "headRefName": "box1/10-held", "closingIssuesReferences": []},
 {"state": "OPEN", "headRefName": "laptop/main-fix", "closingIssuesReferences": []},
 {"state": "OPEN", "headRefName": "box2/99-other",
  "closingIssuesReferences": [{"number": 16}, {"number": 14}]},
 {"state": "OPEN", "headRefName": "box2/12x-no-issue", "closingIssuesReferences": []},
 {"state": "CLOSED", "headRefName": "box1/12-closed", "closingIssuesReferences": []}]
JSON
jq -s '.[0] + .[1]' "$tmp/fill.issue.json" - > "$tmp/issue.json" <<'JSON'
[
 {"state": "OPEN", "number": 10, "title": "pr", "createdAt": "2026-01-10T00:00:00Z",
  "labels": [{"name": "owner:a"}, {"name": "crate:pr"}, {"name": "blocked"}]},
 {"state": "OPEN", "number": 11, "title": "built", "createdAt": "2026-01-11T00:00:00Z",
  "labels": [{"name": "owner:b"}, {"name": "crate:built"}]},
 {"state": "OPEN", "number": 12, "title": "blocked",
  "createdAt": "2026-01-12T00:00:00Z",
  "labels": [{"name": "owner:c"}, {"name": "crate:blocked"}, {"name": "blocked"}]},
 {"state": "CLOSED", "number": 13, "title": "closed",
  "createdAt": "2026-01-13T00:00:00Z",
  "labels": [{"name": "owner:e"}, {"name": "crate:blocked"}]},
 {"state": "OPEN", "number": 14, "title": "closed by a pr",
  "createdAt": "2026-01-14T00:00:00Z",
  "labels": [{"name": "owner:f"}, {"name": "crate:closes"}, {"name": "blocked"}]},
 {"state": "OPEN", "number": 1, "title": "on pr", "createdAt": "2026-01-01T00:00:00Z",
  "labels": [{"name": "ready"}, {"name": "crate:pr"}]},
 {"state": "OPEN", "number": 2, "title": "on built",
  "createdAt": "2026-01-02T00:00:00Z",
  "labels": [{"name": "ready"}, {"name": "crate:built"}]},
 {"state": "OPEN", "number": 3, "title": "on blocked",
  "createdAt": "2026-01-05T00:00:00Z",
  "labels": [{"name": "ready"}, {"name": "crate:blocked"}]},
 {"state": "OPEN", "number": 4, "title": "no crate",
  "createdAt": "2026-01-06T00:00:00Z",
  "labels": [{"name": "ready"}]},
 {"state": "OPEN", "number": 5, "title": "night", "createdAt": "2026-01-04T00:00:00Z",
  "labels": [{"name": "night"}, {"name": "crate:free"}, {"name": "ready"}]},
 {"state": "OPEN", "number": 6, "title": "one held",
  "createdAt": "2026-01-03T00:00:00Z",
  "labels": [{"name": "ready"}, {"name": "crate:blocked"}, {"name": "crate:built"}]},
 {"state": "OPEN", "number": 7, "title": "not ready",
  "createdAt": "2026-01-01T00:00:00Z", "labels": [{"name": "crate:free"}]},
 {"state": "OPEN", "number": 8, "title": "night owned",
  "createdAt": "2026-01-07T00:00:00Z",
  "labels": [{"name": "night"}, {"name": "owner:d"}, {"name": "blocked"},
   {"name": "crate:other"}]},
 {"state": "OPEN", "number": 9, "title": "on closes",
  "createdAt": "2026-01-08T00:00:00Z",
  "labels": [{"name": "ready"}, {"name": "crate:closes"}]},
 {"state": "OPEN", "number": 1000, "title": "same second, later",
  "createdAt": "2026-01-09T00:00:00Z", "labels": [{"name": "ready"}]},
 {"state": "OPEN", "number": 999, "title": "same second",
  "createdAt": "2026-01-09T00:00:00Z", "labels": [{"name": "ready"}]}
]
JSON
failed=0
check() {
  want_out=$1 want_code=$2 want_err=$3
  shift 3
  PATH="$tmp/bin:$PATH" STUB=$tmp sh "$here/free.sh" "$@" >"$tmp/out" 2>"$tmp/err"
  code=$?
  out=$(cat "$tmp/out") err=$(cat "$tmp/err")
  # $(...) drops a trailing newline, so an empty want also asks for an empty file.
  [ "$out" = "$want_out" ] && [ "$code" = "$want_code" ] && [ "$err" = "$want_err" ] &&
    { [ -n "$want_out" ] || [ ! -s "$tmp/out" ]; } && return
  printf 'free.sh %s: want exit %s and\n%s\n%s\ngot exit %s and\n%s\n%s\n' "$*" \
    "$want_code" "$want_out" "$want_err" "$code" "$out" "$err"
  failed=1
}
check "5 night
3 on blocked
4 no crate
999 same second
1000 same second, later" 0 ""
check "5 night" 0 "" night
check "" 0 "" nothing
touch "$tmp/fail.issue"
check "" 1 "HTTP 502: Bad Gateway"
rm "$tmp/fail.issue"
touch "$tmp/fail.pr"
check "" 1 "HTTP 502: Bad Gateway"
exit "$failed"
