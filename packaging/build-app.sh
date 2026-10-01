#!/bin/sh
# macOS release artifacts in dist/: a universal (arm64 + x86_64) litty.app in a dmg, and
# per-target tarballs of the bare binary (used by `cargo binstall` and the Homebrew formula).
# Signed ad hoc, or with SIGN_ID (a "Developer ID Application" identity in the keychain); with
# NOTARY_APPLE_ID, NOTARY_TEAM_ID and NOTARY_PASSWORD too, the dmg is notarized and stapled.
set -e
sign() {
  if [ -n "$SIGN_ID" ]; then
    codesign --force --options runtime --timestamp -s "$SIGN_ID" "$@"
  else
    codesign --force -s - "$@"
  fi
}
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
# Ad hoc, pin the designated requirement to the bundle id so macOS privacy grants (Screen
# Recording, Files, ...) survive rebuilds; the default requirement is the binary's hash.
if [ -n "$SIGN_ID" ]; then sign dist/litty.app
else sign -r='designated => identifier "dev.litty.app"' dist/litty.app; fi
mkdir dist/dmg && cp -R dist/litty.app dist/dmg/ && ln -s /Applications dist/dmg/Applications
DMG="dist/litty-$VERSION-macos-universal.dmg"
hdiutil create -quiet -volname "litty" -srcfolder dist/dmg -ov -format UDZO "$DMG"
if [ -n "$SIGN_ID" ]; then
  sign "$DMG"
  if [ -n "$NOTARY_APPLE_ID" ]; then
    xcrun notarytool submit "$DMG" --apple-id "$NOTARY_APPLE_ID" --team-id "$NOTARY_TEAM_ID" --password "$NOTARY_PASSWORD" --wait
    xcrun stapler staple "$DMG"
  fi
fi
for t in aarch64-apple-darwin x86_64-apple-darwin; do
  d="litty-$VERSION-$t"
  # One architecture per tarball (the universal binary is only for the app).
  mkdir "dist/$d" && cp "target/$t/release/litty" README.md LICENSE packaging/litty.terminfo "dist/$d/"
  sign "dist/$d/litty"
  tar -C dist -czf "dist/$d.tar.gz" "$d"
  rm -rf "dist/$d"
done
rm -rf dist/dmg dist/icon.iconset
ls dist
