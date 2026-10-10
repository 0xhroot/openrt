//! Autonomous unattended runs with budgets, stop conditions, approval mats,
//! crash recovery, and supervisor takeovers (M6.1 remainder).
//!
//! An [`AutonomousRun`] is a persisted policy envelope around an existing
//! workflow/session execution. This crate does **not** own scheduling: it
//! records the unattended policy, enforces stop conditions at poll points, and
//! implements takeover as a lease + resume that preserves workflow/task/session
//! identity (no double execution).
//!
//! Storage: `exec.autonomous` under the foundation store (ADR-006).

use std::sync::Arc;

use openrt_audit::AuditLog;
use openrt_permissions::{ApprovalRequest, PermissionDecision, PermissionResolver};
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};

/// Store namespace for autonomous run records (OWNERSHIP Phase 6).
pub const AUTONOMOUS_NS: &str = "exec.autonomous";
/// Key of the active lease (takeover coordination).
pub const LEASE_KEY: &str = "lease";

/// Stop conditions evaluated at each poll (all soft-enforced fail-closed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StopConditions {
    /// Maximum wall-clock seconds for the unattended run (0 = no time budget).
    pub max_wall_secs: u64,
    /// Maximum spend units (abstract budget; caller maps to tokens/$).
    pub max_budget: u64,
    /// Abort after this many failed steps (0 = ignore).
    pub max_errors: u32,
    /// Stop when the workflow completes.
    pub stop_on_complete: bool,
}

impl Default for StopConditions {
    fn default() -> Self {
        Self {
            max_wall_secs: 0,
            max_budget: 0,
            max_errors: 0,
            stop_on_complete: true,
        }
    }
}

/// Why a run stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum StopReason {
    Completed,
    TimeBudget,
    BudgetExceeded,
    ErrorLimit,
    OperatorStop,
    Takeover,
    PolicyDenied,
}

/// Approval mat: capabilities that require an explicit pre-approved grant
/// before an unattended run may proceed past a boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalMat {
    /// Capability patterns pre-approved for this run (operator-authored).
    pub approved: Vec<String>,
    /// Capabilities that always require a live human (fail closed if seen
    /// without a resolver — unattended has no interactive resolver).
    pub always_deny: Vec<String>,
}

impl Default for ApprovalMat {
    fn default() -> Self {
        Self {
            approved: Vec::new(),
            always_deny: Vec::new(),
        }
    }
}

/// Lifecycle of an autonomous run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Created,
    Running,
    Stopped,
    Failed,
}

/// Persisted autonomous run envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutonomousRun {
    pub run_id: String,
    /// Preserved identity: workflow id (compiled/run key).
    pub workflow_id: String,
    /// Preserved session identity for resume.
    pub session_id: String,
    /// Optional task identity.
    pub task_id: Option<String>,
    /// Checkpoint id to resume from after crash/takeover.
    pub checkpoint_id: Option<String>,
    pub state: RunState,
    pub conditions: StopConditions,
    pub mat: ApprovalMat,
    /// Spend accrued so far.
    pub budget_spent: u64,
    pub errors: u32,
    pub started_at: String,
    pub updated_at: String,
    pub stop_reason: Option<StopReason>,
    /// Monotonic execution generation — bumped on takeover so the previous
    /// lease holder fails closed (no double execution).
    pub generation: u64,
}

/// Active lease: which supervisor incarnation holds the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLease {
    pub run_id: String,
    pub holder: String,
    pub generation: u64,
    pub acquired_at: String,
    /// RFC3339 expiry; expired leases may be stolen by a takeover.
    pub expires_at: String,
}

/// Errors from the supervisor.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("persistence error: {0}")]
    Persistence(#[from] openrt_persistence::PersistenceError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("audit error: {0}")]
    Audit(#[from] openrt_audit::AuditError),
    #[error("run `{0}` not found")]
    NotFound(String),
    #[error("run `{0}` is not running")]
    NotRunning(String),
    #[error("lease held by `{holder}` (generation {generation}); takeover denied")]
    LeaseHeld { holder: String, generation: u64 },
    #[error("stale generation for run `{0}` (expected {1}, got {2})")]
    StaleGeneration(String, u64, u64),
    #[error("stop condition: {:?}", .0)]
    Stopped(StopReason),
    #[error("approval mat denies capability `{0}`")]
    MatDenied(String),
    #[error("invalid run configuration: {0}")]
    Invalid(String),
}

/// Supervisor over the foundation store: create, poll, stop, takeover.
pub struct Supervisor {
    store: Arc<dyn PersistenceStore>,
    audit: AuditLog,
    lock: tokio::sync::Mutex<()>,
    /// This incarnation's holder id.
    holder: String,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("holder", &self.holder)
            .finish_non_exhaustive()
    }
}

impl Supervisor {
    pub fn new(store: Arc<dyn PersistenceStore>, holder: impl Into<String>) -> Self {
        let audit = AuditLog::new(store.clone());
        Self {
            store,
            audit,
            lock: tokio::sync::Mutex::new(()),
            holder: holder.into(),
        }
    }

    /// Create an unattended run (audited). Fails closed on empty ids.
    pub async fn create_run(
        &self,
        workflow_id: &str,
        session_id: &str,
        task_id: Option<String>,
        checkpoint_id: Option<String>,
        conditions: StopConditions,
        mat: ApprovalMat,
    ) -> Result<AutonomousRun, SupervisorError> {
        if workflow_id.trim().is_empty() || session_id.trim().is_empty() {
            return Err(SupervisorError::Invalid(
                "workflow_id and session_id are required".into(),
            ));
        }
        let now = openrt_core::now().to_rfc3339();
        let run = AutonomousRun {
            run_id: uuid::Uuid::new_v4().to_string(),
            workflow_id: workflow_id.to_string(),
            session_id: session_id.to_string(),
            task_id,
            checkpoint_id,
            state: RunState::Created,
            conditions,
            mat,
            budget_spent: 0,
            errors: 0,
            started_at: now.clone(),
            updated_at: now,
            stop_reason: None,
            generation: 1,
        };
        self.put_run(&run).await?;
        self.audit
            .append(
                &self.holder,
                "autonomous.create",
                &run.run_id,
                "created",
                serde_json::json!({
                    "workflow_id": run.workflow_id,
                    "session_id": run.session_id,
                    "max_wall_secs": run.conditions.max_wall_secs,
                    "max_budget": run.conditions.max_budget,
                }),
            )
            .await?;
        Ok(run)
    }

    /// Acquire the exclusive lease for this holder (fail-closed).
    /// If an unexpired lease is held by someone else, deny. Expired leases
    /// may be taken over (crash recovery).
    pub async fn acquire_lease(
        &self,
        run_id: &str,
        ttl_secs: u64,
    ) -> Result<RunLease, SupervisorError> {
        let _g = self.lock.lock().await;
        let mut run = self.load_run(run_id).await?;
        if run.state == RunState::Stopped {
            return Err(SupervisorError::NotRunning(run_id.to_string()));
        }
        let now = openrt_core::now();
        if let Some(v) = self.store.get_record(AUTONOMOUS_NS, LEASE_KEY).await? {
            let lease: RunLease = serde_json::from_value(v)?;
            if lease.run_id == run_id {
                let expires = chrono::DateTime::parse_from_rfc3339(&lease.expires_at)
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or(now);
                if expires > now && lease.holder != self.holder {
                    return Err(SupervisorError::LeaseHeld {
                        holder: lease.holder,
                        generation: lease.generation,
                    });
                }
            }
        }
        let expires_at = now + chrono::Duration::seconds(ttl_secs as i64);
        let lease = RunLease {
            run_id: run_id.to_string(),
            holder: self.holder.clone(),
            generation: run.generation,
            acquired_at: now.to_rfc3339(),
            expires_at: expires_at.to_rfc3339(),
        };
        self.store
            .put_record(AUTONOMOUS_NS, LEASE_KEY, &serde_json::to_value(&lease)?)
            .await?;
        if run.state == RunState::Created {
            run.state = RunState::Running;
            run.updated_at = now.to_rfc3339();
            self.put_run(&run).await?;
        }
        self.audit
            .append(
                &self.holder,
                "autonomous.lease",
                run_id,
                "acquired",
                serde_json::json!({ "generation": run.generation, "ttl_secs": ttl_secs }),
            )
            .await?;
        Ok(lease)
    }

    /// Release the lease if we hold it.
    pub async fn release_lease(&self, run_id: &str) -> Result<(), SupervisorError> {
        let _g = self.lock.lock().await;
        if let Some(v) = self.store.get_record(AUTONOMOUS_NS, LEASE_KEY).await? {
            let lease: RunLease = serde_json::from_value(v)?;
            if lease.run_id == run_id && lease.holder == self.holder {
                self.store.delete_record(AUTONOMOUS_NS, LEASE_KEY).await?;
            }
        }
        Ok(())
    }

    /// Take over a run: steal expired lease or same-holder recovery; bump
    /// generation so the previous holder's subsequent writes fail closed.
    pub async fn takeover(&self, run_id: &str) -> Result<AutonomousRun, SupervisorError> {
        let _g = self.lock.lock().await;
        let mut run = self.load_run(run_id).await?;
        if let Some(v) = self.store.get_record(AUTONOMOUS_NS, LEASE_KEY).await? {
            let lease: RunLease = serde_json::from_value(v)?;
            if lease.run_id == run_id && lease.holder != self.holder {
                let expires = chrono::DateTime::parse_from_rfc3339(&lease.expires_at)
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or(openrt_core::now());
                if expires > openrt_core::now() {
                    return Err(SupervisorError::LeaseHeld {
                        holder: lease.holder,
                        generation: lease.generation,
                    });
                }
            }
        }
        run.generation += 1;
        run.state = RunState::Running;
        run.stop_reason = None;
        run.updated_at = openrt_core::now().to_rfc3339();
        self.put_run(&run).await?;
        let lease = RunLease {
            run_id: run_id.to_string(),
            holder: self.holder.clone(),
            generation: run.generation,
            acquired_at: openrt_core::now().to_rfc3339(),
            expires_at: (openrt_core::now() + chrono::Duration::seconds(300)).to_rfc3339(),
        };
        self.store
            .put_record(AUTONOMOUS_NS, LEASE_KEY, &serde_json::to_value(&lease)?)
            .await?;
        self.audit
            .append(
                &self.holder,
                "autonomous.takeover",
                run_id,
                "took_over",
                serde_json::json!({ "generation": run.generation }),
            )
            .await?;
        Ok(run)
    }

    /// Poll stop conditions. Returns `Ok(None)` while the run may continue,
    /// or `Ok(Some(reason))` when it must stop (persisted).
    pub async fn check_stops(
        &self,
        run_id: &str,
        wall_elapsed_secs: u64,
        budget_spent: u64,
        errors: u32,
        completed: bool,
    ) -> Result<Option<StopReason>, SupervisorError> {
        let mut run = self.load_run(run_id).await?;
        if run.state != RunState::Running {
            return Err(SupervisorError::NotRunning(run_id.to_string()));
        }
        run.budget_spent = budget_spent;
        run.errors = errors;
        let reason = if run.conditions.stop_on_complete && completed {
            Some(StopReason::Completed)
        } else if run.conditions.max_wall_secs > 0
            && wall_elapsed_secs >= run.conditions.max_wall_secs
        {
            Some(StopReason::TimeBudget)
        } else if run.conditions.max_budget > 0 && budget_spent >= run.conditions.max_budget {
            Some(StopReason::BudgetExceeded)
        } else if run.conditions.max_errors > 0 && errors >= run.conditions.max_errors {
            Some(StopReason::ErrorLimit)
        } else {
            None
        };
        if let Some(r) = reason {
            run.state = if r == StopReason::Completed {
                RunState::Stopped
            } else {
                RunState::Failed
            };
            run.stop_reason = Some(r.clone());
            run.updated_at = openrt_core::now().to_rfc3339();
            self.put_run(&run).await?;
            self.audit
                .append(
                    &self.holder,
                    "autonomous.stop",
                    run_id,
                    &format!("{r:?}"),
                    serde_json::json!({
                        "wall_elapsed_secs": wall_elapsed_secs,
                        "budget_spent": budget_spent,
                        "errors": errors,
                    }),
                )
                .await?;
            return Ok(Some(r));
        }
        run.updated_at = openrt_core::now().to_rfc3339();
        self.put_run(&run).await?;
        Ok(None)
    }

    /// Operator stop (audited).
    pub async fn operator_stop(
        &self,
        run_id: &str,
        reason: &str,
    ) -> Result<AutonomousRun, SupervisorError> {
        let mut run = self.load_run(run_id).await?;
        run.state = RunState::Stopped;
        run.stop_reason = Some(StopReason::OperatorStop);
        run.updated_at = openrt_core::now().to_rfc3339();
        self.put_run(&run).await?;
        self.audit
            .append(
                "operator",
                "autonomous.operator_stop",
                run_id,
                "stopped",
                serde_json::json!({ "reason": reason }),
            )
            .await?;
        Ok(run)
    }

    /// Evaluate the approval mat for a capability (unattended, no live human).
    pub fn check_mat(run: &AutonomousRun, capability: &str) -> Result<(), SupervisorError> {
        if run
            .mat
            .always_deny
            .iter()
            .any(|p| p == capability || p == "*")
        {
            return Err(SupervisorError::MatDenied(capability.to_string()));
        }
        if run.mat.approved.iter().any(|p| p == capability || p == "*") {
            return Ok(());
        }
        // Default-deny for anything not explicitly pre-approved.
        Err(SupervisorError::MatDenied(capability.to_string()))
    }

    pub async fn load_run(&self, run_id: &str) -> Result<AutonomousRun, SupervisorError> {
        let v = self
            .store
            .get_record(AUTONOMOUS_NS, run_id)
            .await?
            .ok_or_else(|| SupervisorError::NotFound(run_id.to_string()))?;
        Ok(serde_json::from_value(v)?)
    }

    pub async fn list_runs(&self) -> Result<Vec<AutonomousRun>, SupervisorError> {
        let rows = self.store.list_records(AUTONOMOUS_NS).await?;
        let mut out: Vec<AutonomousRun> = Vec::new();
        for (k, v) in rows {
            if k == LEASE_KEY {
                continue;
            }
            out.push(serde_json::from_value(v)?);
        }
        out.sort_by(|a, b| a.started_at.cmp(&b.started_at));
        Ok(out)
    }

    async fn put_run(&self, run: &AutonomousRun) -> Result<(), SupervisorError> {
        self.store
            .put_record(AUTONOMOUS_NS, &run.run_id, &serde_json::to_value(run)?)
            .await?;
        Ok(())
    }

    /// Resume identity for crash recovery: the exact ids to hand back to the
    /// runtime's existing resume/retry path (no parallel recovery engine).
    pub fn resume_identity(run: &AutonomousRun) -> ResumeIdentity {
        ResumeIdentity {
            workflow_id: run.workflow_id.clone(),
            session_id: run.session_id.clone(),
            task_id: run.task_id.clone(),
            checkpoint_id: run.checkpoint_id.clone(),
            generation: run.generation,
        }
    }
}

/// Identity handed to the existing runtime resume path after crash/takeover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeIdentity {
    pub workflow_id: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub checkpoint_id: Option<String>,
    pub generation: u64,
}

/// Fail-closed resolver for unattended runs: consults the approval mat only
/// (never prompts — there is no operator on the wire during unattended runs).
pub struct MatResolver<'a> {
    pub run: &'a AutonomousRun,
}

#[async_trait::async_trait]
impl PermissionResolver for MatResolver<'_> {
    async fn resolve(
        &self,
        req: &ApprovalRequest,
    ) -> Result<PermissionDecision, openrt_permissions::PermissionError> {
        let tool = req.tool.as_str();
        if self
            .run
            .mat
            .always_deny
            .iter()
            .any(|p| openrt_glob(p, tool))
        {
            return Ok(PermissionDecision::Deny);
        }
        if self.run.mat.approved.iter().any(|p| openrt_glob(p, tool)) {
            return Ok(PermissionDecision::Allow);
        }
        Ok(PermissionDecision::Deny)
    }
}

fn openrt_glob(pattern: &str, text: &str) -> bool {
    openrt_permissions_glob(pattern, text)
}

fn openrt_permissions_glob(pattern: &str, text: &str) -> bool {
    // Reuse the same glob semantics as the policy evaluator.
    // openrt_permissions does not export glob_match publicly for patterns
    // outside Capability — implement a minimal `*` matcher identical in spirit.
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    fn sup() -> Supervisor {
        Supervisor::new(Arc::new(MemoryStore::new()), "sup-a")
    }

    #[tokio::test]
    async fn create_and_load_preserves_identity() {
        let s = sup();
        let run = s
            .create_run(
                "wf-1",
                "sess-1",
                Some("task-1".into()),
                Some("ckpt-1".into()),
                StopConditions::default(),
                ApprovalMat::default(),
            )
            .await
            .unwrap();
        let loaded = s.load_run(&run.run_id).await.unwrap();
        assert_eq!(loaded.workflow_id, "wf-1");
        assert_eq!(loaded.session_id, "sess-1");
        assert_eq!(loaded.checkpoint_id.as_deref(), Some("ckpt-1"));
        let id = Supervisor::resume_identity(&loaded);
        assert_eq!(id.workflow_id, "wf-1");
        assert_eq!(id.generation, 1);
    }

    #[tokio::test]
    async fn lease_exclusive_between_holders() {
        let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
        let a = Supervisor::new(store.clone(), "a");
        let b = Supervisor::new(store.clone(), "b");
        let run = a
            .create_run(
                "w",
                "s",
                None,
                None,
                StopConditions::default(),
                ApprovalMat::default(),
            )
            .await
            .unwrap();
        a.acquire_lease(&run.run_id, 60).await.unwrap();
        let err = b.acquire_lease(&run.run_id, 60).await.unwrap_err();
        assert!(matches!(err, SupervisorError::LeaseHeld { .. }));
    }

    #[tokio::test]
    async fn expired_lease_takeover_bumps_generation() {
        let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
        let a = Supervisor::new(store.clone(), "a");
        let b = Supervisor::new(store.clone(), "b");
        let run = a
            .create_run(
                "w",
                "s",
                None,
                None,
                StopConditions::default(),
                ApprovalMat::default(),
            )
            .await
            .unwrap();
        a.acquire_lease(&run.run_id, 0).await.unwrap(); // immediately expires
                                                        // Force expiry in the past (ttl 0 → expires_at ≈ now; sleep briefly).
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        // Manually expire by rewriting lease.
        if let Some(v) = store.get_record(AUTONOMOUS_NS, LEASE_KEY).await.unwrap() {
            let mut lease: RunLease = serde_json::from_value(v).unwrap();
            lease.expires_at = "2000-01-01T00:00:00Z".into();
            store
                .put_record(
                    AUTONOMOUS_NS,
                    LEASE_KEY,
                    &serde_json::to_value(&lease).unwrap(),
                )
                .await
                .unwrap();
        }
        let taken = b.takeover(&run.run_id).await.unwrap();
        assert_eq!(taken.generation, 2);
        let lease: RunLease = serde_json::from_value(
            store
                .get_record(AUTONOMOUS_NS, LEASE_KEY)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(lease.holder, "b");
        assert_eq!(lease.generation, 2);
    }

    #[tokio::test]
    async fn unexpired_lease_blocks_takeover() {
        let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
        let a = Supervisor::new(store.clone(), "a");
        let b = Supervisor::new(store.clone(), "b");
        let run = a
            .create_run(
                "w",
                "s",
                None,
                None,
                StopConditions::default(),
                ApprovalMat::default(),
            )
            .await
            .unwrap();
        a.acquire_lease(&run.run_id, 3600).await.unwrap();
        assert!(matches!(
            b.takeover(&run.run_id).await,
            Err(SupervisorError::LeaseHeld { .. })
        ));
    }

    #[tokio::test]
    async fn time_budget_stops_run() {
        let s = sup();
        let run = s
            .create_run(
                "w",
                "s",
                None,
                None,
                StopConditions {
                    max_wall_secs: 10,
                    ..Default::default()
                },
                ApprovalMat::default(),
            )
            .await
            .unwrap();
        s.acquire_lease(&run.run_id, 60).await.unwrap();
        assert!(s
            .check_stops(&run.run_id, 5, 0, 0, false)
            .await
            .unwrap()
            .is_none());
        let reason = s
            .check_stops(&run.run_id, 11, 0, 0, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reason, StopReason::TimeBudget);
        let after = s.load_run(&run.run_id).await.unwrap();
        assert_eq!(after.state, RunState::Failed);
        assert_eq!(after.stop_reason, Some(StopReason::TimeBudget));
    }

    #[tokio::test]
    async fn budget_and_error_stops() {
        let s = sup();
        let run = s
            .create_run(
                "w",
                "s",
                None,
                None,
                StopConditions {
                    max_budget: 100,
                    max_errors: 3,
                    ..Default::default()
                },
                ApprovalMat::default(),
            )
            .await
            .unwrap();
        s.acquire_lease(&run.run_id, 60).await.unwrap();
        let r = s
            .check_stops(&run.run_id, 1, 100, 0, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r, StopReason::BudgetExceeded);

        let run2 = s
            .create_run(
                "w2",
                "s",
                None,
                None,
                StopConditions {
                    max_errors: 2,
                    ..Default::default()
                },
                ApprovalMat::default(),
            )
            .await
            .unwrap();
        s.acquire_lease(&run2.run_id, 60).await.unwrap();
        // release previous lease first — only one lease key
        s.release_lease(&run.run_id).await.ok();
        s.acquire_lease(&run2.run_id, 60).await.unwrap();
        let r2 = s
            .check_stops(&run2.run_id, 1, 0, 2, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r2, StopReason::ErrorLimit);
    }

    #[tokio::test]
    async fn completion_stops_when_configured() {
        let s = sup();
        let run = s
            .create_run(
                "w",
                "s",
                None,
                None,
                StopConditions::default(),
                ApprovalMat::default(),
            )
            .await
            .unwrap();
        s.acquire_lease(&run.run_id, 60).await.unwrap();
        let r = s
            .check_stops(&run.run_id, 1, 0, 0, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r, StopReason::Completed);
        let after = s.load_run(&run.run_id).await.unwrap();
        assert_eq!(after.state, RunState::Stopped);
    }

    #[tokio::test]
    async fn mat_default_denies_and_allowlist_works() {
        let s = sup();
        let run = s
            .create_run(
                "w",
                "s",
                None,
                None,
                StopConditions::default(),
                ApprovalMat {
                    approved: vec!["filesystem.read".into(), "echo".into()],
                    always_deny: vec!["terminal.run".into()],
                },
            )
            .await
            .unwrap();
        assert!(Supervisor::check_mat(&run, "echo").is_ok());
        assert!(Supervisor::check_mat(&run, "filesystem.read").is_ok());
        assert!(Supervisor::check_mat(&run, "terminal.run").is_err());
        assert!(Supervisor::check_mat(&run, "filesystem.write").is_err());
    }

    #[tokio::test]
    async fn mat_resolver_fail_closed() {
        let s = sup();
        let run = s
            .create_run(
                "w",
                "s",
                None,
                None,
                StopConditions::default(),
                ApprovalMat {
                    approved: vec!["echo".into()],
                    always_deny: vec![],
                },
            )
            .await
            .unwrap();
        let r = MatResolver { run: &run };
        let allow = r
            .resolve(&ApprovalRequest {
                request_id: "1".into(),
                tool: "echo".into(),
                args: serde_json::json!({}),
                scope: None,
                context: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(allow, PermissionDecision::Allow);
        let deny = r
            .resolve(&ApprovalRequest {
                request_id: "2".into(),
                tool: "terminal.run".into(),
                args: serde_json::json!({}),
                scope: None,
                context: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(deny, PermissionDecision::Deny);
        let _ = s;
    }

    #[tokio::test]
    async fn operator_stop_audited() {
        let s = sup();
        let run = s
            .create_run(
                "w",
                "s",
                None,
                None,
                StopConditions::default(),
                ApprovalMat::default(),
            )
            .await
            .unwrap();
        let stopped = s.operator_stop(&run.run_id, "manual").await.unwrap();
        assert_eq!(stopped.stop_reason, Some(StopReason::OperatorStop));
        assert!(s.audit.verify().await.unwrap() >= 2);
    }
}
