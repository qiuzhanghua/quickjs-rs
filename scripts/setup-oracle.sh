#!/bin/sh
# Fetch and build the C reference implementation of QuickJS, used as the
# differential-test oracle by `quickjs-conformance`.
#
#   ./scripts/setup-oracle.sh [target-dir]
#
# Default target directory: /tmp/quickjs-master (one of the paths searched by
# quickjs_conformance::locate_oracle, override with $QJS_ORACLE).
set -eu

DEST="${1:-/tmp/quickjs-master}"
TARBALL="$(dirname "$DEST")/quickjs-master.tar.gz"

if [ -x "$DEST/qjs" ] && [ -x "$DEST/qjsc" ] && [ -x "$DEST/qjsc-dump" ]; then
    echo "oracle already built: $DEST/qjs ($("$DEST/qjs" --help | head -1))"
    exit 0
fi

echo "downloading bellard/quickjs master -> $TARBALL"
curl -fsSL -o "$TARBALL" https://codeload.github.com/bellard/quickjs/tar.gz/refs/heads/master

rm -rf "$DEST.old"
if [ -d "$DEST" ]; then mv "$DEST" "$DEST.old"; fi
tar xzf "$TARBALL" -C "$(dirname "$DEST")"
mv "$(dirname "$DEST")/quickjs-master" "$DEST" 2>/dev/null || true

NPROC="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
echo "building reference qjs + qjsc (clang -O2, ~1 min)"
make -C "$DEST" -j"$NPROC" qjs qjsc

# `qjsc-dump` is the bytecode oracle: the disassembler is a compile-time option
# in the reference implementation, so the compiler has to be rebuilt with it.
# The macro is passed through CFLAGS because the Makefile extends DEFINES itself.
if [ ! -x "$DEST/qjsc-dump" ]; then
    echo "building the bytecode-disassembling qjsc (second build, ~1 min)"
    BASE_CFLAGS="-g -Wall -Wextra -Wno-sign-compare -Wno-missing-field-initializers \
-Wundef -Wuninitialized -Wunused -Wno-unused-parameter -Wwrite-strings \
-Wchar-subscripts -funsigned-char -fwrapv -D_GNU_SOURCE \
-DCONFIG_VERSION=\"$(cat "$DEST/VERSION")\" -DDUMP_BYTECODE=1"
    make -C "$DEST" clean >/dev/null 2>&1 || true
    make -C "$DEST" -j"$NPROC" qjsc CFLAGS="$BASE_CFLAGS" >/dev/null
    cp "$DEST/qjsc" "$DEST/qjsc-dump"
    # Rebuild the plain compiler, so `qjsc` stays the parse-only oracle that
    # `quickjs-parse`'s tests compare against.
    make -C "$DEST" clean >/dev/null 2>&1 || true
    make -C "$DEST" -j"$NPROC" qjs qjsc >/dev/null
fi

echo "oracle ready:"
echo "  $DEST/qjs         reference interpreter"
echo "  $DEST/qjsc        parse-only oracle (exit status = accept/reject)"
echo "  $DEST/qjsc-dump   bytecode disassembler (compile oracle)"
"$DEST/qjs" -e 'print("QuickJS", typeof BigInt, typeof Iterator)' 
