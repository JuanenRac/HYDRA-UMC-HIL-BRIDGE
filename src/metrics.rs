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
    /// One label (`name`, `value`) when the series is one of several that share a name.
    pub label: Option<(&'static str, &'static str)>,
}

impl Series {
    pub fn counter(name: &'static str, help: &'static str, value: u64) -> Series {
        Series {
            name,
            kind: "counter",
            help,
            value: value as f64,
            label: None,
        }
    }

    /// A counter that shares its name with others and tells them apart by one label.
    pub fn labelled_counter(
        name: &'static str,
        help: &'static str,
        label: (&'static str, &'static str),
        value: u64,
    ) -> Series {
        Series {
            label: Some(label),
            ..Series::counter(name, help, value)
        }
    }

    pub fn gauge(name: &'static str, help: &'static str, value: f64) -> Series {
        Series {
            name,
            kind: "gauge",
            help,
            value,
            label: None,
        }
    }
}

/// The upper bounds, in seconds, of the buckets of the latency histograms.
pub const LATENCY_BOUNDS_S: [f64; 10] = [0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 5.0];

/// How long something took: the count per bucket (not cumulative), the sum and the count.
#[derive(Debug, Clone, Default)]
pub struct Histogram {
    buckets: [u64; LATENCY_BOUNDS_S.len()],
    sum_s: f64,
    count: u64,
}

impl Histogram {
    pub fn observe(&mut self, seconds: f64) {
        let seconds = if seconds.is_finite() && seconds >= 0.0 {
            seconds
        } else {
            0.0
        };
        self.count += 1;
        self.sum_s += seconds;
        if let Some(i) = LATENCY_BOUNDS_S.iter().position(|b| seconds <= *b) {
            self.buckets[i] += 1;
        }
    }

    #[cfg(test)]
    pub fn count(&self) -> u64 {
        self.count
    }
}

/// A histogram with its name, its help and the label that tells it from the others of the same name.
pub struct HistogramSeries<'a> {
    pub name: &'static str,
    pub help: &'static str,
    pub label: (&'static str, &'static str),
    pub histogram: &'a Histogram,
}

/// The text of the exposition: the version as a labelled info series, then every series with its HELP and TYPE lines (once per name).
#[cfg(test)]
pub fn exposition(version: &str, series: &[Series]) -> String {
    exposition_with_histograms(version, series, &[])
}

/// Like the plain exposition, with histograms (`_bucket`, `_sum` and `_count` lines) after the series.
pub fn exposition_with_histograms(
    version: &str,
    series: &[Series],
    histograms: &[HistogramSeries],
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# HELP hydra_umc_hil_bridge_info The version of the running service."
    );
    let _ = writeln!(out, "# TYPE hydra_umc_hil_bridge_info gauge");
    let _ = writeln!(out, "hydra_umc_hil_bridge_info{{version=\"{version}\"}} 1");
    let mut declared: Vec<&str> = Vec::new();
    for item in series {
        if !declared.contains(&item.name) {
            let _ = writeln!(out, "# HELP {} {}", item.name, item.help);
            let _ = writeln!(out, "# TYPE {} {}", item.name, item.kind);
            declared.push(item.name);
        }
        match item.label {
            Some((key, value)) => {
                let _ = writeln!(out, "{}{{{key}=\"{value}\"}} {}", item.name, item.value);
            }
            None => {
                let _ = writeln!(out, "{} {}", item.name, item.value);
            }
        }
    }
    for item in histograms {
        if !declared.contains(&item.name) {
            let _ = writeln!(out, "# HELP {} {}", item.name, item.help);
            let _ = writeln!(out, "# TYPE {} histogram", item.name);
            declared.push(item.name);
        }
        let (key, value) = item.label;
        let mut cumulative = 0u64;
        for (bound, count) in LATENCY_BOUNDS_S.iter().zip(item.histogram.buckets.iter()) {
            cumulative += count;
            let _ = writeln!(
                out,
                "{}_bucket{{{key}=\"{value}\",le=\"{bound}\"}} {cumulative}",
                item.name
            );
        }
        let _ = writeln!(
            out,
            "{}_bucket{{{key}=\"{value}\",le=\"+Inf\"}} {}",
            item.name, item.histogram.count
        );
        let _ = writeln!(
            out,
            "{}_sum{{{key}=\"{value}\"}} {}",
            item.name, item.histogram.sum_s
        );
        let _ = writeln!(
            out,
            "{}_count{{{key}=\"{value}\"}} {}",
            item.name, item.histogram.count
        );
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
                let base = name
                    .strip_suffix("_bucket")
                    .or_else(|| name.strip_suffix("_sum"))
                    .or_else(|| name.strip_suffix("_count"))
                    .filter(|b| declared.contains(*b))
                    .unwrap_or(name);
                assert!(declared.contains(base), "{name} used before its TYPE line");
                assert!(value.parse::<f64>().is_ok(), "{line}");
            }
        }
    }

    #[test]
    fn a_histogram_has_cumulative_buckets_and_labelled_counters_share_one_declaration() {
        let mut real = Histogram::default();
        real.observe(0.0004);
        real.observe(0.02);
        real.observe(30.0);
        let text = exposition_with_histograms(
            "1.0.0",
            &[
                Series::labelled_counter(
                    "hydra_umc_hil_bridge_blocked_total",
                    "Blocked.",
                    ("reason", "interlock"),
                    2,
                ),
                Series::labelled_counter(
                    "hydra_umc_hil_bridge_blocked_total",
                    "Blocked.",
                    ("reason", "limits"),
                    1,
                ),
            ],
            &[HistogramSeries {
                name: "hydra_umc_hil_bridge_route_seconds",
                help: "Time to answer a route request.",
                label: ("mode", "real"),
                histogram: &real,
            }],
        );
        assert_valid_exposition(&text);
        assert_eq!(
            text.matches("# TYPE hydra_umc_hil_bridge_blocked_total")
                .count(),
            1
        );
        assert!(text.contains("hydra_umc_hil_bridge_blocked_total{reason=\"limits\"} 1"));
        assert!(text
            .contains("hydra_umc_hil_bridge_route_seconds_bucket{mode=\"real\",le=\"0.001\"} 1"));
        assert!(text
            .contains("hydra_umc_hil_bridge_route_seconds_bucket{mode=\"real\",le=\"0.025\"} 2"));
        assert!(
            text.contains("hydra_umc_hil_bridge_route_seconds_bucket{mode=\"real\",le=\"+Inf\"} 3")
        );
        assert!(text.contains("hydra_umc_hil_bridge_route_seconds_count{mode=\"real\"} 3"));
        assert_eq!(real.count(), 3);
        // a nonsense duration is counted as zero, not as NaN in the sum
        real.observe(f64::NAN);
        let again = exposition_with_histograms(
            "1",
            &[],
            &[HistogramSeries {
                name: "x_seconds",
                help: "x",
                label: ("mode", "real"),
                histogram: &real,
            }],
        );
        assert!(again.contains("x_seconds_count{mode=\"real\"} 4"));
        assert!(!again.contains("NaN"));
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
