// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/session_record.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! Recording and replay of a hardware-in-the-loop session in the data lake.
//!
//! With `--record-url` (off by default) every command the bridge decides, and every position the controller reads back, is sent to
//! HYDRA-UMC-DATALAKE (`POST /ingest/batch`) as the samples of one session, so a session can be kept for regression and for the generator of
//! synthetic data:
//!
//! * `sourceId` is `hil-<session>`; `kind` is `hil_command` or `hil_measured`.
//! * a command is three fields of one sample: `cmd.<joint>` (the position), `out.<joint>` (what happened to it, see [`outcome_code`]) and
//!   `real.<joint>` (1 for the real mode, 0 for the simulation). A measured position is `pos.<joint>`.
//! * every sample of a session has its own millisecond: when two events fall in the same one the later is dated one millisecond after the
//!   previous, so the order of the session is the order of the timestamps and nothing replaces anything.
//!
//! Recording never decides anything and never waits for the lake: an event goes to a bounded queue and a writer thread sends it in batches.
//! A full queue drops the event (counted) and a batch the lake refuses is dropped (counted); neither changes what happened to a command.
//! No certificate, token or address is ever recorded.
//!
//! Replay (`POST /replay`, only with `--replay`) reads a session back and runs its commands again in simulation mode, never in real mode,
//! and compares them with the limits of the URDF the bridge has now (see `server.rs`).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::bridge::RouteOutcome;
use crate::http_client;

/// How many events wait for the writer at most.
pub const QUEUE_CAPACITY: usize = 4096;
/// How many samples go in one request.
const BATCH_SIZE: usize = 100;
/// How long the writer waits for more events before it sends what it has.
const BATCH_WAIT_MS: u64 = 200;
const MAX_ANSWER_BYTES: u64 = 8 * 1024 * 1024;

/// The code of what happened to a command, as recorded in `out.<joint>`.
pub fn outcome_code(outcome: &RouteOutcome) -> u8 {
    match outcome {
        RouteOutcome::SentReal => 0,
        RouteOutcome::SentSimulation => 1,
        RouteOutcome::BlockedByInterlock { .. } => 2,
        RouteOutcome::TransportFailure { .. } => 3,
    }
}

/// A real command refused before it was routed (the twin predicted a contact, the zones were not clear).
pub const OUTCOME_REFUSED_BEFORE_ROUTING: u8 = 4;

pub fn outcome_name(code: u8) -> &'static str {
    match code {
        0 => "sent_real",
        1 => "sent_simulation",
        2 => "blocked_by_interlock",
        3 => "transport_failure",
        4 => "refused_before_routing",
        _ => "unknown",
    }
}

/// A session name is used in a query: letters, digits, `.`, `_` and `-`, up to 64 characters.
pub fn valid_session(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// One thing that happened, before it has a time of its own.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Command {
        joint: String,
        position: f64,
        real: bool,
        outcome: u8,
    },
    Measured {
        joint: String,
        position: f64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub kind: &'static str,
    pub at_ms: u64,
    pub fields: Vec<(String, f64)>,
}

impl Event {
    fn into_sample(self, at_ms: u64) -> Sample {
        match self {
            Event::Command {
                joint,
                position,
                real,
                outcome,
            } => Sample {
                kind: "hil_command",
                at_ms,
                fields: vec![
                    (format!("cmd.{joint}"), position),
                    (format!("out.{joint}"), f64::from(outcome)),
                    (format!("real.{joint}"), if real { 1.0 } else { 0.0 }),
                ],
            },
            Event::Measured { joint, position } => Sample {
                kind: "hil_measured",
                at_ms,
                fields: vec![(format!("pos.{joint}"), position)],
            },
        }
    }
}

/// Where the lake is.
#[derive(Debug, Clone)]
pub struct Lake {
    /// `host:port`
    pub addr: String,
    pub token: Option<String>,
    pub timeout: Duration,
}

/// What the recorder counts (shown in `GET /stats` and `GET /metrics`).
#[derive(Debug, Default)]
pub struct RecorderCounters {
    /// Samples the lake accepted.
    pub recorded: AtomicU64,
    /// Events dropped because the queue was full.
    pub dropped: AtomicU64,
    /// Samples in a batch the lake did not accept.
    pub failed: AtomicU64,
}

#[derive(Debug)]
pub struct Recorder {
    session: String,
    lake: Lake,
    sender: SyncSender<(Event, u64)>,
    last_ms: Mutex<u64>,
    pub counters: Arc<RecorderCounters>,
    /// `--replay`: `POST /replay` may read the session back.
    pub replay: bool,
}

pub fn source_id(session: &str) -> String {
    format!("hil-{session}")
}

impl Recorder {
    /// Starts the writer thread. The thread ends when the recorder is dropped.
    pub fn start(session: &str, lake: Lake, replay: bool) -> Result<Recorder, String> {
        if !valid_session(session) {
            return Err(
                "the session name may only have letters, digits, '.', '_' and '-' (up to 64)"
                    .to_string(),
            );
        }
        let (sender, receiver) = sync_channel::<(Event, u64)>(QUEUE_CAPACITY);
        let counters = Arc::new(RecorderCounters::default());
        let writer_counters = counters.clone();
        let writer_lake = lake.clone();
        let source = source_id(session);
        std::thread::Builder::new()
            .name("hil-session-writer".to_string())
            .spawn(move || {
                let mut batch: Vec<Sample> = Vec::new();
                loop {
                    match receiver.recv_timeout(Duration::from_millis(BATCH_WAIT_MS)) {
                        Ok((event, at_ms)) => {
                            batch.push(event.into_sample(at_ms));
                            if batch.len() < BATCH_SIZE {
                                continue;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => {
                            send_batch(&writer_lake, &source, &mut batch, &writer_counters);
                            return;
                        }
                    }
                    send_batch(&writer_lake, &source, &mut batch, &writer_counters);
                }
            })
            .map_err(|e| format!("cannot start the session writer: {e}"))?;
        Ok(Recorder {
            session: session.to_string(),
            lake,
            sender,
            last_ms: Mutex::new(0),
            counters,
            replay,
        })
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    pub fn lake(&self) -> &Lake {
        &self.lake
    }

    /// Queues one event with its own millisecond. Never blocks and never fails the caller.
    pub fn record(&self, event: Event, now_ms: u64) {
        let at_ms = {
            let mut last = self.last_ms.lock().unwrap_or_else(|p| p.into_inner());
            let at = now_ms.max(last.saturating_add(1));
            *last = at;
            at
        };
        match self.sender.try_send((event, at_ms)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

fn send_batch(lake: &Lake, source: &str, batch: &mut Vec<Sample>, counters: &RecorderCounters) {
    if batch.is_empty() {
        return;
    }
    let samples: Vec<Value> = batch
        .iter()
        .map(|sample| {
            let fields: serde_json::Map<String, Value> = sample
                .fields
                .iter()
                .map(|(name, value)| (name.clone(), json!(value)))
                .collect();
            json!({
                "sourceId": source,
                "kind": sample.kind,
                "timestamp": sample.at_ms,
                "fields": fields,
            })
        })
        .collect();
    let body = json!({ "samples": samples }).to_string();
    let accepted = matches!(
        http_client::request(
            &lake.addr,
            "POST",
            "/ingest/batch",
            lake.token.as_deref(),
            Some(&body),
            lake.timeout,
            64 * 1024,
        ),
        Ok(answer) if answer.status == 202 || answer.status == 200
    );
    let count = batch.len() as u64;
    if accepted {
        counters.recorded.fetch_add(count, Ordering::Relaxed);
    } else {
        counters.failed.fetch_add(count, Ordering::Relaxed);
        eprintln!("[hil-bridge] session record: the data lake did not accept {count} sample(s); they are dropped");
    }
    batch.clear();
}

/// What is known of one command while its fields are read: the joint and position, the outcome code and the mode.
type Partial = (Option<(String, f64)>, Option<u8>, bool);

/// One command read back from a session.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedCommand {
    pub at_ms: u64,
    pub joint: String,
    pub position: f64,
    pub real: bool,
    pub outcome: Option<u8>,
}

/// Reads the commands of a session back, oldest first. `max_points` bounds the points read from the lake (a session larger than that is an
/// error, not a silently cut one).
pub fn fetch_commands(
    lake: &Lake,
    session: &str,
    from_ms: Option<u64>,
    to_ms: Option<u64>,
    max_points: usize,
) -> Result<Vec<RecordedCommand>, String> {
    if !valid_session(session) {
        return Err("the session name may only have letters, digits, '.', '_' and '-'".to_string());
    }
    let page = 5_000usize.min(max_points.max(1));
    let mut by_time: BTreeMap<u64, Partial> = BTreeMap::new();
    let mut cursor: Option<String> = None;
    let mut read = 0usize;
    loop {
        let mut path = format!(
            "/query?sourceId={}&kind=hil_command&limit={page}",
            http_client::encode_query_value(&source_id(session))
        );
        if let Some(from) = from_ms {
            path.push_str(&format!("&start={from}"));
        }
        if let Some(to) = to_ms {
            path.push_str(&format!("&end={to}"));
        }
        if let Some(cursor) = &cursor {
            path.push_str(&format!(
                "&cursor={}",
                http_client::encode_query_value(cursor)
            ));
        }
        let answer = http_client::request(
            &lake.addr,
            "GET",
            &path,
            lake.token.as_deref(),
            None,
            lake.timeout,
            MAX_ANSWER_BYTES,
        )
        .map_err(|e| format!("the data lake could not be read: {e}"))?;
        if answer.status != 200 {
            return Err(format!(
                "the data lake refused the query with status {}",
                answer.status
            ));
        }
        let points: Vec<Value> = serde_json::from_str(&answer.body)
            .map_err(|_| "the data lake gave an answer that is not a list of points".to_string())?;
        read += points.len();
        if read > max_points {
            return Err(format!(
                "the session has more than {max_points} points; narrow it with from_ms/to_ms or raise --replay-max-points"
            ));
        }
        for point in &points {
            let (Some(field), Some(at), Some(value)) = (
                point.get("field").and_then(Value::as_str),
                point.get("timestamp").and_then(Value::as_u64),
                point.get("value").and_then(Value::as_f64),
            ) else {
                continue;
            };
            let entry = by_time.entry(at).or_insert((None, None, false));
            if let Some(joint) = field.strip_prefix("cmd.") {
                entry.0 = Some((joint.to_string(), value));
            } else if field.starts_with("out.") {
                entry.1 = Some(value as u8);
            } else if field.starts_with("real.") {
                entry.2 = value >= 0.5;
            }
        }
        match answer.next_cursor {
            Some(next) if !points.is_empty() => cursor = Some(next),
            _ => break,
        }
    }
    Ok(by_time
        .into_iter()
        .filter_map(|(at_ms, (command, outcome, real))| {
            command.map(|(joint, position)| RecordedCommand {
                at_ms,
                joint,
                position,
                real,
                outcome,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_client::tests::canned_peer;

    fn lake(addr: &str) -> Lake {
        Lake {
            addr: addr.to_string(),
            token: Some("lake-token-of-16-chars".to_string()),
            timeout: Duration::from_secs(2),
        }
    }

    fn wait_for(mut done: impl FnMut() -> bool) -> bool {
        for _ in 0..100 {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn a_session_name_is_safe_for_a_query() {
        assert!(valid_session("bench-1.a_b"));
        for bad in ["", "a b", "a/b", "a&b", "ñ", &"x".repeat(65)] {
            assert!(!valid_session(bad), "{bad}");
        }
        assert!(Recorder::start("a b", lake("127.0.0.1:1"), false).is_err());
    }

    #[test]
    fn every_outcome_has_its_own_code_and_name() {
        let outcomes = [
            RouteOutcome::SentReal,
            RouteOutcome::SentSimulation,
            RouteOutcome::BlockedByInterlock { reason: "x".into() },
            RouteOutcome::TransportFailure { reason: "x".into() },
        ];
        let codes: Vec<u8> = outcomes.iter().map(outcome_code).collect();
        assert_eq!(codes, vec![0, 1, 2, 3]);
        assert_eq!(outcome_name(2), "blocked_by_interlock");
        assert_eq!(
            outcome_name(OUTCOME_REFUSED_BEFORE_ROUTING),
            "refused_before_routing"
        );
        assert_eq!(outcome_name(99), "unknown");
    }

    #[test]
    fn events_in_the_same_millisecond_get_their_own_one() {
        let peer = canned_peer(vec![(202, String::new(), r#"{"written":3}"#.to_string())]);
        let recorder = Recorder::start("s1", lake(&peer.addr), false).unwrap();
        for position in [0.1, 0.2, 0.3] {
            recorder.record(
                Event::Command {
                    joint: "j1".to_string(),
                    position,
                    real: true,
                    outcome: 0,
                },
                1_000,
            );
        }
        assert!(wait_for(|| recorder
            .counters
            .recorded
            .load(Ordering::Relaxed)
            == 3));
        let requests = peer.requests.lock().unwrap().clone();
        let body = requests[0].split_once("\r\n\r\n").unwrap().1;
        let value: Value = serde_json::from_str(body).unwrap();
        let samples = value["samples"].as_array().unwrap();
        let times: Vec<u64> = samples
            .iter()
            .map(|s| s["timestamp"].as_u64().unwrap())
            .collect();
        assert_eq!(times, vec![1_000, 1_001, 1_002]);
        assert_eq!(samples[0]["sourceId"], "hil-s1");
        assert_eq!(samples[0]["kind"], "hil_command");
        assert_eq!(samples[0]["fields"]["cmd.j1"], 0.1);
        assert_eq!(samples[0]["fields"]["out.j1"], 0.0);
        assert_eq!(samples[0]["fields"]["real.j1"], 1.0);
        assert!(requests[0].contains("Authorization: Bearer lake-token-of-16-chars"));
        // no certificate, token or address can be in the samples: the event has no field for them
        assert!(!body.contains("certificate"));
    }

    #[test]
    fn a_lake_that_refuses_is_counted_and_never_blocks_the_caller() {
        let peer = canned_peer(vec![(503, String::new(), "{}".to_string())]);
        let recorder = Recorder::start("s2", lake(&peer.addr), false).unwrap();
        let started = std::time::Instant::now();
        recorder.record(
            Event::Measured {
                joint: "j1".to_string(),
                position: 0.5,
            },
            5,
        );
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(wait_for(|| recorder
            .counters
            .failed
            .load(Ordering::Relaxed)
            == 1));
        assert_eq!(recorder.counters.recorded.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_full_queue_drops_events_and_counts_them() {
        // a lake that nobody listens to: the writer is slow to give up, the queue fills
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = closed.local_addr().unwrap().to_string();
        drop(closed);
        let recorder = Recorder::start("s3", lake(&addr), false).unwrap();
        let (sender, _receiver) = sync_channel::<(Event, u64)>(1);
        let full = Recorder {
            session: "s3".into(),
            lake: lake(&addr),
            sender,
            last_ms: Mutex::new(0),
            counters: Arc::new(RecorderCounters::default()),
            replay: false,
        };
        for _ in 0..5 {
            full.record(
                Event::Measured {
                    joint: "j".into(),
                    position: 0.0,
                },
                1,
            );
        }
        assert_eq!(full.counters.dropped.load(Ordering::Relaxed), 4);
        drop(recorder);
    }

    #[test]
    fn a_session_is_read_back_in_order_with_its_outcomes() {
        let page1 = json!([
            {"sourceId":"hil-s4","kind":"hil_command","field":"cmd.j1","timestamp":10,"value":0.5},
            {"sourceId":"hil-s4","kind":"hil_command","field":"out.j1","timestamp":10,"value":0.0},
            {"sourceId":"hil-s4","kind":"hil_command","field":"real.j1","timestamp":10,"value":1.0},
        ])
        .to_string();
        let page2 = json!([
            {"sourceId":"hil-s4","kind":"hil_command","field":"cmd.j2","timestamp":11,"value":-1.0},
            {"sourceId":"hil-s4","kind":"hil_command","field":"out.j2","timestamp":11,"value":2.0},
            {"sourceId":"hil-s4","kind":"hil_command","field":"real.j2","timestamp":11,"value":1.0},
            {"sourceId":"hil-s4","kind":"hil_command","field":"cmd.j1","timestamp":12,"value":0.6},
            {"sourceId":"hil-s4","kind":"hil_command","field":"out.j1","timestamp":12,"value":1.0},
            {"sourceId":"hil-s4","kind":"hil_command","field":"real.j1","timestamp":12,"value":0.0},
        ])
        .to_string();
        let peer = canned_peer(vec![
            (200, "X-Next-Cursor: page-two\r\n".to_string(), page1),
            (200, String::new(), page2),
        ]);
        let commands = fetch_commands(&lake(&peer.addr), "s4", Some(5), Some(100), 100).unwrap();
        assert_eq!(commands.len(), 3);
        assert_eq!(
            commands[0],
            RecordedCommand {
                at_ms: 10,
                joint: "j1".into(),
                position: 0.5,
                real: true,
                outcome: Some(0)
            }
        );
        assert_eq!(commands[1].outcome, Some(2));
        assert!(!commands[2].real);
        let requests = peer.requests.lock().unwrap().clone();
        assert!(requests[0].contains("sourceId=hil-s4&kind=hil_command"));
        assert!(requests[0].contains("&start=5&end=100"));
        assert!(requests[1].contains("&cursor=page-two"));
    }

    #[test]
    fn a_session_bigger_than_the_bound_or_a_lake_that_fails_is_an_error() {
        let big = json!([
            {"field":"cmd.j1","timestamp":1,"value":0.1},
            {"field":"out.j1","timestamp":1,"value":0.0},
            {"field":"real.j1","timestamp":1,"value":1.0},
        ])
        .to_string();
        let peer = canned_peer(vec![(200, String::new(), big)]);
        let err = fetch_commands(&lake(&peer.addr), "s5", None, None, 2).unwrap_err();
        assert!(err.contains("more than 2 points"));
        let refusing = canned_peer(vec![(401, String::new(), "{}".to_string())]);
        assert!(fetch_commands(&lake(&refusing.addr), "s5", None, None, 10).is_err());
        assert!(fetch_commands(&lake(&refusing.addr), "bad name", None, None, 10).is_err());
    }
}
