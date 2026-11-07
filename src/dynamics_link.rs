// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/dynamics_link.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! The rigid-body engine of HYDRA-UMC-PHYSICS-REPLICA, reached from the bridge: it asks the `simulate` command of that program (a separate process
//! whose path the caller gives) how the robot moves under given torques, and sets the answer against the joint positions recorded from a real arm.
//!
//! Nothing here starts on its own and nothing leaves the machine: the simulator is a local program run with fixed arguments (no shell), and the
//! recording is a file. The numbers are those of a model (the `<inertial>` blocks and gravity, no friction and no motor dynamics): the result is
//! evidence = simulator until it has been compared with a real arm, and this module is the comparison.

use std::process::Command;

/// What `simulate --json` of the physics replica prints, reduced to what the comparison needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Simulated {
    pub joints: Vec<String>,
    /// (time in seconds, position of each joint), by increasing time.
    pub samples: Vec<(f64, Vec<f64>)>,
}

/// How far the simulated motion is from the recorded one, for one joint.
#[derive(Debug, Clone, PartialEq)]
pub struct JointDeviation {
    pub joint: String,
    pub max_abs: f64,
    pub rms: f64,
    /// How many recorded samples fell inside the simulated time span and were compared.
    pub compared: usize,
}

fn joint_names(value: &serde_json::Value, what: &str) -> Result<Vec<String>, String> {
    value["joints"]
        .as_array()
        .ok_or_else(|| format!("{what} has no joints"))?
        .iter()
        .map(|j| {
            j.as_str()
                .map(str::to_string)
                .ok_or_else(|| "a joint name is not text".to_string())
        })
        .collect()
}

fn sample_row(row: &serde_json::Value, count: usize) -> Result<(f64, Vec<f64>), String> {
    let t = row["t_s"].as_f64().ok_or("a sample has no time")?;
    let positions: Vec<f64> = row["positions"]
        .as_array()
        .ok_or("a sample has no positions")?
        .iter()
        .map(|p| {
            p.as_f64()
                .ok_or_else(|| "a position is not a number".to_string())
        })
        .collect::<Result<_, _>>()?;
    if positions.len() != count || !t.is_finite() || positions.iter().any(|p| !p.is_finite()) {
        return Err("a sample does not match the joints or is not finite".to_string());
    }
    Ok((t, positions))
}

pub fn parse_simulated(text: &str) -> Result<Simulated, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("the simulator did not print JSON: {e}"))?;
    let joints = joint_names(&v, "the simulator answer")?;
    let rows = v["samples"]
        .as_array()
        .ok_or("the simulator answer has no samples")?;
    let mut samples = Vec::new();
    for row in rows {
        samples.push(sample_row(row, joints.len())?);
    }
    if samples.len() < 2 || samples.windows(2).any(|w| w[1].0 <= w[0].0) {
        return Err("the samples must be at least two, by increasing time".to_string());
    }
    Ok(Simulated { joints, samples })
}

/// The position of joint `index` at time `t`, by straight lines between samples; None outside the simulated time span.
fn position_at(sim: &Simulated, index: usize, t: f64) -> Option<f64> {
    let first = sim.samples.first()?;
    let last = sim.samples.last()?;
    if t < first.0 || t > last.0 {
        return None;
    }
    let after = sim.samples.iter().position(|s| s.0 >= t)?;
    if after == 0 {
        return Some(first.1[index]);
    }
    let (t0, p0) = (sim.samples[after - 1].0, sim.samples[after - 1].1[index]);
    let (t1, p1) = (sim.samples[after].0, sim.samples[after].1[index]);
    Some(p0 + (p1 - p0) * (t - t0) / (t1 - t0))
}

/// `recorded` is (time, position of each joint in the order of `sim.joints`). A recorded sample outside the simulated span is not compared.
pub fn deviations(
    sim: &Simulated,
    recorded: &[(f64, Vec<f64>)],
) -> Result<Vec<JointDeviation>, String> {
    if recorded.iter().any(|(_, p)| p.len() != sim.joints.len()) {
        return Err("a recorded sample does not have one position per joint".to_string());
    }
    let mut out = Vec::new();
    for (index, joint) in sim.joints.iter().enumerate() {
        let (mut max_abs, mut sum_sq, mut compared) = (0.0_f64, 0.0_f64, 0usize);
        for (t, positions) in recorded {
            if let Some(expected) = position_at(sim, index, *t) {
                let d = (positions[index] - expected).abs();
                max_abs = max_abs.max(d);
                sum_sq += d * d;
                compared += 1;
            }
        }
        let rms = if compared == 0 {
            0.0
        } else {
            (sum_sq / compared as f64).sqrt()
        };
        out.push(JointDeviation {
            joint: joint.clone(),
            max_abs,
            rms,
            compared,
        });
    }
    Ok(out)
}

/// Reads a recording: `{"joints": [...], "samples": [{"t_s": 0.0, "positions": [...]}, ...]}`, with the joints in any order; they are put in the order of `joints`.
pub fn parse_recording(text: &str, joints: &[String]) -> Result<Vec<(f64, Vec<f64>)>, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("the recording is not JSON: {e}"))?;
    let names = joint_names(&v, "the recording")?;
    let order: Vec<usize> = joints
        .iter()
        .map(|j| {
            names
                .iter()
                .position(|n| n == j)
                .ok_or_else(|| format!("the recording has no joint {j}"))
        })
        .collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for row in v["samples"]
        .as_array()
        .ok_or("the recording has no samples")?
    {
        let (t, all) = sample_row(row, names.len())?;
        out.push((t, order.iter().map(|&i| all[i]).collect()));
    }
    Ok(out)
}

/// Runs `simulate --json` of the physics replica. `extra` are the flags to pass on (`--joints`, `--velocities`, `--torques`, `--duration`, `--dt`).
pub fn run_simulator(binary: &str, urdf: &str, extra: &[String]) -> Result<Simulated, String> {
    let output = Command::new(binary)
        .arg("simulate")
        .arg("--urdf")
        .arg(urdf)
        .args(extra)
        .arg("--json")
        .output()
        .map_err(|e| format!("the simulator could not be started: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "the simulator refused: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_simulated(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIM: &str = r#"{"joints":["a","b"],"dt_s":0.1,"steps":2,"samples":[
        {"t_s":0.0,"positions":[0.0,1.0],"velocities":[0.0,0.0]},
        {"t_s":1.0,"positions":[1.0,1.0],"velocities":[1.0,0.0]},
        {"t_s":2.0,"positions":[2.0,1.0],"velocities":[1.0,0.0]}]}"#;

    #[test]
    fn a_simulator_answer_is_read_and_a_bad_one_is_refused() {
        let sim = parse_simulated(SIM).unwrap();
        assert_eq!(sim.joints, vec!["a", "b"]);
        assert_eq!(sim.samples.len(), 3);
        assert!(parse_simulated("not json").is_err());
        assert!(
            parse_simulated(r#"{"joints":["a"],"samples":[{"t_s":0,"positions":[0]}]}"#).is_err()
        );
        assert!(parse_simulated(
            r#"{"joints":["a"],"samples":[{"t_s":1,"positions":[0]},{"t_s":1,"positions":[0]}]}"#
        )
        .is_err());
        assert!(parse_simulated(r#"{"joints":["a"],"samples":[{"t_s":0,"positions":[0,1]},{"t_s":1,"positions":[0,1]}]}"#).is_err());
    }

    #[test]
    fn a_recording_that_follows_the_simulation_has_no_deviation() {
        let sim = parse_simulated(SIM).unwrap();
        let recorded = vec![
            (0.5, vec![0.5, 1.0]),
            (1.5, vec![1.5, 1.0]),
            (2.0, vec![2.0, 1.0]),
        ];
        let d = deviations(&sim, &recorded).unwrap();
        assert!(d.iter().all(|j| j.max_abs < 1e-12 && j.compared == 3));
    }

    #[test]
    fn a_late_arm_shows_its_deviation_and_samples_outside_the_span_are_not_compared() {
        let sim = parse_simulated(SIM).unwrap();
        let recorded = vec![
            (1.0, vec![0.8, 1.0]),
            (1.5, vec![1.3, 1.0]),
            (5.0, vec![9.0, 9.0]),
        ];
        let d = deviations(&sim, &recorded).unwrap();
        assert_eq!(d[0].compared, 2);
        assert!((d[0].max_abs - 0.2).abs() < 1e-12);
        assert!((d[0].rms - 0.2).abs() < 1e-12);
        assert_eq!(d[1].max_abs, 0.0);
        assert!(deviations(&sim, &[(1.0, vec![1.0])]).is_err());
    }

    #[test]
    fn a_recording_is_put_in_the_order_of_the_simulated_joints() {
        let text = r#"{"joints":["b","a","c"],"samples":[{"t_s":0.0,"positions":[10,20,30]}]}"#;
        let r = parse_recording(text, &["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(r, vec![(0.0, vec![20.0, 10.0])]);
        assert!(parse_recording(text, &["z".to_string()]).is_err());
        assert!(parse_recording(
            r#"{"joints":["a"],"samples":[{"t_s":0,"positions":[1,2]}]}"#,
            &["a".to_string()]
        )
        .is_err());
    }

    #[test]
    fn a_simulator_that_cannot_be_started_is_an_error_not_a_panic() {
        assert!(run_simulator("/no/such/program", "robot.urdf", &[]).is_err());
    }
}
