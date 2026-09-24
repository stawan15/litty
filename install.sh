#!/bin/sh
# Installs the latest litty: curl -fsSL https://raw.githubusercontent.com/stawan15/litty/master/install.sh | sh
set -eu
REPO=stawan15/litty
tag=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")
tag=${tag##*/}
ver=${tag#v}
base="https://github.com/$REPO/releases/download/$tag"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

case "$(uname -s)" in
Darwin)
  curl -fsSL "$base/litty-$ver-macos-universal.dmg" -o "$tmp/litty.dmg"
  hdiutil attach -nobrowse -quiet -mountpoint "$tmp/mnt" "$tmp/litty.dmg"
  dest=/Applications
  [ -w "$dest" ] || { dest="$HOME/Applications"; mkdir -p "$dest"; }
  rm -rf "$dest/litty.app"
  cp -R "$tmp/mnt/litty.app" "$dest/"
  hdiutil detach -quiet "$tmp/mnt"
  echo "Installed $dest/litty.app"
  ;;
Linux)
  case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
  esac
  name="litty-$ver-$arch-unknown-linux-gnu"
  curl -fsSL "$base/$name.tar.gz" | tar -xz -C "$tmp"
  install -Dm755 "$tmp/$name/litty" "$HOME/.local/bin/litty"
  install -Dm644 "$tmp/$name/litty.desktop" "$HOME/.local/share/applications/litty.desktop"
  install -Dm644 "$tmp/$name/litty.png" "$HOME/.local/share/icons/hicolor/512x512/apps/litty.png"
  echo "Installed ~/.local/bin/litty (make sure ~/.local/bin is in your PATH)"
  ;;
*)
  echo "unsupported OS: $(uname -s)" >&2
  exit 1
  ;;
esac
