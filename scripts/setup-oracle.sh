#!/bin/sh
# Fetch and build the C reference implementation of QuickJS, used as the
# differential-test oracle by `quickjs-conformance`.
#
#   ./scripts/setup-oracle.sh [target-dir]
#
# Default target directory: /tmp/quickjs-master (one of the paths searched by
# quickjs_conformance::locate_oracle, override with $QJS_ORACLE).
#
# Building this is not optional decoration: without it `scripts/lang-diff.py`
# measures nothing and only prints SKIPPED, so every "byte-for-byte identical"
# claim in the README is unverifiable on that machine. It needs network access
# once, and a C toolchain.
set -eu

DEST="${1:-/tmp/quickjs-master}"
PARENT="$(dirname "$DEST")"
TARBALL="$PARENT/quickjs-master.tar.gz"
LOG="$PARENT/quickjs-oracle-build.log"

if [ -x "$DEST/qjs" ] && [ -x "$DEST/qjsc" ] && [ -x "$DEST/qjsc-dump" ]; then
    echo "oracle already built: $DEST/qjs ($("$DEST/qjs" --help | head -1))"
    exit 0
fi

NPROC="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"

# `extra` is passed as an *environment* CFLAGS, never as a command-line one.
#
# This distinction is the whole reason this script used to fail. On the command
# line, `make CFLAGS=...` overrides the Makefile's own `CFLAGS+=$(DEFINES)`, and
# `DEFINES` is where `-DCONFIG_VERSION=\"$(cat VERSION)\"` comes from. Without it
# the preprocessor sees `-DCONFIG_VERSION=2026-06-04`, which is the arithmetic
# expression `2026 - 6 - 4`, and the build dies with
#
#     qjsc.c:417: error: expected ')' before numeric constant
#
# In the environment the Makefile appends its own flags to ours, so both the
# extra define and the version string survive with their quoting intact.
build() {
    extra="$1"
    shift
    CFLAGS="$extra" make -C "$DEST" -j"$NPROC" "$@" >>"$LOG" 2>&1 || {
        echo "build failed: $*" >&2
        echo "  full log: $LOG" >&2
        tail -20 "$LOG" >&2
        exit 1
    }
}

clean() {
    make -C "$DEST" clean >>"$LOG" 2>&1 || true
}

echo "downloading bellard/quickjs master -> $TARBALL"
curl -fsSL -o "$TARBALL" https://codeload.github.com/bellard/quickjs/tar.gz/refs/heads/master

rm -rf "$DEST.old"
if [ -d "$DEST" ]; then mv "$DEST" "$DEST.old"; fi
tar xzf "$TARBALL" -C "$PARENT"
# The tarball's top-level directory is always `quickjs-master`, so nothing needs
# moving unless the caller asked for another name.
if [ "$DEST" != "$PARENT/quickjs-master" ]; then
    mv "$PARENT/quickjs-master" "$DEST"
fi
: >"$LOG"

echo "building reference qjsc, then qjs (-j$NPROC, ~1 min)"
# Two invocations rather than one: `qjs` links `repl.o`, and `repl.c` is
# *generated* by `qjsc` (`repl.c: $(QJSC) repl.js`). Naming both goals on one
# command line makes the generator a sibling of the target that consumes it.
build "" qjsc
build "" qjs

# `qjsc-dump` is the bytecode oracle: the disassembler is a compile-time option
# in the reference implementation, so the compiler has to be rebuilt to get it.
if [ ! -x "$DEST/qjsc-dump" ]; then
    echo "building the bytecode-disassembling qjsc (second build, ~1 min)"
    clean
    build "-DDUMP_BYTECODE=1" qjsc
    cp "$DEST/qjsc" "$DEST/qjsc-dump"
    # Rebuild the plain pair, so `qjsc` stays the parse-only oracle that
    # `quickjs-parse`'s tests compare against.
    clean
    build "" qjsc
    build "" qjs
fi

echo "oracle ready:"
echo "  $DEST/qjs         reference interpreter"
echo "  $DEST/qjsc        parse-only oracle (exit status = accept/reject)"
echo "  $DEST/qjsc-dump   bytecode disassembler (compile oracle)"
"$DEST/qjs" -e 'print("QuickJS", typeof BigInt, typeof Iterator)'
