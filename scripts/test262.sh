#!/bin/sh
# Runs the official test262 suite against the built engine.
#
# Deliberately NOT part of scripts/check.sh: a full run is a minute of wall clock on eight
# cores and needs a test262 checkout on disk, so gating every commit on it would trade the
# fast feedback loop for a number that changes slowly. Run it when the question is "how
# much of the language actually works", not "did I just break something".
#
#   ./scripts/test262.sh                     # all of test/built-ins
#   ./scripts/test262.sh Array RegExp        # selected areas
#   TEST262_GROUP=1 ./scripts/test262.sh     # failures grouped by built-in
#   TEST262_SIGNATURES=1 ./scripts/test262.sh  # failure shapes, most common first
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

DIRS="${*:-}"
if [ -z "$DIRS" ]; then
    exec ./target/release/test262 --root "$ROOT" --dir test/built-ins --show-failures "${TEST262_SHOW_FAILURES:-10}"
fi

status=0
for dir in $DIRS; do
    ./target/release/test262 --root "$ROOT" --dir "test/built-ins/$dir" \
        --show-failures "${TEST262_SHOW_FAILURES:-10}" || status=$?
    echo
done
exit "$status"
