#!/usr/bin/env bash
# Runs the benchmarks of one crate at two commits on a quiet AWS host, posts the
# tables on an issue or PR, and ends the host.
#
# Usage: scripts/bench-host.sh <issue> <asker> <crate> <base> <head> [filter...]
#
#   issue   the issue or PR that gets the result, and the `issue` tag of the host
#   asker   the session that asked for the run, such as box2.red-team
#   crate   the crate whose benches run (`cargo bench -p <crate>`)
#   base    the commit to compare against, on GitHub
#   head    the commit under test, on GitHub
#   filter  a divan filter, given to the benches as one argument
#
# It runs base, head, base, head, so a drift of the host shows as a difference
# between the two runs of one commit.
#
# Runs from any machine with `aws` (credentials for the account of the test budget),
# `gh`, `ssh`, `curl`, and bash 3.2 or later. It needs no clone. It posts on the
# ledger (#15), as the account of `gh`: the cap before the launch, a register line
# after it, and an end line. A launch that would pass 15 USD of caps today is
# withdrawn. On exit, it ends the host, ends any other host of this run, deletes the
# key pair and the security group, and exits 1 when it cannot confirm each. A
# SIGKILL skips this; the host still ends itself.
#
# Environment:
#   BENCH_MINUTES    the host shuts itself down after this, 1 to 120 (120)
#   BENCH_PRICE_MAX  the spot limit, in USD an hour (1.20)

set -euo pipefail

repo=synnaxlabs/foundation
ledger=15
region=us-east-1
type=c7i.metal-24xl
# The person's limits for #1139.
day_cap=15
minutes_max=120

usage() {
    sed -n '5,12p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

fail() {
    echo "bench-host: $*" >&2
    exit 1
}

[[ $# -ge 5 ]] || usage
issue=$1 asker=$2 crate=$3 base=$4 head=$5
shift 5
filters=("$@")
minutes=${BENCH_MINUTES:-$minutes_max}
price_max=${BENCH_PRICE_MAX:-1.20}

for tool in aws gh ssh curl; do
    command -v "$tool" >/dev/null || fail "needs $tool"
done
# Patterns live in variables, because bash 3.2 reads a quoted pattern as text.
number='^[0-9]+$'
session='^[a-z0-9][a-z0-9.-]*$'
package='^[a-z0-9_-]+$'
whole='^[1-9][0-9]*$'
price='^[0-9]+(\.[0-9]+)?$'
hash='^[0-9a-f]{40}$'
[[ $issue =~ $number ]] || fail "the issue is a number"
[[ $asker =~ $session ]] || fail "the asker is a session name"
[[ $crate =~ $package ]] || fail "the crate is a package name"
if ! [[ $minutes =~ $whole ]] || ((minutes > minutes_max)); then
    fail "BENCH_MINUTES is from 1 to $minutes_max"
fi
if ! [[ $price_max =~ $price ]] || awk -v p="$price_max" 'BEGIN { exit p > 0 }'; then
    fail "BENCH_PRICE_MAX is a price over 0"
fi

sha() {
    local sha
    sha=$(gh api "repos/$repo/commits/$1" --jq .sha) || fail "no commit $1 on GitHub"
    [[ $sha =~ $hash ]] || fail "no commit $1 on GitHub"
    echo "$sha"
}
base=$(sha "$base")
head=$(sha "$head")
me=$(gh api user --jq .login)

ec2() { aws ec2 --region "$region" "$@"; }

# Posts `$2` on issue `$1` and prints the comment's id.
post() {
    gh api "repos/$repo/issues/$1/comments" -f body="$2" --jq .id
}

# The sum of the caps that this account posted today, up to the comment `$1`, less
# those withdrawn.
caps() {
    local today
    today=$(date -u +%F)
    gh api "repos/$repo/issues/$ledger/comments" --paginate --jq ".[]
        | select(.user.login == \"$me\" and (.created_at | startswith(\"$today\")))
        | \"\(.id) \(.body | split(\"\n\")[0])\"" |
        awk -v upto="$1" '
            $2 == "bench-host" && $3 == "cap" && $4 ~ /^[0-9]+\.[0-9][0-9]$/ &&
                $1 + 0 <= upto + 0 { cap[$1] = $4 }
            $2 == "bench-host" && $3 == "withdrawn" { gone[$4 + 0] = 1 }
            END {
                for (id in cap) if (!((id + 0) in gone)) sum += cap[id]
                printf "%.2f", sum
            }'
}

# The epoch time `$1` as UTC, with GNU or BSD `date`.
utc() {
    date -u -d "@$1" +%Y-%m-%dT%H:%MZ 2>/dev/null || date -u -r "$1" +%Y-%m-%dT%H:%MZ
}

# The spot limit, plus 0.03 USD an hour for the disk and address, for the life plus
# 10 min of boot (#15).
cap=$(awk -v p="$price_max" -v m="$minutes" \
    'BEGIN { printf "%.2f", int((p + 0.03) * (m + 10) / 60 * 100 + 0.999999) / 100 }')
name="bench-$issue-$(date -u +%Y%m%dT%H%M%S)"
work=$(mktemp -d)
key=$work/key
cap_id=
keyed=
group=
attempted=
instance=
launched=

finish() {
    local status=$? ended=unconfirmed running=unknown left note= line hours
    set +e
    if [[ -n $attempted ]]; then
        if [[ -n $instance ]] &&
            ec2 terminate-instances --instance-ids "$instance" >/dev/null &&
            ec2 wait instance-terminated --instance-ids "$instance"; then
            ended=yes
        fi
        # By the run's name, so a host whose id was lost ends too.
        if left=$(ec2 describe-instances --filters "Name=tag:Name,Values=$name" \
            "Name=instance-state-name,Values=pending,running,stopping,stopped" \
            --query 'Reservations[].Instances[].InstanceId' --output text); then
            running=${left:-none}
            # shellcheck disable=SC2086 # One argument for each id.
            [[ -n $left ]] && ec2 terminate-instances --instance-ids $left >/dev/null
        fi
        [[ -z $instance && $running == none ]] && ended="no host launched"
        [[ $ended != yes || $running != none ]] && status=1
    fi
    if [[ -n $group ]]; then
        for _ in $(seq 24); do
            ec2 delete-security-group --group-id "$group" 2>"$work/group" &&
                group= && break
            sleep 5
        done
        if [[ -n $group ]]; then
            cat "$work/group" >&2
            note+=" Security group $group is left."
            status=1
        fi
    fi
    if [[ -n $keyed ]] && ! ec2 delete-key-pair --key-name "$name" >/dev/null; then
        note+=" Key pair $name is left."
        status=1
    fi
    line=
    if [[ -n $attempted ]]; then
        hours=$(awk -v s="$launched" -v e="$(date +%s)" \
            'BEGIN { printf "%.2f", (e - s) / 3600 }')
        line="bench-host end ${instance:-unknown} for #$issue: $hours h."
        line+=" Terminated: $ended. Still running from this run: $running.$note"
    elif [[ -n $cap_id ]]; then
        line="bench-host withdrawn $cap_id: no host launched.$note"
    fi
    if [[ -n $line ]] && ! post "$ledger" "$line" >/dev/null; then
        echo "bench-host: post this on #$ledger: $line" >&2
        status=1
    fi
    rm -rf "$work"
    exit "$status"
}
trap finish EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

line="bench-host cap $cap USD for #$issue, asked by $asker: 1 $type spot, at most"
line+=" $price_max USD an hour for $minutes min."
cap_id=$(post "$ledger" "$line")
# Read after the post, so of two launches at once, the later one counts both.
total=$(caps "$cap_id")
if awk -v t="$total" -v c="$day_cap" 'BEGIN { exit !(t > c) }'; then
    fail "$total USD of caps today with this one, over $day_cap"
fi

ec2 create-key-pair --key-name "$name" --key-type ed25519 \
    --query KeyMaterial --output text >"$key"
keyed=1
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
attempted=1
launched=$(date +%s)
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
# The shutdown counts from boot, a few minutes after the launch.
line="bench-host launch $instance: $type, #$issue, asked by $asker, cap $cap USD,"
line+=" ends by $(utc $((launched + (minutes + 10) * 60)))."
post "$ledger" "$line" >/dev/null
ec2 wait instance-running --instance-ids "$instance"
address=$(ec2 describe-instances --instance-ids "$instance" \
    --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)

remote() {
    ssh -i "$key" -o StrictHostKeyChecking=accept-new \
        -o UserKnownHostsFile="$work/known_hosts" -o ConnectTimeout=10 \
        -o ServerAliveInterval=30 "ubuntu@$address" "$@"
}
reached=
for _ in $(seq 60); do
    remote true 2>/dev/null && reached=1 && break
    sleep 5
done
[[ -n $reached ]] || fail "no SSH to $instance after 5 min"

# The host builds both commits before the first run, so no build runs beside a
# benchmark.
remote bash -s -- "$repo" "$base" "$head" "$crate" <<'SETUP' ||
set -euo pipefail
repo=$1 base=$2 head=$3 crate=$4
sudo apt-get -qq update >/dev/null
sudo DEBIAN_FRONTEND=noninteractive apt-get -qq install -y build-essential git \
    >/dev/null
curl -fsS https://sh.rustup.rs |
    sh -s -- -y --profile minimal --default-toolchain none >/dev/null
for commit in "$base" "$head"; do
    [ -d "$commit" ] && continue
    git init -q "$commit"
    git -C "$commit" fetch -q --depth 1 "https://github.com/$repo" "$commit"
    git -C "$commit" checkout -q FETCH_HEAD
    cd "$commit"
    ~/.cargo/bin/rustup toolchain install >/dev/null
    ~/.cargo/bin/cargo bench -q -p "$crate" --no-run
    cd
done
SETUP
    fail "the setup on $instance failed"

host=$(remote 'echo "$(lscpu | sed -n "s/^Model name: *//p"), $(nproc) CPUs,' \
    '$(uname -r)"')
quoted=
if [[ ${#filters[@]} -gt 0 ]]; then
    quoted=$(printf '%q ' "${filters[@]}")
fi
report=$work/report.md
{
    echo "## Benchmarks on a quiet host"
    echo
    echo "\`cargo bench -p $crate${quoted:+ -- $quoted}\` on AWS $type spot"
    echo "($instance, $region): $host. The host runs nothing else."
    echo
    echo "Base $base, head $head. The runs go base, head, base, head."
} >"$report"

failed=
for run in 1 2 3 4; do
    commit=$base label=base result=
    ((run % 2 == 0)) && commit=$head label=head
    load=$(remote cat /proc/loadavg | cut -d' ' -f1-3)
    out=$work/run$run.txt
    # `commit` and `crate` are checked, so only the filters need quotes.
    if ! remote "cd $commit && ~/.cargo/bin/cargo bench -q -p $crate -- $quoted" \
        >"$out" 2>&1; then
        failed=$run
        result=", failed"
    fi
    {
        echo
        echo "<details><summary>Run $run: $label ${commit:0:8}$result, load $load" \
            "</summary>"
        echo
        echo '```'
        cat "$out"
        echo '```'
        echo
        echo "</details>"
    } >>"$report"
    [[ -z $failed ]] || break
done

gh api "repos/$repo/issues/$issue/comments" -F "body=@$report" --jq .id >/dev/null
echo "bench-host: posted on #$issue"
if [[ -n $failed ]]; then
    cat "$work/run$failed.txt" >&2
    fail "run $failed failed; the report up to it is on #$issue"
fi
