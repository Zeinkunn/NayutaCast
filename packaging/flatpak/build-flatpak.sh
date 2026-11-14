#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FLATPAK_DIR="${ROOT_DIR}/packaging/flatpak"
MANIFEST="${FLATPAK_DIR}/io.github.zeinkunn.NayutaCast.yml"
ARCH="${1:-$(uname -m)}"

python3 "${FLATPAK_DIR}/generate-cargo-sources.py" \
  "${ROOT_DIR}/Cargo.lock" \
  -o "${FLATPAK_DIR}/cargo-sources.json"

flatpak-builder \
  --force-clean \
  --user \
  --install-deps-from=flathub \
  --repo="${ROOT_DIR}/repo" \
  --arch="${ARCH}" \
  "${ROOT_DIR}/build-dir" \
  "${MANIFEST}"

flatpak build-bundle \
  "${ROOT_DIR}/repo" \
  "${ROOT_DIR}/nayutacast-linux-${ARCH}.flatpak" \
  io.github.zeinkunn.NayutaCast \
  --arch="${ARCH}"

echo "Built bundle: ${ROOT_DIR}/nayutacast-linux-${ARCH}.flatpak"
