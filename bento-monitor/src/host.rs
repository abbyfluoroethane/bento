//! What the host has left: processor load, memory, swap, and free space
//! on the two directories an instance grows into (SPEC 5.1, 5.3).
//!
//! Processor, memory, and filesystem readings come from `bento-hostinfo`,
//! which `bentod` also reads for the capacity ceiling (SPEC 6.1) and for
//! the dashboard charts (SPEC 14.4). What stays here is the part only
//! this screen needs: load, uptime, and the formatting.
//!
//! The parsers take text so that they can be tested without the host
//! they describe.

use std::path::Path;
use std::time::Duration;

pub use bento_hostinfo::{
    CpuTimes, Disk, Memory, busy_fraction, disk_usage, parse_cpu, parse_meminfo,
};

pub fn parse_loadavg(loadavg: &str) -> Option<[f64; 3]> {
    let mut fields = loadavg.split_whitespace();
    let mut load = [0.0; 3];
    for slot in &mut load {
        *slot = fields.next()?.parse().ok()?;
    }
    Some(load)
}

/// The first field of `/proc/uptime`, which the unit uptimes are measured
/// against.
pub fn parse_uptime(uptime: &str) -> Option<Duration> {
    let seconds: f64 = uptime.split_whitespace().next()?.parse().ok()?;
    Duration::try_from_secs_f64(seconds.max(0.0)).ok()
}

/// Every host reading of one frame. A field that the host does not answer
/// for stays `None` and the screen says so, rather than showing a zero
/// that reads as a real measurement.
#[derive(Debug, Clone, Default)]
pub struct HostSample {
    pub cpu: Option<CpuTimes>,
    pub busy: Option<f64>,
    pub cores: usize,
    pub memory: Memory,
    pub load: Option<[f64; 3]>,
    pub uptime: Option<Duration>,
    pub image_disk: Option<Disk>,
    pub storage_disk: Option<Disk>,
}

impl HostSample {
    /// Takes a reading. `previous` supplies the processor counters the
    /// busy share is measured against.
    pub fn take(previous: Option<&HostSample>, image_dir: &Path, storage_dir: &Path) -> Self {
        let cpu = std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|text| parse_cpu(&text));
        let busy = match (previous.and_then(|prev| prev.cpu), cpu) {
            (Some(before), Some(after)) => busy_fraction(before, after),
            _ => None,
        };
        HostSample {
            cpu,
            // A frame with no measurable processor time keeps the last
            // share, so the gauge does not blink to zero.
            busy: busy.or_else(|| previous.and_then(|prev| prev.busy)),
            cores: std::thread::available_parallelism().map_or(0, |count| count.get()),
            memory: std::fs::read_to_string("/proc/meminfo")
                .map(|text| parse_meminfo(&text))
                .unwrap_or_default(),
            load: std::fs::read_to_string("/proc/loadavg")
                .ok()
                .and_then(|text| parse_loadavg(&text)),
            uptime: std::fs::read_to_string("/proc/uptime")
                .ok()
                .and_then(|text| parse_uptime(&text)),
            image_disk: disk_usage(image_dir).ok(),
            storage_disk: disk_usage(storage_dir).ok(),
        }
    }
}

/// Bytes as an operator reads them. Binary units, because that is what
/// both libvirt and `qemu-img` report in.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A duration as an operator reads it: the two largest units that matter.
pub fn human_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let (days, hours, minutes) = (
        seconds / 86_400,
        (seconds % 86_400) / 3600,
        (seconds % 3600) / 60,
    );
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {}s", seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loadavg_and_uptime_read_their_first_fields() {
        assert_eq!(
            parse_loadavg("0.52 0.31 0.10 1/523 44"),
            Some([0.52, 0.31, 0.10])
        );
        assert_eq!(parse_loadavg("nonsense"), None);
        assert_eq!(
            parse_uptime("3600.12 7000.00"),
            Some(Duration::from_secs_f64(3600.12))
        );
    }

    #[test]
    fn sizes_and_durations_read_as_an_operator_writes_them() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(512 * 1024 * 1024), "512 MiB");
        assert_eq!(human_duration(Duration::from_secs(45)), "45s");
        assert_eq!(human_duration(Duration::from_secs(3661)), "1h 1m");
        assert_eq!(human_duration(Duration::from_secs(90_061)), "1d 1h");
    }
}
