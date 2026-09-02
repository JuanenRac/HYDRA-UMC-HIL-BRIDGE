// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/server.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0 - see LICENSE
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

use crate::bridge::{Bridge, CommandSink, RecordingSink, SimulatedTransport};
use crate::interlock::TwinRiskReport;
use crate::protocol::{JointCommand, Mode};

fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap()
}

fn write_json(request: tiny_http::Request, status: u16, body: &serde_json::Value) {
    let text = body.to_string();
    let response = Response::from_string(text)
        .with_status_code(status)
        .with_header(json_header());
    let _ = request.respond(response);
}

fn read_body(request: &mut tiny_http::Request) -> std::io::Result<String> {
    // as_reader() returns `&mut dyn Read` - a trait object, so the method
    // call below resolves via dynamic dispatch and needs no local
    // `use std::io::Read` (only calling through a generic `T: Read`
    // bound would).
    let mut raw = String::new();
    request.as_reader().read_to_string(&mut raw)?;
    Ok(raw)
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

pub fn bind(addr: &str) -> std::io::Result<Server> {
    Server::http(addr).map_err(std::io::Error::other)
}

pub fn run(server: Server) {
    for mut request in server.incoming_requests() {
        let path = request.url().split('?').next().unwrap_or("").to_string();

        if path == "/stats" && request.method() == &Method::Get {
            write_json(
                request,
                200,
                &json!({"role": "Hardware-in-the-loop real-vs-virtual command bridge"}),
            );
            continue;
        }
        if request.method() != &Method::Post {
            write_json(request, 404, &json!({"error": "not found"}));
            continue;
        }

        let raw = match read_body(&mut request) {
            Ok(raw) => raw,
            Err(e) => {
                write_json(
                    request,
                    400,
                    &json!({"error": format!("could not read request body: {e}")}),
                );
                continue;
            }
        };

        match path.as_str() {
            "/route" => handle_route(request, &raw),
            "/mirror" => handle_mirror(request, &raw),
            _ => write_json(request, 404, &json!({"error": "not found"})),
        }
    }
}

fn handle_route(request: tiny_http::Request, raw: &str) {
    let req: RouteRequest = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(e) => {
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
    let bridge = Bridge::new(req.mode);

    let mut recording_real = RecordingSink::default();
    let use_simulated = req.transport.disconnected
        || req.transport.latency_ms.is_some()
        || req.transport.timeout_ms.is_some();
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
    write_json(request, 200, &serde_json::to_value(&outcome).unwrap());
}

fn handle_mirror(request: tiny_http::Request, raw: &str) {
    let req: MirrorRequest = match serde_json::from_str(raw) {
        Ok(r) => r,
        Err(e) => {
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
    let bridge = Bridge::new(Mode::Simulation);
    let mut mirror_sink = RecordingSink::default();
    match bridge.mirror_command(&command, &mut mirror_sink) {
        Ok(()) => write_json(request, 200, &json!({"mirrored": true})),
        Err(e) => write_json(
            request,
            200,
            &json!({"mirrored": false, "error": e.to_string()}),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::thread;

    fn start_test_server() -> u16 {
        let server = bind("127.0.0.1:0").expect("bind on an OS-assigned port must succeed");
        let port = server
            .server_addr()
            .to_ip()
            .expect("tiny_http always binds a real IP socket for an http:// server")
            .port();
        thread::spawn(move || run(server));
        port
    }

    fn post(port: u16, path: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect must succeed");
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
        let body = r#"{"mode":"real","joint":"shoulder","position":1.0}"#;
        let (status, resp) = post(port, "/route", body);
        assert_eq!(status, 200);
        assert!(resp.contains("SentReal"));
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
        let body = r#"{"mode":"real","joint":"shoulder","position":1.0,"transport":{"disconnected":true}}"#;
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
    fn unknown_path_is_404() {
        let port = start_test_server();
        let (status, _) = get(port, "/nope");
        assert_eq!(status, 404);
    }
}
