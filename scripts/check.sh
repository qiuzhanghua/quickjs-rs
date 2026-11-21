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
