// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/http_client.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! One blocking HTTP/1.1 request over a plain socket, for the services the bridge asks (the zone monitor, the data lake).
//!
//! No TLS and no redirects: these services run on the same machine or the same wired network, like the twin. The connection is closed after
//! the answer (`Connection: close`), the answer is read within a bound and anything that is not a complete answer is a transport error, never
//! a success.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::bridge::TransportError;

/// What came back: the status, the page cursor of the data lake when it sent one, and the body.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpAnswer {
    pub status: u16,
    pub next_cursor: Option<String>,
    pub body: String,
}

/// `http://host:port` (no path, no other scheme) to `host:port`.
pub fn parse_base_url(url: &str, what: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("the {what} URL must start with http://"))?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.contains('/') || !rest.contains(':') {
        return Err(format!("the {what} URL must be http://HOST:PORT"));
    }
    Ok(rest.to_string())
}

/// Sends one request and reads the whole answer (at most `max_bytes`).
pub fn request(
    addr: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
    timeout: Duration,
    max_bytes: u64,
) -> Result<HttpAnswer, TransportError> {
    let timeout_ms = timeout.as_millis() as u64;
    let timed_out = || TransportError::Timeout {
        after_ms: timeout_ms,
    };
    let socket = addr
        .to_socket_addrs()
        .map_err(|_| TransportError::Disconnected)?
        .next()
        .ok_or(TransportError::Disconnected)?;
    let mut stream =
        TcpStream::connect_timeout(&socket, timeout).map_err(|_| TransportError::Disconnected)?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(token) = token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(body) = body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    if let Some(body) = body {
        head.push_str(body);
    }
    stream.write_all(head.as_bytes()).map_err(|_| timed_out())?;
    let mut raw = Vec::new();
    let mut limited = (&mut stream).take(max_bytes);
    limited.read_to_end(&mut raw).map_err(|_| timed_out())?;
    let text = String::from_utf8_lossy(&raw);
    let (head, answer) = text
        .split_once("\r\n\r\n")
        .ok_or(TransportError::Disconnected)?;
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or(TransportError::Disconnected)?;
    let next_cursor = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("x-next-cursor")
            .then(|| value.trim().to_string())
    });
    Ok(HttpAnswer {
        status,
        next_cursor,
        body: answer.to_string(),
    })
}

/// Percent-encodes a value for a query string (everything but the unreserved characters).
pub fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A peer that answers every request with the next canned answer `(status, extra header lines, body)` and remembers what it was asked.
    pub(crate) struct CannedPeer {
        pub addr: String,
        pub requests: Arc<Mutex<Vec<String>>>,
    }

    pub(crate) fn canned_peer(answers: Vec<(u16, String, String)>) -> CannedPeer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        std::thread::spawn(move || {
            for (next, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { break };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // read the head, then as much body as Content-Length says
                loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let wanted = head
                            .lines()
                            .find_map(|l| {
                                let (n, v) = l.split_once(':')?;
                                n.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if body.len() >= wanted {
                            break;
                        }
                    }
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf).to_string());
                let (status, headers, body) = answers
                    .get(next.min(answers.len().saturating_sub(1)))
                    .cloned()
                    .unwrap_or((500, String::new(), String::new()));
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        CannedPeer { addr, requests }
    }

    #[test]
    fn a_complete_answer_gives_its_status_cursor_and_body() {
        let peer = canned_peer(vec![(
            200,
            "X-Next-Cursor: abc123\r\n".to_string(),
            "[1,2]".to_string(),
        )]);
        let answer = request(
            &peer.addr,
            "GET",
            "/query?x=1",
            Some("a-token-of-16-chars"),
            None,
            Duration::from_secs(2),
            4096,
        )
        .unwrap();
        assert_eq!(answer.status, 200);
        assert_eq!(answer.next_cursor.as_deref(), Some("abc123"));
        assert_eq!(answer.body, "[1,2]");
        let asked = peer.requests.lock().unwrap()[0].clone();
        assert!(asked.starts_with("GET /query?x=1 HTTP/1.1"));
        assert!(asked.contains("Authorization: Bearer a-token-of-16-chars"));
    }

    #[test]
    fn nobody_listening_is_disconnected_and_a_url_needs_host_and_port() {
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = closed.local_addr().unwrap().to_string();
        drop(closed);
        let err = request(
            &addr,
            "GET",
            "/",
            None,
            None,
            Duration::from_millis(300),
            1024,
        );
        assert!(err.is_err());
        assert_eq!(
            parse_base_url("http://127.0.0.1:8095/", "data lake").unwrap(),
            "127.0.0.1:8095"
        );
        for bad in ["", "127.0.0.1:1", "https://h:1", "http://h", "http://h:1/x"] {
            assert!(parse_base_url(bad, "data lake").is_err(), "{bad}");
        }
    }

    #[test]
    fn a_query_value_is_percent_encoded() {
        assert_eq!(encode_query_value("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(encode_query_value("a b&c="), "a%20b%26c%3D");
    }
}
