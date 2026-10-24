// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/audit.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! Who sent each order that was meant for the real robot.
//!
//! Every real-mode order leaves two records: `order` (it arrived: who, from where, what, with which certificate), written BEFORE the
//! interlock is asked and before anything is sent, and `outcome` (what the bridge did with it). With `--audit-log FILE` both are appended
//! to a file as one JSON object per line, and if the first cannot be written the order is refused: an order that cannot be accounted for
//! does not reach the hardware. The last `RING_CAPACITY` records are also kept in memory and served by `GET /audit`.
//!
//! The caller is what the request says about itself (`X-Hydra-Caller`, at most 64 printable characters) together with what the bridge
//! itself saw: the address of the peer and whether the request carried the Bearer token. A name in a header is a label, not a proof: the
//! proof is the token, and one token is one identity until tokens are issued per caller.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;

pub const RING_CAPACITY: usize = 200;
pub const MAX_CALLER_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AuditEntry {
    pub at_ms: u64,
    /// `order` when it arrived, `outcome` when the bridge decided.
    pub phase: &'static str,
    pub caller: String,
    pub peer: String,
    /// True when the request carried the Bearer token this bridge asks for (false also when it asks for none).
    pub authenticated: bool,
    pub mode: &'static str,
    pub joint: String,
    pub position: f64,
    pub certificate_key_id: Option<String>,
    pub certificate_nonce: Option<String>,
    pub kind: Option<&'static str>,
    pub delivered: Option<bool>,
    pub reason: Option<String>,
}

/// The caller label of a request: the header value when it is short printable text, else `unidentified`.
pub fn caller_label(header: Option<&str>) -> String {
    match header.map(str::trim) {
        Some(value)
            if !value.is_empty()
                && value.chars().count() <= MAX_CALLER_CHARS
                && value.chars().all(|c| !c.is_control()) =>
        {
            value.to_string()
        }
        _ => "unidentified".to_string(),
    }
}

#[derive(Debug, Default)]
pub struct Audit {
    path: Option<PathBuf>,
    ring: VecDeque<AuditEntry>,
}

impl Audit {
    pub fn new(path: Option<PathBuf>) -> Self {
        Audit {
            path,
            ring: VecDeque::new(),
        }
    }

    pub fn has_file(&self) -> bool {
        self.path.is_some()
    }

    /// Append the record to the file (when there is one), then keep it in memory. An error means it is NOT on disk.
    pub fn record(&mut self, entry: AuditEntry) -> Result<(), String> {
        if let Some(path) = &self.path {
            let line = serde_json::to_string(&entry)
                .map_err(|e| format!("cannot encode the audit record: {e}"))?;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("cannot open the audit log: {e}"))?;
            writeln!(file, "{line}").map_err(|e| format!("cannot write the audit log: {e}"))?;
            file.sync_data()
                .map_err(|e| format!("cannot flush the audit log: {e}"))?;
        }
        if self.ring.len() >= RING_CAPACITY {
            self.ring.pop_front();
        }
        self.ring.push_back(entry);
        Ok(())
    }

    pub fn recent(&self) -> Vec<&AuditEntry> {
        self.ring.iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(phase: &'static str, joint: &str) -> AuditEntry {
        AuditEntry {
            at_ms: 1,
            phase,
            caller: "orchestrator".into(),
            peer: "127.0.0.1:5000".into(),
            authenticated: true,
            mode: "real",
            joint: joint.into(),
            position: 0.5,
            certificate_key_id: None,
            certificate_nonce: None,
            kind: None,
            delivered: None,
            reason: None,
        }
    }

    #[test]
    fn a_caller_label_is_short_printable_text_or_unidentified() {
        assert_eq!(caller_label(Some("  planner-1 ")), "planner-1");
        assert_eq!(caller_label(None), "unidentified");
        assert_eq!(caller_label(Some("")), "unidentified");
        assert_eq!(caller_label(Some("bad\nname")), "unidentified");
        assert_eq!(caller_label(Some(&"x".repeat(65))), "unidentified");
        assert_eq!(caller_label(Some(&"x".repeat(64))), "x".repeat(64));
    }

    #[test]
    fn the_ring_keeps_only_the_most_recent_records() {
        let mut audit = Audit::new(None);
        for i in 0..RING_CAPACITY + 5 {
            audit.record(entry("order", &format!("j{i}"))).unwrap();
        }
        let recent = audit.recent();
        assert_eq!(recent.len(), RING_CAPACITY);
        assert_eq!(recent[0].joint, "j5");
        assert!(!audit.has_file());
    }

    #[test]
    fn records_are_appended_to_the_file_one_json_object_per_line() {
        let dir = std::env::temp_dir().join(format!("hil-audit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut audit = Audit::new(Some(path.clone()));
        audit.record(entry("order", "shoulder")).unwrap();
        audit.record(entry("outcome", "shoulder")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            (first["phase"].as_str(), first["caller"].as_str()),
            (Some("order"), Some("orchestrator"))
        );
        assert!(audit.has_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_that_cannot_be_written_is_an_error_and_not_in_memory_either() {
        let missing = std::env::temp_dir()
            .join("hil-audit-no-such-dir")
            .join("deeper")
            .join("audit.jsonl");
        let mut audit = Audit::new(Some(missing));
        let error = audit.record(entry("order", "shoulder")).unwrap_err();
        assert!(error.contains("audit log"), "{error}");
        assert!(audit.recent().is_empty());
    }
}
