#!/bin/sh
# Runs wait.sh against fixed GraphQL answers. A stub `gh` gives the answers in order
# and applies the script's jq, then stops the script with "out of answers". A stub
# `sleep` returns at once. Exit 1 on a failure.
set -u
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/bin"
cat > "$tmp/bin/gh" <<'EOF'
#!/bin/sh
n=$(($(cat "$STUB/calls") + 1))
echo "$n" > "$STUB/calls"
[ -f "$STUB/$n.json" ] || { echo "out of answers"; exit 0; }
while [ "$1" != --jq ]; do shift; done
jq -r "$2" "$STUB/$n.json"
EOF
printf '#!/bin/sh\n' > "$tmp/bin/sleep"
chmod +x "$tmp/bin/gh" "$tmp/bin/sleep"
failed=0

# pr <state> <context>...: a GraphQL answer for a PR in the merge queue.
pr() {
  state=$1
  shift
  nodes=
  for c in "$@"; do nodes=${nodes:+$nodes,}$c; done
  printf '{"data":{"repository":{"pullRequest":{"state":"%s","mergeable":"MERGEABLE",
    "reviewDecision":null,"isInMergeQueue":true,"autoMergeRequest":null,
    "commits":{"nodes":[{"commit":{"statusCheckRollup":{"contexts":{"nodes":[%s]}}}}]}
    }}}}' "$state" "$nodes"
}

# run <name> <exit> <output> <polls> <answer>...: wait.sh gets the answers in order,
# and must stop after <polls> of them with <exit> and <output>.
run() {
  name=$1 code=$2 out=$3 polls=$4
  shift 4
  rm -f "$tmp"/*.json
  i=0
  for a in "$@"; do
    i=$((i + 1))
    echo "$a" > "$tmp/$i.json"
  done
  echo 0 > "$tmp/calls"
  got=$(STUB=$tmp PATH="$tmp/bin:$PATH" sh "$here/wait.sh" 7)
  c=$?
  p=$(cat "$tmp/calls")
  if [ "$c" != "$code" ] || [ "$got" != "$out" ] || [ "$p" != "$polls" ]; then
    echo "FAIL $name: exit $c, output '$got', polls $p"
    failed=1
  else
    echo "ok $name"
  fi
}

# job <workflow> <name> <databaseId> <conclusion or null> <required>
job() {
  printf '{"__typename":"CheckRun","name":"%s","databaseId":%s,"conclusion":%s,
    "isRequired":%s,"checkSuite":{"workflowRun":{"workflow":{"name":"%s"}}}}' \
    "$2" "$3" "$4" "$5" "$1"
}
review='{"__typename":"StatusContext","context":"review","state":"SUCCESS",
  "isRequired":true}'
pending='{"__typename":"StatusContext","context":"review","state":"PENDING",
  "isRequired":true}'

run merged 0 "#7 merged" 1 "$(pr MERGED)"
run "canceled gate waits" 0 "#7 merged" 2 \
  "$(pr OPEN "$(job Review gate 1 '"SUCCESS"' false)" \
    "$(job Review gate 2 '"CANCELLED"' false)" \
    "$(job CI test 3 '"SUCCESS"' true)" "$review")" \
  "$(pr MERGED)"
run "pending checks wait" 0 "#7 merged" 2 \
  "$(pr OPEN "$(job CI test 3 null true)" "$pending")" "$(pr MERGED)"
run "skipped check waits" 0 "#7 merged" 2 \
  "$(pr OPEN "$(job CI deny 3 '"SKIPPED"' true)" "$review")" "$(pr MERGED)"
run "failed check" 1 "#7 has a failed check" 1 \
  "$(pr OPEN "$(job CI test 3 '"FAILURE"' true)" "$review")"
run "failed check that is not required" 1 "#7 has a failed check" 1 \
  "$(pr OPEN "$(job Review gate 3 '"FAILURE"' false)" "$review")"
run "timed out check" 1 "#7 has a failed check" 1 \
  "$(pr OPEN "$(job CI mutants 3 '"TIMED_OUT"' true)" "$review")"
run "failed status" 1 "#7 has a failed check" 1 \
  "$(pr OPEN '{"__typename":"StatusContext","context":"review","state":"FAILURE",
    "isRequired":true}')"
run "rerun passed" 0 "#7 merged" 2 \
  "$(pr OPEN "$(job CI test 3 '"FAILURE"' true)" \
    "$(job CI test 4 '"SUCCESS"' true)" "$review")" \
  "$(pr MERGED)"
run "rerun failed" 1 "#7 has a failed check" 1 \
  "$(pr OPEN "$(job CI test 4 '"FAILURE"' true)" \
    "$(job CI test 3 '"SUCCESS"' true)" "$review")"
run "same job name in two workflows" 1 "#7 has a failed check" 1 \
  "$(pr OPEN "$(job CI changes 3 '"FAILURE"' false)" \
    "$(job Arm changes 4 '"SUCCESS"' false)" "$review")"
run "canceled required check" 1 "#7 has a canceled required check" 1 \
  "$(pr OPEN "$(job CI test 3 '"CANCELLED"' true)" "$review")"
run "canceled required check that a rerun passed" 0 "#7 merged" 2 \
  "$(pr OPEN "$(job CI test 3 '"CANCELLED"' true)" \
    "$(job CI test 4 '"SUCCESS"' true)" "$review")" \
  "$(pr MERGED)"
exit $failed
