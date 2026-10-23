// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/access.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! Optional Bearer token for the HTTP API.
//!
//! On loopback the API needs no token (the only caller is the machine itself) unless one is set. Away from loopback a token is
//! mandatory: `serve` refuses to start without one. With a token set, every request except `GET /stats` (the health path of the
//! manifest) must carry `Authorization: Bearer <token>`; the comparison takes constant time. The token comes from a file
//! (`--token-file`) or the `HYDRA_UMC_HIL_BRIDGE_TOKEN` environment variable.

use std::net::IpAddr;

pub const TOKEN_ENV: &str = "HYDRA_UMC_HIL_BRIDGE_TOKEN";
pub const MIN_TOKEN_LENGTH: usize = 16;

/// True for `localhost` and the loopback addresses; a wildcard (0.0.0.0, ::) or any other address is not loopback.
pub fn is_loopback(addr: &str) -> bool {
    let host = addr.trim().trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// The configured token (`None` when there is none). The file wins over the environment value. An unreadable file or a token that is
/// too short is an error.
pub fn load_token(file: Option<&str>, env_value: Option<&str>) -> Result<Option<String>, String> {
    let mut token = env_value.map(|v| v.trim().to_string()).unwrap_or_default();
    if let Some(path) = file {
        token = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read the token file: {e}"))?
            .trim()
            .to_string();
    }
    if token.is_empty() {
        return Ok(None);
    }
    if token.chars().count() < MIN_TOKEN_LENGTH {
        return Err(format!(
            "the bearer token must have at least {MIN_TOKEN_LENGTH} characters"
        ));
    }
    Ok(Some(token))
}

/// Why this address cannot be served (`None` when it can): away from loopback a token is required.
pub fn bind_problem(addr: &str, token: &Option<String>) -> Option<String> {
    if !is_loopback(addr) && token.is_none() {
        return Some(format!(
            "--addr {addr} is not loopback: set --token-file (or {TOKEN_ENV}) so that only a caller holding the token can use the API"
        ));
    }
    None
}

/// Comparison that takes the same time whatever the point where the two values differ.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

/// True when the `Authorization` header value is exactly `Bearer <expected>`.
pub fn bearer_matches(header_value: Option<&str>, expected: &str) -> bool {
    let Some(value) = header_value else {
        return false;
    };
    let Some((scheme, presented)) = value.split_once(' ') else {
        return false;
    };
    scheme == "Bearer" && constant_time_eq(presented.trim().as_bytes(), expected.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "a-long-random-token-1234";

    #[test]
    fn loopback_detection() {
        for addr in [
            "127.0.0.1",
            "127.9.9.9",
            "::1",
            "[::1]",
            "localhost",
            " LOCALHOST ",
        ] {
            assert!(is_loopback(addr), "{addr}");
        }
        for addr in [
            "0.0.0.0",
            "::",
            "192.168.0.180",
            "example.com",
            "127.0.0.1.evil.example",
            "",
        ] {
            assert!(!is_loopback(addr), "{addr}");
        }
    }

    #[test]
    fn a_non_loopback_address_needs_a_token() {
        assert_eq!(bind_problem("127.0.0.1", &None), None);
        assert_eq!(bind_problem("0.0.0.0", &Some(TOKEN.to_string())), None);
        assert!(bind_problem("0.0.0.0", &None)
            .unwrap()
            .contains("not loopback"));
        assert!(bind_problem("192.168.0.180", &None).is_some());
    }

    #[test]
    fn the_token_comes_from_the_environment_or_a_file_and_must_be_long_enough() {
        assert_eq!(load_token(None, None), Ok(None));
        assert_eq!(load_token(None, Some("  ")), Ok(None));
        assert_eq!(
            load_token(None, Some(&format!(" {TOKEN} "))),
            Ok(Some(TOKEN.to_string()))
        );
        assert!(load_token(None, Some("short"))
            .unwrap_err()
            .contains("at least 16"));
        assert!(load_token(Some("does-not-exist.token"), None)
            .unwrap_err()
            .contains("cannot read"));

        let path =
            std::env::temp_dir().join(format!("hil-bridge-token-{}.txt", std::process::id()));
        std::fs::write(&path, format!("{TOKEN}\n")).unwrap();
        // the file wins over the environment
        let loaded = load_token(path.to_str(), Some("an-environment-token-9999"));
        let _ = std::fs::remove_file(&path);
        assert_eq!(loaded, Ok(Some(TOKEN.to_string())));
    }

    #[test]
    fn only_the_exact_bearer_token_matches() {
        assert!(bearer_matches(Some(&format!("Bearer {TOKEN}")), TOKEN));
        assert!(!bearer_matches(None, TOKEN));
        assert!(!bearer_matches(Some("Bearer wrong-token-0000000"), TOKEN));
        assert!(!bearer_matches(Some(&format!("Basic {TOKEN}")), TOKEN));
        assert!(!bearer_matches(Some(TOKEN), TOKEN));
        assert!(!bearer_matches(Some("Bearer "), TOKEN));
        assert!(!bearer_matches(Some(&format!("Bearer {TOKEN}x")), TOKEN));
    }
}
