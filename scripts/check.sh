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

# NFR-7: the engine has no third-party crates, the host layer has only the agreed ones.
# A requirement nobody checks is a requirement nobody has, and this one decides whether the
# interpreter can be embedded without dragging an async runtime along.
echo "== scripts/check-no-engine-deps.sh (NFR-7)"
./scripts/check-no-engine-deps.sh

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

# Every count README.md publishes, checked against what the suites measure. Six of them
# had drifted while they were maintained by hand (35 vs 36 literals, 41 vs 59
# declaration-conflict cases, 365 vs 366 bytecode cases); a number in a table is a claim,
# so it gets a test.
echo "== scripts/verify-claims.py (numbers README.md publishes)"
python3 ./scripts/verify-claims.py
