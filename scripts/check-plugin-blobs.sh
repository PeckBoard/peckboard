#!/usr/bin/env bash
# Fail when a bundled plugin blob is older than its source.
#
# Plugins such as session-control live in their own repos (checked out
# under peck-plugins/<id>/, git-ignored here) but ship as a prebuilt blob in
# peck-plugins-wasm/<id>.wasm with its version in <id>.version. A fix landed
# in the plugin repo does nothing until the blob is rebuilt (its build.sh)
# and re-committed here — 0.1.45 shipped a stale session-control blob that
# auto-paused orchestrators. This compares each <id>.version with the
# version in the plugin's Cargo.toml when that checkout is present.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
status=0
for vfile in "$ROOT"/peck-plugins-wasm/*.version; do
  [[ -e "$vfile" ]] || continue
  id="$(basename "$vfile" .version)"
  manifest="$ROOT/peck-plugins/$id/Cargo.toml"
  if [[ ! -f "$manifest" ]]; then
    echo "skip $id: no source checkout at peck-plugins/$id"
    continue
  fi
  blob="$(tr -d '[:space:]' <"$vfile")"
  src="$(awk -F'"' '/^version[[:space:]]*=/ {print $2; exit}' "$manifest")"
  if [[ "$blob" != "$src" ]]; then
    echo "STALE $id: peck-plugins-wasm/$id.version is $blob but source is $src"
    echo "  rebuild: peck-plugins/$id/build.sh, copy the wasm to peck-plugins-wasm/$id.wasm, bump $id.version"
    status=1
  else
    echo "ok $id $blob"
  fi
done
exit $status
