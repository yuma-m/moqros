#!/usr/bin/env bash
# Download Cisco's prebuilt OpenH264 library for this platform (needed only for the
# `h264` feature). Cisco covers the H.264 patent royalties for this binary only when
# it is downloaded from Cisco by the end user, which is why moqros never bundles or
# compiles OpenH264 itself. See https://www.openh264.org/BINARY_LICENSE.txt
#
# Usage: scripts/fetch_openh264.sh [DEST_DIR]   (default: ./openh264)
# Then:  export OPENH264_LIBRARY=<printed path>
set -euo pipefail

version=2.6.0
dest="${1:-openh264}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)          file="libopenh264-${version}-mac-arm64.dylib"; sha=052e98bfcf7a9167d22f3bbb3f5988ef79065591f36af8b52924b22b13624551 ;;
  Darwin-x86_64)         file="libopenh264-${version}-mac-x64.dylib";   sha=e3dc8bc01fe69363f61fd3c02fd27798537a585eadd38cd808f303d1ee505a19 ;;
  Linux-x86_64)          file="libopenh264-${version}-linux64.8.so";    sha=2f0cde7c6a6abcf5cae76942894ea42897fa677bce4ed6c91a24dd1b041d5f04 ;;
  Linux-aarch64|Linux-arm64) file="libopenh264-${version}-linux-arm64.8.so"; sha=12e7b33623667cdab0e575170c147b1b36eadb77d0d2aa7ceb5afd3e58902140 ;;
  *) echo "unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

mkdir -p "$dest"
out="$dest/$file"
if [ ! -f "$out" ]; then
  curl -fsSL "http://ciscobinary.openh264.org/${file}.bz2" | bunzip2 > "$out.tmp"
  mv "$out.tmp" "$out"
fi

actual=$( (sha256sum "$out" 2>/dev/null || shasum -a 256 "$out") | cut -d' ' -f1)
if [ "$actual" != "$sha" ]; then
  echo "checksum mismatch for $out: $actual" >&2
  rm -f "$out"
  exit 1
fi

echo "$(cd "$(dirname "$out")" && pwd)/$(basename "$out")"
