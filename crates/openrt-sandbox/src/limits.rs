//! Tunables for the Python sandbox.

use std::time::Duration;

/// Resource limits applied to the sandboxed interpreter process.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SandboxLimits {
    /// Wall-clock timeout; the sandbox process is killed (SIGKILL) on expiry.
    pub wall_timeout: Duration,
    /// Maximum resident-set / address-space size in bytes (RLIMIT_AS).
    pub memory_bytes: u64,
    /// Cumulative CPU seconds before the kernel sends SIGXCPU (RLIMIT_CPU).
    pub cpu_seconds: u32,
    /// Maximum number of processes / threads for the uid (RLIMIT_NPROC).
    pub max_procs: u32,
    /// Maximum captured output bytes (stdout + stderr, per-channel).
    pub output_bytes: usize,
}

/// Defaults: 30 s wall, 1 GiB memory, 10 CPU seconds, 32 processes,
/// 64 KiB output.
pub const DEFAULT_LIMITS: SandboxLimits = SandboxLimits {
    wall_timeout: Duration::from_secs(30),
    memory_bytes: 1024 * 1024 * 1024,
    cpu_seconds: 10,
    max_procs: 32,
    output_bytes: 64 * 1024,
};

impl Default for SandboxLimits {
    fn default() -> Self {
        DEFAULT_LIMITS
    }
}
