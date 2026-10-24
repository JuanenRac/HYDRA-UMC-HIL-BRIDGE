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

mod access;
mod audit;
mod bridge;
mod certificate;
mod interlock;
mod protocol;
mod server;
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
    if let Some(problem) = access::bind_problem(&addr, &token) {
        eprintln!("[hil-bridge] fatal: {problem}");
        return ExitCode::from(2);
    }

    let interlock = match load_interlock(args) {
        Ok(interlock) => interlock,
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
    let audit_log = find_flag(args, "--audit-log")
        .or_else(|| env::var("HYDRA_UMC_HIL_BRIDGE_AUDIT_LOG").ok())
        .filter(|path| !path.trim().is_empty())
        .map(std::path::PathBuf::from);

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
            eprintln!("[hil-bridge] POST /route, POST /mirror, GET /stats, GET /audit");
            let fault_injection = has_flag(args, "--fault-injection");
            if fault_injection {
                eprintln!("[hil-bridge] simulated transport fault injection is ENABLED");
            }
            if interlock.trusted_keys.is_empty() {
                eprintln!("[hil-bridge] no twin public key (--twin-public-key-file): the real mode blocks every command until one is configured");
            } else {
                eprintln!("[hil-bridge] real commands need a certificate from the twin: trusted key(s) {:?}, at most {} ms old", interlock.trusted_keys.key_ids(), interlock.policy.max_age_ms);
            }
            if interlock.allow_unsigned_risk {
                eprintln!("[hil-bridge] WARNING: --allow-unsigned-risk is ON: a risk report written by the caller is believed in real mode (bench use only)");
            }
            if watchdog.require_validity {
                eprintln!("[hil-bridge] a real command needs its validity window (issued_at_ms, valid_for_ms); a joint whose last command ran out is stopped by the watchdog (windows up to {} ms)", watchdog.max_valid_for_ms);
            } else {
                eprintln!("[hil-bridge] WARNING: --allow-undated-commands is ON: a real command without a validity window stays in force if its producer stops");
            }
            match &audit_log {
                Some(path) => eprintln!("[hil-bridge] every real order is recorded in {} before it is decided; if the record cannot be written the order is refused", path.display()),
                None => eprintln!("[hil-bridge] no --audit-log: the last real orders are kept in memory only (GET /audit)"),
            }
            server::run_with_config(
                bound,
                server::ServerConfig {
                    fault_injection,
                    token,
                    interlock,
                    audit_log,
                    watchdog,
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

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("route") => run_route(&args[1..]),
        Some("mirror") => run_mirror(&args[1..]),
        Some("serve") => run_serve(&args[1..]),
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
    fn a_command_with_a_non_finite_position_is_refused() {
        for bad in ["NaN", "inf", "-inf"] {
            let a = args(&["--joint", "shoulder", "--position", bad]);
            assert!(parse_command(&a).is_err(), "position {bad}");
        }
        let ok = args(&["--joint", "shoulder", "--position", "1.5"]);
        assert!(parse_command(&ok).is_ok());
    }
}
