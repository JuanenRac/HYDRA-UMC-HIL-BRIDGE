# Simulation bench

The bridge can be exercised end to end without a controller, a robot or a twin service. The simulated path replaces three things: the real sink (a controller is never
reached), the transport (`SimulatedTransport` in `src/bridge.rs`, which can be disconnected or slower than its delivery budget) and the twin (a risk report is
supplied on the command line or by a test). It proves the routing, the safety interlock that fails closed, the certificate and zone checks, the HTTP access rules
and the failure paths; it says nothing about a real controller, a real network or real timing.

## Run it

```
cargo test --locked --all-targets
cargo build --locked
target/debug/hydra-umc-hil-bridge route --mode real --joint j1 --position 0.5 --transport-latency-ms 500 --transport-timeout-ms 100
```

The workflow named "Simulation bench" does this on a clean Linux runner with the Rust 1.85.1 toolchain on every push and pull request, by hand and once a week.
Cargo runs only there (or in the build queue of the maintainer), never as a heavy build on a developer PC.

## What it runs

| Step | What it exercises |
|------|-------------------|
| `cargo test --locked --all-targets` | the unit tests of every module: the interlock, the simulated transport, the certificate check, the safety zones, the collision shadow, the limits, the audit record, the access rules and the HTTP server |
| `route --mode simulation` | a command always reaches the simulated sink (exit 0) |
| `route --mode real --clear-distance 2.0` | the interlock allows a command when the twin report says the path is clear (exit 0) |
| `route --mode real` without a report | blocked: not being told about a risk is not the same as there being none (exit 1) |
| `route --mode real --collision-risk --distance 0.02` | blocked on an imminent collision (exit 1) |
| `route ... --transport-disconnected`, `--transport-latency-ms` over `--transport-timeout-ms` | the command is allowed but never reported as delivered (exit 3) |
| `route` without `--mode` | refused (exit 2) |

The exit codes are checked by the workflow, so the documented behaviour of the command line (`docs/CLI_REFERENCE.md`) cannot drift from the code.

## What it cannot show

A real HYDRA-UMC controller on the other side, a real gRPC or WebSocket transport (not wired in yet), the real HYDRA-UMC-TWIN service and its risk reports, and the
timing of a real network. The hardware row of the status table therefore stays at "simulated" until a person has run the bridge on a real setup.
