#!/usr/bin/env bash
# Runs the handoff benchmark on one Linux host and writes the results to
# results/<UTC time>/ next to this script.
#
#   run.sh <host> [reps]
#
# <host> is an ssh destination with passwordless sudo (Ubuntu 24.04). The script
# installs the build tools, builds there, records the host, and runs the matrix.
# It changes no host setting. It exits non-zero when a run failed.
#
# Environment, with defaults:
#   HANDOFF_SSH=ssh     the ssh command, for example "ssh -i key.pem"
#   SECS=10             seconds of each run
#   CPU=1               the first core; shards take S cores from it, producers P
#                       after them
#   PACE=10000          frames per second per producer in the paced runs

set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=$HERE/results/$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$OUT"
CTL=$(mktemp -d /tmp/handoff.XXXXXX)
SSH="${HANDOFF_SSH:-ssh} -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
-o ControlMaster=auto -o ControlPath=$CTL/%C -o ControlPersist=600"
BIN=foundation/target/release/handoff
HOST=${1:?usage: run.sh <host> [reps]}
REPS=${2:-3}
SECS=${SECS:-10}
CPU=${CPU:-1}
PACE=${PACE:-10000}
FAILED=0

on() {
  $SSH "$HOST" "$@"
}

log() {
  echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$OUT/run.log" >&2
}

setup() {
  on bash -s <<'SETUP'
set -e
if ! command -v cc >/dev/null || ! command -v rsync >/dev/null; then
  sudo apt-get update -qq
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
    build-essential rsync >/dev/null
fi
if [ ! -x ~/.cargo/bin/cargo ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y -q --profile minimal --default-toolchain none
fi
mkdir -p foundation
SETUP
  rsync -az -e "$SSH" --delete --exclude target --exclude .git \
    --exclude /bench/handoff/results "$ROOT/" "$HOST:foundation/"
  on 'cd foundation && ~/.cargo/bin/cargo build --release --locked -q -p handoff'
}

record() {
  on bash -s >"$OUT/host.txt" 2>&1 <<'RECORD'
set -x
uname -a
cat /etc/os-release
lscpu
lscpu -e
token=$(curl -s -m 2 -X PUT http://169.254.169.254/latest/api/token \
  -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
for key in instance-type placement/availability-zone; do
  curl -s -m 2 -H "X-aws-ec2-metadata-token: $token" \
    "http://169.254.169.254/latest/meta-data/$key"
  echo
done
cat /sys/devices/system/clocksource/clocksource0/current_clocksource
cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null
uptime
~/.cargo/bin/rustc --version
RECORD
}

# Runs one configuration REPS times and appends each table line to results.md.
run() {
  local design=$1
  shift
  local rep
  for rep in $(seq 1 "$REPS"); do
    log "$design $* rep $rep"
    if ! on "$BIN $design secs=$SECS cpu=$CPU $*" >>"$OUT/results.md" 2>>"$OUT/run.log"
    then
      FAILED=$((FAILED + 1))
      log "failed: $design $*"
      echo "| failed: $design $* |" >>"$OUT/results.md"
    fi
  done
}

log "setup on $HOST"
setup
record
on "$BIN columns" >"$OUT/results.md"

for work in 1 8; do
  for shards in 4 8; do
    run handoff producers=8 shards=$shards work=$work
  done
  run inline shards=8 work=$work
done
for work in 1 8; do
  for shards in 4 8; do
    run handoff producers=8 shards=$shards work=$work pace=$PACE
  done
  run inline shards=8 work=$work pace=$PACE
done
run handoff producers=8 shards=8 work=1 pace=$PACE spin=20000

rm -rf "$CTL"
log "done: $FAILED failed, results in $OUT"
exit $((FAILED > 0))
