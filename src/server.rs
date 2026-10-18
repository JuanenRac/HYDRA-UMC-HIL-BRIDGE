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

use serde::Deserialize;
use serde_json::json;
use tiny_http::{Header, Method, Response, Server};

use crate::bridge::{Bridge, CommandSink, RecordingSink, RouteOutcome, SimulatedTransport};
use crate::interlock::TwinRiskReport;
use crate::protocol::{JointCommand, Mode};

fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()
}

fn header(name: &str, value: &str) -> tiny_http::Header {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes())
        .expect("static header names and values are valid")
}

fn write_json(request: tiny_http::Request, status: u16, body: &serde_json::Value) {
    let text = body.to_string();
    let response = Response::from_string(text)
        .with_status_code(status)
        .with_header(json_header());
    let _ = request.respond(response);
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
    risk: Option<TwinRiskReport>,
    #[serde(default)]
    transport: TransportConfig,
}

#[derive(Deserialize)]
struct MirrorRequest {
    joint: String,
    position: f64,
}

/// Real process-lifetime counters, found missing while auditing the code:
/// `GET /stats` used to return a hardcoded
/// static JSON body with nothing real to aggregate - each request built a
/// fresh `Bridge`/`RecordingSink` and threw them away. `run()`'s own
/// request loop is single-threaded (one request handled at a time, no
/// per-request thread spawned), so plain `u64` fields are enough, with no
/// `Mutex` or atomics needed; every increment and the `/stats` read that
/// reports them happen from the same loop iteration in sequence, never
/// concurrently.
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
}

pub fn bind(addr: &str) -> std::io::Result<Server> {
    Server::http(addr).map_err(std::io::Error::other)
}

/// `fault_injection` lets a request pick a simulated transport (disconnected
/// link, latency, timeout). It is a test facility: without it the field is
/// refused, so a deployed server cannot be told to fake a transport fault.
pub fn run(server: Server, fault_injection: bool) {
    let mut stats = Stats::default();
    for mut request in server.incoming_requests() {
        let path = request.url().split('?').next().unwrap_or("").to_string();

        if path == "/stats" && request.method() == &Method::Get {
            write_json(
                request,
                200,
                &json!({
                    "role": "Hardware-in-the-loop real-vs-virtual command bridge",
                    "routedReal": stats.routed_real,
                    "routedSimulation": stats.routed_simulation,
                    "blockedByInterlock": stats.blocked_by_interlock,
                    "transportFailures": stats.transport_failures,
                    "mirrored": stats.mirrored,
                    "mirrorFailures": stats.mirror_failures,
                    "malformedRequests": stats.malformed_requests,
                    "notFound": stats.not_found,
                }),
            );
            continue;
        }
        if request.method() != &Method::Post {
            stats.not_found += 1;
            if matches!(path.as_str(), "/route" | "/mirror" | "/stats") {
                let response =
                    Response::from_string(json!({"error": "method not allowed"}).to_string())
                        .with_status_code(405)
                        .with_header(json_header())
                        .with_header(header(
                            "Allow",
                            if path == "/stats" { "GET" } else { "POST" },
                        ));
                let _ = request.respond(response);
            } else {
                write_json(request, 404, &json!({"error": "not found"}));
            }
            continue;
        }

        if let Err((status, message)) = check_request_origin(&request) {
            stats.malformed_requests += 1;
            write_json(request, status, &json!({"error": message}));
            continue;
        }

        let raw = match read_body(&mut request) {
            Ok(raw) => raw,
            Err(e) => {
                stats.malformed_requests += 1;
                let (status, message) = match e {
                    BodyError::TooLarge => (
                        413,
                        format!("request body is larger than {MAX_BODY_BYTES} bytes"),
                    ),
                    BodyError::NotUtf8 => (400, "request body is not valid UTF-8".to_string()),
                    BodyError::Io(e) => (400, format!("could not read request body: {e}")),
                };
                write_json(request, status, &json!({"error": message}));
                continue;
            }
        };

        match path.as_str() {
            "/route" => handle_route(request, &raw, &mut stats, fault_injection),
            "/mirror" => handle_mirror(request, &raw, &mut stats),
            _ => {
                stats.not_found += 1;
                write_json(request, 404, &json!({"error": "not found"}));
            }
        }
    }
}

fn handle_route(request: tiny_http::Request, raw: &str, stats: &mut Stats, fault_injection: bool) {
    let req: RouteRequest = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(e) => {
            stats.malformed_requests += 1;
            write_json(
                request,
                400,
                &json!({"error": format!("malformed request JSON: {e}")}),
            );
            return;
        }
    };

    let command = JointCommand {
        joint: req.joint,
        position: req.position,
    };
    if let Err(reason) = command.validate() {
        stats.malformed_requests += 1;
        write_json(request, 400, &json!({"error": reason}));
        return;
    }
    let use_simulated = req.transport.disconnected
        || req.transport.latency_ms.is_some()
        || req.transport.timeout_ms.is_some();
    if use_simulated && !fault_injection {
        stats.malformed_requests += 1;
        write_json(
            request,
            400,
            &json!({"error": "simulated transport faults are disabled; start the server with --fault-injection"}),
        );
        return;
    }
    let bridge = Bridge::new(req.mode);

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

    let outcome = bridge.route_command(command, req.risk, real_sink, &mut sim_sink);
    match &outcome {
        RouteOutcome::SentReal => stats.routed_real += 1,
        RouteOutcome::SentSimulation => stats.routed_simulation += 1,
        RouteOutcome::BlockedByInterlock { .. } => stats.blocked_by_interlock += 1,
        RouteOutcome::TransportFailure { .. } => stats.transport_failures += 1,
    }
    eprintln!("{}", outcome.log_line(req.mode));
    let response = Response::from_string(serde_json::to_value(&outcome).unwrap().to_string())
        .with_status_code(200)
        .with_header(json_header())
        .with_header(header("X-Outcome-Kind", outcome.kind()))
        .with_header(header(
            "X-Delivered",
            if outcome.delivered() { "true" } else { "false" },
        ));
    let _ = request.respond(response);
}

fn handle_mirror(request: tiny_http::Request, raw: &str, stats: &mut Stats) {
    let req: MirrorRequest = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(e) => {
            stats.malformed_requests += 1;
            write_json(
                request,
                400,
                &json!({"error": format!("malformed request JSON: {e}")}),
            );
            return;
        }
    };

    let command = JointCommand {
        joint: req.joint,
        position: req.position,
    };
    if let Err(reason) = command.validate() {
        stats.malformed_requests += 1;
        write_json(request, 400, &json!({"error": reason}));
        return;
    }
    let bridge = Bridge::new(Mode::Simulation);
    let mut mirror_sink = RecordingSink::default();
    match bridge.mirror_command(&command, &mut mirror_sink) {
        Ok(()) => {
            stats.mirrored += 1;
            write_json(request, 200, &json!({"mirrored": true}));
        }
        Err(e) => {
            stats.mirror_failures += 1;
            write_json(
                request,
                200,
                &json!({"mirrored": false, "error": e.to_string()}),
            );
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
}
