# Contributing to HYDRA-UMC-HIL-BRIDGE 🦾

We welcome contributions to the hardware-in-the-loop bridge of the HYDRA-UMC platform.

## Technology Stack
- **Language**: Rust (2021 edition), built with `cargo`.
- **Dependencies**: `serde`/`serde_json` for the JSON contract, `tiny_http` for the blocking HTTP JSON surface. No async runtime.
- **Surface**: `route`/`mirror`/`serve` subcommands plus `POST /route`, `POST /mirror` and `GET /stats` over HTTP JSON.
- **Transport**: no real gRPC/WebSocket transport is wired in yet - commands reach an in-memory `RecordingSink` or a `SimulatedTransport`. See the comment in `Cargo.toml` for what gets added once real transport work starts.

## Guidelines
1. **Safety interlock**: any change to the simulation-to-real routing logic must keep the interlock impossible to bypass - a `route --mode real` command must stay blocked whenever the twin risk report says a collision is imminent.
2. **Fail-safe transport**: `CommandSink::send()` returns a `Result`; never let a transport failure be reported as a successful delivery, and keep `TransportFailure` distinct from `BlockedByInterlock`.
3. **Simulation mode is never gated** by the interlock - keep it that way.
4. **Testing**: run `cargo test --all-targets`, `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings`, plus `python tools/ci_validate.py`, before opening a pull request.
