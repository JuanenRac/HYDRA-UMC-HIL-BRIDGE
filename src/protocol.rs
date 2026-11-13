// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/protocol.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! The real message shapes this bridge routes, independent of whatever
//! real transport (gRPC, WebSocket) eventually carries them - see
//! `Cargo.toml`'s own comment for why neither is wired in yet.

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct JointCommand {
    pub joint: String,
    pub position: f64,
}

/// The longest joint name accepted; a joint name is a short identifier.
pub const MAX_JOINT_NAME_LEN: usize = 64;

impl JointCommand {
    /// Refuses a command that cannot be a real joint target: an empty or
    /// oversized joint name, or a position that is not a finite number
    /// (`NaN` and the infinities parse as valid `f64` text).
    ///
    /// Only the shape of the value is checked here; the joints and their
    /// ranges come from the URDF (`limits.rs`, `serve --urdf`).
    pub fn validate(&self) -> Result<(), String> {
        if self.joint.trim().is_empty() {
            return Err("joint name must not be empty".to_string());
        }
        if self.joint.len() > MAX_JOINT_NAME_LEN {
            return Err(format!(
                "joint name is longer than {MAX_JOINT_NAME_LEN} bytes"
            ));
        }
        if !self.position.is_finite() {
            return Err("position must be a finite number".to_string());
        }
        Ok(())
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
    fn a_normal_command_is_valid() {
        assert!(cmd("shoulder", 1.25).validate().is_ok());
    }

    #[test]
    fn an_empty_or_blank_joint_is_refused() {
        assert!(cmd("", 1.0).validate().is_err());
        assert!(cmd("   ", 1.0).validate().is_err());
    }

    #[test]
    fn an_oversized_joint_name_is_refused() {
        assert!(cmd(&"j".repeat(MAX_JOINT_NAME_LEN + 1), 1.0)
            .validate()
            .is_err());
    }

    #[test]
    fn a_non_finite_position_is_refused() {
        assert!(cmd("shoulder", f64::NAN).validate().is_err());
        assert!(cmd("shoulder", f64::INFINITY).validate().is_err());
        assert!(cmd("shoulder", f64::NEG_INFINITY).validate().is_err());
    }

    #[test]
    fn the_limits_of_a_joint_name_are_exact() {
        assert!(cmd(&"j".repeat(MAX_JOINT_NAME_LEN), 0.0).validate().is_ok());
        assert!(cmd("a", 0.0).validate().is_ok());
        // the length is counted in bytes: 33 two-byte characters are 66 bytes
        assert!(cmd(&"é".repeat(33), 0.0).validate().is_err());
        assert!(cmd(&"é".repeat(32), 0.0).validate().is_ok());
        // a name that is only whitespace of any kind is empty
        assert!(cmd("\t\n ", 0.0).validate().is_err());
    }

    #[test]
    fn extreme_but_finite_positions_are_shape_valid() {
        for position in [0.0, -0.0, f64::MAX, f64::MIN, f64::MIN_POSITIVE, -1.0e-300] {
            assert!(cmd("j", position).validate().is_ok(), "{position}");
        }
    }

    #[test]
    fn the_refusals_say_which_rule_was_broken() {
        let empty = cmd("", 1.0).validate().unwrap_err();
        let long = cmd(&"j".repeat(MAX_JOINT_NAME_LEN + 1), 1.0)
            .validate()
            .unwrap_err();
        let nan = cmd("j", f64::NAN).validate().unwrap_err();
        assert!(empty.contains("empty"), "{empty}");
        assert!(long.contains(&MAX_JOINT_NAME_LEN.to_string()), "{long}");
        assert!(nan.contains("finite"), "{nan}");
    }

    #[test]
    fn a_command_round_trips_through_json_and_unknown_shapes_are_refused() {
        let original = cmd("shoulder", -1.5);
        let text = serde_json::to_string(&original).unwrap();
        assert_eq!(text, r#"{"joint":"shoulder","position":-1.5}"#);
        assert_eq!(
            serde_json::from_str::<JointCommand>(&text).unwrap(),
            original
        );
        for bad in [
            r#"{"joint":"j"}"#,
            r#"{"position":1.0}"#,
            r#"{"joint":5,"position":1.0}"#,
            r#"{"joint":"j","position":"1.0"}"#,
            r#"[]"#,
            r#""#,
        ] {
            assert!(serde_json::from_str::<JointCommand>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_mode_is_spelled_in_lowercase_and_nothing_else_is_a_mode() {
        assert_eq!(serde_json::to_string(&Mode::Real).unwrap(), r#""real""#);
        assert_eq!(
            serde_json::to_string(&Mode::Simulation).unwrap(),
            r#""simulation""#
        );
        assert_eq!(
            serde_json::from_str::<Mode>(r#""real""#).unwrap(),
            Mode::Real
        );
        for bad in [r#""Real""#, r#""REAL""#, r#""sim""#, r#""""#, "1", "null"] {
            assert!(serde_json::from_str::<Mode>(bad).is_err(), "{bad}");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Route to the real HYDRA-UMC controller (subject to the safety
    /// interlock - see `interlock.rs`).
    Real,
    /// Route to HYDRA-UMC-TWIN only. Never gated by the interlock: the
    /// whole point of simulation mode is to be able to see a predicted
    /// collision play out, not have it silently swallowed.
    Simulation,
}
