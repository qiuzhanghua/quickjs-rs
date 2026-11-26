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
# Test262 moves, and a baseline is only comparable against the commit it was measured on:
# the numbers in docs/test262-status.md and the outcome file next to them are pinned to the
# revision printed here. A fresh clone lands on HEAD, so re-runs can silently shift the
# denominator — which is exactly what happened when /tmp was wiped and the checkout came
# back three tests larger.
TEST262_COMMIT="$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
echo "== test262 checkout: $TEST262_COMMIT ($(find "$ROOT/test/built-ins" -name '*.js' | wc -l) files)"
if [ -n "${TEST262_PINNED_COMMIT:-}" ] && [ "$TEST262_COMMIT" != "$TEST262_PINNED_COMMIT" ]; then
    echo "   note: this is not the pinned commit $TEST262_PINNED_COMMIT; numbers will not"
    echo "         line up with docs/test262-baseline.txt.gz"
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
status=0
if [ -z "$DIRS" ]; then
    # shellcheck disable=SC2086 # word splitting of outcomes_args is intended
    ./target/release/test262 --root "$ROOT" --dir test/built-ins \
        --show-failures "${TEST262_SHOW_FAILURES:-10}" $outcomes_args || status=$?
else
    for dir in $DIRS; do
        # shellcheck disable=SC2086
        ./target/release/test262 --root "$ROOT" --dir "test/built-ins/$dir" \
            --show-failures "${TEST262_SHOW_FAILURES:-10}" $outcomes_args || status=$?
        echo
    done
fi

# Record which test262 the outcomes came from. A baseline is only comparable against the
# revision it was measured on, and nothing else in the file says which one that was — so a
# wiped checkout came back three tests larger and the stored numbers stopped lining up.
# `test262-diff.py` ignores this line (it has no tab-separated outcome).
if [ -n "$OUTCOMES" ] && [ -f "$OUTCOMES" ]; then
    { echo "# test262 $TEST262_COMMIT"; cat "$OUTCOMES"; } > "$OUTCOMES.tmp" &&
        mv "$OUTCOMES.tmp" "$OUTCOMES"
fi
exit "$status"
