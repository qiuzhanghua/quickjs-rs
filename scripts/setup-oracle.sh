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

if [ -x "$DEST/qjs" ]; then
    echo "oracle already built: $DEST/qjs ($("$DEST/qjs" --help | head -1))"
    exit 0
fi

echo "downloading bellard/quickjs master -> $TARBALL"
curl -fsSL -o "$TARBALL" https://codeload.github.com/bellard/quickjs/tar.gz/refs/heads/master

rm -rf "$DEST.old"
if [ -d "$DEST" ]; then mv "$DEST" "$DEST.old"; fi
tar xzf "$TARBALL" -C "$(dirname "$DEST")"
mv "$(dirname "$DEST")/quickjs-master" "$DEST" 2>/dev/null || true

echo "building reference qjs (clang -O2, ~1 min)"
make -C "$DEST" -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)" qjs qjsc
echo "oracle ready: $DEST/qjs"
"$DEST/qjs" -e 'print("QuickJS", typeof BigInt, typeof Iterator)' 
