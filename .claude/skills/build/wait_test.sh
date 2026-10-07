#!/bin/sh
# Runs wait.sh against fixed GraphQL answers, then once against the API on merged #1428
# to check the query. A stub `gh` acts as gh does: on an answer with an error message,
# an HTTP error, or no network, it exits 1, prints the body, if any, and prints the
# error on stderr; with no login, it exits 4. After the answers run out, it fails with
# "out of answers". `gh api rate_limit` gives the count in `$STUB/left.<n>` after call
# <n>, else in `$STUB/left`, else 1. As gh does, it logs each request on stderr for a
# true `GH_DEBUG`, or, when that is not set, for a true `DEBUG`. A stub `sleep` returns
# at once, and stops the script on its fifth call; when `$STUB/slow` exists, it sleeps.
# Needs `jq`. Exit 1 on a failure.
set -u
here=$(cd "$(dirname "$0")" && pwd)
gh=$(command -v gh)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/bin" "$tmp/live"
cat > "$tmp/bin/gh" <<'STUB'
#!/bin/sh
case ${GH_DEBUG-unset} in
  unset) case ${DEBUG-} in 1 | true | yes | api) log=1 ;; *) log= ;; esac ;;
  "" | 0 | false | no) log= ;;
  *) log=1 ;;
esac
[ -z "$log" ] || echo "* Request to https://api.github.com/graphql" >&2
n=$(cat "$STUB/calls")
[ "$2" != rate_limit ] ||
  { cat "$STUB/left.$n" 2>/dev/null || cat "$STUB/left" 2>/dev/null || echo 1; exit 0; }
n=$((n + 1))
echo "$n" > "$STUB/calls"
a=$STUB/$n.json
[ -f "$a" ] || { echo "gh: out of answers" >&2; exit 1; }
case $(cat "$a") in
  offline) echo "error connecting to api.github.com" >&2; exit 1 ;;
  "no login") echo "To get started with GitHub CLI, please run:  gh auth login" >&2
    exit 4 ;;
  "http "*) cut -c6- "$a"; echo "gh: HTTP 502 (HTTP 502)" >&2; exit 1 ;;
esac
# gh fails only on an error with a message.
if jq -e '[.errors[]?.message | select(. != null and . != "")] | length > 0' \
  "$a" > /dev/null 2>&1; then
  cat "$a"
  jq -r '"gh: " + .errors[0].message' "$a" >&2
  exit 1
fi
while [ "$1" != --jq ]; do shift; done
jq -r "$2" "$a"
STUB
cat > "$tmp/bin/sleep" <<'STUB'
#!/bin/sh
[ ! -f "$STUB/slow" ] || exec /bin/sleep 10
n=$(($(cat "$STUB/sleeps" 2>/dev/null || echo 0) + 1))
echo "$n" > "$STUB/sleeps"
[ "$n" -lt 5 ] || kill "$PPID"
STUB
# Records the answer body, then runs the real gh. $6 is `query=...`. The real gh can
# itself call gh from PATH, so it gets the PATH without this stub.
cat > "$tmp/live/gh" <<STUB
#!/bin/sh
PATH='$PATH'
"$gh" api graphql -F n=1428 -f "\$6" < /dev/null > "$tmp/live/1428.json"
exec "$gh" "\$@"
STUB
cp "$tmp/bin/sleep" "$tmp/live/sleep"
chmod +x "$tmp/bin/gh" "$tmp/bin/sleep" "$tmp/live/gh" "$tmp/live/sleep"
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

# answers <answer>...: the stub gives the answers in order.
answers() {
  rm -f "$tmp"/*.json "$tmp"/left* "$tmp/sleeps" "$tmp/slow"
  i=0
  for a in "$@"; do
    i=$((i + 1))
    echo "$a" > "$tmp/$i.json"
  done
  echo 0 > "$tmp/calls"
}

# run <name> <exit> <output> <polls> <answer>...: wait.sh gets the answers in order,
# and must stop after <polls> of them with <exit> and <output>. `$left` sets the
# rate limit, `$spent` the call after which it is 0, and `$gh_debug` and `$debug`
# set GH_DEBUG and DEBUG, which are else unset.
run() {
  name=$1 code=$2 out=$3 polls=$4
  shift 4
  answers "$@"
  [ -z "${left-}" ] || echo "$left" > "$tmp/left"
  [ -z "${spent-}" ] || echo 0 > "$tmp/left.$spent"
  got=$(
    unset GH_DEBUG DEBUG
    [ -z "${gh_debug-}" ] || export GH_DEBUG="$gh_debug"
    [ -z "${debug-}" ] || export DEBUG="$debug"
    STUB=$tmp PATH="$tmp/bin:$PATH" sh "$here/wait.sh" 7
  )
  check "$name" $? "$got" "$(cat "$tmp/calls")" "$code" "$out" "$polls"
  unset left spent gh_debug debug
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
run "not JSON" 0 "#7 merged" 2 "http <html>" "$merged"
run "HTTP error" 0 "#7 merged" 2 'http {"message":"Bad credentials"}' "$merged"
run "rate limited" 0 "#7 merged" 3 \
  'http {"message":"You have exceeded a secondary rate limit."}' \
  '{"errors":[{"type":"RATE_LIMITED","message":"limit"}]}' "$merged"
run "server timeout" 0 "#7 merged" 2 \
  '{"data":null,"errors":[{"message":"Something went wrong."}]}' "$merged"
stop="#7 cannot be read, gh failed 3 times:"
run "three failed calls" 1 "$stop error connecting to api.github.com" 3 \
  offline offline offline
run "an answer resets the failures" 0 "#7 merged" 5 \
  offline offline "$(pr OPEN)" offline "$merged"
run "no login" 1 "$stop To get started with GitHub CLI, please run:  gh auth login" \
  3 "no login" "no login" "no login"
e='{"errors":[{"path":["query","nope"],"extensions":{"code":"undefinedField"},
  "message":"Field '"'nope'"' does not exist on type '"'Query'"'"}]}'
run "wrong query" 1 "$stop $e
gh: Field 'nope' does not exist on type 'Query'" 3 \
  "$e" "$e" "$e"
e='{"data":{"repository":{"pullRequest":null}},"errors":[{"type":"NOT_FOUND",
  "path":["repository","pullRequest"],
  "message":"Could not resolve to a PullRequest with the number of 7."}]}'
run "no such PR" 1 \
  "$stop $e
gh: Could not resolve to a PullRequest with the number of 7." 3 \
  "$e" "$e" "$e"
run "error with no message" 1 "#7 closed" 1 \
  "$(pr CLOSED | jq -c '. + {errors: [{type: "NOT_FOUND"}]}')"
for n in abc "" 7x; do
  echo 0 > "$tmp/calls"
  got=$(STUB=$tmp PATH="$tmp/bin:$PATH" sh "$here/wait.sh" "$n" 2> "$tmp/err")
  check "PR number '$n'" $? "$got|$(cat "$tmp/err")" "$(cat "$tmp/calls")" 2 \
    "|usage: wait.sh <PR number>" 0
done
e='{"errors":[{"type":"RATE_LIMITED","message":"API rate limit exceeded."}]}'
left=0 run "spent rate limit" 0 "#7 merged" 4 "$e" "$e" "$e" "$merged"
left=1 run "rate limit error with calls left" 1 \
  "$stop $e
gh: API rate limit exceeded." 3 "$e" "$e" "$e"
spent=3 run "a spent rate limit keeps the failures" 1 \
  "$stop error connecting to api.github.com" 4 offline offline "$e" offline
gh_debug=1 run "GH_DEBUG" 0 "#7 merged" 1 "$merged"
debug=1 run "DEBUG" 0 "#7 merged" 1 "$merged"
gh_debug=0 debug=1 run "GH_DEBUG=0 with DEBUG" 0 "#7 merged" 1 "$merged"
answers "$(pr OPEN)"
touch "$tmp/slow"
# The shell reports a job that a signal ended on stderr.
got=$(
  STUB=$tmp PATH="$tmp/bin:$PATH" sh "$here/wait.sh" 7 > /dev/null &
  /bin/sleep 1
  kill "$!"
  start=$(date +%s)
  wait "$!"
  echo "$? $(($(date +%s) - start))"
) 2> /dev/null
[ "${got#* }" -gt 2 ] || fast=yes
check "TERM stops it at once" "${got% *}" "${fast-no}" "$(cat "$tmp/calls")" 143 yes 1
run "waits at most five times" 143 "" 5 "$(pr OPEN)" "$(pr OPEN)" "$(pr OPEN)" \
  "$(pr OPEN)" "$(pr OPEN)"

got=$(STUB=$tmp/live PATH="$tmp/live:$PATH" sh "$here/wait.sh" 1428)
check "API on #1428" $? "$got" 1 0 "#1428 merged" 1
# The API's answer for #1428, put back in the queue: its last `gate` run was canceled,
# which makes GitHub give FAILURE as its rollup state.
replay=$(jq -c '.data.repository.pullRequest += {state: "OPEN", isInMergeQueue: true}' \
  "$tmp/live/1428.json")
run "#1428 in the queue waits" 0 "#7 merged" 2 "$replay" "$merged"
# The case above holds only while the last Review run of #1428 is a canceled `gate`.
premise=$(jq '.data.repository.pullRequest.commits.nodes[0].commit.statusCheckRollup
  .contexts.nodes | map(select(.checkSuite.workflowRun.workflow.name? == "Review"))
  | max_by(.checkSuite.workflowRun.databaseId)
  | .name == "gate" and .conclusion == "CANCELLED" and .isRequired == false' \
  "$tmp/live/1428.json")
check "#1428 ends with a canceled gate" 0 "$premise" 1 0 true 1
# The jq reads each of these fields, and a fixture can give one that the query lost.
fields=$(jq '.data.repository.pullRequest | . as $pr
  | all("mergeable", "reviewDecision", "isInMergeQueue", "autoMergeRequest";
    . as $f | $pr | has($f))
  and (.commits.nodes[0].commit.statusCheckRollup.contexts.nodes
  | any(.[]; .__typename == "CheckRun")
  and all(.[]; .__typename != "CheckRun" or ((.name | type) == "string"
    and has("conclusion") and (.databaseId | type) == "number"
    and (.checkSuite.workflowRun.databaseId | type) == "number"
    and (.checkSuite.workflowRun.workflow.name | type) == "string"
    and (.isRequired | type) == "boolean"))
  and all(.[]; .__typename != "StatusContext" or (.state | type) == "string"))' \
  "$tmp/live/1428.json")
check "API on #1428 gives each field" 0 "$fields" 1 0 true 1
exit $failed
