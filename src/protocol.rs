// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/protocol.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
// =============================================================================
//! The real message shapes this bridge routes, independent of whatever
//! real transport (gRPC, WebSocket) eventually carries them - see
//! `Cargo.toml`'s own comment for why neither is wired in yet.

#[derive(Debug, Clone, PartialEq)]
pub struct JointCommand {
    pub joint: String,
    pub position: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Route to the real HYDRA-UMC controller (subject to the safety
    /// interlock - see `interlock.rs`).
    Real,
    /// Route to HYDRA-UMC-TWIN only. Never gated by the interlock: the
    /// whole point of simulation mode is to be able to see a predicted
    /// collision play out, not have it silently swallowed.
    Simulation,
}
