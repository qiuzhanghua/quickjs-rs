#!/bin/sh
# Runs the real-world corpus against both engines and compares byte for byte.
#
# Why a second differential suite when `lang-diff.py` already exists: that one runs
# upstream's own `tests/*.js`, which are small, single-purpose, and written by the
# same author as the reference. Ordinary program-shaped code exercises *sequences*
# — a Map built in a loop, then spread, then sorted — and that is where three wrong
# answers in v0.4.0 came from (the default `Set` iterator yielding pairs,
# `Array.from(new Set(...))` returning `[]`, and `break label` failing inside
# `for…of`). None of those were caught by upstream's suite.
#
#   ./scripts/realworld.sh
#
# Each program must produce identical stdout, stderr and exit status on both engines.
# Needs the reference built (`./scripts/setup-oracle.sh`); without it this prints
# SKIPPED rather than passing vacuously, for the same reason `lang-diff.py` does.
set -eu
cd "$(dirname "$0")/.."

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ORACLE="${QJS_ORACLE:-$REPO_ROOT/oracle/quickjs-master/qjs}"
if [ ! -x "$ORACLE" ]; then
    for candidate in "$REPO_ROOT"/oracle/quickjs-master/qjs /tmp/quickjs-master/qjs /opt/quickjs/qjs; do
        if [ -x "$candidate" ]; then
            ORACLE="$candidate"
            break
        fi
    done
fi
if [ ! -x "$ORACLE" ]; then
    echo "SKIPPED: no reference interpreter ($ORACLE)"
    echo "         run ./scripts/setup-oracle.sh to build it; this measured nothing"
    exit 0
fi

OURS="./target/release/qjs"
if [ ! -x "$OURS" ]; then
    echo "building qjs"
    cargo build --release --offline -q -p quickjs-cli --bin qjs
fi

TMP="${TMPDIR:-/tmp}/quickjs-realworld.$$"
mkdir -p "$TMP"
trap 'rm -rf "$TMP"' EXIT INT TERM

status=0
pass=0
fail=0
for program in scripts/realworld/*.js; do
    name="$(basename "$program")"
    # `-m` when the program is a module, matching what a user would type.
    mode=""
    if head -n 5 "$program" | grep -q '^// module:'; then
        mode="-m"
    fi
    # shellcheck disable=SC2086 # mode is empty or -m
    "$ORACLE" $mode "$program" >"$TMP/ref.out" 2>"$TMP/ref.err"
    ref_status=$?
    # shellcheck disable=SC2086
    "$OURS" $mode "$program" >"$TMP/our.out" 2>"$TMP/our.err"
    our_status=$?
    if [ "$ref_status" != "$our_status" ]; then
        echo "GAP   $name: exit $ref_status vs $our_status"
        status=1
        fail=$((fail + 1))
        continue
    fi
    if ! diff -q "$TMP/ref.out" "$TMP/our.out" >/dev/null; then
        echo "GAP   $name: stdout differs"
        diff "$TMP/ref.out" "$TMP/our.out" | head -20
        status=1
        fail=$((fail + 1))
        continue
    fi
    if ! diff -q "$TMP/ref.err" "$TMP/our.err" >/dev/null; then
        echo "GAP   $name: stderr differs"
        diff "$TMP/ref.err" "$TMP/our.err" | head -20
        status=1
        fail=$((fail + 1))
        continue
    fi
    pass=$((pass + 1))
done

echo "== realworld: $pass passed, $fail differ"
exit "$status"
