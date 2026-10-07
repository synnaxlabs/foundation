#!/bin/bash
# usage: run.sh <outdir> <rounds> <samples> <e2e passes per round>
R=/tmp/claude-1000/-home-ubuntu-Desktop-synnaxlabs-foundation-wt-builder-4/8461716e-4eb5-45fe-acc8-35e084bb504f/scratchpad/r2-perf
O=$R/out/$1; mkdir -p $O; cd $O
rounds=$2; samples=$3; passes=$4
logs=(base head headm rnd1 baseB)
e2es=(base head rnd1 baseB)
bin() { case $1 in baseB) echo base;; *) echo $1;; esac; }
uptime > $O/load_before.txt
for r in $(seq -w 1 $rounds); do
  read cpu b1 b2 host < <(python3 -I $R/src/pick.py)
  echo "round $r cpu $cpu busy $b1 sibling $b2 host $host load $(cut -d' ' -f1-3 /proc/loadavg)" >> $O/rounds.txt
  n=${#logs[@]}; s=$((10#$r % n))
  for i in $(seq 0 $((n-1))); do
    l=${logs[$(((i+s)%n))]}
    taskset -c $cpu $R/bin/log_$(bin $l) --bench --sample-count $samples > $O/log_${l}_$r.txt 2>&1
  done
  for p in $(seq -w 1 $passes); do
    n=${#e2es[@]}; s=$(((10#$r + 10#$p) % n))
    for i in $(seq 0 $((n-1))); do
      l=${e2es[$(((i+s)%n))]}
      taskset -c $cpu $R/bin/e2e_$(bin $l) --bench > $O/e2e_${l}_${r}_$p.txt 2>&1
    done
  done
done
uptime > $O/load_after.txt
echo done > $O/done.txt
