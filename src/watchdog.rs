// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/watchdog.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! The consumer-side watchdog of the real commands.
//!
//! A producer that follows a moving target (visual servoing) sends commands that are only good for a short time: each carries when it
//! was issued (`issued_at_ms`), how long it is good for (`valid_for_ms`) and a sequence number (`seq`). A real command with that window
//! is a lease on its joint: the bridge refuses a command that is already expired, issued in the future, good for longer than the
//! configured maximum, or not newer than the last one accepted for the joint, and when the lease runs out without a newer valid command
//! the bridge stops the joint by itself instead of leaving the last target in force. The producer may have died, lost its detection or
//! lost the link: the stop does not depend on it.
//!
//! The clock is a parameter, so every rule here is tested without waiting.

use std::collections::HashMap;

/// The longest lease accepted, in milliseconds, unless configured otherwise.
pub const DEFAULT_MAX_VALID_FOR_MS: u64 = 1000;
/// How far in the future `issued_at_ms` may lie (clock difference between the producer and this bridge).
pub const DEFAULT_FUTURE_SKEW_MS: u64 = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub struct Validity {
    pub issued_at_ms: u64,
    pub valid_for_ms: u64,
    /// Strictly increasing per joint; a repeated or older number is refused.
    pub seq: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchdogConfig {
    /// A real command without a validity window is refused. The library default is off (the callers that predate the window);
    /// `serve` turns it on unless `--allow-undated-commands` is given.
    pub require_validity: bool,
    pub max_valid_for_ms: u64,
    pub future_skew_ms: u64,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        WatchdogConfig {
            require_validity: false,
            max_valid_for_ms: DEFAULT_MAX_VALID_FOR_MS,
            future_skew_ms: DEFAULT_FUTURE_SKEW_MS,
        }
    }
}

#[derive(Debug, Clone)]
struct Lease {
    expires_at_ms: u64,
    last_seq: Option<u64>,
    last_position: f64,
    stopped: bool,
}

/// A joint the watchdog stopped because its lease ran out.
#[derive(Debug, Clone, PartialEq)]
pub struct Stop {
    pub joint: String,
    pub last_position: f64,
    pub expired_at_ms: u64,
}

#[derive(Debug, Default)]
pub struct Leases {
    config: WatchdogConfig,
    by_joint: HashMap<String, Lease>,
}

impl Leases {
    pub fn new(config: WatchdogConfig) -> Self {
        Leases {
            config,
            by_joint: HashMap::new(),
        }
    }

    /// How many joints hold a lease that has not run out.
    pub fn active(&self, now_ms: u64) -> usize {
        self.by_joint
            .values()
            .filter(|l| !l.stopped && l.expires_at_ms > now_ms)
            .count()
    }

    /// Whether a real command to `joint` may go ahead. Nothing is recorded: the lease starts when the command was delivered (`grant`).
    pub fn check(
        &self,
        joint: &str,
        validity: Option<&Validity>,
        now_ms: u64,
    ) -> Result<(), String> {
        let Some(v) = validity else {
            if self.config.require_validity {
                return Err("a real command needs its validity window (issued_at_ms and valid_for_ms): without one the last target would stay in force if the producer stopped".to_string());
            }
            return Ok(());
        };
        if v.valid_for_ms == 0 {
            return Err("valid_for_ms must be above zero".to_string());
        }
        if v.valid_for_ms > self.config.max_valid_for_ms {
            return Err(format!(
                "valid_for_ms {} is above the {} ms this bridge accepts",
                v.valid_for_ms, self.config.max_valid_for_ms
            ));
        }
        if v.issued_at_ms > now_ms.saturating_add(self.config.future_skew_ms) {
            return Err("issued_at_ms is in the future".to_string());
        }
        if v.issued_at_ms.saturating_add(v.valid_for_ms) <= now_ms {
            return Err("the command is already expired".to_string());
        }
        if let (Some(seq), Some(lease)) = (v.seq, self.by_joint.get(joint)) {
            if let Some(last) = lease.last_seq {
                if seq <= last {
                    return Err(format!(
                        "seq {seq} is not newer than {last}, the last one accepted for this joint"
                    ));
                }
            }
        }
        Ok(())
    }

    /// A command with a validity window was delivered: the joint holds a lease until `issued_at_ms + valid_for_ms`.
    pub fn grant(&mut self, joint: &str, position: f64, validity: &Validity) {
        let lease = self.by_joint.entry(joint.to_string()).or_insert(Lease {
            expires_at_ms: 0,
            last_seq: None,
            last_position: position,
            stopped: false,
        });
        lease.expires_at_ms = validity.issued_at_ms.saturating_add(validity.valid_for_ms);
        lease.last_position = position;
        lease.stopped = false;
        if validity.seq.is_some() {
            lease.last_seq = validity.seq;
        }
    }

    /// The joints whose lease has run out since the last call, each reported once. The caller stops them.
    pub fn due_stops(&mut self, now_ms: u64) -> Vec<Stop> {
        let mut stops = Vec::new();
        for (joint, lease) in self.by_joint.iter_mut() {
            if !lease.stopped && lease.expires_at_ms <= now_ms {
                lease.stopped = true;
                stops.push(Stop {
                    joint: joint.clone(),
                    last_position: lease.last_position,
                    expired_at_ms: lease.expires_at_ms,
                });
            }
        }
        stops.sort_by(|a, b| a.joint.cmp(&b.joint));
        stops
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leases(require: bool) -> Leases {
        Leases::new(WatchdogConfig {
            require_validity: require,
            ..WatchdogConfig::default()
        })
    }

    fn window(issued: u64, valid_for: u64, seq: Option<u64>) -> Validity {
        Validity {
            issued_at_ms: issued,
            valid_for_ms: valid_for,
            seq,
        }
    }

    #[test]
    fn a_command_without_a_window_passes_only_when_it_is_not_required() {
        assert!(leases(false).check("j1", None, 1000).is_ok());
        assert!(leases(true).check("j1", None, 1000).is_err());
    }

    #[test]
    fn a_fresh_window_passes() {
        assert!(leases(true)
            .check("j1", Some(&window(1000, 200, Some(1))), 1050)
            .is_ok());
    }

    #[test]
    fn an_expired_or_future_or_overlong_or_empty_window_is_refused() {
        let l = leases(true);
        assert!(l.check("j1", Some(&window(1000, 200, None)), 1200).is_err());
        assert!(l.check("j1", Some(&window(1000, 200, None)), 5000).is_err());
        assert!(l.check("j1", Some(&window(9000, 200, None)), 1000).is_err());
        assert!(l
            .check("j1", Some(&window(1000, 5000, None)), 1000)
            .is_err());
        assert!(l.check("j1", Some(&window(1000, 0, None)), 1000).is_err());
        assert!(l
            .check("j1", Some(&window(u64::MAX, u64::MAX, None)), 1000)
            .is_err());
    }

    #[test]
    fn a_repeated_or_older_sequence_is_refused_per_joint() {
        let mut l = leases(true);
        l.grant("j1", 0.5, &window(1000, 200, Some(7)));
        assert!(l
            .check("j1", Some(&window(1050, 200, Some(7))), 1060)
            .is_err());
        assert!(l
            .check("j1", Some(&window(1050, 200, Some(3))), 1060)
            .is_err());
        assert!(l
            .check("j1", Some(&window(1050, 200, Some(8))), 1060)
            .is_ok());
        assert!(l
            .check("j2", Some(&window(1050, 200, Some(1))), 1060)
            .is_ok());
    }

    #[test]
    fn a_lease_that_runs_out_stops_the_joint_once() {
        let mut l = leases(true);
        l.grant("j1", 0.5, &window(1000, 200, Some(1)));
        assert_eq!(l.active(1100), 1);
        assert!(l.due_stops(1199).is_empty());
        let stops = l.due_stops(1200);
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0].joint, "j1");
        assert_eq!(stops[0].last_position, 0.5);
        assert_eq!(stops[0].expired_at_ms, 1200);
        assert!(l.due_stops(9999).is_empty(), "a stop is reported once");
        assert_eq!(l.active(1300), 0);
    }

    #[test]
    fn a_newer_command_renews_the_lease_and_it_can_stop_again_later() {
        let mut l = leases(true);
        l.grant("j1", 0.5, &window(1000, 200, Some(1)));
        l.grant("j1", 0.6, &window(1150, 200, Some(2)));
        assert!(l.due_stops(1200).is_empty());
        assert_eq!(l.due_stops(1350)[0].last_position, 0.6);
        l.grant("j1", 0.7, &window(2000, 200, Some(3)));
        assert_eq!(l.active(2100), 1);
        assert_eq!(l.due_stops(2200).len(), 1);
    }

    #[test]
    fn each_joint_has_its_own_lease() {
        let mut l = leases(true);
        l.grant("a", 1.0, &window(1000, 100, None));
        l.grant("b", 2.0, &window(1000, 500, None));
        let stops = l.due_stops(1100);
        assert_eq!(
            stops.iter().map(|s| s.joint.as_str()).collect::<Vec<_>>(),
            vec!["a"]
        );
        assert_eq!(l.due_stops(1500)[0].joint, "b");
    }
}
