#!/usr/bin/env bash
# Tests scripts/bench-host.sh with stand-ins for `aws`, `ssh`, `curl`, `gh`, and
# `sleep` on PATH. Nothing reaches AWS or GitHub. Needs `jq`, which runs the `--jq`
# filters of the script.
#
# Usage: scripts/bench-host-test.sh

set -euo pipefail

script=$(cd "$(dirname "$0")" && pwd)/bench-host.sh
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
bin=$root/bin
mkdir "$bin"

# Each stand-in logs its call to $T/calls. The environment of a case makes one fail.
cat >"$bin/aws" <<'EOF'
#!/usr/bin/env bash
echo "aws $*" >>"$T/calls"
case "$*" in
*terminate-instances*) [[ -z ${FAIL_TERMINATE:-} ]] || exit 254 ;;
*delete-security-group*)
    [[ -z ${FAIL_GROUP:-} ]] || { echo DependencyViolation >&2; exit 254; } ;;
*create-key-pair*) echo KEY ;;
*create-security-group*) echo sg-1 ;;
*ssm*) echo ami-1 ;;
*run-instances*) echo i-1 ;;
*describe-instances*--instance-ids*) echo 192.0.2.1 ;;
*describe-instances*)
    [[ -z ${FAIL_DESCRIBE:-} ]] || exit 254
    echo "${LEFT:-}" ;;
esac
EOF
cat >"$bin/ssh" <<'EOF'
#!/usr/bin/env bash
echo "ssh ${*: -1}" >>"$T/calls"
case "${*: -1}" in
*loadavg*) echo "0.01 0.02 0.03 1/2 3" ;;
*lscpu*) echo "Xeon, 96 CPUs, 6.8" ;;
*"cargo bench"*)
    echo run >>"$T/runs"
    run=$(wc -l <"$T/runs")
    echo "table $run"
    [[ $run != "${FAIL_RUN:-}" ]] || { echo "bench error" >&2; exit 101; } ;;
esac
[[ $* != *" bash -s "* ]] || cat >/dev/null
EOF
cat >"$bin/curl" <<'EOF'
#!/usr/bin/env bash
echo 198.51.100.7
EOF
cat >"$bin/sleep" <<'EOF'
#!/usr/bin/env bash
EOF
# Comments live in $T/comments.json. Posts get the ids 1001, 1002, and so on.
cat >"$bin/gh" <<'EOF'
#!/usr/bin/env bash
path=$2 body= filter= get=1
shift 2
while [[ $# -gt 0 ]]; do
    case $1 in
    -f) body=${2#body=} get= && shift ;;
    -F) body=$(cat "${2#body=@}") get= && shift ;;
    --jq) filter=$2 && shift ;;
    esac
    shift
done
case $path in
user) echo bench-bot ;;
*/commits/*)
    sha=${path##*/}
    [[ $sha != missing ]] || exit 1
    printf '%s%0*d\n' "$sha" $((40 - ${#sha})) 0 ;;
*/comments)
    if [[ -n $get ]]; then
        jq -r "$filter" "$T/comments.json"
    else
        id=$(($(cat "$T/id" 2>/dev/null || echo 1000) + 1))
        echo "$id" >"$T/id"
        jq --argjson id "$id" --arg body "$body" --arg at "$(date -u +%FT%TZ)" \
            --arg on "${path#repos/*/*/issues/}" \
            '. + [{id: $id, user: {login: "bench-bot"}, created_at: $at,
                body: $body, on: $on}]' \
            "$T/comments.json" >"$T/next" && mv "$T/next" "$T/comments.json"
        echo "$id"
    fi ;;
esac
EOF
chmod +x "$bin"/*

failures=0
today=$(date -u +%F)

# Runs the script in a fresh state. `$1` is the JSON of the comments already on the
# ledger; the rest are the script's arguments.
run() {
    T=$(mktemp -d "$root/case.XXXX")
    export T
    echo "$1" >"$T/comments.json"
    shift
    status=0
    PATH="$bin:$PATH" bash "$script" "$@" >"$T/out" 2>&1 || status=$?
}

# The first lines of the comments posted on issue `$1`.
posted() {
    jq -r --arg on "$1" '.[] | select(.on == ($on + "/comments")) | .body' \
        "$T/comments.json"
}

check() {
    local name=$1
    shift
    if "$@"; then
        echo "ok $name"
    else
        echo "FAIL $name"
        sed 's/^/  /' "$T/out" "$T/calls" 2>/dev/null
        failures=$((failures + 1))
    fi
}

has() { grep -qF -- "$2" <<<"$1"; }

# A ledger comment: id, author, first line, and day (today when not given).
note() {
    printf '{"id": %s, "user": {"login": "%s"}, "created_at": "%sT01:00:00Z",
        "body": "%s", "on": "15/comments"}' "$1" "$2" "${4:-$today}" "$3"
}

args=(1047 box2.red-team delivery aaaa1111 bbbb2222)

run '[]' "${args[@]}" release 'push (pop|peek)'
check "a run posts the cap, the launch, the report, and the end" eval '
    [[ $status == 0 ]] &&
    has "$(posted 15)" "bench-host cap 2.40 USD for #1047, asked by box2.red-team" &&
    has "$(posted 15)" "bench-host launch i-1: c7i.metal-24xl, #1047, asked by \
box2.red-team, cap 2.40 USD, ends by" &&
    has "$(posted 15)" "bench-host end i-1 for #1047: 0.00 h. Terminated: yes. \
Still running from this run: none." &&
    [[ $(posted 1047 | grep -c "^table") == 4 ]]'
check "each filter goes to the host as one argument" eval '
    has "$(cat "$T/calls")" "-- release push\ \(pop\|peek\)"'

for minutes in -30 0 121 08 x; do
    BENCH_MINUTES=$minutes run '[]' "${args[@]}"
    check "BENCH_MINUTES=$minutes is refused before any call" eval '
        [[ $status == 1 && ! -e $T/calls ]] &&
        has "$(cat "$T/out")" "BENCH_MINUTES is from 1 to 120" &&
        [[ -z $(posted 15) ]]'
done
for price in -1 0 1e3; do
    BENCH_PRICE_MAX=$price run '[]' "${args[@]}"
    check "BENCH_PRICE_MAX=$price is refused" eval '
        [[ $status == 1 && -z $(posted 15) ]]'
done

BENCH_DAY_CAP=100 run "[$(note 1 bench-bot "bench-host cap 12.61 USD for #1")]" \
    "${args[@]}"
check "a launch over 15 USD of caps today is withdrawn" eval '
    [[ $status == 1 ]] && has "$(cat "$T/out")" "15.01 USD of caps today" &&
    has "$(posted 15)" "bench-host withdrawn 1001: no host launched." &&
    [[ ! -e $T/calls ]]'

run "[$(note 1 someone "bench-host cap 50.00 USD"),
    $(note 2 bench-bot "bench-host cap -100.00 USD"),
    $(note 3 bench-bot "bench-host cap 13.00 USD")]" "${args[@]}"
check "a cap of another account or not a price does not count" eval '
    [[ $status == 1 ]] && has "$(cat "$T/out")" "15.40 USD of caps today"'

run "[$(note 1 bench-bot "bench-host cap 13.00 USD"),
    $(note 2 bench-bot "bench-host withdrawn 1: no host launched."),
    $(note 3 bench-bot "bench-host cap 13.00 USD" 2000-01-01)]" "${args[@]}"
check "a withdrawn cap and a cap of another day do not count" eval '[[ $status == 0 ]]'

run "[$(note 9000 bench-bot "bench-host cap 13.00 USD")]" "${args[@]}"
check "a cap posted after this one does not count" eval '[[ $status == 0 ]]'

FAIL_TERMINATE=1 run '[]' "${args[@]}"
check "an unconfirmed termination exits 1 and says so" eval '
    [[ $status == 1 ]] &&
    has "$(posted 15)" "Terminated: unconfirmed. Still running from this run: none."'

FAIL_DESCRIBE=1 run '[]' "${args[@]}"
check "an unknown count of hosts exits 1 and says so" eval '
    [[ $status == 1 ]] &&
    has "$(posted 15)" "Terminated: yes. Still running from this run: unknown."'

LEFT=i-9 run '[]' "${args[@]}"
check "another host of this run ends, and the run exits 1" eval '
    [[ $status == 1 ]] &&
    has "$(cat "$T/calls")" "terminate-instances --instance-ids i-9" &&
    has "$(posted 15)" "Still running from this run: i-9."'

FAIL_GROUP=1 run '[]' "${args[@]}"
check "a security group left exits 1 and is named" eval '
    [[ $status == 1 ]] && has "$(posted 15)" "Security group sg-1 is left." &&
    [[ $(grep -c delete-security-group "$T/calls") == 24 ]]'

FAIL_RUN=2 run '[]' "${args[@]}"
check "a failed run posts the report up to it and exits 1" eval '
    [[ $status == 1 ]] && has "$(posted 1047)" "Run 2: head bbbb2222, failed, load" &&
    has "$(posted 1047)" "bench error" && ! has "$(posted 1047)" "Run 3" &&
    has "$(cat "$T/out")" "run 2 failed" &&
    has "$(posted 15)" "Terminated: yes. Still running from this run: none."'

run '[]' 1047 box2.red-team delivery missing bbbb2222
check "a commit not on GitHub is refused before any post" eval '
    [[ $status == 1 && -z $(posted 15) ]] &&
    has "$(cat "$T/out")" "no commit missing on GitHub"'

run '[]' 1047 box2.red-team 'delivery; reboot' aaaa1111 bbbb2222
check "a crate that is not a package name is refused" eval '
    [[ $status == 1 && -z $(posted 15) ]]'

[[ $failures == 0 ]] || { echo "$failures failed"; exit 1; }
echo "all passed"
