#!/usr/bin/env bash
# Run a Docker demo with its end-to-end check (docker/e2e) and tear it down again.
# On failure, the services' logs are written to ./e2e-logs/<demo>.log.
#
# Usage: scripts/e2e.sh sample|teleop [docker compose up flags, e.g. --build]
#   CODEC=av1 scripts/e2e.sh sample
set -euo pipefail

demo="${1:?usage: $0 sample|teleop [up flags]}"
shift
root="$(cd "$(dirname "$0")/.." && pwd)"
case "$demo" in
  sample) base="$root/docker/compose.yml" ;;
  teleop) base="$root/docker/teleop.compose.yml" ;;
  *) echo "unknown demo: $demo" >&2; exit 2 ;;
esac
compose=(docker compose -f "$base" -f "$root/docker/e2e/$demo.compose.yml")

cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then
    mkdir -p "$root/e2e-logs"
    "${compose[@]}" --profile e2e logs --no-color --timestamps >"$root/e2e-logs/$demo${CODEC:+-$CODEC}.log" 2>&1 || true
    echo "logs: e2e-logs/$demo${CODEC:+-$CODEC}.log" >&2
  fi
  "${compose[@]}" --profile e2e down --volumes --remove-orphans >/dev/null 2>&1 || true
  exit "$status"
}
trap cleanup EXIT

"${compose[@]}" up --detach "$@"
"${compose[@]}" run --rm check
