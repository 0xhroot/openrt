//! Sandbox policy: named profiles, admission validation, and the policy
//! digest that identifies exactly what was admitted.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::limits::SandboxLimits;
use crate::model::{IsolationLevel, NetworkMode, SandboxErrorCode, SandboxFailure, SandboxRequest};

/// Named sandbox profiles. Each maps to a concrete [`SandboxPolicy`]; the
/// profile set is the vocabulary workflows/tools select from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProfile {
    /// Read-only system mounts, no network, scratch workspace only.
    ReadOnly,
    /// Local development: scratch + temp workspace, no network.
    Development,
    /// May use the network (still gated by execution mode + netpol; admits
    /// NONE/LOCAL_ONLY/ALLOWLIST/FULL).
    Networked,
    /// Default for confined code execution (e.g. `python.run`): minimal
    /// mounts, no network, fail-closed.
    Strict,
    /// Tightest limits for untrusted/autonomous payloads.
    Untrusted,
}

impl SandboxProfile {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Development => "development",
            Self::Networked => "networked",
            Self::Strict => "strict",
            Self::Untrusted => "untrusted",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "read_only" | "readonly" => Some(Self::ReadOnly),
            "development" => Some(Self::Development),
            "networked" => Some(Self::Networked),
            "strict" => Some(Self::Strict),
            "untrusted" => Some(Self::Untrusted),
            _ => None,
        }
    }
}

impl std::fmt::Display for SandboxProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The admission policy one run is evaluated against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    pub profile: SandboxProfile,
    /// Network modes this policy admits (each with its true enforcement).
    pub allowed_network: Vec<NetworkMode>,
    /// Read-only mount source prefixes (canonicalized at validation).
    pub ro_prefixes: Vec<PathBuf>,
    /// Read-write mount source prefixes (canonicalized at validation).
    pub rw_prefixes: Vec<PathBuf>,
    /// Environment keys a request may set (values come from the request —
    /// the ambient host environment is never inherited).
    pub env_allowlist: Vec<String>,
    /// Upper bounds for requested limits (requests are clamped/rejected
    /// above these).
    pub max_limits: SandboxLimits,
    /// Explicit operator opt-in to the degraded unconfined `subprocess`
    /// backend when no namespace backend exists (fail-closed default).
    pub allow_unconfined_fallback: bool,
    /// Minimum isolation the profile itself demands.
    pub required_isolation: Option<IsolationLevel>,
    /// Require a kernel syscall filter (seccomp) for every run under this
    /// profile (denied honestly when unsupported).
    pub require_syscall_filter: bool,
}

fn system_ro_prefixes() -> Vec<PathBuf> {
    ["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc"]
        .iter()
        .map(PathBuf::from)
        .collect()
}

fn default_max_limits() -> SandboxLimits {
    SandboxLimits::default()
}

fn tight_max_limits() -> SandboxLimits {
    SandboxLimits {
        wall_timeout: std::time::Duration::from_secs(15),
        memory_bytes: 512 * 1024 * 1024,
        cpu_seconds: 5,
        max_procs: 16,
        output_bytes: 32 * 1024,
    }
}

impl SandboxPolicy {
    /// Built-in policy for a profile (conservative defaults; operators can
    /// override via [`SandboxPolicy::with_fallback`] / manager overrides).
    pub fn for_profile(profile: SandboxProfile) -> Self {
        let temp = std::env::temp_dir();
        let (allowed_network, rw_prefixes, max_limits) = match profile {
            SandboxProfile::ReadOnly => (vec![NetworkMode::None], vec![], default_max_limits()),
            SandboxProfile::Development => (
                // `local_only` stays kernel-enforced (netns, no host/DNS
                // egress), so it is safe under Sovereign Mode as well.
                vec![NetworkMode::None, NetworkMode::LocalOnly],
                vec![temp],
                default_max_limits(),
            ),
            SandboxProfile::Networked => (
                vec![
                    NetworkMode::None,
                    NetworkMode::LocalOnly,
                    NetworkMode::Allowlist,
                    NetworkMode::Full,
                ],
                vec![temp],
                default_max_limits(),
            ),
            SandboxProfile::Strict => (vec![NetworkMode::None], vec![], default_max_limits()),
            SandboxProfile::Untrusted => (vec![NetworkMode::None], vec![], tight_max_limits()),
        };
        let required_isolation = match profile {
            SandboxProfile::Networked => None,
            _ => Some(IsolationLevel::Namespace),
        };
        Self {
            profile,
            allowed_network,
            ro_prefixes: system_ro_prefixes(),
            rw_prefixes,
            env_allowlist: vec!["PATH".into()],
            max_limits,
            allow_unconfined_fallback: false,
            required_isolation,
            require_syscall_filter: false,
        }
    }

    pub fn with_fallback(mut self, allow: bool) -> Self {
        self.allow_unconfined_fallback = allow;
        self
    }

    pub fn with_env_allowlist(mut self, keys: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.env_allowlist = keys.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_rw_prefix(mut self, prefix: impl Into<PathBuf>) -> Self {
        self.rw_prefixes.push(prefix.into());
        self
    }

    /// Stable digest of the policy (sha256 over its canonical JSON). Recorded
    /// on every admission so results can be tied to exactly what was allowed.
    pub fn digest(&self) -> String {
        let v = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        openrt_core::digest::sha256_hex(v.to_string().as_bytes())
    }

    fn canonical_prefix(p: &Path) -> Option<PathBuf> {
        use std::path::Component;
        if p.exists() {
            std::fs::canonicalize(p).ok()
        } else {
            // Non-existent sources can't be mounted anyway; lexically
            // normalize so `..` tricks still fail closed.
            let mut out = PathBuf::new();
            for c in p.components() {
                match c {
                    Component::ParentDir => {
                        out.pop();
                    }
                    Component::CurDir => {}
                    other => out.push(other.as_os_str()),
                }
            }
            Some(out)
        }
    }

    fn prefix_allowed(canonical_source: &Path, prefixes: &[PathBuf]) -> bool {
        prefixes.iter().any(|p| {
            let Some(cp) = Self::canonical_prefix(p) else {
                return false;
            };
            canonical_source.starts_with(&cp)
        })
    }

    /// Validate a request against this policy. Errors are structured
    /// failures (code + WHAT/WHY/WHERE/FIX + denial state).
    pub fn validate(&self, req: &SandboxRequest) -> Result<(), SandboxFailure> {
        // 1. Network mode must be in the profile's allowed set.
        if !self.allowed_network.contains(&req.network) {
            return Err(SandboxFailure::new(
                SandboxErrorCode::NetworkDenied,
                format!("network mode `{}` not allowed", req.network.as_str()),
                format!(
                    "profile `{}` only admits [{}]",
                    self.profile,
                    self.allowed_network
                        .iter()
                        .map(|m| m.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                "SandboxPolicy.allowed_network",
                "choose an admitted network mode or a profile that allows this mode",
            ));
        }
        // 2. Allowlist specifics: non-empty list, python payload only
        //    (interpreter-level enforcement is the honest mechanism here).
        if req.network == NetworkMode::Allowlist {
            if req.network_allowlist.is_empty() {
                return Err(SandboxFailure::new(
                    SandboxErrorCode::NetworkDenied,
                    "allowlist network mode requires at least one allowed host",
                    "empty allowlist cannot be enforced",
                    "SandboxRequest.network_allowlist",
                    "list the hosts/globs to allow, or use network mode `full`/`none`",
                ));
            }
            let is_python = req
                .program
                .rsplit('/')
                .next()
                .map(|b| b.contains("python"))
                .unwrap_or(false);
            if !is_python {
                return Err(SandboxFailure::new(
                    SandboxErrorCode::NetworkDenied,
                    "allowlist enforcement is only available for python payloads",
                    "the allowlist is enforced by the interpreter's audit hooks; other \
                     programs would run without a kernel-level filter"
                        .to_string(),
                    "SandboxRequest.network = allowlist",
                    "use a python payload, network mode `none` (kernel-enforced), or \
                     `full` behind an explicit mode/netpol grant",
                ));
            }
        }
        // 3. Mounts: canonical source must fall under allowed prefixes.
        for m in &req.mounts {
            let canonical = Self::canonical_prefix(&m.source).unwrap_or_else(|| m.source.clone());
            let allowed = if m.read_only {
                Self::prefix_allowed(&canonical, &self.ro_prefixes)
                    || Self::prefix_allowed(&canonical, &self.rw_prefixes)
            } else {
                Self::prefix_allowed(&canonical, &self.rw_prefixes)
            };
            if !allowed {
                return Err(SandboxFailure::new(
                    SandboxErrorCode::FilesystemDenied,
                    format!(
                        "mount source `{}` (canonical `{}`) outside policy prefixes",
                        m.source.display(),
                        canonical.display()
                    ),
                    format!(
                        "profile `{}` mounts only system paths read-only and [{}] read-write",
                        self.profile,
                        self.rw_prefixes
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    "SandboxRequest.mounts",
                    "mount a path under an allowed prefix, or extend the policy rw/ro prefixes",
                ));
            }
            if !m.target.is_absolute() {
                return Err(SandboxFailure::new(
                    SandboxErrorCode::FilesystemDenied,
                    format!("mount target `{}` is not absolute", m.target.display()),
                    "bwrap mounts require absolute targets",
                    "SandboxRequest.mounts[].target",
                    "use an absolute in-sandbox target such as `/data`",
                ));
            }
        }
        // 4. Environment: keys must be allowlisted (values never come from
        //    the host process).
        for key in req.env.keys() {
            if !self.env_allowlist.iter().any(|k| k == key) {
                return Err(SandboxFailure::new(
                    SandboxErrorCode::PolicyDenied,
                    format!("environment key `{key}` not on the allowlist"),
                    "sandboxed runs receive an explicitly allowlisted environment only",
                    "SandboxRequest.env",
                    "add the key to SandboxPolicy.env_allowlist or drop it from the request",
                ));
            }
        }
        // 5. Limits must fit within the profile maximums.
        let r = &req.limits;
        let m = &self.max_limits;
        if r.wall_timeout > m.wall_timeout
            || r.memory_bytes > m.memory_bytes
            || r.cpu_seconds > m.cpu_seconds
            || r.max_procs > m.max_procs
            || r.output_bytes > m.output_bytes
        {
            return Err(SandboxFailure::new(
                SandboxErrorCode::ResourceDenied,
                "requested limits exceed the profile maximums",
                format!(
                    "profile `{}` caps: {}s wall, {} MiB memory, {} cpu-s, {} procs, {} B output",
                    self.profile,
                    m.wall_timeout.as_secs(),
                    m.memory_bytes / (1024 * 1024),
                    m.cpu_seconds,
                    m.max_procs,
                    m.output_bytes
                ),
                "SandboxRequest.limits",
                "reduce the requested limits or run under a profile with higher maximums",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_are_stable_and_distinct() {
        let a = SandboxPolicy::for_profile(SandboxProfile::Strict);
        let b = SandboxPolicy::for_profile(SandboxProfile::Strict);
        let c = SandboxPolicy::for_profile(SandboxProfile::Development);
        assert_eq!(a.digest(), b.digest());
        assert_ne!(a.digest(), c.digest());
        assert_eq!(a.digest().len(), 64);
    }

    #[test]
    fn strict_rejects_network() {
        let p = SandboxPolicy::for_profile(SandboxProfile::Strict);
        let req = SandboxRequest::python_script("/usr/bin/python3", "print(1)")
            .with_network(NetworkMode::Full, Vec::<String>::new());
        let err = p.validate(&req).unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::NetworkDenied);
        assert!(err.render().contains("NETWORK_DENIED"), "{}", err);
        assert!(err.state.is_denial());
    }

    #[test]
    fn strict_rejects_env_not_allowlisted() {
        let p = SandboxPolicy::for_profile(SandboxProfile::Strict);
        let mut req = SandboxRequest::python_script("/usr/bin/python3", "print(1)");
        req.env.insert("AWS_SECRET_ACCESS_KEY".into(), "x".into());
        let err = p.validate(&req).unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::PolicyDenied);
    }

    #[test]
    fn strict_rejects_rw_mount_outside_prefixes() {
        let p = SandboxPolicy::for_profile(SandboxProfile::Strict);
        let req = SandboxRequest::python_script("/usr/bin/python3", "print(1)")
            .with_mount(crate::model::Mount::rw("/etc", "/host-etc"));
        let err = p.validate(&req).unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::FilesystemDenied);
    }

    #[test]
    fn symlink_traversal_mount_denied() {
        // /tmp/../etc normalizes to /etc — must not pass as "under /tmp".
        let p = SandboxPolicy::for_profile(SandboxProfile::Development);
        let req = SandboxRequest::python_script("/usr/bin/python3", "print(1)")
            .with_mount(crate::model::Mount::rw("/tmp/../etc/hostname", "/leak"));
        let err = p.validate(&req).unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::FilesystemDenied, "{err}");
    }

    #[test]
    fn limits_over_profile_max_denied() {
        let p = SandboxPolicy::for_profile(SandboxProfile::Untrusted);
        let req = SandboxRequest::python_script("/usr/bin/python3", "print(1)").with_limits(
            SandboxLimits {
                wall_timeout: std::time::Duration::from_secs(600),
                ..SandboxLimits::default()
            },
        );
        let err = p.validate(&req).unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::ResourceDenied);
    }

    #[test]
    fn allowlist_requires_python_payload() {
        let p = SandboxPolicy::for_profile(SandboxProfile::Networked);
        let req = SandboxRequest::new("/usr/bin/curl", ["https://example.com"])
            .with_network(NetworkMode::Allowlist, ["example.com"]);
        let err = p.validate(&req).unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::NetworkDenied);
        assert!(err.why.contains("interpreter"), "{}", err.why);
    }

    #[test]
    fn development_accepts_temp_mount() {
        let p = SandboxPolicy::for_profile(SandboxProfile::Development);
        let temp = std::env::temp_dir();
        let req = SandboxRequest::python_script("/usr/bin/python3", "print(1)")
            .with_mount(crate::model::Mount::rw(temp.join("data"), "/data"));
        p.validate(&req).unwrap();
    }
}
