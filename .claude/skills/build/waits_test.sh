#!/bin/sh
# Runs waits.sh against a fixed list of issues and fixed states of the issues and PRs
# that they name. A stub `gh` acts as gh does: `issue list` keeps the open issues, or
# all for `--state all`, with the `--label`, cuts the list to `--limit` (30 when not
# given), and applies `--jq`. `api graphql` answers each `issueOrPullRequest` of the
# query from `$STUB/items.json`, and fails as gh does on a number that is not there.
# When `$STUB/fail.<issue|api>` exists, it prints an error and exits 1. Needs `jq`.
# Exit 1 on a failure.
set -u
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/bin"
cat > "$tmp/bin/gh" <<'STUB'
#!/bin/sh
kind=$1 state=open limit=30 label= query= expr=.
while [ "$#" -gt 0 ]; do
  case $1 in
    --state) state=$2 ;;
    --limit) limit=$2 ;;
    --label) label=$2 ;;
    -f) query=${2#query=} ;;
    --jq) expr=$2 ;;
  esac
  shift
done
[ ! -f "$STUB/fail.$kind" ] || { echo "HTTP 502: Bad Gateway" >&2; exit 1; }
if [ "$kind" = api ]; then
  nums=$(printf '%s' "$query" | grep -o 'number: [0-9]*' | cut -c9-)
  for n in $nums; do
    jq -e --arg n "$n" 'has($n)' "$STUB/items.json" >/dev/null && continue
    echo "gh: Could not resolve to an issue or pull request with the number of $n." >&2
    exit 1
  done
  exec jq -r --arg nums "$nums" '. as $i | {data: {repository: (
    {__typename: "Repository"} + ([$nums | splits("\n") | select(. != "")
      | {key: "n\(.)", value: $i[.]}] | from_entries))}} | '"$expr" "$STUB/items.json"
fi
jq -r --arg s "$state" --arg b "$label" --argjson l "$limit" \
  "[.[] | select(\$s == \"all\" or .state == \"OPEN\")
  | select(\$b == \"\" or any(.labels[]; .name == \$b)) | del(.state)] | .[:\$l]
  | $expr" "$STUB/issue.json"
STUB
chmod +x "$tmp/bin/gh"
# 40 and 42 are done issues, 41 an open one, 43 one not planned, 44 a duplicate, 50 an
# open PR, 51 a PR closed with no merge, and 52 a merged PR.
cat > "$tmp/items.json" <<'JSON'
{"40": {"state": "CLOSED", "stateReason": "COMPLETED"},
 "41": {"state": "OPEN", "stateReason": null},
 "42": {"state": "CLOSED", "stateReason": "COMPLETED"},
 "43": {"state": "CLOSED", "stateReason": "NOT_PLANNED"},
 "44": {"state": "CLOSED", "stateReason": "DUPLICATE"},
 "50": {"state": "OPEN"}, "51": {"state": "CLOSED"}, "52": {"state": "MERGED"}}
JSON
# 30 blocked issues before the ones that count, so a list cut to 30 misses them.
jq -n '[range(30) | {state: "OPEN", number: (200 + .), body: "Waits on #41.",
  labels: [{name: "blocked"}]}]' > "$tmp/fill.json"
jq -s '.[0] + .[1]' "$tmp/fill.json" - > "$tmp/issue.json" <<'JSON'
[
 {"state": "OPEN", "number": 1000, "body": "Waits on #40.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 1, "body": "Goal.\n\nWaits on #40.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 2, "body": "Waits on #40, #41.",
  "labels": [{"name": "blocked"}, {"name": "owner:a"}]},
 {"state": "OPEN", "number": 3, "body": "Waits on #50.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 4, "body": "Waits on #51.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 5, "body": "Waits on #40.",
  "labels": [{"name": "crate:x"}]},
 {"state": "OPEN", "number": 6, "body": "Waits on #40, which builds x.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 7, "body": "See #40.", "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 8, "body": "Goal.\r\nWaits on #40 and #42.\r\n",
  "labels": [{"name": "crate:x"}, {"name": "owner:b"}, {"name": "blocked"}]},
 {"state": "OPEN", "number": 10, "body": "It no longer Waits on #40.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 9, "body": "Waits on #40.\nWaits on #41.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 11, "body": "Waits on #40, and the person.",
  "labels": [{"name": "blocked"}, {"name": "owner:c"}]},
 {"state": "OPEN", "number": 12, "body": "Waits on the person.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 13, "body": "Waits on #40.\nWaits on the person.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 14, "body": "Waits on #52.",
  "labels": [{"name": "blocked"}, {"name": "owner:d"}]},
 {"state": "OPEN", "number": 15, "body": "Waits on #43.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 16, "body": "Waits on #43, #41.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 17, "body": "Waits on #40.\nWaits on #44.",
  "labels": [{"name": "blocked"}, {"name": "owner:e"}]},
 {"state": "CLOSED", "number": 18, "body": "Waits on #40.",
  "labels": [{"name": "blocked"}]}
]
JSON
failed=0
check() {
  want_out=$1 want_code=$2 want_err=$3 case=$4
  PATH="$tmp/bin:$PATH" STUB=$tmp sh "$here/waits.sh" >"$tmp/out" 2>"$tmp/err"
  code=$?
  out=$(cat "$tmp/out") err=$(cat "$tmp/err")
  # $(...) drops a trailing newline, so an empty want also asks for an empty file.
  [ "$out" = "$want_out" ] && [ "$code" = "$want_code" ] && [ "$err" = "$want_err" ] &&
    { [ -n "$want_out" ] || [ ! -s "$tmp/out" ]; } && return
  printf 'waits.sh, %s: want exit %s and\n%s\n%s\ngot exit %s and\n%s\n%s\n' "$case" \
    "$want_code" "$want_out" "$want_err" "$code" "$out" "$err"
  failed=1
}
check '1 - ended
4 - dropped
6 - no line
7 - no line
8 b ended
10 - no line
14 d ended
15 - dropped
17 e dropped
1000 - ended' 0 "" all
touch "$tmp/fail.issue"
check "" 1 "HTTP 502: Bad Gateway" "issue call fails"
rm "$tmp/fail.issue"
touch "$tmp/fail.api"
check "" 1 "HTTP 502: Bad Gateway" "graphql call fails"
rm "$tmp/fail.api"
cp "$tmp/issue.json" "$tmp/all.json"
jq '.[0].body = "Waits on #40, #99."' "$tmp/all.json" > "$tmp/issue.json"
check "" 1 "gh: Could not resolve to an issue or pull request with the number of 99." \
  "unknown number"
jq '[.[] | select(.number == 7 or .number == 12)]' "$tmp/all.json" > "$tmp/issue.json"
check "7 - no line" 0 "" "no number"
cp "$tmp/fill.json" "$tmp/issue.json"
check "" 0 "" nothing
exit "$failed"
