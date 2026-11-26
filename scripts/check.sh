#!/bin/sh
# Every milestone must leave the tree green: this is the gate run before a
# `git commit`.
#
#   ./scripts/check.sh            # fmt + clippy + tests
#   ./scripts/check.sh --fast     # tests only
set -eu
cd "$(dirname "$0")/.."

if [ "${1:-}" != "--fast" ]; then
    echo "== cargo fmt --check"
    cargo fmt --all -- --check
    echo "== cargo clippy -D warnings"
    cargo clippy --all-targets --offline -- -D warnings
fi

echo "== cargo test"
cargo test --offline

# The upstream suite, test by test. This is the step that used to be ad hoc, and the
# reason `test_loop.js` and `test_bigint.js` went unmeasured: it is skipped, loudly,
# when the reference interpreter is not built.
echo "== scripts/lang-diff.py (upstream differential suite)"
python3 ./scripts/lang-diff.py

# Ordinary program-shaped code, diffed against the reference. Upstream's own suite is
# small and single-purpose; three of v0.4.0's wrong answers (the `Set` default iterator,
# `Array.from(Set)`, `break label` inside `for…of`) were found by running code shaped
# like this and by nothing else. Skips loudly when the reference is not built.
echo "== scripts/realworld.sh (real-world corpus)"
./scripts/realworld.sh
