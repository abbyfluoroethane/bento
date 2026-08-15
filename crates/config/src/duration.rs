//! A Go-compatible duration, so an existing `bento.toml` keeps working.
//!
//! The Go build parsed `name_cooldown` with `time.ParseDuration`, whose
//! syntax is a signed sequence of decimal numbers each with a unit
//! suffix: `"300ms"`, `"1h30m"`, `"-1.5h"`. Nothing in the Rust ecosystem
//! reproduces that grammar exactly, and a config file that stops parsing
//! on upgrade is worse than forty lines of parser.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Deserializer};

/// A duration in Go's `time.ParseDuration` syntax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct GoDuration(pub Duration);

impl GoDuration {
    /// The value as a [`std::time::Duration`].
    pub fn std(self) -> Duration {
        self.0
    }

    /// Whole seconds, for a `Retry-After` header or a log line.
    pub fn as_secs(self) -> u64 {
        self.0.as_secs()
    }
}

impl From<Duration> for GoDuration {
    fn from(d: Duration) -> Self {
        GoDuration(d)
    }
}

impl fmt::Display for GoDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.0.as_secs();
        let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
        match (h, m, s) {
            (0, 0, s) => write!(f, "{s}s"),
            (0, m, 0) => write!(f, "{m}m"),
            (0, m, s) => write!(f, "{m}m{s}s"),
            (h, 0, 0) => write!(f, "{h}h"),
            (h, m, 0) => write!(f, "{h}h{m}m"),
            (h, m, s) => write!(f, "{h}h{m}m{s}s"),
        }
    }
}

fn unit_nanos(unit: &str) -> Option<f64> {
    Some(match unit {
        "ns" => 1.0,
        "us" | "µs" | "μs" => 1_000.0,
        "ms" => 1_000_000.0,
        "s" => 1_000_000_000.0,
        "m" => 60.0 * 1_000_000_000.0,
        "h" => 3600.0 * 1_000_000_000.0,
        _ => return None,
    })
}

/// Parses Go's `time.ParseDuration` syntax. A negative result is an
/// error here: every duration Bento reads from the configuration is a
/// timeout or a cooldown, and neither has a meaning below zero.
pub fn parse_go_duration(input: &str) -> Result<Duration, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty duration".to_string());
    }
    // Go accepts "0" with no unit as the single exception.
    if s == "0" || s == "+0" || s == "-0" {
        return Ok(Duration::ZERO);
    }
    let (negative, mut rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if rest.is_empty() {
        return Err(format!("invalid duration {input:?}"));
    }

    let mut total_nanos = 0.0_f64;
    while !rest.is_empty() {
        let digits_end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let number = &rest[..digits_end];
        if number.is_empty() || number == "." {
            return Err(format!("invalid duration {input:?}: expected a number"));
        }
        let value: f64 = number
            .parse()
            .map_err(|_| format!("invalid duration {input:?}: {number:?} is not a number"))?;
        rest = &rest[digits_end..];

        let unit_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let unit = &rest[..unit_end];
        if unit.is_empty() {
            return Err(format!("invalid duration {input:?}: missing unit"));
        }
        let nanos_per = unit_nanos(unit)
            .ok_or_else(|| format!("invalid duration {input:?}: unknown unit {unit:?}"))?;
        total_nanos += value * nanos_per;
        rest = &rest[unit_end..];
    }

    if negative || total_nanos < 0.0 {
        return Err(format!("invalid duration {input:?}: must not be negative"));
    }
    Ok(Duration::from_nanos(total_nanos as u64))
}

impl<'de> Deserialize<'de> for GoDuration {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(de)?;
        parse_go_duration(&raw)
            .map(GoDuration)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_go_forms() {
        let cases = [
            ("24h", Duration::from_secs(24 * 3600)),
            ("48h", Duration::from_secs(48 * 3600)),
            ("1h30m", Duration::from_secs(5400)),
            ("300ms", Duration::from_millis(300)),
            ("1.5h", Duration::from_secs(5400)),
            ("90s", Duration::from_secs(90)),
            ("0", Duration::ZERO),
            ("2h45m30s", Duration::from_secs(9930)),
        ];
        for (input, want) in cases {
            assert_eq!(parse_go_duration(input).unwrap(), want, "input {input:?}");
        }
    }

    #[test]
    fn rejects_bad_input() {
        for input in ["soon", "", "12", "5x", "h", "-1h", "1h-30m"] {
            assert!(
                parse_go_duration(input).is_err(),
                "{input:?} should not parse"
            );
        }
    }

    #[test]
    fn displays_round_numbers() {
        assert_eq!(GoDuration(Duration::from_secs(86400)).to_string(), "24h");
        assert_eq!(GoDuration(Duration::from_secs(5400)).to_string(), "1h30m");
        assert_eq!(GoDuration(Duration::from_secs(45)).to_string(), "45s");
    }
}
