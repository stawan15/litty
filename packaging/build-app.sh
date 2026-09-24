#!/bin/sh
# Builds dist/lite-term.app (ad-hoc signed) and dist/lite-term-VERSION-macos-arm64.dmg
set -e
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
cargo build --release
rm -rf dist && mkdir -p dist/lite-term.app/Contents/MacOS dist/lite-term.app/Contents/Resources dist/icon.iconset
cp target/release/lite-term dist/lite-term.app/Contents/MacOS/
sed "s/@VERSION@/$VERSION/g" packaging/Info.plist > dist/lite-term.app/Contents/Info.plist
swift packaging/icon.swift dist/icon.png
for s in 16 32 128 256 512; do
  sips -z $s $s dist/icon.png --out dist/icon.iconset/icon_${s}x${s}.png >/dev/null
  sips -z $((s * 2)) $((s * 2)) dist/icon.png --out dist/icon.iconset/icon_${s}x${s}@2x.png >/dev/null
done
iconutil -c icns dist/icon.iconset -o dist/lite-term.app/Contents/Resources/AppIcon.icns
codesign --force --deep -s - dist/lite-term.app
mkdir dist/dmg && cp -R dist/lite-term.app dist/dmg/ && ln -s /Applications dist/dmg/Applications
hdiutil create -quiet -volname "lite-term" -srcfolder dist/dmg -ov -format UDZO "dist/lite-term-$VERSION-macos-arm64.dmg"
rm -rf dist/dmg dist/icon.iconset dist/icon.png
echo "built dist/lite-term-$VERSION-macos-arm64.dmg"
