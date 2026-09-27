#!/bin/sh
# macOS release artifacts in dist/: a universal (arm64 + x86_64) ad-hoc signed litty.app in a dmg,
# and per-target tarballs of the bare binary (used by `cargo binstall` and the Homebrew formula).
set -e
cd "$(dirname "$0")/.."
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null 2>&1 || true
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin
rm -rf dist && mkdir -p dist/litty.app/Contents/MacOS dist/litty.app/Contents/Resources dist/icon.iconset
lipo -create -output dist/litty.app/Contents/MacOS/litty target/aarch64-apple-darwin/release/litty target/x86_64-apple-darwin/release/litty
sed "s/@VERSION@/$VERSION/g" packaging/Info.plist > dist/litty.app/Contents/Info.plist
for s in 16 32 128 256 512; do
  sips -z $s $s packaging/litty.png --out dist/icon.iconset/icon_${s}x${s}.png >/dev/null
  sips -z $((s * 2)) $((s * 2)) packaging/litty.png --out dist/icon.iconset/icon_${s}x${s}@2x.png >/dev/null
done
iconutil -c icns dist/icon.iconset -o dist/litty.app/Contents/Resources/AppIcon.icns
tic -x -o dist/litty.app/Contents/Resources/terminfo packaging/litty.terminfo
codesign --force --deep -s - dist/litty.app
mkdir dist/dmg && cp -R dist/litty.app dist/dmg/ && ln -s /Applications dist/dmg/Applications
hdiutil create -quiet -volname "litty" -srcfolder dist/dmg -ov -format UDZO "dist/litty-$VERSION-macos-universal.dmg"
for t in aarch64-apple-darwin x86_64-apple-darwin; do
  d="litty-$VERSION-$t"
  # One architecture per tarball (the universal binary is only for the app).
  mkdir "dist/$d" && cp "target/$t/release/litty" README.md LICENSE packaging/litty.terminfo "dist/$d/"
  codesign --force -s - "dist/$d/litty"
  tar -C dist -czf "dist/$d.tar.gz" "$d"
  rm -rf "dist/$d"
done
rm -rf dist/dmg dist/icon.iconset
ls dist
