// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/certificate.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! Interlock certificates: the proof that the twin made the risk report of a command.
//!
//! The interlock (`interlock.rs`) used to decide on a `TwinRiskReport` that the caller itself wrote in the request: whoever asked
//! to move a real robot also said "the twin sees no collision". A real-mode command now needs a certificate: the report, bound to the
//! joint and the exact position of that command, signed with the Ed25519 key of HYDRA-UMC-TWIN. This bridge holds only the PUBLIC half
//! (`--twin-public-key-file`), so nobody who can reach it can mint one, and a certificate is also checked for:
//!
//! - being for THIS command (same joint, same position), so it cannot be kept and used for another move;
//! - being fresh (`max_age_ms`, a little future skew allowed), so a report from before the arm moved is not a report now;
//! - being new (its nonce is remembered), so it cannot be sent twice.
//!
//! The signed text is the one HYDRA-UMC-TWIN builds (see its `certificate.rs`):
//!
//! ```text
//! hydra-umc-interlock-certificate-v1 \n key_id \n joint \n position \n collision_imminent \n distance_m \n issued_at_ms \n nonce
//! ```
//!
//! with the numbers in Rust's shortest round-trip form (`{:?}`). A fixed vector, produced by an independent RFC 8032 implementation, is
//! verified in the tests of both repositories, so the two cannot drift apart without a test failing.

use std::collections::{HashSet, VecDeque};

use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};

use crate::interlock::TwinRiskReport;
use crate::protocol::JointCommand;

pub const DOMAIN: &str = "hydra-umc-interlock-certificate-v1";
/// How old a certificate may be when it arrives, by default.
pub const DEFAULT_MAX_AGE_MS: u64 = 2_000;
/// How far ahead of this machine's clock a certificate may be dated (a little skew between two machines is normal).
pub const MAX_FUTURE_SKEW_MS: u64 = 1_000;
/// Nonces remembered at most; a certificate older than the maximum age is refused anyway, so the oldest can be forgotten.
pub const NONCE_CACHE_CAPACITY: usize = 8_192;

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct Certificate {
    pub joint: String,
    pub position: f64,
    pub collision_imminent: bool,
    pub distance_m: f64,
    pub issued_at_ms: u64,
    pub nonce: String,
    pub key_id: String,
    pub signature: String,
}

impl Certificate {
    pub fn signed_text(&self) -> String {
        format!(
            "{DOMAIN}\n{}\n{}\n{:?}\n{}\n{:?}\n{}\n{}",
            self.key_id,
            self.joint,
            self.position,
            self.collision_imminent,
            self.distance_m,
            self.issued_at_ms,
            self.nonce
        )
    }
}

pub fn decode_hex(text: &str) -> Result<Vec<u8>, ()> {
    if text.len() % 2 == 1 || !text.is_ascii() {
        return Err(());
    }
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).map_err(|_| ()))
        .collect()
}

/// The public keys of the twins this bridge trusts, each with the identifier a certificate names it by (the first 8 bytes, in hex).
#[derive(Debug, Clone, Default)]
pub struct TrustedKeys {
    keys: Vec<(String, VerifyingKey)>,
}

impl TrustedKeys {
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn key_ids(&self) -> Vec<&str> {
        self.keys.iter().map(|(id, _)| id.as_str()).collect()
    }

    /// One public key per line, 64 hexadecimal characters; blank lines and lines starting with `#` are ignored.
    pub fn from_text(text: &str) -> Result<Self, String> {
        let mut keys = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let bytes = decode_hex(line).map_err(|_| {
                format!(
                    "line {}: a public key is 64 hexadecimal characters",
                    index + 1
                )
            })?;
            let raw: [u8; 32] = bytes.try_into().map_err(|_| {
                format!(
                    "line {}: a public key is 64 hexadecimal characters",
                    index + 1
                )
            })?;
            let key = VerifyingKey::from_bytes(&raw)
                .map_err(|_| format!("line {}: not a valid Ed25519 public key", index + 1))?;
            let id: String = raw[..8].iter().map(|b| format!("{b:02x}")).collect();
            keys.push((id, key));
        }
        Ok(TrustedKeys { keys })
    }

    fn find(&self, key_id: &str) -> Option<&VerifyingKey> {
        self.keys
            .iter()
            .find(|(id, _)| id == key_id)
            .map(|(_, key)| key)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CertificatePolicy {
    pub max_age_ms: u64,
    pub max_future_skew_ms: u64,
}

impl Default for CertificatePolicy {
    fn default() -> Self {
        CertificatePolicy {
            max_age_ms: DEFAULT_MAX_AGE_MS,
            max_future_skew_ms: MAX_FUTURE_SKEW_MS,
        }
    }
}

/// The nonces already accepted, bounded: one certificate is good for one command.
#[derive(Debug, Default)]
pub struct NonceCache {
    order: VecDeque<String>,
    seen: HashSet<String>,
}

impl NonceCache {
    /// Remember a nonce; false when it was already there (a replay).
    pub fn insert(&mut self, nonce: &str) -> bool {
        if self.seen.contains(nonce) {
            return false;
        }
        if self.order.len() >= NONCE_CACHE_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        self.order.push_back(nonce.to_string());
        self.seen.insert(nonce.to_string());
        true
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.order.len()
    }
}

/// Check a certificate for a command. Returns the twin's risk report when it is genuine, current, new and for this command, or the
/// reason it is not (for the answer and for the audit); the reasons never say more than the caller already knows.
pub fn verify(
    certificate: &Certificate,
    command: &JointCommand,
    keys: &TrustedKeys,
    policy: &CertificatePolicy,
    now_ms: u64,
    nonces: &mut NonceCache,
) -> Result<TwinRiskReport, String> {
    if keys.is_empty() {
        return Err("no twin public key is configured on this bridge (--twin-public-key-file): a real command cannot be certified".into());
    }
    let Some(key) = keys.find(&certificate.key_id) else {
        return Err("the certificate names a key this bridge does not trust".into());
    };
    let signature_bytes: [u8; 64] = decode_hex(&certificate.signature)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or("the certificate signature is not 64 bytes of hexadecimal")?;
    if key
        .verify(
            certificate.signed_text().as_bytes(),
            &Signature::from_bytes(&signature_bytes),
        )
        .is_err()
    {
        return Err("the certificate signature is not valid".into());
    }
    if certificate.joint != command.joint || certificate.position != command.position {
        return Err("the certificate is for another command (joint or position differ)".into());
    }
    if certificate.issued_at_ms > now_ms.saturating_add(policy.max_future_skew_ms) {
        return Err("the certificate is dated in the future".into());
    }
    if now_ms.saturating_sub(certificate.issued_at_ms) > policy.max_age_ms {
        return Err(format!(
            "the certificate is older than {} ms: ask the twin again",
            policy.max_age_ms
        ));
    }
    if certificate.nonce.len() != 32 || !certificate.nonce.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("the certificate nonce is not 32 hexadecimal characters".into());
    }
    if !nonces.insert(&certificate.nonce) {
        return Err("the certificate was already used".into());
    }
    Ok(TwinRiskReport {
        collision_imminent: certificate.collision_imminent,
        distance_m: certificate.distance_m,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Produced by HYDRA-UMC-TWIN for the seed [7; 32] and checked against an independent RFC 8032 implementation.
    pub const VECTOR_PUBLIC_KEY: &str =
        "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    pub const VECTOR_KEY_ID: &str = "ea4a6c63e29c520a";
    pub const VECTOR_NONCE: &str = "00112233445566778899aabbccddeeff";
    pub const VECTOR_SIGNATURE: &str = "325a7b5fda1f54fc002b73616ea3e786eb9d7418d572460c11e21d7edde313f76d4ddc0d028c5a510885e7c3b3a923e055283d188ec6774654a41962e664850a";
    const ISSUED: u64 = 1_700_000_000_000;

    fn cert() -> Certificate {
        Certificate {
            joint: "shoulder".into(),
            position: 0.5,
            collision_imminent: false,
            distance_m: 0.4,
            issued_at_ms: ISSUED,
            nonce: VECTOR_NONCE.into(),
            key_id: VECTOR_KEY_ID.into(),
            signature: VECTOR_SIGNATURE.into(),
        }
    }

    fn command() -> JointCommand {
        JointCommand {
            joint: "shoulder".into(),
            position: 0.5,
        }
    }

    fn keys() -> TrustedKeys {
        TrustedKeys::from_text(&format!("# the twin\n{VECTOR_PUBLIC_KEY}\n")).unwrap()
    }

    fn check(c: &Certificate, now: u64) -> Result<TwinRiskReport, String> {
        verify(
            c,
            &command(),
            &keys(),
            &CertificatePolicy::default(),
            now,
            &mut NonceCache::default(),
        )
    }

    #[test]
    fn the_published_vector_verifies() {
        let report = check(&cert(), ISSUED + 100).unwrap();
        assert!(!report.collision_imminent);
        assert_eq!(report.distance_m, 0.4);
        assert_eq!(keys().key_ids(), vec![VECTOR_KEY_ID]);
    }

    #[test]
    fn the_signed_text_is_the_one_the_twin_builds() {
        assert_eq!(
            cert().signed_text(),
            format!("hydra-umc-interlock-certificate-v1\n{VECTOR_KEY_ID}\nshoulder\n0.5\nfalse\n0.4\n{ISSUED}\n{VECTOR_NONCE}")
        );
    }

    #[test]
    fn changing_a_signed_field_is_a_bad_signature() {
        let mut variants = Vec::new();
        let mut c = cert();
        c.distance_m = 9.0;
        variants.push(c);
        let mut c = cert();
        c.collision_imminent = true;
        variants.push(c);
        let mut c = cert();
        c.issued_at_ms += 1;
        variants.push(c);
        let mut c = cert();
        c.nonce = "ffffffffffffffffffffffffffffffff".into();
        variants.push(c);
        for variant in variants {
            let reason = check(&variant, ISSUED + 100).unwrap_err();
            assert!(reason.contains("signature is not valid"), "{reason}");
        }
    }

    #[test]
    fn a_certificate_for_another_command_is_refused() {
        // the fields of the vector are for shoulder/0.5: asking for another joint or position fails after the signature is shown valid
        for other in [
            JointCommand {
                joint: "elbow".into(),
                position: 0.5,
            },
            JointCommand {
                joint: "shoulder".into(),
                position: 0.6,
            },
            JointCommand {
                joint: "shoulder".into(),
                position: 0.5000000001,
            },
        ] {
            let reason = verify(
                &cert(),
                &other,
                &keys(),
                &CertificatePolicy::default(),
                ISSUED + 10,
                &mut NonceCache::default(),
            )
            .unwrap_err();
            assert!(reason.contains("another command"), "{reason}");
        }
    }

    #[test]
    fn an_old_certificate_and_one_from_the_future_are_refused() {
        assert!(check(&cert(), ISSUED + 2_000).is_ok());
        assert!(check(&cert(), ISSUED + 2_001)
            .unwrap_err()
            .contains("older"));
        assert!(check(&cert(), ISSUED - 1_000).is_ok());
        assert!(check(&cert(), ISSUED - 1_001)
            .unwrap_err()
            .contains("future"));
    }

    #[test]
    fn the_age_is_configurable() {
        let policy = CertificatePolicy {
            max_age_ms: 10_000,
            max_future_skew_ms: 0,
        };
        let ok = verify(
            &cert(),
            &command(),
            &keys(),
            &policy,
            ISSUED + 9_000,
            &mut NonceCache::default(),
        );
        assert!(ok.is_ok());
        let early = verify(
            &cert(),
            &command(),
            &keys(),
            &policy,
            ISSUED - 1,
            &mut NonceCache::default(),
        );
        assert!(early.unwrap_err().contains("future"));
    }

    #[test]
    fn a_certificate_is_good_once() {
        let mut nonces = NonceCache::default();
        let policy = CertificatePolicy::default();
        assert!(verify(
            &cert(),
            &command(),
            &keys(),
            &policy,
            ISSUED + 5,
            &mut nonces
        )
        .is_ok());
        let again = verify(
            &cert(),
            &command(),
            &keys(),
            &policy,
            ISSUED + 6,
            &mut nonces,
        )
        .unwrap_err();
        assert!(again.contains("already used"), "{again}");
    }

    #[test]
    fn a_refused_certificate_does_not_use_up_its_nonce() {
        let mut nonces = NonceCache::default();
        let policy = CertificatePolicy::default();
        // refused for age first: the nonce is not recorded, so a fresh check of the same certificate is not "already used"
        assert!(verify(
            &cert(),
            &command(),
            &keys(),
            &policy,
            ISSUED + 99_999,
            &mut nonces
        )
        .is_err());
        assert_eq!(nonces.len(), 0);
        assert!(verify(
            &cert(),
            &command(),
            &keys(),
            &policy,
            ISSUED + 5,
            &mut nonces
        )
        .is_ok());
    }

    #[test]
    fn a_key_that_is_not_trusted_and_no_key_at_all_are_refused() {
        let mut c = cert();
        c.key_id = "0000000000000000".into();
        assert!(check(&c, ISSUED).unwrap_err().contains("does not trust"));
        let none = TrustedKeys::default();
        let reason = verify(
            &cert(),
            &command(),
            &none,
            &CertificatePolicy::default(),
            ISSUED,
            &mut NonceCache::default(),
        )
        .unwrap_err();
        assert!(reason.contains("no twin public key"), "{reason}");
    }

    #[test]
    fn a_malformed_signature_or_nonce_is_refused_not_a_panic() {
        for bad in ["", "zz", "abc", &"00".repeat(63), &"00".repeat(65)] {
            let mut c = cert();
            c.signature = bad.to_string();
            assert!(check(&c, ISSUED).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_nonce_cache_is_bounded_and_forgets_the_oldest() {
        let mut cache = NonceCache::default();
        for i in 0..NONCE_CACHE_CAPACITY + 10 {
            assert!(cache.insert(&format!("{i:032x}")));
        }
        assert_eq!(cache.len(), NONCE_CACHE_CAPACITY);
        assert!(cache.insert(&format!("{:032x}", 0))); // the oldest was forgotten
        assert!(!cache.insert(&format!("{:032x}", NONCE_CACHE_CAPACITY + 9))); // the newest is still remembered
    }

    #[test]
    fn the_key_file_takes_one_key_per_line_and_refuses_anything_else() {
        assert_eq!(TrustedKeys::from_text("").unwrap().len(), 0);
        assert_eq!(
            TrustedKeys::from_text(&format!(
                "\n# note\n  {VECTOR_PUBLIC_KEY}  \n{VECTOR_PUBLIC_KEY}\n"
            ))
            .unwrap()
            .len(),
            2
        );
        for bad in ["zz", "ab", &"ab".repeat(31), &"ab".repeat(33)] {
            assert!(TrustedKeys::from_text(bad).is_err(), "{bad}");
        }
    }
}
