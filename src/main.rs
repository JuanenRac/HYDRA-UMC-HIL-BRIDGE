// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/main.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
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

mod bridge;
mod interlock;
mod protocol;

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
    Ok(JointCommand { joint, position })
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
        let distance_m: f64 = find_flag(args, "--distance")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        Some(TwinRiskReport {
            collision_imminent: true,
            distance_m,
        })
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

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("route") => run_route(&args[1..]),
        Some("mirror") => run_mirror(&args[1..]),
        _ => {
            println!("{PROJECT_NAME} v{VERSION}");
            println!("{ROLE}");
            ExitCode::SUCCESS
        }
    }
}
