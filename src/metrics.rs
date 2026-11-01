// =============================================================================
// HYDRA-UMC-HIL-BRIDGE - src/metrics.rs
// Copyright (C) 2026 JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
// GPL-3.0-or-later - see LICENSE
// =============================================================================
//! What the bridge counts about itself, in the text format of Prometheus (`GET /metrics`, version 0.0.4 of the exposition format).
//!
//! The numbers are the ones `GET /stats` already gives, in the shape a monitor reads: how many commands were routed, blocked, refused or
//! failed, how the watchdog and the link to the twin are doing. No joint, position, certificate, token or address is ever a metric. Unlike
//! `/stats` (the health path of the manifest) the route is behind the token when the bridge has one, because it tells how many real commands
//! were issued and refused.

use std::fmt::Write as _;

pub struct Series {
    pub name: &'static str,
    pub kind: &'static str,
    pub help: &'static str,
    pub value: f64,
}

impl Series {
    pub fn counter(name: &'static str, help: &'static str, value: u64) -> Series {
        Series {
            name,
            kind: "counter",
            help,
            value: value as f64,
        }
    }

    pub fn gauge(name: &'static str, help: &'static str, value: f64) -> Series {
        Series {
            name,
            kind: "gauge",
            help,
            value,
        }
    }
}

/// The text of the exposition: the version as a labelled info series, then every series with its HELP and TYPE lines.
pub fn exposition(version: &str, series: &[Series]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# HELP hydra_umc_hil_bridge_info The version of the running service."
    );
    let _ = writeln!(out, "# TYPE hydra_umc_hil_bridge_info gauge");
    let _ = writeln!(out, "hydra_umc_hil_bridge_info{{version=\"{version}\"}} 1");
    for item in series {
        let _ = writeln!(out, "# HELP {} {}", item.name, item.help);
        let _ = writeln!(out, "# TYPE {} {}", item.name, item.kind);
        let _ = writeln!(out, "{} {}", item.name, item.value);
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The shape the exposition format asks for: comments are `# HELP`/`# TYPE`, every other line is `name{labels} value`, a name is declared
    /// before it is used and every value is a number.
    pub(crate) fn assert_valid_exposition(text: &str) {
        let mut declared = std::collections::BTreeSet::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("# TYPE ") {
                let mut parts = rest.split_whitespace();
                let name = parts.next().expect("a name");
                let kind = parts.next().expect("a type");
                assert!(
                    ["counter", "gauge", "histogram", "summary", "untyped"].contains(&kind),
                    "{line}"
                );
                declared.insert(name.to_string());
            } else if line.starts_with("# HELP ") {
                assert!(line.split_whitespace().count() >= 4, "{line}");
            } else if !line.is_empty() {
                let (series, value) = line.rsplit_once(' ').unwrap_or_else(|| panic!("{line}"));
                let name = series.split('{').next().unwrap();
                assert!(
                    name.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':'),
                    "{line}"
                );
                assert!(declared.contains(name), "{name} used before its TYPE line");
                assert!(value.parse::<f64>().is_ok(), "{line}");
            }
        }
    }

    #[test]
    fn the_text_has_the_version_and_every_series_declared_once() {
        let text = exposition(
            "1.2.3",
            &[
                Series::counter(
                    "hydra_umc_hil_bridge_routed_real_total",
                    "Real commands routed.",
                    7,
                ),
                Series::gauge(
                    "hydra_umc_hil_bridge_active_leases",
                    "Joints with a valid lease.",
                    2.0,
                ),
            ],
        );
        assert_valid_exposition(&text);
        assert!(text.contains("hydra_umc_hil_bridge_info{version=\"1.2.3\"} 1"));
        assert!(text.contains("hydra_umc_hil_bridge_routed_real_total 7"));
        assert!(text.contains("# TYPE hydra_umc_hil_bridge_active_leases gauge"));
        assert_eq!(
            text.matches("# TYPE hydra_umc_hil_bridge_routed_real_total")
                .count(),
            1
        );
    }
}
