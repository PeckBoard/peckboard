#!/usr/bin/env bash
# Build Peckboard for production.
# 1. Builds the React frontend into web/dist
# 2. Compiles the Rust binary (which embeds web/dist via rust-embed)
set -e

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

echo "Building frontend..."
cd "$ROOT/web"
npm run build

echo "Building backend (release)..."
cd "$ROOT"
cargo build --release

echo ""
echo "Build complete: target/release/peckboard"
