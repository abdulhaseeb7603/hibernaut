#!/bin/sh
# Installs ccnap for this user: downloads the latest release and runs `ccnap setup`.
#   curl -fsSL https://raw.githubusercontent.com/abdulhaseeb7603/ccnap/main/install.sh | sh
set -eu
repo=abdulhaseeb7603/ccnap
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-musl ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *) echo "ccnap: no prebuilt binary for $(uname -sm); build it with: cargo install --git https://github.com/$repo" >&2; exit 1 ;;
esac
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
curl -fsSL "https://github.com/$repo/releases/latest/download/ccnap-$target" -o "$tmp"
chmod +x "$tmp"
"$tmp" setup
