// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/main.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! Entry point for HYDRA-UMC-HIL-BRIDGE.
//!
//! Bare invocation (no arguments) is unchanged: prints identity, version
//! and role, exits 0.
//!
//! The real `route` and `mirror` subcommands run this project's actual v0
//! bridging logic - mode-based routing gated by a real safety interlock,
//! and unconditional real-vs-virtual mirroring. See `protocol.rs`/
//! `interlock.rs`/`bridge.rs` for what "real" means here, and their own
//! module docs for what is still out of scope (any real gRPC/WebSocket
//! transport - see `Cargo.toml`).

// the stats document of the server is one large json! literal
#![recursion_limit = "256"]

mod access;
mod audit;
mod bridge;
mod certificate;
mod collision_shadow;
mod dynamics_link;
#[allow(dead_code)] // only the HMAC of the signed token is used
mod hmac;
mod http_client;
mod interlock;
mod limits;
#[cfg(test)]
mod loader_tests;
mod metrics;
mod protocol;
mod safety_zones;
mod server;
mod server_identity;
mod session_record;
mod twin_link;
mod twin_pass;
mod watchdog;

use std::env;
use std::process::ExitCode;

use bridge::{Bridge, CommandSink, RecordingSink, RouteOutcome, SimulatedTransport};
use interlock::TwinRiskReport;
use protocol::{JointCommand, Mode};

const PROJECT_NAME: &str = "HYDRA-UMC-HIL-BRIDGE";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const ROLE: &str =
    "Hardware-in-the-loop interface for real-vs-virtual command syncing with the Digital Twin.";

fn find_flag(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        // a following flag is not this flag's value (`--joint --position 1`)
        .filter(|v| !v.starts_with("--"))
        .cloned()
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn parse_command(args: &[String]) -> Result<JointCommand, String> {
    let joint = find_flag(args, "--joint").ok_or("missing required --joint NAME")?;
    let position_str = find_flag(args, "--position").ok_or("missing required --position VALUE")?;
    let position: f64 = position_str
        .parse()
        .map_err(|_| format!("'{position_str}' is not a valid number for --position"))?;
    let command = JointCommand { joint, position };
    command.validate()?;
    Ok(command)
}

fn run_route(args: &[String]) -> ExitCode {
    let mode = match find_flag(args, "--mode").as_deref() {
        Some("real") => Mode::Real,
        Some("simulation") => Mode::Simulation,
        Some(other) => {
            eprintln!("route: --mode must be 'real' or 'simulation', got '{other}'");
            return ExitCode::from(2);
        }
        None => {
            eprintln!("route: missing required --mode real|simulation");
            return ExitCode::from(2);
        }
    };

    let command = match parse_command(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("route: {e}");
            return ExitCode::from(2);
        }
    };

    let risk = if has_flag(args, "--collision-risk") {
        let distance_m: f64 = match find_flag(args, "--distance") {
            None => 0.0,
            Some(text) => match text.parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("route: --distance must be a number of meters, got '{text}'");
                    return ExitCode::from(2);
                }
            },
        };
        Some(TwinRiskReport {
            collision_imminent: true,
            distance_m,
        })
    } else if let Some(text) = find_flag(args, "--clear-distance") {
        // a twin report that says the path is clear, with this much clearance (the real mode refuses to forward without a report)
        match text.parse::<f64>() {
            Ok(distance_m) => Some(TwinRiskReport {
                collision_imminent: false,
                distance_m,
            }),
            Err(_) => {
                eprintln!("route: --clear-distance must be a number of meters, got '{text}'");
                return ExitCode::from(2);
            }
        }
    } else {
        None
    };

    let bridge = Bridge::new(mode);

    // Real transport isn't wired in yet (see Cargo.toml), but its
    // fail-safe behavior needs to be exercisable without any hardware:
    // `--transport-latency-ms`/`--transport-timeout-ms` swap the
    // always-succeeds `RecordingSink` for a `SimulatedTransport` that can
    // actually miss its budget. Omitting both flags is unchanged from
    // before this flag existed.
    let mut recording_real = RecordingSink::default();
    let mut simulated_real = if has_flag(args, "--transport-disconnected") {
        SimulatedTransport::disconnected()
    } else {
        SimulatedTransport::healthy().with_timeout(
            find_flag(args, "--transport-latency-ms")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            find_flag(args, "--transport-timeout-ms")
                .and_then(|s| s.parse().ok())
                .unwrap_or(u64::MAX),
        )
    };
    let use_simulated_transport = has_flag(args, "--transport-disconnected")
        || has_flag(args, "--transport-latency-ms")
        || has_flag(args, "--transport-timeout-ms");
    let real_sink: &mut dyn CommandSink = if use_simulated_transport {
        &mut simulated_real
    } else {
        &mut recording_real
    };
    let mut sim_sink = RecordingSink::default();
    let outcome = bridge.route_command(command, risk, real_sink, &mut sim_sink);
    eprintln!("{}", outcome.log_line(bridge.mode));

    match outcome {
        RouteOutcome::SentReal => {
            println!("SENT: routed to real HYDRA-UMC controller");
            ExitCode::SUCCESS
        }
        RouteOutcome::SentSimulation => {
            println!("SENT: routed to HYDRA-UMC-TWIN (simulation)");
            ExitCode::SUCCESS
        }
        RouteOutcome::BlockedByInterlock { reason } => {
            println!("BLOCKED: safety interlock refused to forward to real hardware ({reason})");
            ExitCode::from(1)
        }
        RouteOutcome::TransportFailure { reason } => {
            println!("TRANSPORT FAILURE: command was not confirmed delivered ({reason})");
            ExitCode::from(3)
        }
    }
}

fn run_mirror(args: &[String]) -> ExitCode {
    let command = match parse_command(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mirror: {e}");
            return ExitCode::from(2);
        }
    };

    let bridge = Bridge::new(Mode::Simulation);
    let mut mirror_sink = RecordingSink::default();
    match bridge.mirror_command(&command, &mut mirror_sink) {
        Ok(()) => {
            println!(
                "MIRRORED: joint '{}' = {:.6} shadowed into the twin",
                command.joint, command.position
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("TRANSPORT FAILURE: shadow update was not confirmed delivered ({e})");
            ExitCode::from(3)
        }
    }
}

/// How the real mode is certified, from `--twin-public-key-file` (one 64-hex Ed25519 public key per line; also
/// HYDRA_UMC_HIL_BRIDGE_TWIN_KEY_FILE), `--certificate-max-age-ms` (default 2000) and `--allow-unsigned-risk` (the bench setting; also
/// HYDRA_UMC_HIL_BRIDGE_ALLOW_UNSIGNED_RISK=1). With none of them the real mode blocks every command.
fn load_interlock(args: &[String]) -> Result<server::InterlockConfig, String> {
    let mut config = server::InterlockConfig::default();
    if let Some(path) = find_flag(args, "--twin-public-key-file")
        .or_else(|| env::var("HYDRA_UMC_HIL_BRIDGE_TWIN_KEY_FILE").ok())
    {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read the twin public key file: {e}"))?;
        config.trusted_keys =
            certificate::TrustedKeys::from_text(&text).map_err(|e| format!("{path}: {e}"))?;
        if config.trusted_keys.is_empty() {
            return Err(format!("{path} holds no public key"));
        }
    }
    if let Some(text) = find_flag(args, "--certificate-max-age-ms") {
        let age: u64 = text.parse().map_err(|_| {
            "--certificate-max-age-ms must be a whole number of milliseconds".to_string()
        })?;
        if age == 0 || age > 60_000 {
            return Err("--certificate-max-age-ms must be between 1 and 60000".to_string());
        }
        config.policy.max_age_ms = age;
    }
    config.allow_unsigned_risk = has_flag(args, "--allow-unsigned-risk")
        || env::var("HYDRA_UMC_HIL_BRIDGE_ALLOW_UNSIGNED_RISK")
            .map(|v| v == "1")
            .unwrap_or(false);
    Ok(config)
}

/// The pass verdict of the twin as a condition of moving the real robot (see `twin_pass.rs`): `--require-twin-pass` turns it on and locks it on for
/// as long as the bridge runs (also HYDRA_UMC_HIL_BRIDGE_REQUIRE_TWIN_PASS=1), `--twin-pass-file` keeps the value a change from HYDRA-UMC-SERVER sets
/// (`{"require_twin_pass": true}`), and `--twin-robot-id` names the robot a verdict must be for (also HYDRA_UMC_HIL_BRIDGE_TWIN_ROBOT_ID). Off by default.
fn load_twin_pass(
    args: &[String],
    interlock: &server::InterlockConfig,
) -> Result<server::TwinPassConfig, String> {
    let mut config = server::TwinPassConfig {
        robot_id: find_flag(args, "--twin-robot-id")
            .or_else(|| env::var("HYDRA_UMC_HIL_BRIDGE_TWIN_ROBOT_ID").ok())
            .unwrap_or_default()
            .trim()
            .to_string(),
        ..server::TwinPassConfig::default()
    };
    if let Some(path) = find_flag(args, "--twin-pass-file") {
        let path = std::path::PathBuf::from(path);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let value: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|e| format!("{}: not JSON ({e})", path.display()))?;
                config.required = value
                    .get("require_twin_pass")
                    .and_then(serde_json::Value::as_bool)
                    .ok_or_else(|| {
                        format!(
                            "{}: expected {{\"require_twin_pass\": true or false}}",
                            path.display()
                        )
                    })?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        }
        config.file = Some(path);
    }
    if has_flag(args, "--require-twin-pass")
        || env::var("HYDRA_UMC_HIL_BRIDGE_REQUIRE_TWIN_PASS")
            .map(|v| v == "1")
            .unwrap_or(false)
    {
        config.required = true;
        config.locked = true;
    }
    if config.required && (interlock.trusted_keys.is_empty() || config.robot_id.is_empty()) {
        return Err("the pass verdict of the twin is required, so --twin-public-key-file and --twin-robot-id must be set: without them every real command would be refused".to_string());
    }
    Ok(config)
}

/// The lease of the real commands. A real command needs its validity window unless `--allow-undated-commands` (or the variable) says
/// otherwise; the longest window accepted is `--max-valid-for-ms` (default 1000).
fn load_watchdog(args: &[String]) -> Result<watchdog::WatchdogConfig, String> {
    let mut config = watchdog::WatchdogConfig {
        require_validity: true,
        ..watchdog::WatchdogConfig::default()
    };
    if has_flag(args, "--allow-undated-commands")
        || env::var("HYDRA_UMC_HIL_BRIDGE_ALLOW_UNDATED_COMMANDS")
            .map(|v| v == "1")
            .unwrap_or(false)
    {
        config.require_validity = false;
    }
    if let Some(text) = find_flag(args, "--max-valid-for-ms") {
        let ms: u64 = text
            .parse()
            .map_err(|_| "--max-valid-for-ms must be a whole number of milliseconds".to_string())?;
        if ms == 0 || ms > 60_000 {
            return Err("--max-valid-for-ms must be between 1 and 60000".to_string());
        }
        config.max_valid_for_ms = ms;
    }
    Ok(config)
}

/// The joints and ranges of the robot, from `--urdf FILE` (also HYDRA_UMC_HIL_BRIDGE_URDF). With it a real command for an unknown joint, a
/// fixed one or a position outside its range is blocked; without it only the shape of the command is checked.
fn load_limits(args: &[String]) -> Result<Option<limits::JointLimits>, String> {
    let Some(path) = find_flag(args, "--urdf")
        .or_else(|| env::var("HYDRA_UMC_HIL_BRIDGE_URDF").ok())
        .filter(|path| !path.trim().is_empty())
    else {
        return Ok(None);
    };
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("cannot read the URDF {path}: {e}"))?;
    limits::JointLimits::from_urdf(&text)
        .map(Some)
        .map_err(|e| format!("{path}: {e}"))
}

/// How many requests are served at once, from `--workers` (1 to 256, default 8).
fn load_workers(args: &[String]) -> Result<usize, String> {
    match find_flag(args, "--workers") {
        None => Ok(server::DEFAULT_WORKERS),
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if (1..=256).contains(&n) => Ok(n),
            _ => Err(format!(
                "--workers must be a whole number from 1 to 256, got {raw}"
            )),
        },
    }
}

/// The link to HYDRA-UMC-TWIN (`--twin-url http://HOST:PORT` and its token): where the position of the real joints is reported. None when no
/// URL is given. The token is the one the twin asks for on `/interlock/*` (`--twin-state-token-file`, or HYDRA_UMC_HIL_BRIDGE_TWIN_TOKEN).
fn load_twin_link(
    args: &[String],
) -> Result<Option<std::sync::Arc<dyn twin_link::TwinStateSink>>, String> {
    let Some((addr, token, timeout_ms)) = twin_endpoint(args)? else {
        return Ok(None);
    };
    Ok(Some(std::sync::Arc::new(twin_link::HttpTwinSink {
        addr,
        token,
        timeout: std::time::Duration::from_millis(timeout_ms),
    })))
}

/// `--collision-shadow` (off by default): after a real command is delivered, ask the twin whether it predicts a contact. It needs the link
/// to the twin (`--twin-url`) and uses its address and token; `--collision-shadow-timeout-ms` (default 1000) is how long it waits.
fn load_collision_shadow(
    args: &[String],
) -> Result<Option<std::sync::Arc<dyn collision_shadow::CollisionShadow>>, String> {
    if !has_flag(args, "--collision-shadow") {
        if find_flag(args, "--collision-shadow-timeout-ms").is_some() {
            return Err("--collision-shadow-timeout-ms needs --collision-shadow".to_string());
        }
        if has_flag(args, "--collision-shadow-enforce") {
            return Err("--collision-shadow-enforce needs --collision-shadow".to_string());
        }
        return Ok(None);
    }
    let Some((addr, token, _)) = twin_endpoint(args)? else {
        return Err(
            "--collision-shadow needs --twin-url (and its token): the twin makes the prediction"
                .to_string(),
        );
    };
    let timeout_ms: u64 = match find_flag(args, "--collision-shadow-timeout-ms") {
        None => 1000,
        Some(raw) => match raw.parse::<u64>() {
            Ok(ms) if (10..=10_000).contains(&ms) => ms,
            _ => {
                return Err(format!(
                "--collision-shadow-timeout-ms must be a whole number from 10 to 10000, got {raw}"
            ))
            }
        },
    };
    Ok(Some(std::sync::Arc::new(
        collision_shadow::HttpCollisionShadow {
            addr,
            token,
            timeout: std::time::Duration::from_millis(timeout_ms),
        },
    )))
}

/// A bearer token for a service the bridge asks, from a file or from the environment; None when neither is given (the service may be open on
/// loopback). A token that is there must have at least 16 characters.
fn optional_token(
    args: &[String],
    file_flag: &str,
    env_name: &str,
) -> Result<Option<String>, String> {
    let token = match find_flag(args, file_flag) {
        Some(path) => Some(
            std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {file_flag}: {e}"))?
                .trim()
                .to_string(),
        ),
        None => env::var(env_name)
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty()),
    };
    match token {
        Some(t) if t.len() < 16 => Err(format!(
            "the token of {file_flag} must have at least 16 characters"
        )),
        other => Ok(other),
    }
}

/// A whole number of milliseconds from `flag`, between `low` and `high`; `default` when the flag is not there.
fn millis_flag(
    args: &[String],
    flag: &str,
    default: u64,
    low: u64,
    high: u64,
) -> Result<u64, String> {
    match find_flag(args, flag) {
        None => Ok(default),
        Some(raw) => match raw.parse::<u64>() {
            Ok(ms) if (low..=high).contains(&ms) => Ok(ms),
            _ => Err(format!(
                "{flag} must be a whole number from {low} to {high}, got {raw}"
            )),
        },
    }
}

/// `--safety-zones-url http://HOST:PORT` (off by default): the state of the protected zones is one more condition of the interlock of the
/// real mode. `--safety-zones-token-file` (or HYDRA_UMC_HIL_BRIDGE_ZONES_TOKEN), `--safety-zones-timeout-ms` (default 500) and
/// `--safety-zones-max-age-ms` (default 5000, the oldest state that is believed).
type ZonesSetting = (std::sync::Arc<dyn safety_zones::ZonesState>, u64);

fn load_safety_zones(args: &[String]) -> Result<Option<ZonesSetting>, String> {
    let Some(url) = find_flag(args, "--safety-zones-url") else {
        for flag in [
            "--safety-zones-token-file",
            "--safety-zones-timeout-ms",
            "--safety-zones-max-age-ms",
        ] {
            if find_flag(args, flag).is_some() {
                return Err(format!("{flag} needs --safety-zones-url"));
            }
        }
        return Ok(None);
    };
    let addr = http_client::parse_base_url(&url, "safety zones")?;
    let token = optional_token(
        args,
        "--safety-zones-token-file",
        "HYDRA_UMC_HIL_BRIDGE_ZONES_TOKEN",
    )?;
    let timeout_ms = millis_flag(args, "--safety-zones-timeout-ms", 500, 10, 10_000)?;
    let max_age_ms = millis_flag(args, "--safety-zones-max-age-ms", 5_000, 100, 60_000)?;
    Ok(Some((
        std::sync::Arc::new(safety_zones::HttpZonesState {
            addr,
            token,
            timeout: std::time::Duration::from_millis(timeout_ms),
        }),
        max_age_ms,
    )))
}

/// `--record-url http://HOST:PORT` (off by default): the commands that are decided and the positions that are read back are recorded in
/// HYDRA-UMC-DATALAKE as one session (`--record-session NAME`, default made from the clock). `--record-token-file` (or
/// HYDRA_UMC_HIL_BRIDGE_RECORD_TOKEN) is the ingest token of the lake and `--record-timeout-ms` (default 1000) how long it waits for it.
/// `--replay` additionally turns on `POST /replay` (reads a session back and runs it again in simulation); `--replay-max-points`
/// (default 20000) bounds what it reads.
fn load_recorder(
    args: &[String],
) -> Result<(Option<std::sync::Arc<session_record::Recorder>>, usize), String> {
    let replay_max_points = match find_flag(args, "--replay-max-points") {
        None => 0,
        Some(raw) => match raw.parse::<usize>() {
            Ok(n) if (1..=200_000).contains(&n) => n,
            _ => {
                return Err(format!(
                    "--replay-max-points must be a whole number from 1 to 200000, got {raw}"
                ))
            }
        },
    };
    let Some(url) = find_flag(args, "--record-url") else {
        for flag in [
            "--record-token-file",
            "--record-timeout-ms",
            "--record-session",
            "--replay-max-points",
        ] {
            if find_flag(args, flag).is_some() {
                return Err(format!("{flag} needs --record-url"));
            }
        }
        if has_flag(args, "--replay") {
            return Err(
                "--replay needs --record-url: the session is read from the data lake".to_string(),
            );
        }
        return Ok((None, 0));
    };
    let addr = http_client::parse_base_url(&url, "data lake")?;
    let token = optional_token(
        args,
        "--record-token-file",
        "HYDRA_UMC_HIL_BRIDGE_RECORD_TOKEN",
    )?;
    let timeout_ms = millis_flag(args, "--record-timeout-ms", 1_000, 10, 30_000)?;
    let session = find_flag(args, "--record-session").unwrap_or_else(|| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("s{now}")
    });
    let recorder = session_record::Recorder::start(
        &session,
        session_record::Lake {
            addr,
            token,
            timeout: std::time::Duration::from_millis(timeout_ms),
        },
        has_flag(args, "--replay"),
    )?;
    Ok((Some(std::sync::Arc::new(recorder)), replay_max_points))
}

/// `--sim-robot` (off by default): deliver the commands to the simulated robot of HYDRA-UMC-TWIN (started with `--sim-config`) instead of a
/// recording. It needs the link to the twin (`--twin-url` and its token), which it uses.
fn load_sim_robot(
    args: &[String],
) -> Result<Option<std::sync::Arc<twin_link::TwinSimSink>>, String> {
    if !has_flag(args, "--sim-robot") {
        return Ok(None);
    }
    let Some((addr, token, timeout_ms)) = twin_endpoint(args)? else {
        return Err(
            "--sim-robot needs --twin-url (and its token): the simulated robot is the twin's"
                .to_string(),
        );
    };
    Ok(Some(std::sync::Arc::new(twin_link::TwinSimSink {
        addr,
        token,
        timeout: std::time::Duration::from_millis(timeout_ms),
    })))
}

/// `host:port`, the token and the timeout of `--twin-url`; None when there is no URL.
fn twin_endpoint(args: &[String]) -> Result<Option<(String, String, u64)>, String> {
    let Some(url) = find_flag(args, "--twin-url") else {
        return Ok(None);
    };
    let addr = twin_link::HttpTwinSink::parse_url(&url)?;
    let token = match find_flag(args, "--twin-state-token-file") {
        Some(path) => std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read --twin-state-token-file: {e}"))?
            .trim()
            .to_string(),
        None => env::var("HYDRA_UMC_HIL_BRIDGE_TWIN_TOKEN")
            .map(|t| t.trim().to_string())
            .map_err(|_| "--twin-url needs --twin-state-token-file (or HYDRA_UMC_HIL_BRIDGE_TWIN_TOKEN): the twin asks for its token".to_string())?,
    };
    if token.len() < 16 {
        return Err("the twin token must have at least 16 characters".to_string());
    }
    let timeout_ms: u64 = match find_flag(args, "--twin-timeout-ms") {
        None => 500,
        Some(raw) => match raw.parse::<u64>() {
            Ok(ms) if (10..=10_000).contains(&ms) => ms,
            _ => {
                return Err(format!(
                    "--twin-timeout-ms must be a whole number from 10 to 10000, got {raw}"
                ))
            }
        },
    };
    Ok(Some((addr, token, timeout_ms)))
}

fn run_serve(args: &[String]) -> ExitCode {
    let addr = find_flag(args, "--addr").unwrap_or_else(|| "127.0.0.1".to_string());
    let port = find_flag(args, "--port").unwrap_or_else(|| "8113".to_string());
    let bind_addr = format!("{addr}:{port}");

    let token = match access::load_token(
        find_flag(args, "--token-file").as_deref(),
        env::var(access::TOKEN_ENV).ok().as_deref(),
    ) {
        Ok(token) => token,
        Err(e) => {
            eprintln!("[hil-bridge] fatal: {e}");
            return ExitCode::from(2);
        }
    };
    // The signing key of HYDRA-UMC-SERVER, from a file: with it a signed token of the server is accepted, nothing is open (GET /health and GET /stats
    // included) and a viewer may read but not command. Off unless it is given.
    let identity_key = match find_flag(args, "--identity-key-file")
        .or_else(|| env::var("HYDRA_UMC_HIL_BRIDGE_IDENTITY_KEY_FILE").ok())
        .filter(|path| !path.is_empty())
    {
        Some(path) => match server_identity::load_identity_key(&path) {
            Ok(key) => Some(key),
            Err(e) => {
                eprintln!("[hil-bridge] fatal: --identity-key-file: {e}");
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    if identity_key.is_none() {
        if let Some(problem) = access::bind_problem(&addr, &token) {
            eprintln!("[hil-bridge] fatal: {problem}");
            return ExitCode::from(2);
        }
    } else {
        eprintln!("[hil-bridge] tokens of HYDRA-UMC-SERVER accepted: every route needs an identity, a viewer may read but not command");
    }

    let interlock = match load_interlock(args) {
        Ok(interlock) => interlock,
        Err(message) => {
            eprintln!("[hil-bridge] fatal: {message}");
            return ExitCode::from(2);
        }
    };
    let twin_pass = match load_twin_pass(args, &interlock) {
        Ok(twin_pass) => twin_pass,
        Err(message) => {
            eprintln!("[hil-bridge] fatal: {message}");
            return ExitCode::from(2);
        }
    };
    let watchdog = match load_watchdog(args) {
        Ok(watchdog) => watchdog,
        Err(message) => {
            eprintln!("[hil-bridge] fatal: {message}");
            return ExitCode::from(2);
        }
    };
    let limits = match load_limits(args) {
        Ok(limits) => limits,
        Err(message) => {
            eprintln!("[hil-bridge] fatal: {message}");
            return ExitCode::from(2);
        }
    };
    let workers = match load_workers(args) {
        Ok(workers) => workers,
        Err(message) => {
            eprintln!("[hil-bridge] fatal: {message}");
            return ExitCode::from(2);
        }
    };
    let audit_log = find_flag(args, "--audit-log")
        .or_else(|| env::var("HYDRA_UMC_HIL_BRIDGE_AUDIT_LOG").ok())
        .filter(|path| !path.trim().is_empty())
        .map(std::path::PathBuf::from);

    let twin = match load_twin_link(args) {
        Ok(twin) => twin,
        Err(e) => {
            eprintln!("[hil-bridge] fatal: {e}");
            return ExitCode::from(2);
        }
    };

    let collision_shadow = match load_collision_shadow(args) {
        Ok(shadow) => shadow,
        Err(e) => {
            eprintln!("[hil-bridge] fatal: {e}");
            return ExitCode::from(2);
        }
    };

    let safety_zones = match load_safety_zones(args) {
        Ok(zones) => zones,
        Err(e) => {
            eprintln!("[hil-bridge] fatal: {e}");
            return ExitCode::from(2);
        }
    };
    let (recorder, replay_max_points) = match load_recorder(args) {
        Ok(recorder) => recorder,
        Err(e) => {
            eprintln!("[hil-bridge] fatal: {e}");
            return ExitCode::from(2);
        }
    };

    let sim_robot = match load_sim_robot(args) {
        Ok(sim) => sim,
        Err(e) => {
            eprintln!("[hil-bridge] fatal: {e}");
            return ExitCode::from(2);
        }
    };
    if sim_robot.is_some() {
        eprintln!("[hil-bridge] --sim-robot: the commands are delivered to the simulated robot of the twin (a model, no robot behind it); the interlock, the limits and the watchdog work as they do with a robot");
    }

    match server::bind(&bind_addr) {
        Ok(bound) => {
            eprintln!(
                "[hil-bridge] HTTP API listening on {bind_addr} ({})",
                if token.is_some() {
                    "Bearer token required"
                } else {
                    "no token, loopback only"
                }
            );
            eprintln!(
                "[hil-bridge] POST /route, POST /mirror, POST /measured, GET /stats, GET /audit, GET and POST /settings/twin-pass"
            );
            match &twin {
                Some(_) => eprintln!("[hil-bridge] the position of the real joints is reported to the twin (--twin-url); GET /stats says whether the last report arrived"),
                None => eprintln!("[hil-bridge] no --twin-url: the twin is not told where the real joints are (start the twin with --require-fresh-state-ms to make it insist)"),
            }
            match &collision_shadow {
                Some(_) if has_flag(args, "--collision-shadow-enforce") => eprintln!("[hil-bridge] --collision-shadow-enforce: the twin is asked BEFORE each real command is routed; a predicted contact, or no answer inside the wait budget, refuses the command (never sent unchecked)"),
                Some(_) => eprintln!("[hil-bridge] --collision-shadow: after each delivered real command the twin is asked whether it predicts a contact; the answer is recorded and never blocks anything"),
                None => eprintln!("[hil-bridge] no --collision-shadow: the collision prediction of the twin is not asked"),
            }
            match &safety_zones {
                Some((_, max_age_ms)) => eprintln!("[hil-bridge] --safety-zones-url: a real command is forwarded only while the zone monitor says READY and said it at most {max_age_ms} ms ago; any other state, or no answer, refuses it (the physical stop stays with the firmware)"),
                None => eprintln!("[hil-bridge] no --safety-zones-url: the state of the protected zones is not part of the decision"),
            }
            match &recorder {
                Some(r) if r.replay => eprintln!("[hil-bridge] --record-url: session {:?} is recorded in the data lake; POST /replay is ON (it runs a session again in simulation, never in real mode)", r.session()),
                Some(r) => eprintln!("[hil-bridge] --record-url: session {:?} is recorded in the data lake; no --replay, POST /replay is off", r.session()),
                None => eprintln!("[hil-bridge] no --record-url: nothing is recorded"),
            }
            let fault_injection = has_flag(args, "--fault-injection");
            if fault_injection {
                eprintln!("[hil-bridge] simulated transport fault injection is ENABLED");
            }
            if interlock.trusted_keys.is_empty() {
                eprintln!("[hil-bridge] no twin public key (--twin-public-key-file): the real mode blocks every command until one is configured");
            } else {
                eprintln!("[hil-bridge] real commands need a certificate from the twin: trusted key(s) {:?}, at most {} ms old", interlock.trusted_keys.key_ids(), interlock.policy.max_age_ms);
            }
            if twin_pass.required {
                eprintln!("[hil-bridge] a real command also needs a pass verdict of the twin for its trajectory (robot {:?}){}", twin_pass.robot_id, if twin_pass.locked { "; --require-twin-pass: the switch cannot be turned off while the bridge runs" } else { "" });
            } else {
                eprintln!("[hil-bridge] the pass verdict of the twin is not required (require_twin_pass is off; --require-twin-pass or POST /settings/twin-pass turns it on)");
            }
            if interlock.allow_unsigned_risk {
                eprintln!("[hil-bridge] WARNING: --allow-unsigned-risk is ON: a risk report written by the caller is believed in real mode (bench use only)");
            }
            if watchdog.require_validity {
                eprintln!("[hil-bridge] a real command needs its validity window (issued_at_ms, valid_for_ms); a joint whose last command ran out is stopped by the watchdog (windows up to {} ms)", watchdog.max_valid_for_ms);
            } else {
                eprintln!("[hil-bridge] WARNING: --allow-undated-commands is ON: a real command without a validity window stays in force if its producer stops");
            }
            match &limits {
                Some(l) => eprintln!("[hil-bridge] {} joint(s) with their ranges read from the URDF: a real command for another joint, a fixed one or a position outside its range is blocked", l.len()),
                None => eprintln!("[hil-bridge] no --urdf: the joints and their ranges are not known, only the shape of a command is checked"),
            }
            eprintln!("[hil-bridge] serving with {workers} worker thread(s)");
            match &audit_log {
                Some(path) => eprintln!("[hil-bridge] every real order is recorded in {} before it is decided; if the record cannot be written the order is refused", path.display()),
                None => eprintln!("[hil-bridge] no --audit-log: the last real orders are kept in memory only (GET /audit)"),
            }
            server::run_with_config(
                bound,
                server::ServerConfig {
                    fault_injection,
                    token,
                    identity_key,
                    health_open: access::health_open_requested(
                        has_flag(args, "--health-open"),
                        env::var(access::HEALTH_OPEN_ENV).ok().as_deref(),
                    ),
                    interlock,
                    twin_pass,
                    audit_log,
                    watchdog,
                    limits,
                    workers,
                    twin,
                    shadow_enforce: collision_shadow.is_some()
                        && has_flag(args, "--collision-shadow-enforce"),
                    collision_shadow,
                    sim_robot,
                    zones_max_age_ms: safety_zones.as_ref().map_or(0, |(_, age)| *age),
                    safety_zones: safety_zones.map(|(zones, _)| zones),
                    recorder,
                    replay_max_points,
                },
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[hil-bridge] fatal: could not start HTTP server on {bind_addr}: {e}");
            ExitCode::from(2)
        }
    }
}

/// Sets the rigid-body simulation of HYDRA-UMC-PHYSICS-REPLICA against joint positions recorded from a real arm (`compare-dynamics`): exit 0 when every joint stays
/// within `--max-deviation` (when given), 1 when one does not, 2 for bad input. The numbers are those of a model until a real arm has been compared.
fn run_compare_dynamics(args: &[String]) -> ExitCode {
    let (Some(simulator), Some(urdf), Some(recording)) = (
        find_flag(args, "--simulator"),
        find_flag(args, "--urdf"),
        find_flag(args, "--recording"),
    ) else {
        eprintln!(
            "compare-dynamics: --simulator PATH, --urdf PATH and --recording FILE are required"
        );
        return ExitCode::from(2);
    };
    let mut extra = Vec::new();
    for flag in [
        "--joints",
        "--velocities",
        "--torques",
        "--duration",
        "--dt",
    ] {
        if let Some(value) = find_flag(args, flag) {
            extra.push(flag.to_string());
            extra.push(value);
        }
    }
    let limit = match find_flag(args, "--max-deviation") {
        None => None,
        Some(text) => match text.trim().parse::<f64>() {
            Ok(v) if v.is_finite() && v >= 0.0 => Some(v),
            _ => {
                eprintln!("compare-dynamics: --max-deviation wants a finite number, zero or more");
                return ExitCode::from(2);
            }
        },
    };
    let simulated = match dynamics_link::run_simulator(&simulator, &urdf, &extra) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("compare-dynamics: {e}");
            return ExitCode::from(2);
        }
    };
    let text = match std::fs::read_to_string(&recording) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("compare-dynamics: the recording cannot be read: {e}");
            return ExitCode::from(2);
        }
    };
    let recorded = match dynamics_link::parse_recording(&text, &simulated.joints) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("compare-dynamics: {e}");
            return ExitCode::from(2);
        }
    };
    let result = match dynamics_link::deviations(&simulated, &recorded) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("compare-dynamics: {e}");
            return ExitCode::from(2);
        }
    };
    eprintln!("note: the simulation is a model (inertial blocks and gravity, no friction, no motor dynamics); evidence = simulator");
    let within = limit.is_none_or(|l| result.iter().all(|j| j.max_abs <= l));
    let rows: Vec<serde_json::Value> = result
        .iter()
        .map(|j| serde_json::json!({"joint": j.joint, "max_abs": j.max_abs, "rms": j.rms, "compared": j.compared}))
        .collect();
    println!(
        "{}",
        serde_json::json!({"within_limit": within, "joints": rows})
    );
    if within {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("route") => run_route(&args[1..]),
        Some("mirror") => run_mirror(&args[1..]),
        Some("serve") => run_serve(&args[1..]),
        Some("compare-dynamics") => run_compare_dynamics(&args[1..]),
        _ => {
            println!("{PROJECT_NAME} v{VERSION}");
            println!("{ROLE}");
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_flag_does_not_take_the_next_flag_as_its_value() {
        let a = args(&["--joint", "--position", "1"]);
        assert_eq!(find_flag(&a, "--joint"), None);
        assert_eq!(find_flag(&a, "--position"), Some("1".to_string()));
        assert!(parse_command(&a).is_err());
    }

    #[test]
    fn the_number_of_workers_is_a_whole_number_from_1_to_256() {
        assert_eq!(load_workers(&args(&[])), Ok(server::DEFAULT_WORKERS));
        assert_eq!(load_workers(&args(&["--workers", "3"])), Ok(3));
        for bad in ["0", "257", "x", "-1"] {
            assert!(load_workers(&args(&["--workers", bad])).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_missing_or_wrong_urdf_stops_the_start() {
        assert_eq!(load_limits(&args(&[])), Ok(None));
        assert!(load_limits(&args(&["--urdf", "/no/such/file.urdf"])).is_err());
    }

    #[test]
    fn a_command_with_a_non_finite_position_is_refused() {
        for bad in ["NaN", "inf", "-inf"] {
            let a = args(&["--joint", "shoulder", "--position", bad]);
            assert!(parse_command(&a).is_err(), "position {bad}");
        }
        let ok = args(&["--joint", "shoulder", "--position", "1.5"]);
        assert!(parse_command(&ok).is_ok());
    }

    #[test]
    fn the_collision_shadow_is_off_and_needs_the_twin_link() {
        assert!(load_collision_shadow(&args(&[])).unwrap().is_none());
        // its options mean nothing without the switch, and the switch means nothing without a twin to ask
        assert!(load_collision_shadow(&args(&["--collision-shadow-timeout-ms", "100"])).is_err());
        let no_twin = load_collision_shadow(&args(&["--collision-shadow"])).unwrap_err();
        assert!(no_twin.contains("--twin-url"), "{no_twin}");
    }

    #[test]
    fn the_safety_zones_are_off_and_their_options_need_the_url() {
        assert!(load_safety_zones(&args(&[])).unwrap().is_none());
        for flag in [
            "--safety-zones-timeout-ms",
            "--safety-zones-max-age-ms",
            "--safety-zones-token-file",
        ] {
            assert!(load_safety_zones(&args(&[flag, "500"])).is_err(), "{flag}");
        }
        let (_, age) = load_safety_zones(&args(&["--safety-zones-url", "http://127.0.0.1:18300"]))
            .unwrap()
            .unwrap();
        assert_eq!(age, 5_000);
        let (_, age) = load_safety_zones(&args(&[
            "--safety-zones-url",
            "http://127.0.0.1:18300",
            "--safety-zones-max-age-ms",
            "2000",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(age, 2_000);
        for bad in ["50", "70000", "soon"] {
            let a = args(&[
                "--safety-zones-url",
                "http://127.0.0.1:18300",
                "--safety-zones-max-age-ms",
                bad,
            ]);
            assert!(load_safety_zones(&a).is_err(), "{bad}");
        }
        assert!(load_safety_zones(&args(&["--safety-zones-url", "https://h:1"])).is_err());
    }

    #[test]
    fn the_session_record_is_off_and_replay_needs_it() {
        assert!(load_recorder(&args(&[])).unwrap().0.is_none());
        for flags in [
            vec!["--replay"],
            vec!["--record-session", "a"],
            vec!["--record-timeout-ms", "100"],
            vec!["--replay-max-points", "10"],
        ] {
            assert!(load_recorder(&args(&flags)).is_err(), "{flags:?}");
        }
        let (recorder, max) = load_recorder(&args(&[
            "--record-url",
            "http://127.0.0.1:1",
            "--record-session",
            "bench-1",
            "--replay",
            "--replay-max-points",
            "500",
        ]))
        .unwrap();
        let recorder = recorder.unwrap();
        assert_eq!(recorder.session(), "bench-1");
        assert!(recorder.replay);
        assert_eq!(max, 500);
        assert!(load_recorder(&args(&[
            "--record-url",
            "http://127.0.0.1:1",
            "--record-session",
            "a b"
        ]))
        .is_err());
        let (default_session, _) =
            load_recorder(&args(&["--record-url", "http://127.0.0.1:1"])).unwrap();
        assert!(!default_session.unwrap().replay);
    }
}
