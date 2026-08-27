@echo off
REM =============================================================================
REM HYDRA-UMC-HIL-BRIDGE - build.bat
REM Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
REM GPL-3.0 - see LICENSE
REM =============================================================================
setlocal
cd /d "%~dp0"

echo == HYDRA-UMC-HIL-BRIDGE :: build ==

echo -- Odometer version bump --
python bump_version.py
if errorlevel 1 ( echo NATIVE VERSION BUMP FAILED. & pause & exit /b 1 )
python "%~dp0bump_manifest_version.py" --sync
if errorlevel 1 ( echo VERSION SYNCHRONIZATION FAILED. & pause & exit /b 1 )
if errorlevel 1 goto :error

echo -- cargo test --
cargo test
if errorlevel 1 goto :error

echo -- cargo build --release --
cargo build --release
if errorlevel 1 goto :error

echo == Build OK ==
pause
exit /b 0

:error
echo == Build FAILED ==
pause
exit /b 1
