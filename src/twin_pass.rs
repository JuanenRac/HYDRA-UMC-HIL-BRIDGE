// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/twin_pass.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! The pass verdict of the twin as a condition of moving the real robot.
//!
//! HYDRA-UMC-TWIN runs a planned route in simulation (`POST /dryrun/trajectory`) and answers a **verdict**, signed with its Ed25519 key,
//! that is bound to the exact samples of the trajectory, to the robot and to a time window. With the switch `require_twin_pass` ON, a real
//! command is not forwarded unless its request carries that verdict and the samples it was made for (`twin_pass`), and:
//!
//! - the verdict is `pass` (`fail` and `incomplete` allow nothing);
//! - its signature is valid for a key of the twin this bridge trusts (`--twin-public-key-file`);
//! - the SHA-256 of the canonical text of the samples, computed HERE from the samples that came with the request, is the one the verdict signed
//!   (so the samples cannot be swapped for another trajectory);
//! - it is for the robot this bridge drives (`--twin-robot-id`);
//! - it is inside its window (a little future skew allowed);
//! - the command is one of the points of that trajectory (same joint, same position), and each point is good for ONE command: a verdict
//!   covers a whole trajectory, which is sent as many commands, so the verdict is not used up by the first, but a point is not sent twice.
//!
//! The text the twin signs, and the canonical text of the samples, are the ones of `src/dryrun.rs` of HYDRA-UMC-TWIN. This is in addition to
//! the certificate of the risk report that every real command already needs (`certificate.rs`): both must hold.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use ed25519_dalek::{Signature, Verifier as _};
use serde::Deserialize;

use crate::certificate::{decode_hex, TrustedKeys};
use crate::protocol::JointCommand;

pub const DOMAIN: &str = "hydra-umc-dryrun-verdict-v1";
/// The most samples a request may carry for the check.
pub const MAX_SAMPLES: usize = 20_000;
/// How far ahead of this machine's clock a verdict may be dated.
pub const MAX_FUTURE_SKEW_MS: u64 = 1_000;
/// Verdicts remembered at most (the oldest is forgotten first, an expired one before any other).
pub const LEDGER_CAPACITY: usize = 1_024;

#[derive(Debug, Clone, Deserialize)]
pub struct Sample {
    pub t: f64,
    pub joints: BTreeMap<String, f64>,
}

/// The fields of the answer of the twin that are signed. The answer carries more (a message, the checks): they are not read.
#[derive(Debug, Clone, Deserialize)]
pub struct Verdict {
    pub verdict: String,
    pub robot_id: String,
    pub trajectory_sha256: String,
    pub issued_at_ms: u64,
    pub valid_until_ms: u64,
    pub nonce: String,
    pub key_id: String,
    pub signature: String,
}

/// What a real command carries when the switch is on: the answer of the twin as it came, and the samples it was made for.
#[derive(Debug, Clone, Deserialize)]
pub struct Proof {
    pub verdict: Verdict,
    pub samples: Vec<Sample>,
}

/// The text that is hashed (the one of `canonical_trajectory` in HYDRA-UMC-TWIN): a version line, then one line per sample with the time and
/// the joints in name order, every number in the shortest form that reads back to the same value.
pub fn canonical_trajectory(samples: &[Sample]) -> String {
    let mut text = String::from("hydra-umc-trajectory-v1\n");
    for s in samples {
        text.push_str(&format!("t={:?}", s.t));
        for (name, position) in &s.joints {
            text.push_str(&format!(" {name}={position:?}"));
        }
        text.push('\n');
    }
    text
}

pub fn trajectory_sha256(samples: &[Sample]) -> String {
    crate::hmac::to_hex(&crate::hmac::sha256(
        canonical_trajectory(samples).as_bytes(),
    ))
}

/// The text a verdict signs.
pub fn signed_text(v: &Verdict, sha256: &str) -> String {
    format!(
        "{DOMAIN}\n{}\n{}\n{sha256}\n{}\n{}\n{}\n{}",
        v.key_id, v.robot_id, v.verdict, v.issued_at_ms, v.valid_until_ms, v.nonce
    )
}

#[derive(Debug)]
struct Entry {
    valid_until_ms: u64,
    used: HashSet<usize>,
}

/// The points of the trajectories already sent, per verdict.
#[derive(Debug, Default)]
pub struct VerdictLedger {
    order: VecDeque<String>,
    entries: HashMap<String, Entry>,
}

impl VerdictLedger {
    /// The first of `candidates` (indexes of the samples that match a command) not sent yet under this verdict.
    pub fn first_unused(&self, nonce: &str, candidates: &[usize]) -> Option<usize> {
        match self.entries.get(nonce) {
            None => candidates.first().copied(),
            Some(entry) => candidates.iter().copied().find(|i| !entry.used.contains(i)),
        }
    }

    /// Remember that this point was sent.
    pub fn mark(&mut self, nonce: &str, valid_until_ms: u64, index: usize, now_ms: u64) {
        if !self.entries.contains_key(nonce) {
            self.forget_expired(now_ms);
            while self.order.len() >= LEDGER_CAPACITY {
                if let Some(oldest) = self.order.pop_front() {
                    self.entries.remove(&oldest);
                }
            }
            self.order.push_back(nonce.to_string());
            self.entries.insert(
                nonce.to_string(),
                Entry {
                    valid_until_ms,
                    used: HashSet::new(),
                },
            );
        }
        if let Some(entry) = self.entries.get_mut(nonce) {
            entry.used.insert(index);
        }
    }

    fn forget_expired(&mut self, now_ms: u64) {
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.valid_until_ms < now_ms)
            .map(|(k, _)| k.clone())
            .collect();
        for nonce in expired {
            self.entries.remove(&nonce);
            self.order.retain(|n| n != &nonce);
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// The point of the trajectory a command was matched to; it is remembered (`VerdictLedger::mark`) once the command was really sent.
#[derive(Debug, PartialEq)]
pub struct Matched {
    pub nonce: String,
    pub valid_until_ms: u64,
    pub index: usize,
}

/// Check the proof for a command. Does not change the ledger: the point is marked by the caller when the command went. The reasons name no key.
pub fn check(
    proof: Option<&Proof>,
    command: &JointCommand,
    keys: &TrustedKeys,
    robot_id: &str,
    now_ms: u64,
    ledger: &VerdictLedger,
) -> Result<Matched, String> {
    let Some(proof) = proof else {
        return Err("this bridge requires a pass verdict of the twin for the trajectory of a real command (field \"twin_pass\")".into());
    };
    if keys.is_empty() {
        return Err("no twin public key is configured on this bridge (--twin-public-key-file): a verdict cannot be checked".into());
    }
    if robot_id.is_empty() {
        return Err("no robot is named on this bridge (--twin-robot-id): a verdict cannot be tied to the robot".into());
    }
    if proof.samples.is_empty() || proof.samples.len() > MAX_SAMPLES {
        return Err(format!(
            "the trajectory must have between 1 and {MAX_SAMPLES} samples"
        ));
    }
    let v = &proof.verdict;
    let Some(key) = keys.find(&v.key_id) else {
        return Err("the verdict names a key this bridge does not trust".into());
    };
    let sha = trajectory_sha256(&proof.samples);
    if sha != v.trajectory_sha256 {
        return Err("the samples are not the trajectory the verdict was made for".into());
    }
    let signature_bytes: [u8; 64] = decode_hex(&v.signature)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or("the verdict signature is not 64 bytes of hexadecimal")?;
    if key
        .verify(
            signed_text(v, &sha).as_bytes(),
            &Signature::from_bytes(&signature_bytes),
        )
        .is_err()
    {
        return Err("the verdict signature is not valid".into());
    }
    match v.verdict.as_str() {
        "pass" => {}
        "fail" => return Err("the twin did not pass this trajectory (verdict: fail)".into()),
        "incomplete" => {
            return Err(
                "the twin could not check this trajectory completely (verdict: incomplete)".into(),
            )
        }
        _ => return Err("the verdict is not pass".into()),
    }
    if v.robot_id != robot_id {
        return Err("the verdict is for another robot".into());
    }
    if v.nonce.len() != 32 || !v.nonce.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("the verdict nonce is not 32 hexadecimal characters".into());
    }
    if v.issued_at_ms > now_ms.saturating_add(MAX_FUTURE_SKEW_MS) {
        return Err("the verdict is dated in the future".into());
    }
    if v.valid_until_ms <= v.issued_at_ms || now_ms > v.valid_until_ms {
        return Err("the verdict is out of its validity window: ask the twin again".into());
    }
    let candidates: Vec<usize> = proof
        .samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.joints.get(&command.joint) == Some(&command.position))
        .map(|(i, _)| i)
        .collect();
    if candidates.is_empty() {
        return Err("this command is not a point of the trajectory the twin passed".into());
    }
    match ledger.first_unused(&v.nonce, &candidates) {
        Some(index) => Ok(Matched {
            nonce: v.nonce.clone(),
            valid_until_ms: v.valid_until_ms,
            index,
        }),
        None => {
            Err("every point of the trajectory that matches this command was already sent".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey};

    const NOW: u64 = 1_800_000_000_000;
    const NONCE: &str = "00112233445566778899aabbccddeeff";

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn key_id() -> String {
        crate::hmac::to_hex(&signing_key().verifying_key().to_bytes()[..8])
    }

    fn keys() -> TrustedKeys {
        TrustedKeys::from_text(&crate::hmac::to_hex(
            &signing_key().verifying_key().to_bytes(),
        ))
        .unwrap()
    }

    fn sample(t: f64, shoulder: f64, elbow: f64) -> Sample {
        let mut joints = BTreeMap::new();
        joints.insert("shoulder".to_string(), shoulder);
        joints.insert("elbow".to_string(), elbow);
        Sample { t, joints }
    }

    fn samples() -> Vec<Sample> {
        vec![
            sample(0.0, 0.0, 0.5),
            sample(0.1, 0.25, 0.5),
            sample(0.2, 0.5, 0.75),
        ]
    }

    /// A verdict as the twin would issue it for these samples.
    fn proof_with(
        verdict: &str,
        robot: &str,
        issued: u64,
        until: u64,
        samples: Vec<Sample>,
    ) -> Proof {
        let sha = trajectory_sha256(&samples);
        let mut v = Verdict {
            verdict: verdict.to_string(),
            robot_id: robot.to_string(),
            trajectory_sha256: sha.clone(),
            issued_at_ms: issued,
            valid_until_ms: until,
            nonce: NONCE.to_string(),
            key_id: key_id(),
            signature: String::new(),
        };
        let signature = signing_key().sign(signed_text(&v, &sha).as_bytes());
        v.signature = crate::hmac::to_hex(&signature.to_bytes());
        Proof {
            verdict: v,
            samples,
        }
    }

    fn proof() -> Proof {
        proof_with("pass", "arm-1", NOW - 100, NOW + 5_000, samples())
    }

    fn command(joint: &str, position: f64) -> JointCommand {
        JointCommand {
            joint: joint.to_string(),
            position,
        }
    }

    fn run(p: &Proof, c: &JointCommand, ledger: &VerdictLedger) -> Result<Matched, String> {
        check(Some(p), c, &keys(), "arm-1", NOW, ledger)
    }

    #[test]
    fn the_canonical_text_is_the_one_of_the_twin() {
        assert_eq!(
            canonical_trajectory(&samples()[..2]),
            "hydra-umc-trajectory-v1\nt=0.0 elbow=0.5 shoulder=0.0\nt=0.1 elbow=0.5 shoulder=0.25\n"
        );
    }

    #[test]
    fn a_pass_for_a_point_of_the_trajectory_goes() {
        let matched = run(
            &proof(),
            &command("shoulder", 0.25),
            &VerdictLedger::default(),
        )
        .unwrap();
        assert_eq!(matched.index, 1);
        assert_eq!(matched.nonce, NONCE);
    }

    #[test]
    fn without_a_verdict_nothing_goes() {
        let reason = check(
            None,
            &command("shoulder", 0.25),
            &keys(),
            "arm-1",
            NOW,
            &VerdictLedger::default(),
        )
        .unwrap_err();
        assert!(reason.contains("twin_pass"));
    }

    #[test]
    fn fail_and_incomplete_allow_nothing() {
        for name in ["fail", "incomplete", "maybe"] {
            let p = proof_with(name, "arm-1", NOW - 100, NOW + 5_000, samples());
            assert!(
                run(&p, &command("shoulder", 0.25), &VerdictLedger::default()).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn other_samples_than_the_ones_signed_are_refused() {
        let mut p = proof();
        p.samples[1].joints.insert("shoulder".into(), 0.3);
        let reason = run(&p, &command("shoulder", 0.3), &VerdictLedger::default()).unwrap_err();
        assert!(reason.contains("not the trajectory"));
    }

    #[test]
    fn a_changed_field_is_a_bad_signature() {
        let mut p = proof();
        p.verdict.valid_until_ms += 1;
        assert!(
            run(&p, &command("shoulder", 0.25), &VerdictLedger::default())
                .unwrap_err()
                .contains("signature")
        );
        let mut p = proof();
        p.verdict.verdict = "pass".into();
        p.verdict.signature = "00".repeat(64);
        assert!(
            run(&p, &command("shoulder", 0.25), &VerdictLedger::default())
                .unwrap_err()
                .contains("signature")
        );
    }

    #[test]
    fn a_key_that_is_not_trusted_is_refused() {
        let mut p = proof();
        p.verdict.key_id = "0000000000000000".into();
        assert!(
            run(&p, &command("shoulder", 0.25), &VerdictLedger::default())
                .unwrap_err()
                .contains("does not trust")
        );
        let empty = TrustedKeys::default();
        assert!(check(
            Some(&proof()),
            &command("shoulder", 0.25),
            &empty,
            "arm-1",
            NOW,
            &VerdictLedger::default()
        )
        .is_err());
    }

    #[test]
    fn another_robot_is_refused_and_so_is_a_bridge_that_names_none() {
        let p = proof_with("pass", "arm-2", NOW - 100, NOW + 5_000, samples());
        assert!(
            run(&p, &command("shoulder", 0.25), &VerdictLedger::default())
                .unwrap_err()
                .contains("another robot")
        );
        assert!(check(
            Some(&proof()),
            &command("shoulder", 0.25),
            &keys(),
            "",
            NOW,
            &VerdictLedger::default()
        )
        .is_err());
    }

    #[test]
    fn the_window_is_kept() {
        let late = proof_with("pass", "arm-1", NOW - 10_000, NOW - 1, samples());
        assert!(
            run(&late, &command("shoulder", 0.25), &VerdictLedger::default())
                .unwrap_err()
                .contains("window")
        );
        let future = proof_with("pass", "arm-1", NOW + 60_000, NOW + 90_000, samples());
        assert!(run(
            &future,
            &command("shoulder", 0.25),
            &VerdictLedger::default()
        )
        .unwrap_err()
        .contains("future"));
        let backwards = proof_with("pass", "arm-1", NOW, NOW - 5, samples());
        assert!(run(
            &backwards,
            &command("shoulder", 0.25),
            &VerdictLedger::default()
        )
        .is_err());
    }

    #[test]
    fn a_command_that_is_not_a_point_of_the_trajectory_is_refused() {
        let reason = run(
            &proof(),
            &command("shoulder", 0.26),
            &VerdictLedger::default(),
        )
        .unwrap_err();
        assert!(reason.contains("not a point"));
        assert!(run(&proof(), &command("wrist", 0.25), &VerdictLedger::default()).is_err());
    }

    #[test]
    fn each_point_is_good_for_one_command_but_the_verdict_is_not_used_up() {
        let mut ledger = VerdictLedger::default();
        let first = run(&proof(), &command("elbow", 0.5), &ledger).unwrap();
        assert_eq!(first.index, 0);
        ledger.mark(&first.nonce, first.valid_until_ms, first.index, NOW);
        // the same position appears in two samples: the second one is used next
        let second = run(&proof(), &command("elbow", 0.5), &ledger).unwrap();
        assert_eq!(second.index, 1);
        ledger.mark(&second.nonce, second.valid_until_ms, second.index, NOW);
        assert!(run(&proof(), &command("elbow", 0.5), &ledger)
            .unwrap_err()
            .contains("already sent"));
        // another point of the same verdict still goes
        assert!(run(&proof(), &command("shoulder", 0.5), &ledger).is_ok());
    }

    #[test]
    fn a_check_alone_does_not_use_a_point() {
        let ledger = VerdictLedger::default();
        run(&proof(), &command("shoulder", 0.25), &ledger).unwrap();
        run(&proof(), &command("shoulder", 0.25), &ledger).unwrap();
        assert_eq!(ledger.len(), 0);
    }

    #[test]
    fn the_ledger_is_bounded_and_forgets_the_expired_first() {
        let mut ledger = VerdictLedger::default();
        ledger.mark("old", NOW - 1, 0, NOW - 10);
        for i in 0..LEDGER_CAPACITY {
            ledger.mark(&format!("n{i}"), NOW + 10_000, 0, NOW);
        }
        assert!(ledger.len() <= LEDGER_CAPACITY);
        assert!(
            ledger.first_unused("old", &[0]).is_some(),
            "an expired verdict is forgotten, so its point reads as unused"
        );
    }

    #[test]
    fn a_trajectory_without_samples_or_with_too_many_is_refused() {
        let mut p = proof();
        p.samples.clear();
        assert!(run(&p, &command("shoulder", 0.25), &VerdictLedger::default()).is_err());
        let big = vec![sample(0.0, 0.0, 0.0); MAX_SAMPLES + 1];
        let p = proof_with("pass", "arm-1", NOW - 100, NOW + 5_000, big);
        assert!(run(&p, &command("shoulder", 0.0), &VerdictLedger::default()).is_err());
    }

    #[test]
    fn the_answer_of_the_twin_is_read_as_it_comes() {
        let p = proof();
        let text = serde_json::json!({
            "verdict": {
                "verdict": p.verdict.verdict, "message": "ok", "robot_id": p.verdict.robot_id, "samples": 3,
                "trajectory_sha256": p.verdict.trajectory_sha256, "issued_at_ms": p.verdict.issued_at_ms,
                "valid_until_ms": p.verdict.valid_until_ms, "nonce": p.verdict.nonce, "key_id": p.verdict.key_id,
                "signature": p.verdict.signature, "checks": {"trajectory": {"ok": true, "complete": true}, "collision": {"checked": false}}
            },
            "samples": [
                {"t": 0.0, "joints": {"shoulder": 0.0, "elbow": 0.5}},
                {"t": 0.1, "joints": {"shoulder": 0.25, "elbow": 0.5}},
                {"t": 0.2, "joints": {"shoulder": 0.5, "elbow": 0.75}}
            ]
        })
        .to_string();
        let parsed: Proof = serde_json::from_str(&text).unwrap();
        assert!(run(
            &parsed,
            &command("shoulder", 0.25),
            &VerdictLedger::default()
        )
        .is_ok());
    }
}
