// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/bridge.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
// =============================================================================
//! Real bidirectional routing: which sink a `JointCommand` actually
//! reaches, and whether the safety interlock (`interlock.rs`) blocks it
//! first. `CommandSink` is a trait rather than a concrete gRPC/WebSocket
//! client because neither transport is wired in yet (see `Cargo.toml`) -
//! `RecordingSink` is the one always-succeeds implementation, and
//! `SimulatedTransport` is a second implementation that can model a slow
//! or disconnected link (configurable latency/timeout/connection state)
//! so the bridge's fail-safe behavior is testable without any real
//! hardware or transport. Both are honest about not transmitting
//! anywhere real: the same reasoning as `NullEStopRequester` in
//! HYDRA-UMC-SAFETY-ZONES.

use std::fmt;

use crate::interlock::{assess_interlock, InterlockDecision, TwinRiskReport};
use crate::protocol::{JointCommand, Mode};

/// A transport-level failure to deliver a command - distinct from an
/// `InterlockDecision::Block` (a deliberate safety refusal). Either one
/// must stop the bridge from claiming the command reached real hardware.
#[derive(Debug, Clone, PartialEq)]
pub enum TransportError {
    /// The send did not complete within the transport's own timeout.
    Timeout { after_ms: u64 },
    /// The transport has no live connection to send on at all.
    Disconnected,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Timeout { after_ms } => {
                write!(f, "transport timed out after {after_ms}ms")
            }
            TransportError::Disconnected => write!(f, "transport is disconnected"),
        }
    }
}

pub trait CommandSink {
    fn send(&mut self, command: &JointCommand) -> Result<(), TransportError>;
}

#[derive(Debug, Default)]
pub struct RecordingSink {
    pub received: Vec<JointCommand>,
}

impl CommandSink for RecordingSink {
    fn send(&mut self, command: &JointCommand) -> Result<(), TransportError> {
        self.received.push(command.clone());
        Ok(())
    }
}

/// A transport stand-in that can simulate the two ways a real link fails
/// without needing real hardware to provoke them: a connection that
/// isn't there (`connected = false`) and a link that is too slow
/// (`latency_ms > timeout_ms`). Successful sends are still recorded, so
/// tests can assert on what actually got through.
#[derive(Debug)]
pub struct SimulatedTransport {
    pub connected: bool,
    pub latency_ms: u64,
    pub timeout_ms: u64,
    pub received: Vec<JointCommand>,
}

impl SimulatedTransport {
    /// A transport with no artificial latency or timeout - every send
    /// that reaches it succeeds. Verifiable timeouts are opted into via
    /// `with_timeout`.
    pub fn healthy() -> Self {
        SimulatedTransport {
            connected: true,
            latency_ms: 0,
            timeout_ms: u64::MAX,
            received: Vec::new(),
        }
    }

    pub fn with_timeout(mut self, latency_ms: u64, timeout_ms: u64) -> Self {
        self.latency_ms = latency_ms;
        self.timeout_ms = timeout_ms;
        self
    }

    pub fn disconnected() -> Self {
        SimulatedTransport {
            connected: false,
            latency_ms: 0,
            timeout_ms: u64::MAX,
            received: Vec::new(),
        }
    }
}

impl CommandSink for SimulatedTransport {
    fn send(&mut self, command: &JointCommand) -> Result<(), TransportError> {
        if !self.connected {
            return Err(TransportError::Disconnected);
        }
        if self.latency_ms > self.timeout_ms {
            return Err(TransportError::Timeout {
                after_ms: self.timeout_ms,
            });
        }
        self.received.push(command.clone());
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RouteOutcome {
    SentReal,
    SentSimulation,
    BlockedByInterlock {
        reason: String,
    },
    /// The interlock allowed the command (or simulation mode never gates
    /// it), but the transport itself failed to deliver it - fail-safe:
    /// this is never reported as `SentReal`/`SentSimulation`, so a caller
    /// can never mistake a dropped command for a delivered one.
    TransportFailure {
        reason: String,
    },
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
            Mode::Simulation => match sim_sink.send(&command) {
                Ok(()) => RouteOutcome::SentSimulation,
                Err(e) => RouteOutcome::TransportFailure {
                    reason: e.to_string(),
                },
            },
            Mode::Real => {
                let risk = risk.unwrap_or(TwinRiskReport {
                    collision_imminent: false,
                    distance_m: f64::INFINITY,
                });
                match assess_interlock(&risk) {
                    InterlockDecision::Allow => match real_sink.send(&command) {
                        Ok(()) => RouteOutcome::SentReal,
                        Err(e) => RouteOutcome::TransportFailure {
                            reason: e.to_string(),
                        },
                    },
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
    /// simulated one back out) is observation, not actuation. Still
    /// reports a transport failure rather than swallowing it, so a
    /// dropped shadow update is never silently mistaken for a delivered
    /// one.
    pub fn mirror_command(
        &self,
        command: &JointCommand,
        mirror_sink: &mut dyn CommandSink,
    ) -> Result<(), TransportError> {
        mirror_sink.send(command)
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
        let result = bridge.mirror_command(&cmd("j1", 1.0), &mut mirror);
        assert_eq!(result, Ok(()));
        assert_eq!(mirror.received.len(), 1);
    }

    #[test]
    fn simulated_transport_healthy_delivers_the_command() {
        let mut transport = SimulatedTransport::healthy();
        let result = transport.send(&cmd("j1", 0.5));
        assert_eq!(result, Ok(()));
        assert_eq!(transport.received.len(), 1);
    }

    #[test]
    fn simulated_transport_disconnected_fails_safe_without_hardware() {
        let mut transport = SimulatedTransport::disconnected();
        let result = transport.send(&cmd("j1", 0.5));
        assert_eq!(result, Err(TransportError::Disconnected));
        assert!(transport.received.is_empty());
    }

    #[test]
    fn simulated_transport_verifiable_timeout_when_latency_exceeds_budget() {
        let mut transport = SimulatedTransport::healthy().with_timeout(150, 100);
        let result = transport.send(&cmd("j1", 0.5));
        assert_eq!(result, Err(TransportError::Timeout { after_ms: 100 }));
        assert!(transport.received.is_empty());
    }

    #[test]
    fn simulated_transport_latency_at_exactly_the_timeout_still_succeeds() {
        // Boundary: latency == timeout counts as still within budget, not over it.
        let mut transport = SimulatedTransport::healthy().with_timeout(100, 100);
        let result = transport.send(&cmd("j1", 0.5));
        assert_eq!(result, Ok(()));
        assert_eq!(transport.received.len(), 1);
    }

    #[test]
    fn simulated_transport_preserves_confirmed_command_order() {
        // This is evidence for the local transport contract only: commands
        // confirmed by one sink retain their issuance order. A real network
        // transport must provide its own sequence/acknowledgement guarantee
        // before it can claim this property across a connection.
        let mut transport = SimulatedTransport::healthy();
        transport.send(&cmd("j1", 0.1)).unwrap();
        transport.send(&cmd("j2", 0.2)).unwrap();
        assert_eq!(transport.received, vec![cmd("j1", 0.1), cmd("j2", 0.2)]);
    }

    #[test]
    fn real_mode_transport_timeout_never_reports_sent_real() {
        // The safe-failure path this vital improvement closes: interlock
        // allows the command (no risk reported), but the transport itself
        // times out - the bridge must not claim delivery happened.
        let bridge = Bridge::new(Mode::Real);
        let mut real = SimulatedTransport::healthy().with_timeout(500, 100);
        let mut sim = RecordingSink::default();
        let outcome = bridge.route_command(cmd("j1", 0.5), None, &mut real, &mut sim);
        assert_eq!(
            outcome,
            RouteOutcome::TransportFailure {
                reason: "transport timed out after 100ms".to_string()
            }
        );
        assert!(real.received.is_empty());
    }

    #[test]
    fn real_mode_disconnected_transport_never_reports_sent_real() {
        let bridge = Bridge::new(Mode::Real);
        let mut real = SimulatedTransport::disconnected();
        let mut sim = RecordingSink::default();
        let outcome = bridge.route_command(cmd("j1", 0.5), None, &mut real, &mut sim);
        assert_eq!(
            outcome,
            RouteOutcome::TransportFailure {
                reason: "transport is disconnected".to_string()
            }
        );
    }

    #[test]
    fn interlock_block_takes_precedence_over_transport_state() {
        // Even a perfectly healthy transport must not be used once the
        // interlock has already decided to block - the interlock check
        // runs first and short-circuits.
        let bridge = Bridge::new(Mode::Real);
        let mut real = SimulatedTransport::healthy();
        let mut sim = RecordingSink::default();
        let risk = Some(TwinRiskReport {
            collision_imminent: true,
            distance_m: 0.01,
        });
        let outcome = bridge.route_command(cmd("j1", 0.5), risk, &mut real, &mut sim);
        assert!(matches!(outcome, RouteOutcome::BlockedByInterlock { .. }));
        assert!(real.received.is_empty());
    }
}
