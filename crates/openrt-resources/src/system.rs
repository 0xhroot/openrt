//! Host system information collected from `/proc` (+ one `statvfs` call).
//!
//! All parsers are pure string functions so they can be unit-tested without a
//! live environment; `SystemInfo::collect` wires them to real files.

use std::ffi::CString;
use std::path::Path;

use crate::ResourceError;

/// Result of `parse_meminfo`/`/proc/meminfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemInfo {
    pub total_kb: u64,
    pub available_kb: u64,
}

impl MemInfo {
    /// Parse `/proc/meminfo` text into the two fields OpenRT uses.
    pub fn parse(text: &str) -> Result<Self, ResourceError> {
        let mut total = None;
        let mut available = None;
        for line in text.lines() {
            let mut it = line.split_whitespace();
            let Some(key) = it.next() else { continue };
            let Some(value) = it.next().and_then(|v| v.parse::<u64>().ok()) else {
                continue;
            };
            match key.trim_end_matches(':') {
                "MemTotal" => total = Some(value),
                "MemAvailable" => available = Some(value),
                _ => {}
            }
        }
        let total_kb = total
            .ok_or_else(|| ResourceError::Unavailable("meminfo".into(), "no MemTotal".into()))?;
        let available_kb = available.unwrap_or(total_kb);
        Ok(Self {
            total_kb,
            available_kb,
        })
    }
}

/// Computed from `/proc/cpuinfo`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CpuInfo {
    pub count: usize,
    pub model: Option<String>,
}

impl CpuInfo {
    pub fn parse(text: &str) -> Self {
        let mut count = 0usize;
        let mut model = None;
        for line in text.lines() {
            let mut it = line.splitn(2, ':');
            let Some(key) = it.next() else { continue };
            let Some(value) = it.next() else { continue };
            let k = key.trim();
            let v = value.trim().to_string();
            if k == "processor" {
                count += 1;
            } else if k == "model name" && model.is_none() {
                model = Some(v);
            }
        }
        if count == 0 {
            // Some kernels expose `cpu` lines instead of `processor`.
            for line in text.lines() {
                if line.trim_start().starts_with("processor") {
                    count += 1;
                }
            }
        }
        Self { count, model }
    }
}

/// Result of `statvfs` on a mount point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskUsage {
    pub total_bytes: u64,
    pub avail_bytes: u64,
}

/// Query disk space via the POSIX `statvfs` syscall.
///
/// # Safety
/// `path` must be a valid NUL-terminated path; the syscall writes into a
/// zero-initialized struct we own. Only the standard syscall path with
/// `io::Error::last_os_error` is used.
pub fn statvfs_avail(path: &str) -> Result<DiskUsage, ResourceError> {
    let c_path =
        CString::new(path).map_err(|e| ResourceError::Io(format!("invalid path {path:?}: {e}")))?;
    // SAFETY: `c_path` is a valid NUL-terminated path buffer and `buf` is a
    // zeroed struct of the correct size owned by this frame.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `statvfs` is memory-safe given a valid path and writable buf;
    // it may be interrupted by EINTR, handled through `last_os_error`.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(ResourceError::Io(format!("statvfs({path}): {err}")));
    }
    let bsize = buf.f_bsize.max(1) as u64;
    Ok(DiskUsage {
        total_bytes: (buf.f_blocks as u64).saturating_mul(bsize),
        avail_bytes: (buf.f_bavail as u64).saturating_mul(bsize),
    })
}

fn read_proc(name: &str) -> Result<String, ResourceError> {
    let path = Path::new("/proc").join(name);
    std::fs::read_to_string(&path)
        .map_err(|e| ResourceError::Unavailable(path.display().to_string(), e.to_string()))
}

/// A snapshot of host state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemInfo {
    /// OS via `std::env::consts::OS` (e.g. `linux`).
    pub os: String,
    /// Kernel release from `/proc/sys/kernel/osrelease`.
    pub kernel: String,
    /// `std::env::consts::ARCH`.
    pub arch: String,
    pub cpu: CpuInfo,
    /// Uptime in whole seconds (`/proc/uptime`).
    pub uptime_secs: u64,
    pub mem: MemInfo,
    pub disk: DiskUsage,
}

impl Default for SystemInfo {
    fn default() -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            kernel: String::new(),
            arch: std::env::consts::ARCH.to_string(),
            cpu: CpuInfo::default(),
            uptime_secs: 0,
            mem: MemInfo {
                total_kb: 0,
                available_kb: 0,
            },
            disk: DiskUsage {
                total_bytes: 0,
                avail_bytes: 0,
            },
        }
    }
}

impl SystemInfo {
    /// Collect a best-effort snapshot. Missing optional fields degrade to
    /// sane defaults rather than failing the whole attempt.
    pub fn collect() -> Self {
        let mem = read_proc("meminfo")
            .ok()
            .as_deref()
            .and_then(|t| MemInfo::parse(t).ok())
            .unwrap_or(MemInfo {
                total_kb: 0,
                available_kb: 0,
            });
        let cpu = read_proc("cpuinfo")
            .ok()
            .as_deref()
            .map(CpuInfo::parse)
            .unwrap_or_default();
        let kernel = read_proc("sys/kernel/osrelease")
            .unwrap_or_default()
            .trim()
            .to_string();
        let uptime_secs = read_proc("uptime")
            .ok()
            .and_then(|t| {
                t.split_whitespace()
                    .next()
                    .and_then(|x| x.parse::<f64>().ok())
            })
            .map(|s| s as u64)
            .unwrap_or(0);
        let disk = statvfs_avail("/").unwrap_or(DiskUsage {
            total_bytes: 0,
            avail_bytes: 0,
        });
        Self {
            os: std::env::consts::OS.to_string(),
            kernel,
            arch: std::env::consts::ARCH.to_string(),
            cpu,
            uptime_secs,
            mem,
            disk,
        }
    }

    /// Human-readable heartbeat line (used by `doctor` / `--verbose`).
    pub fn summary(&self) -> String {
        format!(
            "{} {} ({}), {} cpu(s), mem {} MB free / {} MB, disk {} MB free, up {}s",
            self.os,
            self.kernel,
            self.arch,
            self.cpu.count,
            self.mem.available_kb / 1024,
            self.mem.total_kb / 1024,
            self.disk.avail_bytes / 1024 / 1024,
            self.uptime_secs,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpuinfo_pure() {
        let text = "processor\t: 0\nmodel name\t: AMD EPYC\n";
        assert_eq!(CpuInfo::parse(text).count, 1);
        assert_eq!(CpuInfo::parse(text).model.as_deref(), Some("AMD EPYC"));
        assert_eq!(CpuInfo::parse("").count, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn collect_succeeds() {
        let si = SystemInfo::collect();
        assert_eq!(si.os, "linux");
        assert!(si.mem.total_kb > 0);
        assert!(si.disk.total_bytes > 0);
    }
}
