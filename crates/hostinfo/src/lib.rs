//! What the host has: memory and free space on a filesystem.
//!
//! Two callers read these numbers for different reasons. `bentod` reads
//! them once at startup to build the ceiling that a create or a resize is
//! checked against (SPEC 6.1). `bento-monitor` reads them every frame to
//! draw the operator screen.
//!
//! The parsers take text so that a test can supply a host it does not
//! have. Only the two `read_*` functions and [`disk_usage`] touch the
//! real host.

use std::ffi::CString;
use std::io;
use std::path::Path;

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

/// Reads the four `/proc/meminfo` lines that matter. A line the host does
/// not print stays zero.
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

/// Reads `/proc/meminfo` from the host.
pub fn read_memory() -> io::Result<Memory> {
    Ok(parse_meminfo(&std::fs::read_to_string("/proc/meminfo")?))
}

/// Free space on one filesystem, in bytes.
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_line_without_a_number_is_skipped() {
        let memory = parse_meminfo("MemTotal: nonsense\nMemAvailable: 50 kB\nbroken line\n");
        assert_eq!(memory.total, 0);
        assert_eq!(memory.available, 50 * 1024);
    }

    #[test]
    fn the_root_filesystem_answers_statvfs() {
        let disk = disk_usage(Path::new("/")).expect("statvfs of /");
        assert!(disk.total > 0);
        assert!(disk.available <= disk.total);
        assert!(disk_usage(Path::new("/no/such/place")).is_err());
    }
}
