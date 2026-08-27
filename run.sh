#!/usr/bin/env bash
# =============================================================================
# HYDRA-UMC-HIL-BRIDGE - run.sh
# Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
# GPL-3.0 - see LICENSE
# =============================================================================
# Forwards all arguments (e.g. "./run.sh route --mode real --joint j1 --position 0.5").
set -euo pipefail
cd "$(dirname "$0")"

BIN="target/release/hydra-umc-hil-bridge"
if [ ! -f "$BIN" ] && [ ! -f "${BIN}.exe" ]; then
  echo "No compiled binary found - run build.sh first." >&2
  exit 1
fi

if [ -f "${BIN}.exe" ]; then
  exec "${BIN}.exe" "$@"
else
  exec "$BIN" "$@"
fi
