#!/bin/sh
# Installs hibernaut for this user: downloads the latest release and runs `hibernaut setup`.
#   curl -fsSL https://raw.githubusercontent.com/abdulhaseeb7603/hibernaut/main/install.sh | sh
set -eu
repo=abdulhaseeb7603/hibernaut
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *) echo "hibernaut: no prebuilt binary for $(uname -sm); build it with: cargo install --git https://github.com/$repo" >&2; exit 1 ;;
esac
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
base="https://github.com/$repo/releases/latest/download"
curl -fsSL "$base/hibernaut-$target" -o "$dir/hibernaut"
# The tray icon, on macOS; `hibernaut setup` installs it from next to hibernaut.
case "$target" in
  *apple*) curl -fsSL "$base/hibernaut-tray-$target" -o "$dir/hibernaut-tray" && chmod +x "$dir/hibernaut-tray" ;;
esac
chmod +x "$dir/hibernaut"
"$dir/hibernaut" setup
