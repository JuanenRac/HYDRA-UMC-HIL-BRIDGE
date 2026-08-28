# HYDRA-UMC-HIL-BRIDGE — CLI Reference

`hydra-umc-hil-bridge` is a single Rust binary (`src/main.rs`) with hand-parsed
`route`/`mirror` subcommands. It routes one `JointCommand` at a time through
the bridge's real safety-interlock and (optionally) simulated-transport
logic — see `src/interlock.rs` and `src/bridge.rs` for what "real" means
here. Every example below was captured from a real, built release binary —
the output shown is real, not illustrative.

## Usage

```
$ ./run.sh route --mode real --joint j1 --position 0.5
```

`run.sh` forwards all arguments unchanged to
`target/release/hydra-umc-hil-bridge`. The examples below invoke the release
binary directly instead, which is equivalent.

Bare invocation (no arguments) prints identity/version/role and exits `0`:

```
$ hydra-umc-hil-bridge
HYDRA-UMC-HIL-BRIDGE v0.0.3
Hardware-in-the-loop interface for real-vs-virtual command syncing with the Digital Twin.
```

## Commands

### `route --mode <real|simulation> --joint NAME --position VALUE [--collision-risk --distance M] [--transport-disconnected | --transport-latency-ms N --transport-timeout-ms N]`

Routes one joint command according to `--mode`:

- `simulation` mode always reaches the simulated sink, ungated by the
  interlock.
- `real` mode is only forwarded to the real sink if the safety interlock
  allows it. `--collision-risk` (with `--distance METERS`) simulates a
  `TwinRiskReport` with `collision_imminent: true`; without it, `route`
  behaves as if no twin risk was reported at all (an honest "allow" default,
  not a bypass — see `interlock.rs`'s own docs).
- `--transport-disconnected`, or either of
  `--transport-latency-ms`/`--transport-timeout-ms`, swaps the
  always-succeeds sink for a `SimulatedTransport` that can actually miss its
  delivery budget — exercising the fail-safe transport path without any real
  gRPC/WebSocket transport (not wired in yet).

**Simulation mode** — always reaches the twin, exit `0`:

```
$ hydra-umc-hil-bridge route --mode simulation --joint j1 --position 0.5
SENT: routed to HYDRA-UMC-TWIN (simulation)
```

**Real mode, no risk reported** — interlock allows, healthy sink, exit `0`:

```
$ hydra-umc-hil-bridge route --mode real --joint j1 --position 0.5
SENT: routed to real HYDRA-UMC controller
```

**Real mode, imminent collision** — the safety interlock blocks the command
before it ever reaches the real sink (exit `1`):

```
$ hydra-umc-hil-bridge route --mode real --joint j1 --position 0.5 --collision-risk --distance 0.02
BLOCKED: safety interlock refused to forward to real hardware (twin reports imminent collision at 0.020m)
```

**Real mode, disconnected simulated transport** — interlock allows the
command, but the transport itself has no live connection; never reported as
delivered (exit `3`):

```
$ hydra-umc-hil-bridge route --mode real --joint j1 --position 0.5 --transport-disconnected
TRANSPORT FAILURE: command was not confirmed delivered (transport is disconnected)
```

**Real mode, transport timeout** — latency exceeds the configured timeout
budget (exit `3`):

```
$ hydra-umc-hil-bridge route --mode real --joint j1 --position 0.5 --transport-latency-ms 500 --transport-timeout-ms 100
TRANSPORT FAILURE: command was not confirmed delivered (transport timed out after 100ms)
```

**Missing `--mode`** (exit `2`):

```
$ hydra-umc-hil-bridge route --joint j1 --position 0.5
route: missing required --mode real|simulation
```

**Invalid `--mode` value** (exit `2`):

```
$ hydra-umc-hil-bridge route --mode bogus --joint j1 --position 0.5
route: --mode must be 'real' or 'simulation', got 'bogus'
```

**Missing `--joint`** (exit `2`):

```
$ hydra-umc-hil-bridge route --mode simulation --position 0.5
route: missing required --joint NAME
```

**Non-numeric `--position`** (exit `2`):

```
$ hydra-umc-hil-bridge route --mode simulation --joint j1 --position notanumber
route: 'notanumber' is not a valid number for --position
```

### `mirror --joint NAME --position VALUE`

Shadows one command onto the twin unconditionally — independent of `--mode`
(there is none for `mirror`) and never gated by the interlock, since
mirroring a real robot's motion into the twin (or a simulated one back out)
is observation, not actuation.

```
$ hydra-umc-hil-bridge mirror --joint j2 --position -1.25
MIRRORED: joint 'j2' = -1.250000 shadowed into the twin
```

`mirror` shares the same `--joint`/`--position` parsing errors as `route`
(missing flag or non-numeric value, exit `2`), and reports a transport
failure the same way `route` does if the shadow update isn't confirmed
delivered (exit `3`) — `mirror` doesn't currently expose the
`--transport-*` flags to provoke that path from the CLI, but the underlying
`bridge.mirror_command` returns the same `Result` that `route`'s handling
above demonstrates.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | ok — command reached its sink (`SentReal`/`SentSimulation`), or `mirror` delivered |
| `1` | `route` in real mode: the safety interlock blocked the command |
| `2` | usage error — missing/malformed `--mode`, `--joint`, or `--position` |
| `3` | transport failure — the interlock allowed the command, but delivery to the sink was not confirmed (disconnected or timed out) |

## Not yet wired in

There is no real gRPC/WebSocket transport yet (see `Cargo.toml`) — every
`route`/`mirror` invocation above talks only to in-process `RecordingSink`/
`SimulatedTransport` stand-ins. The `--transport-*` flags on `route` exist
specifically so the bridge's fail-safe behavior (never reporting a dropped
command as delivered) is exercisable today, without any real hardware or
transport.
