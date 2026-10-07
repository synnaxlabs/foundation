#!/usr/bin/env bash
# Runs the benchmarks of one crate at two commits on a quiet AWS host, posts the
# tables on an issue or PR, and ends the host.
#
# Usage: scripts/bench-host.sh <issue> <crate> <base> <head> [divan filter...]
#
#   issue   the issue or PR that gets the result, and the `issue` tag of the host
#   crate   the crate whose benches run (`cargo bench -p <crate>`)
#   base    the commit to compare against
#   head    the commit under test
#
# It runs base, head, base, head, so a drift of the host shows as a difference
# between the two runs of one commit.
#
# Runs from any machine with `aws` (credentials for the account of the test budget),
# `gh`, `ssh`, `curl`, and bash 3.2 or later. It needs no clone. Before the launch,
# it checks the spot price and the day's ledger, and posts the cap on the ledger
# issue. On exit, for any reason, it ends the host, checks that no host of the issue
# still runs, and posts the hours on the ledger.
#
# Environment (each has a default):
#   BENCH_REGION     AWS region (us-east-1)
#   BENCH_TYPE       instance type (c7i.metal-24xl)
#   BENCH_PRICE_MAX  the most the spot host may cost, in USD an hour (1.20)
#   BENCH_MINUTES    the host shuts itself down after this (120)
#   BENCH_DAY_CAP    the most the caps of one UTC day may sum to, in USD (15)
#   BENCH_LEDGER     the ledger issue (15)

set -euo pipefail

usage() {
    sed -n '4,10p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

[[ $# -ge 4 ]] || usage
issue=$1 crate=$2 base=$3 head=$4
shift 4
filters=("$@")

region=${BENCH_REGION:-us-east-1}
type=${BENCH_TYPE:-c7i.metal-24xl}
price_max=${BENCH_PRICE_MAX:-1.20}
minutes=${BENCH_MINUTES:-120}
day_cap=${BENCH_DAY_CAP:-15}
ledger=${BENCH_LEDGER:-15}
repo=synnaxlabs/foundation
marker='bench-host cap USD'

fail() {
    echo "bench-host: $*" >&2
    exit 1
}
for tool in aws gh ssh curl; do
    command -v "$tool" >/dev/null || fail "needs $tool"
done
[[ $minutes -le 240 ]] || fail "a host lives at most 240 min"
[[ $issue =~ ^[0-9]+$ ]] || fail "the issue is a number"
base=$(gh api "repos/$repo/commits/$base" --jq .sha)
head=$(gh api "repos/$repo/commits/$head" --jq .sha)

ec2() { aws ec2 --region "$region" "$@"; }

# The cap is the spot price limit times the lifetime: the host cannot cost more.
cap=$(awk -v p="$price_max" -v m="$minutes" 'BEGIN { printf "%.2f", p * m / 60 }')
# The highest price over the zones, because the launch can go to any of them.
spot=$(ec2 describe-spot-price-history --instance-types "$type" \
    --product-descriptions Linux/UNIX --start-time "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    --query 'SpotPriceHistory[].SpotPrice' --output text | tr '\t' '\n' | sort -n |
    tail -1)
[[ -n $spot && $spot != None ]] || fail "no spot price for $type in $region"
if awk -v s="$spot" -v m="$price_max" 'BEGIN { exit !(s > m) }'; then
    fail "spot $type is $spot USD an hour, over $price_max"
fi

today=$(date -u +%F)
spent=$(gh api "repos/$repo/issues/$ledger/comments" --paginate \
    --jq ".[] | select(.created_at | startswith(\"$today\")) | .body" |
    awk -v marker="$marker" 'index($0, marker) == 1 { sum += $4 }
        END { printf "%.2f", sum }')
total=$(awk -v a="$spent" -v b="$cap" 'BEGIN { printf "%.2f", a + b }')
if awk -v t="$total" -v c="$day_cap" 'BEGIN { exit !(t > c) }'; then
    fail "$spent USD of caps today, and $cap more passes $day_cap"
fi

name="bench-$issue-$(date -u +%Y%m%dT%H%M%S)"
work=$(mktemp -d)
key=$work/key
instance=
group=
launched=

finish() {
    local status=$?
    set +e
    if [[ -n $instance ]]; then
        ec2 terminate-instances --instance-ids "$instance" >/dev/null
        ec2 wait instance-terminated --instance-ids "$instance"
    fi
    local running
    running=$(ec2 describe-instances \
        --filters "Name=tag:issue,Values=$issue" \
        "Name=tag:project,Values=foundation-bench" \
        "Name=instance-state-name,Values=pending,running,stopping,shutting-down" \
        --query 'Reservations[].Instances[].InstanceId' --output text)
    [[ -n $group ]] && ec2 delete-security-group --group-id "$group" >/dev/null
    ec2 delete-key-pair --key-name "$name" >/dev/null 2>&1
    if [[ -n $launched ]]; then
        local hours
        hours=$(awk -v s="$launched" -v e="$(date +%s)" \
            'BEGIN { printf "%.2f", (e - s) / 3600 }')
        local line="bench-host hours $hours for #$issue: $type spot $instance, ended."
        line+=" Still running with \`issue=$issue\`: ${running:-none}."
        gh issue comment "$ledger" -R "$repo" --body "$line" >/dev/null
    fi
    if [[ -n $running ]]; then
        echo "bench-host: still running: $running" >&2
        status=1
    fi
    rm -rf "$work"
    exit "$status"
}
trap finish EXIT

line="$marker $cap for #$issue: 1 $type spot, at most $price_max USD an hour"
line+=" for $minutes min (spot now $spot). Caps today with this one: $total of"
line+=" $day_cap USD."
gh issue comment "$ledger" -R "$repo" --body "$line" >/dev/null

ec2 create-key-pair --key-name "$name" --key-type ed25519 \
    --query KeyMaterial --output text >"$key"
chmod 600 "$key"
group=$(ec2 create-security-group --group-name "$name" \
    --description "bench host for #$issue" --query GroupId --output text)
ip=$(curl -fsS https://checkip.amazonaws.com)
ec2 authorize-security-group-ingress --group-id "$group" \
    --protocol tcp --port 22 --cidr "$ip/32" >/dev/null

ubuntu=/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3
image=$(aws ssm get-parameter --region "$region" --name "$ubuntu/ami-id" \
    --query Parameter.Value --output text)
tags="{Key=project,Value=foundation-bench},{Key=issue,Value=$issue}"
tags+=",{Key=Name,Value=$name}"
instance=$(ec2 run-instances --image-id "$image" --instance-type "$type" \
    --key-name "$name" --security-group-ids "$group" \
    --instance-initiated-shutdown-behavior terminate \
    --instance-market-options "MarketType=spot,SpotOptions={MaxPrice=$price_max,\
SpotInstanceType=one-time,InstanceInterruptionBehavior=terminate}" \
    --block-device-mappings "DeviceName=/dev/sda1,Ebs={VolumeSize=100,\
VolumeType=gp3,DeleteOnTermination=true}" \
    --tag-specifications "ResourceType=instance,Tags=[$tags]" \
    "ResourceType=volume,Tags=[$tags]" \
    --user-data "$(printf '#!/bin/sh\nshutdown -h +%s\n' "$minutes")" \
    --query 'Instances[0].InstanceId' --output text)
launched=$(date +%s)
ec2 wait instance-running --instance-ids "$instance"
address=$(ec2 describe-instances --instance-ids "$instance" \
    --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)

remote() {
    ssh -i "$key" -o StrictHostKeyChecking=accept-new \
        -o UserKnownHostsFile="$work/known_hosts" -o ConnectTimeout=10 \
        -o ServerAliveInterval=30 "ubuntu@$address" "$@"
}
for _ in $(seq 60); do
    remote true 2>/dev/null && break
    sleep 5
done

# The host builds both commits before the first run, so no build runs beside a
# benchmark.
remote bash -s -- "$repo" "$base" "$head" "$crate" <<'SETUP'
set -euo pipefail
repo=$1 base=$2 head=$3 crate=$4
sudo apt-get -qq update >/dev/null
sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y build-essential git \
    >/dev/null
curl -fsS https://sh.rustup.rs |
    sh -s -- -y --profile minimal --default-toolchain none >/dev/null
for commit in "$base" "$head"; do
    git clone -q "https://github.com/$repo" "$commit"
    git -C "$commit" checkout -q "$commit"
    (cd "$commit" && ~/.cargo/bin/rustup toolchain install >/dev/null)
    (cd "$commit" && ~/.cargo/bin/cargo bench -q -p "$crate" --no-run)
done
SETUP

host=$(remote 'echo "$(lscpu | sed -n "s/^Model name: *//p"), $(nproc) CPUs,' \
    '$(uname -r)"')
report=$work/report.md
{
    echo "## Benchmarks on a quiet host"
    echo
    echo "\`cargo bench -p $crate${filters[*]:+ -- ${filters[*]}}\` on AWS $type spot"
    echo "($instance, $region): $host. The host runs nothing else."
    echo
    echo "Base $base, head $head. The runs go base, head, base, head."
} >"$report"

for run in 1 2 3 4; do
    commit=$base label=base
    (( run % 2 == 0 )) && commit=$head label=head
    load=$(remote cat /proc/loadavg | cut -d' ' -f1-3)
    table=$(remote "cd $commit && ~/.cargo/bin/cargo bench -q -p $crate --" \
        "${filters[*]:-}" 2>&1)
    {
        echo
        echo "<details><summary>Run $run: $label ${commit:0:8}, load $load</summary>"
        echo
        echo '```'
        echo "$table"
        echo '```'
        echo
        echo "</details>"
    } >>"$report"
done

gh issue comment "$issue" -R "$repo" --body-file "$report" >/dev/null
echo "bench-host: posted on #$issue"
