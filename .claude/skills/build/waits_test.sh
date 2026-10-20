#!/bin/sh
# Runs waits.sh against fixed lists of PRs and issues, through the stub `gh` of
# free_test.sh. Needs `jq`. Exit 1 on a failure.
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
jq -n '[range(30) | {state: "OPEN", number: (100 + .)}]' > "$tmp/fill.pr.json"
jq -n '[range(30) | {state: "OPEN", number: (200 + .), title: "fill",
  body: "Waits on #40.", labels: []}]' > "$tmp/fill.issue.json"
jq -s '.[0] + .[1]' "$tmp/fill.pr.json" - > "$tmp/pr.json" <<'JSON'
[{"state": "OPEN", "number": 50}, {"state": "CLOSED", "number": 51}]
JSON
jq -s '.[0] + .[1]' "$tmp/fill.issue.json" - > "$tmp/issue.json" <<'JSON'
[
 {"state": "OPEN", "number": 1000, "title": "after 8", "body": "Waits on #40.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 1, "title": "a\\b", "body": "Goal.\n\nWaits on #40.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 2, "title": "one open", "body": "Waits on #40, #41.",
  "labels": [{"name": "blocked"}, {"name": "owner:a"}]},
 {"state": "OPEN", "number": 3, "title": "open pr", "body": "Waits on #50.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 4, "title": "closed pr", "body": "Waits on #51.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 5, "title": "not blocked", "body": "Waits on #40.",
  "labels": [{"name": "crate:x"}]},
 {"state": "OPEN", "number": 6, "title": "free text",
  "body": "Waits on #40, which builds x.", "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 7, "title": "no line", "body": "See #40.",
  "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 8, "title": "crlf",
  "body": "Goal.\r\nWaits on #40 and #42.\r\n",
  "labels": [{"name": "crate:x"}, {"name": "owner:b"}, {"name": "blocked"}]},
 {"state": "OPEN", "number": 10, "title": "in a sentence",
  "body": "It no longer Waits on #40.", "labels": [{"name": "blocked"}]},
 {"state": "OPEN", "number": 9, "title": "two lines",
  "body": "Waits on #40.\nWaits on #41.", "labels": [{"name": "blocked"}]},
 {"state": "CLOSED", "number": 40, "title": "closed", "body": "", "labels": []},
 {"state": "OPEN", "number": 41, "title": "open", "body": "", "labels": []},
 {"state": "CLOSED", "number": 42, "title": "closed", "body": "", "labels": []}
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
check '1 - a\b
4 - closed pr
8 b crlf
1000 - after 8' 0 "" all
touch "$tmp/fail.issue"
check "" 1 "HTTP 502: Bad Gateway" "issue call fails"
rm "$tmp/fail.issue"
touch "$tmp/fail.pr"
check "" 1 "HTTP 502: Bad Gateway" "pr call fails"
rm "$tmp/fail.pr"
cp "$tmp/fill.issue.json" "$tmp/issue.json"
check "" 0 "" nothing
exit "$failed"
