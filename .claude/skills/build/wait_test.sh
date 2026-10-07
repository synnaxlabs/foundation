#!/bin/sh
# Runs wait.sh against fixed GraphQL answers, then once against the API on merged
# #1428 to check the query. A stub `gh` prints the answers in order, then an error
# "out of answers". A stub `sleep` returns at once. Exit 1 on a failure.
set -u
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/bin" "$tmp/live"
cat > "$tmp/bin/gh" <<'EOF'
#!/bin/sh
n=$(($(cat "$STUB/calls") + 1))
echo "$n" > "$STUB/calls"
[ -f "$STUB/$n.json" ] || { echo '{"errors":[{"message":"out of answers"}]}'; exit 1; }
[ "$(cat "$STUB/$n.json")" = offline ] && exit 1
cat "$STUB/$n.json"
EOF
printf '#!/bin/sh\n' > "$tmp/bin/sleep"
cp "$tmp/bin/sleep" "$tmp/live/sleep"
chmod +x "$tmp/bin/gh" "$tmp/bin/sleep" "$tmp/live/sleep"
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

# with <object>: merges <object> into the pull request of the answer on stdin.
with() {
  jq -c ".data.repository.pullRequest += $1"
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
  check "$name" $? "$got" "$(cat "$tmp/calls")" "$code" "$out" "$polls"
}

# check <name> <exit> <output> <polls> <expected exit> <output> <polls>
check() {
  if [ "$2" != "$5" ] || [ "$3" != "$6" ] || [ "$4" != "$7" ]; then
    echo "FAIL $1: exit $2, output '$3', polls $4"
    failed=1
  else
    echo "ok $1"
  fi
}

# job <workflow> <workflow run> <name> <databaseId> <conclusion or null> <required>
job() {
  printf '{"__typename":"CheckRun","name":"%s","databaseId":%s,"conclusion":%s,
    "isRequired":%s,"checkSuite":{"workflowRun":{"databaseId":%s,
    "workflow":{"name":"%s"}}}}' "$3" "$4" "$5" "$6" "$2" "$1"
}
status() {
  printf '{"__typename":"StatusContext","state":"%s"}' "$1"
}
review=$(status SUCCESS)
merged=$(pr MERGED)

run merged 0 "#7 merged" 1 "$merged"
run closed 1 "#7 closed" 1 "$(pr CLOSED)"
run "canceled gate waits" 0 "#7 merged" 2 \
  "$(pr OPEN "$(job Review 1 gate 1 '"SUCCESS"' false)" \
    "$(job Review 2 gate 2 '"CANCELLED"' false)" \
    "$(job CI 3 test 3 '"SUCCESS"' true)" "$review")" \
  "$merged"
run "pending checks wait" 0 "#7 merged" 2 \
  "$(pr OPEN "$(job CI 1 test 3 null true)" "$(status PENDING)")" "$merged"
run "skipped check waits" 0 "#7 merged" 2 \
  "$(pr OPEN "$(job CI 1 deny 3 '"SKIPPED"' true)" "$review")" "$merged"
run "no checks yet" 0 "#7 merged" 2 \
  "$(pr OPEN | with '{"commits":{"nodes":[{"commit":{"statusCheckRollup":null}}]}}')" \
  "$merged"
for c in FAILURE TIMED_OUT STARTUP_FAILURE ACTION_REQUIRED; do
  run "$c check" 1 "#7 has a failed check" 1 \
    "$(pr OPEN "$(job CI 1 test 3 "\"$c\"" true)" "$review")"
done
run "failed check that is not required" 1 "#7 has a failed check" 1 \
  "$(pr OPEN "$(job Review 1 gate 3 '"FAILURE"' false)" "$review")"
for c in FAILURE ERROR; do
  run "$c status" 1 "#7 has a failed check" 1 "$(pr OPEN "$(status $c)")"
done
run "rerun passed" 0 "#7 merged" 2 \
  "$(pr OPEN "$(job CI 1 test 3 '"FAILURE"' true)" \
    "$(job CI 1 test 4 '"SUCCESS"' true)" "$review")" \
  "$merged"
run "rerun failed" 1 "#7 has a failed check" 1 \
  "$(pr OPEN "$(job CI 1 test 4 '"FAILURE"' true)" \
    "$(job CI 1 test 3 '"SUCCESS"' true)" "$review")"
run "two jobs of one run" 1 "#7 has a failed check" 1 \
  "$(pr OPEN "$(job CI 1 test 3 '"FAILURE"' true)" \
    "$(job CI 1 check 4 '"SUCCESS"' true)" "$review")"
run "same job name in two workflows" 1 "#7 has a failed check" 1 \
  "$(pr OPEN "$(job CI 1 changes 3 '"FAILURE"' false)" \
    "$(job Arm 2 changes 4 '"SUCCESS"' false)" "$review")"
run "canceled required check" 1 "#7 has a canceled required check" 1 \
  "$(pr OPEN "$(job CI 1 test 3 '"CANCELLED"' true)" "$review")"
run "canceled required check that a rerun passed" 0 "#7 merged" 2 \
  "$(pr OPEN "$(job CI 1 test 3 '"CANCELLED"' true)" \
    "$(job CI 1 test 4 '"SUCCESS"' true)" "$review")" \
  "$merged"
run "new run replaces a canceled run" 0 "#7 merged" 2 \
  "$(pr OPEN "$(job CI 1 changes 1 '"CANCELLED"' false)" \
    "$(job CI 1 check 2 '"CANCELLED"' true)" \
    "$(job CI 5 changes 5 null false)" "$review")" \
  "$merged"
run conflict 1 "#7 conflicts with main" 1 \
  "$(pr OPEN | with '{"mergeable":"CONFLICTING"}')"
run "requested changes" 1 "#7 has requested changes" 1 \
  "$(pr OPEN | with '{"reviewDecision":"CHANGES_REQUESTED"}')"
run "auto-merge waits" 0 "#7 merged" 2 \
  "$(pr OPEN | with '{"isInMergeQueue":false,"autoMergeRequest":{"enabledAt":"x"}}')" \
  "$merged"
run "left the merge queue" 1 "#7 left the merge queue" 1 \
  "$(pr OPEN | with '{"isInMergeQueue":false}')"
run offline 0 "#7 merged" 2 offline "$merged"
run "not JSON" 0 "#7 merged" 2 "<html>" "$merged"
run "rate limited" 0 "#7 merged" 2 \
  '{"errors":[{"type":"RATE_LIMITED","message":"limit"}]}' "$merged"
run "query error" 1 "#7 has a query error: no field" 1 \
  '{"errors":[{"message":"no field"}]}'

got=$(PATH="$tmp/live:$PATH" sh "$here/wait.sh" 1428)
check "API on #1428" $? "$got" 1 0 "#1428 merged" 1
exit $failed
