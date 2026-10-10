//! Backend detection and resolution (M8.5 container execution, M8.7 syscall
//! hardening): honest capability reporting — no fake backends, no silent
//! downgrades.

use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::model::{
    CapStatus, IsolationLevel, NetworkMode, SandboxErrorCode, SandboxFailure, SandboxRequest,
};
use crate::policy::SandboxPolicy;
use crate::sandbox::SandboxBackend;
use crate::PythonSandbox;

/// Execution backend a sandbox run can be dispatched to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// bubblewrap: kernel namespaces (user/pid/ipc/net) as rootless confinement.
    Bubblewrap,
    /// Degraded process isolation: clean env + timeout + output caps only.
    Subprocess,
    /// Docker container engine (requires a running daemon).
    Docker,
    /// systemd-nspawn (typically root/userns configured).
    SystemdNspawn,
}

impl BackendKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bubblewrap => "bubblewrap",
            Self::Subprocess => "subprocess",
            Self::Docker => "docker",
            Self::SystemdNspawn => "systemd_nspawn",
        }
    }

    /// Map to the existing two-valued backend enum used by outcomes
    /// (`SandboxOutcome`/`SandboxResult` stay backward compatible).
    pub fn to_outcome_backend(self) -> SandboxBackend {
        match self {
            Self::Bubblewrap | Self::Docker | Self::SystemdNspawn => SandboxBackend::Bubblewrap,
            Self::Subprocess => SandboxBackend::Subprocess,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendStatus {
    pub kind: BackendKind,
    pub status: CapStatus,
}

/// Kernel/security hardening support detected on this host (M8.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyscallSupport {
    /// Kernel supports seccomp (`/proc/self/status: Seccomp >= 1`).
    pub seccomp_kernel: bool,
    /// Our own process runs under full seccomp filtering (`Seccomp: 2`).
    pub seccomp_enforced: bool,
    /// `no_new_privs` is set for this process.
    pub no_new_privs: bool,
    /// Kernel reports a Landlock ABI (uname >= 5.13).
    pub landlock_kernel: bool,
    /// Whether *this workspace* can install a syscall filter for sandboxes.
    pub filter_enforcement: CapStatus,
}

/// Full sandbox capability matrix (structured, machine-readable).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    pub backends: Vec<BackendStatus>,
    pub network_modes: Vec<(NetworkMode, CapStatus)>,
    pub syscall: SyscallSupport,
    /// Loopback inside `local_only` can be brought up (best effort).
    pub local_only_loopback: CapStatus,
}

fn run_ok(bin: &str, args: &[&str]) -> bool {
    matches!(Command::new(bin).args(args).output(), Ok(o) if o.status.success())
}

fn run_output(bin: &str, args: &[&str]) -> Option<String> {
    match Command::new(bin).args(args).output() {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).trim().to_string()),
        _ => None,
    }
}

/// Docker daemon probe: the CLI existing is *not* enough — `docker info`
/// talks to the daemon and fails fast when it is not running.
pub fn docker_status() -> CapStatus {
    match Command::new("docker").arg("info").output() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CapStatus::Unavailable {
            reason: "docker CLI not installed".into(),
        },
        Err(e) => CapStatus::Unavailable {
            reason: format!("docker info failed to run: {e}"),
        },
        Ok(o) if o.status.success() => CapStatus::Available,
        Ok(o) => {
            let detail = String::from_utf8_lossy(&o.stderr);
            let detail = detail
                .lines()
                .last()
                .unwrap_or("daemon unreachable")
                .to_string();
            CapStatus::Unavailable {
                reason: format!("docker daemon not running ({detail})"),
            }
        }
    }
}

pub fn nspawn_status() -> CapStatus {
    if run_ok("systemd-nspawn", &["--version"]) {
        CapStatus::PermissionRequired {
            reason: "systemd-nspawn binary present; running containers needs root or a \
                     configured user namespace (not usable unprivileged on this host)"
                .into(),
        }
    } else {
        CapStatus::Unavailable {
            reason: "systemd-nspawn not installed".into(),
        }
    }
}

fn kernel_release() -> String {
    run_output("uname", &["-r"]).unwrap_or_default()
}

fn kernel_ge(major: u32, minor: u32) -> bool {
    let rel = kernel_release();
    let mut it = rel.split('.');
    let Some(maj) = it.next().and_then(|s| s.parse::<u32>().ok()) else {
        return false;
    };
    let min = it.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
    maj > major || (maj == major && min >= minor)
}

fn parse_proc_status(key: &str) -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    text.lines().find_map(|l| {
        l.strip_prefix(key)
            .and_then(|v| v.trim().parse::<String>().ok())
    })
}

pub fn detect_syscall_support() -> SyscallSupport {
    let seccomp = parse_proc_status("Seccomp:").and_then(|v| v.parse::<u8>().ok());
    let nnp = parse_proc_status("NoNewPrivs:").is_some_and(|v| v == "1");
    SyscallSupport {
        seccomp_kernel: seccomp.is_some_and(|m| m >= 1),
        seccomp_enforced: seccomp == Some(2),
        no_new_privs: nnp,
        landlock_kernel: kernel_ge(5, 13),
        // Honest: the offline workspace forbids `unsafe` and ships no
        // seccomp-BPF/landlock binding, so we cannot install a filter
        // ourselves (kernel support ≠ our ability to use it).
        filter_enforcement: CapStatus::Unavailable {
            reason: "no seccomp-BPF/Landlock binding available offline (workspace \
                     unsafe_code=forbid; no libseccomp/landlock crate)"
                .into(),
        },
    }
}

/// Detect the full capability matrix for this host.
pub fn detect_capabilities() -> SandboxCapabilities {
    let bwrap = PythonSandbox::is_bwrap_available();
    let bwrap_status = if bwrap {
        CapStatus::Available
    } else {
        CapStatus::Unavailable {
            reason: "bubblewrap (`bwrap`) not installed — kernel namespace confinement \
                     unavailable"
                .into(),
        }
    };
    let netns_reason = "requires kernel network namespaces (bubblewrap); not enforceable \
                        without it"
        .to_string();
    SandboxCapabilities {
        backends: vec![
            BackendStatus {
                kind: BackendKind::Bubblewrap,
                status: bwrap_status,
            },
            BackendStatus {
                kind: BackendKind::Subprocess,
                status: CapStatus::Partial {
                    reason: "degraded: clean env + timeout + output caps, no OS confinement \
                             (explicit policy opt-in only)"
                        .into(),
                },
            },
            BackendStatus {
                kind: BackendKind::Docker,
                status: docker_status(),
            },
            BackendStatus {
                kind: BackendKind::SystemdNspawn,
                status: nspawn_status(),
            },
        ],
        network_modes: vec![
            (
                NetworkMode::None,
                if bwrap {
                    CapStatus::Available
                } else {
                    CapStatus::Unavailable {
                        reason: netns_reason.clone(),
                    }
                },
            ),
            (
                NetworkMode::LocalOnly,
                if bwrap {
                    CapStatus::Partial {
                        reason: "kernel network namespace; loopback brought up best-effort \
                                 (in-sandbox only, host unreachable, DNS unreachable)"
                            .into(),
                    }
                } else {
                    CapStatus::Unavailable {
                        reason: netns_reason.clone(),
                    }
                },
            ),
            (
                NetworkMode::Allowlist,
                CapStatus::Partial {
                    reason: "interpreter-level enforcement for python payloads (audit \
                             hooks block non-allowlisted getaddrinfo/connect); kernel-level \
                             allowlisting needs netfilter/root — UNAVAILABLE unprivileged"
                        .into(),
                },
            ),
            (NetworkMode::Full, CapStatus::Available),
        ],
        syscall: detect_syscall_support(),
        local_only_loopback: if bwrap {
            CapStatus::Partial {
                reason: "loopback is brought up best-effort inside the namespace; host \
                         stays unreachable"
                    .into(),
            }
        } else {
            CapStatus::Unavailable {
                reason: "needs a per-run network namespace".into(),
            }
        },
    }
}

/// Resolve the backend + isolation level for an admitted-to-be request.
/// Fails closed with structured codes — never silently degrades.
pub fn resolve_backend(
    policy: &SandboxPolicy,
    req: &SandboxRequest,
) -> Result<(BackendKind, IsolationLevel), SandboxFailure> {
    // 1. Syscall filter requirement (M8.7): honest denial when unsupported.
    if req.require_syscall_filter || policy.require_syscall_filter {
        let sup = detect_syscall_support();
        if !matches!(sup.filter_enforcement, CapStatus::Available) {
            return Err(SandboxFailure::new(
                SandboxErrorCode::SyscallDenied,
                "kernel syscall filter required but not provisionable",
                sup.filter_enforcement
                    .reason()
                    .unwrap_or("syscall filtering unsupported on this host")
                    .to_string(),
                "SandboxRequest.require_syscall_filter / policy.require_syscall_filter",
                "drop the syscall-filter requirement (namespace isolation still applies) \
                 or provide a seccomp/landlock capability out of band",
            ));
        }
    }

    // 2. Explicit container isolation (M8.5): only real engines, honest
    //    statuses, no namespace masquerading as a container.
    let wants_container = req.isolation_required == Some(IsolationLevel::Container)
        || policy.required_isolation == Some(IsolationLevel::Container);
    if wants_container {
        if docker_status().is_available() {
            return Ok((BackendKind::Docker, IsolationLevel::Container));
        }
        return Err(SandboxFailure::new(
            SandboxErrorCode::ContainerUnavailable,
            "container isolation requested but no container engine is available",
            format!(
                "docker: {}; systemd-nspawn: {}",
                docker_status().reason().unwrap_or("unknown"),
                nspawn_status().reason().unwrap_or("unknown")
            ),
            "SandboxRequest.isolation_required / policy.required_isolation",
            "run with namespace isolation (`strict`/`networked` profiles) or start a \
             container engine (e.g. `systemctl --user start docker` is not sufficient — \
             a root daemon is required)",
        ));
    }

    // 3. Namespace backend preferred.
    if PythonSandbox::is_bwrap_available() {
        // FULL/ALLOWLIST run on the host network *while still namespaced* for
        // fs/pid/ipc — isolation level stays Namespace (network enforcement
        // for those modes is the mode/netpol gate + interpreter allowlist).
        let required_ok = match req.isolation_required.or(policy.required_isolation) {
            Some(IsolationLevel::Container) => unreachable!("handled above"),
            Some(iso) if iso > IsolationLevel::Namespace => false,
            _ => true,
        };
        if !required_ok {
            return Err(SandboxFailure::new(
                SandboxErrorCode::AdmissionDenied,
                "required isolation level exceeds available namespace backend",
                "no container engine available for the requested isolation level",
                "SandboxRequest.isolation_required",
                "request namespace-level isolation or make a container engine available",
            ));
        }
        return Ok((BackendKind::Bubblewrap, IsolationLevel::Namespace));
    }

    // 4. Degraded subprocess fallback — explicit operator opt-in only, and
    //    never when the request needs confinement the fallback cannot give.
    if policy.allow_unconfined_fallback {
        if req.require_syscall_filter {
            return Err(SandboxFailure::new(
                SandboxErrorCode::SyscallDenied,
                "syscall filtering impossible on the unconfined fallback",
                "the subprocess backend has no kernel confinement",
                "SandboxRequest.require_syscall_filter",
                "install bubblewrap or drop the syscall-filter requirement",
            ));
        }
        let iso = match req.isolation_required {
            Some(IsolationLevel::None) | Some(IsolationLevel::Process) => IsolationLevel::Process,
            None => IsolationLevel::Process,
            Some(_) => {
                return Err(SandboxFailure::new(
                    SandboxErrorCode::AdmissionDenied,
                    "required isolation not reachable via the subprocess fallback",
                    "namespace/container isolation requires bubblewrap or a container engine",
                    "SandboxRequest.isolation_required",
                    "install bubblewrap or lower the required isolation",
                ));
            }
        };
        return Ok((BackendKind::Subprocess, iso));
    }

    Err(SandboxFailure::new(
        SandboxErrorCode::BackendUnavailable,
        "no confined sandbox backend available",
        "bubblewrap is not installed and the unconfined fallback is not opted in \
         (fail-closed default)"
            .to_string(),
        "backend detection (bubblewrap)",
        "install bubblewrap, or set `python_sandbox.allow_unconfined_fallback` / \
         policy.allow_unconfined_fallback explicitly if a degraded unconfined run is \
         acceptable",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::SandboxProfile;

    #[test]
    fn capabilities_report_is_structured() {
        let caps = detect_capabilities();
        assert_eq!(caps.backends.len(), 4);
        assert_eq!(caps.network_modes.len(), 4);
        // Syscall filter must never claim Available on this workspace.
        assert!(!caps.syscall.filter_enforcement.is_available());
        // Docker status must reflect the real daemon, never a fake claim.
        let docker = caps
            .backends
            .iter()
            .find(|b| b.kind == BackendKind::Docker)
            .expect("docker row");
        if run_ok("docker", &["info"]) {
            assert!(docker.status.is_available());
        } else {
            assert!(!docker.status.is_available(), "fake docker GREEN");
        }
    }

    #[test]
    fn syscall_filter_requirement_denies_honestly() {
        let policy = SandboxPolicy::for_profile(SandboxProfile::Strict);
        let mut req = SandboxRequest::python_script("/usr/bin/python3", "print(1)");
        req.require_syscall_filter = true;
        let err = resolve_backend(&policy, &req).unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::SyscallDenied);
        assert!(err.render().contains("SYSCALL_DENIED"));
    }

    #[test]
    fn container_requirement_denies_without_engine() {
        if docker_status().is_available() {
            eprintln!("docker available — container admission path taken instead");
            return;
        }
        let policy = SandboxPolicy::for_profile(SandboxProfile::Strict);
        let mut req = SandboxRequest::python_script("/usr/bin/python3", "print(1)");
        req.isolation_required = Some(IsolationLevel::Container);
        let err = resolve_backend(&policy, &req).unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::ContainerUnavailable);
        assert!(err.why.contains("docker"), "{}", err.why);
    }

    #[test]
    fn strict_without_bwrap_denies_without_fallback() {
        if PythonSandbox::is_bwrap_available() {
            eprintln!("bwrap present — resolution takes the namespace path");
            return;
        }
        let policy = SandboxPolicy::for_profile(SandboxProfile::Strict);
        let req = SandboxRequest::python_script("/usr/bin/python3", "print(1)");
        let err = resolve_backend(&policy, &req).unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::BackendUnavailable);
    }
}
