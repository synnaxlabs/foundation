#!/usr/bin/env bash
# Runs the carrier benchmark between two Linux hosts and writes the results to
# results/<UTC time>/ next to this script.
#
#   run.sh <server> <client> [reps]
#
# <server> and <client> are ssh destinations with passwordless sudo (Ubuntu 24.04).
# They reach each other on UDP and TCP ports 4433 to 4454. The script installs the
# build tools, builds on each host, sets each host for the run, and puts the settings
# back when it ends. It exits non-zero when a run, a server, or a restore failed.
#
# Environment, with defaults:
#   CARRIER_SSH=ssh     the ssh command, for example "ssh -i key.pem"
#   CARRIER_DRY=0       1 skips what a container cannot set: sysctls, IRQs, RPS,
#                       irqbalance, and CPU idle states
#   PROFILES="default none gro-only gso-only jumbo lowat-off awake"
#   SECS=20             seconds of each bulk and ping run, after the warmup
#   PACED_SECS=60       seconds of each paced run, after the warmup
#   RUN_CPU=2           the CPU of the measured client and servers
#   LOAD_CPUS="4 5"     the CPUs of the two link load flows
#   IRQ_CPUS=8-15       the CPUs that take the NIC interrupts
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: $0 <server> <client> [reps]" >&2
  exit 2
fi
SERVER=$1
CLIENT=$2
REPS=${3:-3}
PROFILES=${PROFILES:-default none gro-only gso-only jumbo lowat-off awake}
SECS=${SECS:-20}
PACED_SECS=${PACED_SECS:-60}
RUN_CPU=${RUN_CPU:-2}
read -r -a LOAD_CPUS <<<"${LOAD_CPUS:-4 5}"
IRQ_CPUS=${IRQ_CPUS:-8-15}
IRQ_LIST=$(seq "${IRQ_CPUS%-*}" "${IRQ_CPUS#*-}" | paste -sd, -)
DRY=${CARRIER_DRY:-0}
LONGEST=$((SECS > PACED_SECS + 4 ? SECS : PACED_SECS + 4))

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=$HERE/results/$(date -u +%Y%m%dT%H%M%SZ)
mkdir -p "$OUT/samples"
CTL=$(mktemp -d /tmp/carrier.XXXXXX)
SSH="${CARRIER_SSH:-ssh} -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
-o ControlMaster=auto -o ControlPath=$CTL/%C -o ControlPersist=600"
BIN=foundation/target/release/carrier
FAILED=0
REP=0
PROFILE=

on() {
  local host=$1
  shift
  $SSH "$host" "$@"
}

log() {
  echo "[$(date -u +%H:%M:%S)] $*" | tee -a "$OUT/run.log" >&2
}

# Counts a failed run and writes it into its table.
fail() {
  local file=$1
  shift
  FAILED=$((FAILED + 1))
  log "failed: $*"
  echo "| $REP | $PROFILE | failed: $* |" >>"$OUT/$file"
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
mkdir -p foundation carrier-certs carrier-logs carrier-samples
EOF
  rsync -az -e "$SSH" --delete --exclude target --exclude .git \
    --exclude /bench/carrier/results "$ROOT/" "$host:foundation/"
  on "$host" 'cd foundation && ~/.cargo/bin/cargo build --release --locked -q -p carrier'
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
  on "$host" bash -s -- "$iface" >"$OUT/host-$name.txt" 2>&1 <<'EOF'
iface=$1
set -x
uname -a
cat /etc/os-release
lscpu
lscpu -e
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
ethtool -c "$iface"
ethtool -n "$iface" rx-flow-hash udp4
ethtool -n "$iface" rx-flow-hash tcp4
ip link show "$iface"
sysctl net.core net.ipv4.tcp_congestion_control net.ipv4.tcp_rmem \
  net.ipv4.tcp_wmem net.ipv4.tcp_notsent_lowat
cat /sys/devices/system/clocksource/clocksource0/current_clocksource
cat /sys/devices/system/cpu/cpuidle/current_driver
grep . /sys/devices/system/cpu/cpu0/cpuidle/state*/name
cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor
systemctl is-active irqbalance
for irq in $(ls /sys/class/net/$iface/device/msi_irqs); do
  echo "$irq: $(cat /proc/irq/$irq/smp_affinity_list)"
done
grep . /sys/class/net/$iface/queues/rx-*/rps_cpus
grep -E "$iface|CPU0" /proc/interrupts
cat /proc/cmdline
grep -E 'CONFIG_(IRQ_TIME_ACCOUNTING|HZ)[ =]' "/boot/config-$(uname -r)"
cd foundation && ~/.cargo/bin/rustc -V
EOF
}

# Saves the host settings once, then sets them for the run: socket buffer limits,
# NIC interrupts on IRQ_CPUS, RPS off, and irqbalance stopped.
tune() {
  on "$1" sudo bash -s -- "$2" "$DRY" "$IRQ_LIST" <<'EOF'
set -e
iface=$1
dry=$2
irqs=$3
saved=/tmp/carrier-saved
if [ ! -f $saved ]; then
  {
    echo "ip link set dev $iface mtu $(cat /sys/class/net/$iface/mtu)"
    ethtool -k "$iface" | awk -v iface="$iface" '
      /^generic-receive-offload:/ { gro = $2 }
      /^generic-segmentation-offload:/ { gso = $2 }
      /^tcp-segmentation-offload:/ { tso = $2 }
      END { print "ethtool -K " iface " gro " gro " gso " gso " tso " tso }'
    if [ "$dry" != 1 ]; then
      for key in net.core.rmem_max net.core.wmem_max net.core.rmem_default \
        net.ipv4.tcp_notsent_lowat; do
        echo "sysctl -qw $key=$(sysctl -n $key)"
      done
      for irq in $(ls /sys/class/net/$iface/device/msi_irqs); do
        echo "echo $(cat /proc/irq/$irq/smp_affinity_list) >/proc/irq/$irq/smp_affinity_list"
      done
      for rps in /sys/class/net/$iface/queues/rx-*/rps_cpus; do
        echo "echo $(cat $rps) >$rps"
      done
      if systemctl is-active -q irqbalance; then
        echo "systemctl start irqbalance"
      fi
    fi
  } >$saved.part
  mv $saved.part $saved
fi
[ "$dry" = 1 ] && exit 0
# noq leaves the UDP socket buffers at the kernel default; TCP tunes its own.
sysctl -qw net.core.rmem_max=16777216 net.core.wmem_max=16777216 \
  net.core.rmem_default=4194304
if systemctl is-active -q irqbalance; then
  systemctl stop irqbalance
fi
for irq in $(ls /sys/class/net/$iface/device/msi_irqs); do
  echo "$irqs" >/proc/irq/$irq/smp_affinity_list
done
for rps in /sys/class/net/$iface/queues/rx-*/rps_cpus; do
  echo 0 >$rps
done
EOF
}

# Prints the host settings of a profile: GRO, GSO, TSO, MTU, tcp_notsent_lowat, and
# whether CPUs stay out of idle states.
settings() {
  case $1 in
    default) echo "on on on 1500 16384 no" ;;
    none) echo "off off off 1500 16384 no" ;;
    gro-only) echo "on off off 1500 16384 no" ;;
    gso-only) echo "off on on 1500 16384 no" ;;
    jumbo) echo "on on on 9001 16384 no" ;;
    lowat-off) echo "on on on 1500 4294967295 no" ;;
    awake) echo "on on on 1500 16384 yes" ;;
    *)
      echo "unknown profile $1" >&2
      return 1
      ;;
  esac
}

# Sets a host for a profile. An offload the NIC fixes stays as it is.
apply() {
  local host=$1 iface=$2
  shift 2
  on "$host" sudo bash -s -- "$iface" "$DRY" "$@" <<'EOF'
set -e
iface=$1
dry=$2
gro=$3
gso=$4
tso=$5
mtu=$6
lowat=$7
awake=$8
for feature in generic-receive-offload:gro:$gro \
  generic-segmentation-offload:gso:$gso tcp-segmentation-offload:tso:$tso; do
  IFS=: read -r name flag state <<<"$feature"
  if ethtool -k "$iface" | grep -q "^$name: .*\[fixed\]"; then
    echo "$name is fixed on $iface" >&2
  else
    ethtool -K "$iface" "$flag" "$state"
  fi
done
ip link set dev "$iface" mtu "$mtu"
[ "$dry" = 1 ] && exit 0
sysctl -qw net.ipv4.tcp_notsent_lowat="$lowat"
if [ -f /tmp/carrier-awake.pid ]; then
  kill "$(cat /tmp/carrier-awake.pid)"
  rm /tmp/carrier-awake.pid
fi
if [ "$awake" = yes ]; then
  # CPUs stay out of idle states while this file is open with a limit of 0.
  nohup bash -c 'exec 3>/dev/cpu_dma_latency; printf 0x00000000 >&3
    echo $$ >/tmp/carrier-awake.pid; exec sleep infinity' \
    >/dev/null 2>&1 </dev/null &
  sleep 0.5
  [ -f /tmp/carrier-awake.pid ]
fi
EOF
}

restore() {
  on "$1" sudo bash -s <<'EOF'
set -e
pkill -x carrier || true
if [ -f /tmp/carrier-awake.pid ]; then
  kill "$(cat /tmp/carrier-awake.pid)" || true
  rm /tmp/carrier-awake.pid
fi
if [ -f /tmp/carrier-saved ]; then
  bash -e /tmp/carrier-saved
  rm /tmp/carrier-saved
fi
EOF
}

cleanup() {
  local status=$?
  for host in "$SERVER" "$CLIENT"; do
    if ! restore "$host"; then
      log "restore failed on $host"
      status=1
    fi
    $SSH -O exit "$host" 2>/dev/null || true
  done
  rm -rf "$CTL"
  exit "$status"
}

# Starts the measured servers on RUN_CPU and one load server pair on each LOAD_CPU.
# Ports: QUIC on 4433, 4443, 4453 and TLS on the next port up.
serve() {
  on "$SERVER" bash -s -- "$BIN" "$RUN_CPU,$IRQ_LIST" "$RUN_CPU" "${LOAD_CPUS[@]}" \
    "$@" <<'EOF'
set -e
bin=$1
cpus=$2
run=$3
a=$4
b=$5
shift 5
pkill -x carrier || true
sleep 0.5
start() {
  local carrier=$1 port=$2 cpu=$3
  shift 3
  nohup taskset -c "$cpu" "$bin" server "$carrier" "0.0.0.0:$port" \
    carrier-certs/cert.pem carrier-certs/key.pem "$@" \
    >>"carrier-logs/server-$port.log" 2>&1 </dev/null &
}
start quic 4433 "$run" cpus="$cpus" "$@"
start tls 4434 "$run" cpus="$cpus" "$@"
start quic 4443 "$a" "$@"
start tls 4444 "$a" "$@"
start quic 4453 "$b" "$@"
start tls 4454 "$b" "$@"
sleep 1
for port in 4433 4443 4453; do
  ss -lunH "sport = :$port" | grep -q . || { echo "no QUIC server on $port" >&2; exit 1; }
  port=$((port + 1))
  ss -ltnH "sport = :$port" | grep -q . || { echo "no TLS server on $port" >&2; exit 1; }
done
EOF
}

# Runs one client on CPU $1 against the server pair at port $3 and prints its line.
client() {
  local cpu=$1 carrier=$2 port=$3
  shift 3
  [[ $carrier == tls ]] && port=$((port + 1))
  on "$CLIENT" "timeout $((LONGEST + 60)) taskset -c $cpu $BIN client $carrier \
    $SERVER_IP:$port carrier-certs/ca.pem $*"
}

# Prints the counters of a host: TCP retransmits, UDP receive buffer errors, UDP
# send buffer errors, UDP input errors, ENA allowance drops, and qdisc drops.
counters() {
  on "$1" bash -s -- "$2" <<'EOF'
iface=$1
nstat -asz TcpRetransSegs UdpRcvbufErrors UdpSndbufErrors UdpInErrors | awk '
  { v[$1] = $2 }
  END {
    printf "%d %d %d %d", v["TcpRetransSegs"], v["UdpRcvbufErrors"],
      v["UdpSndbufErrors"], v["UdpInErrors"]
  }'
ethtool -S "$iface" | awk '/allowance_exceeded/ { s += $2 } END { printf " %d", s }'
tc -s qdisc show dev "$iface" root | awk '
  /dropped/ {
    for (i = 1; i < NF; i++) if ($i == "(dropped") { gsub(",", "", $(i + 1)); s += $(i + 1) }
  }
  END { printf " %d\n", s }'
EOF
}

# Prints the counters of both hosts.
snapshot() {
  echo "$(counters "$SERVER" "$SERVER_IF") $(counters "$CLIENT" "$CLIENT_IF")"
}

# Prints the growth of each counter, summed over both hosts, as table columns.
growth() {
  awk -v a="$1" -v b="$2" 'BEGIN {
    split(a, x, " ")
    split(b, y, " ")
    for (i = 1; i <= 6; i++) printf " %d |", y[i] - x[i] + y[i + 6] - x[i + 6]
  }'
}

# Runs the measured client and prints its line with the counter growth. The counters
# cover the whole host, load flows included.
measure() {
  local before after line
  log "run: $*"
  before=$(snapshot)
  line=$(client "$RUN_CPU" "$@" 2>>"$OUT/run.log") || return 1
  after=$(snapshot)
  echo "$line$(growth "$before" "$after")"
}

# The options of the profile for clients and servers.
flags() {
  if [[ $PROFILE == jumbo ]]; then
    echo mtu=9001
  fi
}

# The options of a measured client, with a samples file named by the arguments.
options() {
  local opts=(cpus="$RUN_CPU,$IRQ_LIST" $(flags))
  if [[ $# -gt 0 ]]; then
    opts+=(samples="carrier-samples/$REP-$PROFILE-$(IFS=-; echo "$*")")
  fi
  echo "${opts[@]}"
}

bulk() {
  local carrier=$1 flag=${2:-}
  local line
  # shellcheck disable=SC2046 # options are single words
  if line=$(measure "$carrier" 4433 bulk "$SECS" $flag $(options)); then
    echo "| $REP | $PROFILE | ${flag:--} $line" >>"$OUT/bulk.md"
  else
    fail bulk.md "bulk $carrier $flag"
  fi
}

ping() {
  local carrier=$1 frames=$2 size=$3
  local line
  # shellcheck disable=SC2046
  if line=$(measure "$carrier" 4433 ping "$frames" "$size" "$SECS" \
    $(options "$carrier" "$frames" ping "$size")); then
    echo "| $REP | $PROFILE | none $line" >>"$OUT/latency.md"
  else
    fail latency.md "ping $carrier $frames $size"
  fi
}

# Runs a paced test. A link load runs two bulk flows on other CPUs and ports beside
# it, so the flows share only the link, and its Gbit/s replaces the load column.
paced() {
  local carrier=$1 frames=$2 load=$3
  local mode=none ok=1 gbps=0 line i
  local pids=(0 0)
  [[ $load == shared ]] && mode=shared
  if [[ $load == link-* ]]; then
    for i in 0 1; do
      # shellcheck disable=SC2046
      client "${LOAD_CPUS[$i]}" "${load#link-}" $((4443 + 10 * i)) bulk \
        $((PACED_SECS + 4)) $(flags) >"$OUT/load-$i.part" 2>>"$OUT/run.log" &
      pids[i]=$!
    done
    sleep 2
  fi
  # shellcheck disable=SC2046
  line=$(measure "$carrier" 4433 paced "$frames" 256 1000 "$PACED_SECS" "$mode" \
    $(options "$carrier" "$frames" paced "$load")) || ok=0
  if [[ $load == link-* ]]; then
    for i in 0 1; do
      if wait "${pids[i]}"; then
        sed "s/^/| $REP | $PROFILE | $load-$i /" "$OUT/load-$i.part" >>"$OUT/load.md"
        gbps=$(awk -F'|' -v s="$gbps" '{ print s + $3 }' "$OUT/load-$i.part")
      else
        log "failed: load flow $i of $load"
        ok=0
      fi
      rm "$OUT/load-$i.part"
    done
  fi
  if ((!ok)); then
    fail latency.md "paced $carrier $frames $load"
    return
  fi
  if [[ $load == link-* ]]; then
    line=$(echo "$line" | awk -F'|' -v OFS='|' -v g="$gbps" '{ $7 = sprintf(" %.2f ", g); print }')
  fi
  echo "| $REP | $PROFILE | $load $line" >>"$OUT/latency.md"
}

# Prints the items rotated left by $1, so each rep runs them in another order.
rotate() {
  local n=$1
  shift
  local items=("$@")
  local k=$((n % ${#items[@]}))
  echo "${items[@]:k}" "${items[@]:0:k}"
}

matrix() {
  local carriers variants v c load size
  carriers=$(rotate "$REP" quic tls)
  variants=$(rotate "$REP" quic:stream quic:datagram tls:stream)
  case $PROFILE in
    default)
      for c in $(rotate "$REP" quic quic:unsegmented tls); do
        bulk "${c%%:*}" "$([[ $c == *:* ]] && echo "${c#*:}")"
      done
      for v in $variants; do
        local sizes="64 256 1024 4096 16384 65536"
        [[ $v == *datagram ]] && sizes="64 256 1024"
        for size in $sizes; do
          ping "${v%:*}" "${v#*:}" "$size"
        done
      done
      for v in $variants; do
        for load in $(rotate "$REP" none shared link-quic link-tls); do
          paced "${v%:*}" "${v#*:}" "$load"
        done
      done
      ;;
    none | awake)
      for v in $variants; do
        ping "${v%:*}" "${v#*:}" 64
        paced "${v%:*}" "${v#*:}" none
      done
      if [[ $PROFILE == none ]]; then
        for c in $carriers; do
          bulk "$c"
        done
      fi
      ;;
    gro-only | gso-only | jumbo)
      for c in $carriers; do
        bulk "$c"
      done
      ;;
    lowat-off)
      bulk tls
      ;;
  esac
}

# Copies the server logs and latency samples, and counts server errors as failures.
collect() {
  local logs=$OUT/server-$REP-$PROFILE.log errors
  on "$SERVER" 'cat carrier-logs/*.log && rm carrier-logs/*.log' >"$logs"
  errors=$(grep -c '^error:' "$logs" || true)
  if ((errors > 0)); then
    FAILED=$((FAILED + errors))
    log "$errors server errors in $logs"
  fi
  rsync -az -e "$SSH" --remove-source-files "$CLIENT:carrier-samples/" "$OUT/samples/"
}

# Writes a table header: the columns before `--`, the client's columns for a kind of
# test, then the columns after `--`.
header() {
  local file=$1 kind=$2 line="|" column count
  shift 2
  while [[ $1 != -- ]]; do
    line+=" $1 |"
    shift
  done
  shift
  line+=$(on "$CLIENT" "$BIN" columns "$kind" | cut -c2-)
  for column in "$@"; do
    line+=" $column |"
  done
  count=$(tr -cd '|' <<<"$line" | wc -c)
  printf '%s\n|%s\n' "$line" "$(printf -- '---|%.0s' $(seq 2 "$count"))" >"$OUT/$file"
}

trap cleanup EXIT
for profile in $PROFILES; do
  settings "$profile" >/dev/null
done
git -C "$ROOT" rev-parse HEAD >"$OUT/commit.txt"
git -C "$ROOT" status --short >>"$OUT/commit.txt"

log "setting up $SERVER and $CLIENT"
setup "$SERVER" &
server_setup=$!
setup "$CLIENT" &
client_setup=$!
wait "$server_setup"
wait "$client_setup"
SERVER_IP=$(address "$SERVER")
CLIENT_IP=$(address "$CLIENT")
SERVER_IF=$(interface "$SERVER" "$CLIENT_IP")
CLIENT_IF=$(interface "$CLIENT" "$SERVER_IP")
log "server $SERVER_IP ($SERVER_IF), client $CLIENT_IP ($CLIENT_IF)"

on "$SERVER" bash -s <<'EOF'
set -e
cd carrier-certs
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 7 \
  -subj /CN=carrier-ca -keyout ca-key.pem -out ca.pem 2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
  -subj /CN=carrier.test -keyout key.pem -out leaf.csr 2>/dev/null
openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca-key.pem -days 7 \
  -extfile <(echo subjectAltName=DNS:carrier.test) -out cert.pem 2>/dev/null
EOF
on "$SERVER" cat carrier-certs/ca.pem | on "$CLIENT" 'cat >carrier-certs/ca.pem'

tune "$SERVER" "$SERVER_IF"
tune "$CLIENT" "$CLIENT_IF"
# shellcheck disable=SC2046
apply "$SERVER" "$SERVER_IF" $(settings default) 2>>"$OUT/run.log"
# shellcheck disable=SC2046
apply "$CLIENT" "$CLIENT_IF" $(settings default) 2>>"$OUT/run.log"
record "$SERVER" "$SERVER_IF" server
record "$CLIENT" "$CLIENT_IF" client

COUNTERS=("TCP retrans" "UDP rcvbuf errors" "UDP sndbuf errors" "UDP in errors"
  "ENA allowance drops" "qdisc drops")
header bulk.md bulk rep profile options -- "${COUNTERS[@]}"
header load.md bulk rep profile load --
header latency.md latency rep profile load -- "${COUNTERS[@]}"

for REP in $(seq 1 "$REPS"); do
  for PROFILE in $PROFILES; do
    log "rep $REP, profile $PROFILE"
    # shellcheck disable=SC2046
    apply "$SERVER" "$SERVER_IF" $(settings "$PROFILE") 2>>"$OUT/run.log"
    # shellcheck disable=SC2046
    apply "$CLIENT" "$CLIENT_IF" $(settings "$PROFILE") 2>>"$OUT/run.log"
    if ((REP == 1)); then
      for host in "$SERVER:$SERVER_IF" "$CLIENT:$CLIENT_IF"; do
        on "${host%:*}" ethtool -k "${host##*:}" >"$OUT/offload-$PROFILE-${host%:*}.txt"
      done
    fi
    # shellcheck disable=SC2046
    serve $(flags)
    matrix
    collect
  done
done

if ((FAILED > 0)); then
  log "done with $FAILED failures: $OUT"
  exit 1
fi
log "done: $OUT"
