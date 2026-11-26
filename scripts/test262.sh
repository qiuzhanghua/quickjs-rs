#!/bin/sh
# Runs the official test262 suite against the built engine.
#
# Deliberately NOT part of scripts/check.sh: a full run is a few minutes of wall clock
# and needs a test262 checkout on disk, so gating every commit on it would trade the
# fast feedback loop for a number that changes slowly. Run it when the question is "how
# much of the language actually works", not "did I just break something".
#
#   ./scripts/test262.sh                     # all of test/built-ins
#   ./scripts/test262.sh Array RegExp        # selected areas
#   TEST262_GROUP=1 ./scripts/test262.sh     # failures grouped by built-in
#   TEST262_SIGNATURES=1 ./scripts/test262.sh  # failure shapes, most common first
#   TEST262_OUTCOMES=/tmp/now.txt ./scripts/test262.sh   # per-test outcomes
#   TEST262_JOBS=4 ./scripts/test262.sh      # cap concurrency (default: min(cores, 8))
#
# For "what changed since the baseline", use scripts/test262-diff.py against the file
# written by TEST262_OUTCOMES; it reports +fixed / -regressed as test-name sets.
#
# Exit status is non-zero if any runnable test fails, so it can gate a release.
set -eu
cd "$(dirname "$0")/.."

ROOT="${TEST262_ROOT:-/tmp/tc39-test262}"
if [ ! -d "$ROOT/harness" ]; then
    echo "== fetching test262 into $ROOT (sparse: harness + built-ins)"
    git clone --depth 1 --filter=blob:none --sparse \
        https://github.com/tc39/test262 "$ROOT"
    git -C "$ROOT" sparse-checkout set harness test/built-ins
fi

echo "== building qjs"
cargo build --release --offline -q -p quickjs-cli --bin qjs 2>/dev/null ||
    cargo build --release --offline -q
echo "== building test262 runner"
cargo build --release --offline -q -p quickjs-conformance --bin test262

# Each engine process is capped by the runner itself (ulimit -v), because
# `qjs --memory-limit` is accepted and then ignored. Without that cap a single test
# that allocates in a loop takes the machine down with it.
OUTCOMES="${TEST262_OUTCOMES:-}"
outcomes_args=""
if [ -n "$OUTCOMES" ]; then
    outcomes_args="--outcomes $OUTCOMES"
fi

DIRS="${*:-}"
if [ -z "$DIRS" ]; then
    # shellcheck disable=SC2086 # word splitting of outcomes_args is intended
    exec ./target/release/test262 --root "$ROOT" --dir test/built-ins \
        --show-failures "${TEST262_SHOW_FAILURES:-10}" $outcomes_args
fi

status=0
for dir in $DIRS; do
    # shellcheck disable=SC2086
    ./target/release/test262 --root "$ROOT" --dir "test/built-ins/$dir" \
        --show-failures "${TEST262_SHOW_FAILURES:-10}" $outcomes_args || status=$?
    echo
done
exit "$status"
