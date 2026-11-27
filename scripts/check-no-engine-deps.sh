#!/bin/sh
# NFR-7, as an assertion rather than a promise.
#
# The requirement used to read "runtime zero third-party crates" and nothing checked it,
# which is the same as not having it. Now the rule is layered, and both halves are checked
# here:
#
#   1. The engine — `quickjs-pure` and everything it pulls in — has an empty `cargo tree`.
#      An embedder takes the interpreter, not an async runtime.
#   2. The host layer may depend on tokio/libc/windows-sys, but only those, and only for
#      the event loop. A new dependency there is a decision, so it has to be made
#      deliberately by editing the list below.
#
#   ./scripts/check-no-engine-deps.sh
set -eu
cd "$(dirname "$0")/.."

fail=0

# --- 1. the engine's closure must be empty -------------------------------------------
#
# `-e normal` drops dev- and build-dependencies, which is what "what an embedder takes"
# means. Anything left after the workspace crates is a third-party crate.
engine=$(cargo tree -p quickjs-pure -e normal --prefix none 2>/dev/null \
    | grep -v '^quickjs-' \
    | sed 's/ v[0-9].*//' \
    | sort -u \
    | sed '/^$/d')

if [ -n "$engine" ]; then
    echo "NFR-7 FAILED: the engine gained third-party dependencies:"
    echo "$engine" | sed 's/^/  /'
    echo "  An embedder must be able to take quickjs-pure without them."
    echo "  If one is genuinely needed, it belongs in quickjs-host or quickjs-cli."
    fail=1
else
    echo "NFR-7 ok: quickjs-pure's dependency closure is empty"
fi

# --- 2. the host layer's dependencies are the agreed ones ---------------------------
#
# Kept as an explicit list so adding one is a visible edit to this file rather than a
# line that appears in Cargo.toml and nowhere else.
# `rustix` brings `bitflags`, `errno`, `libc` and its per-platform syscall tables
# (`linux-raw-sys`). All of it is the safe-syscall story NFR-6 needs rather than raw
# `libc` in the host layer, so all of it is listed.
#
# One line on purpose: the loop below matches with surrounding spaces, and a newline
# inside the string silently stopped `windows-sys` from matching.
allowed=" libc bytes mio pin-project-lite socket2 tokio windows-link windows-sys rustix bitflags errno linux-raw-sys "

for target in "" "--target x86_64-pc-windows-msvc" "--target x86_64-unknown-linux-gnu"; do
    # shellcheck disable=SC2086 # the empty case must expand to no argument at all
    deps=$(cargo tree -p quickjs-host -e normal --prefix none $target 2>/dev/null \
        | grep -v '^quickjs-' \
        | sed 's/ v[0-9].*//' \
        | sed 's/ (\*)//' \
        | sort -u \
        | sed '/^$/d')
    label=${target:-native}
    for dep in $deps; do
        case "$allowed" in
            *" $dep "*) ;;
            *)
                echo "NFR-7 FAILED: quickjs-host gained '$dep' ($label)"
                echo "  Allowed: $allowed"
                fail=1
                ;;
        esac
    done
done
[ "$fail" -eq 0 ] && echo "NFR-7 ok: quickjs-host's dependencies are the agreed host set"

exit "$fail"
