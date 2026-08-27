// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/interlock.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
// =============================================================================
//! Real safety interlock logic: the "Safety Interlock" feature this
//! README already advertised before any of it existed in code -
//! "Blocks physical execution if the simulated twin detects an
//! impending collision."
//!
//! This is deliberately independent of any real Twin/physics integration:
//! it takes a `TwinRiskReport` (whatever produces one - today a test, one
//! day HYDRA-UMC-TWIN's real physics tick) and makes a real, testable
//! decision from it. It is *advisory-blocking* only, in the same spirit
//! as HYDRA-UMC-SAFETY-ZONES's own detect-vs-enforce boundary: this
//! bridge can refuse to *forward* a real command, but the actual,
//! physical motor-power enforcement still belongs to HYDRA-UMC (the
//! firmware) alone.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TwinRiskReport {
    pub collision_imminent: bool,
    pub distance_m: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InterlockDecision {
    Allow,
    Block { reason: String },
}

/// Real decision: a real command bound for real hardware is blocked
/// whenever the twin reports an imminent collision, regardless of
/// reported distance (a `collision_imminent` twin report is already the
/// twin's own conclusion, not a raw sensor number this module should
/// second-guess with its own threshold).
pub fn assess_interlock(risk: &TwinRiskReport) -> InterlockDecision {
    if risk.collision_imminent {
        InterlockDecision::Block {
            reason: format!("twin reports imminent collision at {:.3}m", risk.distance_m),
        }
    } else {
        InterlockDecision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_risk_allows() {
        let risk = TwinRiskReport {
            collision_imminent: false,
            distance_m: 2.0,
        };
        assert_eq!(assess_interlock(&risk), InterlockDecision::Allow);
    }

    #[test]
    fn imminent_collision_blocks() {
        let risk = TwinRiskReport {
            collision_imminent: true,
            distance_m: 0.02,
        };
        match assess_interlock(&risk) {
            InterlockDecision::Block { reason } => assert!(reason.contains("0.020")),
            InterlockDecision::Allow => panic!("expected Block"),
        }
    }

    #[test]
    fn far_distance_without_imminent_flag_still_allows() {
        // Honest: this module trusts the twin's own collision_imminent
        // conclusion rather than re-deriving one from distance alone.
        let risk = TwinRiskReport {
            collision_imminent: false,
            distance_m: 0.001,
        };
        assert_eq!(assess_interlock(&risk), InterlockDecision::Allow);
    }
}
