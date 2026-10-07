#!/bin/bash
# usage: setup.sh <tree> <shim>. Adds the scratch bench targets to a tree.
set -e
R=/tmp/claude-1000/-home-ubuntu-Desktop-synnaxlabs-foundation-wt-builder-4/8461716e-4eb5-45fe-acc8-35e084bb504f/scratchpad/r2-perf
t=$R/$1; b=$t/crates/buffer
mkdir -p $b/benches/log $b/benches/e2e $b/benches/log_alloc
cp $R/src/log_main.rs $b/benches/log/main.rs
cp $R/src/$2 $b/benches/log/shim.rs
cp $R/src/e2e_main.rs $b/benches/e2e/main.rs
cp $R/src/$2 $b/benches/log_alloc/shim.rs
python3 -I - $b <<'PY'
import sys
b=sys.argv[1]
m=open(b+'/benches/log/main.rs').read()
old="fn main() {\n    divan::main();\n}"
assert old in m
m=m.replace(old,"#[global_allocator]\nstatic ALLOC: divan::AllocProfiler = divan::AllocProfiler::system();\n\n"+old)
open(b+'/benches/log_alloc/main.rs','w').write(m)
c=open(b+'/Cargo.toml').read()
if 'divan' not in c:
    c=c.replace('counting = { path = "../counting" }\n','counting = { path = "../counting" }\ndivan = { workspace = true }\n')
    c=c.replace('[lints]','[[bench]]\nname = "log"\nharness = false\n\n[[bench]]\nname = "log_alloc"\nharness = false\n\n[[bench]]\nname = "e2e"\nharness = false\n\n[lints]')
    open(b+'/Cargo.toml','w').write(c)
PY
cd $t
out=$(CARGO_TARGET_DIR=$R/target-$1 cargo bench -p buffer --bench log --bench log_alloc --bench e2e --no-run 2>&1) || { echo "$out" | tail -40; exit 1; }
mkdir -p $R/bin
for n in log log_alloc e2e; do
  bin=$(echo "$out" | grep -o "target-$1/release/deps/$n-[0-9a-f]*" | tail -1)
  cp $R/$bin $R/bin/${n}_$1
done
echo "$out" | tail -5
echo built $1
