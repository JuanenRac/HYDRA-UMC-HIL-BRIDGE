<p align="center">
  <img src="images/HYDRA_UMC_BANNER.svg" alt="HYDRA-UMC-HIL-BRIDGE banner" width="100%">
</p>

# 🌉 HYDRA-UMC-HIL-BRIDGE

<p align="center">🇺🇸 <b>English</b> | <a href="README_spa.md">🇪🇸 Español</a> | <a href="README_fra.md">🇫🇷 Français</a> | <a href="README_ita.md">🇮🇹 Italiano</a> | <a href="README_deu.md">🇩🇪 Deutsch</a> | <a href="README_zho.md">🇨🇳 简体中文</a> | <a href="README_jpn.md">🇯🇵 日本語</a></p>

### ⚡ Hardware-in-the-Loop Interface for Real-vs-Virtual Sync

<p align="left">
  <img src="https://img.shields.io/badge/Licencia-GPL%203.0-blue.svg" alt="GPL 3.0">
  <img src="https://img.shields.io/badge/Protocol-WebSocket%20%2F%20gRPC-yellow.svg" alt="Protocol">
  <img src="https://img.shields.io/badge/Feature-Zero--Latency%20Sync-green.svg" alt="Sync">
  <img src="https://img.shields.io/badge/Stage-Established%20v0-brightgreen.svg" alt="Established v0 stage">
</p>

---

## 1. 🛠️ TECHNICAL OVERVIEW

**HYDRA-UMC-HIL-BRIDGE** is the communication artery that enables Hardware-in-the-Loop (HIL) functionality. It synchronizes the state between the physical controllers and the Digital Twin engine in real-time.

It allows developers to send commands from any interface (App, Suite, Studio) to the simulator as if it were a physical robot, and conversely, it can mirror a physical robot's movements into the virtual world for remote supervision and digital twin shadowing.

### Key Features:
* 🛡️ **Safety Interlock (v0):** the real `route` subcommand blocks a real-hardware-bound command whenever a twin risk report says a collision is imminent - see "Honesty check" below for exactly what runs today.
* 🌉 **Bidirectional Bridge (v0, no transport yet):** real mode-based routing (Real/Simulation) and real unconditional mirroring exist; there is no real gRPC/WebSocket connection to an actual HYDRA-UMC-TWIN or HYDRA-UMC controller yet.
* ⚡ **Zero-Latency Mirroring (partial):** the real `mirror` subcommand shadows a command into a recording sink today; "zero-latency" over a real network connection is still future work.
* 📡 **Unified Protocol (planned):** uses gRPC for high-speed local sync and WebSockets for remote monitoring - no network transport is wired in yet, on purpose (see `Cargo.toml`).
* 🧪 **Fail-safe transport, testable without hardware (v0):** `CommandSink::send()` returns a real `Result`, and a `SimulatedTransport` can model a timed-out or disconnected link; the bridge reports a distinct `TransportFailure` outcome rather than ever claiming a command was delivered when it wasn't - exercisable via `--transport-latency-ms`/`--transport-timeout-ms`/`--transport-disconnected` on `route`.

**Honesty check - what actually runs today:** `route --mode real|simulation --joint NAME --position VALUE [--collision-risk] [--distance METERS] [--transport-latency-ms MS] [--transport-timeout-ms MS] [--transport-disconnected]` makes a real routing decision - `simulation` mode always sends, `real` mode is gated by a real safety interlock that blocks the command whenever `--collision-risk` is set. `mirror --joint NAME --position VALUE` shadows a command unconditionally. Both route to an in-memory sink by default (`RecordingSink`, not a real controller or a real HYDRA-UMC-TWIN instance), or to a `SimulatedTransport` that can genuinely fail (timeout/disconnect) when the transport flags above are passed - there is no gRPC/WebSocket transport yet. See [`CHANGELOG.md`](CHANGELOG.md) for exactly what shipped, and the Roadmap below for what's still ahead.

---

## 2. 🔄 HIL SYNC FLOW

The mode-based split at `BRIDGE` and the safety interlock gate on the
`Real Mode` path are real today (`bridge.rs`/`interlock.rs`), routing to
an in-memory sink rather than a live process. Everything touching a real
`APP`, `TWIN` or `CORE` process over a real connection remains future
work.

```mermaid
flowchart LR
    APP["Control Interface - planned"] --> BRIDGE["HIL-BRIDGE - real v0 routing"]
    BRIDGE -- Simulation Mode --> TWIN["HYDRA-UMC-TWIN - planned"]
    BRIDGE -- "Real Mode (interlock-gated - real v0)" --> CORE["HYDRA-UMC Core (STM32) - planned"]
    CORE -- Feedback --> BRIDGE
    TWIN -- Feedback --> BRIDGE
    BRIDGE --> APP
```

---

## 3. 🧱 ARCHITECTURE & DESIGN DECISIONS

* **Why this bridge has no `hardware/`/`firmware/`/`os/` folders.** Pure software - it bridges existing hardware (real apps, real HYDRA-UMC firmware) to the digital twin, no board of its own.
* **Why it's a sibling, not a submodule, of HYDRA-UMC-TWIN.** A hardware-in-the-loop bridge needs to keep running (and keep a real device's commands flowing) independent of whatever HYDRA-UMC-TWIN's own render/physics loop is doing at that instant - a separate process means a twin restart doesn't drop an in-flight real command.
* **How this fits the rest of the ecosystem.** Lets HYDRA-UMC-SUITE, HYDRA-UMC-ANDROID-CONTROL and HYDRA-UMC-IOS-CONTROL control HYDRA-UMC-TWIN as if it were a real HYDRA-UMC-SERVER-backed cell - the same control surface, a simulated target.
* **Why the interlock trusts the twin's own `collision_imminent` flag instead of re-deriving one from a distance threshold.** The twin has already done the real geometric/physics reasoning by the time it reports risk - second-guessing that conclusion here with an independent distance cutoff would just be a second, possibly-inconsistent safety opinion. See `interlock.rs`'s own module docs for how this mirrors HYDRA-UMC-SAFETY-ZONES's detect-vs-enforce boundary.
* **Why `Simulation` mode is never gated by the interlock.** The entire point of routing a command to the twin instead of real hardware is to be able to see a predicted collision play out safely - blocking it there would defeat the feature it exists to support.
* **Why `CommandSink` is a trait with only an in-memory `RecordingSink` implementation today.** No real gRPC/WebSocket transport exists yet (see `Cargo.toml`'s own comment) - `RecordingSink` is honest about that: it records what it was asked to send without transmitting anywhere, the same reasoning as `NullEStopRequester` in HYDRA-UMC-SAFETY-ZONES.
* **Why `CommandSink::send()` returns a `Result` instead of `()`.** A real transport can fail to deliver (timeout, disconnect) independently of whether the interlock allowed the command through - if `send()` can't fail, the bridge has no way to avoid claiming success on a command that never actually arrived. `SimulatedTransport` exists specifically so that failure path is real and testable today, without waiting for a real transport to exist.
* **Why `TransportFailure` is a separate `RouteOutcome` from `BlockedByInterlock`.** They are different kinds of "didn't happen": one is a deliberate safety refusal (the interlock decided not to forward it), the other is best-effort delivery that simply didn't complete. Collapsing them into one generic failure would hide which safety layer actually stopped the command - useful for debugging and for any future policy that treats them differently (e.g. retrying a transport failure is reasonable; retrying past an interlock block is not).

---

## 📂 DIRECTORY STRUCTURE

Pure software bridge with no hardware design of its own, so this project has
no `hardware/`, `firmware/` or `os/` folders under the repository structure policy.

```text
HYDRA-UMC-HIL-BRIDGE/
├── src/
│   ├── protocol.rs       # Real JointCommand/Mode types
│   ├── interlock.rs      # Real safety interlock decision
│   ├── bridge.rs         # Real mode-based routing + mirroring + CommandSink/SimulatedTransport
│   ├── server.rs         # Plain JSON/HTTP surface (tiny_http, blocking, no async runtime)
│   └── main.rs           # Entry point + real `route`/`mirror` subcommands
├── docs/                # Documentation and integration guides
├── build/               # Build notes/artifacts (cargo's own output lives in target/, gitignored)
├── images/              # Media and diagrams
├── systemd/
│   └── hydra-umc-hil-bridge.service # Local CM5 route/mirror API systemd unit
├── tools/
│   ├── build_test.py    # Non-versioning build/compile check
│   └── ci_validate.py   # Manifest/CHANGELOG/docs validation used by CI
├── Cargo.toml           # Package metadata, dependencies, odometer version
├── bump_version.py      # Odometer-style native version bump (used by build.sh/.bat)
├── bump_manifest_version.py # Syncs hydra-umc.project.json's version to the native one (--sync)
├── build.sh / build.bat # Bumps version, `cargo test`, then `cargo build --release`
├── build-test.sh / build-test.bat # Non-versioning build check (no CHANGELOG/version bump)
└── run.sh / run.bat     # Runs the compiled release binary (forwards arguments)
```

---

## 🏗️ BUILD AND RUN GUIDE

Requires the Rust toolchain (`cargo`/`rustc`, install via [rustup](https://rustup.rs)) and Python 3.10+ (only for `bump_version.py`).

```bash
# Linux / macOS
./build.sh   # odometer version bump, `cargo test` (23 tests), then `cargo build --release`
./run.sh     # runs target/release/hydra-umc-hil-bridge, prints name + version + role
```

```bat
:: Windows
build.bat
run.bat
```

`build.sh`/`build.bat` bump this project's own `Cargo.toml` version following the ecosystem's "odometer" rule (PATCH+1, carrying into MINOR past 9), run the real test suite, then build a release binary.

The real `route` and `mirror` subcommands:

```bash
./run.sh route --mode real --joint shoulder --position 0.5
# SENT: routed to real HYDRA-UMC controller

./run.sh route --mode real --joint shoulder --position 0.5 --collision-risk --distance 0.02
# BLOCKED: safety interlock refused to forward to real hardware (twin reports imminent collision at 0.020m)

./run.sh route --mode simulation --joint shoulder --position 0.5 --collision-risk --distance 0.02
# SENT: routed to HYDRA-UMC-TWIN (simulation)

./run.sh mirror --joint elbow --position -0.3
# MIRRORED: joint 'elbow' = -0.300000 shadowed into the twin

./run.sh route --mode real --joint shoulder --position 0.5 --transport-timeout-ms 100 --transport-latency-ms 500
# TRANSPORT FAILURE: command was not confirmed delivered (transport timed out after 100ms)

./run.sh route --mode real --joint shoulder --position 0.5 --transport-disconnected
# TRANSPORT FAILURE: command was not confirmed delivered (transport is disconnected)
```

`route` exits `0` (sent), `1` (blocked by the safety interlock - a real, meaningful outcome, not an error), `2` (bad input), or `3` (transport failure - the interlock allowed it, but delivery wasn't confirmed). `mirror` exits `0`, `2`, or `3`.

`Cargo.toml` intentionally carries no external crates yet - see the comment inside it for what gets added once real gRPC/WebSocket transport work starts.

---

## 🚀 ROADMAP
* **Phase 1:** Digital Twin synchronization with real-time hardware telemetry and sub-10ms latency.
* **Phase 2:** Physics Replica integration with industrial-grade simulators (Isaac Sim) and deformable body support.
* **Phase 3:** Node Healing automated recovery patterns for decentralized failover and early sensor degradation detection.
* **Phase 4:** Multi-controller HIL synchronization (Swarm HIL) and photorealistic synthetic data generation support.

---

## 🔗 Related Projects

This project is part of the HYDRA-UMC robotics ecosystem by the same author (JuanenRac / Electro Hobby 3D). Worth knowing about, since a request might actually be about one of these rather than this repository.

**Parent Project**
- **[HYDRA-UMC-TWIN](https://github.com/JuanenRac/HYDRA-UMC-TWIN)** — integration hub for the digital-twin engine, with a real version-compatibility sync contract; the parent this repo is one specific simulation service of, within its own digital-twin engine.

**Sibling Projects** — the other simulation services of HYDRA-UMC-TWIN's own digital-twin engine
- **[HYDRA-UMC-PHYSICS-REPLICA](https://github.com/JuanenRac/HYDRA-UMC-PHYSICS-REPLICA)** — real forward kinematics and joint-limit validation over a real URDF subset.
- **[HYDRA-UMC-SYNTHETIC-DATA-GEN](https://github.com/JuanenRac/HYDRA-UMC-SYNTHETIC-DATA-GEN)** — real procedural 2D scene generator with YOLO/COCO annotation export.

**Directly Related**
- **[HYDRA-UMC-STUDIO](https://github.com/JuanenRac/HYDRA-UMC-STUDIO)** — web control dashboard with real-time multi-robot 3D visualization — one of the 3 client interfaces that can send commands through this bridge as if it were a physical robot, once a real transport exists.
- **[HYDRA-UMC-SUITE](https://github.com/JuanenRac/HYDRA-UMC-SUITE)** — desktop (PySide6) swarm command center for multiple servers at once, packaged as a standalone executable — one of the 3 client interfaces that can send commands through this bridge as if it were a physical robot, once a real transport exists.
- **[HYDRA-UMC-ANDROID-CONTROL](https://github.com/JuanenRac/HYDRA-UMC-ANDROID-CONTROL)** — native Android control app with biometric login and a paired Wear OS companion — one of the 3 client interfaces that can send commands through this bridge as if it were a physical robot, once a real transport exists.
- **[HYDRA-UMC-IOS-CONTROL](https://github.com/JuanenRac/HYDRA-UMC-IOS-CONTROL)** — iOS/iPadOS control app (Flutter) with real-time WebSocket sync — one of the 3 client interfaces that can send commands through this bridge as if it were a physical robot, once a real transport exists.
- **[HYDRA-UMC-SERVER](https://github.com/JuanenRac/HYDRA-UMC-SERVER)** — the real headless backend (REST/WebSocket) every control client actually talks to — the backend behind all 3 of those client interfaces, the real controller this bridge's own `route --mode real` ultimately targets once a transport exists.

**Also Part of the Ecosystem**

*Core Hardware & Platform*
- **[HYDRA-UMC](https://github.com/JuanenRac/HYDRA-UMC)** — the physical robot-arm motherboard: CM5 host + dual-core STM32H745, orchestrating up to 8 tool arms over CAN-OTA/SPI-OTA.
- **[HYDRA-UMC-OS](https://github.com/JuanenRac/HYDRA-UMC-OS)** — reproducible Raspberry Pi OS product layer for the CM5: read-only agent, validated config/profiles, WiFi first-contact provisioning.
- **[HYDRA-UMC-SDK](https://github.com/JuanenRac/HYDRA-UMC-SDK)** — the shared JSON-Schema contract and safety-gate boundary every bridge validates its commands against.

*Core Backend & Clients*
- **[HYDRA-UMC-DSI](https://github.com/JuanenRac/HYDRA-UMC-DSI)** — native touch UI for the onboard 7" DSI touchscreen, embedded on the CM5 itself.
- **[HYDRA-UMC-EDITOR-URDF](https://github.com/JuanenRac/HYDRA-UMC-EDITOR-URDF)** — desktop graphical URDF creator/editor that pushes finished models into STUDIO's own catalog.
- **[HYDRA-UMC-BRIDGE-AMR](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-AMR)** — coordination boundary for AGV/AMR fleets via a real VDA 5050 MQTT publisher.
- **[HYDRA-UMC-BRIDGE-CNC](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-CNC)** — high-level CNC-cell coordinator with real GRBL status/control-byte access.
- **[HYDRA-UMC-BRIDGE-DROIDS](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-DROIDS)** — coordination boundary for legged/humanoid droids, with a real Boston Dynamics Spot command sender.
- **[HYDRA-UMC-BRIDGE-LASER](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-LASER)** — laser-cell safety coordinator reading 3 real key/enclosure/interlock GPIO safeguards.
- **[HYDRA-UMC-BRIDGE-OPENPNP](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-OPENPNP)** — safe high-level board-flow coordinator for OpenPnP pick-and-place.
- **[HYDRA-UMC-BRIDGE-PRINTER3D](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-PRINTER3D)** — safe coordination boundary for Moonraker/Klipper 3D printers, with real gated job commands.
- **[HYDRA-UMC-BRIDGE-ROS2](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-ROS2)** — safety coordinator with a real, lazily-imported rclpy ROS 2 transport.
- **[HYDRA-UMC-BRIDGE-UAV](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-UAV)** — coordination boundary for camera-equipped UAVs, with a real MAVLink command sender.

*URTC Tool Platform*
- **[URTC](https://github.com/JuanenRac/URTC)** — firmware for the physical Universal Robot Tool Controller PCB, 25+ tool profiles over CAN bus.
- **[URTC-FLASHER](https://github.com/JuanenRac/URTC-FLASHER)** — desktop GUI flashing tool for URTC boards, CAN-OTA plus full-chip SWD/JTAG.
- **[URTC-TESTER](https://github.com/JuanenRac/URTC-TESTER)** — desktop live CAN-bus diagnostic tool for URTC boards, one panel per tool profile.
- **[URTC-WEB-STUDIO](https://github.com/JuanenRac/URTC-WEB-STUDIO)** — browser-based alternative to URTC-TESTER via the Web Serial API, no local install needed.

*Vision AI Node (Hailo-8)*
- **[HYDRA-UMC-VISION-NODE](https://github.com/JuanenRac/HYDRA-UMC-VISION-NODE)** — integration hub for the Hailo-8 vision pipeline, with a real per-stage hardware-readiness check.
- **[HYDRA-UMC-DETECTION-HEF](https://github.com/JuanenRac/HYDRA-UMC-DETECTION-HEF)** — real compiled-model registry with Hailo-architecture/checksum safe-load verification.
- **[HYDRA-UMC-VISION-STREAMER](https://github.com/JuanenRac/HYDRA-UMC-VISION-STREAMER)** — real GStreamer pipeline + MediaMTX config generator with a real HailoRT integration boundary.
- **[HYDRA-UMC-VISUAL-SERVOING-API](https://github.com/JuanenRac/HYDRA-UMC-VISUAL-SERVOING-API)** — real Position-Based Visual Servoing correction law, safety-gated on upstream zone state.
- **[HYDRA-UMC-SAFETY-ZONES](https://github.com/JuanenRac/HYDRA-UMC-SAFETY-ZONES)** — real zone-breach checking and E-STOP requesting, with calibration-freshness enforcement.

*Cognitive AI Node (Hailo-10)*
- **[HYDRA-UMC-COGNITIVE-NODE](https://github.com/JuanenRac/HYDRA-UMC-COGNITIVE-NODE)** — integration hub for the Hailo-10 cognitive pipeline (LLM/VLA/voice orchestration).
- **[HYDRA-UMC-VLA-ENGINE](https://github.com/JuanenRac/HYDRA-UMC-VLA-ENGINE)** — real action-token encoding/decoding and trajectory generation for a Vision-Language-Action model.
- **[HYDRA-UMC-VOICE-UI](https://github.com/JuanenRac/HYDRA-UMC-VOICE-UI)** — real voice front-end (VAD + intent parser) with a bounded, confirmation-gated Watch relay.
- **[HYDRA-UMC-SEMANTIC-PLANNER](https://github.com/JuanenRac/HYDRA-UMC-SEMANTIC-PLANNER)** — real rule-based task decomposition and semantic error recovery over MCU error codes.
- **[HYDRA-UMC-DOCS-QA](https://github.com/JuanenRac/HYDRA-UMC-DOCS-QA)** — real stdlib-only TF-IDF document search over this ecosystem's own Markdown docs.

*Orchestration & Swarm*
- **[HYDRA-UMC-ORCHESTRATOR](https://github.com/JuanenRac/HYDRA-UMC-ORCHESTRATOR)** — integration hub with a real gRPC/Protobuf health-report contract and mission state machine.
- **[HYDRA-UMC-JOB-DISPATCHER](https://github.com/JuanenRac/HYDRA-UMC-JOB-DISPATCHER)** — real priority-based job queue with deduplication, over a real HTTP API.
- **[HYDRA-UMC-NODE-HEALING](https://github.com/JuanenRac/HYDRA-UMC-NODE-HEALING)** — real gRPC-based fleet health watchdog with retry/backoff and identity-mismatch detection.
- **[HYDRA-UMC-PATH-PLANNER-3D](https://github.com/JuanenRac/HYDRA-UMC-PATH-PLANNER-3D)** — real RRT-based 3D path planner with real obstacle/workspace collision validation.
- **[HYDRA-UMC-SWARM-SYNC](https://github.com/JuanenRac/HYDRA-UMC-SWARM-SYNC)** — real CRDT LWW-Element-Map state sync, property-tested for multi-cell convergence.

*Data & Analytics*
- **[HYDRA-UMC-DATALAKE](https://github.com/JuanenRac/HYDRA-UMC-DATALAKE)** — real sqlite3-backed time-series store with a real ingest/query HTTP API.
- **[HYDRA-UMC-ANOMALY-DETECTOR](https://github.com/JuanenRac/HYDRA-UMC-ANOMALY-DETECTOR)** — real FFT + statistical baseline anomaly detector with drift monitoring.
- **[HYDRA-UMC-PRODUCTION-REPORTS](https://github.com/JuanenRac/HYDRA-UMC-PRODUCTION-REPORTS)** — real OEE/availability calculation over DATALAKE history, with reproducible CSV export.
- **[HYDRA-UMC-TELEMETRY-COLLECTOR](https://github.com/JuanenRac/HYDRA-UMC-TELEMETRY-COLLECTOR)** — real CAN/WebSocket ingestion pipeline into DATALAKE, with sequence deduplication.

*Industrial Gateway*
- **[HYDRA-UMC-GATEWAY-INDUSTRIAL](https://github.com/JuanenRac/HYDRA-UMC-GATEWAY-INDUSTRIAL)** — integration hub relaying to industrial protocols, with a real command allowlist/backpressure layer.
- **[HYDRA-UMC-OPCUA-SERVER](https://github.com/JuanenRac/HYDRA-UMC-OPCUA-SERVER)** — real OPC-UA address space, verified with a real binary-protocol client session.
- **[HYDRA-UMC-MQTT-BROKER](https://github.com/JuanenRac/HYDRA-UMC-MQTT-BROKER)** — real MQTT broker with optional per-client authentication and topic ACLs.
- **[HYDRA-UMC-MTCONNECT-ADAPTER](https://github.com/JuanenRac/HYDRA-UMC-MTCONNECT-ADAPTER)** — real MTConnect `/probe` and `/current` XML endpoints with degraded-mode output.

*Complementary Tools & Ecosystem Operations*
- **[HYDRA-UMC-DASHBOARD-AI](https://github.com/JuanenRac/HYDRA-UMC-DASHBOARD-AI)** — Smart Summaries and Anomaly Highlighting panels over DATALAKE/ANOMALY-DETECTOR, with an honest statistical fallback.
- **[HYDRA-UMC-TOOL-CLI](https://github.com/JuanenRac/HYDRA-UMC-TOOL-CLI)** — fleet CLI with a real, stable exit-code contract, a genuine live client of HYDRA-UMC-SERVER's own API.
- **[HYDRA-UMC-WATCH](https://github.com/JuanenRac/HYDRA-UMC-WATCH)** — WearOS companion app with real haptic alerts and a paired-phone voice relay.
- **[URTC-SMART-RACK](https://github.com/JuanenRac/URTC-SMART-RACK)** — firmware for a board-mounting rack with real tool-ID decoding and Smart Idle pre-heating logic.
- **[URTC-VISION-TOOL](https://github.com/JuanenRac/URTC-VISION-TOOL)** — firmware plus a real Python vision companion for a thermal/RGB inspection tool head.
- **[HYDRA-UMC-UPDATER](https://github.com/JuanenRac/HYDRA-UMC-UPDATER)** — administrative desktop tool that discovers, clones and updates every repo in this ecosystem.


## 👤 AUTHOR
**JuanenRac** (Electro Hobby 3D)
📧 electrohobby3d@gmail.com
📺 [youtube.com/@electrohobby3d](https://youtube.com/@electrohobby3d)

## 📜 LICENSE
GPL-3.0 - See LICENSE for details.
