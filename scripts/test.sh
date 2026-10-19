#!/usr/bin/env bash
# Runs the tests of the workspace in CI, in one of two parts that can run at the same
# time. Each other argument goes to each cargo command.
#
# Usage: scripts/test.sh nextest|serial [cargo flag...]
#
#   nextest  each test that a harness lists, with nextest, other than each full test
#            path in the env var `SKIPPED`
#   serial   the doc tests, and each test target with no harness (`ALONE`), with
#            `cargo test`: nextest runs a test binary to list its tests, and such a
#            binary lists none
#
# Each command runs to its end, and the script fails when one failed.

set -u

ALONE="alloc browse bytes erase global interrupt local memory peak roots"

part=${1:-}
shift || true
case $part in
  nextest)
    filter=""
    for target in $ALONE; do filter+=" | (kind(test) & binary(=$target))"; done
    for test in ${SKIPPED:-}; do filter+=" | test(=$test)"; done
    exec cargo nextest run --workspace "$@" --profile ci -E "not (${filter# | })"
    ;;
  serial)
    failed=0
    cargo test --workspace "$@" --no-fail-fast --doc || failed=1
    cargo test --workspace "$@" --no-fail-fast $(printf -- '--test %s ' $ALONE) ||
      failed=1
    exit "$failed"
    ;;
  *)
    echo "usage: scripts/test.sh nextest|serial [cargo flag...]" >&2
    exit 2
    ;;
esac
