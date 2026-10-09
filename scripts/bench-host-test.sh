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
*delete-key-pair*) [[ -z ${FAIL_KEY:-} ]] || { echo InvalidKeyPair >&2; exit 254; } ;;
*run-instances*) [[ -z ${FAIL_LAUNCH:-} ]] || exit 254; echo i-1 ;;
*describe-instances*--instance-ids*) echo 192.0.2.1 ;;
*describe-instances*)
    [[ -z ${FAIL_DESCRIBE:-} ]] || exit 254
    echo "${LEFT:-}" ;;
esac
EOF
# It logs the remote command, and runs it in $T/home with the stand-ins of the host,
# except the reads of `loadavg` and `lscpu`.
cat >"$bin/ssh" <<'EOF'
#!/usr/bin/env bash
while (($#)) && [[ $1 != ubuntu@* ]]; do shift; done
(($#)) || { echo "ssh: no ubuntu@ host" >&2; exit 255; }
shift
echo "ssh $*" >>"$T/calls"
case "$*" in
*loadavg*) echo "0.01 0.02 0.03 1/2 3" ;;
*lscpu*) echo "Xeon, 96 CPUs, 6.8" ;;
*)
    mkdir -p "$T/home/.cargo"
    [[ -e $T/home/.cargo/bin ]] || ln -s "$REMOTE" "$T/home/.cargo/bin"
    # Like `sshd`, it passes only `T` and the switches of a case.
    cd "$T/home" && env -i T="$T" BIG="${BIG:-}" EDGE="${EDGE:-}" LONG="${LONG:-}" \
        FAIL_RUN="${FAIL_RUN:-}" HOME="$T/home" PATH="$REMOTE:/usr/bin:/bin" \
        bash -c "$*" ;;
esac
EOF
export REMOTE=$root/remote
mkdir "$REMOTE"
# The stand-ins of the host. `cargo` logs the `RUSTFLAGS` it gets.
cat >"$REMOTE/cargo" <<'EOF'
#!/usr/bin/env bash
echo "$RUSTFLAGS" >>"$T/rustflags"
[[ $* != *--no-run* ]] || exit 0
echo run >>"$T/runs"
run=$(($(wc -l <"$T/runs")))
[[ -z ${BIG:-} ]] || { head -c 70000 /dev/zero | tr '\0' x && echo; }
[[ -z ${EDGE:-} ]] || { echo ab && echo "table $run" && yes 1234567 | head -1873; }
[[ -z ${LONG:-} ]] || head -c 20000 /dev/zero | tr '\0' x
echo "table $run"
[[ $run != "${FAIL_RUN:-}" ]] || { echo "bench error" >&2; exit 101; }
EOF
cat >"$REMOTE/git" <<'EOF'
#!/usr/bin/env bash
[[ $1 != init ]] || mkdir -p "${!#}"
EOF
for tool in rustup sudo curl; do
    printf '#!/usr/bin/env bash\n' >"$REMOTE/$tool"
done
cat >"$bin/curl" <<'EOF'
#!/usr/bin/env bash
echo 198.51.100.7
EOF
cat >"$bin/sleep" <<'EOF'
#!/usr/bin/env bash
[[ -z ${SLOW:-} ]] || exec /bin/sleep 1
EOF
# Comments live in $T/comments.json. Posts get the ids 1001, 1002, and so on. Like
# GitHub, it refuses a body over 65536 characters.
cat >"$bin/gh" <<'EOF'
#!/usr/bin/env bash
path=$2 body= filter= get=1
shift 2
while [[ $# -gt 0 ]]; do
    case $1 in
    -f) body=${2#body=} get= && shift
        [[ -z ${FAIL_END:-} || $body != "bench-host end"* ]] || exit 1 ;;
    -F) body=$(cat "${2#body=@}") get= && shift
        [[ -z ${FAIL_REPORT:-} ]] || exit 1 ;;
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
    if ((${#body} > 65536)); then
        echo "body is too long (maximum is 65536 characters)" >&2
        exit 1
    elif [[ -n $get ]]; then
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
chmod +x "$bin"/* "$REMOTE"/*

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
        for file in "$T/out" "$T/calls"; do
            [[ ! -e $file ]] || sed 's/^/  /' "$file"
        done
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
aligned='-C target-cpu=x86-64-v2 -C llvm-args=-align-all-functions=6'

run '[]' "${args[@]}" release 'push (pop|peek)'
check "a run posts the cap, the launch, the report, and the end" eval '
    [[ $status == 0 ]] &&
    has "$(posted 15)" "bench-host cap 2.67 USD for #1047, asked by box2.red-team" &&
    has "$(posted 15)" "bench-host launch i-1: c7i.metal-24xl, #1047, asked by \
box2.red-team, cap 2.67 USD, ends by" &&
    has "$(posted 15)" "bench-host end i-1 for #1047: 0.00 h. Terminated: yes. \
Still running from this run: none." &&
    [[ $(posted 1047 | grep -c "^table") == 4 ]]'
check "each remote cargo gets the aligned flags, named in the report" eval '
    [[ $(($(wc -l <"$T/rustflags"))) == 6 &&
        $(sort -u "$T/rustflags") == "$aligned" ]] &&
    has "$(posted 1047)" "\`RUSTFLAGS=\"$aligned\"\`"'
check "each filter goes to the host as one argument" eval '
    has "$(cat "$T/calls")" "-- release push\ \(pop\|peek\)"'

for minutes in -30 0 121 08 x 18446744073709551736; do
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

BENCH_DAY_CAP=100 run "[$(note 1 bench-bot "bench-host cap 12.34 USD for #1")]" \
    "${args[@]}"
check "a launch over 15 USD of caps today is withdrawn" eval '
    [[ $status == 1 ]] && has "$(cat "$T/out")" "15.01 USD of caps today" &&
    has "$(posted 15)" "bench-host withdrawn 1001: no host launched." &&
    [[ ! -e $T/calls ]]'

run "[$(note 1 someone "bench-host cap 50.00 USD"),
    $(note 2 bench-bot "bench-host cap -100.00 USD"),
    $(note 3 bench-bot "bench-host cap 13.00 USD")]" "${args[@]}"
check "a cap of another account or not a price does not count" eval '
    [[ $status == 1 ]] && has "$(cat "$T/out")" "15.67 USD of caps today"'

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

BIG=1 run '[]' "${args[@]}"
check "each run of a long output posts its end" eval '
    [[ $status == 0 && $(posted 1047 | grep -c "^table") == 4 ]] &&
    has "$(posted 1047)" "(only the last 15000 bytes" &&
    ! has "$(posted 1047)" xxxxxxxx'

EDGE=1 run '[]' "${args[@]}"
check "a cut at the start of a line keeps that line" eval '
    [[ $status == 0 && $(posted 1047 | grep -c "^table") == 8 ]]'

LONG=1 run '[]' "${args[@]}"
check "a last line over 15000 bytes keeps its last 15000 bytes" eval '
    lengths=$(posted 1047 | grep "table [1-4]$" | awk "{ print length }" | uniq -c)
    [[ $status == 0 && $lengths == *"4 14999" ]]'

FAIL_REPORT=1 run '[]' "${args[@]}"
check "a report that does not post is printed, and the run exits 1" eval '
    [[ $status == 1 ]] && has "$(cat "$T/out")" "table 4" &&
    has "$(cat "$T/out")" "the report did not post on #1047" &&
    has "$(posted 15)" "Terminated: yes. Still running from this run: none."'

FAIL_KEY=1 run '[]' "${args[@]}"
check "a key pair left exits 1 and is named" eval '
    [[ $status == 1 ]] && has "$(posted 15)" "Key pair bench-1047-"'

FAIL_LAUNCH=1 run '[]' "${args[@]}"
check "a failed launch exits 1 and says no host launched" eval '
    [[ $status == 1 ]] && has "$(posted 15)" "Terminated: no host launched."'

FAIL_END=1 run '[]' "${args[@]}"
check "an end line that does not post exits 1 and is printed" eval '
    [[ $status == 1 ]] &&
    has "$(cat "$T/out")" "post this on #15: bench-host end i-1"'

T=$(mktemp -d "$root/case.XXXX")
export T
echo '[]' >"$T/comments.json"
SLOW=1 FAIL_GROUP=1 PATH="$bin:$PATH" bash "$script" "${args[@]}" >"$T/out" 2>&1 &
pid=$!
until grep -qs delete-security-group "$T/calls" || ! kill -0 "$pid" 2>/dev/null; do
    /bin/sleep 0.2
done
signaled=
kill -TERM "$pid" 2>/dev/null && signaled=1
status=0
wait "$pid" || status=$?
check "a signal during the cleanup does not stop it" eval '
    [[ -n $signaled && $status == 1 ]] &&
    has "$(posted 15)" "Security group sg-1 is left." &&
    [[ $(grep -c delete-security-group "$T/calls") == 24 ]]'

run '[]' 1047 box2.red-team delivery missing bbbb2222
check "a commit not on GitHub is refused before any post" eval '
    [[ $status == 1 && -z $(posted 15) ]] &&
    has "$(cat "$T/out")" "no commit missing on GitHub"'

run '[]' 1047 box2.red-team 'delivery; reboot' aaaa1111 bbbb2222
check "a crate that is not a package name is refused" eval '
    [[ $status == 1 && -z $(posted 15) ]]'

[[ $failures == 0 ]] || { echo "$failures failed"; exit 1; }
echo "all passed"
