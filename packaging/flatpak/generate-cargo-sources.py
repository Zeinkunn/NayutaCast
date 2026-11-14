#!/usr/bin/env python3
"""Generate offline Cargo Flatpak sources from Cargo.lock using Python stdlib."""

import argparse
import json
import sys
from pathlib import Path

try:
    import tomllib  # Python 3.11+
except ImportError:  # pragma: no cover
    tomllib = None

CRATES_IO_DOWNLOAD = "https://static.crates.io/crates"
CARGO_HOME = "cargo"
CARGO_VENDOR = f"{CARGO_HOME}/vendor"


def _parse_cargo_lock_fallback(text: str) -> dict:
    """Minimal fallback parser for [[package]] entries on Python < 3.11."""
    packages: list[dict] = []
    metadata: dict[str, str] = {}
    current: dict | None = None
    in_metadata = False

    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        if line == "[[package]]":
            in_metadata = False
            if current is not None:
                packages.append(current)
            current = {}
            continue
        if line == "[metadata]":
            in_metadata = True
            if current is not None:
                packages.append(current)
                current = None
            continue
        if line.startswith("["):
            in_metadata = False
            continue
        if "=" not in line:
            continue
        key, val = line.split("=", 1)
        key = key.strip().strip('"')
        val = val.strip()
        if val.startswith('"') and val.endswith('"'):
            val_str = val[1:-1]
            if in_metadata:
                metadata[key] = val_str
            elif current is not None and key in ("name", "version", "source", "checksum"):
                current[key] = val_str

    if current is not None:
        packages.append(current)
    return {"package": packages, "metadata": metadata}


def load_cargo_lock(lock_path: Path) -> dict:
    """Load Cargo.lock using tomllib when available, or the fallback parser."""
    if tomllib is not None:
        with lock_path.open("rb") as f:
            return tomllib.load(f)
    return _parse_cargo_lock_fallback(lock_path.read_text(encoding="utf-8"))


def generate_cargo_sources(lock_path: Path) -> list[dict]:
    """Parse Cargo.lock and emit Flatpak archive/inline source definitions."""
    lock_data = load_cargo_lock(lock_path)
    metadata = lock_data.get("metadata", {})
    packages = lock_data.get("package", [])
    sources: list[dict] = []

    for pkg in sorted(packages, key=lambda p: (p.get("name", ""), p.get("version", ""))):
        src = pkg.get("source")
        if not src:
            continue
        name = pkg["name"]
        version = pkg["version"]

        if not src.startswith("registry+"):
            raise RuntimeError(
                f"Unsupported non-registry dependency source for {name} {version}: {src}"
            )

        checksum = pkg.get("checksum") or metadata.get(f"checksum {name} {version} ({src})")
        if not checksum:
            raise RuntimeError(f"Missing checksum in Cargo.lock for {name} {version}")

        dest = f"{CARGO_VENDOR}/{name}-{version}"
        sources.append(
            {
                "type": "archive",
                "archive-type": "tar-gzip",
                "url": f"{CRATES_IO_DOWNLOAD}/{name}/{name}-{version}.crate",
                "sha256": checksum,
                "dest": dest,
            }
        )
        sources.append(
            {
                "type": "inline",
                "contents": json.dumps({"package": checksum, "files": {}}),
                "dest": dest,
                "dest-filename": ".cargo-checksum.json",
            }
        )

    config_toml = (
        '[source.crates-io]\n'
        'replace-with = "vendored-sources"\n\n'
        '[source.vendored-sources]\n'
        f'directory = "{CARGO_VENDOR}"\n'
    )
    sources.append(
        {
            "type": "inline",
            "contents": config_toml,
            "dest": CARGO_HOME,
            "dest-filename": "config.toml",
        }
    )
    return sources


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("cargo_lock", type=Path, help="Path to Cargo.lock")
    parser.add_argument(
        "-o",
        "--output",
        type=Path,
        default=Path("cargo-sources.json"),
        help="Output JSON path (default: cargo-sources.json)",
    )
    args = parser.parse_args()

    sources = generate_cargo_sources(args.cargo_lock)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w", encoding="utf-8") as f:
        json.dump(sources, f, indent=2)
        f.write("\n")

    print(f"Generated {len(sources)} Flatpak cargo source entries at {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
