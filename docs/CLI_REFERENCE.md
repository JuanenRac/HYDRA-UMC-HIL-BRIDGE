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
$ hydra-umc-hil-bridge route --mode real --joint j1 --position 0.5 --clear-distance 2.0 --transport-disconnected
TRANSPORT FAILURE: command was not confirmed delivered (transport is disconnected)
```

**Real mode, transport timeout** — latency exceeds the configured timeout
budget (exit `3`):

```
$ hydra-umc-hil-bridge route --mode real --joint j1 --position 0.5 --clear-distance 2.0 --transport-latency-ms 500 --transport-timeout-ms 100
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
further). With `--urdf FILE` the bridge knows the joints and their ranges: a real
command for an unknown joint or a position outside its range is refused (counted as
`outsideLimits`); positions are in the units of the URDF (radians, metres). Without
`--urdf` it only checks the shape of the values.

**`POST /mirror`** — `{"joint": "elbow", "position": -0.3}`, responds
`{"mirrored": true}` or `{"mirrored": false, "error": "..."}`, both `200`;
`400` on malformed JSON or an invalid `joint`/`position` (same checks as `/route`).

**`GET /health`** — closed by default: it answers `404` like a route that does not exist. With `serve --health-open` (or `HYDRA_UMC_HIL_BRIDGE_HEALTH_OPEN=1`) it answers `200` `{"status": "ok", "service": ..., "version": ...}` to a caller on loopback; a caller that is not on loopback must present the bearer token of the bridge (`401` otherwise), and a service without a token never answers it. It is checked before the token gate and carries no path, configuration or secret.

**`GET /stats`** — `200` with the process-lifetime counters: `role`,
`routedReal`, `routedSimulation`, `blockedByInterlock`, `transportFailures`,
`mirrored`, `mirrorFailures`, `malformedRequests` and `notFound`.

A known path with the wrong method (`GET /route`, `POST /stats`) is `405`
with an `Allow` header; any other path is `404`.
There is no authentication - same as every other loopback-only internal API
on the CM5 (see the systemd unit's own `RestrictAddressFamilies`/
`ProtectSystem` hardening for what it relies on instead).

`serve --identity-key-file FILE` (or `HYDRA_UMC_HIL_BRIDGE_IDENTITY_KEY_FILE`; off by default) accepts the signed token of HYDRA-UMC-SERVER next to the token of the service: a viewer may read but not change, and a request with no credential is refused even on loopback, `GET /health` and `GET /stats` included.

## compare-dynamics

```
compare-dynamics --simulator PATH --urdf ROBOT.urdf --recording FILE [--joints "..."] [--velocities "..."] [--torques "..."] [--duration S] [--dt S] [--max-deviation RAD]
```

Runs `simulate --json` of HYDRA-UMC-PHYSICS-REPLICA (the program at `--simulator`, with fixed arguments and no shell) and compares its joint positions with a recording of a real arm, by straight lines between the simulated samples. The recording is `{"joints": [...], "samples": [{"t_s": 0.0, "positions": [...]}]}` with the joints in any order. It prints `{"within_limit": ..., "joints": [{"joint", "max_abs", "rms", "compared"}]}`; exit `0` within `--max-deviation` (or when none is given), `1` over it, `2` for bad input. Samples outside the simulated time are not compared. Evidence: simulator until a real arm has been recorded.

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
| `--require-twin-pass` | `HYDRA_UMC_HIL_BRIDGE_REQUIRE_TWIN_PASS=1` | Also require the pass verdict of the twin for the trajectory of a real command, and lock the switch on (see `docs/TWIN_PASS.md`). Off by default. |
| `--twin-robot-id NAME` | `HYDRA_UMC_HIL_BRIDGE_TWIN_ROBOT_ID` | The robot a verdict must be for. |
| `--twin-pass-file FILE` | | Where the value of `require_twin_pass` is kept (`{"require_twin_pass": true}`). |
| `--audit-log FILE` | `HYDRA_UMC_HIL_BRIDGE_AUDIT_LOG` | Append every real order (`order` before it is decided, `outcome` after) as a JSON line; an order whose record cannot be written is refused with `503`. |
| `--max-valid-for-ms N` | | Longest validity window accepted for a real command, 1 to 60000 (default 1000). |
| `--allow-undated-commands` | `HYDRA_UMC_HIL_BRIDGE_ALLOW_UNDATED_COMMANDS=1` | Accept a real command without `issued_at_ms` and `valid_for_ms`. Off by default; announced at every start: such a command stays in force if its producer stops. |
| `--urdf FILE` | `HYDRA_UMC_HIL_BRIDGE_URDF` | The URDF of the robot. A real command for a joint it does not have, a fixed joint, a joint that follows another (`<mimic>`) or a position outside the `lower`/`upper` of its `<limit>` is blocked (`BlockedByInterlock`, counted in `outsideLimits`) before its certificate is used. `continuous` joints have no position limit. A URDF without a usable `<limit>` stops the start. Simulation mode is not gated. |
| `--workers N` | | Requests served at once, 1 to 256 (default 8). A stuck client holds one worker; real orders are still decided one at a time. |

`GET /audit` (behind the Bearer token when there is one) answers `{"records": [...last 200...], "file": bool}`. A record has `at_ms`, `phase`, `caller`
(the `X-Hydra-Caller` header, or `unidentified`), `peer`, `authenticated`, `mode`, `joint`, `position`, `certificate_key_id`, `certificate_nonce` and, for an outcome, `kind`,
`delivered` and `reason`. `GET /stats` adds `uncertified`, `auditFailures`, `certifiedMode` and `unsignedRiskAllowed`.

**Validity window and watchdog.** A `POST /route` in real mode carries `issued_at_ms` (milliseconds since the epoch), `valid_for_ms` (1 up to `--max-valid-for-ms`) and, optionally, a `seq` that must grow for each joint. A command that is already expired, issued more than 2 s in the future, good for too long or not newer than the last accepted for its joint is blocked (`BlockedByInterlock`) before its certificate is used. A delivered command holds a lease on its joint until `issued_at_ms + valid_for_ms`; when it ends without a newer valid command the bridge stops the joint itself (`phase` `watchdog`, `kind` `watchdog_stop` in the audit) whether the producer is alive or not. `GET /stats` adds `outsideLimits`, `urdfLimits` (joints read from the URDF, `null` without one), `workers`, `staleCommands`, `watchdogStops`, `watchdogStopFailures`, `watchdogRequiresValidity`, `watchdogMaxValidForMs` and `activeLeases`. The stop reaches the same recording sink as the commands, because no transport is wired in yet.


## Telling the twin where the real joints are (`serve --twin-url`)

The twin signs the risk of a command from what it believes about the robot, so what it believes has to follow the hardware. With `--twin-url` the bridge
reports to `POST /interlock/state` of HYDRA-UMC-TWIN: the position of a joint right after a real command was delivered (`source: "commanded"`) and, on
`POST /measured {"joint", "position"}`, the position the controller read back (`source: "measured"`).

```json
{"joint":"shoulder","position":0.5,"seq":7,"stamp_ms":1700000000000,"source":"measured"}
```

`seq` is made from the clock and a per-joint counter, so it grows even across a restart; the twin refuses a report that is not newer than the last one of its joint, and
the bridge takes that refusal (`409`) as "a newer report got there first". A report is sent after the lock of the real orders is released and never changes what happened to
the command: if it cannot be delivered the command stays `SentReal`, the failure is counted (`twinReportFailures`), `twinInSync` turns `false` until a later report arrives,
and the audit log gets a `twin_report` record with the reason. The answer of `POST /route` carries `X-Twin-Report: delivered|failed` when a real command was reported.
Simulation mode and blocked commands are never reported.

| Flag | Environment | Meaning |
|---|---|---|
| `--twin-url http://HOST:PORT` | | Where the twin listens (plain HTTP, no path). Without it the twin is not told. |
| `--twin-state-token-file FILE` | `HYDRA_UMC_HIL_BRIDGE_TWIN_TOKEN` | The token the twin asks for on `/interlock/*` (at least 16 characters). Required with `--twin-url`. |
| `--twin-timeout-ms N` | | Connect, write and read limit of one report, 10 to 10000 (default 500). |

`GET /stats` adds `twinLinked`, `twinReports`, `twinReportFailures` and `twinInSync` (`null` until the first report). The fail-closed half is in the twin: started with
`--require-fresh-state-ms N` it signs no certificate for a joint it has not heard of in the last N milliseconds, and without a certificate the real mode blocks.
Nothing here was tried against a real controller: the link has only run between the two programs on one machine.

## Asking the twin for a collision prediction, in shadow (`serve --collision-shadow`)

Off by default. With `--collision-shadow` (it needs `--twin-url` and its token, and a twin started with `--collision-predict`) the bridge asks `POST /predict/collision` of the twin after each real command that was delivered, naming the joint and the commanded position (the other joints come from the last positions the twin was told). The answer says whether the twin predicts that the robot would enter a zone of its zones file or touch itself.

**It only informs.** It is asked after the command went and after the report to the twin, so it never blocks a command, never changes the decision of the interlock and never touches the certificates; a command that the shadow says would have collided is still `SentReal`. The answer is recorded in the audit log (`collision_shadow`, with the level and the number of contacts, or the reason it failed), counted in `GET /stats` and shown in the header `X-Collision-Shadow: clear|collision|failed` of the answer of `POST /route`. A twin that is down, has the route off or refuses the question is `failed` and never read as `clear`. The wait is bounded by the timeout, so the answer of `POST /route` can be late by up to that long.

| Flag | Meaning |
|---|---|
| `--collision-shadow` | Ask the twin. Without it nothing is asked and `POST /route` answers as before. |
| `--collision-shadow-timeout-ms N` | Connect, write and read limit of one question, 10 to 10000 (default 1000). Needs `--collision-shadow`. |
| `--collision-shadow-enforce` | Ask BEFORE a real command is routed and refuse it on a predicted contact (`409`) or when the twin gave no answer inside the limit above (`503`). Needs `--collision-shadow`. |

With `--collision-shadow-enforce` the shadow stops being only informative: the question is asked before the command reaches the interlock, so the wait of `--collision-shadow-timeout-ms` is the budget the command can wait. A command that was not checked in time is refused, never sent unchecked. A refusal is `{"error", "blockedBy": "collision_shadow" | "collision_shadow_unavailable"}`, is recorded in the audit log (`shadow_blocked` or `shadow_unavailable`) and does not use up the certificate of the command. `GET /stats` adds `shadowEnforce`, `shadowBlocked` and `shadowUnavailable`; `GET /metrics` the counters `hydra_umc_hil_bridge_shadow_blocked_total` and `hydra_umc_hil_bridge_shadow_unavailable_total`. A command in `simulation` mode, and a body that cannot be read, are not asked about. The question is asked once: with the shadow enforced it is not asked again after the command went.

`GET /stats` adds `collisionShadow`, `shadowPredictions` (answered), `shadowWouldBlock` (answered with a predicted contact), `shadowFailures` and `shadowLast` (`null` until the first question). Simulation mode and blocked commands are never asked about. Not tried with a real robot or a real controller; the prediction is described in the documentation of HYDRA-UMC-TWIN and HYDRA-UMC-PHYSICS-REPLICA.

## Delivering the commands to the twin's simulated robot (`serve --sim-robot`) and `GET /metrics`

Off by default. With `--sim-robot` (it needs `--twin-url` and its token, which it uses) the place a command is delivered to is the **simulated robot of the twin** (the twin started
with `--sim-config`) and not a recording: a real-mode command that passed the interlock becomes `POST /sim/command` on the twin, and so does a simulation-mode command. The
watchdog stops a joint whose lease ran out with `{"joint","stop":true}`, which brakes the simulated joint where it can. The twin then moves the joint over time and reports
it to itself as `measured`, so the whole loop runs without a robot: the certificate of the twin, the signature check, the limits, the validity window, the audit, the watchdog and the
state that follows the joint. A refusal or an unreachable simulated robot is a delivery that failed (`TransportFailure`), never a command that went. The interlock does not
change; only what is behind it is a model.

`GET /metrics` answers the counters in the text format of Prometheus (version 0.0.4 of the exposition format): the same numbers as `GET /stats` (commands routed, blocked,
refused and failed, the watchdog, the link to the twin, the shadow predictions) and the version. Unlike `/stats` it needs the token when the bridge has one, because it tells how
many real commands were issued and refused. No joint, position, certificate, token or address is a metric.

## Safety zones, session record and replay

All of this is off unless its flag is given.

| Flag | Meaning |
|---|---|
| `--safety-zones-url http://HOST:PORT` | Ask `GET /state` of HYDRA-UMC-SAFETY-ZONES before a real command is routed. Only a `READY` that is at most `--safety-zones-max-age-ms` old (100 to 60000, default 5000) lets the command go on. |
| `--safety-zones-token-file FILE` | Bearer token of the zone monitor (also `HYDRA_UMC_HIL_BRIDGE_ZONES_TOKEN`), at least 16 characters. |
| `--safety-zones-timeout-ms N` | How long to wait for the monitor (10 to 10000, default 500). |
| `--record-url http://HOST:PORT` | Record the commands that are decided and the positions that are read back in HYDRA-UMC-DATALAKE. |
| `--record-session NAME` | Name of the session (letters, digits, `.`, `_`, `-`; default made from the clock). |
| `--record-token-file FILE` | Ingest token of the lake (also `HYDRA_UMC_HIL_BRIDGE_RECORD_TOKEN`). |
| `--record-timeout-ms N` | How long each batch may take (10 to 30000, default 1000). |
| `--replay` | Turn on `POST /replay` (needs `--record-url`). |
| `--replay-max-points N` | The most points a replay reads from the lake (1 to 200000, default 20000). |

A refused command answers `409` with `blockedBy: safety_zones` when the zones are not clear, and `503` with `blockedBy: safety_zones_unavailable` when the monitor gave no state, an old state or a state with no readable time. The twin is asked after the monitor, so a refusal by the monitor ends the request.

A recorded command is one sample of `kind` `hil_command` with the fields `cmd.<joint>` (the position), `out.<joint>` (0 sent to the real robot, 1 sent in simulation, 2 blocked by the interlock, 3 transport failure, 4 refused before routing) and `real.<joint>` (1 for the real mode). A position read back is a sample of `kind` `hil_measured` with `pos.<joint>`. Every sample of a session has its own millisecond.

`POST /replay` takes `{"session": "NAME", "from_ms": N, "to_ms": N}` (all optional; the session defaults to the one being recorded) and answers the number of commands, how many were recorded as real, the count by outcome, how many were replayed to the simulation, the commands that now fall outside the limits of the URDF (`outsideLimits`, at most 100) and `regressions`, the real orders that were delivered and would now be blocked. `realHardwareTouched` is always `false`.

`GET /metrics` adds `hydra_umc_hil_bridge_refused_total{reason}`, `hydra_umc_hil_bridge_route_seconds` (a histogram per mode), the counters of the zone checks and, with `--record-url`, `hydra_umc_hil_bridge_session_recorded_total`, `_dropped_total` and `_failed_total`.
