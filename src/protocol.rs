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
