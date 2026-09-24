#!/bin/sh
# Fills the package-manager templates from a release's SHA256SUMS: render.sh VERSION SHA256SUMS OUTDIR
set -e
VERSION=$1 SUMS=$2 OUT=$3
cd "$(dirname "$0")/templates"
sha() { awk -v f="$1" '$2 == f { print $1 }' "$SUMS"; }
DMG=$(sha "litty-$VERSION-macos-universal.dmg")
LX=$(sha "litty-$VERSION-x86_64-unknown-linux-gnu.tar.gz")
LA=$(sha "litty-$VERSION-aarch64-unknown-linux-gnu.tar.gz")
[ -n "$DMG" ] && [ -n "$LX" ] && [ -n "$LA" ] || { echo "missing checksum in $SUMS" >&2; exit 1; }
mkdir -p "$OUT"
for f in cask.rb formula.rb PKGBUILD; do
  sed "s/@VERSION@/$VERSION/g; s/@DMG_SHA@/$DMG/g; s/@LINUX_X86_64_SHA@/$LX/g; s/@LINUX_AARCH64_SHA@/$LA/g" "$f" > "$OUT/$f"
done
