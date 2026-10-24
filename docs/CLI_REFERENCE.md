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
HYDRA-UMC-HIL-BRIDGE v0.0.7
Hardware-in-the-loop interface for real-vs-virtual command syncing with the Digital Twin.
```

## Commands

### `route --mode <real|simulation> --joint NAME --position VALUE [--collision-risk --distance M | --clear-distance M] [--transport-disconnected | --transport-latency-ms N --transport-timeout-ms N]`

Routes one joint command according to `--mode`:

- `simulation` mode always reaches the simulated sink, ungated by the
  interlock.
- `real` mode is only forwarded to the real sink if the safety interlock
  allows it, and the interlock **fails closed**: it needs a twin risk report
  that says the path is clear. `--clear-distance METERS` supplies one
  (`collision_imminent: false` with that clearance); `--collision-risk` (with
  `--distance METERS`) simulates a `TwinRiskReport` with `collision_imminent:
  true`. Without either, `route` is **blocked** ("no twin risk report"): not
  being told about a risk is not the same as there being none. A NaN distance
  and less than 5 cm of clearance (`MIN_CLEARANCE_M`) are blocked too, even
  when `collision_imminent` is false — see `interlock.rs`'s own docs.
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

**Real mode, twin report says clear** — interlock allows, healthy sink, exit `0`:

```
$ hydra-umc-hil-bridge route --mode real --joint j1 --position 0.5 --clear-distance 2.0
SENT: routed to real HYDRA-UMC controller
```

**Real mode, no report** — blocked, exit `1`:

```
$ hydra-umc-hil-bridge route --mode real --joint j1 --position 0.5
BLOCKED: safety interlock refused to forward to real hardware (no twin risk report: a real command is not forwarded without one)
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

### `serve [--addr ADDR] [--port PORT] [--fault-injection]`

Starts a real, blocking HTTP JSON server (`src/server.rs`, `tiny_http`, no
async runtime) that exposes the exact same `Bridge::route_command()`/
`Bridge::mirror_command()` logic the `route`/`mirror` subcommands run above
— this closes the "only reachable as a one-shot CLI invocation" gap, not
the still-deferred real gRPC/WebSocket transport question. `ADDR` defaults
to `127.0.0.1`, `PORT` to `8113`. This is the same binary the
`systemd/hydra-umc-hil-bridge.service` unit runs on a deployed CM5
(`hydra-umc-hil-bridge serve --addr 127.0.0.1 --port 8113`, loopback-only).

`--fault-injection` lets a `/route` request pick a simulated transport (the
`transport` object below). It is off by default: a server started without it
answers `400` to any request that carries a `transport` field, so a deployed
instance cannot be told to fake a link fault.

```
$ hydra-umc-hil-bridge serve --addr 127.0.0.1 --port 8113
[hil-bridge] HTTP API listening on 127.0.0.1:8113
[hil-bridge] POST /route, POST /mirror, GET /stats
```

**`POST /route`** — same fields as the CLI's `route` flags, as JSON:

```bash
$ curl -X POST http://127.0.0.1:8113/route \
    -H 'Content-Type: application/json' \
    -d '{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":true,"distance_m":0.02}}'
{"BlockedByInterlock":{"reason":"twin reports imminent collision at 0.020m"}}
```

Every `POST` must be declared as `Content-Type: application/json` (otherwise
`415`) and must not carry an `Origin` header (`403`): a web page can send a
"simple" cross-site `POST` to a loopback port without any preflight, and this
API has no authentication, so requests that look like they come from a browser
are refused. Local tools and services do not send `Origin`.

`mode` is `"real"` or `"simulation"`; `risk` mirrors `--collision-risk --distance`
(`collision_imminent: true`) or `--clear-distance` (`collision_imminent: false`) and is
**required in real mode**: a real command without one is `BlockedByInterlock` ("no twin risk
report"), and so is one with a NaN distance or under 5 cm of clearance; in simulation mode it
is optional. `transport` (optional object with
`disconnected`/`latency_ms`/`timeout_ms`, only with `--fault-injection`) mirrors
the CLI's `--transport-*` flags. The response body is the real `RouteOutcome` enum
serialized as JSON (`SentReal`, `SentSimulation`,
`BlockedByInterlock { reason }`, or `TransportFailure { reason }`) — always
HTTP `200` when the request itself was well-formed; `400` on malformed
JSON, on a body that is not valid UTF-8, on an empty or over-long `joint`, on
a `position` that is not a finite number, or on a `transport` field without
`--fault-injection`; `413` when the body is larger than 16 KB (it is not read
further). The bridge does not know the joint list, limits or units yet: it
only checks the shape of the values.

**`POST /mirror`** — `{"joint": "elbow", "position": -0.3}`, responds
`{"mirrored": true}` or `{"mirrored": false, "error": "..."}`, both `200`;
`400` on malformed JSON or an invalid `joint`/`position` (same checks as `/route`).

**`GET /stats`** — `200` with the process-lifetime counters: `role`,
`routedReal`, `routedSimulation`, `blockedByInterlock`, `transportFailures`,
`mirrored`, `mirrorFailures`, `malformedRequests` and `notFound`.

A known path with the wrong method (`GET /route`, `POST /stats`) is `405`
with an `Allow` header; any other path is `404`.
There is no authentication - same as every other loopback-only internal API
on the CM5 (see the systemd unit's own `RestrictAddressFamilies`/
`ProtectSystem` hardening for what it relies on instead).

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | ok — command reached its sink (`SentReal`/`SentSimulation`), or `mirror` delivered |
| `1` | `route` in real mode: the safety interlock blocked the command |
| `2` | usage error — missing/malformed `--mode`, `--joint`, or `--position` (a flag followed by another flag has no value; an empty joint or a non-finite position is refused) |
| `3` | transport failure — the interlock allowed the command, but delivery to the sink was not confirmed (disconnected or timed out) |

## Not yet wired in

There is no real gRPC/WebSocket transport yet (see `Cargo.toml`) — every
`route`/`mirror` invocation above talks only to in-process `RecordingSink`/
`SimulatedTransport` stand-ins. The `--transport-*` flags on `route` exist
specifically so the bridge's fail-safe behavior (never reporting a dropped
command as delivered) is exercisable today, without any real hardware or
transport.

## The certified real mode and the audit (`serve`)

`POST /route` in `real` mode no longer gates on a `risk` the caller wrote. It reads `certificate` (see `docs/INTERLOCK_CERTIFICATE.md`): the risk report of the
twin for exactly that joint and position, signed with the Ed25519 key of HYDRA-UMC-TWIN. A real command that has none, or whose certificate is forged, for another
command, too old, from the future or already used, answers `200` with `{"BlockedByInterlock": {"reason": "..."}}`; a `certificate` object that is not shaped like one is a `400`.
`mode: "simulation"` needs none.

| Flag | Environment | Meaning |
|---|---|---|
| `--twin-public-key-file FILE` | `HYDRA_UMC_HIL_BRIDGE_TWIN_KEY_FILE` | The trusted public key(s) of the twin, one 64-hex per line. Without one every real command is blocked. |
| `--certificate-max-age-ms N` | | Oldest certificate accepted, 1 to 60000 (default 2000). |
| `--allow-unsigned-risk` | `HYDRA_UMC_HIL_BRIDGE_ALLOW_UNSIGNED_RISK=1` | Bench setting: believe the `risk` the caller wrote, in real mode. Off by default; announced at every start. |
| `--audit-log FILE` | `HYDRA_UMC_HIL_BRIDGE_AUDIT_LOG` | Append every real order (`order` before it is decided, `outcome` after) as a JSON line; an order whose record cannot be written is refused with `503`. |

`GET /audit` (behind the Bearer token when there is one) answers `{"records": [...last 200...], "file": bool}`. A record has `at_ms`, `phase`, `caller`
(the `X-Hydra-Caller` header, or `unidentified`), `peer`, `authenticated`, `mode`, `joint`, `position`, `certificate_key_id`, `certificate_nonce` and, for an outcome, `kind`,
`delivered` and `reason`. `GET /stats` adds `uncertified`, `auditFailures`, `certifiedMode` and `unsignedRiskAllowed`.

