#!/usr/bin/env bash
# Refresh (or check) first-party provider seed catalogs from live CLIs.
# See scripts/refresh-provider-model-seeds.py for flags and behaviour.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
exec python3 "$ROOT/scripts/refresh-provider-model-seeds.py" "$@"
