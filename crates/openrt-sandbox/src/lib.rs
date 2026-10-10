//! Sandbox + isolated execution fabric (Phase 2 M2.4 python sandbox extended
//! by Phase 8).
//!
//! Two layers, one authority:
//!
//! * **Legacy python surface** — [`PythonSandbox`] / [`PythonRun`] run
//!   untrusted Python in a confined interpreter (bwrap namespaces or the
//!   explicit unconfined subprocess fallback) with rlimits, wall timeout and
//!   output caps. Kept for compatibility; `python.run` production wiring now
//!   goes through the unified manager below when one is attached.
//! * **Unified sandbox authority (Phase 8)** — [`SandboxManager`] admits a
//!   [`SandboxRequest`] against a [`SandboxPolicy`] (profile), the host
//!   [`openrt_resources::ResourceGate`], the mode/netpol gate, and backend
//!   detection; executes with enforced mounts/network/limits; verifies
//!   cleanup (PID-identity-safe kill, workspace removal) and records
//!   everything in `sandbox.instances` + the chained audit log.
//!
//! Honest capability reporting: [`SandboxManager::capabilities`] classifies
//! bubblewrap / Docker / systemd-nspawn / syscall filtering as
//! `available`/`partial`/`permission_required`/`unavailable` with reasons —
//! never a fake backend and never a silent isolation downgrade.
//!
//! The crate never uses `unsafe` (workspace-wide `unsafe_code = "forbid"`).

mod backend;
mod executor;
mod limits;
mod manager;
mod model;
mod policy;
mod sandbox;

pub use backend::{
    detect_capabilities, detect_syscall_support, docker_status, nspawn_status, resolve_backend,
    BackendKind, BackendStatus, SandboxCapabilities, SyscallSupport,
};
pub use limits::{SandboxLimits, DEFAULT_LIMITS};
pub use manager::{capability_rows, Admission, SandboxManager, SANDBOX_NS};
pub use model::{
    CapStatus, CleanupOutcome, CleanupPolicy, IsolationLevel, Mount, NetworkMode,
    SandboxCapability, SandboxErrorCode, SandboxFailure, SandboxId, SandboxInstanceRecord,
    SandboxRequest, SandboxResult, SandboxState,
};
pub use policy::{SandboxPolicy, SandboxProfile};
pub use sandbox::{PythonRun, PythonSandbox, SandboxBackend, SandboxError, SandboxOutcome};
