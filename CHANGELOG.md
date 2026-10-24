# Changelog

All notable changes to this project are documented here.

## [Unreleased]

- **A real command needs a risk certificate signed by the twin.** The report that gates a command in `real` mode is no longer the one the caller writes: the request carries `certificate`, an Ed25519 signature of HYDRA-UMC-TWIN over the report for exactly that joint and position (`src/certificate.rs`, `docs/INTERLOCK_CERTIFICATE.md`). The bridge holds only the public key (`--twin-public-key-file`) and blocks a certificate that is forged, for another command, older than `--certificate-max-age-ms` (default 2000), dated in the future or already used (the last 8192 nonces are remembered). With no key configured the real mode blocks every command and says why; `--allow-unsigned-risk` (bench use, announced at every start) believes the report of the caller again. Simulation mode is unchanged. `/stats` says whether the mode is certified and counts the refusals.
- **Every real order is accounted for.** Each one leaves an `order` record (the caller from `X-Hydra-Caller`, the peer address, whether the Bearer token was presented, the certificate key and nonce) before the interlock is asked and an `outcome` record after. `--audit-log FILE` appends them as JSON lines, and an order whose record cannot be written is refused with 503 and not forwarded; `GET /audit` lists the last 200. A test vector shared with HYDRA-UMC-TWIN and checked against an independent RFC 8032 implementation, a test against both real binaries by hand, and 26 new tests (86 in all).
- **Optional Bearer token for the HTTP API.** `serve --token-file FILE` (or `HYDRA_UMC_HIL_BRIDGE_TOKEN`, at least 16 characters) makes every request but `GET /stats` carry `Authorization: Bearer <token>` (constant-time comparison); `GET /stats` reports how many requests were refused. On loopback no token is needed; any other address is refused at start without one.
- The manifest declares the health route of the service (`/stats`), so the service registry, the ecosystem status of the server and the watchdog know where to probe it.
- Building no longer changes the version; raising it is the explicit `bump` command (`python tools/hydra_project_tool.py bump`). The build scripts only report the version they built, and CI fails if a build script runs a bump.
- The banner follows the animated style of the ecosystem banners, with the name, the kind and the technologies of the project.
- Added `tools/hydra_project_tool.py`: the same `validate`, `test`, `version`, `status` and `bump` commands in every repository. The checks run with `HYDRA_UMC_CI=1` and fail if the version changes; only `bump` changes it.

## [0.0.9] - Maturity lowered to functional

- Build version synchronized with `hydra-umc.project.json` and the repository-native version source.
- The licence is written as GPL-3.0-or-later in every header and in the package metadata.
- Maturity is functional: nothing here has been verified on real hardware yet.

## [0.0.8] - The HTTP API refuses browser-style requests and invalid commands

- `POST` requests must be declared as `Content-Type: application/json` (`415` otherwise) and are refused with `403` when they carry an `Origin` header: a web page could send a "simple" cross-site `POST` (`text/plain`, no preflight) to the loopback port and reach `/route`.
- A command is validated before it is routed: an empty or over-long joint name and a non-finite position (`NaN`, `inf`, `1e999`) are refused with `400` over HTTP and with exit code `2` on the CLI. The joint list, limits and units are still not known to the bridge.
- The simulated-transport fields of a `/route` request (`transport`) are refused with `400` unless the server is started with `--fault-injection`, so a deployed instance cannot be told to fake a link fault.
- A request body that is not valid UTF-8 is a `400` instead of a `413` (`413` stays for bodies over 16 KB).
- A known path with the wrong method (`GET /route`, `POST /stats`) answers `405` with an `Allow` header instead of `404`.
- CLI: a flag followed by another flag no longer takes it as its value (`--joint --position 1`), and a malformed `--distance` is an error instead of a silent `0.0`.
- `Cargo.lock` is now versioned so builds of this binary are reproducible; the manifest `build` command lost a stray trailing dot; the crate license is `GPL-3.0-or-later` like the rest of the family.
- `docs/CLI_REFERENCE.md`: fixed the unclosed code block in the real-mode examples, the stale version in the sample output and the `GET /stats` description; the README no longer lists a `build/` folder that does not exist, states every condition that blocks a real command, and counts 47 tests.
- **`GET /stats` now reports real process-lifetime counters** (new
  `Stats` struct in `server.rs`): it used to return a hardcoded static JSON
  body, since each request built a fresh `Bridge`/`RecordingSink` with
  nothing persistent to aggregate. Tracks real routed/blocked/transport-
  failure/mirror/malformed/not-found counts across the server's own
  request loop (single-threaded, so plain counters need no locking). New
  regression test drives one of each real outcome through `/route` and
  `/mirror`, then confirms `/stats` reflects every one of them.
- Added a deterministic test proving that commands confirmed by one local
  `SimulatedTransport` retain issue order. It deliberately does not claim a
  future network transport has that guarantee: a real transport must provide
  its own sequence and acknowledgement evidence before hardware validation.

## [0.0.7] - The interlock fails closed: no twin report, a NaN distance or under 5 cm of clearance block a real command

- In `real` mode a command without a twin risk report is now `BlockedByInterlock` ("no twin risk report"): the report used to default to "no collision" and the caller sends it in the same request, so omitting it was enough to reach the hardware. Simulation mode is unchanged.
- A NaN distance and less than 5 cm of clearance (`MIN_CLEARANCE_M`) also block, even when `collision_imminent` is false; the old test that allowed 1 mm of clearance now asserts the opposite.
- New `--clear-distance METERS` on `route` supplies a clear report; the HTTP `risk` field is required in real mode.
- Request bodies over 16 KB are refused with `413` instead of being read without a limit.
- 9 new tests (35 in total); `docs/CLI_REFERENCE.md`, the 7 READMEs and a Portuguese README say so.

## [0.0.6] - Simulated, blocked and failed-delivery outcomes are named everywhere

- Every routed command now reports a stable outcome kind (`sent_real`,
  `sent_simulation`, `blocked_by_interlock`, `transport_failure`) and whether it was
  delivered: as `X-Outcome-Kind` and `X-Delivered` headers on the HTTP route response
  and as one operational log line per command (mode, kind, delivery, reason). The JSON
  body is unchanged. Two new tests.

## [0.0.5]

- **Fixed CI**: `cargo fmt --check` was failing on `src/bridge.rs`/
  `src/server.rs` (unwrapped lines), and `cargo clippy -- -D warnings`
  was failing on `std::io::Error::new(ErrorKind::Other, e)` (now
  `std::io::Error::other`, clippy's own suggested idiom). No behavior
  change - `cargo test`: 23/23 passing throughout.

## [0.0.4] - Real v0: JSON/HTTP server mode, plus CM5 deployment

- **`protocol.rs`/`interlock.rs`/`bridge.rs`** - `JointCommand`, `Mode`,
  `TwinRiskReport`, `RouteOutcome` gained `Serialize`/`Deserialize`
  derives (behavior-preserving, additive only) so `server.rs` can hand
  them straight to `serde_json` without a second, parallel JSON shape.
  `Mode` serializes as lowercase (`"real"`/`"simulation"`), matching the
  CLI's own `--mode` values.
- **`server.rs`** (new) - `POST /route` and `POST /mirror` reach the
  exact same `Bridge::route_command()`/`Bridge::mirror_command()` the
  CLI's own `route`/`mirror` subcommands already run, over a real
  `tiny_http` server (blocking, no async runtime - same convention as
  `HYDRA-UMC-TWIN`'s own `server.rs`). No real gRPC/WebSocket transport
  exists yet (see `Cargo.toml`), so both routes still only ever reach
  `RecordingSink`/`SimulatedTransport`, the same honest fakes the CLI
  already uses - this closes the "only reachable as a one-shot CLI" gap,
  not that still-deferred transport question. Real gap this closes:
  this project's own routing/interlock decision logic was only ever
  reachable as a one-shot CLI.
- **`main.rs`** - new `serve` subcommand (`--addr`/`--port`, default
  `127.0.0.1:8113`).
- **`systemd/hydra-umc-hil-bridge.service`** (new) - loopback-only unit
  for `HYDRA-UMC-OS/provisioning/install_hil_bridge.sh` (new, that
  repo), compiled as a release binary, same pattern as
  `install_twin.sh`.
- 8 new tests (`server.rs`'s own `#[cfg(test)]` module, real end-to-end
  HTTP over a raw `TcpStream`) - 23 total.

## [0.0.3] - Real v0: fail-safe transport layer, testable without hardware

- **`bridge.rs`** - `CommandSink::send()` now returns `Result<(), TransportError>` instead of `()`. `TransportError` is `Timeout { after_ms }` or `Disconnected`. `RecordingSink` always succeeds (unchanged behavior); a new `SimulatedTransport` implementation models a slow (`latency_ms > timeout_ms`) or disconnected (`connected: false`) link with no real hardware involved.
- **`bridge.rs`** - `RouteOutcome` gained `TransportFailure { reason }`: `route_command()` and `mirror_command()` now propagate a transport error as this outcome rather than ever reporting `SentReal`/`SentSimulation` when delivery wasn't actually confirmed - the interlock's `Block` and a transport failure are two distinct reasons a command can fail to reach hardware, and both must stop the bridge from claiming success.
- **`main.rs`** - `route` gained `--transport-latency-ms`/`--transport-timeout-ms`/`--transport-disconnected` (all optional, default behavior unchanged) so the fail-safe path is exercisable from the CLI without any real hardware. New exit code 3 for `TransportFailure`.
- 7 new tests (healthy/disconnected/timeout `SimulatedTransport` behavior including the `latency_ms == timeout_ms` boundary, real-mode routing never reporting `SentReal` on timeout or disconnection, and interlock block taking precedence over transport state). 14 tests total.
- Real verification beyond the test suite: `route --transport-timeout-ms 100 --transport-latency-ms 500` and `--transport-disconnected` both correctly report `TRANSPORT FAILURE` with exit code 3, never `SENT`.

## [0.0.2] - Real v0 mode-based routing and safety interlock
- `protocol.rs` - real `JointCommand` and `Mode` (Real/Simulation) types.
- `interlock.rs` - `assess_interlock()`: the real code-level enforcement of the "Safety Interlock" feature this README already advertised - blocks a real-hardware-bound command whenever a `TwinRiskReport` says a collision is imminent, trusting the twin's own conclusion rather than re-deriving one from a raw distance threshold. Advisory-blocking only, in the same spirit as HYDRA-UMC-SAFETY-ZONES's detect-vs-enforce boundary: this bridge can refuse to forward a command, physical enforcement still belongs to the firmware alone.
- `bridge.rs` - `Bridge::route_command()`: real mode-based routing, interlock-gated only in `Real` mode (`Simulation` mode is never gated - the point of simulating is to see a predicted collision, not have it swallowed). `Bridge::mirror_command()`: unconditional real-vs-virtual shadowing. `CommandSink` trait + `RecordingSink`, the one real (non-transmitting, honestly so) implementation today - no gRPC/WebSocket transport exists yet.
- `main.rs` - two new real subcommands: `route --mode real|simulation --joint NAME --position VALUE [--collision-risk] [--distance METERS]` and `mirror --joint NAME --position VALUE`. Bare invocation unchanged.
- 7 new real tests covering interlock decisions (allow/block/trusting the twin's own flag) and routing (simulation always sent, real gated, blocked commands never reach the real sink, mirroring reaches its sink regardless of mode).
- Real verification beyond the test suite: ran `route`/`mirror` for all 4 real outcomes (real-mode sent, real-mode blocked, simulation-mode sent ungated despite risk, mirrored) and confirmed each printed message and exit code.

- `build.sh` called `bump_manifest_version.py` (no `--sync`) as its very first line, before also calling `bump_version.py` later - the same double-bump pattern seen in other Rust projects in the ecosystem. Rewritten to bump the native version first, then sync the manifest. `build.sh`/`build.bat` now also run `cargo test` and use the ecosystem's no-autoclose pattern for the first time in this project; `run.sh`/`run.bat` now forward arguments.

## [0.0.1] - Initial scaffolding

- **`src/main.rs`** - minimal real entry point. No bridge logic yet - the real-time link between HYDRA-UMC-TWIN's simulation and real hardware-in-the-loop I/O lands in a later pass.
- **`Cargo.toml`** - crate metadata, no runtime dependencies yet.
- **`build.sh` / `build.bat`**, **`run.sh` / `run.bat`** - `cargo build --release` and run the resulting binary.
