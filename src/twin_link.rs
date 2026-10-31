// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/twin_link.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! The way back to the twin: where the real robot is.
//!
//! The twin signs the risk of a command from what it believes about the robot, so what it believes must follow the hardware. After a real
//! command was delivered the bridge reports the position to the twin (`commanded`), and when the controller reads a joint back the bridge
//! reports that (`measured`). The shape and the rules are the ones of `POST /interlock/state` of HYDRA-UMC-TWIN:
//!
//! ```json
//! {"joint":"shoulder","position":0.5,"seq":7,"stamp_ms":1700000000000,"source":"measured"}
//! ```
//!
//! `seq` grows with every report of a joint and the twin refuses one that is not newer, so a late or replayed message cannot move it back.
//! The sequence is made from the clock as well as a counter, so it also keeps growing when the bridge is restarted.
//!
//! A report that cannot be delivered never changes what happened to the command (it already went): it is counted, recorded and shown by
//! `GET /stats` (`twinInSync`). The fail-closed half lives in the twin: started with `--require-fresh-state-ms` it stops signing for a
//! joint it has not heard of lately, and without a certificate the real mode blocks.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::bridge::TransportError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StateSource {
    /// What the bridge ordered and the controller accepted.
    Commanded,
    /// What the controller read back from the joint.
    Measured,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct JointStateReport {
    pub joint: String,
    pub position: f64,
    pub seq: u64,
    pub stamp_ms: u64,
    pub source: StateSource,
}

/// What happened to the report of one command.
#[derive(Debug, Clone, PartialEq)]
pub enum MirrorOutcome {
    /// No twin link is configured: nothing was attempted.
    NotConfigured,
    Delivered,
    Failed {
        reason: String,
    },
}

impl MirrorOutcome {
    pub fn kind(&self) -> &'static str {
        match self {
            MirrorOutcome::NotConfigured => "not_configured",
            MirrorOutcome::Delivered => "delivered",
            MirrorOutcome::Failed { .. } => "failed",
        }
    }
}

/// Makes the `seq` of the reports: one counter per joint that never goes back, not even across a restart of the bridge.
#[derive(Debug, Default)]
pub struct StateSequencer {
    last: HashMap<String, u64>,
}

impl StateSequencer {
    /// The next `seq` of `joint`: the clock in milliseconds, or the last one plus one when the clock did not advance.
    pub fn next(&mut self, joint: &str, now_ms: u64) -> u64 {
        let last = self.last.get(joint).copied().unwrap_or(0);
        let seq = now_ms.max(last + 1);
        self.last.insert(joint.to_string(), seq);
        seq
    }
}

/// Anything that can tell the twin where a joint is.
pub trait TwinStateSink: Send + Sync + std::fmt::Debug {
    fn report(&self, report: &JointStateReport) -> Result<(), TransportError>;
}

/// Keeps the reports in memory: the stand-in for a link in tests and the answer of a simulated link that is down.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct RecordingStateSink {
    pub received: std::sync::Mutex<Vec<JointStateReport>>,
    pub connected: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl RecordingStateSink {
    pub fn connected() -> Self {
        Self {
            received: Default::default(),
            connected: std::sync::atomic::AtomicBool::new(true),
        }
    }

    pub fn disconnected() -> Self {
        Self::default()
    }

    pub fn reports(&self) -> Vec<JointStateReport> {
        self.received
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

#[cfg(test)]
impl TwinStateSink for RecordingStateSink {
    fn report(&self, report: &JointStateReport) -> Result<(), TransportError> {
        if !self.connected.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(TransportError::Disconnected);
        }
        self.received
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(report.clone());
        Ok(())
    }
}

/// `POST /interlock/state` of HYDRA-UMC-TWIN over plain HTTP (the twin runs on the same machine or the same wired network; no TLS here).
#[derive(Debug, Clone)]
pub struct HttpTwinSink {
    /// `host:port`
    pub addr: String,
    pub token: String,
    pub timeout: Duration,
}

impl HttpTwinSink {
    /// `http://host:port` (no path, no other scheme) to `host:port`.
    pub fn parse_url(url: &str) -> Result<String, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or("the twin URL must start with http://")?;
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() || rest.contains('/') || !rest.contains(':') {
            return Err("the twin URL must be http://HOST:PORT".to_string());
        }
        Ok(rest.to_string())
    }
}

/// A status line reduced to what the bridge needs: 200 is delivered, 409 means the twin already has a newer report of the joint (a report
/// that overtook this one), anything else is a refusal.
fn status_to_result(status: u16) -> Result<(), TransportError> {
    match status {
        200 | 409 => Ok(()),
        other => Err(TransportError::Rejected { status: other }),
    }
}

impl TwinStateSink for HttpTwinSink {
    fn report(&self, report: &JointStateReport) -> Result<(), TransportError> {
        let timeout_ms = self.timeout.as_millis() as u64;
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
        let body = serde_json::to_string(report).expect("a report always serializes");
        let request = format!(
            "POST /interlock/state HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.addr,
            self.token,
            body.len(),
            body
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|_| TransportError::Timeout {
                after_ms: timeout_ms,
            })?;
        let mut head = Vec::new();
        let mut chunk = [0u8; 256];
        // only the status line is needed
        while !head.contains(&b'\n') && head.len() < 1024 {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => head.extend_from_slice(&chunk[..n]),
                Err(_) => {
                    return Err(TransportError::Timeout {
                        after_ms: timeout_ms,
                    })
                }
            }
        }
        let line = String::from_utf8_lossy(&head);
        let status: u16 = line
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or(TransportError::Disconnected)?;
        status_to_result(status)
    }
}

/// Sends `report` and says what happened.
pub fn deliver(sink: Option<&dyn TwinStateSink>, report: &JointStateReport) -> MirrorOutcome {
    match sink {
        None => MirrorOutcome::NotConfigured,
        Some(sink) => match sink.report(report) {
            Ok(()) => MirrorOutcome::Delivered,
            Err(e) => MirrorOutcome::Failed {
                reason: e.to_string(),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(seq: u64) -> JointStateReport {
        JointStateReport {
            joint: "shoulder".to_string(),
            position: 0.5,
            seq,
            stamp_ms: 1_700_000_000_000,
            source: StateSource::Measured,
        }
    }

    #[test]
    fn the_shape_on_the_wire_is_the_one_the_twin_expects() {
        // The same text is checked in HYDRA-UMC-TWIN: the two repositories agree on it.
        let wire = r#"{"joint":"shoulder","position":0.5,"seq":7,"stamp_ms":1700000000000,"source":"measured"}"#;
        let mut r = report(7);
        assert_eq!(serde_json::to_string(&r).unwrap(), wire);
        assert_eq!(serde_json::from_str::<JointStateReport>(wire).unwrap(), r);
        r.source = StateSource::Commanded;
        assert!(serde_json::to_string(&r)
            .unwrap()
            .ends_with(r#""source":"commanded"}"#));
    }

    #[test]
    fn the_sequence_never_goes_back_and_each_joint_has_its_own() {
        let mut seq = StateSequencer::default();
        assert_eq!(seq.next("a", 1000), 1000);
        assert_eq!(seq.next("a", 1000), 1001);
        assert_eq!(seq.next("a", 900), 1002); // the clock went back
        assert_eq!(seq.next("a", 5000), 5000);
        assert_eq!(seq.next("b", 10), 10);
    }

    #[test]
    fn a_connected_sink_keeps_the_reports_in_order_and_a_disconnected_one_fails() {
        let sink = RecordingStateSink::connected();
        assert_eq!(deliver(Some(&sink), &report(1)), MirrorOutcome::Delivered);
        assert_eq!(deliver(Some(&sink), &report(2)), MirrorOutcome::Delivered);
        assert_eq!(sink.reports(), vec![report(1), report(2)]);
        let down = RecordingStateSink::disconnected();
        let outcome = deliver(Some(&down), &report(3));
        assert_eq!(
            outcome,
            MirrorOutcome::Failed {
                reason: "transport is disconnected".to_string()
            }
        );
        assert_eq!(outcome.kind(), "failed");
        assert!(down.reports().is_empty());
        assert_eq!(deliver(None, &report(4)), MirrorOutcome::NotConfigured);
    }

    #[test]
    fn the_status_of_the_twin_maps_to_delivered_superseded_or_refused() {
        assert_eq!(status_to_result(200), Ok(()));
        // 409: the twin holds a newer report of the joint already
        assert_eq!(status_to_result(409), Ok(()));
        assert_eq!(
            status_to_result(401),
            Err(TransportError::Rejected { status: 401 })
        );
        assert_eq!(
            status_to_result(404),
            Err(TransportError::Rejected { status: 404 })
        );
    }

    #[test]
    fn the_twin_url_is_http_host_and_port_only() {
        assert_eq!(
            HttpTwinSink::parse_url("http://127.0.0.1:18400").unwrap(),
            "127.0.0.1:18400"
        );
        assert_eq!(
            HttpTwinSink::parse_url("http://twin.local:80/").unwrap(),
            "twin.local:80"
        );
        for bad in [
            "https://127.0.0.1:1",
            "127.0.0.1:1",
            "http://",
            "http://host",
            "http://host:1/path",
        ] {
            assert!(HttpTwinSink::parse_url(bad).is_err(), "{bad}");
        }
    }

    /// A tiny server that answers every request with one canned status, and keeps the request it got.
    fn canned_server(status_line: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = vec![0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
                let _ = stream.write_all(
                    format!("HTTP/1.1 {status_line}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}")
                        .as_bytes(),
                );
            }
        });
        (addr, rx)
    }

    fn http_sink(addr: String) -> HttpTwinSink {
        HttpTwinSink {
            addr,
            token: "a-certify-token-of-sixteen+".to_string(),
            timeout: Duration::from_millis(1500),
        }
    }

    #[test]
    fn the_http_sink_posts_the_report_with_the_token_and_reads_the_status() {
        let (addr, rx) = canned_server("200 OK");
        assert_eq!(http_sink(addr).report(&report(9)), Ok(()));
        let sent = rx.recv().unwrap();
        assert!(sent.starts_with("POST /interlock/state HTTP/1.1"));
        assert!(sent.contains("Authorization: Bearer a-certify-token-of-sixteen+"));
        assert!(sent.contains("Content-Type: application/json"));
        assert!(sent.contains(r#""seq":9"#));
    }

    #[test]
    fn the_http_sink_reports_a_refusal_and_a_dead_port() {
        let (addr, _rx) = canned_server("401 Unauthorized");
        assert_eq!(
            http_sink(addr).report(&report(1)),
            Err(TransportError::Rejected { status: 401 })
        );
        let (addr, _rx) = canned_server("409 Conflict");
        assert_eq!(http_sink(addr).report(&report(1)), Ok(()));
        // a port nobody listens on
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = free.local_addr().unwrap().to_string();
        drop(free);
        assert_eq!(
            http_sink(dead).report(&report(1)),
            Err(TransportError::Disconnected)
        );
    }

    #[test]
    fn a_twin_that_never_answers_is_a_timeout() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let sink = HttpTwinSink {
            addr,
            token: "a-certify-token-of-sixteen+".to_string(),
            timeout: Duration::from_millis(150),
        };
        // the listener accepts at the OS level (backlog) but nobody reads or answers
        assert_eq!(
            sink.report(&report(1)),
            Err(TransportError::Timeout { after_ms: 150 })
        );
        drop(listener);
    }
}
