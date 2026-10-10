//! Unified sandbox model (Phase 8, M8.1): authoritative identity, execution
//! requests, explicit lifecycle states, and the normalized failure codes the
//! phase requires (WHAT/WHY/WHERE/FIX).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::limits::SandboxLimits;

/// Authoritative sandbox instance identity (uuid v4).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SandboxId(pub String);

impl SandboxId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for SandboxId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SandboxId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Explicit lifecycle state of a sandbox instance. Denial states are terminal
/// *before* execution; a denial never reaches `Running`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxState {
    /// Instance allocated, not yet admitted.
    Created,
    /// Passed policy + resource + backend admission; may execute.
    Admitted,
    /// A child process is (or was) executing the payload.
    Running,
    /// Payload exited 0.
    Succeeded,
    /// Payload exited non-zero (or the executor failed after admission).
    Failed,
    /// Wall-clock timeout hit; the process tree was killed.
    Timeout,
    /// Killed deliberately (kill switch, output flood, operator destroy).
    Killed,
    /// Backend/profile could not be satisfied (deny-by-default).
    AdmissionDenied,
    /// Policy violation (network mode, mounts, env, isolation requirement).
    PolicyDenied,
    /// Host resource admission (`ResourceGate`) denied.
    ResourceDenied,
    /// Workspace destroyed and verified (or destroy attempted).
    Destroyed,
}

impl SandboxState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Admitted => "admitted",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::Killed => "killed",
            Self::AdmissionDenied => "admission_denied",
            Self::PolicyDenied => "policy_denied",
            Self::ResourceDenied => "resource_denied",
            Self::Destroyed => "destroyed",
        }
    }

    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Created | Self::Admitted | Self::Running)
    }

    pub fn is_denial(&self) -> bool {
        matches!(
            self,
            Self::AdmissionDenied | Self::PolicyDenied | Self::ResourceDenied
        )
    }
}

/// Network mode requested for a run. Each mode reports the mechanism that
/// enforces it (`IsolationLevel`) — never a stronger claim than reality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    /// No network at all: kernel network namespace without configured
    /// interfaces (TCP, UDP and DNS all fail).
    None,
    /// Only loopback *inside* the sandbox network namespace; host network and
    /// DNS unreachable.
    LocalOnly,
    /// Host network, but connections are intercepted and checked against an
    /// allowlist (interpreter-level enforcement for python payloads — kernel
    /// netfilter allowlisting is not available unprivileged).
    Allowlist,
    /// Host network (still gated by execution mode + netpol at admission).
    Full,
}

impl NetworkMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::LocalOnly => "local_only",
            Self::Allowlist => "allowlist",
            Self::Full => "full",
        }
    }
}

/// Highest isolation mechanism actually enforced for a run. Reported in every
/// [`SandboxResult`] so no caller can mistake a degraded run for a confined
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationLevel {
    /// Nothing enforced (refused by default — only reachable via explicit
    /// unconfined-fallback opt-in).
    None,
    /// Interpreter-level enforcement (e.g. python audit hooks).
    Interpreter,
    /// Clean env + timeout + output caps, no OS confinement.
    Process,
    /// Kernel namespaces (user/pid/ipc/net as configured).
    Namespace,
    /// Full container engine.
    Container,
}

impl IsolationLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Interpreter => "interpreter",
            Self::Process => "process",
            Self::Namespace => "namespace",
            Self::Container => "container",
        }
    }
}

/// A policy-declared mount inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub source: PathBuf,
    pub target: PathBuf,
    pub read_only: bool,
}

impl Mount {
    pub fn ro(source: impl Into<PathBuf>, target: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            read_only: true,
        }
    }

    pub fn rw(source: impl Into<PathBuf>, target: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            target: target.into(),
            read_only: false,
        }
    }
}

/// What happens to the scratch workspace after the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupPolicy {
    /// Remove the workspace and verify it is gone (default; the sandbox is
    /// disposable).
    Disposable,
    /// Keep artifacts for inspection; the record stays until `destroy()`.
    Keep,
}

impl Default for CleanupPolicy {
    fn default() -> Self {
        Self::Disposable
    }
}

/// One execution request against the sandbox authority.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxRequest {
    /// Program to execute (absolute path preferred; resolved against the
    /// sandbox PATH otherwise).
    pub program: String,
    /// Program arguments.
    pub args: Vec<String>,
    /// Files materialized into the scratch workspace before execution
    /// (name → content). Names must be single path components.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
    /// Working directory *inside the workspace* (`.` = workspace root).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Explicit environment entries. Every key must be on the policy
    /// allowlist; ambient host environment is never inherited.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Additional policy-validated mounts (the workspace is always mounted).
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// Requested network mode.
    pub network: NetworkMode,
    /// Hosts/globs allowed in [`NetworkMode::Allowlist`] (python payloads
    /// only; enforcement is interpreter-level).
    #[serde(default)]
    pub network_allowlist: Vec<String>,
    /// Requested limits (clamped by the policy maximums).
    pub limits: SandboxLimits,
    /// Minimum isolation the caller will accept; admission fails if no
    /// backend can provide it.
    #[serde(default)]
    pub isolation_required: Option<IsolationLevel>,
    /// Require a kernel syscall filter (seccomp); admission fails honestly
    /// with `SYSCALL_DENIED` when the workspace cannot provide one.
    #[serde(default)]
    pub require_syscall_filter: bool,
    /// Workspace lifecycle.
    #[serde(default)]
    pub cleanup: CleanupPolicy,
    /// Parent run (task/workflow/autonomous run) recorded on the instance.
    #[serde(default)]
    pub parent: Option<String>,
}

impl SandboxRequest {
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            files: BTreeMap::new(),
            cwd: None,
            env: BTreeMap::new(),
            mounts: Vec::new(),
            network: NetworkMode::None,
            network_allowlist: Vec::new(),
            limits: SandboxLimits::default(),
            isolation_required: None,
            require_syscall_filter: false,
            cleanup: CleanupPolicy::Disposable,
            parent: None,
        }
    }

    /// Convenience: run a python script payload (`script.py` in the
    /// workspace) with `python -I`.
    pub fn python_script(python: impl Into<String>, script: impl Into<String>) -> Self {
        let mut req = Self::new(python, ["-I".to_string(), "script.py".to_string()]);
        req.files.insert("script.py".into(), script.into());
        req
    }

    pub fn with_limits(mut self, limits: SandboxLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_network(
        mut self,
        mode: NetworkMode,
        allowlist: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.network = mode;
        self.network_allowlist = allowlist.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_parent(mut self, parent: impl Into<String>) -> Self {
        self.parent = Some(parent.into());
        self
    }

    pub fn with_cleanup(mut self, cleanup: CleanupPolicy) -> Self {
        self.cleanup = cleanup;
        self
    }

    pub fn with_mount(mut self, mount: Mount) -> Self {
        self.mounts.push(mount);
        self
    }
}

/// Normalized sandbox failure codes. Every denial maps to exactly one of
/// these and carries WHAT/WHY/WHERE/FIX.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SandboxErrorCode {
    AdmissionDenied,
    PolicyDenied,
    FilesystemDenied,
    NetworkDenied,
    ResourceDenied,
    Timeout,
    ProcessLimit,
    SyscallDenied,
    ContainerUnavailable,
    BackendUnavailable,
    CleanupFailed,
    VerificationFailed,
}

impl SandboxErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AdmissionDenied => "ADMISSION_DENIED",
            Self::PolicyDenied => "POLICY_DENIED",
            Self::FilesystemDenied => "FILESYSTEM_DENIED",
            Self::NetworkDenied => "NETWORK_DENIED",
            Self::ResourceDenied => "RESOURCE_DENIED",
            Self::Timeout => "TIMEOUT",
            Self::ProcessLimit => "PROCESS_LIMIT",
            Self::SyscallDenied => "SYSCALL_DENIED",
            Self::ContainerUnavailable => "CONTAINER_UNAVAILABLE",
            Self::BackendUnavailable => "BACKEND_UNAVAILABLE",
            Self::CleanupFailed => "CLEANUP_FAILED",
            Self::VerificationFailed => "VERIFICATION_FAILED",
        }
    }

    /// Lifecycle state recorded for this failure class.
    pub fn state(&self) -> SandboxState {
        match self {
            Self::ResourceDenied => SandboxState::ResourceDenied,
            Self::AdmissionDenied
            | Self::PolicyDenied
            | Self::FilesystemDenied
            | Self::NetworkDenied
            | Self::SyscallDenied
            | Self::ContainerUnavailable
            | Self::BackendUnavailable
            | Self::ProcessLimit => SandboxState::AdmissionDenied,
            Self::Timeout => SandboxState::Timeout,
            Self::CleanupFailed | Self::VerificationFailed => SandboxState::Failed,
        }
    }
}

/// Structured sandbox failure: WHAT went wrong, WHY, WHERE, FIX — plus the
/// normalized code and the lifecycle state a denial maps to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxFailure {
    pub code: SandboxErrorCode,
    pub what: String,
    pub why: String,
    pub where_: String,
    pub fix: String,
    pub state: SandboxState,
}

impl SandboxFailure {
    pub fn new(
        code: SandboxErrorCode,
        what: impl Into<String>,
        why: impl Into<String>,
        where_: impl Into<String>,
        fix: impl Into<String>,
    ) -> Self {
        let state = code.state();
        Self {
            code,
            what: what.into(),
            why: why.into(),
            where_: where_.into(),
            fix: fix.into(),
            state,
        }
    }

    /// Single-line rendering used by tool/CLI error surfaces.
    pub fn render(&self) -> String {
        format!(
            "{}: {} (why: {}; where: {}; fix: {})",
            self.code.as_str(),
            self.what,
            self.why,
            self.where_,
            self.fix
        )
    }
}

impl std::fmt::Display for SandboxFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render())
    }
}

impl std::error::Error for SandboxFailure {}

/// Cleanup verification outcome for a (disposable) workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupOutcome {
    /// True only when every check below passed.
    pub verified: bool,
    /// No sandbox process remains (PID-identity-checked).
    pub processes_gone: bool,
    /// Workspace directory removed (or never created).
    pub workspace_removed: bool,
    /// Human-readable detail when anything failed.
    #[serde(default)]
    pub detail: Option<String>,
}

impl CleanupOutcome {
    pub fn ok(processes_gone: bool, workspace_removed: bool) -> Self {
        Self {
            verified: processes_gone && workspace_removed,
            processes_gone,
            workspace_removed,
            detail: None,
        }
    }

    pub fn failed(detail: impl Into<String>) -> Self {
        Self {
            verified: false,
            processes_gone: false,
            workspace_removed: false,
            detail: Some(detail.into()),
        }
    }
}

/// Terminal result of one sandboxed execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxResult {
    pub id: SandboxId,
    pub state: SandboxState,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// Output cap hit (flood turned into a controlled kill).
    pub output_limited: bool,
    /// Why the process was killed, when it was (`output_limit`, halt reason…).
    #[serde(default)]
    pub kill_reason: Option<String>,
    pub backend: crate::sandbox::SandboxBackend,
    /// Highest isolation actually enforced for this run.
    pub isolation_level: IsolationLevel,
    pub network: NetworkMode,
    /// Policy digest admitted for this run (integrity of the policy inputs).
    pub policy_digest: String,
    pub profile: String,
    /// Scratch workspace path (already removed when `cleanup.verified`).
    pub workspace: Option<PathBuf>,
    pub cleanup: CleanupOutcome,
    pub started_at: String,
    pub ended_at: String,
    pub wall_ms: u64,
}

/// Persisted instance record (`sandbox.instances` namespace, ADR-006).
/// Contains identity/policy facts only — never secrets or env values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxInstanceRecord {
    pub id: SandboxId,
    pub profile: String,
    pub backend: String,
    pub network: NetworkMode,
    pub isolation_level: IsolationLevel,
    pub policy_digest: String,
    pub state: SandboxState,
    #[serde(default)]
    pub parent: Option<String>,
    pub actor: String,
    pub created_at: String,
    #[serde(default)]
    pub ended_at: Option<String>,
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub failure: Option<SandboxFailure>,
    /// Process identity recorded at spawn (for identity-checked destroy).
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub starttime: Option<String>,
}

/// Capability availability for one sandbox feature/backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum CapStatus {
    Available,
    Unavailable {
        reason: String,
    },
    PermissionRequired {
        reason: String,
    },
    Unsupported {
        reason: String,
    },
    /// Usable but with a weaker mechanism than the ideal one.
    Partial {
        reason: String,
    },
}

impl CapStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unavailable { .. } => "unavailable",
            Self::PermissionRequired { .. } => "permission_required",
            Self::Unsupported { .. } => "unsupported",
            Self::Partial { .. } => "partial",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Available => None,
            Self::Unavailable { reason }
            | Self::PermissionRequired { reason }
            | Self::Unsupported { reason }
            | Self::Partial { reason } => Some(reason),
        }
    }

    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }
}

/// One named capability in the sandbox capability matrix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCapability {
    pub name: String,
    pub status: CapStatus,
}
