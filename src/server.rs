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

use std::sync::atomic::Ordering;
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
use crate::metrics::{Histogram, HistogramSeries, Series};
use crate::protocol::{JointCommand, Mode};
use crate::safety_zones::{ZonesDecision, ZonesState};
use crate::session_record::{Event, Recorder};
use crate::twin_link::{
    deliver, JointStateReport, MirrorOutcome, StateSequencer, StateSource, TwinStateSink,
};
use crate::twin_pass::{Proof as TwinPassProof, VerdictLedger};
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
    /// The pass verdict of the twin for the trajectory this command belongs to, with the samples it was made for (see `twin_pass.rs`). Read only
    /// when the switch `require_twin_pass` is on.
    twin_pass: Option<TwinPassProof>,
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
    /// Real commands refused for lack of a valid pass verdict of the twin (the switch `require_twin_pass`).
    twin_pass_refused: u64,
    audit_failures: u64,
    stale_commands: u64,
    outside_limits: u64,
    watchdog_stops: u64,
    watchdog_stop_failures: u64,
    twin_reports: u64,
    twin_report_failures: u64,
    /// Whether the last report to the twin arrived (None: none was tried yet or no twin is configured).
    twin_in_sync: Option<bool>,
    /// The collision prediction asked of the twin in shadow (see `collision_shadow.rs`): answered, with a contact predicted, and failed.
    shadow_predictions: u64,
    shadow_would_block: u64,
    shadow_failures: u64,
    /// Enforced mode: real commands refused because the twin predicted a contact, and because it gave no prediction in time.
    shadow_blocked: u64,
    shadow_unavailable: u64,
    /// The last shadow answer: "clear", "collision" or "failed" (None: none was asked yet).
    shadow_last: Option<&'static str>,
    /// The zone monitor as a condition of the interlock: real commands refused because the zones were not clear, and because it gave no
    /// usable answer in time.
    zones_checked: u64,
    zones_blocked: u64,
    zones_unavailable: u64,
    /// The last state of the zones as the interlock read it: "clear", "blocked" or "unavailable" (None: none was asked yet).
    zones_last: Option<&'static str>,
    /// Sessions read back from the data lake and run again in simulation, and the replays that failed.
    replays: u64,
    replay_failures: u64,
    /// How long `POST /route` took to answer, per mode.
    route_real_seconds: Histogram,
    route_simulation_seconds: Histogram,
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

/// The pass verdict of the twin as a condition of moving the real robot (see `twin_pass.rs`). The switch itself lives in the state of the server, so
/// it can be changed while the bridge runs (`POST /settings/twin-pass`); this is where it starts and what it is tied to.
#[derive(Debug, Default, Clone)]
pub struct TwinPassConfig {
    /// The value at start.
    pub required: bool,
    /// Started with `--require-twin-pass`: the switch cannot be turned off while the bridge runs.
    pub locked: bool,
    /// `--twin-pass-file`: where a change of the switch is kept (None: only in memory).
    pub file: Option<std::path::PathBuf>,
    /// `--twin-robot-id`: the robot a verdict must be for. Without it the switch cannot be turned on.
    pub robot_id: String,
}

/// Everything `run_with_config` needs besides the socket.
#[derive(Debug, Default, Clone)]
pub struct ServerConfig {
    pub fault_injection: bool,
    pub token: Option<String>,
    /// The signing key of HYDRA-UMC-SERVER (`--identity-key-file`, see `server_identity.rs`). With it a signed token of the server is accepted next to the token, a
    /// viewer may read but not command, and nothing is open, not even `GET /health` or `GET /stats`. None: the earlier rules.
    pub identity_key: Option<Vec<u8>>,
    /// `--health-open`: opens `GET /health`. Closed (the default) the route answers 404 like a route that does not exist; open it answers a
    /// caller on loopback, and any other caller needs the token (a bridge without a token never answers one).
    pub health_open: bool,
    pub interlock: InterlockConfig,
    pub twin_pass: TwinPassConfig,
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
    /// `--collision-shadow`: after a real command is delivered, ask the twin whether it predicts a contact. Information only, it never blocks
    /// anything. Needs the twin link. None: nothing is asked.
    pub collision_shadow: Option<std::sync::Arc<dyn crate::collision_shadow::CollisionShadow>>,
    /// `--collision-shadow-enforce`: the prediction is asked BEFORE a real command is routed, inside the wait budget of the shadow, and the
    /// command is refused when the twin predicts a contact or does not answer in time (never sent unchecked). Off by default: the shadow
    /// then only informs. Needs `collision_shadow`.
    pub shadow_enforce: bool,
    /// `--sim-robot`: the place the real commands (and the stops of the watchdog) are delivered to is the simulated robot of the twin, not a
    /// recording. The interlock, the certificates, the limits and the watchdog work as they do with a robot; only the robot is a model.
    pub sim_robot: Option<std::sync::Arc<crate::twin_link::TwinSimSink>>,
    /// `--safety-zones-url`: the state of the protected zones is asked of HYDRA-UMC-SAFETY-ZONES before a real command is routed, and only a
    /// recent `READY` lets it go on. None: the zones are not part of the decision.
    pub safety_zones: Option<std::sync::Arc<dyn ZonesState>>,
    /// `--safety-zones-max-age-ms`: how old the state of the zones may be (0: `DEFAULT_ZONES_MAX_AGE_MS`).
    pub zones_max_age_ms: u64,
    /// `--record-url`: the commands and measured positions are recorded in the data lake as a session. None: nothing is recorded.
    pub recorder: Option<Arc<Recorder>>,
    /// `--replay-max-points`: the most points `POST /replay` reads from the lake (0: `DEFAULT_REPLAY_MAX_POINTS`).
    pub replay_max_points: usize,
    /// A real order is forwarded only when its request carried the Bearer token or a valid identity of HYDRA-UMC-SERVER; without either it is refused with
    /// `401` (after it is recorded in the audit log), whatever the address it came from. Simulation is not affected. `serve` turns it on unless
    /// `--allow-anonymous-real` is given; the default of the struct is off so that the earlier tests keep their meaning.
    pub require_identity_for_real: bool,
}

/// How old the state of the zones may be when nothing else is said.
pub const DEFAULT_ZONES_MAX_AGE_MS: u64 = 5_000;
/// The most points a replay reads from the data lake when nothing else is said.
pub const DEFAULT_REPLAY_MAX_POINTS: usize = 20_000;

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
    hold_sink: Box<dyn CommandSink + Send>,
    twin_seq: StateSequencer,
    /// The report of a real command that was just delivered, made under the lock and sent after it is released.
    pending_twin: Option<JointStateReport>,
    /// The switch `require_twin_pass` as it is now, and the points of the trajectories already sent under each verdict.
    twin_pass_required: bool,
    twin_pass_ledger: VerdictLedger,
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
        hold_sink: match &config.sim_robot {
            Some(sim) => Box::new((**sim).clone()),
            None => Box::new(RecordingSink::default()),
        },
        twin_seq: StateSequencer::default(),
        pending_twin: None,
        twin_pass_required: config.twin_pass.required,
        twin_pass_ledger: VerdictLedger::default(),
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
            stop_lapsed_leases(leases, hold_sink.as_mut(), audit, stats);
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

    if path == "/health" && request.method() == &Method::Get {
        // before the token gate: the route is closed unless the operator opened it, and then it names the service and its version only
        let peer = request
            .remote_addr()
            .map(|a| a.to_string())
            .unwrap_or_default();
        let presented = header_value(&request, "authorization");
        let reply = if !config.health_open {
            Reply::json(404, json!({"error": "not found"}))
        } else if let Some(key) = &config.identity_key {
            if crate::server_identity::allowed(
                presented.as_deref(),
                request.method().as_str(),
                config.token.as_deref(),
                key,
                crate::server_identity::now_seconds(),
            ) {
                Reply::json(
                    200,
                    json!({"status": "ok", "service": "hydra-umc-hil-bridge", "version": env!("CARGO_PKG_VERSION")}),
                )
            } else {
                Reply::json(401, json!({"error": "a valid identity is required"}))
                    .with("WWW-Authenticate", "Bearer")
            }
        } else if crate::access::peer_is_loopback(&peer)
            || config
                .token
                .as_deref()
                .is_some_and(|t| bearer_matches(presented.as_deref(), t))
        {
            Reply::json(
                200,
                json!({"status": "ok", "service": "hydra-umc-hil-bridge", "version": env!("CARGO_PKG_VERSION")}),
            )
        } else {
            Reply::json(401, json!({"error": "a valid bearer token is required"}))
                .with("WWW-Authenticate", "Bearer")
        };
        reply.send(request);
        return;
    }

    if let Some(key) = &config.identity_key {
        let presented = header_value(&request, "authorization");
        authenticated = crate::server_identity::allowed(
            presented.as_deref(),
            request.method().as_str(),
            config.token.as_deref(),
            key,
            crate::server_identity::now_seconds(),
        );
        if !authenticated {
            shared.lock().stats.unauthorized += 1;
            Reply::json(401, json!({"error": "a valid identity is required"}))
                .with("WWW-Authenticate", "Bearer")
                .send(request);
            return;
        }
    } else if let Some(expected) = &config.token {
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
                "requireTwinPass": state.twin_pass_required,
                "blockedByTwinPass": stats.twin_pass_refused,
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
                "collisionShadow": config.collision_shadow.is_some(),
                "shadowPredictions": stats.shadow_predictions,
                "shadowWouldBlock": stats.shadow_would_block,
                "shadowFailures": stats.shadow_failures,
                "shadowEnforce": config.shadow_enforce && config.collision_shadow.is_some(),
                "shadowBlocked": stats.shadow_blocked,
                "shadowUnavailable": stats.shadow_unavailable,
                "shadowLast": stats.shadow_last,
                "safetyZones": config.safety_zones.is_some(),
                "zonesMaxAgeMs": zones_max_age_ms(config),
                "zonesChecked": stats.zones_checked,
                "zonesBlocked": stats.zones_blocked,
                "zonesUnavailable": stats.zones_unavailable,
                "zonesLast": stats.zones_last,
                "sessionRecord": config.recorder.as_ref().map(|r| r.session()),
                "sessionRecorded": config.recorder.as_ref().map(|r| r.counters.recorded.load(Ordering::Relaxed)),
                "sessionDropped": config.recorder.as_ref().map(|r| r.counters.dropped.load(Ordering::Relaxed)),
                "sessionFailed": config.recorder.as_ref().map(|r| r.counters.failed.load(Ordering::Relaxed)),
                "replayEnabled": config.recorder.as_ref().is_some_and(|r| r.replay),
                "replays": stats.replays,
                "replayFailures": stats.replay_failures,
                "urdfLimits": config.limits.as_ref().map(|l| l.len()),
                "workers": if config.workers == 0 { DEFAULT_WORKERS } else { config.workers },
            })
        };
        Reply::json(200, body).send(request);
        return;
    }
    if path == "/metrics" && request.method() == &Method::Get {
        // the counters in the text format of Prometheus; behind the token like every route but /stats
        let text = {
            let state = shared.lock();
            let stats = &state.stats;
            let mut series = vec![
                Series::counter(
                    "hydra_umc_hil_bridge_routed_real_total",
                    "Real-mode commands that were delivered.",
                    stats.routed_real,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_routed_simulation_total",
                    "Simulation-mode commands that were delivered.",
                    stats.routed_simulation,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_blocked_by_interlock_total",
                    "Real commands the interlock did not let through.",
                    stats.blocked_by_interlock,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_transport_failures_total",
                    "Commands that could not be delivered (timeout, disconnected, refused).",
                    stats.transport_failures,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_mirrored_total",
                    "Commands mirrored into the sink.",
                    stats.mirrored,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_mirror_failures_total",
                    "Mirrored commands that failed.",
                    stats.mirror_failures,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_malformed_requests_total",
                    "Requests refused as malformed.",
                    stats.malformed_requests,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_not_found_total",
                    "Requests to a path that does not exist.",
                    stats.not_found,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_unauthorized_total",
                    "Requests refused for a missing or wrong token.",
                    stats.unauthorized,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_uncertified_total",
                    "Real commands refused for lack of a valid certificate.",
                    stats.uncertified,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_audit_failures_total",
                    "Real orders refused because they could not be recorded.",
                    stats.audit_failures,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_stale_commands_total",
                    "Real commands refused as outside their validity window.",
                    stats.stale_commands,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_outside_limits_total",
                    "Real commands refused as outside the joint limits.",
                    stats.outside_limits,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_watchdog_stops_total",
                    "Joints stopped because their command lease ran out.",
                    stats.watchdog_stops,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_watchdog_stop_failures_total",
                    "Watchdog stops that could not be delivered.",
                    stats.watchdog_stop_failures,
                ),
                Series::gauge(
                    "hydra_umc_hil_bridge_active_leases",
                    "Joints with a valid command lease.",
                    state.leases.active(now_ms()) as f64,
                ),
                Series::gauge(
                    "hydra_umc_hil_bridge_certified_mode",
                    "1 when real commands need a certificate signed by the twin.",
                    if config.interlock.trusted_keys.is_empty() {
                        0.0
                    } else {
                        1.0
                    },
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_twin_reports_total",
                    "State reports sent to the twin.",
                    stats.twin_reports,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_twin_report_failures_total",
                    "State reports to the twin that failed.",
                    stats.twin_report_failures,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_shadow_predictions_total",
                    "Collision predictions asked of the twin in shadow.",
                    stats.shadow_predictions,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_shadow_would_block_total",
                    "Shadow predictions that said a contact was coming.",
                    stats.shadow_would_block,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_shadow_failures_total",
                    "Shadow predictions that could not be asked.",
                    stats.shadow_failures,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_shadow_blocked_total",
                    "Real commands refused because the twin predicted a contact (enforced shadow).",
                    stats.shadow_blocked,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_shadow_unavailable_total",
                    "Real commands refused because the twin gave no prediction in time (enforced shadow).",
                    stats.shadow_unavailable,
                ),
            ];
            let refused = |reason, value: u64| {
                Series::labelled_counter(
                    "hydra_umc_hil_bridge_refused_total",
                    "Real commands that were not forwarded, by the reason.",
                    ("reason", reason),
                    value,
                )
            };
            series.extend([
                refused("interlock", stats.blocked_by_interlock),
                refused("certificate", stats.uncertified),
                refused("stale", stats.stale_commands),
                refused("limits", stats.outside_limits),
                refused("collision_shadow", stats.shadow_blocked),
                refused("collision_shadow_unavailable", stats.shadow_unavailable),
                refused("safety_zones", stats.zones_blocked),
                refused("safety_zones_unavailable", stats.zones_unavailable),
                refused("audit", stats.audit_failures),
                Series::counter(
                    "hydra_umc_hil_bridge_zones_checked_total",
                    "Times the state of the protected zones was asked before a real command.",
                    stats.zones_checked,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_replays_total",
                    "Recorded sessions that were run again in simulation.",
                    stats.replays,
                ),
                Series::counter(
                    "hydra_umc_hil_bridge_replay_failures_total",
                    "Replays that could not read the session from the data lake.",
                    stats.replay_failures,
                ),
            ]);
            if let Some(recorder) = &config.recorder {
                series.extend([
                    Series::counter(
                        "hydra_umc_hil_bridge_session_recorded_total",
                        "Samples of the session that the data lake accepted.",
                        recorder.counters.recorded.load(Ordering::Relaxed),
                    ),
                    Series::counter(
                        "hydra_umc_hil_bridge_session_dropped_total",
                        "Events of the session dropped because the queue to the data lake was full.",
                        recorder.counters.dropped.load(Ordering::Relaxed),
                    ),
                    Series::counter(
                        "hydra_umc_hil_bridge_session_failed_total",
                        "Samples of the session that the data lake did not accept.",
                        recorder.counters.failed.load(Ordering::Relaxed),
                    ),
                ]);
            }
            if let Some(in_sync) = stats.twin_in_sync {
                series.push(Series::gauge(
                    "hydra_umc_hil_bridge_twin_in_sync",
                    "1 when the last state report reached the twin.",
                    if in_sync { 1.0 } else { 0.0 },
                ));
            }
            crate::metrics::exposition_with_histograms(
                env!("CARGO_PKG_VERSION"),
                &series,
                &[
                    HistogramSeries {
                        name: "hydra_umc_hil_bridge_route_seconds",
                        help: "Time to answer POST /route, including the questions to the twin and to the zone monitor.",
                        label: ("mode", "real"),
                        histogram: &stats.route_real_seconds,
                    },
                    HistogramSeries {
                        name: "hydra_umc_hil_bridge_route_seconds",
                        help: "Time to answer POST /route, including the questions to the twin and to the zone monitor.",
                        label: ("mode", "simulation"),
                        histogram: &stats.route_simulation_seconds,
                    },
                ],
            )
        };
        let _ = request.respond(
            Response::from_string(text)
                .with_status_code(200)
                .with_header(header(
                    "Content-Type",
                    "text/plain; version=0.0.4; charset=utf-8",
                )),
        );
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
    if path == "/settings/twin-pass" && request.method() == &Method::Get {
        let presented = header_value(&request, "authorization");
        handle_twin_pass_settings(&Method::Get, "", shared, presented.as_deref()).send(request);
        return;
    }
    if request.method() != &Method::Post {
        shared.lock().stats.not_found += 1;
        if matches!(
            path.as_str(),
            "/route"
                | "/mirror"
                | "/measured"
                | "/replay"
                | "/stats"
                | "/audit"
                | "/settings/twin-pass"
        ) {
            Reply::json(405, json!({"error": "method not allowed"}))
                .with(
                    "Allow",
                    if path == "/stats" || path == "/audit" {
                        "GET"
                    } else if path == "/settings/twin-pass" {
                        "GET, POST"
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
    // Enforced shadow: the twin is asked before the command is routed (outside the lock); a refusal never reaches the interlock or the robot.
    let route_started = (path == "/route").then(std::time::Instant::now);
    // The zone monitor first (a cheap question to a service on the same machine), then the twin; either one refusing ends the request.
    let refused_by_zones = if path == "/route" {
        config
            .safety_zones
            .as_ref()
            .and_then(|zones| zones_precheck(shared, zones.as_ref(), &raw, &info))
    } else {
        None
    };
    let refused_by_shadow =
        if refused_by_zones.is_none() && path == "/route" && config.shadow_enforce {
            config
                .collision_shadow
                .as_ref()
                .and_then(|shadow| shadow_precheck(shared, shadow.as_ref(), &raw, &info))
        } else {
            None
        };
    let reply = if let Some(refused) = refused_by_zones.or(refused_by_shadow) {
        refused
    } else {
        match path.as_str() {
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
                            sim_robot: config.sim_robot.as_deref(),
                            recorder: config.recorder.as_deref(),
                            twin_pass_robot_id: &config.twin_pass.robot_id,
                            require_identity_for_real: config.require_identity_for_real,
                        },
                        &info,
                    );
                    (reply, state.pending_twin.take())
                };
                // The twin is told AFTER the lock is released: the call can take as long as its timeout, and the command already went.
                match pending {
                    Some(report) => {
                        let outcome = report_to_twin(shared, &report, &info);
                        let mut reply = reply.with("X-Twin-Report", outcome.kind());
                        // then, if asked for, what the twin predicts for the joint that just moved (information only)
                        // (with the shadow enforced the twin was already asked before the command went)
                        if let Some(shadow) = config
                            .collision_shadow
                            .as_ref()
                            .filter(|_| !config.shadow_enforce)
                        {
                            let asked = shadow_collision(shared, shadow.as_ref(), &report, &info);
                            reply = reply.with("X-Collision-Shadow", asked.kind());
                        }
                        reply
                    }
                    None => reply,
                }
            }
            "/measured" => handle_measured(&raw, shared, &info),
            "/settings/twin-pass" => handle_twin_pass_settings(
                &Method::Post,
                &raw,
                shared,
                header_value(&request, "authorization").as_deref(),
            ),
            "/replay" if config.recorder.as_ref().is_some_and(|r| r.replay) => {
                handle_replay(&raw, shared)
            }
            "/replay" => {
                shared.lock().stats.not_found += 1;
                Reply::json(
                    404,
                    json!({"error": "replay is off: start the bridge with --record-url and --replay"}),
                )
            }
            "/mirror" => handle_mirror(&raw, &mut shared.lock().stats),
            _ => {
                shared.lock().stats.not_found += 1;
                Reply::json(404, json!({"error": "not found"}))
            }
        }
    };
    if let Some(started) = route_started {
        let seconds = started.elapsed().as_secs_f64();
        let mut state = shared.lock();
        if route_mode_is_real(&raw) {
            state.stats.route_real_seconds.observe(seconds);
        } else {
            state.stats.route_simulation_seconds.observe(seconds);
        }
    }
    reply.send(request);
}

/// Whether a `/route` body asks for the real mode (anything that cannot be read counts as the simulation: it never reaches the hardware).
fn route_mode_is_real(raw: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| {
            v.get("mode")
                .and_then(serde_json::Value::as_str)
                .map(|m| m == "real")
        })
        .unwrap_or(false)
}

fn zones_max_age_ms(config: &ServerConfig) -> u64 {
    if config.zones_max_age_ms == 0 {
        DEFAULT_ZONES_MAX_AGE_MS
    } else {
        config.zones_max_age_ms
    }
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
    /// `--sim-robot`: the commands are delivered to the simulated robot of the twin.
    sim_robot: Option<&'a crate::twin_link::TwinSimSink>,
    /// `--record-url`: where the decided commands are recorded.
    recorder: Option<&'a Recorder>,
    /// The robot a pass verdict of the twin must be for (`--twin-robot-id`).
    twin_pass_robot_id: &'a str,
    /// A real order needs an authenticated request (see `ServerConfig::require_identity_for_real`).
    require_identity_for_real: bool,
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
        twin_pass_required,
        twin_pass_ledger,
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
    // The point of the passed trajectory this command matched, when the switch is on (remembered once the command went).
    let mut twin_pass_matched: Option<(String, u64, usize)> = None;

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

    // Without the token or the identity of the server the caller is only an address: a real order is not forwarded (it was recorded above).
    if req.mode == Mode::Real && context.require_identity_for_real && !info.authenticated {
        stats.unauthorized += 1;
        return Reply::json(
            401,
            json!({"error": "a real order needs the Bearer token or a valid identity of HYDRA-UMC-SERVER; it was not forwarded (--allow-anonymous-real is the bench setting)"}),
        )
        .with("WWW-Authenticate", "Bearer");
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
    let mut twin_sim = context.sim_robot.cloned();
    let real_sink: &mut dyn CommandSink = if use_simulated {
        &mut simulated_real
    } else if let Some(sim) = twin_sim.as_mut() {
        sim
    } else {
        &mut recording_real
    };
    let mut recording_sim = RecordingSink::default();
    let mut twin_sim_for_simulation = context.sim_robot.cloned();
    let sim_sink: &mut dyn CommandSink = match twin_sim_for_simulation.as_mut() {
        Some(sim) => sim,
        None => &mut recording_sim,
    };

    let outcome = match req.mode {
        Mode::Simulation => bridge.route_command(command.clone(), req.risk, real_sink, sim_sink),
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
                // With the switch on, the trajectory of the command must have a pass verdict of the twin. It is looked at before the certificate, so a
                // command without it does not use up the certificate it carries; the point of the trajectory is marked only when the command went.
                let twin_pass_check = if *twin_pass_required {
                    Some(crate::twin_pass::check(
                        req.twin_pass.as_ref(),
                        &command,
                        &context.interlock.trusted_keys,
                        context.twin_pass_robot_id,
                        now_ms(),
                        twin_pass_ledger,
                    ))
                } else {
                    None
                };
                twin_pass_matched = match &twin_pass_check {
                    Some(Ok(matched)) => {
                        Some((matched.nonce.clone(), matched.valid_until_ms, matched.index))
                    }
                    _ => None,
                };
                let report = if let Err(reason) =
                    leases.check(&command.joint, validity.as_ref(), now_ms())
                {
                    stats.stale_commands += 1;
                    Err(reason)
                } else if let Some(Err(reason)) = twin_pass_check {
                    stats.twin_pass_refused += 1;
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
                    Ok(report) => {
                        bridge.route_command(command.clone(), Some(report), real_sink, sim_sink)
                    }
                    Err(reason) => {
                        stats.uncertified += 1;
                        RouteOutcome::BlockedByInterlock { reason }
                    }
                }
            }
        }
    };
    if req.mode == Mode::Real && outcome == RouteOutcome::SentReal {
        if let Some((nonce, valid_until_ms, index)) = &twin_pass_matched {
            twin_pass_ledger.mark(nonce, *valid_until_ms, *index, now_ms());
        }
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
    if let Some(recorder) = context.recorder {
        recorder.record(
            Event::Command {
                joint: command.joint.clone(),
                position: command.position,
                real: req.mode == Mode::Real,
                outcome: crate::session_record::outcome_code(&outcome),
            },
            now_ms(),
        );
    }
    eprintln!("{}", outcome.log_line(req.mode));
    Reply::json(200, serde_json::to_value(&outcome).unwrap())
        .with("X-Outcome-Kind", outcome.kind())
        .with(
            "X-Delivered",
            if outcome.delivered() { "true" } else { "false" },
        )
}

/// The body of `POST /settings/twin-pass`: the new value of the switch and the confirmation, nothing else.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TwinPassSettingsRequest {
    #[serde(rename = "requireTwinPass")]
    require_twin_pass: bool,
    confirm: Option<bool>,
}

/// The switch `require_twin_pass` as the settings route reports it.
fn twin_pass_view(
    config: &ServerConfig,
    required: bool,
    changed: Option<bool>,
) -> serde_json::Value {
    let mut view = json!({
        "requireTwinPass": required,
        "locked": config.twin_pass.locked,
        "persisted": config.twin_pass.file.is_some(),
        "ready": !config.interlock.trusted_keys.is_empty() && !config.twin_pass.robot_id.is_empty(),
        "robotIdSet": !config.twin_pass.robot_id.is_empty(),
        "keysConfigured": !config.interlock.trusted_keys.is_empty(),
        "canChange": config.identity_key.is_some(),
    });
    if let (Some(changed), Some(map)) = (changed, view.as_object_mut()) {
        map.insert("changed".to_string(), json!(changed));
    }
    view
}

/// Keeps the value of the switch where `--twin-pass-file` says, whole or not at all.
fn write_twin_pass_file(path: &std::path::Path, required: bool) -> std::io::Result<()> {
    let text = json!({"require_twin_pass": required}).to_string();
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)
}

/// `GET` and `POST /settings/twin-pass`. Reading is for whoever may reach the bridge. Changing is for an administrator of HYDRA-UMC-SERVER
/// (a valid signed token with the role `admin`, which needs `--identity-key-file`), needs `"confirm": true` in either direction, cannot turn
/// the switch off when the bridge was started with `--require-twin-pass`, and cannot turn it on while the twin key or the robot is not set
/// (every real command would be refused for a reason the operator could not fix from here).
fn handle_twin_pass_settings(
    method: &Method,
    raw: &str,
    shared: &Shared,
    authorization: Option<&str>,
) -> Reply {
    let config = &shared.config;
    if method == &Method::Get {
        let required = shared.lock().twin_pass_required;
        return Reply::json(200, twin_pass_view(config, required, None));
    }
    let token = authorization
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| *scheme == "Bearer")
        .map(|(_, token)| token.trim());
    let is_admin = match (&config.identity_key, token) {
        (Some(key), Some(token)) => {
            crate::server_identity::verify_token(token, key, crate::server_identity::now_seconds())
                .map(|identity| identity.role == "admin")
                .unwrap_or(false)
        }
        _ => false,
    };
    if !is_admin {
        shared.lock().stats.unauthorized += 1;
        return Reply::json(
            403,
            json!({"error": "changing this switch needs an administrator of HYDRA-UMC-SERVER (a signed token with the role admin; start the bridge with --identity-key-file)"}),
        );
    }
    let request: TwinPassSettingsRequest = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(_) => {
            shared.lock().stats.malformed_requests += 1;
            return Reply::json(
                400,
                json!({"error": "the body must be an object with requireTwinPass (true or false) and confirm: true"}),
            );
        }
    };
    if request.confirm != Some(true) {
        return Reply::json(
            400,
            json!({"error": "changing this switch needs confirm: true", "code": "CONFIRMATION_REQUIRED"}),
        );
    }
    let mut state = shared.lock();
    let before = state.twin_pass_required;
    if !request.require_twin_pass && config.twin_pass.locked {
        return Reply::json(
            409,
            json!({"error": "the bridge was started with --require-twin-pass: the switch cannot be turned off while it runs", "code": "SETTING_REFUSED"}),
        );
    }
    if request.require_twin_pass
        && (config.interlock.trusted_keys.is_empty() || config.twin_pass.robot_id.is_empty())
    {
        return Reply::json(
            409,
            json!({"error": "turning the switch on needs the twin key (--twin-public-key-file) and the robot (--twin-robot-id)", "code": "SETTING_REFUSED"}),
        );
    }
    if let Some(path) = &config.twin_pass.file {
        if let Err(e) = write_twin_pass_file(path, request.require_twin_pass) {
            eprintln!("[hil-bridge] could not keep the switch require_twin_pass: {e}");
            return Reply::json(
                409,
                json!({"error": "the value could not be kept in the file of --twin-pass-file, so it was not changed", "code": "SETTING_REFUSED"}),
            );
        }
    }
    state.twin_pass_required = request.require_twin_pass;
    eprintln!(
        "[hil-bridge] require_twin_pass is now {} (it was {})",
        request.require_twin_pass, before
    );
    Reply::json(
        200,
        twin_pass_view(
            config,
            request.require_twin_pass,
            Some(before != request.require_twin_pass),
        ),
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

/// Asks the twin what it predicts for the command that just went (outside the lock), counts the answer and leaves an audit record. Nothing
/// here can change what happened to the command or what happens to the next one.
fn shadow_collision(
    shared: &Shared,
    shadow: &dyn crate::collision_shadow::CollisionShadow,
    report: &JointStateReport,
    info: &RequestInfo,
) -> crate::collision_shadow::ShadowOutcome {
    use crate::collision_shadow::ShadowOutcome;
    let outcome = crate::collision_shadow::ask(shadow, &report.joint, report.position);
    let mut state = shared.lock();
    state.stats.shadow_last = Some(outcome.kind());
    let reason = match &outcome {
        ShadowOutcome::Answered(verdict) => {
            state.stats.shadow_predictions += 1;
            if verdict.collision {
                state.stats.shadow_would_block += 1;
                eprintln!(
                    "[hil-bridge] shadow: the twin predicts a contact for joint {:?} at {} ({}); the command was not blocked",
                    report.joint,
                    report.position,
                    verdict.summary()
                );
            }
            Some(verdict.summary())
        }
        ShadowOutcome::Failed { reason } => {
            state.stats.shadow_failures += 1;
            eprintln!(
                "[hil-bridge] shadow: the twin gave no prediction for joint {:?}: {reason}",
                report.joint
            );
            Some(reason.clone())
        }
    };
    let entry = AuditEntry {
        at_ms: now_ms(),
        phase: "collision_shadow",
        caller: info.caller.clone(),
        peer: info.peer.clone(),
        authenticated: info.authenticated,
        mode: "real",
        joint: report.joint.clone(),
        position: report.position,
        certificate_key_id: None,
        certificate_nonce: None,
        kind: Some(outcome.kind()),
        delivered: None,
        reason,
    };
    if let Err(e) = state.audit.record(entry) {
        state.stats.audit_failures += 1;
        eprintln!("[hil-bridge] audit failure for a shadow prediction: {e}");
    }
    outcome
}

/// Enforced shadow: asks the twin about a real command BEFORE it is routed. `None` lets the command go on to the interlock (not a real command
/// we can read, or the twin predicts no contact); `Some(reply)` refuses it: `409` for a predicted contact, `503` when the twin gave no prediction
/// inside the wait budget. A command is never sent unchecked while the shadow is enforced.
fn shadow_precheck(
    shared: &Shared,
    shadow: &dyn crate::collision_shadow::CollisionShadow,
    raw: &str,
    info: &RequestInfo,
) -> Option<Reply> {
    use crate::collision_shadow::ShadowOutcome;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    if value.get("mode").and_then(serde_json::Value::as_str) != Some("real") {
        return None;
    }
    let joint = value.get("joint").and_then(serde_json::Value::as_str)?;
    let position = value
        .get("position")
        .and_then(serde_json::Value::as_f64)
        .filter(|p| p.is_finite())?;
    let outcome = crate::collision_shadow::ask(shadow, joint, position);
    let mut state = shared.lock();
    state.stats.shadow_last = Some(outcome.kind());
    let (refusal, reason, kind) = match &outcome {
        ShadowOutcome::Answered(verdict) => {
            state.stats.shadow_predictions += 1;
            if verdict.collision {
                state.stats.shadow_would_block += 1;
                state.stats.shadow_blocked += 1;
                eprintln!(
                    "[hil-bridge] shadow: command for joint {joint:?} at {position} refused, the twin predicts a contact ({})",
                    verdict.summary()
                );
                (
                    Some(Reply::json(
                        409,
                        json!({
                            "error": "the twin predicts a contact for this command, so it was not sent",
                            "blockedBy": "collision_shadow",
                            "twin": verdict.summary(),
                        }),
                    )),
                    verdict.summary(),
                    "shadow_blocked",
                )
            } else {
                (None, verdict.summary(), "clear")
            }
        }
        ShadowOutcome::Failed { reason } => {
            state.stats.shadow_failures += 1;
            state.stats.shadow_unavailable += 1;
            eprintln!(
                "[hil-bridge] shadow: command for joint {joint:?} refused, the twin gave no prediction in time: {reason}"
            );
            (
                Some(Reply::json(
                    503,
                    json!({
                        "error": "the twin gave no collision prediction in time, so the command was not sent",
                        "blockedBy": "collision_shadow_unavailable",
                    }),
                )),
                reason.clone(),
                "shadow_unavailable",
            )
        }
    };
    let entry = AuditEntry {
        at_ms: now_ms(),
        phase: "collision_shadow",
        caller: info.caller.clone(),
        peer: info.peer.clone(),
        authenticated: info.authenticated,
        mode: "real",
        joint: joint.to_string(),
        position,
        certificate_key_id: None,
        certificate_nonce: None,
        kind: Some(kind),
        delivered: refusal.is_some().then_some(false),
        reason: Some(reason),
    };
    if let Err(e) = state.audit.record(entry) {
        state.stats.audit_failures += 1;
        eprintln!("[hil-bridge] audit failure for a shadow check: {e}");
    }
    drop(state);
    if refusal.is_some() {
        record_refusal(&shared.config, joint, position);
    }
    refusal
}

/// A real command refused before it was routed is part of the session too.
fn record_refusal(config: &ServerConfig, joint: &str, position: f64) {
    if let Some(recorder) = &config.recorder {
        recorder.record(
            Event::Command {
                joint: joint.to_string(),
                position,
                real: true,
                outcome: crate::session_record::OUTCOME_REFUSED_BEFORE_ROUTING,
            },
            now_ms(),
        );
    }
}

/// The zone monitor as a condition of the interlock: asks `GET /state` BEFORE a real command is routed (outside the lock). `None` lets the
/// command go on to the rest of the interlock (not a real command we can read, or the zones are clear and the state is recent); `Some(reply)`
/// refuses it: `409` when the zones are not clear, `503` when the monitor gave no usable state in time. A real command is never forwarded on
/// the silence of the monitor.
fn zones_precheck(
    shared: &Shared,
    zones: &dyn ZonesState,
    raw: &str,
    info: &RequestInfo,
) -> Option<Reply> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    if value.get("mode").and_then(serde_json::Value::as_str) != Some("real") {
        return None;
    }
    let joint = value.get("joint").and_then(serde_json::Value::as_str)?;
    let position = value
        .get("position")
        .and_then(serde_json::Value::as_f64)
        .filter(|p| p.is_finite())?;
    let verdict = zones.state();
    let decision =
        crate::safety_zones::assess(&verdict, now_ms(), zones_max_age_ms(&shared.config));
    let mut state = shared.lock();
    state.stats.zones_checked += 1;
    let (refusal, kind, reason) = match &decision {
        ZonesDecision::Allow => {
            state.stats.zones_last = Some("clear");
            (None, "clear", "READY".to_string())
        }
        ZonesDecision::Blocked { reason } => {
            state.stats.zones_blocked += 1;
            state.stats.zones_last = Some("blocked");
            eprintln!("[hil-bridge] safety zones: command for joint {joint:?} at {position} refused: {reason}");
            (
                Some(Reply::json(
                    409,
                    json!({
                        "error": "the protected zones are not clear, so the command was not sent",
                        "blockedBy": "safety_zones",
                        "zones": reason,
                    }),
                )),
                "zones_blocked",
                reason.clone(),
            )
        }
        ZonesDecision::Unavailable { reason } => {
            state.stats.zones_unavailable += 1;
            state.stats.zones_last = Some("unavailable");
            eprintln!("[hil-bridge] safety zones: command for joint {joint:?} at {position} refused, no usable state: {reason}");
            (
                Some(Reply::json(
                    503,
                    json!({
                        "error": "the zone monitor gave no recent state, so the command was not sent",
                        "blockedBy": "safety_zones_unavailable",
                    }),
                )),
                "zones_unavailable",
                reason.clone(),
            )
        }
    };
    let entry = AuditEntry {
        at_ms: now_ms(),
        phase: "safety_zones",
        caller: info.caller.clone(),
        peer: info.peer.clone(),
        authenticated: info.authenticated,
        mode: "real",
        joint: joint.to_string(),
        position,
        certificate_key_id: None,
        certificate_nonce: None,
        kind: Some(kind),
        delivered: refusal.is_some().then_some(false),
        reason: Some(reason),
    };
    if let Err(e) = state.audit.record(entry) {
        state.stats.audit_failures += 1;
        eprintln!("[hil-bridge] audit failure for a zones check: {e}");
    }
    drop(state);
    if refusal.is_some() {
        record_refusal(&shared.config, joint, position);
    }
    refusal
}

#[derive(Deserialize, Default)]
struct ReplayRequest {
    /// The session to read back (default: the one this bridge is recording).
    session: Option<String>,
    from_ms: Option<u64>,
    to_ms: Option<u64>,
}

/// `POST /replay`: reads a recorded session back from the data lake and runs its commands again in SIMULATION mode, whatever mode they were
/// recorded in: a replay never reaches the real hardware. Each command is also compared with the limits of the URDF the bridge has now, so a
/// change of the robot description shows which recorded commands would no longer be accepted.
fn handle_replay(raw: &str, shared: &Shared) -> Reply {
    let Some(recorder) = shared.config.recorder.as_ref() else {
        return Reply::json(404, json!({"error": "no session store configured"}));
    };
    let req: ReplayRequest = if raw.trim().is_empty() {
        ReplayRequest::default()
    } else {
        match serde_json::from_str(raw) {
            Ok(r) => r,
            Err(e) => {
                shared.lock().stats.malformed_requests += 1;
                return Reply::json(
                    400,
                    json!({"error": format!("malformed request JSON: {e}")}),
                );
            }
        }
    };
    let session = req
        .session
        .clone()
        .unwrap_or_else(|| recorder.session().to_string());
    if !crate::session_record::valid_session(&session) {
        shared.lock().stats.malformed_requests += 1;
        return Reply::json(
            400,
            json!({"error": "the session name may only have letters, digits, '.', '_' and '-' (up to 64)"}),
        );
    }
    let max_points = if shared.config.replay_max_points == 0 {
        DEFAULT_REPLAY_MAX_POINTS
    } else {
        shared.config.replay_max_points
    };
    // The read of the lake happens outside the lock: it can take as long as its timeout.
    let commands = match crate::session_record::fetch_commands(
        recorder.lake(),
        &session,
        req.from_ms,
        req.to_ms,
        max_points,
    ) {
        Ok(commands) => commands,
        Err(reason) => {
            shared.lock().stats.replay_failures += 1;
            return Reply::json(502, json!({"error": reason}));
        }
    };
    let bridge = Bridge::new(Mode::Simulation);
    let mut real_unused = RecordingSink::default();
    let mut twin_sim = shared.config.sim_robot.as_deref().cloned();
    let mut recording = RecordingSink::default();
    let sim_sink: &mut dyn CommandSink = match twin_sim.as_mut() {
        Some(sim) => sim,
        None => &mut recording,
    };
    let mut by_outcome: std::collections::BTreeMap<&'static str, u64> = Default::default();
    let (mut real, mut delivered, mut failed, mut regressions) = (0u64, 0u64, 0u64, 0u64);
    let mut outside = Vec::new();
    for recorded in &commands {
        if recorded.real {
            real += 1;
        }
        *by_outcome
            .entry(
                recorded
                    .outcome
                    .map(crate::session_record::outcome_name)
                    .unwrap_or("unknown"),
            )
            .or_insert(0) += 1;
        let command = JointCommand {
            joint: recorded.joint.clone(),
            position: recorded.position,
        };
        if command.validate().is_err() {
            failed += 1;
            continue;
        }
        if let Some(limits) = &shared.config.limits {
            if let Err(reason) = limits.check(&command.joint, command.position) {
                if recorded.outcome == Some(0) {
                    regressions += 1;
                }
                if outside.len() < 100 {
                    outside.push(json!({
                        "atMs": recorded.at_ms,
                        "joint": recorded.joint,
                        "position": recorded.position,
                        "recordedOutcome": recorded.outcome.map(crate::session_record::outcome_name),
                        "reason": reason,
                    }));
                }
            }
        }
        match bridge.route_command(command, None, &mut real_unused, sim_sink) {
            RouteOutcome::SentSimulation => delivered += 1,
            _ => failed += 1,
        }
    }
    shared.lock().stats.replays += 1;
    Reply::json(
        200,
        json!({
            "session": session,
            "commands": commands.len(),
            "recordedAsReal": real,
            "byOutcome": by_outcome,
            "replayedToSimulation": delivered,
            "simulationFailures": failed,
            "limitsChecked": shared.config.limits.is_some(),
            "outsideLimits": outside,
            "regressions": regressions,
            "firstAtMs": commands.first().map(|c| c.at_ms),
            "lastAtMs": commands.last().map(|c| c.at_ms),
            "realHardwareTouched": false,
        }),
    )
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
    if let Some(recorder) = &shared.config.recorder {
        recorder.record(
            Event::Measured {
                joint: command.joint.clone(),
                position: command.position,
            },
            now_ms(),
        );
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
    fn the_metrics_are_prometheus_text_behind_the_token_and_follow_what_the_bridge_does() {
        let port = start_test_server_with_token();
        assert_eq!(get_status_with_headers(port, "/metrics", ""), 401);
        let auth = format!("Authorization: Bearer {TEST_TOKEN}\r\n");
        let (status, first) = get_with(port, "/metrics", &auth);
        assert_eq!(status, 200, "{first}");
        crate::metrics::tests::assert_valid_exposition(&first);
        assert!(first.contains("hydra_umc_hil_bridge_info{version=\""));
        assert!(first.contains("hydra_umc_hil_bridge_routed_simulation_total 0"));
        assert!(!first.contains(TEST_TOKEN));

        let body = r#"{"mode":"simulation","joint":"shoulder","position":0.5}"#;
        let headers = format!("{auth}Content-Type: application/json\r\n");
        assert_eq!(post_raw(port, "/route", &headers, body.as_bytes()), 200);
        let (_, second) = get_with(port, "/metrics", &auth);
        crate::metrics::tests::assert_valid_exposition(&second);
        assert!(
            second.contains("hydra_umc_hil_bridge_routed_simulation_total 1"),
            "{second}"
        );
        assert!(
            second.contains("hydra_umc_hil_bridge_unauthorized_total 1"),
            "{second}"
        );
        // no joint, position or certificate is ever a metric
        assert!(
            !second.contains("shoulder") && !second.contains("position"),
            "{second}"
        );
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

    /// A token of HYDRA-UMC-SERVER signed with `key`, valid for ten minutes.
    fn server_token(key: &[u8], role: &str) -> String {
        fn b64(data: &[u8]) -> String {
            const TABLE: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in data.chunks(3) {
                let n = chunk
                    .iter()
                    .enumerate()
                    .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
                for i in 0..=chunk.len() {
                    out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
                }
            }
            out
        }
        let head = b64(br#"{"alg":"HS256","typ":"JWT"}"#);
        let exp = crate::server_identity::now_seconds() as u64 + 600;
        let body = b64(format!(r#"{{"username":"ana","role":"{role}","exp":{exp}}}"#).as_bytes());
        let mac = crate::hmac::hmac_sha256(key, format!("{head}.{body}").as_bytes());
        format!("{head}.{body}.{}", b64(&mac))
    }

    fn identity_required_config() -> ServerConfig {
        ServerConfig {
            require_identity_for_real: true,
            interlock: InterlockConfig {
                allow_unsigned_risk: true,
                ..InterlockConfig::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn a_real_order_without_a_token_or_an_identity_is_refused_but_simulation_is_not() {
        let port = start_with(identity_required_config());
        let real = r#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":false,"distance_m":2.0}}"#;
        let (status, resp) = post_with(port, "/route", "", real);
        assert_eq!(status, 401);
        assert!(resp.contains("not forwarded"), "{resp}");
        let simulation = r#"{"mode":"simulation","joint":"shoulder","position":1.0}"#;
        assert_eq!(post_with(port, "/route", "", simulation).0, 200);
        // the order was recorded before it was refused, and nothing was routed to the robot
        let (_, stats) = get_with(port, "/stats", "");
        assert!(stats.contains("\"routedReal\":0"), "{stats}");
        assert!(stats.contains("\"unauthorized\":1"), "{stats}");
    }

    #[test]
    fn a_real_order_with_the_token_goes_on_to_the_interlock() {
        let mut config = identity_required_config();
        config.token = Some(TEST_TOKEN.to_string());
        let port = start_with(config);
        let real = r#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":false,"distance_m":2.0}}"#;
        let auth = format!("Authorization: Bearer {TEST_TOKEN}\r\n");
        let (status, resp) = post_with(port, "/route", &auth, real);
        assert_eq!(status, 200);
        assert!(resp.contains("SentReal"), "{resp}");
    }

    #[test]
    fn a_real_order_with_the_identity_of_the_server_goes_on_and_a_viewer_never_does() {
        let key = b"0123456789abcdef0123456789abcdef".to_vec();
        let mut config = identity_required_config();
        config.identity_key = Some(key.clone());
        let port = start_with(config);
        let real = r#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":false,"distance_m":2.0}}"#;
        let admin = format!(
            "Authorization: Bearer {}\r\n",
            server_token(&key, "operator")
        );
        let (status, resp) = post_with(port, "/route", &admin, real);
        assert_eq!(status, 200);
        assert!(resp.contains("SentReal"), "{resp}");
        let viewer = format!("Authorization: Bearer {}\r\n", server_token(&key, "viewer"));
        assert_eq!(post_with(port, "/route", &viewer, real).0, 401);
    }

    #[test]
    fn the_bench_setting_leaves_the_earlier_behaviour() {
        let mut config = identity_required_config();
        config.require_identity_for_real = false;
        let port = start_with(config);
        let real = r#"{"mode":"real","joint":"shoulder","position":1.0,"risk":{"collision_imminent":false,"distance_m":2.0}}"#;
        let (status, resp) = post_with(port, "/route", "", real);
        assert_eq!(status, 200);
        assert!(resp.contains("SentReal"), "{resp}");
    }

    #[test]
    fn with_the_identity_of_the_server_nothing_is_open_and_a_viewer_only_reads() {
        let key = b"0123456789abcdef0123456789abcdef".to_vec();
        let port = start_with(ServerConfig {
            identity_key: Some(key.clone()),
            health_open: true,
            ..Default::default()
        });
        let body = r#"{"mode":"simulation","joint":"shoulder","position":0.5}"#;
        for path in ["/stats", "/health"] {
            assert_eq!(get_status_with_headers(port, path, ""), 401, "GET {path}");
            let viewer = format!("Authorization: Bearer {}\r\n", server_token(&key, "viewer"));
            assert_eq!(
                get_status_with_headers(port, path, &viewer),
                200,
                "GET {path} as a viewer"
            );
        }
        let viewer = format!(
            "Content-Type: application/json\r\nAuthorization: Bearer {}\r\n",
            server_token(&key, "viewer")
        );
        assert_eq!(post_raw(port, "/route", &viewer, body.as_bytes()), 401);
        let operator = format!(
            "Content-Type: application/json\r\nAuthorization: Bearer {}\r\n",
            server_token(&key, "operator")
        );
        assert_eq!(post_raw(port, "/route", &operator, body.as_bytes()), 200);
        let forged = format!(
            "Authorization: Bearer {}\r\n",
            server_token(b"another-key-of-more-than-32-characters!", "admin")
        );
        assert_eq!(get_status_with_headers(port, "/stats", &forged), 401);
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

    // ---- the collision prediction asked in shadow (see collision_shadow.rs) ----

    fn shadow_config(
        twin: &std::sync::Arc<crate::twin_link::RecordingStateSink>,
        shadow: &std::sync::Arc<crate::collision_shadow::ScriptedShadow>,
    ) -> ServerConfig {
        let shadow: std::sync::Arc<dyn crate::collision_shadow::CollisionShadow> = shadow.clone();
        ServerConfig {
            collision_shadow: Some(shadow),
            ..twin_config(twin)
        }
    }

    /// The raw answer of a POST, headers included.
    fn post_full(port: u16, path: &str, body: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect must succeed");
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        raw
    }

    #[test]
    fn characterization_without_the_shadow_nothing_is_asked_and_the_reply_is_as_before() {
        let sink = std::sync::Arc::new(crate::twin_link::RecordingStateSink::connected());
        let port = start_with(twin_config(&sink));
        let raw = post_full(
            port,
            "/route",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        assert!(raw.contains("SentReal"), "{raw}");
        assert!(
            raw.to_ascii_lowercase()
                .contains("x-twin-report: delivered"),
            "{raw}"
        );
        assert!(
            !raw.to_ascii_lowercase().contains("x-collision-shadow"),
            "{raw}"
        );
        let stats = stats_json(port);
        assert_eq!(stats["collisionShadow"], false);
        assert_eq!(stats["shadowPredictions"], 0);
        assert_eq!(stats["shadowLast"], serde_json::Value::Null);
    }

    #[test]
    fn a_predicted_contact_is_recorded_and_never_blocks_the_command() {
        let twin = std::sync::Arc::new(crate::twin_link::RecordingStateSink::connected());
        let shadow = std::sync::Arc::new(crate::collision_shadow::ScriptedShadow::answering(true));
        let port = start_with(shadow_config(&twin, &shadow));
        let raw = post_full(
            port,
            "/route",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        // the command went, exactly as it would without the shadow
        assert!(raw.contains("SentReal"), "{raw}");
        assert!(
            raw.to_ascii_lowercase().contains("x-delivered: true"),
            "{raw}"
        );
        assert!(
            raw.to_ascii_lowercase()
                .contains("x-collision-shadow: collision"),
            "{raw}"
        );
        assert_eq!(shadow.questions(), vec![("shoulder".to_string(), 1.0)]);
        let stats = stats_json(port);
        assert_eq!(stats["routedReal"], 1);
        assert_eq!(stats["blockedByInterlock"], 0);
        assert_eq!(stats["collisionShadow"], true);
        assert_eq!(stats["shadowPredictions"], 1);
        assert_eq!(stats["shadowWouldBlock"], 1);
        assert_eq!(stats["shadowLast"], "collision");
        let (_, audit) = get_with(port, "/audit", "");
        assert!(audit.contains("collision_shadow"), "{audit}");
        assert!(audit.contains("danger"), "{audit}");
        // a second command is decided as if the shadow did not exist
        let raw = post_full(
            port,
            "/route",
            &route_body("real", Some(&fresh_certificate(NONCE_B))),
        );
        assert!(raw.contains("SentReal"), "{raw}");
    }

    #[test]
    fn a_clear_answer_is_counted_as_clear() {
        let twin = std::sync::Arc::new(crate::twin_link::RecordingStateSink::connected());
        let shadow = std::sync::Arc::new(crate::collision_shadow::ScriptedShadow::answering(false));
        let port = start_with(shadow_config(&twin, &shadow));
        let raw = post_full(
            port,
            "/route",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        assert!(
            raw.to_ascii_lowercase()
                .contains("x-collision-shadow: clear"),
            "{raw}"
        );
        let stats = stats_json(port);
        assert_eq!(stats["shadowPredictions"], 1);
        assert_eq!(stats["shadowWouldBlock"], 0);
        assert_eq!(stats["shadowLast"], "clear");
    }

    #[test]
    fn a_twin_that_gives_no_prediction_is_a_failure_and_never_read_as_clear() {
        let twin = std::sync::Arc::new(crate::twin_link::RecordingStateSink::connected());
        let shadow = std::sync::Arc::new(crate::collision_shadow::ScriptedShadow::failing());
        let port = start_with(shadow_config(&twin, &shadow));
        let raw = post_full(
            port,
            "/route",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        assert!(raw.contains("SentReal"), "{raw}");
        assert!(
            raw.to_ascii_lowercase()
                .contains("x-collision-shadow: failed"),
            "{raw}"
        );
        let stats = stats_json(port);
        assert_eq!(stats["shadowFailures"], 1);
        assert_eq!(stats["shadowPredictions"], 0);
        assert_eq!(stats["shadowLast"], "failed");
    }

    #[test]
    fn a_blocked_command_and_a_simulated_one_are_not_asked_about() {
        let twin = std::sync::Arc::new(crate::twin_link::RecordingStateSink::connected());
        let shadow = std::sync::Arc::new(crate::collision_shadow::ScriptedShadow::answering(true));
        let port = start_with(shadow_config(&twin, &shadow));
        post_with(port, "/route", "", &route_body("real", None));
        post_with(port, "/route", "", &route_body("simulation", None));
        assert!(shadow.questions().is_empty());
        assert_eq!(stats_json(port)["shadowPredictions"], 0);
    }

    // ---- the shadow enforced: the twin is asked before the command is routed ----

    fn enforced_config(
        twin: &std::sync::Arc<crate::twin_link::RecordingStateSink>,
        shadow: &std::sync::Arc<crate::collision_shadow::ScriptedShadow>,
    ) -> ServerConfig {
        ServerConfig {
            shadow_enforce: true,
            ..shadow_config(twin, shadow)
        }
    }

    #[test]
    fn an_enforced_shadow_refuses_a_command_the_twin_predicts_a_contact_for() {
        let twin = std::sync::Arc::new(crate::twin_link::RecordingStateSink::connected());
        let shadow = std::sync::Arc::new(crate::collision_shadow::ScriptedShadow::answering(true));
        let port = start_with(enforced_config(&twin, &shadow));
        let raw = post_full(
            port,
            "/route",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        assert!(raw.starts_with("HTTP/1.1 409"), "{raw}");
        assert!(raw.contains("collision_shadow"), "{raw}");
        assert!(!raw.contains("SentReal"), "{raw}");
        // nothing reached the robot, the certificate was not used up and the twin was told nothing
        let stats = stats_json(port);
        assert_eq!(stats["routedReal"], 0);
        assert_eq!(stats["shadowEnforce"], true);
        assert_eq!(stats["shadowBlocked"], 1);
        assert_eq!(stats["shadowUnavailable"], 0);
        assert_eq!(shadow.questions(), vec![("shoulder".to_string(), 1.0)]);
        let (_, audit) = get_with(port, "/audit", "");
        assert!(audit.contains("shadow_blocked"), "{audit}");
        assert!(
            !audit.contains("\"phase\":\"order\""),
            "a refused command is not an order that went: {audit}"
        );
    }

    #[test]
    fn an_enforced_shadow_lets_a_clear_command_through_and_asks_only_once() {
        let twin = std::sync::Arc::new(crate::twin_link::RecordingStateSink::connected());
        let shadow = std::sync::Arc::new(crate::collision_shadow::ScriptedShadow::answering(false));
        let port = start_with(enforced_config(&twin, &shadow));
        let raw = post_full(
            port,
            "/route",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        assert!(raw.contains("SentReal"), "{raw}");
        assert_eq!(shadow.questions().len(), 1, "asked before, not again after");
        let stats = stats_json(port);
        assert_eq!(stats["routedReal"], 1);
        assert_eq!(stats["shadowBlocked"], 0);
        assert_eq!(stats["shadowPredictions"], 1);
    }

    #[test]
    fn an_enforced_shadow_never_sends_a_command_the_twin_could_not_check() {
        let twin = std::sync::Arc::new(crate::twin_link::RecordingStateSink::connected());
        let shadow = std::sync::Arc::new(crate::collision_shadow::ScriptedShadow::failing());
        let port = start_with(enforced_config(&twin, &shadow));
        let raw = post_full(
            port,
            "/route",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        assert!(raw.starts_with("HTTP/1.1 503"), "{raw}");
        assert!(raw.contains("collision_shadow_unavailable"), "{raw}");
        let stats = stats_json(port);
        assert_eq!(stats["routedReal"], 0);
        assert_eq!(stats["shadowUnavailable"], 1);
        assert_eq!(stats["shadowBlocked"], 0);
    }

    #[test]
    fn an_enforced_shadow_does_not_ask_about_simulation_or_a_body_it_cannot_read() {
        let twin = std::sync::Arc::new(crate::twin_link::RecordingStateSink::connected());
        let shadow = std::sync::Arc::new(crate::collision_shadow::ScriptedShadow::answering(true));
        let port = start_with(enforced_config(&twin, &shadow));
        post_with(port, "/route", "", &route_body("simulation", None));
        post_with(port, "/route", "", "{");
        assert!(shadow.questions().is_empty());
        assert_eq!(stats_json(port)["shadowBlocked"], 0);
    }

    #[test]
    fn without_enforcement_the_shadow_still_only_informs() {
        let twin = std::sync::Arc::new(crate::twin_link::RecordingStateSink::connected());
        let shadow = std::sync::Arc::new(crate::collision_shadow::ScriptedShadow::answering(true));
        let port = start_with(shadow_config(&twin, &shadow));
        assert_eq!(stats_json(port)["shadowEnforce"], false);
    }

    // ---- the zone monitor as a condition of the interlock, the session record, the replay and the new metrics ----

    fn now_plus(ms: i64) -> u64 {
        (now_ms() as i64 + ms) as u64
    }

    fn zones_config(
        zones: &std::sync::Arc<crate::safety_zones::tests::ScriptedZones>,
    ) -> ServerConfig {
        let zones: std::sync::Arc<dyn ZonesState> = zones.clone();
        ServerConfig {
            safety_zones: Some(zones),
            ..certified_config()
        }
    }

    #[test]
    fn a_real_command_goes_on_only_while_the_zones_say_a_recent_ready() {
        let zones = std::sync::Arc::new(crate::safety_zones::tests::ScriptedZones::saying(
            "READY",
            Some(now_plus(0)),
        ));
        let port = start_with(zones_config(&zones));
        let (status, _) = post_with(
            port,
            "/route",
            "",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        assert_eq!(status, 200);
        let stats = stats_json(port);
        assert_eq!(stats["zonesChecked"], 1);
        assert_eq!(stats["zonesLast"], "clear");
        assert_eq!(stats["routedReal"], 1);
        // a simulation command is not asked about
        post_with(port, "/route", "", &route_body("simulation", None));
        assert_eq!(stats_json(port)["zonesChecked"], 1);
    }

    #[test]
    fn zones_that_are_not_clear_stale_or_silent_refuse_the_command_before_its_certificate_is_used()
    {
        use crate::safety_zones::tests::ScriptedZones;
        for (zones, status, blocked_by) in [
            (
                ScriptedZones::saying("DANGER", Some(now_plus(0))),
                409,
                "safety_zones",
            ),
            (
                ScriptedZones::saying("INHIBITED", Some(now_plus(0))),
                409,
                "safety_zones",
            ),
            (
                ScriptedZones::saying("READY", Some(now_plus(-60_000))),
                503,
                "safety_zones_unavailable",
            ),
            (
                ScriptedZones::saying("READY", None),
                503,
                "safety_zones_unavailable",
            ),
            (ScriptedZones::failing(), 503, "safety_zones_unavailable"),
        ] {
            let zones = std::sync::Arc::new(zones);
            let port = start_with(zones_config(&zones));
            let certificate = fresh_certificate(NONCE_A);
            let (got, body) =
                post_with(port, "/route", "", &route_body("real", Some(&certificate)));
            assert_eq!(got, status, "{body}");
            assert!(body.contains(blocked_by), "{body}");
            let stats = stats_json(port);
            assert_eq!(stats["routedReal"], 0);
            // the interlock never saw the certificate, so it was not used up
            assert_eq!(stats["uncertified"], 0);
            assert_eq!(
                stats["zonesBlocked"].as_u64().unwrap()
                    + stats["zonesUnavailable"].as_u64().unwrap(),
                1
            );
            let metrics = get_with(port, "/metrics", "").1;
            crate::metrics::tests::assert_valid_exposition(&metrics);
            assert!(
                metrics.contains("hydra_umc_hil_bridge_refused_total{reason=\"safety_zones"),
                "{metrics}"
            );
            let audit = get_with(port, "/audit", "").1;
            assert!(audit.contains("safety_zones"), "{audit}");
        }
    }

    #[test]
    fn route_latency_is_a_histogram_per_mode() {
        let port = start_with(certified_config());
        post_with(port, "/route", "", &route_body("simulation", None));
        post_with(port, "/route", "", &route_body("real", None));
        let metrics = get_with(port, "/metrics", "").1;
        crate::metrics::tests::assert_valid_exposition(&metrics);
        assert!(
            metrics.contains("hydra_umc_hil_bridge_route_seconds_count{mode=\"simulation\"} 1"),
            "{metrics}"
        );
        assert!(
            metrics.contains("hydra_umc_hil_bridge_route_seconds_count{mode=\"real\"} 1"),
            "{metrics}"
        );
        assert!(
            metrics.contains("hydra_umc_hil_bridge_refused_total{reason=\"certificate\"} 1"),
            "{metrics}"
        );
    }

    fn lake_config(peer_addr: &str, replay: bool) -> ServerConfig {
        let recorder = crate::session_record::Recorder::start(
            "bench",
            crate::session_record::Lake {
                addr: peer_addr.to_string(),
                token: None,
                timeout: Duration::from_secs(2),
            },
            replay,
        )
        .unwrap();
        ServerConfig {
            recorder: Some(Arc::new(recorder)),
            ..certified_config()
        }
    }

    #[test]
    fn decided_commands_and_measured_positions_are_recorded_in_the_lake_with_their_outcome() {
        let peer =
            crate::http_client::tests::canned_peer(vec![(202, String::new(), "{}".to_string())]);
        let port = start_with(lake_config(&peer.addr, false));
        post_with(
            port,
            "/route",
            "",
            &route_body("real", Some(&fresh_certificate(NONCE_A))),
        );
        post_with(port, "/route", "", &route_body("real", None));
        post_with(
            port,
            "/measured",
            "",
            r#"{"joint":"shoulder","position":0.25}"#,
        );
        let mut seen = String::new();
        for _ in 0..100 {
            seen = peer.requests.lock().unwrap().join("\n");
            if seen.matches("\"sourceId\":\"hil-bench\"").count() >= 3 {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(seen.contains("\"cmd.shoulder\":1.0"), "{seen}");
        assert!(seen.contains("\"out.shoulder\":0.0"), "{seen}");
        assert!(seen.contains("\"out.shoulder\":2.0"), "{seen}");
        assert!(
            seen.contains("hil_measured") && seen.contains("\"pos.shoulder\":0.25"),
            "{seen}"
        );
        assert!(!seen.contains("signature"), "no certificate in the lake");
        let stats = stats_json(port);
        assert_eq!(stats["sessionRecord"], "bench");
        assert_eq!(stats["replayEnabled"], false);
    }

    #[test]
    fn replay_is_off_without_the_switch_and_runs_a_session_again_in_simulation_with_it() {
        let session = serde_json::json!([
            {"field":"cmd.shoulder","timestamp":10,"value":0.5},
            {"field":"out.shoulder","timestamp":10,"value":0.0},
            {"field":"real.shoulder","timestamp":10,"value":1.0},
            {"field":"cmd.shoulder","timestamp":11,"value":9.0},
            {"field":"out.shoulder","timestamp":11,"value":0.0},
            {"field":"real.shoulder","timestamp":11,"value":1.0},
            {"field":"cmd.elbow","timestamp":12,"value":0.1},
            {"field":"out.elbow","timestamp":12,"value":2.0},
            {"field":"real.elbow","timestamp":12,"value":0.0},
        ])
        .to_string();
        let peer = crate::http_client::tests::canned_peer(vec![(200, String::new(), session)]);
        let off = start_with(lake_config(&peer.addr, false));
        let (status, body) = post_with(off, "/replay", "", "{}");
        assert_eq!(status, 404, "{body}");
        assert!(body.contains("--replay"));

        let mut config = lake_config(&peer.addr, true);
        config.limits = Some(JointLimits::from_urdf(ARM_URDF).unwrap());
        let port = start_with(config);
        let (status, body) = post_with(port, "/replay", "", r#"{"session":"bench"}"#);
        assert_eq!(status, 200, "{body}");
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["commands"], 3);
        assert_eq!(value["recordedAsReal"], 2);
        assert_eq!(value["replayedToSimulation"], 3);
        assert_eq!(value["realHardwareTouched"], false);
        assert_eq!(value["byOutcome"]["sent_real"], 2);
        assert_eq!(value["limitsChecked"], true);
        // the real order that went through before is now outside the range of the URDF: a regression
        assert!(value["regressions"].as_u64().unwrap() >= 1, "{body}");
        let stats = stats_json(port);
        assert_eq!(stats["replays"], 1);
        assert_eq!(stats["routedReal"], 0);
        let (bad, _) = post_with(port, "/replay", "", r#"{"session":"a b"}"#);
        assert_eq!(bad, 400);
    }

    #[test]
    fn a_replay_that_cannot_read_the_lake_is_a_502_and_counted() {
        let peer =
            crate::http_client::tests::canned_peer(vec![(503, String::new(), "{}".to_string())]);
        let port = start_with(lake_config(&peer.addr, true));
        let (status, _) = post_with(port, "/replay", "", "{}");
        assert_eq!(status, 502);
        assert_eq!(stats_json(port)["replayFailures"], 1);
    }

    fn start_test_server_health(health_open: bool, token: Option<&str>) -> u16 {
        let server = bind("127.0.0.1:0").expect("bind on an OS-assigned port must succeed");
        let port = server
            .server_addr()
            .to_ip()
            .expect("tiny_http always binds a real IP socket for an http:// server")
            .port();
        let config = ServerConfig {
            health_open,
            token: token.map(str::to_string),
            interlock: InterlockConfig {
                allow_unsigned_risk: true,
                ..InterlockConfig::default()
            },
            ..ServerConfig::default()
        };
        thread::spawn(move || run_with_config(server, config));
        port
    }

    #[test]
    fn health_is_closed_by_default_even_with_a_token() {
        let port = start_test_server_health(false, None);
        assert_eq!(get(port, "/health").0, 404);
        let port = start_test_server_health(false, Some(TEST_TOKEN));
        let header = format!("Authorization: Bearer {TEST_TOKEN}\r\n");
        assert_eq!(get_status_with_headers(port, "/health", ""), 404);
        assert_eq!(get_status_with_headers(port, "/health", &header), 404);
    }

    #[test]
    fn open_health_names_the_service_and_the_version_on_loopback() {
        let port = start_test_server_health(true, Some(TEST_TOKEN));
        let (status, body) = get(port, "/health");
        assert_eq!(status, 200);
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["service"], "hydra-umc-hil-bridge");
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(json.as_object().unwrap().len(), 3);
        // the other routes keep asking for the token
        assert_eq!(get_status_with_headers(port, "/nope", ""), 401);
    }

    // ---- the pass verdict of the twin as a condition of the real mode (twin_pass.rs) ----

    const SAMPLES_JSON: &str =
        r#"[{"t":0.0,"joints":{"shoulder":1.0}},{"t":0.1,"joints":{"shoulder":1.5}}]"#;

    /// The answer of the twin for `SAMPLES_JSON`, signed with the key of these tests, and the samples it was made for.
    fn twin_pass_json(verdict: &str, robot: &str) -> String {
        let samples: Vec<crate::twin_pass::Sample> = serde_json::from_str(SAMPLES_JSON).unwrap();
        let sha = crate::twin_pass::trajectory_sha256(&samples);
        let mut v = crate::twin_pass::Verdict {
            verdict: verdict.to_string(),
            robot_id: robot.to_string(),
            trajectory_sha256: sha.clone(),
            issued_at_ms: now_ms() - 100,
            valid_until_ms: now_ms() + 60_000,
            nonce: "0123456789abcdef0123456789abcdef".to_string(),
            key_id: hex(&twin_key().verifying_key().to_bytes()[..8]),
            signature: String::new(),
        };
        v.signature = hex(&twin_key()
            .sign(crate::twin_pass::signed_text(&v, &sha).as_bytes())
            .to_bytes());
        format!(
            r#"{{"verdict":{{"verdict":"{}","robot_id":"{}","trajectory_sha256":"{}","issued_at_ms":{},"valid_until_ms":{},"nonce":"{}","key_id":"{}","signature":"{}"}},"samples":{SAMPLES_JSON}}}"#,
            v.verdict,
            v.robot_id,
            v.trajectory_sha256,
            v.issued_at_ms,
            v.valid_until_ms,
            v.nonce,
            v.key_id,
            v.signature
        )
    }

    fn pass_body(position: f64, certificate: &str, twin_pass: Option<&str>) -> String {
        let pass = twin_pass
            .map(|p| format!(r#","twin_pass":{p}"#))
            .unwrap_or_default();
        format!(
            r#"{{"mode":"real","joint":"shoulder","position":{position:?},"certificate":{certificate}{pass}}}"#
        )
    }

    fn certificate_at(position: f64, nonce: &str) -> String {
        certificate_json(
            &twin_key(),
            "shoulder",
            position,
            false,
            2.0,
            now_ms(),
            nonce,
        )
    }

    fn twin_pass_config(required: bool) -> ServerConfig {
        ServerConfig {
            twin_pass: TwinPassConfig {
                required,
                robot_id: "arm-1".to_string(),
                ..TwinPassConfig::default()
            },
            ..certified_config()
        }
    }

    #[test]
    fn with_the_switch_off_a_real_command_does_not_need_a_verdict() {
        let port = start_with(twin_pass_config(false));
        let (status, resp) = post_with(
            port,
            "/route",
            "",
            &pass_body(1.0, &certificate_at(1.0, NONCE_A), None),
        );
        assert_eq!(status, 200);
        assert!(resp.contains("SentReal"), "{resp}");
    }

    #[test]
    fn with_the_switch_on_a_real_command_without_a_verdict_is_blocked_and_keeps_its_certificate() {
        let port = start_with(twin_pass_config(true));
        let certificate = certificate_at(1.0, NONCE_A);
        let (_, blocked) = post_with(port, "/route", "", &pass_body(1.0, &certificate, None));
        assert!(blocked.contains("BlockedByInterlock"), "{blocked}");
        assert!(blocked.contains("twin_pass"), "{blocked}");
        // the certificate was not used up by the refusal: with the verdict the same command goes
        let (_, sent) = post_with(
            port,
            "/route",
            "",
            &pass_body(1.0, &certificate, Some(&twin_pass_json("pass", "arm-1"))),
        );
        assert!(sent.contains("SentReal"), "{sent}");
        let (_, stats) = get_with(port, "/stats", "");
        assert!(stats.contains("\"requireTwinPass\":true"), "{stats}");
        assert!(stats.contains("\"blockedByTwinPass\":1"), "{stats}");
    }

    #[test]
    fn with_the_switch_on_only_a_pass_for_this_robot_and_this_command_goes() {
        let port = start_with(twin_pass_config(true));
        for (position, nonce, pass) in [
            (1.0, NONCE_A, twin_pass_json("fail", "arm-1")),
            (1.0, NONCE_B, twin_pass_json("incomplete", "arm-1")),
            (
                1.0,
                "cccccccccccccccccccccccccccccccc",
                twin_pass_json("pass", "arm-2"),
            ),
            // a position that is not a point of the passed trajectory
            (
                1.25,
                "dddddddddddddddddddddddddddddddd",
                twin_pass_json("pass", "arm-1"),
            ),
        ] {
            let (_, resp) = post_with(
                port,
                "/route",
                "",
                &pass_body(position, &certificate_at(position, nonce), Some(&pass)),
            );
            assert!(resp.contains("BlockedByInterlock"), "{position} {resp}");
        }
    }

    #[test]
    fn a_point_of_the_trajectory_is_sent_once_and_the_other_points_still_go() {
        let port = start_with(twin_pass_config(true));
        let pass = twin_pass_json("pass", "arm-1");
        let (_, first) = post_with(
            port,
            "/route",
            "",
            &pass_body(1.0, &certificate_at(1.0, NONCE_A), Some(&pass)),
        );
        assert!(first.contains("SentReal"), "{first}");
        let (_, again) = post_with(
            port,
            "/route",
            "",
            &pass_body(1.0, &certificate_at(1.0, NONCE_B), Some(&pass)),
        );
        assert!(
            again.contains("BlockedByInterlock") && again.contains("already sent"),
            "{again}"
        );
        let (_, next) = post_with(
            port,
            "/route",
            "",
            &pass_body(
                1.5,
                &certificate_at(1.5, "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"),
                Some(&pass),
            ),
        );
        assert!(next.contains("SentReal"), "{next}");
    }

    #[test]
    fn the_simulation_mode_never_needs_a_verdict() {
        let port = start_with(twin_pass_config(true));
        let (status, resp) = post_with(port, "/route", "", &route_body("simulation", None));
        assert_eq!(status, 200);
        assert!(resp.contains("SentSimulation"), "{resp}");
    }

    fn admin_headers(key: &[u8], role: &str) -> String {
        format!("Authorization: Bearer {}\r\n", server_token(key, role))
    }

    fn settings_config(
        key: &[u8],
        locked: bool,
        robot_id: &str,
        file: Option<std::path::PathBuf>,
    ) -> ServerConfig {
        ServerConfig {
            identity_key: Some(key.to_vec()),
            twin_pass: TwinPassConfig {
                required: locked,
                locked,
                robot_id: robot_id.to_string(),
                file,
            },
            ..certified_config()
        }
    }

    const SETTINGS_KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

    #[test]
    fn the_switch_is_read_by_any_identity_and_changed_only_by_an_administrator_with_a_confirmation()
    {
        let port = start_with(settings_config(SETTINGS_KEY, false, "arm-1", None));
        let (status, view) = get_with(
            port,
            "/settings/twin-pass",
            &admin_headers(SETTINGS_KEY, "viewer"),
        );
        assert_eq!(status, 200);
        assert!(
            view.contains("\"requireTwinPass\":false") && view.contains("\"ready\":true"),
            "{view}"
        );
        // an operator may command the robot but not change this switch
        let (status, _) = post_with(
            port,
            "/settings/twin-pass",
            &admin_headers(SETTINGS_KEY, "operator"),
            r#"{"requireTwinPass":true,"confirm":true}"#,
        );
        assert_eq!(status, 403);
        let admin = admin_headers(SETTINGS_KEY, "admin");
        let (status, body) = post_with(
            port,
            "/settings/twin-pass",
            &admin,
            r#"{"requireTwinPass":true}"#,
        );
        assert_eq!(status, 400);
        assert!(body.contains("CONFIRMATION_REQUIRED"), "{body}");
        for bad in [
            r#"{}"#,
            r#"{"requireTwinPass":"yes","confirm":true}"#,
            r#"{"requireTwinPass":true,"confirm":true,"x":1}"#,
        ] {
            assert_eq!(
                post_with(port, "/settings/twin-pass", &admin, bad).0,
                400,
                "{bad}"
            );
        }
        let (status, body) = post_with(
            port,
            "/settings/twin-pass",
            &admin,
            r#"{"requireTwinPass":true,"confirm":true}"#,
        );
        assert_eq!(status, 200);
        assert!(
            body.contains("\"requireTwinPass\":true") && body.contains("\"changed\":true"),
            "{body}"
        );
        // from now on a real command needs the verdict
        let (_, resp) = post_with(
            port,
            "/route",
            &admin,
            &pass_body(1.0, &certificate_at(1.0, NONCE_A), None),
        );
        assert!(resp.contains("BlockedByInterlock"), "{resp}");
        // and it can be turned off again
        let (status, body) = post_with(
            port,
            "/settings/twin-pass",
            &admin,
            r#"{"requireTwinPass":false,"confirm":true}"#,
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"requireTwinPass\":false"), "{body}");
    }

    #[test]
    fn without_the_identity_of_the_server_nobody_can_change_the_switch() {
        let port = start_with(twin_pass_config(false));
        let (status, _) = post_with(
            port,
            "/settings/twin-pass",
            "",
            r#"{"requireTwinPass":true,"confirm":true}"#,
        );
        assert_eq!(status, 403);
        let (status, view) = get_with(port, "/settings/twin-pass", "");
        assert_eq!(status, 200);
        assert!(view.contains("\"canChange\":false"), "{view}");
    }

    #[test]
    fn a_locked_switch_stays_on_and_a_switch_without_its_key_or_robot_cannot_be_turned_on() {
        let locked = start_with(settings_config(SETTINGS_KEY, true, "arm-1", None));
        let admin = admin_headers(SETTINGS_KEY, "admin");
        let (status, body) = post_with(
            locked,
            "/settings/twin-pass",
            &admin,
            r#"{"requireTwinPass":false,"confirm":true}"#,
        );
        assert_eq!(status, 409);
        assert!(body.contains("SETTING_REFUSED"), "{body}");
        let no_robot = start_with(settings_config(SETTINGS_KEY, false, "", None));
        let (status, _) = post_with(
            no_robot,
            "/settings/twin-pass",
            &admin,
            r#"{"requireTwinPass":true,"confirm":true}"#,
        );
        assert_eq!(status, 409);
    }

    #[test]
    fn a_change_is_kept_in_the_file_and_a_file_that_cannot_be_written_changes_nothing() {
        let dir = std::env::temp_dir().join(format!("hil-twin-pass-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("twin_pass.json");
        let port = start_with(settings_config(
            SETTINGS_KEY,
            false,
            "arm-1",
            Some(file.clone()),
        ));
        let admin = admin_headers(SETTINGS_KEY, "admin");
        let (status, _) = post_with(
            port,
            "/settings/twin-pass",
            &admin,
            r#"{"requireTwinPass":true,"confirm":true}"#,
        );
        assert_eq!(status, 200);
        assert!(std::fs::read_to_string(&file).unwrap().contains("true"));
        let missing = dir.join("no-such-folder").join("twin_pass.json");
        let port = start_with(settings_config(SETTINGS_KEY, false, "arm-1", Some(missing)));
        let (status, _) = post_with(
            port,
            "/settings/twin-pass",
            &admin,
            r#"{"requireTwinPass":true,"confirm":true}"#,
        );
        assert_eq!(status, 409);
        let (_, view) = get_with(port, "/settings/twin-pass", &admin);
        assert!(view.contains("\"requireTwinPass\":false"), "{view}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
