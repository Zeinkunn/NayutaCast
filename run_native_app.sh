#!/usr/bin/env bash
set -e

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd)"
cd "$DIR"

export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-1}"
export DISPLAY="${DISPLAY:-:1}"
export RUST_LOG="${RUST_LOG:-info}"

echo "[*] Launching 100% Native Rust NayutaCast App..."
exec ./target/debug/nayutacast-app "$@"
