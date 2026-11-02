// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/safety_zones.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! The state of the protected zones as one more condition of the interlock.
//!
//! With `--safety-zones-url` (off by default) the bridge asks `GET /state` of HYDRA-UMC-SAFETY-ZONES before it routes a real command, in
//! addition to the twin: a real command goes on only while the zone monitor says `READY` and said it recently. Any other state (`WARNING`,
//! `DANGER`, `INHIBITED`, `FAULT`), a monitor that does not answer, has no verdict yet or whose verdict is older than
//! `--safety-zones-max-age-ms`, or a state whose time cannot be read, refuses the command: "the monitor did not say" is not "the zones are
//! clear". Like the rest of the bridge this can only refuse to forward a command; the physical stop belongs to the firmware.
//!
//! Simulation commands are not asked about.

use std::time::Duration;

use crate::bridge::TransportError;
use crate::http_client;

/// What the zone monitor said last.
#[derive(Debug, Clone, PartialEq)]
pub struct ZonesVerdict {
    /// "READY", "WARNING", "DANGER", "INHIBITED" or "FAULT" (the states of the safety contract of the SDK).
    pub state: String,
    /// When the monitor reached it (epoch milliseconds), when it said so in a way that can be read.
    pub at_ms: Option<u64>,
}

/// Anything that can tell the state of the zones.
pub trait ZonesState: Send + Sync + std::fmt::Debug {
    fn state(&self) -> Result<ZonesVerdict, TransportError>;
}

/// What the interlock makes of one answer.
#[derive(Debug, Clone, PartialEq)]
pub enum ZonesDecision {
    Allow,
    /// The monitor answered and the zones are not clear.
    Blocked {
        reason: String,
    },
    /// The monitor did not say, or said too long ago to be believed.
    Unavailable {
        reason: String,
    },
}

/// Reads the answer of `GET /state`. Anything that is not the full answer of a verdict is an error.
pub fn parse_state(status: u16, body: &str) -> Result<ZonesVerdict, TransportError> {
    if status != 200 {
        return Err(TransportError::Rejected { status });
    }
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| TransportError::Rejected { status: 502 })?;
    let sdk = value
        .get("sdkSafetyState")
        .ok_or(TransportError::Rejected { status: 502 })?;
    let state = sdk
        .get("state")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(TransportError::Rejected { status: 502 })?
        .to_string();
    let at_ms = sdk
        .get("timestamp_utc")
        .and_then(serde_json::Value::as_str)
        .and_then(parse_iso_utc_ms);
    Ok(ZonesVerdict { state, at_ms })
}

/// The decision for one answer (or for no answer). Fail closed.
pub fn assess(
    verdict: &Result<ZonesVerdict, TransportError>,
    now_ms: u64,
    max_age_ms: u64,
) -> ZonesDecision {
    let verdict = match verdict {
        Ok(verdict) => verdict,
        Err(e) => {
            return ZonesDecision::Unavailable {
                reason: format!("the zone monitor gave no state: {e}"),
            }
        }
    };
    if verdict.state != "READY" {
        return ZonesDecision::Blocked {
            reason: format!("the zone monitor reports {}", verdict.state),
        };
    }
    match verdict.at_ms {
        None => ZonesDecision::Unavailable {
            reason: "the zone monitor gave no readable time for its state".to_string(),
        },
        Some(at) if at > now_ms.saturating_add(max_age_ms.min(2_000)) => {
            ZonesDecision::Unavailable {
                reason: "the state of the zone monitor is dated in the future".to_string(),
            }
        }
        Some(at) if now_ms.saturating_sub(at) > max_age_ms => ZonesDecision::Unavailable {
            reason: format!(
                "the state of the zone monitor is {} ms old (at most {max_age_ms} ms)",
                now_ms.saturating_sub(at)
            ),
        },
        Some(_) => ZonesDecision::Allow,
    }
}

/// `GET /state` of HYDRA-UMC-SAFETY-ZONES over plain HTTP.
#[derive(Debug, Clone)]
pub struct HttpZonesState {
    /// `host:port`
    pub addr: String,
    pub token: Option<String>,
    pub timeout: Duration,
}

impl ZonesState for HttpZonesState {
    fn state(&self) -> Result<ZonesVerdict, TransportError> {
        let answer = http_client::request(
            &self.addr,
            "GET",
            "/state",
            self.token.as_deref(),
            None,
            self.timeout,
            64 * 1024,
        )?;
        parse_state(answer.status, &answer.body)
    }
}

/// `2026-01-02T03:04:05.123456+00:00` (also `Z`, an offset, no fraction) to epoch milliseconds. None when it is anything else.
pub fn parse_iso_utc_ms(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' || !matches!(bytes[10], b'T' | b' ')
    {
        return None;
    }
    let num = |from: usize, to: usize| text.get(from..to)?.parse::<i64>().ok();
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let mut rest = &text[19..];
    let mut millis = 0i64;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits: String = fraction
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if digits.is_empty() {
            return None;
        }
        let first3 = format!("{:0<3}", &digits[..digits.len().min(3)]);
        millis = first3.parse().ok()?;
        rest = &fraction[digits.len()..];
    }
    let offset_minutes = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let tail = &rest[1..];
            let (h, m) = match tail.len() {
                5 if tail.as_bytes()[2] == b':' => (
                    tail[0..2].parse::<i64>().ok()?,
                    tail[3..5].parse::<i64>().ok()?,
                ),
                4 => (
                    tail[0..2].parse::<i64>().ok()?,
                    tail[2..4].parse::<i64>().ok()?,
                ),
                2 => (tail.parse::<i64>().ok()?, 0),
                _ => return None,
            };
            sign * (h * 60 + m)
        }
    };
    // days since 1970-01-01 (the civil-from-days algorithm of H. Hinnant, inverted)
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    u64::try_from(seconds * 1_000 + millis).ok()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stand-in for the zone monitor in tests: answers with a fixed verdict, or fails.
    #[derive(Debug)]
    pub struct ScriptedZones {
        pub verdict: Result<ZonesVerdict, TransportError>,
        pub asked: std::sync::atomic::AtomicU64,
    }

    impl ScriptedZones {
        pub fn saying(state: &str, at_ms: Option<u64>) -> Self {
            Self {
                verdict: Ok(ZonesVerdict {
                    state: state.to_string(),
                    at_ms,
                }),
                asked: Default::default(),
            }
        }

        pub fn failing() -> Self {
            Self {
                verdict: Err(TransportError::Disconnected),
                asked: Default::default(),
            }
        }
    }

    impl ZonesState for ScriptedZones {
        fn state(&self) -> Result<ZonesVerdict, TransportError> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.verdict.clone()
        }
    }

    #[test]
    fn the_iso_time_of_the_monitor_is_read_to_milliseconds() {
        assert_eq!(parse_iso_utc_ms("1970-01-01T00:00:00+00:00"), Some(0));
        assert_eq!(parse_iso_utc_ms("1970-01-01T00:00:01.5Z"), Some(1_500));
        assert_eq!(
            parse_iso_utc_ms("2026-03-04T05:06:07.123456+00:00"),
            Some(1_772_600_767_123)
        );
        // an offset is subtracted: 02:00 at +02:00 is 00:00 UTC
        assert_eq!(
            parse_iso_utc_ms("2000-01-01T02:00:00+02:00"),
            parse_iso_utc_ms("2000-01-01T00:00:00Z")
        );
        // a leap day and a date before the year 2000
        assert_eq!(
            parse_iso_utc_ms("2000-02-29T00:00:00Z"),
            Some(951_782_400_000)
        );
        for bad in [
            "",
            "yesterday",
            "2026-13-01T00:00:00Z",
            "2026-01-01T25:00:00Z",
            "2026-01-01T00:00:00.Z",
            "2026-01-01T00:00:00+9",
        ] {
            assert_eq!(parse_iso_utc_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_a_recent_ready_lets_a_command_through() {
        let now = 1_772_600_767_123u64;
        let ready = |at: Option<u64>| {
            Ok(ZonesVerdict {
                state: "READY".to_string(),
                at_ms: at,
            })
        };
        assert_eq!(
            assess(&ready(Some(now - 100)), now, 5_000),
            ZonesDecision::Allow
        );
        assert!(matches!(
            assess(&ready(Some(now - 6_000)), now, 5_000),
            ZonesDecision::Unavailable { .. }
        ));
        assert!(matches!(
            assess(&ready(Some(now + 60_000)), now, 5_000),
            ZonesDecision::Unavailable { .. }
        ));
        assert!(matches!(
            assess(&ready(None), now, 5_000),
            ZonesDecision::Unavailable { .. }
        ));
        for state in [
            "WARNING",
            "DANGER",
            "INHIBITED",
            "FAULT",
            "ready",
            "whatever",
        ] {
            let verdict = Ok(ZonesVerdict {
                state: state.to_string(),
                at_ms: Some(now),
            });
            assert!(
                matches!(assess(&verdict, now, 5_000), ZonesDecision::Blocked { .. }),
                "{state}"
            );
        }
        assert!(matches!(
            assess(&Err(TransportError::Disconnected), now, 5_000),
            ZonesDecision::Unavailable { .. }
        ));
    }

    #[test]
    fn only_the_full_answer_of_a_verdict_is_read() {
        let ok = r#"{"sdkSafetyState":{"schema_version":"1.0","state":"READY","source":"z","timestamp_utc":"2026-03-04T05:06:07+00:00"},"reason":"clear"}"#;
        let verdict = parse_state(200, ok).unwrap();
        assert_eq!(verdict.state, "READY");
        assert_eq!(verdict.at_ms, Some(1_772_600_767_000));
        // an unreadable time is kept as None (the decision refuses it), the rest of the answer still counts
        let odd = r#"{"sdkSafetyState":{"state":"DANGER","timestamp_utc":"later"}}"#;
        assert_eq!(parse_state(200, odd).unwrap().at_ms, None);
        for status in [401, 404, 500, 503] {
            assert!(parse_state(status, ok).is_err(), "{status}");
        }
        for body in [
            "",
            "nope",
            "{}",
            r#"{"sdkSafetyState":{}}"#,
            r#"{"sdkSafetyState":{"state":""}}"#,
        ] {
            assert!(parse_state(200, body).is_err(), "{body}");
        }
    }

    #[test]
    fn the_http_state_reads_a_canned_monitor_with_its_token() {
        let body =
            r#"{"sdkSafetyState":{"state":"WARNING","timestamp_utc":"2026-03-04T05:06:07Z"}}"#;
        let peer =
            crate::http_client::tests::canned_peer(vec![(200, String::new(), body.to_string())]);
        let zones = HttpZonesState {
            addr: peer.addr.clone(),
            token: Some("zones-token-of-16-chars".to_string()),
            timeout: Duration::from_secs(2),
        };
        let verdict = zones.state().unwrap();
        assert_eq!(verdict.state, "WARNING");
        let asked = peer.requests.lock().unwrap()[0].clone();
        assert!(asked.starts_with("GET /state HTTP/1.1"));
        assert!(asked.contains("Authorization: Bearer zones-token-of-16-chars"));
    }
}
