// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/bridge.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
// =============================================================================
//! Real bidirectional routing: which sink a `JointCommand` actually
//! reaches, and whether the safety interlock (`interlock.rs`) blocks it
//! first. `CommandSink` is a trait rather than a concrete gRPC/WebSocket
//! client because neither transport is wired in yet (see `Cargo.toml`) -
//! `RecordingSink` is the one real implementation today, and it is
//! honest about not transmitting anywhere: exactly the same reasoning as
//! `NullEStopRequester` in HYDRA-UMC-SAFETY-ZONES.

use crate::interlock::{assess_interlock, InterlockDecision, TwinRiskReport};
use crate::protocol::{JointCommand, Mode};

pub trait CommandSink {
    fn send(&mut self, command: &JointCommand);
}

#[derive(Debug, Default)]
pub struct RecordingSink {
    pub received: Vec<JointCommand>,
}

impl CommandSink for RecordingSink {
    fn send(&mut self, command: &JointCommand) {
        self.received.push(command.clone());
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RouteOutcome {
    SentReal,
    SentSimulation,
    BlockedByInterlock { reason: String },
}

pub struct Bridge {
    pub mode: Mode,
}

impl Bridge {
    pub fn new(mode: Mode) -> Self {
        Bridge { mode }
    }

    /// Routes one command according to the bridge's current mode. A
    /// `Real`-mode command is only forwarded to `real_sink` if the
    /// interlock allows it; `risk` of `None` is treated the same as "no
    /// imminent collision reported" - an honest default for when no twin
    /// is connected yet, not a silent bypass of the check itself.
    /// `Simulation`-mode commands always reach `sim_sink`, ungated - see
    /// `protocol::Mode::Simulation`'s own docs for why.
    pub fn route_command(
        &self,
        command: JointCommand,
        risk: Option<TwinRiskReport>,
        real_sink: &mut dyn CommandSink,
        sim_sink: &mut dyn CommandSink,
    ) -> RouteOutcome {
        match self.mode {
            Mode::Simulation => {
                sim_sink.send(&command);
                RouteOutcome::SentSimulation
            }
            Mode::Real => {
                let risk = risk.unwrap_or(TwinRiskReport {
                    collision_imminent: false,
                    distance_m: f64::INFINITY,
                });
                match assess_interlock(&risk) {
                    InterlockDecision::Allow => {
                        real_sink.send(&command);
                        RouteOutcome::SentReal
                    }
                    InterlockDecision::Block { reason } => {
                        RouteOutcome::BlockedByInterlock { reason }
                    }
                }
            }
        }
    }

    /// Real-vs-virtual shadowing: mirrors a command onto `mirror_sink`
    /// unconditionally, independent of `self.mode` and never gated by the
    /// interlock - shadowing a real robot's motion into the twin (or a
    /// simulated one back out) is observation, not actuation.
    pub fn mirror_command(&self, command: &JointCommand, mirror_sink: &mut dyn CommandSink) {
        mirror_sink.send(command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(joint: &str, position: f64) -> JointCommand {
        JointCommand {
            joint: joint.to_string(),
            position,
        }
    }

    #[test]
    fn simulation_mode_always_routes_to_sim_sink() {
        let bridge = Bridge::new(Mode::Simulation);
        let mut real = RecordingSink::default();
        let mut sim = RecordingSink::default();
        let risk = Some(TwinRiskReport {
            collision_imminent: true,
            distance_m: 0.0,
        });
        let outcome = bridge.route_command(cmd("j1", 0.5), risk, &mut real, &mut sim);
        assert_eq!(outcome, RouteOutcome::SentSimulation);
        assert_eq!(sim.received.len(), 1);
        assert!(real.received.is_empty());
    }

    #[test]
    fn real_mode_routes_to_real_sink_when_no_risk() {
        let bridge = Bridge::new(Mode::Real);
        let mut real = RecordingSink::default();
        let mut sim = RecordingSink::default();
        let outcome = bridge.route_command(cmd("j1", 0.5), None, &mut real, &mut sim);
        assert_eq!(outcome, RouteOutcome::SentReal);
        assert_eq!(real.received.len(), 1);
        assert!(sim.received.is_empty());
    }

    #[test]
    fn real_mode_blocked_by_interlock_never_reaches_real_sink() {
        let bridge = Bridge::new(Mode::Real);
        let mut real = RecordingSink::default();
        let mut sim = RecordingSink::default();
        let risk = Some(TwinRiskReport {
            collision_imminent: true,
            distance_m: 0.01,
        });
        let outcome = bridge.route_command(cmd("j1", 0.5), risk, &mut real, &mut sim);
        assert!(matches!(outcome, RouteOutcome::BlockedByInterlock { .. }));
        assert!(real.received.is_empty());
    }

    #[test]
    fn mirror_reaches_sink_regardless_of_mode() {
        let bridge = Bridge::new(Mode::Real);
        let mut mirror = RecordingSink::default();
        bridge.mirror_command(&cmd("j1", 1.0), &mut mirror);
        assert_eq!(mirror.received.len(), 1);
    }
}
