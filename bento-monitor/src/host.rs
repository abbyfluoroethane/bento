//! What the host has left: processor load, memory, swap, and free space
//! on the two directories an instance grows into (SPEC 5.1, 5.3).
//!
//! Everything here reads `/proc` or `statvfs`. The parsers take text so
//! that they can be tested without the host they describe.

use std::ffi::CString;
use std::io;
use std::path::Path;
use std::time::Duration;

/// One reading of the aggregate processor counters of `/proc/stat`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub total: u64,
    pub idle: u64,
}

/// Reads the `cpu` line, the one that sums every core.
pub fn parse_cpu(stat: &str) -> Option<CpuTimes> {
    let line = stat.lines().find(|line| line.starts_with("cpu "))?;
    let fields: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.parse().ok())
        .collect();
    if fields.len() < 5 {
        return None;
    }
    // Fields 4 and 5 are idle and iowait. A processor waiting for a disk
    // is not doing work, so both count as idle.
    Some(CpuTimes {
        total: fields.iter().sum(),
        idle: fields[3] + fields[4],
    })
}

/// The busy share between two readings, from 0.0 to 1.0. Returns `None`
/// while no time has passed, which is the state of the first frame.
pub fn busy_fraction(before: CpuTimes, after: CpuTimes) -> Option<f64> {
    let total = after.total.checked_sub(before.total)?;
    let idle = after.idle.checked_sub(before.idle)?;
    if total == 0 {
        return None;
    }
    Some((total.saturating_sub(idle) as f64 / total as f64).clamp(0.0, 1.0))
}

/// Memory and swap, in bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Memory {
    pub total: u64,
    /// `MemAvailable`, the kernel's own estimate of what a new workload
    /// can take. It is the number an overcommit decision needs, not
    /// `MemFree`.
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

impl Memory {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }

    pub fn swap_used(&self) -> u64 {
        self.swap_total.saturating_sub(self.swap_free)
    }
}

pub fn parse_meminfo(meminfo: &str) -> Memory {
    let mut memory = Memory::default();
    for line in meminfo.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let Some(kib) = rest
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u64>().ok())
        else {
            continue;
        };
        let bytes = kib.saturating_mul(1024);
        match key {
            "MemTotal" => memory.total = bytes,
            "MemAvailable" => memory.available = bytes,
            "SwapTotal" => memory.swap_total = bytes,
            "SwapFree" => memory.swap_free = bytes,
            _ => {}
        }
    }
    memory
}

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

/// Free space on one filesystem.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Disk {
    pub total: u64,
    pub available: u64,
}

impl Disk {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }
}

/// Reads the filesystem that holds `path`. The available count is the
/// unprivileged one, because the space a reserve holds back is not space
/// an overlay disk can grow into.
pub fn disk_usage(path: &Path) -> io::Result<Disk> {
    let c_path = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::other("path contains a NUL byte"))?;
    let mut stats: libc::statvfs = unsafe { std::mem::zeroed() };
    // Safety: `statvfs` writes into the struct above and reads a
    // NUL-terminated path that outlives the call.
    let code = unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) };
    if code != 0 {
        return Err(io::Error::last_os_error());
    }
    let block = stats.f_frsize as u64;
    Ok(Disk {
        total: block.saturating_mul(stats.f_blocks as u64),
        available: block.saturating_mul(stats.f_bavail as u64),
    })
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

    const STAT: &str = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 50 0 25 400 25 0 0 0 0 0\nintr 1\n";

    #[test]
    fn the_aggregate_processor_line_counts_iowait_as_idle() {
        let times = parse_cpu(STAT).expect("cpu line");
        assert_eq!(times.total, 1000);
        assert_eq!(times.idle, 850);
    }

    #[test]
    fn the_busy_share_comes_from_the_difference_of_two_readings() {
        let before = CpuTimes {
            total: 1000,
            idle: 850,
        };
        let after = CpuTimes {
            total: 2000,
            idle: 1600,
        };
        assert_eq!(busy_fraction(before, after), Some(0.25));
        // Two identical readings measure nothing, rather than 0 percent.
        assert_eq!(busy_fraction(after, after), None);
        // Counters that went backwards, as they do after a suspend, are
        // refused instead of wrapping into a huge share.
        assert_eq!(busy_fraction(after, before), None);
    }

    #[test]
    fn meminfo_reads_in_bytes() {
        let memory = parse_meminfo(
            "MemTotal:       16384 kB\nMemFree:  1024 kB\nMemAvailable:    8192 kB\nSwapTotal: 2048 kB\nSwapFree: 1024 kB\n",
        );
        assert_eq!(memory.total, 16_384 * 1024);
        assert_eq!(memory.available, 8192 * 1024);
        assert_eq!(memory.used(), 8192 * 1024);
        assert_eq!(memory.swap_used(), 1024 * 1024);
    }

    #[test]
    fn a_host_without_swap_reports_none_used() {
        let memory = parse_meminfo("MemTotal: 100 kB\nMemAvailable: 50 kB\n");
        assert_eq!(memory.swap_total, 0);
        assert_eq!(memory.swap_used(), 0);
    }

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
    fn the_root_filesystem_answers_statvfs() {
        let disk = disk_usage(Path::new("/")).expect("statvfs of /");
        assert!(disk.total > 0);
        assert!(disk.available <= disk.total);
        assert!(disk_usage(Path::new("/no/such/place")).is_err());
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
