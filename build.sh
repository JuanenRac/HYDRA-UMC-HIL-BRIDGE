#!/usr/bin/env bash
# =============================================================================
# HYDRA-UMC-HIL-BRIDGE - build.sh
# Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
# GPL-3.0 - see LICENSE
# =============================================================================
# Bumps the odometer version, runs the real test suite, and builds a
# release binary. Run this before run.sh whenever the source changes.
set -euo pipefail
cd "$(dirname "$0")"

# Keep the window open if this was double-clicked (e.g. from a file
# manager) instead of run from an already-open terminal - fires on
# success AND on a `set -e` early exit alike, but only prompts when
# stdin is actually a terminal (never in CI/piped/non-interactive runs).
trap '[ -t 0 ] && read -r -p "Press Enter to close..." _' EXIT

echo "== HYDRA-UMC-HIL-BRIDGE :: build =="

echo "-- Odometer version bump --"
python3 bump_version.py 2>/dev/null || python bump_version.py
python3 bump_manifest_version.py --sync 2>/dev/null || python bump_manifest_version.py --sync

echo "-- cargo test --"
cargo test

echo "-- cargo build --release --"
cargo build --release

echo "== Build OK =="
