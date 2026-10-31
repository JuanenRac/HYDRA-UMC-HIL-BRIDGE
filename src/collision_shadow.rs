// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/collision_shadow.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! The collision prediction of the twin, asked in shadow.
//!
//! With `--collision-shadow` (off by default; it needs `--twin-url`) the bridge asks `POST /predict/collision` of the twin what the joint
//! of a real command that was just delivered is going to do to the robot: would moving this joint to the commanded position bring the
//! robot into a protected zone or into itself? The answer is recorded (the audit log, `GET /stats`, the header `X-Collision-Shadow` of the
//! reply) and nothing else. It never blocks a command, never changes the decision of the interlock and is asked after the command went, so
//! it only tells an operator what the interlock would have said if it had been asked. A twin that does not answer, that has the route off or
//! that refuses the question is a `failed` shadow, counted, and never read as "clear".
//!
//! The prediction names only the joint that moves; the twin takes the others from the last positions of the real joints that this bridge and
//! the controller reported to it.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::bridge::TransportError;

/// What the twin predicted for one command.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowVerdict {
    /// A contact with a zone or with the robot itself is predicted.
    pub collision: bool,
    /// "clear", "warning" or "danger" (the worst zone level reached).
    pub level: String,
    pub self_collision: bool,
    pub hits_total: u64,
}

impl ShadowVerdict {
    pub fn kind(&self) -> &'static str {
        if self.collision {
            "collision"
        } else {
            "clear"
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "level {}, self collision {}, {} contact(s)",
            self.level, self.self_collision, self.hits_total
        )
    }
}

/// Anything that can ask the twin for a prediction.
pub trait CollisionShadow: Send + Sync + std::fmt::Debug {
    fn predict(&self, joint: &str, position: f64) -> Result<ShadowVerdict, TransportError>;
}

/// Reads the answer of `POST /predict/collision`. Anything that is not the full answer of a successful prediction is an error.
pub fn parse_verdict(status: u16, body: &str) -> Result<ShadowVerdict, TransportError> {
    if status != 200 {
        return Err(TransportError::Rejected { status });
    }
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|_| TransportError::Rejected { status: 502 })?;
    let collision = value
        .get("collision")
        .and_then(serde_json::Value::as_bool)
        .ok_or(TransportError::Rejected { status: 502 })?;
    let level = value
        .get("level")
        .and_then(serde_json::Value::as_str)
        .ok_or(TransportError::Rejected { status: 502 })?
        .to_string();
    Ok(ShadowVerdict {
        collision,
        level,
        self_collision: value
            .get("self_collision")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        hits_total: value
            .get("hits_total")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
    })
}

/// `POST /predict/collision` of HYDRA-UMC-TWIN over plain HTTP.
#[derive(Debug, Clone)]
pub struct HttpCollisionShadow {
    /// `host:port`
    pub addr: String,
    pub token: String,
    pub timeout: Duration,
}

impl CollisionShadow for HttpCollisionShadow {
    fn predict(&self, joint: &str, position: f64) -> Result<ShadowVerdict, TransportError> {
        let timeout_ms = self.timeout.as_millis() as u64;
        let timed_out = || TransportError::Timeout {
            after_ms: timeout_ms,
        };
        let addr = self
            .addr
            .to_socket_addrs()
            .map_err(|_| TransportError::Disconnected)?
            .next()
            .ok_or(TransportError::Disconnected)?;
        let mut stream = TcpStream::connect_timeout(&addr, self.timeout)
            .map_err(|_| TransportError::Disconnected)?;
        let _ = stream.set_read_timeout(Some(self.timeout));
        let _ = stream.set_write_timeout(Some(self.timeout));
        let body = serde_json::json!({"states": [{ joint: position }]}).to_string();
        let request = format!(
            "POST /predict/collision HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.addr,
            self.token,
            body.len(),
            body
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|_| timed_out())?;
        // the twin closes the connection after the answer: read it whole, within a bound
        let mut raw = Vec::new();
        let mut limited = (&mut stream).take(256 * 1024);
        limited.read_to_end(&mut raw).map_err(|_| timed_out())?;
        let text = String::from_utf8_lossy(&raw);
        let (head, answer) = text
            .split_once("\r\n\r\n")
            .ok_or(TransportError::Disconnected)?;
        let status: u16 = head
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or(TransportError::Disconnected)?;
        parse_verdict(status, answer)
    }
}

/// What happened to the question of one command.
#[derive(Debug, Clone, PartialEq)]
pub enum ShadowOutcome {
    Answered(ShadowVerdict),
    Failed { reason: String },
}

impl ShadowOutcome {
    pub fn kind(&self) -> &'static str {
        match self {
            ShadowOutcome::Answered(v) => v.kind(),
            ShadowOutcome::Failed { .. } => "failed",
        }
    }
}

/// Asks the twin and says what happened.
pub fn ask(shadow: &dyn CollisionShadow, joint: &str, position: f64) -> ShadowOutcome {
    match shadow.predict(joint, position) {
        Ok(verdict) => ShadowOutcome::Answered(verdict),
        Err(e) => ShadowOutcome::Failed {
            reason: e.to_string(),
        },
    }
}

/// A stand-in for the twin in tests: answers with a fixed verdict, or fails.
#[cfg(test)]
#[derive(Debug)]
pub struct ScriptedShadow {
    pub verdict: Result<ShadowVerdict, TransportError>,
    pub asked: std::sync::Mutex<Vec<(String, f64)>>,
}

#[cfg(test)]
impl ScriptedShadow {
    pub fn answering(collision: bool) -> Self {
        Self {
            verdict: Ok(ShadowVerdict {
                collision,
                level: if collision { "danger" } else { "clear" }.to_string(),
                self_collision: false,
                hits_total: u64::from(collision),
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

    pub fn questions(&self) -> Vec<(String, f64)> {
        self.asked.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

#[cfg(test)]
impl CollisionShadow for ScriptedShadow {
    fn predict(&self, joint: &str, position: f64) -> Result<ShadowVerdict, TransportError> {
        self.asked
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((joint.to_string(), position));
        self.verdict.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_complete_successful_answer_is_a_verdict() {
        let ok = r#"{"collision":true,"level":"danger","self_collision":false,"hits_total":3,"samples":10}"#;
        let v = parse_verdict(200, ok).unwrap();
        assert!(v.collision && v.level == "danger" && v.hits_total == 3);
        assert_eq!(v.kind(), "collision");
        let clear = parse_verdict(200, r#"{"collision":false,"level":"clear"}"#).unwrap();
        assert_eq!(clear.kind(), "clear");
        // every other status, and an answer that lacks the verdict, are failures: never "clear"
        for status in [400, 401, 404, 405, 422, 502, 503, 504] {
            assert!(parse_verdict(status, ok).is_err(), "{status}");
        }
        for body in [
            "",
            "nope",
            "{}",
            r#"{"collision":"no","level":"clear"}"#,
            r#"{"collision":false}"#,
        ] {
            assert!(parse_verdict(200, body).is_err(), "{body}");
        }
    }

    #[test]
    fn asking_reports_the_failure_with_its_reason() {
        let down = ScriptedShadow::failing();
        match ask(&down, "shoulder", 0.5) {
            ShadowOutcome::Failed { reason } => {
                assert!(reason.contains("disconnected"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(down.questions(), vec![("shoulder".to_string(), 0.5)]);
        let yes = ScriptedShadow::answering(true);
        assert_eq!(ask(&yes, "j", 1.0).kind(), "collision");
        assert_eq!(
            ask(&ScriptedShadow::answering(false), "j", 1.0).kind(),
            "clear"
        );
    }

    /// A twin that answers one request with a canned reply; returns its address and what it received.
    fn canned_twin(reply: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = vec![0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        (addr, rx)
    }

    fn http_shadow(addr: String) -> HttpCollisionShadow {
        HttpCollisionShadow {
            addr,
            token: "a-token-of-at-least-16-characters".to_string(),
            timeout: Duration::from_millis(1500),
        }
    }

    #[test]
    fn the_question_names_only_the_joint_and_carries_the_token() {
        let (addr, received) = canned_twin(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"collision\":true,\"level\":\"warning\",\"self_collision\":false,\"hits_total\":2}",
        );
        let verdict = http_shadow(addr).predict("shoulder", 0.5).unwrap();
        assert_eq!(verdict.level, "warning");
        let request = received.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            request.starts_with("POST /predict/collision HTTP/1.1"),
            "{request}"
        );
        assert!(
            request.contains("Authorization: Bearer a-token-of-at-least-16-characters"),
            "{request}"
        );
        assert!(
            request.ends_with(r#"{"states":[{"shoulder":0.5}]}"#),
            "{request}"
        );
    }

    #[test]
    fn a_twin_with_the_route_off_or_down_is_a_failure() {
        let (addr, _) = canned_twin("HTTP/1.1 405 Method Not Allowed\r\nConnection: close\r\n\r\n{\"error\":\"method not allowed\"}");
        assert_eq!(
            http_shadow(addr).predict("j", 1.0),
            Err(TransportError::Rejected { status: 405 })
        );
        assert_eq!(
            http_shadow("127.0.0.1:1".to_string()).predict("j", 1.0),
            Err(TransportError::Disconnected)
        );
    }
}
