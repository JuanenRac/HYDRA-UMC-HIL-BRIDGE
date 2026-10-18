// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/interlock.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
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

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TwinRiskReport {
    pub collision_imminent: bool,
    pub distance_m: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InterlockDecision {
    Allow,
    Block { reason: String },
}

/// The least clearance, in meters, the twin may report before a real command is blocked even though it did not raise `collision_imminent`: a twin that says "no collision" at
/// one millimetre is not saying the path is safe. 5 cm is a conservative floor for a bench micro-factory, not a certified safety distance (the firmware owns that).
pub const MIN_CLEARANCE_M: f64 = 0.05;

/// Real decision on a twin report that IS present: a real command bound for real hardware is blocked when the twin reports an imminent collision, and also when the report cannot be
/// trusted as a number (NaN) or reports less than `MIN_CLEARANCE_M` of clearance.
pub fn assess_interlock(risk: &TwinRiskReport) -> InterlockDecision {
    if risk.collision_imminent {
        return InterlockDecision::Block {
            reason: format!("twin reports imminent collision at {:.3}m", risk.distance_m),
        };
    }
    if risk.distance_m.is_nan() {
        return InterlockDecision::Block {
            reason: "twin report has no usable distance (NaN)".to_string(),
        };
    }
    if risk.distance_m < MIN_CLEARANCE_M {
        return InterlockDecision::Block {
            reason: format!(
                "twin reports only {:.3}m of clearance (minimum {:.3}m)",
                risk.distance_m, MIN_CLEARANCE_M
            ),
        };
    }
    InterlockDecision::Allow
}

/// The decision for a report that may be missing. Fail closed: a real command is NOT forwarded without a twin report, because "nobody told the bridge about a risk" is not the same
/// as "there is no risk" (the report used to default to "no collision", so omitting it was enough to reach the hardware).
pub fn assess_optional_interlock(risk: Option<&TwinRiskReport>) -> InterlockDecision {
    match risk {
        Some(report) => assess_interlock(report),
        None => InterlockDecision::Block {
            reason: "no twin risk report: a real command is not forwarded without one".to_string(),
        },
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
        let risk = TwinRiskReport {
            collision_imminent: false,
            distance_m: 2.0,
        };
        assert_eq!(assess_interlock(&risk), InterlockDecision::Allow);
        // an unbounded distance (nothing in range) is clear too
        let nothing_near = TwinRiskReport {
            collision_imminent: false,
            distance_m: f64::INFINITY,
        };
        assert_eq!(assess_interlock(&nothing_near), InterlockDecision::Allow);
    }

    #[test]
    fn a_tiny_clearance_blocks_even_without_the_imminent_flag() {
        for distance_m in [0.0, 0.001, 0.049, -0.5] {
            let risk = TwinRiskReport {
                collision_imminent: false,
                distance_m,
            };
            assert!(
                matches!(assess_interlock(&risk), InterlockDecision::Block { .. }),
                "{distance_m} m must block"
            );
        }
    }

    #[test]
    fn exactly_the_minimum_clearance_is_allowed() {
        let risk = TwinRiskReport {
            collision_imminent: false,
            distance_m: MIN_CLEARANCE_M,
        };
        assert_eq!(assess_interlock(&risk), InterlockDecision::Allow);
    }

    #[test]
    fn a_nan_distance_blocks() {
        let risk = TwinRiskReport {
            collision_imminent: false,
            distance_m: f64::NAN,
        };
        match assess_interlock(&risk) {
            InterlockDecision::Block { reason } => assert!(reason.contains("NaN")),
            InterlockDecision::Allow => panic!("a NaN distance must not allow"),
        }
    }

    #[test]
    fn a_missing_report_blocks() {
        match assess_optional_interlock(None) {
            InterlockDecision::Block { reason } => assert!(reason.contains("no twin risk report")),
            InterlockDecision::Allow => panic!("no report must not allow"),
        }
        let clear = TwinRiskReport {
            collision_imminent: false,
            distance_m: 1.0,
        };
        assert_eq!(
            assess_optional_interlock(Some(&clear)),
            InterlockDecision::Allow
        );
    }
}
