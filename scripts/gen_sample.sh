#!/usr/bin/env bash
# Generate sample inputs with ffmpeg: a 10 s test video and a 60-frame PNG sequence.
set -euo pipefail

out="$(cd "$(dirname "$0")/.." && pwd)/samples"
mkdir -p "$out/frames"

ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i "testsrc2=size=1280x720:rate=30:duration=10" \
  -c:v libx264 -pix_fmt yuv420p "$out/sample.mp4"

ffmpeg -hide_banner -loglevel error -y \
  -f lavfi -i "mandelbrot=size=640x480:rate=30" -frames:v 60 \
  "$out/frames/%04d.png"

echo "wrote $out/sample.mp4 and $out/frames/"
