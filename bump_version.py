#!/usr/bin/env python3
# =============================================================================
# HYDRA-UMC-HIL-BRIDGE - bump_version.py
# Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
# GPL-3.0 - see LICENSE
#
# Applies the ecosystem-wide "odometer" version bump to this project's own
# Cargo.toml before every real build: PATCH goes up by 1; if that would push
# PATCH past 9, it resets to 0 and MINOR goes up by 1 instead (e.g.
# 0.0.9 -> 0.1.0), cascading the same carry from MINOR into MAJOR if MINOR
# also passes 9. MAJOR is otherwise never touched by this script - a
# deliberate manual-only decision, same convention across the ecosystem (see
# HYDRA-UMC-VISION-STREAMER/bump_version.py, which applies the identical rule
# to a pyproject.toml instead of a Cargo.toml).
#
# Called from build.sh/build.bat right before `cargo build`, so every real
# build carries a version 1 higher than the last real build. Also runs
# standalone (`python bump_version.py`), which is how it gets verified in
# isolation - stdlib only, no third-party imports.
# =============================================================================
from __future__ import annotations

import re
import sys
from pathlib import Path

CARGO_TOML = Path(__file__).resolve().parent / "Cargo.toml"
VERSION_RE = re.compile(r'^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"\s*$', re.MULTILINE)


def bump(major: int, minor: int, patch: int) -> tuple[int, int, int]:
    """Odometer-style carry: PATCH+1, rolling into MINOR past 9, and MINOR
    rolling into MAJOR past 9 in the same cascade."""
    patch += 1
    if patch > 9:
        patch = 0
        minor += 1
    if minor > 9:
        minor = 0
        major += 1
    return major, minor, patch


def main() -> int:
    if not CARGO_TOML.is_file():
        print(f"ERROR: {CARGO_TOML} does not exist.", file=sys.stderr)
        return 1

    text = CARGO_TOML.read_text(encoding="utf-8")
    match = VERSION_RE.search(text)
    if not match:
        print(f'ERROR: no version = "X.Y.Z" line found in {CARGO_TOML}', file=sys.stderr)
        return 1

    old = tuple(int(part) for part in match.groups())
    new = bump(*old)
    old_str = ".".join(str(part) for part in old)
    new_str = ".".join(str(part) for part in new)

    new_text = text[: match.start()] + f'version = "{new_str}"' + text[match.end():]
    CARGO_TOML.write_text(new_text, encoding="utf-8")

    print(f"Version bumped: {old_str} -> {new_str}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
