#!/usr/bin/env bash
# Build release peckboard-agent binaries locally.
#
# CI (.github/workflows/build-agent.yml) builds every v1 target on its own
# native runner. This helper is for local builds: it compiles for the host
# target by default, or for any target triples you pass as arguments, and
# stages named binaries + SHA-256 checksums into peckboard-agent/dist/.
#
# Cross-target builds require the rustup target and the matching linker/C
# toolchain installed — the aws-lc-rs TLS backend compiles C, and on Linux
# the screenshot backend links libxcb (which is also why the Linux release
# targets are gnu, not musl — libxcb has no static musl build). For all
# five release targets, prefer the CI workflow.
#
# Usage:
#   ./build-release.sh                              # host target
#   ./build-release.sh x86_64-unknown-linux-gnu     # one explicit target
#   ./build-release.sh aarch64-apple-darwin x86_64-apple-darwin
set -euo pipefail

cd "$(dirname "$0")"
DIST="dist"
mkdir -p "$DIST"

# Map a target triple to a human-friendly asset name.
asset_name() {
  case "$1" in
    aarch64-apple-darwin)         echo "peckboard-agent-macos-arm64" ;;
    x86_64-apple-darwin)          echo "peckboard-agent-macos-x86_64" ;;
    x86_64-unknown-linux-gnu)     echo "peckboard-agent-linux-x86_64" ;;
    aarch64-unknown-linux-gnu)    echo "peckboard-agent-linux-arm64" ;;
    x86_64-pc-windows-msvc)       echo "peckboard-agent-windows-x86_64.exe" ;;
    *)                            echo "peckboard-agent-$1" ;;
  esac
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" > "$1.sha256"
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" > "$1.sha256"
  else echo "warn: no sha256 tool found, skipping checksum for $1" >&2; fi
}

# Default to the host target if none given.
if [ "$#" -eq 0 ]; then
  HOST="$(rustc -vV | sed -n 's/^host: //p')"
  set -- "$HOST"
fi

for target in "$@"; do
  echo ">> building $target"
  # Windows targets carry a .exe suffix; everything else has none.
  case "$target" in
    *windows*) binfile="peckboard-agent.exe" ;;
    *)         binfile="peckboard-agent" ;;
  esac

  cargo build --release --manifest-path Cargo.toml --target "$target"

  asset="$(asset_name "$target")"
  cp "target/$target/release/$binfile" "$DIST/$asset"
  ( cd "$DIST" && sha256 "$asset" )
  echo ">> staged $DIST/$asset"
done

echo "done -> $DIST/"
ls -l "$DIST"
