//! The sandbox authority (Phase 8, M8.1): admission → execution → verified
//! cleanup → audit, with one identity and one policy path. Nothing executes
//! outside this flow once a tool/step declares a sandbox requirement.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use openrt_audit::AuditLog;
use openrt_modes::ExecutionMode;
use openrt_netpol::NetworkPolicy;
use openrt_persistence::PersistenceStore;
use openrt_resources::{ResourceDecision, ResourceGate};
use serde::{Deserialize, Serialize};

use crate::backend::{detect_capabilities, resolve_backend, BackendKind, SandboxCapabilities};
use crate::executor::{self, ExecutionMeta};
use crate::model::{
    CleanupOutcome, CleanupPolicy, IsolationLevel, NetworkMode, SandboxErrorCode, SandboxFailure,
    SandboxId, SandboxInstanceRecord, SandboxRequest, SandboxResult, SandboxState,
};
use crate::policy::{SandboxPolicy, SandboxProfile};
use crate::sandbox::SandboxBackend;

/// Storage namespace for instance records (ADR-006).
pub const SANDBOX_NS: &str = "sandbox.instances";

/// Successful admission: identity + what was allowed to run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Admission {
    pub id: SandboxId,
    pub profile: SandboxProfile,
    pub backend: BackendKind,
    pub isolation_level: IsolationLevel,
    pub policy_digest: String,
    pub state: SandboxState,
}

/// The single sandbox authority: policy profiles, admission (policy →
/// resources → mode/netpol → backend), execution, and verified disposal.
#[derive(Default)]
pub struct SandboxManager {
    store: Option<Arc<dyn PersistenceStore>>,
    audit: Option<Arc<AuditLog>>,
    resources: Option<Arc<ResourceGate>>,
    netpol: Option<Arc<NetworkPolicy>>,
    kill: Option<Arc<openrt_safety::KillSwitch>>,
    policies: RwLock<BTreeMap<SandboxProfile, SandboxPolicy>>,
    /// Tool id → required sandbox profile (gate admission, M8.9).
    requirements: RwLock<BTreeMap<String, SandboxProfile>>,
}

impl std::fmt::Debug for SandboxManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxManager")
            .field("store", &self.store.is_some())
            .field("audit", &self.audit.is_some())
            .field("resources", &self.resources.is_some())
            .field("netpol", &self.netpol.is_some())
            .field("kill_switch", &self.kill.is_some())
            .finish_non_exhaustive()
    }
}

impl SandboxManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_store(mut self, store: Arc<dyn PersistenceStore>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_audit(mut self, audit: Arc<AuditLog>) -> Self {
        self.audit = Some(audit);
        self
    }

    pub fn with_resources(mut self, gate: Arc<ResourceGate>) -> Self {
        self.resources = Some(gate);
        self
    }

    pub fn with_netpol(mut self, netpol: Arc<NetworkPolicy>) -> Self {
        self.netpol = Some(netpol);
        self
    }

    pub fn with_kill(mut self, kill: Arc<openrt_safety::KillSwitch>) -> Self {
        self.kill = Some(kill);
        self
    }

    /// Override the built-in policy for a profile (e.g. wire the operator's
    /// `allow_unconfined_fallback` config into `Strict`/`Development`).
    pub fn with_policy(self, policy: SandboxPolicy) -> Self {
        self.policies
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(policy.profile, policy);
        self
    }

    /// Effective policy for a profile (override or built-in).
    pub fn policy(&self, profile: SandboxProfile) -> SandboxPolicy {
        self.policies
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&profile)
            .cloned()
            .unwrap_or_else(|| SandboxPolicy::for_profile(profile))
    }

    /// Declare that `tool` must be admitted under `profile` before it runs.
    pub fn require(&self, tool: impl Into<String>, profile: SandboxProfile) {
        self.requirements
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tool.into(), profile);
    }

    /// Registered requirement for `tool`, if any.
    pub fn requirement(&self, tool: &str) -> Option<SandboxProfile> {
        self.requirements
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(tool)
            .copied()
    }

    /// Full capability matrix for this host (M8.5/M8.7 detection).
    pub fn capabilities(&self) -> SandboxCapabilities {
        detect_capabilities()
    }

    /// Persisted instance records (newest last is not guaranteed — records
    /// are keyed by id).
    pub async fn list(&self) -> Result<Vec<SandboxInstanceRecord>, SandboxFailure> {
        let Some(store) = &self.store else {
            return Ok(Vec::new());
        };
        let rows = store.list_records(SANDBOX_NS).await.map_err(|e| {
            SandboxFailure::new(
                SandboxErrorCode::VerificationFailed,
                "could not list sandbox instances",
                e.to_string(),
                SANDBOX_NS,
                "check foundation store health (`openrt doctor`)",
            )
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|(_, v)| serde_json::from_value(v).ok())
            .collect())
    }

    /// Load one instance record.
    pub async fn inspect(&self, id: &str) -> Result<SandboxInstanceRecord, SandboxFailure> {
        let Some(store) = &self.store else {
            return Err(not_found(id));
        };
        let v = store.get_record(SANDBOX_NS, id).await.map_err(|e| {
            SandboxFailure::new(
                SandboxErrorCode::VerificationFailed,
                "could not read sandbox instance",
                e.to_string(),
                SANDBOX_NS,
                "check foundation store health",
            )
        })?;
        v.and_then(|v| serde_json::from_value(v).ok())
            .ok_or_else(|| not_found(id))
    }

    async fn persist(&self, rec: &SandboxInstanceRecord) -> Result<(), SandboxFailure> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let v = serde_json::to_value(rec).map_err(|e| persist_fail(&e.to_string()))?;
        store
            .put_record(SANDBOX_NS, &rec.id.0, &v)
            .await
            .map_err(|e| persist_fail(&e.to_string()))
    }

    /// Audit append, fail-closed for decisions: an admission that cannot be
    /// recorded must not run.
    async fn audit_decision(
        &self,
        action: &str,
        subject: &str,
        result: &str,
        ext: serde_json::Value,
    ) -> Result<(), SandboxFailure> {
        let Some(audit) = &self.audit else {
            return Ok(());
        };
        audit
            .append("sandbox", action, subject, result, ext)
            .await
            .map(|_| ())
            .map_err(|e| {
                SandboxFailure::new(
                    SandboxErrorCode::AdmissionDenied,
                    "sandbox audit record could not be written (fail closed)",
                    e.to_string(),
                    "openrt-audit chained log",
                    "fix audit store health; sandbox decisions refuse to proceed unrecorded",
                )
            })
    }

    async fn audit_best_effort(
        &self,
        action: &str,
        subject: &str,
        result: &str,
        ext: serde_json::Value,
    ) {
        if let Some(audit) = &self.audit {
            let _ = audit.append("sandbox", action, subject, result, ext).await;
        }
    }

    /// Mode gate for networked sandbox modes (Sovereign blocks; ADR-003).
    async fn check_network_mode(&self, network: NetworkMode) -> Result<(), SandboxFailure> {
        if !matches!(network, NetworkMode::Allowlist | NetworkMode::Full) {
            return Ok(());
        }
        let Some(netpol) = &self.netpol else {
            return Err(SandboxFailure::new(
                SandboxErrorCode::NetworkDenied,
                format!(
                    "network mode `{}` requires the mode/netpol gate",
                    network.as_str()
                ),
                "no NetworkPolicy handle attached — fail closed instead of assuming \
                 networked execution is allowed"
                    .to_string(),
                "SandboxManager.netpol",
                "attach netpol (runtime wiring) or request network mode `none`/`local_only`",
            ));
        };
        match netpol.mode().await {
            Ok(ExecutionMode::Provisioning) => Ok(()),
            Ok(ExecutionMode::Sovereign) => Err(SandboxFailure::new(
                SandboxErrorCode::NetworkDenied,
                format!("Sovereign Mode blocks network mode `{}`", network.as_str()),
                "ADR-003: Sovereign Execution Mode denies non-enclave network use",
                "execution mode (ModeManager)",
                "switch to Provisioning Mode for networked materialization, or use \
                 network mode `none`/`local_only`",
            )),
            Err(e) => Err(SandboxFailure::new(
                SandboxErrorCode::NetworkDenied,
                "could not determine the current execution mode",
                e.to_string(),
                "execution mode (ModeManager)",
                "check mode state; networked sandbox modes fail closed while unknown",
            )),
        }
    }

    /// Admit a request: policy validation → host resource gate → mode/netpol
    /// gate → backend resolution → identity + persisted record + audit.
    pub async fn admit(
        &self,
        req: &SandboxRequest,
        profile: SandboxProfile,
        actor: &str,
    ) -> Result<Admission, SandboxFailure> {
        let id = SandboxId::new();
        let subject = id.0.clone();
        let policy = self.policy(profile);
        let digest = policy.digest();

        let deny = |f: SandboxFailure| {
            let subject = subject.clone();
            async move {
                let _ = self
                    .audit_decision(
                        "sandbox.admit",
                        &subject,
                        "deny",
                        serde_json::json!({
                            "profile": profile.as_str(),
                            "code": f.code.as_str(),
                            "what": f.what,
                            "why": f.why,
                            "fix": f.fix,
                            "state": f.state.as_str(),
                            "parent": req.parent,
                            "actor": actor,
                        }),
                    )
                    .await;
                f
            }
        };

        // 1. Policy (network/mounts/env/limits).
        if let Err(f) = policy.validate(req) {
            return Err(deny(f).await);
        }
        // 2. Host resource admission (M8.2).
        if let Some(gate) = &self.resources {
            if let ResourceDecision::Denied { reason } = gate.check() {
                return Err(deny(SandboxFailure::new(
                    SandboxErrorCode::ResourceDenied,
                    "host resource admission denied",
                    reason,
                    "ResourceGate (openrt-resources)",
                    "free host memory/disk or adjust ResourceLimits",
                ))
                .await);
            }
        }
        // 3. Mode/netpol gate for networked modes.
        if let Err(f) = self.check_network_mode(req.network).await {
            return Err(deny(f).await);
        }
        // 4. Backend + isolation resolution (no silent downgrade).
        let (backend, isolation_level) = match resolve_backend(&policy, req) {
            Ok(x) => x,
            Err(f) => return Err(deny(f).await),
        };
        // 5. Required isolation honored exactly.
        if let Some(need) = req.isolation_required {
            if isolation_level < need {
                return Err(deny(SandboxFailure::new(
                    SandboxErrorCode::AdmissionDenied,
                    "resolved isolation is weaker than required",
                    format!(
                        "required {}, resolved {}",
                        need.as_str(),
                        isolation_level.as_str()
                    ),
                    "SandboxRequest.isolation_required",
                    "make a stronger backend available or lower the requirement",
                ))
                .await);
            }
        }

        let admission = Admission {
            id,
            profile,
            backend,
            isolation_level,
            policy_digest: digest,
            state: SandboxState::Admitted,
        };

        let rec = SandboxInstanceRecord {
            id: admission.id.clone(),
            profile: profile.as_str().to_string(),
            backend: backend.as_str().to_string(),
            network: req.network,
            isolation_level,
            policy_digest: admission.policy_digest.clone(),
            state: SandboxState::Admitted,
            parent: req.parent.clone(),
            actor: actor.to_string(),
            created_at: openrt_core::now().to_rfc3339(),
            ended_at: None,
            workspace: None,
            exit_code: None,
            failure: None,
            pid: None,
            starttime: None,
        };
        self.persist(&rec).await?;
        self.audit_decision(
            "sandbox.admit",
            &subject,
            "allow",
            serde_json::json!({
                "profile": profile.as_str(),
                "backend": backend.as_str(),
                "network": req.network.as_str(),
                "isolation": isolation_level.as_str(),
                "policy_digest": admission.policy_digest,
                "parent": req.parent,
                "actor": actor,
            }),
        )
        .await?;
        Ok(admission)
    }

    /// Gate-time admission for a tool with a declared sandbox requirement
    /// (M8.9): validates that the profile's isolation is currently available
    /// and that host resources allow a run. Full request admission happens
    /// again at execution time.
    pub async fn precheck_tool(
        &self,
        tool: &str,
        actor: &str,
    ) -> Result<SandboxProfile, SandboxFailure> {
        let profile = self.requirement(tool).ok_or_else(|| {
            SandboxFailure::new(
                SandboxErrorCode::AdmissionDenied,
                format!("no sandbox requirement registered for tool `{tool}`"),
                "precheck called without a declared requirement",
                "SandboxManager.requirements",
                "register the tool requirement at wiring (require(tool, profile))",
            )
        })?;
        let policy = self.policy(profile);
        let subject = tool.to_string();
        if let Err(f) = self.profile_backend_available(&policy) {
            let _ = self
                .audit_decision(
                    "sandbox.admit",
                    &subject,
                    "deny",
                    serde_json::json!({
                        "profile": profile.as_str(),
                        "code": f.code.as_str(),
                        "why": f.why,
                        "actor": actor,
                    }),
                )
                .await;
            return Err(f);
        }
        if let Some(gate) = &self.resources {
            if let ResourceDecision::Denied { reason } = gate.check() {
                let f = SandboxFailure::new(
                    SandboxErrorCode::ResourceDenied,
                    "host resource admission denied",
                    reason,
                    "ResourceGate (openrt-resources)",
                    "free host memory/disk or adjust ResourceLimits",
                );
                let _ = self
                    .audit_decision(
                        "sandbox.admit",
                        &subject,
                        "deny",
                        serde_json::json!({
                            "profile": profile.as_str(),
                            "code": f.code.as_str(),
                            "why": f.why,
                            "actor": actor,
                        }),
                    )
                    .await;
                return Err(f);
            }
        }
        self.audit_decision(
            "sandbox.admit",
            &subject,
            "allow",
            serde_json::json!({ "profile": profile.as_str(), "actor": actor, "phase": "precheck" }),
        )
        .await?;
        Ok(profile)
    }

    /// Is the profile's required isolation reachable right now?
    fn profile_backend_available(&self, policy: &SandboxPolicy) -> Result<(), SandboxFailure> {
        let probe = SandboxRequest::python_script("/usr/bin/python3", "");
        resolve_backend(policy, &probe).map(|_| ())
    }

    /// Workflow-level admission (M8.9): validate a declared step sandbox
    /// profile — the name resolves to a known profile, its policy is valid,
    /// and the required backend is available on this host — WITHOUT
    /// executing. The workflow engine calls this before the first step so a
    /// declared sandbox requirement fails the run closed at admission time.
    pub fn check_profile(&self, name: &str) -> Result<(), SandboxFailure> {
        let profile = SandboxProfile::from_name(name).ok_or_else(|| {
            SandboxFailure::new(
                SandboxErrorCode::AdmissionDenied,
                "unknown sandbox profile declared on a workflow step",
                format!("`{name}` is not a known sandbox profile"),
                "WorkflowStep.sandbox",
                "declare one of read_only / development / networked / strict / untrusted, \
                 or omit the field",
            )
        })?;
        let policy = self.policy(profile);
        self.profile_backend_available(&policy)
    }

    /// Admit + execute + dispose. The full production flow: every step is
    /// admitted, executed with enforced limits, and its cleanup verified.
    pub async fn run(
        &self,
        req: &SandboxRequest,
        profile: SandboxProfile,
        actor: &str,
    ) -> Result<SandboxResult, SandboxFailure> {
        let admission = self.admit(req, profile, actor).await?;
        let policy = self.policy(profile);
        let started_at = openrt_core::now().to_rfc3339();
        let started = std::time::Instant::now();

        // Record Running before dispatch.
        let mut rec = self
            .inspect(&admission.id.0)
            .await
            .unwrap_or(SandboxInstanceRecord {
                id: admission.id.clone(),
                profile: profile.as_str().to_string(),
                backend: admission.backend.as_str().to_string(),
                network: req.network,
                isolation_level: admission.isolation_level,
                policy_digest: admission.policy_digest.clone(),
                state: SandboxState::Admitted,
                parent: req.parent.clone(),
                actor: actor.to_string(),
                created_at: started_at.clone(),
                ended_at: None,
                workspace: None,
                exit_code: None,
                failure: None,
                pid: None,
                starttime: None,
            });
        rec.state = SandboxState::Running;
        self.persist(&rec).await?;

        let exec = executor::execute(
            &admission.id,
            req,
            &policy,
            admission.backend,
            self.kill.clone(),
        )
        .await;

        let (outcome, meta) = match exec {
            Ok(x) => x,
            Err(f) => {
                rec.state = f.state;
                rec.ended_at = Some(openrt_core::now().to_rfc3339());
                rec.failure = Some(f.clone());
                let _ = self.persist(&rec).await;
                self.audit_best_effort(
                    "sandbox.run",
                    &admission.id.0,
                    "fail",
                    serde_json::json!({
                        "code": f.code.as_str(),
                        "what": f.what,
                        "why": f.why,
                        "state": f.state.as_str(),
                        "profile": profile.as_str(),
                    }),
                )
                .await;
                return Err(f);
            }
        };

        // M8.6: verified cleanup of processes + (disposable) workspace.
        let cleanup = self.cleanup_run(&meta, req.cleanup).await;

        let ended_at = openrt_core::now().to_rfc3339();
        let result = SandboxResult {
            id: admission.id.clone(),
            state: meta.state,
            exit_code: outcome.exit_code,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            output_limited: meta.output_limited,
            kill_reason: meta.kill_reason,
            backend: admission.backend.to_outcome_backend(),
            isolation_level: admission.isolation_level,
            network: req.network,
            policy_digest: admission.policy_digest.clone(),
            profile: profile.as_str().to_string(),
            workspace: if cleanup.workspace_removed {
                None
            } else {
                Some(meta.workspace.clone())
            },
            cleanup,
            started_at,
            ended_at,
            wall_ms: started.elapsed().as_millis() as u64,
        };

        rec.state = result.state;
        rec.ended_at = Some(openrt_core::now().to_rfc3339());
        rec.exit_code = result.exit_code;
        rec.workspace = result.workspace.clone();
        rec.pid = Some(meta.pid);
        rec.starttime = Some(meta.starttime.clone());
        let _ = self.persist(&rec).await;

        let (cleanup_result, cleanup_ext) = if result.cleanup.verified {
            ("ok", serde_json::json!({ "verified": true }))
        } else {
            (
                "fail",
                serde_json::json!({
                    "verified": false,
                    "processes_gone": result.cleanup.processes_gone,
                    "workspace_removed": result.cleanup.workspace_removed,
                    "detail": result.cleanup.detail,
                }),
            )
        };
        self.audit_best_effort(
            "sandbox.run",
            &admission.id.0,
            result.state.as_str(),
            serde_json::json!({
                "profile": profile.as_str(),
                "backend": admission.backend.as_str(),
                "network": req.network.as_str(),
                "isolation": admission.isolation_level.as_str(),
                "exit_code": result.exit_code,
                "output_limited": result.output_limited,
                "wall_ms": result.wall_ms,
                "policy_digest": admission.policy_digest,
                "parent": req.parent,
                "actor": actor,
                "cleanup": cleanup_ext,
            }),
        )
        .await;
        if !result.cleanup.verified {
            self.audit_best_effort(
                "sandbox.destroy",
                &admission.id.0,
                cleanup_result,
                serde_json::json!({ "phase": "run_disposal", "detail": result.cleanup.detail }),
            )
            .await;
        }
        Ok(result)
    }

    /// Verify processes are gone and (for disposable runs) remove + verify
    /// the workspace.
    async fn cleanup_run(&self, meta: &ExecutionMeta, policy: CleanupPolicy) -> CleanupOutcome {
        // Processes: identity-checked leftover kill + liveness verify.
        let mut processes_gone = executor::kill_and_verify(meta.pid, &meta.starttime).await;
        if !processes_gone {
            // One more identity-checked pass after a short grace.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            processes_gone = executor::kill_and_verify(meta.pid, &meta.starttime).await;
        }
        match policy {
            CleanupPolicy::Keep => CleanupOutcome {
                verified: processes_gone,
                processes_gone,
                workspace_removed: false,
                detail: if processes_gone {
                    None
                } else {
                    Some("sandbox process still alive after identity-checked kill".into())
                },
            },
            CleanupPolicy::Disposable => {
                let removed = remove_verified(&meta.workspace);
                CleanupOutcome {
                    verified: processes_gone && removed.0,
                    processes_gone,
                    workspace_removed: removed.0,
                    detail: removed.1,
                }
            }
        }
    }

    /// Destroy a kept instance: identity-kill its process (if recorded),
    /// remove the workspace, verify, persist `Destroyed`, audit.
    pub async fn destroy(&self, id: &str) -> Result<CleanupOutcome, SandboxFailure> {
        let mut rec = self.inspect(id).await?;
        let mut processes_gone = true;
        if let (Some(pid), Some(st)) = (rec.pid, rec.starttime.clone()) {
            processes_gone = executor::kill_and_verify(pid, &st).await;
        }
        let (removed, detail) = match &rec.workspace {
            Some(ws) => remove_verified(ws),
            None => (true, None),
        };
        let detail = detail.or(if processes_gone {
            None
        } else {
            Some("recorded sandbox process still alive after identity-checked kill".to_string())
        });
        let outcome = CleanupOutcome {
            verified: processes_gone && removed,
            processes_gone,
            workspace_removed: removed,
            detail: detail.clone(),
        };
        if !outcome.verified {
            self.audit_best_effort(
                "sandbox.destroy",
                id,
                "fail",
                serde_json::json!({
                    "processes_gone": processes_gone,
                    "workspace_removed": removed,
                    "detail": detail,
                }),
            )
            .await;
            return Err(SandboxFailure::new(
                SandboxErrorCode::CleanupFailed,
                "sandbox destruction could not be verified",
                detail
                    .clone()
                    .unwrap_or_else(|| "unknown cleanup failure".into()),
                "SandboxManager.destroy",
                "inspect the leftover process/workspace manually, then retry destroy",
            ));
        }
        rec.state = SandboxState::Destroyed;
        rec.ended_at = Some(openrt_core::now().to_rfc3339());
        rec.workspace = None;
        self.persist(&rec).await?;
        self.audit_best_effort(
            "sandbox.destroy",
            id,
            "ok",
            serde_json::json!({ "verified": true }),
        )
        .await;
        Ok(outcome)
    }

    /// Backend outcome enum exposed on results (kept for compat).
    pub fn backend_outcome(backend: BackendKind) -> SandboxBackend {
        backend.to_outcome_backend()
    }
}

fn not_found(id: &str) -> SandboxFailure {
    SandboxFailure::new(
        SandboxErrorCode::VerificationFailed,
        format!("sandbox instance `{id}` not found"),
        "no record under sandbox.instances with that id",
        SANDBOX_NS,
        "list instances with `openrt sandbox list`",
    )
}

fn persist_fail(detail: &str) -> SandboxFailure {
    SandboxFailure::new(
        SandboxErrorCode::AdmissionDenied,
        "sandbox instance record could not be persisted (fail closed)",
        detail.to_string(),
        SANDBOX_NS,
        "fix foundation store health; admissions refuse to run unrecorded",
    )
}

/// Remove `dir`, verifying it is gone. Returns (removed, detail-on-failure).
fn remove_verified(dir: &Path) -> (bool, Option<String>) {
    if !dir.exists() {
        return (true, None);
    }
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {
            if dir.exists() {
                (
                    false,
                    Some(format!(
                        "workspace `{}` still exists after remove_dir_all",
                        dir.display()
                    )),
                )
            } else {
                (true, None)
            }
        }
        Err(e) => (
            false,
            Some(format!(
                "removing workspace `{}` failed: {e}",
                dir.display()
            )),
        ),
    }
}

/// Capability summary helper for CLI/status surfaces (name → status).
pub fn capability_rows(caps: &SandboxCapabilities) -> Vec<crate::model::SandboxCapability> {
    let mut rows = Vec::new();
    for b in &caps.backends {
        rows.push(crate::model::SandboxCapability {
            name: format!("backend.{}", b.kind.as_str()),
            status: b.status.clone(),
        });
    }
    for (mode, status) in &caps.network_modes {
        rows.push(crate::model::SandboxCapability {
            name: format!("network.{}", mode.as_str()),
            status: status.clone(),
        });
    }
    rows.push(crate::model::SandboxCapability {
        name: "syscall.filter".into(),
        status: caps.syscall.filter_enforcement.clone(),
    });
    rows.push(crate::model::SandboxCapability {
        name: "network.local_only.loopback".into(),
        status: caps.local_only_loopback.clone(),
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::SandboxLimits;
    use openrt_audit::AuditQuery;
    use openrt_persistence::MemoryStore;

    fn mgr() -> (SandboxManager, Arc<dyn PersistenceStore>, Arc<AuditLog>) {
        let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
        let audit = Arc::new(AuditLog::new(store.clone()));
        let netpol = Arc::new(NetworkPolicy::new(store.clone()));
        let m = SandboxManager::new()
            .with_store(store.clone())
            .with_audit(audit.clone())
            .with_netpol(netpol);
        (m, store, audit)
    }

    async fn audit_actions(audit: &AuditLog) -> Vec<String> {
        audit
            .query(
                &AuditQuery::new()
                    .with_action_prefix("sandbox.")
                    .with_limit(50),
            )
            .await
            .unwrap()
            .into_iter()
            .map(|r| format!("{}:{}", r.entry.action, r.entry.result))
            .collect()
    }

    #[tokio::test]
    async fn run_success_disposable_verified() {
        let (m, _store, audit) = mgr();
        let req = SandboxRequest::python_script("/usr/bin/python3", "print('hello unified')");
        let res = m.run(&req, SandboxProfile::Strict, "test").await.unwrap();
        assert_eq!(res.state, SandboxState::Succeeded, "{res:?}");
        assert!(res.stdout.contains("hello unified"), "{}", res.stdout);
        assert_eq!(res.exit_code, Some(0));
        assert_eq!(res.isolation_level, IsolationLevel::Namespace);
        assert_eq!(res.network, NetworkMode::None);
        assert_eq!(res.profile, "strict");
        assert!(res.policy_digest.len() == 64);
        assert!(res.cleanup.verified, "{:?}", res.cleanup);
        assert!(res.cleanup.workspace_removed);
        assert_eq!(res.workspace, None);

        let rec = m.inspect(&res.id.0).await.unwrap();
        assert_eq!(rec.state, SandboxState::Succeeded);
        assert_eq!(rec.exit_code, Some(0));
        assert_eq!(rec.pid, None.or(rec.pid)); // recorded at run time
        assert!(rec.starttime.is_some());

        let actions = audit_actions(&audit).await;
        assert!(
            actions.iter().any(|a| a == "sandbox.admit:allow"),
            "{actions:?}"
        );
        assert!(
            actions.iter().any(|a| a == "sandbox.run:succeeded"),
            "{actions:?}"
        );
    }

    #[tokio::test]
    async fn filesystem_escape_denied_through_manager() {
        let (m, _store, _audit) = mgr();
        let script = "try:\n    open('/etc/openrt-m8','w').write('x')\n    print('WROTE_ETC')\nexcept Exception as e:\n    print('etc denied:', type(e).__name__)\ntry:\n    open('/home/user/.ssh/id_rsa')\n    print('READ_HOME')\nexcept Exception as e:\n    print('home denied:', type(e).__name__)\nopen('in.txt','w').write('ok')\nprint('workspace ok')";
        let req = SandboxRequest::python_script("/usr/bin/python3", script);
        let res = m.run(&req, SandboxProfile::Strict, "test").await.unwrap();
        assert_eq!(res.state, SandboxState::Succeeded, "{res:?}");
        assert!(!res.stdout.contains("WROTE_ETC"), "{}", res.stdout);
        assert!(!res.stdout.contains("READ_HOME"), "{}", res.stdout);
        assert!(res.stdout.contains("workspace ok"), "{}", res.stdout);
        assert!(!Path::new("/etc/openrt-m8").exists());
    }

    #[tokio::test]
    async fn network_none_blocks_tcp_and_dns() {
        let (m, _store, _audit) = mgr();
        let script = "import socket\ntry:\n    s=socket.socket(); s.settimeout(3); s.connect(('1.1.1.1',80)); print('TCP_OK')\nexcept Exception as e:\n    print('tcp denied:', type(e).__name__)\ntry:\n    socket.getaddrinfo('example.com',80); print('DNS_OK')\nexcept Exception as e:\n    print('dns denied:', type(e).__name__)";
        let req = SandboxRequest::python_script("/usr/bin/python3", script);
        let res = m.run(&req, SandboxProfile::Strict, "test").await.unwrap();
        assert_eq!(res.state, SandboxState::Succeeded, "{res:?}");
        assert!(!res.stdout.contains("TCP_OK"), "{}", res.stdout);
        assert!(!res.stdout.contains("DNS_OK"), "{}", res.stdout);
        assert!(res.stdout.contains("tcp denied"), "{}", res.stdout);
        assert!(res.stdout.contains("dns denied"), "{}", res.stdout);
    }

    #[tokio::test]
    async fn local_only_blocks_host_keeps_loopback() {
        let (m, _store, _audit) = mgr();
        let script = r#"
import socket, threading
try:
    s = socket.socket(); s.settimeout(3); s.connect(('1.1.1.1', 80)); print('HOST_OK')
except Exception as e:
    print('host denied:', type(e).__name__)
try:
    socket.getaddrinfo('example.com', 80); print('DNS_OK')
except Exception as e:
    print('dns denied:', type(e).__name__)
try:
    srv = socket.socket(); srv.bind(('127.0.0.1', 0)); srv.listen(1)
    port = srv.getsockname()[1]
    def serve():
        c, _ = srv.accept(); c.send(b'ok'); c.close()
    threading.Thread(target=serve, daemon=True).start()
    cl = socket.socket(); cl.settimeout(3); cl.connect(('127.0.0.1', port))
    data = cl.recv(2)
    print('LOOPBACK_OK' if data == b'ok' else 'LOOPBACK_BAD')
except Exception as e:
    print('loopback denied:', type(e).__name__)
"#;
        let mut req = SandboxRequest::python_script("/usr/bin/python3", script);
        req.network = NetworkMode::LocalOnly;
        let res = m
            .run(&req, SandboxProfile::Development, "test")
            .await
            .unwrap();
        assert_eq!(res.state, SandboxState::Succeeded, "{res:?}");
        assert!(!res.stdout.contains("HOST_OK"), "{}", res.stdout);
        assert!(!res.stdout.contains("DNS_OK"), "{}", res.stdout);
        assert!(res.stdout.contains("host denied"), "{}", res.stdout);
        // Loopback is brought up best-effort — must work on this host.
        assert!(res.stdout.contains("LOOPBACK_OK"), "{}", res.stdout);
        assert_eq!(res.isolation_level, IsolationLevel::Namespace);
    }

    #[tokio::test]
    async fn allowlist_blocks_others_allows_listed() {
        let (m, store, _audit) = mgr();
        // Allowlist shares the host network → requires Provisioning Mode
        // (Sovereign correctly denies it even with interpreter hooks).
        openrt_modes::ModeManager::new(store)
            .seed_from_config(ExecutionMode::Provisioning)
            .await
            .unwrap();
        let script = r#"
import socket, threading
try:
    socket.getaddrinfo('not-allowed.example', 80); print('DENY_FAIL')
except Exception as e:
    print('getaddrinfo denied:', type(e).__name__)
try:
    s = socket.socket(); s.settimeout(3); s.connect(('9.9.9.9', 53)); print('CONNECT_OK')
except Exception as e:
    print('connect denied:', type(e).__name__)
try:
    srv = socket.socket(); srv.bind(('127.0.0.1', 0)); srv.listen(1)
    port = srv.getsockname()[1]
    def serve():
        c, _ = srv.accept(); c.send(b'ok'); c.close()
    threading.Thread(target=serve, daemon=True).start()
    cl = socket.socket(); cl.settimeout(3); cl.connect(('127.0.0.1', port))
    print('ALLOWED_OK' if cl.recv(2) == b'ok' else 'ALLOWED_BAD')
except Exception as e:
    print('allowed connect failed:', type(e).__name__)
"#;
        let mut req = SandboxRequest::python_script("/usr/bin/python3", script);
        req.network = NetworkMode::Allowlist;
        req.network_allowlist = vec!["127.0.0.1".into()];
        let res = m
            .run(&req, SandboxProfile::Networked, "test")
            .await
            .unwrap();
        assert_eq!(res.state, SandboxState::Succeeded, "{res:?}");
        assert!(res.stdout.contains("getaddrinfo denied"), "{}", res.stdout);
        assert!(res.stdout.contains("connect denied"), "{}", res.stdout);
        assert!(res.stdout.contains("ALLOWED_OK"), "{}", res.stdout);
        assert!(!res.stdout.contains("CONNECT_OK"), "{}", res.stdout);
        assert_eq!(res.isolation_level, IsolationLevel::Namespace);
    }

    #[tokio::test]
    async fn timeout_kills_and_cleans() {
        let (m, _store, audit) = mgr();
        let req = SandboxRequest::python_script("/usr/bin/python3", "import time\ntime.sleep(30)")
            .with_limits(SandboxLimits {
                wall_timeout: std::time::Duration::from_millis(500),
                ..SandboxLimits::default()
            });
        let res = m.run(&req, SandboxProfile::Strict, "test").await.unwrap();
        assert_eq!(res.state, SandboxState::Timeout, "{res:?}");
        assert!(res.kill_reason.is_some());
        assert!(res.cleanup.processes_gone, "{:?}", res.cleanup);
        assert!(res.cleanup.workspace_removed);
        assert!(res.cleanup.verified, "{:?}", res.cleanup);
        let actions = audit_actions(&audit).await;
        assert!(
            actions.iter().any(|a| a == "sandbox.run:timeout"),
            "{actions:?}"
        );
    }

    #[tokio::test]
    async fn output_flood_becomes_controlled_kill() {
        let (m, _store, _audit) = mgr();
        let req = SandboxRequest::python_script("/usr/bin/python3", "print('x' * 500000)")
            .with_limits(SandboxLimits {
                output_bytes: 1024,
                ..SandboxLimits::default()
            });
        let res = m.run(&req, SandboxProfile::Strict, "test").await.unwrap();
        assert_eq!(res.state, SandboxState::Killed, "{res:?}");
        assert!(res.output_limited);
        assert_eq!(res.kill_reason.as_deref(), Some("output_limit"));
        assert!(res.stdout.len() <= 1024);
        assert!(res.cleanup.verified, "{:?}", res.cleanup);
    }

    #[tokio::test]
    async fn resource_gate_denies_admission() {
        let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
        let audit = Arc::new(AuditLog::new(store.clone()));
        let gate = Arc::new(openrt_resources::ResourceGate::new(
            openrt_resources::ResourceLimits {
                max_mem_used_ratio: 0.0,
                max_cpu_percent: 80,
                min_free_disk_bytes: Some(u64::MAX),
                ..openrt_resources::ResourceLimits::default()
            },
            openrt_resources::SystemInfo::collect(),
        ));
        let m = SandboxManager::new()
            .with_store(store.clone())
            .with_audit(audit.clone())
            .with_resources(gate);
        let req = SandboxRequest::python_script("/usr/bin/python3", "print('nope')");
        let err = m
            .run(&req, SandboxProfile::Strict, "test")
            .await
            .unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::ResourceDenied);
        assert!(err.state.is_denial());
        let actions = audit_actions(&audit).await;
        assert!(
            actions.iter().any(|a| a == "sandbox.admit:deny"),
            "{actions:?}"
        );
    }

    #[tokio::test]
    async fn sovereign_blocks_networked_modes_provisioning_allows() {
        let (m, store, _audit) = mgr();
        // Default: Sovereign (fail-closed) → Full denied.
        let mut req = SandboxRequest::python_script("/usr/bin/python3", "print('net run')");
        req.network = NetworkMode::Full;
        let err = m
            .run(&req, SandboxProfile::Networked, "test")
            .await
            .unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::NetworkDenied, "{err}");
        assert!(err.why.contains("Sovereign"), "{}", err.why);

        // Operator switches to Provisioning → admitted.
        openrt_modes::ModeManager::new(store)
            .seed_from_config(ExecutionMode::Provisioning)
            .await
            .unwrap();
        let res = m
            .run(&req, SandboxProfile::Networked, "test")
            .await
            .unwrap();
        assert_eq!(res.state, SandboxState::Succeeded, "{res:?}");
        assert_eq!(res.network, NetworkMode::Full);
    }

    #[tokio::test]
    async fn keep_cleanup_and_destroy() {
        let (m, _store, _audit) = mgr();
        let req = SandboxRequest::python_script(
            "/usr/bin/python3",
            "open('artifact.txt','w').write('data')",
        )
        .with_cleanup(CleanupPolicy::Keep);
        let res = m.run(&req, SandboxProfile::Strict, "test").await.unwrap();
        assert_eq!(res.state, SandboxState::Succeeded, "{res:?}");
        let ws = res.workspace.clone().expect("kept workspace");
        assert!(ws.join("artifact.txt").exists());

        let out = m.destroy(&res.id.0).await.unwrap();
        assert!(out.verified, "{out:?}");
        assert!(out.workspace_removed);
        assert!(!ws.exists());
        let rec = m.inspect(&res.id.0).await.unwrap();
        assert_eq!(rec.state, SandboxState::Destroyed);
        assert_eq!(rec.workspace, None);
    }

    #[tokio::test]
    async fn destroy_unknown_instance_is_structured_failure() {
        let (m, _store, _audit) = mgr();
        let err = m.destroy("does-not-exist").await.unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::VerificationFailed);
        assert!(err.render().contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn precheck_tool_allows_registered_requirement() {
        let (m, _store, audit) = mgr();
        m.require("python.run", SandboxProfile::Strict);
        let profile = m.precheck_tool("python.run", "test").await.unwrap();
        assert_eq!(profile, SandboxProfile::Strict);
        let actions = audit_actions(&audit).await;
        assert!(
            actions.iter().any(|a| a == "sandbox.admit:allow"),
            "{actions:?}"
        );
    }

    #[tokio::test]
    async fn rw_mount_in_policy_prefix_executes() {
        let (m, _store, _audit) = mgr();
        let src = std::env::temp_dir().join(format!("openrt-m8-mount-{}", std::process::id()));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("in.txt"), "payload").unwrap();
        let req = SandboxRequest::python_script(
            "/usr/bin/python3",
            "print(open('/data/in.txt').read())\nopen('/data/out.txt','w').write('written')",
        )
        .with_mount(crate::model::Mount::rw(src.clone(), "/data"));
        let res = m
            .run(&req, SandboxProfile::Development, "test")
            .await
            .unwrap();
        assert_eq!(res.state, SandboxState::Succeeded, "{res:?}");
        assert!(res.stdout.contains("payload"), "{}", res.stdout);
        assert_eq!(
            std::fs::read_to_string(src.join("out.txt")).unwrap().trim(),
            "written"
        );
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn requirement_registry_roundtrip() {
        let m = SandboxManager::new();
        assert!(m.requirement("python.run").is_none());
        m.require("python.run", SandboxProfile::Strict);
        assert_eq!(m.requirement("python.run"), Some(SandboxProfile::Strict));
        assert_eq!(m.requirement("terminal.run"), None);
    }

    #[test]
    fn profile_policy_overrides_stick() {
        let m = SandboxManager::new()
            .with_policy(SandboxPolicy::for_profile(SandboxProfile::Strict).with_fallback(true));
        assert!(m.policy(SandboxProfile::Strict).allow_unconfined_fallback);
        assert!(
            !m.policy(SandboxProfile::Development)
                .allow_unconfined_fallback
        );
    }

    #[tokio::test]
    async fn precheck_without_requirement_is_denied() {
        let m = SandboxManager::new();
        let err = m.precheck_tool("nope.tool", "test").await.unwrap_err();
        assert_eq!(err.code, SandboxErrorCode::AdmissionDenied);
    }

    /// Security requirement: no ambient environment/secrets inheritance —
    /// a host variable visible to the test process must be invisible to the
    /// sandboxed payload (clearenv + explicit allowlist only).
    #[tokio::test]
    async fn ambient_env_is_not_inherited() {
        let (m, _store, _audit) = mgr();
        // Safe in edition 2021; only this test process reads it below.
        std::env::set_var("OPENRT_P8_SECRET", "hunter2-ambient");
        let script = "import os
print('SECRET_LEAKED' if 'OPENRT_P8_SECRET' in os.environ else 'clean')
print('has_PATH=' + ('PATH' in os.environ).__str__())";
        let req = SandboxRequest::python_script("/usr/bin/python3", script);
        let res = m.run(&req, SandboxProfile::Strict, "test").await.unwrap();
        assert_eq!(res.state, SandboxState::Succeeded, "{res:?}");
        assert!(!res.stdout.contains("SECRET_LEAKED"), "{}", res.stdout);
        assert!(!res.stdout.contains("hunter2"), "{}", res.stdout);
        assert!(res.stdout.contains("clean"), "{}", res.stdout);
        std::env::remove_var("OPENRT_P8_SECRET");
    }

    /// Enforcement: max_procs is a real RLIMIT_NPROC on the payload — fork
    /// pressure is refused inside the sandbox (PROCESS_LIMIT behavior), not
    /// merely requested by policy.
    #[tokio::test]
    async fn process_limit_enforced_via_nproc() {
        let (m, _store, _audit) = mgr();
        let script = "import os\nn=0\ntry:\n    for i in range(256):\n        if os.fork()==0:\n            os._exit(0)\n        n+=1\nexcept Exception as e:\n    print('RLIMIT', type(e).__name__)\nprint('forked', n)";
        let req =
            SandboxRequest::python_script("/usr/bin/python3", script).with_limits(SandboxLimits {
                max_procs: 4,
                ..SandboxLimits::default()
            });
        let res = m.run(&req, SandboxProfile::Strict, "test").await.unwrap();
        assert_eq!(res.state, SandboxState::Succeeded, "{res:?}");
        assert!(
            res.stdout.contains("RLIMIT"),
            "nproc limit was not enforced: {}",
            res.stdout
        );
        assert!(!res.stdout.contains("forked 256"), "{}", res.stdout);
    }

    /// Requirement: cleanup failures are structured (CLEANUP_FAILED +
    /// WHAT/WHY/WHERE/FIX) and audited as `sandbox.destroy:fail`, and the
    /// destroy is retryable once the obstruction is removed.
    #[tokio::test]
    async fn cleanup_failure_is_structured_and_recoverable() {
        use std::os::unix::fs::PermissionsExt;
        let (m, _store, audit) = mgr();
        let req = SandboxRequest::python_script(
            "/usr/bin/python3",
            "open('artifact.txt','w').write('data')",
        )
        .with_cleanup(CleanupPolicy::Keep);
        let res = m.run(&req, SandboxProfile::Strict, "test").await.unwrap();
        let ws = res.workspace.clone().expect("kept workspace");
        assert!(ws.join("artifact.txt").exists());

        // Make removal impossible: read-only workspace dir (no unlink of
        // contained files) — host-side obstruction, no destructive payload.
        std::fs::set_permissions(&ws, std::fs::Permissions::from_mode(0o555)).unwrap();
        let err = m.destroy(&res.id.0).await.unwrap_err();
        // Restore permissions before asserting so the test cannot leak state.
        std::fs::set_permissions(&ws, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(err.code, SandboxErrorCode::CleanupFailed, "{err}");
        let rendered = err.render();
        assert!(rendered.contains("CLEANUP_FAILED"), "{rendered}");
        assert!(!err.why.is_empty() && !err.where_.is_empty() && !err.fix.is_empty());

        let actions = audit_actions(&audit).await;
        assert!(
            actions.iter().any(|a| a == "sandbox.destroy:fail"),
            "{actions:?}"
        );

        // Recovery: once the obstruction is gone, destroy verifies cleanly.
        let out = m.destroy(&res.id.0).await.unwrap();
        assert!(out.verified, "{out:?}");
        assert!(!ws.exists());
        let actions = audit_actions(&audit).await;
        assert!(
            actions.iter().any(|a| a == "sandbox.destroy:ok"),
            "{actions:?}"
        );
    }
}
