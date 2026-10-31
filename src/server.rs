// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/server.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! Plain JSON/HTTP surface (`tiny_http`, blocking, no async runtime) -
//! same convention as `HYDRA-UMC-TWIN`'s and `HYDRA-UMC-SWARM-SYNC`'s
//! own `server.rs`. POST /route and POST /mirror reach the exact same
//! `Bridge::route_command()`/`Bridge::mirror_command()` the CLI's own
//! `route`/`mirror` subcommands already run - real transport still
//! isn't wired in (see `Cargo.toml`), so both routes still only ever
//! reach `RecordingSink`/`SimulatedTransport`, the same honest fakes the
//! CLI already uses; this closes the "only reachable as a one-shot CLI"
//! gap, not the still-deferred real gRPC/WebSocket transport question.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tiny_http::{Header, Method, Response, Server};

use crate::access::bearer_matches;
use crate::audit::{caller_label, Audit, AuditEntry};
use crate::bridge::{Bridge, CommandSink, RecordingSink, RouteOutcome, SimulatedTransport};
use crate::certificate::{verify, Certificate, CertificatePolicy, NonceCache, TrustedKeys};
use crate::interlock::TwinRiskReport;
use crate::limits::JointLimits;
use crate::protocol::{JointCommand, Mode};
use crate::twin_link::{
    deliver, JointStateReport, MirrorOutcome, StateSequencer, StateSource, TwinStateSink,
};
use crate::watchdog::{Leases, Validity, WatchdogConfig};

fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()
}

fn header(name: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes())
        .expect("static header names and values are valid")
}

/// The largest request body this surface reads. Every request here is a few hundred bytes of JSON; reading without a limit let anything that could reach the port make the process
/// allocate as much memory as it was sent.
const MAX_BODY_BYTES: u64 = 16 * 1024;

/// Why a request body could not be used, so each cause gets its own status.
enum BodyError {
    TooLarge,
    NotUtf8,
    Io(std::io::Error),
}

fn read_body(request: &mut tiny_http::Request) -> Result<String, BodyError> {
    use std::io::Read;
    // one byte more than the limit, so "exactly the limit" is accepted and "over" is detected
    let mut raw = Vec::new();
    request
        .as_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(BodyError::Io)?;
    if raw.len() as u64 > MAX_BODY_BYTES {
        return Err(BodyError::TooLarge);
    }
    String::from_utf8(raw).map_err(|_| BodyError::NotUtf8)
}

/// A browser can send a "simple" cross-site POST (`text/plain`, no CORS
/// preflight) to a loopback port, so a body is only accepted when it is
/// declared as JSON and the request does not come from a web page (a
/// browser always adds `Origin` to a cross-site POST; the CLI tools and
/// local services that use this API do not).
fn check_request_origin(request: &tiny_http::Request) -> Result<(), (u16, &'static str)> {
    let mut json_content_type = false;
    for h in request.headers() {
        let name = h.field.as_str().as_str();
        if name.eq_ignore_ascii_case("origin") {
            return Err((403, "requests carrying an Origin header are refused"));
        }
        if name.eq_ignore_ascii_case("content-type") {
            let value = h.value.as_str().to_ascii_lowercase();
            let media_type = value.split(';').next().unwrap_or("").trim().to_string();
            json_content_type = media_type == "application/json";
        }
    }
    if json_content_type {
        Ok(())
    } else {
        Err((415, "Content-Type must be application/json"))
    }
}

#[derive(Deserialize, Default)]
struct TransportConfig {
    #[serde(default)]
    disconnected: bool,
    latency_ms: Option<u64>,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct RouteRequest {
    mode: Mode,
    joint: String,
    position: f64,
    /// The risk report written by the caller itself. In real mode it is only read when the bridge was started with `--allow-unsigned-risk`
    /// (a bench setting); the real mode asks for `certificate`. Simulation mode never gates on either.
    risk: Option<TwinRiskReport>,
    /// The risk report signed by HYDRA-UMC-TWIN for exactly this command (see `certificate.rs`).
    certificate: Option<Certificate>,
    #[serde(default)]
    transport: TransportConfig,
    /// The validity window of the command (see `watchdog.rs`): when it was issued, how long it is good for, and its sequence number.
    issued_at_ms: Option<u64>,
    valid_for_ms: Option<u64>,
    seq: Option<u64>,
}

impl RouteRequest {
    /// The validity window, when the request carries one. Half a window is not a window.
    fn validity(&self) -> Result<Option<Validity>, String> {
        match (self.issued_at_ms, self.valid_for_ms) {
            (Some(issued_at_ms), Some(valid_for_ms)) => Ok(Some(Validity {
                issued_at_ms,
                valid_for_ms,
                seq: self.seq,
            })),
            (None, None) if self.seq.is_none() => Ok(None),
            _ => Err("issued_at_ms and valid_for_ms go together (seq needs both)".to_string()),
        }
    }
}

#[derive(Deserialize)]
struct MirrorRequest {
    joint: String,
    position: f64,
}

/// Real process-lifetime counters. They live in the `State` the workers share behind one lock, so every increment and the `/stats` read
/// that reports them happen in sequence.
#[derive(Default)]
struct Stats {
    routed_real: u64,
    routed_simulation: u64,
    blocked_by_interlock: u64,
    transport_failures: u64,
    mirrored: u64,
    mirror_failures: u64,
    malformed_requests: u64,
    not_found: u64,
    unauthorized: u64,
    uncertified: u64,
    audit_failures: u64,
    stale_commands: u64,
    outside_limits: u64,
    watchdog_stops: u64,
    watchdog_stop_failures: u64,
    twin_reports: u64,
    twin_report_failures: u64,
    /// Whether the last report to the twin arrived (None: none was tried yet or no twin is configured).
    twin_in_sync: Option<bool>,
}

/// How the interlock of the real mode is certified. The default trusts no twin and accepts no unsigned report: a real command is blocked
/// until a public key is configured and the command comes with a certificate made for it.
#[derive(Debug, Default, Clone)]
pub struct InterlockConfig {
    pub trusted_keys: TrustedKeys,
    pub policy: CertificatePolicy,
    /// The bench setting (`--allow-unsigned-risk`): believe the `risk` the caller wrote. Off unless asked for, and announced at every start.
    pub allow_unsigned_risk: bool,
}

/// Everything `run_with_config` needs besides the socket.
#[derive(Debug, Default, Clone)]
pub struct ServerConfig {
    pub fault_injection: bool,
    pub token: Option<String>,
    pub interlock: InterlockConfig,
    /// Where the audit records of the real orders are appended (None: only the last ones are kept, in memory).
    pub audit_log: Option<std::path::PathBuf>,
    /// The lease of the real commands (see `watchdog.rs`).
    pub watchdog: WatchdogConfig,
    /// The joints of the robot and their ranges (`--urdf`): a real command outside them is blocked. None: only the shape of the command is checked.
    pub limits: Option<JointLimits>,
    /// How many requests are served at once (0: `DEFAULT_WORKERS`).
    pub workers: usize,
    /// Where the position of the real joints is reported (`--twin-url`). None: the twin is not told.
    pub twin: Option<std::sync::Arc<dyn TwinStateSink>>,
}

/// What the bridge itself knows about who sent a request.
struct RequestInfo {
    caller: String,
    peer: String,
    authenticated: bool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn header_value(request: &tiny_http::Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

pub fn bind(addr: &str) -> std::io::Result<Server> {
    Server::http(addr).map_err(std::io::Error::other)
}

/// `fault_injection` lets a request pick a simulated transport (disconnected
/// link, latency, timeout). It is a test facility: without it the field is
/// refused, so a deployed server cannot be told to fake a transport fault.
#[cfg(test)]
pub fn run(server: Server, fault_injection: bool) {
    run_with_config(
        server,
        ServerConfig {
            fault_injection,
            interlock: InterlockConfig {
                allow_unsigned_risk: true,
                ..InterlockConfig::default()
            },
            ..ServerConfig::default()
        },
    );
}

/// Like `run`, with an optional Bearer token: when set, every request except `GET /stats` (the health path of the manifest) must
/// carry it. The real mode is then certified by default: see `InterlockConfig`.
#[cfg(test)]
pub fn run_with_token(server: Server, fault_injection: bool, token: Option<String>) {
    run_with_config(
        server,
        ServerConfig {
            fault_injection,
            token,
            interlock: InterlockConfig {
                allow_unsigned_risk: true,
                ..InterlockConfig::default()
            },
            ..ServerConfig::default()
        },
    );
}

/// How many requests are served at once (default 8). A request that is slow (a client that sends its body one byte a minute) holds one worker, not the whole bridge.
pub const DEFAULT_WORKERS: usize = 8;

/// What the decisions share. The real orders are decided one at a time, in the order they get the lock, so the lease of a joint, the
/// nonces of the certificates and the audit log never see two orders at once; reading a request and answering it happen outside the lock.
struct State {
    nonces: NonceCache,
    audit: Audit,
    stats: Stats,
    leases: Leases,
    hold_sink: RecordingSink,
    twin_seq: StateSequencer,
    /// The report of a real command that was just delivered, made under the lock and sent after it is released.
    pending_twin: Option<JointStateReport>,
}

struct Shared {
    config: ServerConfig,
    state: Mutex<State>,
}

impl Shared {
    /// A worker that panicked while holding the lock must not stop the watchdog or the other workers: the state is taken as it was left.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// The answer to a request, built under the lock and sent after it is released.
struct Reply {
    status: u16,
    body: serde_json::Value,
    headers: Vec<(&'static str, String)>,
}

impl Reply {
    fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            body,
            headers: Vec::new(),
        }
    }
    fn with(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
    fn send(self, request: tiny_http::Request) {
        let mut response = Response::from_string(self.body.to_string())
            .with_status_code(self.status)
            .with_header(json_header());
        for (name, value) in self.headers {
            response = response.with_header(header(name, &value));
        }
        let _ = request.respond(response);
    }
}

/// Serve with the whole configuration: the token, the way the real mode is certified and the audit log.
pub fn run_with_config(server: Server, config: ServerConfig) {
    let workers = if config.workers == 0 {
        DEFAULT_WORKERS
    } else {
        config.workers
    };
    let state = State {
        nonces: NonceCache::default(),
        audit: Audit::new(config.audit_log.clone()),
        stats: Stats::default(),
        leases: Leases::new(config.watchdog),
        hold_sink: RecordingSink::default(),
        twin_seq: StateSequencer::default(),
        pending_twin: None,
    };
    let shared = Arc::new(Shared {
        config,
        state: Mutex::new(state),
    });
    let server = Arc::new(server);
    let mut handles = Vec::new();
    for _ in 1..workers {
        let (server, shared) = (server.clone(), shared.clone());
        handles.push(std::thread::spawn(move || worker_loop(&server, &shared)));
    }
    worker_loop(&server, &shared);
    for handle in handles {
        let _ = handle.join();
    }
}

fn worker_loop(server: &Server, shared: &Shared) {
    loop {
        // The watchdog runs between requests, in every worker: a joint whose lease ran out is stopped even when nothing arrives any more.
        {
            let mut state = shared.lock();
            let State {
                leases,
                hold_sink,
                audit,
                stats,
                ..
            } = &mut *state;
            stop_lapsed_leases(leases, hold_sink, audit, stats);
        }
        let request = match server.recv_timeout(Duration::from_millis(WATCHDOG_TICK_MS)) {
            Ok(Some(request)) => request,
            Ok(None) => continue,
            Err(_) => break,
        };
        handle_request(request, shared);
    }
}

fn handle_request(mut request: tiny_http::Request, shared: &Shared) {
    let config = &shared.config;
    let path = request.url().split('?').next().unwrap_or("").to_string();
    let mut authenticated = false;

    if let Some(expected) = &config.token {
        let health_probe = path == "/stats" && request.method() == &Method::Get;
        let presented = header_value(&request, "authorization");
        authenticated = bearer_matches(presented.as_deref(), expected);
        if !health_probe && !authenticated {
            shared.lock().stats.unauthorized += 1;
            Reply::json(401, json!({"error": "a valid bearer token is required"}))
                .with("WWW-Authenticate", "Bearer")
                .send(request);
            return;
        }
    }

    if path == "/stats" && request.method() == &Method::Get {
        let body = {
            let state = shared.lock();
            let stats = &state.stats;
            json!({
                "role": "Hardware-in-the-loop real-vs-virtual command bridge",
                "routedReal": stats.routed_real,
                "routedSimulation": stats.routed_simulation,
                "blockedByInterlock": stats.blocked_by_interlock,
                "transportFailures": stats.transport_failures,
                "mirrored": stats.mirrored,
                "mirrorFailures": stats.mirror_failures,
                "malformedRequests": stats.malformed_requests,
                "notFound": stats.not_found,
                "unauthorized": stats.unauthorized,
                "uncertified": stats.uncertified,
                "auditFailures": stats.audit_failures,
                "staleCommands": stats.stale_commands,
                "outsideLimits": stats.outside_limits,
                "watchdogStops": stats.watchdog_stops,
                "watchdogStopFailures": stats.watchdog_stop_failures,
                "watchdogRequiresValidity": config.watchdog.require_validity,
                "watchdogMaxValidForMs": config.watchdog.max_valid_for_ms,
                "activeLeases": state.leases.active(now_ms()),
                "certifiedMode": !config.interlock.trusted_keys.is_empty(),
                "unsignedRiskAllowed": config.interlock.allow_unsigned_risk,
                "twinLinked": config.twin.is_some(),
                "twinReports": stats.twin_reports,
                "twinReportFailures": stats.twin_report_failures,
                "twinInSync": stats.twin_in_sync,
                "urdfLimits": config.limits.as_ref().map(|l| l.len()),
                "workers": if config.workers == 0 { DEFAULT_WORKERS } else { config.workers },
            })
        };
        Reply::json(200, body).send(request);
        return;
    }
    if path == "/audit" && request.method() == &Method::Get {
        // who sent the last orders for the real robot; behind the token like every route but /stats
        let body = {
            let state = shared.lock();
            json!({"records": state.audit.recent(), "file": state.audit.has_file()})
        };
        Reply::json(200, body).send(request);
        return;
    }
    if request.method() != &Method::Post {
        shared.lock().stats.not_found += 1;
        if matches!(
            path.as_str(),
            "/route" | "/mirror" | "/measured" | "/stats" | "/audit"
        ) {
            Reply::json(405, json!({"error": "method not allowed"}))
                .with(
                    "Allow",
                    if path == "/stats" || path == "/audit" {
                        "GET"
                    } else {
                        "POST"
                    },
                )
                .send(request);
        } else {
            Reply::json(404, json!({"error": "not found"})).send(request);
        }
        return;
    }

    if let Err((status, message)) = check_request_origin(&request) {
        shared.lock().stats.malformed_requests += 1;
        Reply::json(status, json!({"error": message})).send(request);
        return;
    }

    // Reading the body can take as long as the client wants: it is done before the lock is taken.
    let raw = match read_body(&mut request) {
        Ok(raw) => raw,
        Err(e) => {
            shared.lock().stats.malformed_requests += 1;
            let (status, message) = match e {
                BodyError::TooLarge => (
                    413,
                    format!("request body is larger than {MAX_BODY_BYTES} bytes"),
                ),
                BodyError::NotUtf8 => (400, "request body is not valid UTF-8".to_string()),
                BodyError::Io(e) => (400, format!("could not read request body: {e}")),
            };
            Reply::json(status, json!({"error": message})).send(request);
            return;
        }
    };

    let info = RequestInfo {
        caller: caller_label(header_value(&request, "x-hydra-caller").as_deref()),
        peer: request
            .remote_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "unknown".to_string()),
        authenticated,
    };
    let reply = match path.as_str() {
        "/route" => {
            let (reply, pending) = {
                let mut state = shared.lock();
                let reply = handle_route(
                    &raw,
                    &mut state,
                    &RouteContext {
                        fault_injection: config.fault_injection,
                        interlock: &config.interlock,
                        limits: config.limits.as_ref(),
                        twin_linked: config.twin.is_some(),
                    },
                    &info,
                );
                (reply, state.pending_twin.take())
            };
            // The twin is told AFTER the lock is released: the call can take as long as its timeout, and the command already went.
            match pending {
                Some(report) => {
                    let outcome = report_to_twin(shared, &report, &info);
                    reply.with("X-Twin-Report", outcome.kind())
                }
                None => reply,
            }
        }
        "/measured" => handle_measured(&raw, shared, &info),
        "/mirror" => handle_mirror(&raw, &mut shared.lock().stats),
        _ => {
            shared.lock().stats.not_found += 1;
            Reply::json(404, json!({"error": "not found"}))
        }
    };
    reply.send(request);
}

/// How often the watchdog looks at the leases while no request arrives.
const WATCHDOG_TICK_MS: u64 = 50;

/// Stops every joint whose lease ran out and records each stop. A stop that could not be done is counted and recorded as not delivered.
fn stop_lapsed_leases(
    leases: &mut Leases,
    sink: &mut dyn CommandSink,
    audit: &mut Audit,
    stats: &mut Stats,
) {
    for stop in leases.due_stops(now_ms()) {
        let result = sink.stop(&stop.joint);
        let delivered = result.is_ok();
        if delivered {
            stats.watchdog_stops += 1;
        } else {
            stats.watchdog_stop_failures += 1;
        }
        eprintln!(
            "[hil-bridge] watchdog: no valid command for joint {:?} since {} ms, stop delivered={}",
            stop.joint, stop.expired_at_ms, delivered
        );
        let entry = AuditEntry {
            at_ms: now_ms(),
            phase: "watchdog",
            caller: "watchdog".to_string(),
            peer: "local".to_string(),
            authenticated: false,
            mode: "real",
            joint: stop.joint,
            position: stop.last_position,
            certificate_key_id: None,
            certificate_nonce: None,
            kind: Some("watchdog_stop"),
            delivered: Some(delivered),
            reason: Some(match result {
                Ok(()) => "no newer valid command arrived before the lease of the last one ran out"
                    .to_string(),
                Err(e) => format!("the stop could not be delivered: {e}"),
            }),
        };
        if let Err(e) = audit.record(entry) {
            stats.audit_failures += 1;
            eprintln!("[hil-bridge] audit failure for a watchdog stop: {e}");
        }
    }
}

/// The settings `handle_route` reads.
struct RouteContext<'a> {
    fault_injection: bool,
    interlock: &'a InterlockConfig,
    /// The joints and ranges of the robot (`--urdf`), when known.
    limits: Option<&'a JointLimits>,
    /// A twin is configured: a delivered real command is reported to it.
    twin_linked: bool,
}

fn audit_entry(
    info: &RequestInfo,
    phase: &'static str,
    mode: Mode,
    command: &JointCommand,
    certificate: Option<&Certificate>,
) -> AuditEntry {
    AuditEntry {
        at_ms: now_ms(),
        phase,
        caller: info.caller.clone(),
        peer: info.peer.clone(),
        authenticated: info.authenticated,
        mode: if mode == Mode::Real {
            "real"
        } else {
            "simulation"
        },
        joint: command.joint.clone(),
        position: command.position,
        certificate_key_id: certificate.map(|c| c.key_id.clone()),
        certificate_nonce: certificate.map(|c| c.nonce.clone()),
        kind: None,
        delivered: None,
        reason: None,
    }
}

fn handle_route(raw: &str, state: &mut State, context: &RouteContext, info: &RequestInfo) -> Reply {
    let State {
        nonces,
        audit,
        stats,
        leases,
        twin_seq,
        pending_twin,
        ..
    } = state;
    let fault_injection = context.fault_injection;
    let req: RouteRequest = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(e) => {
            stats.malformed_requests += 1;
            return Reply::json(
                400,
                json!({"error": format!("malformed request JSON: {e}")}),
            );
        }
    };

    let validity = match req.validity() {
        Ok(validity) => validity,
        Err(reason) => {
            stats.malformed_requests += 1;
            return Reply::json(400, json!({"error": reason}));
        }
    };
    let command = JointCommand {
        joint: req.joint,
        position: req.position,
    };
    if let Err(reason) = command.validate() {
        stats.malformed_requests += 1;
        return Reply::json(400, json!({"error": reason}));
    }
    let use_simulated = req.transport.disconnected
        || req.transport.latency_ms.is_some()
        || req.transport.timeout_ms.is_some();
    if use_simulated && !fault_injection {
        stats.malformed_requests += 1;
        return Reply::json(
            400,
            json!({"error": "simulated transport faults are disabled; start the server with --fault-injection"}),
        );
    }
    let bridge = Bridge::new(req.mode);

    // A real order is accounted for BEFORE anything else happens to it: if the record cannot be written, it goes no further.
    if req.mode == Mode::Real {
        if let Err(e) = audit.record(audit_entry(
            info,
            "order",
            req.mode,
            &command,
            req.certificate.as_ref(),
        )) {
            stats.audit_failures += 1;
            eprintln!("[hil-bridge] audit failure, real order refused: {e}");
            return Reply::json(
                503,
                json!({"error": "the order could not be recorded in the audit log, so it was not forwarded"}),
            );
        }
    }

    let mut recording_real = RecordingSink::default();
    let mut simulated_real = if req.transport.disconnected {
        SimulatedTransport::disconnected()
    } else {
        SimulatedTransport::healthy().with_timeout(
            req.transport.latency_ms.unwrap_or(0),
            req.transport.timeout_ms.unwrap_or(u64::MAX),
        )
    };
    let real_sink: &mut dyn CommandSink = if use_simulated {
        &mut simulated_real
    } else {
        &mut recording_real
    };
    let mut sim_sink = RecordingSink::default();

    let outcome = match req.mode {
        Mode::Simulation => {
            bridge.route_command(command.clone(), req.risk, real_sink, &mut sim_sink)
        }
        Mode::Real => {
            // Real mode: the report that gates the command must be the twin's. A certificate is checked (signature, command, age, replay);
            // an unsigned report is believed only on the bench setting; anything else is blocked with the reason.
            // The validity window and the joint limits come first, so a stale or impossible command does not use up the certificate it carries.
            let limits_check = match context.limits {
                Some(limits) => limits.check(&command.joint, command.position),
                None => Ok(()),
            };
            if let Err(reason) = limits_check {
                stats.outside_limits += 1;
                RouteOutcome::BlockedByInterlock { reason }
            } else {
                let report = if let Err(reason) =
                    leases.check(&command.joint, validity.as_ref(), now_ms())
                {
                    stats.stale_commands += 1;
                    Err(reason)
                } else if let Some(certificate) = &req.certificate {
                    verify(
                        certificate,
                        &command,
                        &context.interlock.trusted_keys,
                        &context.interlock.policy,
                        now_ms(),
                        nonces,
                    )
                } else if context.interlock.allow_unsigned_risk {
                    req.risk.ok_or_else(|| {
                        "no twin risk report: a real command is not forwarded without one"
                            .to_string()
                    })
                } else {
                    Err("a real command needs a risk certificate signed by the twin (field \"certificate\"); a report written by the caller is not accepted".to_string())
                };
                match report {
                    Ok(report) => bridge.route_command(
                        command.clone(),
                        Some(report),
                        real_sink,
                        &mut sim_sink,
                    ),
                    Err(reason) => {
                        stats.uncertified += 1;
                        RouteOutcome::BlockedByInterlock { reason }
                    }
                }
            }
        }
    };
    if req.mode == Mode::Real && outcome == RouteOutcome::SentReal {
        if let Some(validity) = &validity {
            leases.grant(&command.joint, command.position, validity);
        }
        if context.twin_linked {
            let seq = twin_seq.next(&command.joint, now_ms());
            *pending_twin = Some(JointStateReport {
                joint: command.joint.clone(),
                position: command.position,
                seq,
                stamp_ms: now_ms(),
                source: StateSource::Commanded,
            });
        }
    }
    if req.mode == Mode::Real {
        let mut done = audit_entry(
            info,
            "outcome",
            req.mode,
            &command,
            req.certificate.as_ref(),
        );
        done.kind = Some(outcome.kind());
        done.delivered = Some(outcome.delivered());
        done.reason = match &outcome {
            RouteOutcome::BlockedByInterlock { reason }
            | RouteOutcome::TransportFailure { reason } => Some(reason.clone()),
            _ => None,
        };
        if let Err(e) = audit.record(done) {
            stats.audit_failures += 1;
            eprintln!("[hil-bridge] audit failure for an order that was already decided: {e}");
        }
    }
    match &outcome {
        RouteOutcome::SentReal => stats.routed_real += 1,
        RouteOutcome::SentSimulation => stats.routed_simulation += 1,
        RouteOutcome::BlockedByInterlock { .. } => stats.blocked_by_interlock += 1,
        RouteOutcome::TransportFailure { .. } => stats.transport_failures += 1,
    }
    eprintln!("{}", outcome.log_line(req.mode));
    Reply::json(200, serde_json::to_value(&outcome).unwrap())
        .with("X-Outcome-Kind", outcome.kind())
        .with(
            "X-Delivered",
            if outcome.delivered() { "true" } else { "false" },
        )
}

/// Sends one report to the twin (outside the lock), counts it and leaves an audit record. A report that failed does not undo the command it
/// follows: the command already went; the failure shows in `GET /stats` (`twinInSync`) and in the audit log.
fn report_to_twin(shared: &Shared, report: &JointStateReport, info: &RequestInfo) -> MirrorOutcome {
    let outcome = deliver(shared.config.twin.as_deref(), report);
    let mut state = shared.lock();
    match &outcome {
        MirrorOutcome::Delivered => {
            state.stats.twin_reports += 1;
            state.stats.twin_in_sync = Some(true);
        }
        MirrorOutcome::Failed { reason } => {
            state.stats.twin_report_failures += 1;
            state.stats.twin_in_sync = Some(false);
            eprintln!(
                "[hil-bridge] the twin was not told where joint {:?} is: {reason}",
                report.joint
            );
        }
        MirrorOutcome::NotConfigured => {}
    }
    if outcome != MirrorOutcome::NotConfigured {
        let entry = AuditEntry {
            at_ms: now_ms(),
            phase: "twin_report",
            caller: info.caller.clone(),
            peer: info.peer.clone(),
            authenticated: info.authenticated,
            mode: "real",
            joint: report.joint.clone(),
            position: report.position,
            certificate_key_id: None,
            certificate_nonce: None,
            kind: Some(outcome.kind()),
            delivered: Some(outcome == MirrorOutcome::Delivered),
            reason: match &outcome {
                MirrorOutcome::Failed { reason } => Some(reason.clone()),
                _ => None,
            },
        };
        if let Err(e) = state.audit.record(entry) {
            state.stats.audit_failures += 1;
            eprintln!("[hil-bridge] audit failure for a twin report: {e}");
        }
    }
    outcome
}

/// `POST /measured`: the controller read a joint back; tell the twin where it really is. Not gated by the interlock (it is observation),
/// but it needs a configured twin to mean anything.
fn handle_measured(raw: &str, shared: &Shared, info: &RequestInfo) -> Reply {
    let req: MirrorRequest = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(e) => {
            shared.lock().stats.malformed_requests += 1;
            return Reply::json(
                400,
                json!({"error": format!("malformed request JSON: {e}")}),
            );
        }
    };
    let command = JointCommand {
        joint: req.joint,
        position: req.position,
    };
    if let Err(reason) = command.validate() {
        shared.lock().stats.malformed_requests += 1;
        return Reply::json(400, json!({"error": reason}));
    }
    if shared.config.twin.is_none() {
        return Reply::json(
            200,
            json!({"reported": false, "error": "no twin is configured (--twin-url)"}),
        );
    }
    let report = {
        let mut state = shared.lock();
        let seq = state.twin_seq.next(&command.joint, now_ms());
        JointStateReport {
            joint: command.joint,
            position: command.position,
            seq,
            stamp_ms: now_ms(),
            source: StateSource::Measured,
        }
    };
    match report_to_twin(shared, &report, info) {
        MirrorOutcome::Delivered => Reply::json(200, json!({"reported": true, "seq": report.seq})),
        MirrorOutcome::Failed { reason } => {
            Reply::json(200, json!({"reported": false, "error": reason}))
        }
        MirrorOutcome::NotConfigured => Reply::json(200, json!({"reported": false})),
    }
}

fn handle_mirror(raw: &str, stats: &mut Stats) -> Reply {
    let req: MirrorRequest = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(e) => {
            stats.malformed_requests += 1;
            return Reply::json(
                400,
                json!({"error": format!("malformed request JSON: {e}")}),
            );
        }
    };

    let command = JointCommand {
        joint: req.joint,
        position: req.position,
    };
    if let Err(reason) = command.validate() {
        stats.malformed_requests += 1;
        return Reply::json(400, json!({"error": reason}));
    }
    let bridge = Bridge::new(Mode::Simulation);
    let mut mirror_sink = RecordingSink::default();
    match bridge.mirror_command(&command, &mut mirror_sink) {
        Ok(()) => {
            stats.mirrored += 1;
            Reply::json(200, json!({"mirrored": true}))
        }
        Err(e) => {
            stats.mirror_failures += 1;
            Reply::json(200, json!({"mirrored": false, "error": e.to_string()}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::thread;

    fn start_test_server() -> u16 {
        start_test_server_with(true)
    }

    fn start_test_server_with(fault_injection: bool) -> u16 {
        let server = bind("127.0.0.1:0").expect("bind on an OS-assigned port must succeed");
        let port = server
            .server_addr()
            .to_ip()
            .expect("tiny_http always binds a real IP socket for an http:// server")
            .port();
        thread::spawn(move || run(server, fault_injection));
        port
    }

    fn post(port: u16, path: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect must succeed");
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (headers, resp_body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let status_line = headers.lines().next().unwrap_or("");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, resp_body.to_string())
    }

    /// Sends raw bytes as a POST with the given extra header lines.
    fn post_raw(port: u16, path: &str, extra_headers: &str, body: &[u8]) -> u16 {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect must succeed");
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let text = String::from_utf8_lossy(&raw);
        text.lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    fn get(port: u16, path: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect must succeed");
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (headers, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let status_line = headers.lines().next().unwrap_or("");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, body.to_string())
    }

    #[test]
    fn route_real_mode_no_risk_is_sent_real() {
        let port = start_test_server();
        let body = r#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":false,"distance_m":2.0}}"#;
        let (status, resp) = post(port, "/route", body);
        assert_eq!(status, 200);
        assert!(resp.contains("SentReal"));
    }

    #[test]
    fn route_real_mode_without_a_twin_report_is_blocked() {
        let port = start_test_server();
        let body = r#"{"mode":"real","joint":"shoulder","position":1.0}"#;
        let (status, resp) = post(port, "/route", body);
        assert_eq!(status, 200);
        assert!(resp.contains("BlockedByInterlock"));
        assert!(resp.contains("no twin risk report"));
    }

    #[test]
    fn route_real_mode_with_a_tiny_clearance_is_blocked_even_without_the_imminent_flag() {
        let port = start_test_server();
        let body = r#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":false,"distance_m":0.001}}"#;
        let (status, resp) = post(port, "/route", body);
        assert_eq!(status, 200);
        assert!(resp.contains("BlockedByInterlock"));
    }

    #[test]
    fn an_oversized_body_is_refused_with_413_and_the_server_keeps_serving() {
        let port = start_test_server();
        let big = format!(
            r#"{{"mode":"simulation","joint":"{}","position":1.0}}"#,
            "a".repeat(20_000)
        );
        let (status, _) = post(port, "/route", &big);
        assert_eq!(status, 413);
        let (status, resp) = post(
            port,
            "/route",
            r#"{"mode":"simulation","joint":"shoulder","position":1.0}"#,
        );
        assert_eq!(status, 200);
        assert!(resp.contains("SentSimulation"));
    }

    #[test]
    fn route_real_mode_blocked_by_collision_risk() {
        let port = start_test_server();
        let body = r#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":true,"distance_m":0.1}}"#;
        let (status, resp) = post(port, "/route", body);
        assert_eq!(status, 200);
        assert!(resp.contains("BlockedByInterlock"));
    }

    #[test]
    fn route_simulation_mode_never_gated_by_interlock() {
        let port = start_test_server();
        let body = r#"{"mode":"simulation","joint":"shoulder","position":1.0,"risk":{"collision_imminent":true,"distance_m":0.1}}"#;
        let (status, resp) = post(port, "/route", body);
        assert_eq!(status, 200);
        assert!(resp.contains("SentSimulation"));
    }

    #[test]
    fn route_reports_transport_failure_when_disconnected() {
        let port = start_test_server();
        let body = r#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":false,"distance_m":2.0},"transport":{"disconnected":true}}"#;
        let (status, resp) = post(port, "/route", body);
        assert_eq!(status, 200);
        assert!(resp.contains("TransportFailure"));
    }

    #[test]
    fn route_rejects_malformed_json() {
        let port = start_test_server();
        let (status, _) = post(port, "/route", "not json");
        assert_eq!(status, 400);
    }

    #[test]
    fn mirror_succeeds() {
        let port = start_test_server();
        let body = r#"{"joint":"elbow","position":0.5}"#;
        let (status, resp) = post(port, "/mirror", body);
        assert_eq!(status, 200);
        assert!(resp.contains("\"mirrored\":true"));
    }

    #[test]
    fn stats() {
        let port = start_test_server();
        let (status, body) = get(port, "/stats");
        assert_eq!(status, 200);
        assert!(body.contains("role"));
    }

    #[test]
    fn stats_reports_real_process_lifetime_counters() {
        // Found while auditing the code: /stats
        // used to be a hardcoded static JSON body with nothing real to
        // aggregate. One of each real outcome is driven through /route
        // and /mirror, then /stats must reflect every one of them.
        let port = start_test_server();

        let (status, _) = get(port, "/stats");
        assert_eq!(status, 200);

        post(
            port,
            "/route",
            r#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":false,"distance_m":2.0}}"#,
        );
        post(
            port,
            "/route",
            r#"{"mode":"simulation","joint":"shoulder","position":1.0}"#,
        );
        post(
            port,
            "/route",
            r#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":true,"distance_m":0.1}}"#,
        );
        post(
            port,
            "/route",
            r#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":false,"distance_m":2.0},"transport":{"disconnected":true}}"#,
        );
        post(port, "/route", "not json");
        post(port, "/mirror", r#"{"joint":"elbow","position":0.5}"#);
        get(port, "/nope");

        let (status, body) = get(port, "/stats");
        assert_eq!(status, 200);
        let parsed: serde_json::Value =
            serde_json::from_str(&body).expect("stats body must be real JSON");
        assert_eq!(parsed["routedReal"], 1);
        assert_eq!(parsed["routedSimulation"], 1);
        assert_eq!(parsed["blockedByInterlock"], 1);
        assert_eq!(parsed["transportFailures"], 1);
        assert_eq!(parsed["mirrored"], 1);
        assert_eq!(parsed["malformedRequests"], 1);
        assert_eq!(parsed["notFound"], 1);
    }

    #[test]
    fn a_body_that_is_not_utf8_is_a_400_not_a_413() {
        let port = start_test_server();
        let status = post_raw(
            port,
            "/route",
            "Content-Type: application/json\r\n",
            &[0xFF, 0xFE, 0xFD, 0xFC],
        );
        assert_eq!(status, 400);
    }

    #[test]
    fn a_post_that_is_not_declared_as_json_is_refused() {
        let port = start_test_server();
        let body = br#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":false,"distance_m":2.0}}"#;
        assert_eq!(
            post_raw(port, "/route", "Content-Type: text/plain\r\n", body),
            415
        );
        assert_eq!(post_raw(port, "/route", "", body), 415);
        // the same body declared as JSON (with a charset) is accepted
        assert_eq!(
            post_raw(
                port,
                "/route",
                "Content-Type: application/json; charset=utf-8\r\n",
                body
            ),
            200
        );
    }

    #[test]
    fn a_post_coming_from_a_web_page_is_refused() {
        let port = start_test_server();
        let body = br#"{"mode":"simulation","joint":"shoulder","position":1.0}"#;
        let status = post_raw(
            port,
            "/route",
            "Content-Type: application/json\r\nOrigin: http://evil.example\r\n",
            body,
        );
        assert_eq!(status, 403);
    }

    #[test]
    fn an_invalid_joint_or_position_is_a_400() {
        let port = start_test_server();
        for body in [
            r#"{"mode":"simulation","joint":"","position":1.0}"#,
            r#"{"mode":"simulation","joint":"   ","position":1.0}"#,
            r#"{"mode":"real","joint":"shoulder","position":1e999,"risk":{"collision_imminent":false,"distance_m":2.0}}"#,
        ] {
            let (status, _) = post(port, "/route", body);
            assert_eq!(status, 400, "body: {body}");
        }
        let (status, _) = post(port, "/mirror", r#"{"joint":"","position":0.5}"#);
        assert_eq!(status, 400);
    }

    #[test]
    fn transport_fault_injection_is_refused_unless_enabled() {
        let port = start_test_server_with(false);
        let body = r#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":false,"distance_m":2.0},"transport":{"disconnected":true}}"#;
        let (status, resp) = post(port, "/route", body);
        assert_eq!(status, 400);
        assert!(resp.contains("--fault-injection"));
        // without the transport field the same server keeps routing
        let (status, resp) = post(
            port,
            "/route",
            r#"{"mode":"simulation","joint":"shoulder","position":1.0}"#,
        );
        assert_eq!(status, 200);
        assert!(resp.contains("SentSimulation"));
    }

    #[test]
    fn a_known_path_with_the_wrong_method_is_a_405() {
        let port = start_test_server();
        let (status, _) = get(port, "/route");
        assert_eq!(status, 405);
        let (status, _) = get(port, "/mirror");
        assert_eq!(status, 405);
    }

    #[test]
    fn unknown_path_is_404() {
        let port = start_test_server();
        let (status, _) = get(port, "/nope");
        assert_eq!(status, 404);
    }

    const TEST_TOKEN: &str = "a-long-random-token-1234";

    fn start_test_server_with_token() -> u16 {
        let server = bind("127.0.0.1:0").expect("bind on an OS-assigned port must succeed");
        let port = server
            .server_addr()
            .to_ip()
            .expect("tiny_http always binds a real IP socket for an http:// server")
            .port();
        thread::spawn(move || run_with_token(server, true, Some(TEST_TOKEN.to_string())));
        port
    }

    fn get_status_with_headers(port: u16, path: &str, extra_headers: &str) -> u16 {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect must succeed");
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra_headers}Connection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        raw.lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    #[test]
    fn with_a_token_a_request_without_it_is_refused_but_the_health_path_stays_open() {
        let port = start_test_server_with_token();
        // the health path of the manifest is open for the service registry
        assert_eq!(get_status_with_headers(port, "/stats", ""), 200);
        // everything else needs the token, even a path that does not exist
        assert_eq!(get_status_with_headers(port, "/nope", ""), 401);
        let body = r#"{"mode":"simulation","joint":"shoulder","position":0.5}"#;
        let json = "Content-Type: application/json\r\n";
        assert_eq!(post_raw(port, "/route", json, body.as_bytes()), 401);
        assert_eq!(post_raw(port, "/mirror", json, body.as_bytes()), 401);
        // a wrong token and a wrong scheme are refused too
        let wrong = format!("{json}Authorization: Bearer wrong-token-0000000\r\n");
        assert_eq!(post_raw(port, "/route", &wrong, body.as_bytes()), 401);
        let basic = format!("{json}Authorization: Basic {TEST_TOKEN}\r\n");
        assert_eq!(post_raw(port, "/route", &basic, body.as_bytes()), 401);
    }

    #[test]
    fn with_a_token_the_right_bearer_header_reaches_the_routes() {
        let port = start_test_server_with_token();
        let body = r#"{"mode":"simulation","joint":"shoulder","position":0.5}"#;
        let ok =
            format!("Content-Type: application/json\r\nAuthorization: Bearer {TEST_TOKEN}\r\n");
        assert_eq!(post_raw(port, "/route", &ok, body.as_bytes()), 200);
        // the header name is case-insensitive
        let lower =
            format!("Content-Type: application/json\r\nauthorization: Bearer {TEST_TOKEN}\r\n");
        assert_eq!(post_raw(port, "/mirror", &lower, body.as_bytes()), 200);
        assert_eq!(
            get_status_with_headers(
                port,
                "/nope",
                &format!("Authorization: Bearer {TEST_TOKEN}\r\n")
            ),
            404
        );
    }

    #[test]
    fn refused_requests_are_counted() {
        let port = start_test_server_with_token();
        get_status_with_headers(port, "/nope", "");
        get_status_with_headers(port, "/nope", "");
        let (_, body) = get(port, "/stats");
        assert!(body.contains("\"unauthorized\":2"), "{body}");
    }

    // ---- the real mode is certified by the twin, and every real order is accounted for ----

    use ed25519_dalek::{Signer as _, SigningKey};

    /// The twin of these tests: the same seed and the same signed text as HYDRA-UMC-TWIN.
    fn twin_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn twin_public_key_hex() -> String {
        twin_key()
            .verifying_key()
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A certificate as the twin would issue it, signed by `key`.
    fn certificate_json(
        key: &SigningKey,
        joint: &str,
        position: f64,
        imminent: bool,
        distance: f64,
        issued_at_ms: u64,
        nonce: &str,
    ) -> String {
        let key_id = hex(&twin_key().verifying_key().to_bytes()[..8]);
        let text = format!("hydra-umc-interlock-certificate-v1\n{key_id}\n{joint}\n{position:?}\n{imminent}\n{distance:?}\n{issued_at_ms}\n{nonce}");
        let signature = hex(&key.sign(text.as_bytes()).to_bytes());
        format!(
            r#"{{"joint":"{joint}","position":{position:?},"collision_imminent":{imminent},"distance_m":{distance:?},"issued_at_ms":{issued_at_ms},"nonce":"{nonce}","key_id":"{key_id}","signature":"{signature}"}}"#
        )
    }

    fn fresh_certificate(nonce: &str) -> String {
        certificate_json(&twin_key(), "shoulder", 1.0, false, 2.0, now_ms(), nonce)
    }

    fn route_body(mode: &str, certificate: Option<&str>) -> String {
        match certificate {
            Some(c) => format!(
                r#"{{"mode":"{mode}","joint":"shoulder","position":1.0,"certificate":{c}}}"#
            ),
            None => format!(r#"{{"mode":"{mode}","joint":"shoulder","position":1.0}}"#),
        }
    }

    fn certified_config() -> ServerConfig {
        ServerConfig {
            interlock: InterlockConfig {
                trusted_keys: TrustedKeys::from_text(&twin_public_key_hex()).unwrap(),
                ..InterlockConfig::default()
            },
            ..ServerConfig::default()
        }
    }

    fn start_with(config: ServerConfig) -> u16 {
        let server = bind("127.0.0.1:0").expect("bind on an OS-assigned port must succeed");
        let port = server
            .server_addr()
            .to_ip()
            .expect("a real IP socket")
            .port();
        thread::spawn(move || run_with_config(server, config));
        port
    }

    fn post_with(port: u16, path: &str, extra_headers: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect must succeed");
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (headers, resp_body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let status: u16 = headers
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, resp_body.to_string())
    }

    fn get_with(port: u16, path: &str, extra_headers: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect must succeed");
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra_headers}Connection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (headers, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let status: u16 = headers
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, body.to_string())
    }

    const NONCE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NONCE_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn a_real_command_with_a_certificate_from_the_twin_is_forwarded() {
        let port = start_with(certified_config());
        let (status, resp) = post_with(
            port,
            "/route",
            "",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        assert_eq!(status, 200);
        assert!(resp.contains("SentReal"), "{resp}");
    }

    #[test]
    fn by_default_a_real_command_needs_a_certificate_and_a_written_report_is_not_believed() {
        let port = start_with(certified_config());
        let (_, resp) = post_with(
            port,
            "/route",
            "",
            r#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":false,"distance_m":2.0}}"#,
        );
        assert!(resp.contains("BlockedByInterlock"), "{resp}");
        assert!(resp.contains("signed by the twin"), "{resp}");
        let (_, none) = post_with(port, "/route", "", &route_body("real", None));
        assert!(none.contains("BlockedByInterlock"), "{none}");
    }

    #[test]
    fn with_no_twin_key_configured_every_real_command_is_blocked_even_with_a_certificate() {
        let port = start_with(ServerConfig::default());
        let (_, resp) = post_with(
            port,
            "/route",
            "",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        assert!(resp.contains("BlockedByInterlock"), "{resp}");
        assert!(resp.contains("no twin public key"), "{resp}");
    }

    #[test]
    fn the_bench_setting_believes_a_written_report_and_is_off_unless_asked_for() {
        let mut config = certified_config();
        config.interlock.allow_unsigned_risk = true;
        let port = start_with(config);
        let (_, resp) = post_with(
            port,
            "/route",
            "",
            r#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":false,"distance_m":2.0}}"#,
        );
        assert!(resp.contains("SentReal"), "{resp}");
        let (_, stats) = get_with(port, "/stats", "");
        assert!(stats.contains("\"unsignedRiskAllowed\":true"));
    }

    #[test]
    fn a_certificate_signed_by_anyone_else_is_blocked() {
        let port = start_with(certified_config());
        let forger = SigningKey::from_bytes(&[9u8; 32]);
        let forged = certificate_json(&forger, "shoulder", 1.0, false, 2.0, now_ms(), NONCE_A);
        let (_, resp) = post_with(port, "/route", "", &route_body("real", Some(&forged)));
        assert!(
            resp.contains("BlockedByInterlock") && resp.contains("signature is not valid"),
            "{resp}"
        );
    }

    #[test]
    fn a_certificate_is_good_for_one_command_only() {
        let port = start_with(certified_config());
        let certificate = fresh_certificate(NONCE_A);
        let (_, first) = post_with(port, "/route", "", &route_body("real", Some(&certificate)));
        assert!(first.contains("SentReal"));
        let (_, replay) = post_with(port, "/route", "", &route_body("real", Some(&certificate)));
        assert!(
            replay.contains("BlockedByInterlock") && replay.contains("already used"),
            "{replay}"
        );
    }

    #[test]
    fn a_certificate_for_another_position_or_joint_or_an_old_one_is_blocked() {
        let port = start_with(certified_config());
        let other_position =
            certificate_json(&twin_key(), "shoulder", 0.5, false, 2.0, now_ms(), NONCE_A);
        let (_, resp) = post_with(
            port,
            "/route",
            "",
            &route_body("real", Some(&other_position)),
        );
        assert!(resp.contains("another command"), "{resp}");
        let other_joint =
            certificate_json(&twin_key(), "elbow", 1.0, false, 2.0, now_ms(), NONCE_B);
        let (_, resp) = post_with(port, "/route", "", &route_body("real", Some(&other_joint)));
        assert!(resp.contains("another command"), "{resp}");
        let old = certificate_json(
            &twin_key(),
            "shoulder",
            1.0,
            false,
            2.0,
            now_ms() - 60_000,
            "cccccccccccccccccccccccccccccccc",
        );
        let (_, resp) = post_with(port, "/route", "", &route_body("real", Some(&old)));
        assert!(resp.contains("older than"), "{resp}");
    }

    #[test]
    fn a_genuine_certificate_that_reports_a_collision_still_blocks() {
        let port = start_with(certified_config());
        let imminent =
            certificate_json(&twin_key(), "shoulder", 1.0, true, 0.02, now_ms(), NONCE_A);
        let (_, resp) = post_with(port, "/route", "", &route_body("real", Some(&imminent)));
        assert!(
            resp.contains("BlockedByInterlock") && resp.contains("imminent collision"),
            "{resp}"
        );
    }

    #[test]
    fn simulation_mode_needs_no_certificate() {
        let port = start_with(certified_config());
        let (_, resp) = post_with(port, "/route", "", &route_body("simulation", None));
        assert!(resp.contains("SentSimulation"), "{resp}");
    }

    #[test]
    fn a_malformed_certificate_is_a_400_and_never_reaches_the_hardware() {
        let port = start_with(certified_config());
        let (status, _) = post_with(
            port,
            "/route",
            "",
            r#"{"mode":"real","joint":"shoulder","position":1.0,"certificate":{"joint":"shoulder"}}"#,
        );
        assert_eq!(status, 400);
    }

    #[test]
    fn every_real_order_is_in_the_audit_with_its_caller_and_its_outcome() {
        let port = start_with(certified_config());
        post_with(
            port,
            "/route",
            "X-Hydra-Caller: planner-1\r\n",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        post_with(port, "/route", "", &route_body("real", None));
        post_with(port, "/route", "", &route_body("simulation", None));
        let (status, body) = get_with(port, "/audit", "");
        assert_eq!(status, 200);
        let audit: serde_json::Value = serde_json::from_str(&body).unwrap();
        let records = audit["records"].as_array().unwrap();
        assert_eq!(records.len(), 4, "two real orders, an order and an outcome each; the simulation is not an order for the robot");
        assert_eq!(records[0]["phase"], "order");
        assert_eq!(records[0]["caller"], "planner-1");
        assert_eq!(records[0]["certificate_nonce"], NONCE_A);
        assert_eq!(records[1]["phase"], "outcome");
        assert_eq!(records[1]["kind"], "sent_real");
        assert_eq!(records[1]["delivered"], true);
        assert_eq!(records[2]["caller"], "unidentified");
        assert_eq!(records[3]["kind"], "blocked_by_interlock");
        assert!(records[3]["reason"]
            .as_str()
            .unwrap()
            .contains("signed by the twin"));
        assert!(records[0]["peer"]
            .as_str()
            .unwrap()
            .starts_with("127.0.0.1:"));
    }

    #[test]
    fn the_audit_says_whether_the_request_carried_the_token_and_is_behind_it() {
        let mut config = certified_config();
        config.token = Some("a-bearer-token-of-sixteen".to_string());
        let port = start_with(config);
        let bearer = "Authorization: Bearer a-bearer-token-of-sixteen\r\n";
        assert_eq!(get_with(port, "/audit", "").0, 401);
        post_with(
            port,
            "/route",
            bearer,
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        let (status, body) = get_with(port, "/audit", bearer);
        assert_eq!(status, 200);
        let audit: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(audit["records"][0]["authenticated"], true);
    }

    #[test]
    fn the_audit_is_written_to_the_file_before_the_order_is_decided() {
        let dir = std::env::temp_dir().join(format!("hil-audit-server-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("audit.jsonl");
        let _ = std::fs::remove_file(&path);
        let mut config = certified_config();
        config.audit_log = Some(path.clone());
        let port = start_with(config);
        post_with(
            port,
            "/route",
            "X-Hydra-Caller: supervisor\r\n",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let phases: Vec<String> = text
            .lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["phase"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(phases, vec!["order", "outcome"]);
        assert!(text.contains("supervisor"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_order_that_cannot_be_recorded_is_not_forwarded() {
        let mut config = certified_config();
        config.audit_log = Some(
            std::env::temp_dir()
                .join("hil-no-such-dir-for-audit")
                .join("deeper")
                .join("audit.jsonl"),
        );
        let port = start_with(config);
        let (status, body) = post_with(
            port,
            "/route",
            "",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        assert_eq!(status, 503);
        assert!(body.contains("not forwarded"), "{body}");
        // the certificate was not used up by an order that never went anywhere
        let (_, stats) = get_with(port, "/stats", "");
        assert!(stats.contains("\"auditFailures\":1"), "{stats}");
        assert!(stats.contains("\"routedReal\":0"), "{stats}");
    }

    #[test]
    fn an_unusable_caller_label_is_recorded_as_unidentified() {
        let port = start_with(certified_config());
        let long = "x".repeat(200);
        post_with(
            port,
            "/route",
            &format!("X-Hydra-Caller: {long}\r\n"),
            &route_body("real", None),
        );
        let (_, body) = get_with(port, "/audit", "");
        let audit: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(audit["records"][0]["caller"], "unidentified");
    }

    #[test]
    fn the_stats_say_how_the_real_mode_is_set_up() {
        let (_, stats) = get_with(start_with(certified_config()), "/stats", "");
        assert!(
            stats.contains("\"certifiedMode\":true")
                && stats.contains("\"unsignedRiskAllowed\":false"),
            "{stats}"
        );
        let (_, bare) = get_with(start_with(ServerConfig::default()), "/stats", "");
        assert!(bare.contains("\"certifiedMode\":false"), "{bare}");
    }

    // ---- the consumer-side watchdog (see watchdog.rs) ----

    fn watched_config(require_validity: bool) -> ServerConfig {
        ServerConfig {
            watchdog: WatchdogConfig {
                require_validity,
                ..WatchdogConfig::default()
            },
            ..certified_config()
        }
    }

    fn windowed_body(certificate: &str, issued_at_ms: u64, valid_for_ms: u64, seq: u64) -> String {
        format!(
            r#"{{"mode":"real","joint":"shoulder","position":1.0,"certificate":{certificate},"issued_at_ms":{issued_at_ms},"valid_for_ms":{valid_for_ms},"seq":{seq}}}"#
        )
    }

    fn stat(port: u16, name: &str) -> u64 {
        let (_, body) = get_with(port, "/stats", "");
        serde_json::from_str::<serde_json::Value>(&body).unwrap()[name]
            .as_u64()
            .unwrap_or(u64::MAX)
    }

    #[test]
    fn a_real_command_without_a_window_is_blocked_when_the_window_is_required() {
        let port = start_with(watched_config(true));
        let (status, resp) = post_with(
            port,
            "/route",
            "",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        assert_eq!(status, 200);
        assert!(resp.contains("BlockedByInterlock"), "{resp}");
        assert!(resp.contains("validity window"), "{resp}");
        assert_eq!(stat(port, "staleCommands"), 1);
        assert_eq!(stat(port, "routedReal"), 0);
    }

    #[test]
    fn a_command_with_a_fresh_window_is_forwarded_and_holds_a_lease() {
        let port = start_with(watched_config(true));
        let (_, resp) = post_with(
            port,
            "/route",
            "",
            &windowed_body(&fresh_certificate(NONCE_A), now_ms(), 800, 1),
        );
        assert!(resp.contains("SentReal"), "{resp}");
        assert_eq!(stat(port, "activeLeases"), 1);
    }

    #[test]
    fn an_expired_window_is_blocked_and_does_not_use_up_the_certificate() {
        let port = start_with(watched_config(true));
        let certificate = fresh_certificate(NONCE_A);
        let (_, stale) = post_with(
            port,
            "/route",
            "",
            &windowed_body(&certificate, now_ms().saturating_sub(5000), 200, 1),
        );
        assert!(stale.contains("BlockedByInterlock"), "{stale}");
        assert!(stale.contains("expired"), "{stale}");
        // the same certificate is still good for a fresh command
        let (_, fresh) = post_with(
            port,
            "/route",
            "",
            &windowed_body(&certificate, now_ms(), 800, 1),
        );
        assert!(fresh.contains("SentReal"), "{fresh}");
    }

    #[test]
    fn a_repeated_sequence_number_is_blocked() {
        let port = start_with(watched_config(true));
        let (_, first) = post_with(
            port,
            "/route",
            "",
            &windowed_body(&fresh_certificate(NONCE_A), now_ms(), 800, 5),
        );
        assert!(first.contains("SentReal"), "{first}");
        let (_, again) = post_with(
            port,
            "/route",
            "",
            &windowed_body(&fresh_certificate(NONCE_B), now_ms(), 800, 5),
        );
        assert!(again.contains("BlockedByInterlock"), "{again}");
        assert!(again.contains("not newer"), "{again}");
    }

    #[test]
    fn half_a_window_is_a_400() {
        let port = start_with(watched_config(true));
        let body = r#"{"mode":"real","joint":"shoulder","position":1.0,"issued_at_ms":1}"#;
        let (status, _) = post_with(port, "/route", "", body);
        assert_eq!(status, 400);
    }

    #[test]
    fn a_joint_whose_lease_runs_out_is_stopped_by_the_bridge_itself() {
        let port = start_with(watched_config(true));
        let (_, resp) = post_with(
            port,
            "/route",
            "",
            &windowed_body(&fresh_certificate(NONCE_A), now_ms(), 150, 1),
        );
        assert!(resp.contains("SentReal"), "{resp}");
        assert_eq!(stat(port, "watchdogStops"), 0);
        // nothing else arrives: the producer is gone
        let mut stops = 0;
        for _ in 0..40 {
            thread::sleep(std::time::Duration::from_millis(50));
            stops = stat(port, "watchdogStops");
            if stops > 0 {
                break;
            }
        }
        assert_eq!(stops, 1, "the watchdog must stop the joint once");
        assert_eq!(stat(port, "activeLeases"), 0);
        let (_, audit) = get_with(port, "/audit", "");
        assert!(audit.contains("watchdog_stop"), "{audit}");
        thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(stat(port, "watchdogStops"), 1, "a stop is not repeated");
    }

    #[test]
    fn a_command_without_a_window_pass_when_the_bridge_is_started_for_undated_commands() {
        let port = start_with(watched_config(false));
        let (_, resp) = post_with(
            port,
            "/route",
            "",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        assert!(resp.contains("SentReal"), "{resp}");
        assert_eq!(stat(port, "activeLeases"), 0);
    }

    // ---- the joint limits of the URDF (see limits.rs) ----

    const ARM_URDF: &str = r#"<robot name="arm"><link name="b"/><link name="l"/>
        <joint name="shoulder" type="revolute"><parent link="b"/><child link="l"/>
            <limit lower="-1.5" upper="2.0" effort="1" velocity="1"/></joint>
        <joint name="mount" type="fixed"><parent link="b"/><child link="l"/></joint></robot>"#;

    fn limited_config() -> ServerConfig {
        ServerConfig {
            limits: Some(JointLimits::from_urdf(ARM_URDF).unwrap()),
            ..watched_config(false)
        }
    }

    fn real_body(joint: &str, position: f64, certificate: &str) -> String {
        format!(
            r#"{{"mode":"real","joint":"{joint}","position":{position},"certificate":{certificate}}}"#
        )
    }

    fn certificate_for(joint: &str, position: f64, nonce: &str) -> String {
        // the certificate of the other tests is for joint "shoulder" at 1.0; the limits are checked before it is, so the content of one for
        // another command only matters when the command passes the limits
        let _ = (joint, position);
        fresh_certificate(nonce)
    }

    #[test]
    fn a_real_command_outside_the_range_of_its_joint_is_blocked_before_its_certificate_is_used() {
        let port = start_with(limited_config());
        let (status, resp) = post_with(
            port,
            "/route",
            "",
            &real_body("shoulder", 2.5, &certificate_for("shoulder", 2.5, NONCE_A)),
        );
        assert_eq!(status, 200);
        assert!(resp.contains("BlockedByInterlock"), "{resp}");
        assert!(resp.contains("outside its limits"), "{resp}");
        assert_eq!(stat(port, "outsideLimits"), 1);
        assert_eq!(stat(port, "routedReal"), 0);
        // the same certificate (same nonce) still works for a command inside the range: it was not spent
        let (_, ok) = post_with(
            port,
            "/route",
            "",
            &real_body("shoulder", 1.0, &certificate_for("shoulder", 1.0, NONCE_A)),
        );
        assert!(ok.contains("SentReal"), "{ok}");
    }

    #[test]
    fn a_real_command_for_a_joint_the_urdf_does_not_have_or_a_fixed_one_is_blocked() {
        let port = start_with(limited_config());
        for joint in ["elbow", "mount"] {
            let (_, resp) = post_with(
                port,
                "/route",
                "",
                &real_body(joint, 0.0, &certificate_for(joint, 0.0, NONCE_B)),
            );
            assert!(resp.contains("BlockedByInterlock"), "{joint}: {resp}");
        }
        assert_eq!(stat(port, "outsideLimits"), 2);
    }

    #[test]
    fn the_simulation_mode_is_not_gated_by_the_limits_of_the_urdf() {
        let port = start_with(limited_config());
        let (_, resp) = post_with(
            port,
            "/route",
            "",
            r#"{"mode":"simulation","joint":"elbow","position":9.0}"#,
        );
        assert!(resp.contains("SentSimulation"), "{resp}");
        assert_eq!(stat(port, "outsideLimits"), 0);
    }

    #[test]
    fn stats_say_how_many_joints_the_urdf_gave_and_how_many_workers_serve() {
        let port = start_with(limited_config());
        assert_eq!(stat(port, "urdfLimits"), 2);
        assert_eq!(stat(port, "workers"), DEFAULT_WORKERS as u64);
        let (_, bare) = get_with(start_with(ServerConfig::default()), "/stats", "");
        assert!(bare.contains("\"urdfLimits\":null"), "{bare}");
    }

    // ---- several workers ----

    #[test]
    fn a_client_that_holds_a_connection_open_without_finishing_its_request_does_not_stop_the_others(
    ) {
        let port = start_with(ServerConfig::default());
        // half a request line, never the rest: this connection stays stuck for as long as it stays open
        let mut stuck = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stuck.write_all(b"GET /stats HT").unwrap();
        // a POST that promised a body and never sends it holds a worker too
        let mut half_body = TcpStream::connect(("127.0.0.1", port)).unwrap();
        half_body
            .write_all(
                b"POST /route HTTP/1.1
Host: x
Content-Type: application/json
Content-Length: 500

{",
            )
            .unwrap();
        for _ in 0..3 {
            let (status, _) = get_with(port, "/stats", "");
            assert_eq!(status, 200);
        }
        let (status, resp) = post(
            port,
            "/route",
            r#"{"mode":"simulation","joint":"shoulder","position":1.0}"#,
        );
        assert_eq!(status, 200, "{resp}");
        drop((stuck, half_body));
    }

    #[test]
    fn the_watchdog_still_stops_a_joint_while_the_workers_wait_for_a_stuck_client() {
        let port = start_with(watched_config(true));
        let mut stuck = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stuck
            .write_all(
                b"POST /route HTTP/1.1
Host: x
Content-Type: application/json
Content-Length: 500

{",
            )
            .unwrap();
        let (_, resp) = post_with(
            port,
            "/route",
            "",
            &windowed_body(&fresh_certificate(NONCE_A), now_ms(), 150, 1),
        );
        assert!(resp.contains("SentReal"), "{resp}");
        let mut stops = 0;
        for _ in 0..40 {
            thread::sleep(std::time::Duration::from_millis(50));
            stops = stat(port, "watchdogStops");
            if stops > 0 {
                break;
            }
        }
        assert_eq!(stops, 1);
        drop(stuck);
    }

    #[test]
    fn orders_sent_at_the_same_time_are_all_counted_and_recorded_once() {
        let port = start_with(ServerConfig {
            interlock: InterlockConfig {
                allow_unsigned_risk: true,
                ..InterlockConfig::default()
            },
            ..ServerConfig::default()
        });
        let senders: Vec<_> = (0..16)
            .map(|_| {
                thread::spawn(move || {
                    for _ in 0..5 {
                        let (status, resp) = post(
                            port,
                            "/route",
                            r#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":false,"distance_m":2.0}}"#,
                        );
                        assert_eq!(status, 200);
                        assert!(resp.contains("SentReal"), "{resp}");
                    }
                })
            })
            .collect();
        for sender in senders {
            sender.join().unwrap();
        }
        assert_eq!(stat(port, "routedReal"), 80);
        let (_, audit) = get_with(port, "/audit", "");
        // 80 orders leave 160 records (an order and an outcome each); the body may come in chunks, so they are counted in the text
        assert_eq!(audit.matches("\"phase\":\"order\"").count(), 80, "{audit}");
        assert_eq!(audit.matches("\"phase\":\"outcome\"").count(), 80);
    }

    // ---- the way back to the twin (see twin_link.rs) ----

    fn twin_config(sink: &std::sync::Arc<crate::twin_link::RecordingStateSink>) -> ServerConfig {
        let sink: std::sync::Arc<dyn TwinStateSink> = sink.clone();
        ServerConfig {
            twin: Some(sink),
            ..certified_config()
        }
    }

    fn stats_json(port: u16) -> serde_json::Value {
        serde_json::from_str(&get_with(port, "/stats", "").1).unwrap()
    }

    #[test]
    fn a_delivered_real_command_is_reported_to_the_twin_as_commanded() {
        let sink = std::sync::Arc::new(crate::twin_link::RecordingStateSink::connected());
        let port = start_with(twin_config(&sink));
        let (status, resp) = post_with(
            port,
            "/route",
            "",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        assert_eq!(status, 200, "{resp}");
        assert!(resp.contains("SentReal"), "{resp}");
        let reports = sink.reports();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].joint, "shoulder");
        assert_eq!(reports[0].position, 1.0);
        assert_eq!(reports[0].source, StateSource::Commanded);
        let stats = stats_json(port);
        assert_eq!(stats["twinLinked"], true);
        assert_eq!(stats["twinReports"], 1);
        assert_eq!(stats["twinInSync"], true);
    }

    #[test]
    fn a_report_that_cannot_reach_the_twin_does_not_undo_the_command_but_shows_in_the_stats_and_the_audit(
    ) {
        let sink = std::sync::Arc::new(crate::twin_link::RecordingStateSink::disconnected());
        let port = start_with(twin_config(&sink));
        let (_, resp) = post_with(
            port,
            "/route",
            "",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        assert!(resp.contains("SentReal"), "{resp}");
        let stats = stats_json(port);
        assert_eq!(stats["twinReportFailures"], 1);
        assert_eq!(stats["twinReports"], 0);
        assert_eq!(stats["twinInSync"], false);
        let (_, audit) = get_with(port, "/audit", "");
        assert!(audit.contains("twin_report"), "{audit}");
        assert!(audit.contains("transport is disconnected"), "{audit}");
    }

    #[test]
    fn a_blocked_command_and_a_simulated_one_are_not_reported() {
        let sink = std::sync::Arc::new(crate::twin_link::RecordingStateSink::connected());
        let port = start_with(twin_config(&sink));
        // no certificate: blocked
        post_with(port, "/route", "", &route_body("real", None));
        // simulation mode: the real robot did not move
        post_with(port, "/route", "", &route_body("simulation", None));
        assert!(sink.reports().is_empty());
        assert!(stats_json(port)["twinInSync"].is_null());
    }

    #[test]
    fn the_measured_position_is_reported_with_a_newer_sequence_than_the_commanded_one() {
        let sink = std::sync::Arc::new(crate::twin_link::RecordingStateSink::connected());
        let port = start_with(twin_config(&sink));
        post_with(
            port,
            "/route",
            "",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        let (status, resp) = post_with(
            port,
            "/measured",
            "",
            r#"{"joint":"shoulder","position":0.98}"#,
        );
        assert_eq!(status, 200, "{resp}");
        assert!(resp.contains("\"reported\":true"), "{resp}");
        let reports = sink.reports();
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[1].source, StateSource::Measured);
        assert_eq!(reports[1].position, 0.98);
        assert!(reports[1].seq > reports[0].seq);
    }

    #[test]
    fn measured_without_a_twin_says_so_and_a_bad_body_is_a_400() {
        let port = start_with(certified_config());
        let (status, resp) = post_with(
            port,
            "/measured",
            "",
            r#"{"joint":"shoulder","position":0.98}"#,
        );
        assert_eq!(status, 200);
        assert!(resp.contains("\"reported\":false"), "{resp}");
        assert!(resp.contains("no twin is configured"), "{resp}");
        assert_eq!(post_with(port, "/measured", "", "not json").0, 400);
        assert_eq!(
            post_with(port, "/measured", "", r#"{"joint":"","position":1.0}"#).0,
            400
        );
        assert_eq!(get_with(port, "/measured", "").0, 405);
    }

    #[test]
    fn a_measured_report_that_the_twin_does_not_get_is_told_to_the_caller() {
        let sink = std::sync::Arc::new(crate::twin_link::RecordingStateSink::disconnected());
        let port = start_with(twin_config(&sink));
        let (status, resp) = post_with(
            port,
            "/measured",
            "",
            r#"{"joint":"shoulder","position":0.98}"#,
        );
        assert_eq!(status, 200);
        assert!(resp.contains("\"reported\":false"), "{resp}");
        assert_eq!(stats_json(port)["twinInSync"], false);
    }
}
