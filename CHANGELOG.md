# Changelog

All notable public work on **HYDRA-UMC-HIL-BRIDGE** is summarized here,
newest first. This changelog intentionally omits calendar dates and internal
work-session detail.

## Versioning scheme

`Cargo.toml`'s `version` field is bumped automatically by `bump_version.py`
(stdlib-only, no `cargo` plugin needed) before a real release build
(`cargo build --release`), invoked from `build.sh`/`build.bat`.

It follows the ecosystem-wide base-10 "odometer" rule rather than
semantic-versioning judgment calls:

- `PATCH` +1 on every build
- when `PATCH` would exceed 9, it resets to 0 and `MINOR` +1 instead (e.g. `0.0.9` -> `0.1.0`, never `0.0.10`)
- the same carry cascades into `MAJOR` if `MINOR` would exceed 9

---

## Unreleased - Simulated transport ordering evidence

- Added a deterministic test proving that commands confirmed by one local
  `SimulatedTransport` retain issue order. It deliberately does not claim a
  future network transport has that guarantee: a real transport must provide
  its own sequence and acknowledgement evidence before hardware validation.

## [0.0.3] - Real v0: fail-safe transport layer, testable without hardware

- **`bridge.rs`** - `CommandSink::send()` now returns `Result<(), TransportError>` instead of `()`. `TransportError` is `Timeout { after_ms }` or `Disconnected`. `RecordingSink` always succeeds (unchanged behavior); a new `SimulatedTransport` implementation models a slow (`latency_ms > timeout_ms`) or disconnected (`connected: false`) link with no real hardware involved.
- **`bridge.rs`** - `RouteOutcome` gained `TransportFailure { reason }`: `route_command()` and `mirror_command()` now propagate a transport error as this outcome rather than ever reporting `SentReal`/`SentSimulation` when delivery wasn't actually confirmed - the interlock's `Block` and a transport failure are two distinct reasons a command can fail to reach hardware, and both must stop the bridge from claiming success.
- **`main.rs`** - `route` gained `--transport-latency-ms`/`--transport-timeout-ms`/`--transport-disconnected` (all optional, default behavior unchanged) so the fail-safe path is exercisable from the CLI without any real hardware. New exit code 3 for `TransportFailure`.
- 7 new tests (healthy/disconnected/timeout `SimulatedTransport` behavior including the `latency_ms == timeout_ms` boundary, real-mode routing never reporting `SentReal` on timeout or disconnection, and interlock block taking precedence over transport state). 14 tests total.
- Real verification beyond the test suite: `route --transport-timeout-ms 100 --transport-latency-ms 500` and `--transport-disconnected` both correctly report `TRANSPORT FAILURE` with exit code 3, never `SENT`.

## [0.0.2] - Real v0 mode-based routing and safety interlock
### Added
- `protocol.rs` - real `JointCommand` and `Mode` (Real/Simulation) types.
- `interlock.rs` - `assess_interlock()`: the real code-level enforcement of the "Safety Interlock" feature this README already advertised - blocks a real-hardware-bound command whenever a `TwinRiskReport` says a collision is imminent, trusting the twin's own conclusion rather than re-deriving one from a raw distance threshold. Advisory-blocking only, in the same spirit as HYDRA-UMC-SAFETY-ZONES's detect-vs-enforce boundary: this bridge can refuse to forward a command, physical enforcement still belongs to the firmware alone.
- `bridge.rs` - `Bridge::route_command()`: real mode-based routing, interlock-gated only in `Real` mode (`Simulation` mode is never gated - the point of simulating is to see a predicted collision, not have it swallowed). `Bridge::mirror_command()`: unconditional real-vs-virtual shadowing. `CommandSink` trait + `RecordingSink`, the one real (non-transmitting, honestly so) implementation today - no gRPC/WebSocket transport exists yet.
- `main.rs` - two new real subcommands: `route --mode real|simulation --joint NAME --position VALUE [--collision-risk] [--distance METERS]` and `mirror --joint NAME --position VALUE`. Bare invocation unchanged.
- 7 new real tests covering interlock decisions (allow/block/trusting the twin's own flag) and routing (simulation always sent, real gated, blocked commands never reach the real sink, mirroring reaches its sink regardless of mode).
- Real verification beyond the test suite: ran `route`/`mirror` for all 4 real outcomes (real-mode sent, real-mode blocked, simulation-mode sent ungated despite risk, mirrored) and confirmed each printed message and exit code.

### Fixed
- `build.sh` called `bump_manifest_version.py` (no `--sync`) as its very first line, before also calling `bump_version.py` later - the same double-bump pattern found in other Rust projects this session. Rewritten to bump the native version first, then sync the manifest. `build.sh`/`build.bat` now also run `cargo test` and use the ecosystem's no-autoclose pattern for the first time in this project; `run.sh`/`run.bat` now forward arguments.

## [0.0.1] - Initial scaffolding

- **`src/main.rs`** - minimal real entry point. No bridge logic yet - the real-time link between HYDRA-UMC-TWIN's simulation and real hardware-in-the-loop I/O lands in a later pass.
- **`Cargo.toml`** - crate metadata, no runtime dependencies yet.
- **`build.sh` / `build.bat`**, **`run.sh` / `run.bat`** - `cargo build --release` and run the resulting binary.
