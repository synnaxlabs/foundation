#!/usr/bin/env bash
# Runs the carrier benchmark between two Linux hosts and writes the results to
# results/<UTC time>/ next to this script.
#
#   run.sh <server> <client> [reps] [secs]
#
# <server> and <client> are ssh destinations with passwordless sudo (Ubuntu 24.04).
# They reach each other on UDP and TCP ports 4433, 4434, 4443, and 4444. The script
# installs the build tools, builds on each host, sets the network for the run, and
# puts the settings back when it ends. CARRIER_SSH replaces the ssh command, for
# example "ssh -i key.pem". CARRIER_DRY=1 skips the socket buffer sysctls, which a
# container cannot set, for a dry run of the script.
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: $0 <server> <client> [reps] [secs]" >&2
  exit 2
fi
SERVER=$1
CLIENT=$2
REPS=${3:-3}
SECS=${4:-20}

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=$HERE/results/$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$OUT"

# The measured process runs on RUN_CPU. The link load runs on LOAD_CPU.
RUN_CPU=${RUN_CPU:-2}
LOAD_CPU=${LOAD_CPU:-4}
MTU=1500
SSH=${CARRIER_SSH:-ssh}
DRY=${CARRIER_DRY:-0}

on() {
  local host=$1
  shift
  $SSH -o BatchMode=yes "$host" "$@"
}

log() {
  echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$OUT/run.log" >&2
}

setup() {
  local host=$1
  on "$host" bash -s <<'EOF'
set -e
if ! command -v cc >/dev/null || ! command -v ethtool >/dev/null \
  || ! command -v rsync >/dev/null || ! command -v cmake >/dev/null \
  || ! command -v openssl >/dev/null; then
  sudo apt-get update -qq
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
    build-essential cmake ethtool openssl rsync >/dev/null
fi
if [ ! -x ~/.cargo/bin/cargo ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y -q --profile minimal --default-toolchain none
fi
mkdir -p carrier
EOF
  rsync -az -e "$SSH" --delete --exclude target --exclude results --exclude certs \
    "$HERE/" "$host:carrier/"
  rsync -az -e "$SSH" "$ROOT/rust-toolchain.toml" "$host:carrier/"
  on "$host" 'cd carrier && ~/.cargo/bin/cargo build --release --locked -q'
}

# Prints the first IPv4 address of a host.
address() {
  on "$1" hostname -I | awk '{print $1}'
}

# Prints the interface a host uses to reach an address.
interface() {
  on "$1" ip -4 -o route get "$2" \
    | awk '{for (i = 1; i < NF; i++) if ($i == "dev") print $(i + 1)}'
}

record() {
  local host=$1 iface=$2 name=$3
  on "$host" bash -s -- "$iface" >"$OUT/host-$name.txt" 2>&1 <<'EOF' || true
iface=$1
set -x
uname -a
cat /etc/os-release
lscpu
token=$(curl -s -m 2 -X PUT http://169.254.169.254/latest/api/token \
  -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
for key in instance-type placement/availability-zone placement/group-name; do
  curl -s -m 2 -H "X-aws-ec2-metadata-token: $token" \
    "http://169.254.169.254/latest/meta-data/$key"
  echo
done
ethtool -i "$iface"
ethtool -k "$iface"
ethtool -l "$iface"
ethtool -g "$iface"
ip link show "$iface"
sysctl net.core net.ipv4.tcp_congestion_control net.ipv4.tcp_rmem net.ipv4.tcp_wmem
grep . /sys/devices/system/cpu/cpu0/cpuidle/state*/name
cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor
systemctl is-active irqbalance
grep -E "$iface|CPU0" /proc/interrupts
cat /proc/cmdline
EOF
}

# Saves the network settings once, then sets them for the run.
tune() {
  on "$1" sudo bash -s -- "$2" "$MTU" "$DRY" <<'EOF'
set -e
iface=$1
mtu=$2
dry=$3
keys="net.core.rmem_max net.core.wmem_max net.core.rmem_default net.core.wmem_default"
[ "$dry" = 1 ] && keys=
saved=/tmp/carrier-saved
if [ ! -f $saved ]; then
  {
    for key in $keys; do
      echo "sysctl -qw $key=$(sysctl -n $key)"
    done
    echo "ip link set dev $iface mtu $(cat /sys/class/net/$iface/mtu)"
    ethtool -k "$iface" | awk -v iface="$iface" '
      /^generic-receive-offload:/ { gro = $2 }
      /^generic-segmentation-offload:/ { gso = $2 }
      /^tcp-segmentation-offload:/ { tso = $2 }
      END { print "ethtool -K " iface " gro " gro " gso " gso " tso " tso }'
  } >$saved
fi
# noq leaves the UDP socket buffers at the kernel default; TCP tunes its own.
if [ -n "$keys" ]; then
  sysctl -qw net.core.rmem_max=16777216 net.core.wmem_max=16777216 \
    net.core.rmem_default=4194304 net.core.wmem_default=4194304
fi
ip link set dev "$iface" mtu "$mtu"
EOF
}

offload() {
  on "$1" sudo ethtool -K "$2" gro "$3" gso "$3" tso "$3"
}

restore() {
  on "$1" sudo bash -s <<'EOF' || true
pkill -x carrier
if [ -f /tmp/carrier-saved ]; then
  bash /tmp/carrier-saved && rm /tmp/carrier-saved
fi
EOF
}

# Starts the measured servers on RUN_CPU and the load servers on LOAD_CPU.
serve() {
  # ssh joins its arguments into one string, so the flag that may be empty goes last.
  on "$SERVER" bash -s -- "$RUN_CPU" "$LOAD_CPU" "$1" <<'EOF'
run=$1
load=$2
flag=${3:-}
cd carrier
pkill -x carrier || true
sleep 0.5
for spec in "quic 4433 $run" "tls 4434 $run" "quic 4443 $load" "tls 4444 $load"; do
  set -- $spec
  nohup taskset -c "$3" target/release/carrier server "$1" "0.0.0.0:$2" \
    certs/cert.pem certs/key.pem $flag >"server-$1-$2.log" 2>&1 </dev/null &
done
sleep 1
for port in 4434 4444; do
  if ! ss -ltn "sport = :$port" | grep -q LISTEN; then
    echo "no server on $port" >&2
    exit 1
  fi
done
EOF
}

port() {
  case $1 in
    tls) echo "$2" ;;
    *) echo "$(($2 - 1))" ;;
  esac
}

# Runs one client on CPU $1 and prints its table line.
client() {
  local cpu=$1 carrier=$2 base=$3 flag=$4
  shift 4
  on "$CLIENT" "cd carrier && timeout 300 taskset -c $cpu target/release/carrier \
    client $carrier $SERVER_IP:$(port "$carrier" "$base") certs/ca.pem $flag $*"
}

# Appends one line to $OUT/$1, with the columns that only this script knows.
row() {
  local file=$1 rep=$2 off=$3 load=$4
  shift 4
  local line
  if line=$("$@" 2>>"$OUT/run.log"); then
    echo "| $rep | $off | $load $line" | tee -a "$OUT/$file"
  else
    log "failed: $*"
  fi
}

matrix() {
  local rep=$1 off=$2 flag="" sizes="64 256 1024" loads="none shared link"
  if [[ $off == off ]]; then
    flag=no-gso
    sizes=64
    loads=none
  fi
  serve "$flag"
  for carrier in quic tls; do
    row bulk.md "$rep" "$off" - client "$RUN_CPU" "$carrier" 4434 "$flag" bulk "$SECS"
  done
  for carrier in quic quic-dgram tls; do
    for size in $sizes; do
      row latency.md "$rep" "$off" - \
        client "$RUN_CPU" "$carrier" 4434 "$flag" ping "$size" "$SECS"
    done
    for load in $loads; do
      local paced=(paced 256 1000 "$SECS")
      case $load in
        shared) paced+=(load) ;;
        link)
          # A second process pair on other cores shares only the link. The bulk
          # client sends for longer than the paced run, warmup included.
          local bulk=quic
          [[ $carrier == tls ]] && bulk=tls
          row load.md "$rep" "$off" "for-$carrier" \
            client "$LOAD_CPU" "$bulk" 4444 "$flag" bulk $((SECS + 4)) >/dev/null &
          sleep 1
          ;;
      esac
      row latency.md "$rep" "$off" "$load" \
        client "$RUN_CPU" "$carrier" 4434 "$flag" "${paced[@]}"
      wait
    done
  done
}

log "setting up $SERVER and $CLIENT"
setup "$SERVER" &
setup "$CLIENT" &
wait
SERVER_IP=$(address "$SERVER")
CLIENT_IP=$(address "$CLIENT")
SERVER_IF=$(interface "$SERVER" "$CLIENT_IP")
CLIENT_IF=$(interface "$CLIENT" "$SERVER_IP")
log "server $SERVER_IP ($SERVER_IF), client $CLIENT_IP ($CLIENT_IF)"

on "$SERVER" bash -s <<'EOF'
set -e
mkdir -p carrier/certs
cd carrier/certs
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 7 \
  -subj /CN=carrier-ca -keyout ca-key.pem -out ca.pem 2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -subj /CN=carrier.test -keyout key.pem -out leaf.csr 2>/dev/null
openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca-key.pem -days 7 \
  -extfile <(echo subjectAltName=DNS:carrier.test) -out cert.pem 2>/dev/null
EOF
on "$CLIENT" mkdir -p carrier/certs
on "$SERVER" cat carrier/certs/ca.pem | on "$CLIENT" 'cat >carrier/certs/ca.pem'

trap 'restore "$SERVER"; restore "$CLIENT"' EXIT
record "$SERVER" "$SERVER_IF" server
record "$CLIENT" "$CLIENT_IF" client
tune "$SERVER" "$SERVER_IF"
tune "$CLIENT" "$CLIENT_IF"

{
  echo "| rep | offload | load | carrier | gso | Gbit/s | lost packets | client core % \
| server core % | client thread ns/B | client host ns/B | server thread ns/B \
| server host ns/B |"
  echo "|---|---|---|---|---|---|---|---|---|---|---|---|---|"
} >"$OUT/bulk.md"
{
  echo "| rep | offload | load | carrier | gso | test | size | rate | n | lost \
| p50 us | p99 us | p99.9 us | max us |"
  echo "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
} >"$OUT/latency.md"

for rep in $(seq 1 "$REPS"); do
  for off in on off; do
    log "rep $rep, offload $off"
    offload "$SERVER" "$SERVER_IF" "$off"
    offload "$CLIENT" "$CLIENT_IF" "$off"
    matrix "$rep" "$off"
  done
done
log "done: $OUT"
