#!/usr/bin/env bash
# Generate sample inputs with ffmpeg: a 10 s test video and a 60-frame PNG sequence.
# Existing files are kept, so this is safe to run on every `docker compose up`.
#
# Usage: scripts/gen_sample.sh [OUT_DIR]   (default: ./samples)
set -euo pipefail

out="${1:-$(cd "$(dirname "$0")/.." && pwd)/samples}"
mkdir -p "$out/frames"

if [ ! -f "$out/sample.mp4" ]; then
  ffmpeg -hide_banner -loglevel error -y \
    -f lavfi -i "testsrc2=size=1280x720:rate=30:duration=10" \
    -c:v libx264 -pix_fmt yuv420p "$out/sample.mp4.tmp.mp4"
  mv "$out/sample.mp4.tmp.mp4" "$out/sample.mp4"
  echo "wrote $out/sample.mp4"
fi

if [ -z "$(ls -A "$out/frames")" ]; then
  ffmpeg -hide_banner -loglevel error -y \
    -f lavfi -i "mandelbrot=size=640x480:rate=30" -frames:v 60 \
    "$out/frames/%04d.png"
  echo "wrote $out/frames/"
fi
