//! Runtime assembly: builder → wired runtime → task lifecycle entry points.

use std::path::PathBuf;
use std::sync::Arc;

use openrt_agents::MinimalAgent;
use openrt_core::{AgentId, Session, SessionId, Task};
use openrt_models::MockProvider;
use openrt_persistence::{MemoryStore, PersistenceStore};
use openrt_telemetry::{EventBus, EventBusBuilder, EventSink};
use openrt_tools::ToolExecutor;

use crate::gate::{policy_from_config, resource_gate_from_config};
use crate::sinks::PersistenceSink;
use crate::{Error, GateExecutor};

/// Fully wired runtime. Construct via [`RuntimeBuilder`].
pub struct Runtime {
    pub config: openrt_config::Config,
    pub provider: Arc<dyn openrt_models::ModelProvider>,
    pub agent: Arc<dyn openrt_agents::Agent>,
    pub executor: Arc<dyn ToolExecutor>,
    pub registry: openrt_tools::ToolRegistry,
    pub store: Arc<dyn PersistenceStore>,
    pub bus: EventBus,
    /// Stable identity of this runtime's master orchestrator (P2.3). Emitted
    /// as the `agent_id` of workflow lifecycle events.
    pub orchestrator: openrt_core::AgentId,
    /// Event-log persistence health (failures surfaced, not silent; SEC-06).
    pub event_log_health: Arc<crate::sinks::EventLogHealth>,
    /// Keeps the bus dispatcher alive until the runtime is dropped.
    #[allow(dead_code)]
    dispatcher: tokio::task::JoinHandle<()>,
    /// Serializes concurrent task runs per session (SEC-08).
    pub(crate) session_locks: crate::locks::SessionLocks,
    /// ADR-008 operator kill switch (shared with the gate and tools).
    pub kill_switch: std::sync::Arc<openrt_safety::KillSwitch>,
    /// The active screen backend behind the M2.5 `screen.*`/`input.*` tools.
    pub screen: std::sync::Arc<dyn openrt_computer::ScreenBackend>,
    /// P7 Tool Fabric: environment profile + resolved `ComputerBackend` behind
    /// the `computer.*` tools (discover → capability matrix → observe/verify).
    pub computer: std::sync::Arc<openrt_platform::ComputerControl>,
    /// Phase 11 (M11.8): the ONE `ComputerUse` engine behind the
    /// `computer.use` tool — shared by the gate, the CLI, the WebUI palette,
    /// and the TUI so the observation state / digital twin are never
    /// duplicated per surface (no UI-local engine, no second registry).
    pub computer_engine: std::sync::Arc<openrt_computer::ComputerUse>,
    /// Shared resource gate (ADR-006/P2.6): `resources_ok()` consults it
    /// before any recovery-enabled run may proceed.
    pub resource_gate: std::sync::Arc<openrt_resources::ResourceGate>,
    /// M9: the VRAM usage observer feeding the Memory Fabric planner.
    /// Defaults to [`openrt_resources::NoGpuObserver`] (honest unknown);
    /// a vendor probe (e.g. `nvidia-smi`) can be injected via the builder.
    /// Observations are read-only and never grant/revoke authority — no
    /// second resource decision-maker exists.
    pub gpu_observer: std::sync::Arc<dyn openrt_resources::GpuObserver>,
    /// ADR-002 chained, tamper-evident audit log (P2.7) over the same store.
    /// Every safety-critical signal and its reactions are recorded here.
    pub audit: std::sync::Arc<openrt_audit::AuditLog>,
    /// Phase 8: the unified sandbox authority (admission → execution →
    /// verified cleanup; `sandbox.instances` + `sandbox.*` audit actions).
    pub sandbox: std::sync::Arc<openrt_sandbox::SandboxManager>,
    /// ADR-003 operating-mode engine (P3). Shares the runtime store; reads the
    /// persisted mode on every call (no caching) so transitions made through
    /// any other manager over the same store are honored immediately.
    pub modes: openrt_modes::ModeManager,
    /// Local model fabric registry (P3, ADR-010), wired to the same store.
    pub model_registry: openrt_registry::ModelRegistry,
    /// Mode-independent local router (P3): selects among activated artifacts.
    pub router: openrt_router::Router,
    /// Cancellation token for the in-flight orchestrated workflow run. Tripped
    /// by [`Runtime::cancel_workflow`]; recovery runs derive a child token.
    pub(crate) engine_cancel: openrt_core::CancellationToken,
    /// Phase 11 (Track E): per-workflow cooperative pause flags for runs
    /// executing in THIS process. `(flag, active)` — the flag is consulted
    /// by the workflow engine at checkpoint boundaries only; `active` marks
    /// a live run so a pause request with no active run fails closed.
    /// Dropped/ignored on process exit: pause never survives a crash, and
    /// boot recovery only *records* incomplete runs (never auto-resumes).
    pub(crate) workflow_pause: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<
                String,
                (std::sync::Arc<std::sync::atomic::AtomicBool>, bool),
            >,
        >,
    >,
    /// The same fail-closed permission policy the gate enforces (M5.4 hook
    /// permission checks consult this — no second policy authority).
    pub permission_policy: std::sync::Arc<openrt_permissions::Policy>,
    /// Local metrics registry (M5.7): records gauges and emits them only
    /// through this runtime's Phase-1 bus (ADR-005, no parallel pipeline).
    pub metrics: tokio::sync::Mutex<openrt_telemetry::MetricsRegistry>,
    /// Durable apply log for production `ApplyTarget`s (M5.6): written by
    /// [`crate::phase5::Runtime::improve_apply`] after a successful apply.
    pub applied_log: Arc<tokio::sync::Mutex<std::collections::BTreeMap<String, serde_json::Value>>>,
    /// M5.3 working memory: zero-persistence, session-scoped registers
    /// (ADR-006 one store — working memory NEVER touches `store`).
    /// Keyed by session id; entries are dropped on session end / shutdown.
    pub working: Arc<
        tokio::sync::Mutex<
            std::collections::HashMap<openrt_core::SessionId, openrt_memory::WorkingMemory>,
        >,
    >,
    /// M7B: metadata catalog + artifact store + application adapters shared
    /// with `tools.list` and the `artifact.*`/`application.*` tools.
    pub fabric: std::sync::Arc<openrt_fabric::FabricState>,
    /// M7B: database backends behind `database.*` (config connections only).
    pub database: std::sync::Arc<openrt_database::DatabaseManager>,
    /// M7B: managed browser session behind `browser.*` (closed on shutdown).
    pub browser: std::sync::Arc<openrt_browser::BrowserManager>,
    /// M7B: connected MCP servers behind `mcp.*` (children killed on drop).
    pub mcp: std::sync::Arc<openrt_mcp::McpManager>,
    /// Phase 9 M9.1: the content-addressed document corpus
    /// (`corpus.documents`) — production intake for `openrt ingest *`.
    pub corpus: openrt_ingest::corpus::DocCorpus,
    /// Phase 10 M10.4: live task handles — cancellation *and* pause tokens for
    /// tasks actually running in THIS process. The one authority for
    /// `recovery::{task_cancel, task_pause, task_terminate, task_recover,
    /// task_resume}`; never raw process control. Entries are inserted at
    /// spawn, removed when the [`TaskHandle`] drops or the run finishes.
    pub(crate) active_tasks: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<openrt_core::TaskId, ActiveTask>>,
    >,
    /// M10.7 ECC: the gate's deny-only asset-rule layer — refreshed from the
    /// durable store on every asset load/save and consulted by the gate
    /// before policy (rules restrict; they never grant).
    pub(crate) rule_layer: std::sync::Arc<crate::gate::AssetRuleLayer>,
    /// M10.7 lifecycle: the ONE hook-firing implementation, shared with the
    /// gate (`ToolCall`) and every lifecycle fire point (no second path).
    pub(crate) hook_layer: std::sync::Arc<crate::hooks::HookLayer>,
    /// M10.9: the ONE AgentShield evaluation layer — attached to the gate so
    /// every resolved tool call is scanned before policy adjudication.
    pub(crate) shield_layer: std::sync::Arc<crate::shield::ShieldLayer>,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime")
            .field("config", &self.config.runtime.default_provider)
            .field("provider", &self.provider.info().id)
            .field("agent", &self.agent.id())
            .finish()
    }
}

impl Drop for Runtime {
    /// Best-effort flush on drop so a short-lived CLI process never exits with
    /// history/event-log writes still in the dispatcher queue. Only runs when
    /// we are on a multi-thread runtime (block_in_place panics on
    /// current-thread); tests that need a hard drain call `shutdown()`.
    fn drop(&mut self) {
        // M7B: never leave MCP child processes behind (idempotent).
        self.mcp.shutdown();
        let bus = self.bus.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread {
                tokio::task::block_in_place(|| {
                    handle.block_on(bus.flush());
                });
            }
        }
    }
}

impl Runtime {
    // ---- sessions ----

    /// Create a session (durable + events). Fires active `session_created`
    /// asset hooks through the production path (M5.4) after the event lands.
    pub async fn create_session(&self, title: &str) -> Result<Session, Error> {
        let session = Session::new(title);
        self.store.create_session(&session).await?;
        self.bus
            .emit_correlated(
                openrt_core::EventKind::SessionCreated,
                Some(session.id),
                None,
                None,
                serde_json::json!({ "title": title }),
            )
            .await;
        // Best-effort hook fire: a hook failure is surfaced as an error so it
        // is never silent, but the session itself is already durable.
        self.fire_hooks(openrt_workspace_assets::HookTarget::SessionCreated)
            .await?;
        // M5.3: open a fresh zero-persistence working-memory register file
        // for this session (never written to `store`).
        {
            let mut working = self.working.lock().await;
            working.insert(
                session.id,
                openrt_memory::WorkingMemory::new("live".into(), session.id.to_string()),
            );
        }
        Ok(session)
    }

    /// M10.7 lifecycle: end a session — archive it (the one durable
    /// session-end transition), drop its zero-persistence working-memory
    /// registers, and fire `SessionEnded` hooks. The archived record plus
    /// the hook are the signal (Phase 10 adds no new `EventKind`). Idempotent
    /// on an already-archived session (each call is an end signal).
    pub async fn session_end(&self, id: SessionId) -> Result<(), Error> {
        let mut session = self
            .store
            .load_session(id)
            .await
            .map_err(Error::Persistence)?;
        session.status = openrt_core::SessionStatus::Archived;
        session.updated_at = openrt_core::now();
        self.store.update_session(&session).await?;
        {
            let mut working = self.working.lock().await;
            working.remove(&id);
        }
        self.fire_hooks(openrt_workspace_assets::HookTarget::SessionEnded)
            .await?;
        Ok(())
    }

    pub async fn load_session(&self, id: SessionId) -> Result<Session, Error> {
        // Lazy working-memory create on first access of an existing session
        // (TUI switch_session loads without create_session).
        {
            let mut working = self.working.lock().await;
            working.entry(id).or_insert_with(|| {
                openrt_memory::WorkingMemory::new("live".into(), id.to_string())
            });
        }
        self.store
            .load_session(id)
            .await
            .map_err(Error::Persistence)
    }

    pub async fn list_sessions(&self) -> Result<Vec<Session>, Error> {
        self.store.list_sessions().await.map_err(Error::Persistence)
    }

    /// Operator-initiated session delete (TUI Sessions view / CLI): removes
    /// the persisted transcript through the ONE store and records an audit
    /// row. Refuses ids the store does not know (never a silent success).
    pub async fn delete_session(&self, id: SessionId) -> Result<(), Error> {
        self.store
            .delete_session(id)
            .await
            .map_err(Error::Persistence)?;
        let _ = self
            .audit
            .append(
                "runtime",
                "session.delete",
                &id.to_string(),
                "ok",
                serde_json::json!({}),
            )
            .await;
        Ok(())
    }

    // ---- tasks ----

    /// Task-level resource preflight, taken **before** an expensive model or
    /// tool turn is attempted.
    ///
    /// This is the same authority the tool gate uses — it does not create a
    /// second admission path — but it runs at the *task* boundary so a
    /// resource-refused task fails with a recoverable, structured denial
    /// instead of stalling inside a turn. The snapshot is refreshed off the
    /// async path so the UI is never blocked on `/proc`.
    pub async fn preflight_task(&self) -> openrt_resources::Admission {
        self.resource_gate.refresh().await;
        self.resource_gate.admit()
    }

    /// Authorise a one-shot resource override. **This is the authority.**
    ///
    /// A UI affordance must never be the thing that decides a declared limit may
    /// be bypassed. It previously was: the TUI reached through `Runtime` and
    /// called `ResourceGate::grant_override` directly, which checked one
    /// deployment-wide config flag and nothing else — no identity, no policy
    /// evaluation, and no audit row. The gate could not prevent that, because a
    /// `pub` mutator on a shared `Arc` is reachable by anything holding the
    /// runtime.
    ///
    /// So the authorization act lives here instead, and it is ordered so that a
    /// grant nobody can audit never happens:
    ///
    /// 1. Refuse if the deployment forbids overrides.
    /// 2. Write the `resource.override.granted` audit row, **fail-closed** — a
    ///    failure here returns an error and arms nothing.
    /// 3. Only then arm the one-shot slot.
    ///
    /// Audit-before-arm matters: arming first would leave a window where an
    /// unaudited grant is live and a later audit failure cannot take it back.
    pub async fn grant_resource_override(
        &self,
        reason: &str,
        operator: &str,
    ) -> Result<openrt_resources::ResourceOverride, Error> {
        if !self.resource_gate.limits().allow_operator_override {
            // Refusing here must not erase *why* the work is refused. The
            // operator asked to override something; policy says no; so the
            // caller gets the same structured denial it would have got anyway.
            // A peek, because answering the question must not spend anything.
            let standing = self.resource_gate.admit_peek();
            return Err(match standing.denial {
                Some(d) => Error::ResourceBlocked(d),
                // Policy forbids overrides but nothing is currently blocked, so
                // there is no denial to report — say so plainly instead of
                // inventing one.
                None => Error::Unavailable(
                    "resource overrides are disabled by operator policy (resources.allow_operator_override = false)"
                        .into(),
                ),
            });
        }
        // Fail-closed: an override that cannot be recorded is not an override.
        self.audit
            .append(
                "operator",
                "resource.override.granted",
                operator,
                "granted",
                serde_json::json!({
                    "reason": reason,
                    "note": "one-shot override requested; arms the next admission only",
                }),
            )
            .await
            .map_err(Error::Audit)?;
        if !self.resource_gate.grant_override(reason, operator) {
            // The policy check passed but the gate still refused: the slot could
            // not be written. Fail closed and leave the denial standing.
            let standing = self.resource_gate.admit_peek();
            return Err(match standing.denial {
                Some(d) => Error::ResourceBlocked(d),
                None => Error::Unavailable(
                    "resource override could not be armed (denial not recorded)".into(),
                ),
            });
        }
        Ok(openrt_resources::ResourceOverride {
            reason: reason.to_string(),
            operator: operator.to_string(),
        })
    }

    /// Record a consumed task-level override in the tamper-evident chain.
    ///
    /// The tool gate has always written a `resource.override` row when it
    /// honours an override; the task boundary did not, so an override consumed
    /// by `run_task` produced a successful execution with **no record at all**
    /// — the single place a declared limit is deliberately not enforced was
    /// also the one place with no evidence. Mirrors the tool gate's rule: if the
    /// row cannot be written, the run is refused.
    async fn audit_task_override(
        &self,
        admission: &openrt_resources::Admission,
    ) -> Result<(), Error> {
        if !admission.overridden {
            return Ok(());
        }
        let denial = admission
            .denial
            .as_ref()
            .map(|d| d.to_line())
            .unwrap_or_else(|| "(denial not recorded)".to_string());
        self.audit
            .append(
                "operator",
                "resource.override",
                admission.override_operator.as_deref().unwrap_or("unknown"),
                "allowed",
                serde_json::json!({
                    "scope": "task",
                    "denial": denial,
                    "reason": admission.override_reason,
                    "note": "task admission authorised by a one-shot operator override",
                }),
            )
            .await
            .map_err(Error::Audit)?;
        Ok(())
    }

    /// Run a task inline (awaits the full pipeline) and return its final state.
    pub async fn run_task(&self, session_id: SessionId, prompt: &str) -> Result<Task, Error> {
        // Preflight BEFORE the expensive turn. A refusal here is structured and
        // recoverable: the session and its transcript are untouched, and the
        // caller gets the denial with real remediation options.
        let admission = self.preflight_task().await;
        if !admission.allowed() {
            if let Some(denial) = admission.denial {
                return Err(Error::ResourceBlocked(denial));
            }
        }
        // An override consumed here has to be in the chain before the task runs.
        // A plain refusal writes nothing; an override that cannot be recorded
        // is refused rather than silently honoured.
        self.audit_task_override(&admission).await?;
        // Hold the per-session lock so a concurrent run on the same session
        // cannot interleave the transcript fold (SEC-08).
        let _guard = self.session_locks.lock(session_id).await;
        let session = self
            .store
            .load_session(session_id)
            .await
            .map_err(Error::Persistence)?;
        // M10.7 bounded instinct influence: active instincts (confidence
        // >= 0.5, top 3) append advisory context to the prompt — they can
        // only shape run context, never permissions.
        let prompt = self.instinct_context(prompt).await?;
        // M10.7 lifecycle: the run begins.
        self.hook_layer
            .fire(openrt_workspace_assets::HookTarget::RunStarted)
            .await?;
        let started = std::time::Instant::now();
        let task = self.build(&session, &prompt).await?;
        // M5.7 production path: every real run records its duration and
        // emits the metric on the Phase-1 bus (no test-only caller).
        let duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        self.metrics_observe_task(duration_ms).await?;
        // M11: drain the provider adapter's observed cold/warm tier hit/miss
        // counters onto the reserved model_io.* gauges (gauges only — the
        // durable ledger row belongs to the telemetry cycle).
        self.emit_model_load_gauges().await?;
        // M5.4c genome consumption: every completed run consults the active
        // genome (hooks for RunCompleted + active instincts as context).
        // Failure to load the genome fails the task path closed (surfaced).
        self.consume_active_genome_on_run().await?;
        Ok(task)
    }

    /// Run a task on a background task with a cancellation handle.
    pub async fn spawn_task(
        &self,
        session_id: SessionId,
        prompt: &str,
    ) -> Result<TaskHandle, Error> {
        // Fail fast on a missing session here; the fresh snapshot is loaded in
        // `TaskHandle::execute` under the per-session lock (SEC-08).
        self.store
            .load_session(session_id)
            .await
            .map_err(Error::Persistence)?;
        let cancel = openrt_core::CancellationToken::new();
        let pause = std::sync::Arc::new(openrt_core::PauseToken::new());
        let pipeline = self.snapshot_pipeline();
        // M10.7 bounded instinct influence (same view as `run_task`).
        let prompt = self.instinct_context(prompt).await?;
        let task = openrt_core::Task::new(session_id, &prompt);
        // Phase 10 M10.4: persist the live handle's task *now* (Created) so
        // `task_inspect`/`task_status`/`task_cancel` can see it before
        // `execute` starts, and register its token as the in-process
        // authority for cancel/terminate/recover/pause/resume.
        self.store.save_task(&task).await?;
        self.active_tasks.lock().unwrap().insert(
            task.id,
            ActiveTask {
                cancel: cancel.clone(),
                pause: pause.clone(),
            },
        );
        Ok(TaskHandle {
            task,
            cancel,
            pause,
            session_locks: self.session_locks.clone(),
            store: self.store.clone(),
            pipeline,
            active_tasks: self.active_tasks.clone(),
            hooks: self.hook_layer.clone(),
            task_budget: crate::run::TaskExecutionBudget::from(&self.config.runtime),
        })
    }

    /// Emit the `RuntimeStopping` bookend and flush the bus so every prior
    /// event (including history-ledger rows) is durable before return.
    /// CLI/process exit relies on this — without the flush, HistorySink
    /// writes are cancelled mid-flight when the tokio runtime drops.
    pub async fn shutdown(&self) {
        // M7B: close the managed browser session (best-effort) and MCP
        // children before the bus drains (idempotent).
        let _ = self.browser.close().await;
        self.mcp.shutdown();
        // Close every working-memory register file (zero-persistence; drop).
        {
            let mut working = self.working.lock().await;
            working.clear();
        }
        self.bus
            .emit_correlated(
                openrt_core::EventKind::RuntimeStopping,
                None,
                None,
                None,
                serde_json::json!({}),
            )
            .await;
        self.bus.flush().await;
    }

    // ---- safety (ADR-008) ----

    /// Operator-only computer-use kill switch. On a HALT(computer):
    ///
    /// * an auditable `SafetyHalt` event is emitted on the bus,
    /// * every subsequent `terminal.run`, `python.run`, `input.*`, `screen.*`
    ///   and `browser.*` call is blocked by the gate before it can run,
    /// * an in-flight terminal/python command is killed mid-action by the tool
    ///   itself.
    ///
    /// Returns whether the switch was newly tripped (the first tripper wins;
    /// later calls preserve the original reason). An audit-recording failure
    /// surfaces as an error: the HALT is in force, but the signal could not
    /// be durably recorded.
    pub async fn halt(&self, reason: &str) -> Result<bool, Error> {
        let tripped = self.kill_switch.trip(reason);
        if tripped {
            self.bus
                .emit_correlated(
                    openrt_core::EventKind::SafetyHalt,
                    None,
                    None,
                    None,
                    serde_json::json!({
                        "scope": openrt_safety::KillScope::Computer.as_str(),
                        "reason": reason,
                        "source": "operator",
                    }),
                )
                .await;
            // ADR-002/ADR-008: the signal emission itself is recorded in the
            // chained audit log (fail-closed — a recorder error is surfaced).
            self.audit
                .append(
                    "operator",
                    "safety.halt",
                    openrt_safety::KillScope::Computer.as_str(),
                    "tripped",
                    serde_json::json!({ "reason": reason }),
                )
                .await?;
        }
        Ok(tripped)
    }

    /// Record an operator-or-system event in the ADR-002 chained audit log.
    /// Fail-closed: the recorder error propagates (the caller decides whether
    /// to treat it as fatal).
    pub async fn audit(
        &self,
        actor: &str,
        action: &str,
        subject: &str,
        result: &str,
        ext: serde_json::Value,
    ) -> Result<u64, Error> {
        Ok(self
            .audit
            .append(actor, action, subject, result, ext)
            .await?)
    }

    /// Production audit chain verification (M6.2): recompute every hash.
    pub async fn audit_verify(&self) -> Result<u64, Error> {
        Ok(self.audit.verify().await?)
    }

    /// Production audit search (M6.2): aggregation/query over the chain.
    pub async fn audit_query(
        &self,
        filter: &openrt_audit::AuditQuery,
    ) -> Result<Vec<openrt_audit::AuditRecord>, Error> {
        Ok(self.audit.query(filter).await?)
    }

    /// Production audit retention status (head/tail of the verifiable chain).
    pub async fn audit_retention(&self) -> Result<openrt_audit::RetentionStatus, Error> {
        Ok(self.audit.retention_status().await?)
    }

    /// The tool schemas actually advertised to the model: the registry's
    /// provider tools minus everything the permission policy DENIES (deny-only
    /// filter — `Ask` and `Allow` stay visible so the operator decides at the
    /// gate). This is the SAME list `MinimalAgent` attaches to every
    /// `ModelRequest`; `openrt tool list` and `openrt doctor` report from it
    /// so the diagnostic can never disagree with the wire.
    pub fn advertised_tools(&self) -> Vec<serde_json::Value> {
        advertised_tools(&self.registry, &self.permission_policy)
    }

    // ---- M6.2 versioned security policy document (operator-only) ----

    /// Load the active policy document (fail-closed if uninitialized).
    pub async fn policy_document(&self) -> Result<openrt_policy::PolicyDocument, Error> {
        let store = openrt_policy::PolicyDocumentStore::new(self.store.clone());
        Ok(store.load_current().await?)
    }

    /// Ensure a deny-by-default policy document exists (idempotent bootstrap).
    pub async fn policy_document_ensure(&self) -> Result<openrt_policy::PolicyDocument, Error> {
        let store = openrt_policy::PolicyDocumentStore::new(self.store.clone());
        Ok(store.ensure_initialized("operator", "bootstrap").await?)
    }

    /// Append a rule to the versioned policy document (operator-only
    /// production path; agent code has no handle to this method).
    /// Note: the in-process gate policy is built at Runtime construction from
    /// config; document appends take effect for newly built runtimes and for
    /// [`Runtime::policy_document`] readers. The gate continues to enforce the
    /// live `permission_policy` Arc (never a second authority).
    pub async fn policy_append_rule(
        &self,
        actor: &str,
        note: &str,
        pattern: &str,
        action: openrt_permissions::RuleAction,
        scope: Option<String>,
    ) -> Result<openrt_policy::PolicyDocument, Error> {
        let store = openrt_policy::PolicyDocumentStore::new(self.store.clone());
        let doc = store
            .append_rule(actor, note, pattern, action, scope)
            .await?;
        self.audit
            .append(
                actor,
                "policy.append",
                pattern,
                "appended",
                serde_json::json!({
                    "version": doc.version,
                    "action": format!("{action:?}"),
                    "note": note,
                }),
            )
            .await?;
        Ok(doc)
    }

    /// List all policy document revisions (audit surface).
    pub async fn policy_versions(&self) -> Result<Vec<u64>, Error> {
        let store = openrt_policy::PolicyDocumentStore::new(self.store.clone());
        Ok(store.list_versions().await?)
    }

    // ---- M6.3 secrets / netpol production surfaces ----

    /// Open the encrypted secret store over this runtime's store + keyfile.
    pub async fn secrets(&self) -> Result<std::sync::Arc<openrt_secrets::SecretStore>, Error> {
        let keyfile = openrt_secrets::default_keyfile();
        let store = openrt_secrets::SecretStore::open_keyfile(self.store.clone(), &keyfile).await?;
        Ok(std::sync::Arc::new(store.with_audit(self.audit.clone())))
    }

    /// Network policy (default-deny egress grants) over ModeManager.
    pub fn netpol(&self) -> openrt_netpol::NetworkPolicy {
        openrt_netpol::NetworkPolicy::new(self.store.clone())
    }

    // ---- M6.4 enclave / observability production surfaces ----

    pub fn enclave(&self) -> openrt_enclave::Enclave {
        openrt_enclave::Enclave::new(self.store.clone())
    }

    pub fn observability(&self) -> openrt_observability::Observability {
        openrt_observability::Observability::new(self.store.clone())
    }

    // ---- M6.1 remainder: autonomous unattended runs ----

    pub fn supervisor(&self, holder: impl Into<String>) -> openrt_supervisor::Supervisor {
        openrt_supervisor::Supervisor::new(self.store.clone(), holder)
    }

    /// Cancel the in-flight orchestrated workflow run (P2.7). Trips the token
    /// recovery runs derive; the engine marks the run `Cancelled` rather than
    /// continuing to schedule nodes.
    pub fn cancel_workflow(&self) {
        self.engine_cancel.cancel();
    }

    /// The operator-visible halt state (reason when engaged, `None` when clear).
    pub fn halted(&self) -> Option<String> {
        self.kill_switch.engaged()
    }

    /// Clear a tripped switch after an operator-verified all-clear.
    pub fn clear_halt(&self) {
        self.kill_switch.reset();
    }

    /// Build a computer-use loop over this runtime's gated executor and screen
    /// backend (M2.5). `D` maps a screen snapshot to the next action.
    pub fn computer_loop<D>(
        &self,
        max_steps: usize,
        decider: D,
    ) -> openrt_computer::ComputerUseLoop<D>
    where
        D: FnMut(&openrt_computer::ScreenCapture) -> Option<openrt_computer::ToolCall>,
    {
        openrt_computer::ComputerUseLoop::new(
            self.screen.clone(),
            self.executor.clone(),
            Some(self.kill_switch.clone()),
            max_steps,
            decider,
        )
    }
}

impl Runtime {
    // ---- execution history / replay / fork / time machine (M5.2) ----

    /// Tail sequence of the append-only parent ledger (0 when empty).
    pub async fn history_tail(&self) -> Result<u64, Error> {
        Ok(openrt_timeseries::tail_seq(&*self.store).await?)
    }

    /// Recompute every ledger digest and confirm the chain. Fail-closed.
    pub async fn history_verify(&self) -> Result<u64, Error> {
        Ok(openrt_timeseries::verify_chain(&*self.store).await?)
    }

    /// Time-machine view: materialize the ledger as of `as_of_seq` (read-only;
    /// later history is never rewritten).
    pub async fn history_replay(
        &self,
        as_of_seq: u64,
    ) -> Result<Vec<openrt_timeseries::HistoryRecord>, Error> {
        Ok(openrt_timeseries::materialize_as_of(&*self.store, as_of_seq).await?)
    }

    /// Fork the current head (or `base_label`, e.g. `seq.000000000003`) into
    /// an isolated append-only branch. Parent ledger is never written.
    /// Fail-closed (M10.5): a base beyond the ledger tail would silently
    /// produce a fork anchored to a missing digest — refused up front.
    pub async fn history_fork(&self, branch: &str, base_label: &str) -> Result<(), Error> {
        if branch.trim().is_empty() {
            return Err(Error::History(openrt_timeseries::HistoryError::Corrupt(
                "branch name must not be empty".into(),
            )));
        }
        if let Some(base_seq) = base_label
            .strip_prefix("seq.")
            .and_then(|s| s.parse::<u64>().ok())
        {
            let tail = openrt_timeseries::tail_seq(&*self.store).await?;
            if base_seq > tail {
                return Err(Error::History(openrt_timeseries::HistoryError::NotFound(
                    format!("base `{base_label}` is beyond ledger tail {tail}"),
                )));
            }
        }
        Ok(openrt_timeseries::create_fork(&*self.store, branch, base_label).await?)
    }

    /// Append one commit onto an existing fork branch (divergent execution).
    pub async fn history_fork_commit(
        &self,
        branch: &str,
        node_id: &str,
        status: &str,
        event_kind: &str,
        note: &str,
    ) -> Result<u64, Error> {
        Ok(
            openrt_timeseries::fork_commit(&*self.store, branch, node_id, status, event_kind, note)
                .await?,
        )
    }

    /// Verify a fork branch's chain (bases on the parent head digest).
    pub async fn history_fork_verify(&self, branch: &str) -> Result<u64, Error> {
        Ok(openrt_timeseries::fork_verify_chain(&*self.store, branch).await?)
    }
}

impl Runtime {
    // ---- semantic / episodic memory (M5.3) ----

    /// Register a durable origin-run so memory citations can re-resolve
    /// (fail-closed when the origin is missing at remember/recall time).
    pub async fn memory_remember_origin(
        &self,
        origin_run: &str,
        canonical_origin: &str,
    ) -> Result<(), Error> {
        Ok(openrt_memory::remember_origin(&*self.store, origin_run, canonical_origin).await?)
    }

    /// Persist a semantic fact (digest-sealed, citation re-validated) and a
    /// Phase-4 retrieval chunk so `memory_recall` has a real substrate.
    /// Explicit `override_existing` is required to supersede an id.
    /// Sensitive keys are redacted at write (SEC-05) before anything is
    /// sealed — secrets never enter the durable store in cleartext.
    pub async fn memory_remember(
        &self,
        id: &str,
        origin_run: &str,
        origin_source: &str,
        scope: &str,
        key: &str,
        value: &str,
        occurred_at: &str,
        override_existing: bool,
    ) -> Result<openrt_memory::SemanticItem, Error> {
        // Redaction-at-write: field-name policy from openrt-core, applied
        // before the value is digested/sealed (never repaired later).
        let mut value_json = serde_json::Value::String(value.to_string());
        let mut pair = serde_json::json!({ key: value_json.clone() });
        openrt_core::RedactionPolicy::new().redact_value(&mut pair);
        let redacted = pair
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or(value)
            .to_string();
        value_json = serde_json::Value::String(redacted);
        let final_value = value_json.as_str().unwrap_or(value).to_string();
        Ok(openrt_memory::remember_fact(
            &*self.store,
            id,
            origin_run,
            origin_source,
            scope,
            key,
            &final_value,
            occurred_at,
            override_existing,
        )
        .await?)
    }

    /// Query-backed recall through Phase-4 `openrt_knowledge::retrieve`.
    /// Every presented item re-opens fail-closed (digest + citation) before it
    /// may enter the set; hits that fail are dropped, never repaired. An empty
    /// set is an honest "no verified evidence" answer — never invented.
    pub async fn memory_recall(
        &self,
        query: &str,
        top_k: usize,
        threshold: f32,
    ) -> Result<openrt_memory::RecallSet, Error> {
        Ok(openrt_memory::recall_fact(&*self.store, query, top_k, threshold).await?)
    }

    /// Persist an episodic run record (digest-sealed, citation re-resolved
    /// against registered origins before any later recall may present it).
    pub async fn memory_remember_episode(
        &self,
        run_id: &str,
        session_id: &str,
        category: &str,
        occurred_at: &str,
        payload: &str,
        origin_run: &str,
        origin_source: &str,
        override_existing: bool,
    ) -> Result<openrt_memory::EpisodicRecord, Error> {
        // Redaction-at-write (SEC-05): scrub secret-like keys from the
        // payload JSON before it is digested and sealed into the store.
        let mut payload_value: serde_json::Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => serde_json::Value::String(payload.to_string()),
        };
        openrt_core::RedactionPolicy::new().redact_value(&mut payload_value);
        let safe_payload = payload_value.to_string();
        Ok(openrt_memory::remember_episode(
            &*self.store,
            run_id,
            session_id,
            category,
            occurred_at,
            &safe_payload,
            origin_run,
            origin_source,
            override_existing,
        )
        .await?)
    }

    /// Recall one episodic record by digest (fail-closed digest + citation).
    pub async fn memory_recall_episode(
        &self,
        digest: &str,
    ) -> Result<openrt_memory::EpisodicRecord, Error> {
        Ok(openrt_memory::recall_episode(&*self.store, digest).await?)
    }

    // ---- working memory (M5.3) — zero-persistence, session-scoped ----

    /// Lazily ensure a live working-memory register file exists for `session`.
    async fn working_slot(
        &self,
        session: SessionId,
    ) -> Result<
        tokio::sync::MutexGuard<
            '_,
            std::collections::HashMap<SessionId, openrt_memory::WorkingMemory>,
        >,
        Error,
    > {
        let mut guard = self.working.lock().await;
        guard.entry(session).or_insert_with(|| {
            openrt_memory::WorkingMemory::new("live".into(), session.to_string())
        });
        Ok(guard)
    }

    /// Write a working-memory slot for the active session. Zero-persistence:
    /// never reaches `store`; fails closed after `memory_working_end`.
    pub async fn memory_working_set(
        &self,
        session: SessionId,
        key: &str,
        value: &str,
        origin_run: &str,
    ) -> Result<(), Error> {
        let mut guard = self.working_slot(session).await?;
        let wm = guard
            .get_mut(&session)
            .expect("working_slot inserts the entry");
        Ok(wm.set(
            key.to_string(),
            value.to_string(),
            origin_run.to_string(),
            session.to_string(),
        )?)
    }

    /// Read a working-memory slot (fail-closed: Unset / DigestMismatch /
    /// SessionEnded).
    pub async fn memory_working_get(
        &self,
        session: SessionId,
        key: &str,
    ) -> Result<openrt_memory::WorkingSlot, Error> {
        let guard = self.working_slot(session).await?;
        let wm = guard
            .get(&session)
            .ok_or_else(|| openrt_memory::WorkingError::SessionEnded)?;
        Ok(wm.get(key)?.clone())
    }

    /// True when the slot is present and digest-valid.
    pub async fn memory_working_has(&self, session: SessionId, key: &str) -> Result<bool, Error> {
        let guard = self.working_slot(session).await?;
        Ok(guard.get(&session).map(|wm| wm.has(key)).unwrap_or(false))
    }

    /// End the session's working memory: registers close (fail closed);
    /// entry dropped so no stale live state survives. Zero-persistence.
    pub async fn memory_working_end(&self, session: SessionId) -> Result<(), Error> {
        let mut guard = self.working.lock().await;
        if let Some(wm) = guard.get_mut(&session) {
            wm.end_session();
        }
        guard.remove(&session);
        Ok(())
    }
}

impl Runtime {
    async fn build(&self, session: &Session, prompt: &str) -> Result<Task, Error> {
        let cancel = openrt_core::CancellationToken::new();
        let pipeline = self.snapshot_pipeline();
        let task = openrt_core::Task::new(session.id, prompt);
        let budget = self.task_execution_budget();
        pipeline
            .run_supervised(session, task, prompt, &cancel, None, budget)
            .await
    }
}

impl Runtime {
    /// Supervisor budget for this runtime's config (finite by default; the
    /// global ceilings — turns / tool calls / wall clock / segment count —
    /// bound the whole task across automatically-chained segments).
    pub(crate) fn task_execution_budget(&self) -> crate::run::TaskExecutionBudget {
        crate::run::TaskExecutionBudget::from(&self.config.runtime)
    }
}

impl Runtime {
    // ---- multi-agent workflows (P2.3) ----

    /// Open the execution-scoped shared blackboard over this runtime's store
    /// (hydrated with any persisted `exec.blackboard` records).
    pub async fn blackboard(&self) -> Result<Arc<openrt_blackboard::Blackboard>, Error> {
        Ok(Arc::new(
            openrt_blackboard::Blackboard::with_store(
                self.store.clone(),
                openrt_blackboard::BLACKBOARD_NS,
            )
            .await?,
        ))
    }

    /// Run a workflow DAG over the role sub-agents backed by this runtime's
    /// provider/executor. The run is recorded as a task with its own
    /// execution record; per-step artifacts live in the shared blackboard and
    /// both the DAG definition (`dag/`) and run report (`runs/`) persist via
    /// the ADR-006 store.
    ///
    /// This is the P2.6 checkpointed path: a checkpoint is written before the
    /// first wave, after every node, and when the workflow terminates. See
    /// [`crate::checkpoint`] for the recovery surface (`resume_workflow`,
    /// `retry_failed`, `resume_dry_run`, `rollback`, …).
    pub async fn run_workflow(
        &self,
        session_id: SessionId,
        workflow: openrt_orch::Workflow,
    ) -> Result<openrt_orch::WorkflowRunReport, Error> {
        self.run_workflow_checkpointed(
            session_id,
            workflow,
            crate::checkpoint::CheckpointOptions::default(),
            None,
        )
        .await
    }

    // ---- Phase 6: workflow compiler production entry points ----

    /// Compile a natural-language goal into a validated, persisted workflow
    /// using the REAL tool registry, permission policy, operating mode, and
    /// resource gate (fail-closed; never invents capabilities).
    pub async fn compile_workflow(
        &self,
        request: &str,
        mode: openrt_compiler::CompileMode,
    ) -> Result<openrt_compiler::CompileOutput, Error> {
        use openrt_compiler::{
            CompileInput, NaturalLanguageGoal, ResourceContext, WorkflowCompiler,
        };

        let network_allowed = self.modes.current().await?.mode.network_allowed();
        let tool_names = self.registry.names();

        // Same Policy the gate enforces (no second policy authority).
        let policy = self.permission_policy.clone();
        let policy_allows = std::sync::Arc::new(move |perm: &str| {
            use openrt_permissions::{Capability, PermissionRequest, PolicyEvaluator};
            let req = PermissionRequest::capability(
                "workflow.compile",
                Capability::from_str_unchecked(perm),
                None,
            );
            PolicyEvaluator::new(&policy)
                .evaluate(&req)
                .allows_execution()
        });

        // Compiling a workflow is not running it, so this reports the current
        // posture without spending an outstanding override. The scheduler and
        // sandbox admission paths keep the consuming `check()` because those
        // genuinely admit work.
        let resource = match self.resource_gate.check_peek() {
            openrt_resources::ResourceDecision::Allow => ResourceContext {
                summary: self.resource_gate.current().summary(),
                denied: false,
                reason: None,
            },
            openrt_resources::ResourceDecision::Denied { reason } => ResourceContext {
                summary: self.resource_gate.current().summary(),
                denied: true,
                reason: Some(reason),
            },
        };

        let input = CompileInput::new(NaturalLanguageGoal::new(request))
            .with_mode(mode)
            .with_tools(tool_names)
            .with_policy(policy_allows)
            .with_network(network_allowed)
            .with_model_available(true)
            .with_resource(resource);

        WorkflowCompiler::new()
            .compile(&input, Some(&self.store))
            .await
            .map_err(Error::Compile)
    }

    /// Load a previously compiled workflow by id (compiled record + DAG graph).
    pub async fn load_compiled_workflow(
        &self,
        workflow_id: &str,
    ) -> Result<(openrt_compiler::CompiledRecord, openrt_orch::Workflow), Error> {
        let rec = openrt_compiler::load_compiled(&self.store, workflow_id)
            .await?
            .ok_or_else(|| {
                Error::Orchestration(openrt_orch::WorkflowError::Condition(format!(
                    "no compiled workflow `{workflow_id}` in namespace `compiled`"
                )))
            })?;
        // Prefer the executable graph from `dag`; fail closed if absent.
        let wf = openrt_compiler::load_dag_workflow(&self.store, workflow_id)
            .await?
            .ok_or_else(|| {
                Error::Orchestration(openrt_orch::WorkflowError::Condition(format!(
                    "compiled record `{workflow_id}` exists but DAG graph missing from `dag`"
                )))
            })?;
        Ok((rec, wf))
    }

    /// Execute a compiled workflow through the existing orchestrator
    /// (checkpointed run + resource admission on every new step spawn).
    pub async fn run_compiled(
        &self,
        session_id: SessionId,
        workflow_id: &str,
    ) -> Result<openrt_orch::WorkflowRunReport, Error> {
        let (_rec, wf) = self.load_compiled_workflow(workflow_id).await?;
        self.run_workflow(session_id, wf).await
    }

    /// Compile + (when mode executes) hand off to the orchestrator in one call.
    pub async fn compile_and_run(
        &self,
        session_id: SessionId,
        request: &str,
    ) -> Result<
        (
            openrt_compiler::CompileOutput,
            openrt_orch::WorkflowRunReport,
        ),
        Error,
    > {
        let out = self
            .compile_workflow(request, openrt_compiler::CompileMode::Execute)
            .await?;
        let report = self.run_workflow(session_id, out.workflow.clone()).await?;
        Ok((out, report))
    }

    /// Bind every role to a fresh `MinimalAgent` sharing this runtime's
    /// provider, executor and gate. Role identity is injected by the engine
    /// (each step's `role_prompt` is prepended to the step prompt).
    pub(crate) fn role_agents(
        &self,
    ) -> std::collections::HashMap<openrt_orch::SubAgentKind, Arc<dyn openrt_agents::Agent>> {
        use openrt_orch::SubAgentKind;
        let limits = openrt_agents::AgentTurnLimits {
            max_turns: self.config.runtime.max_turns,
            max_tool_calls: self.config.runtime.max_tool_calls,
            // The workflow engine's node retries / RecoveryPolicy own the
            // retry accounting — a second, hidden provider-level retry would
            // make attempt counts incoherent.
            provider_retries: 0,
            // Part C surfaces: an operator-set streaming idle budget and
            // context guard ratio must reach the agent, or the config knobs
            // silently do nothing.
            idle_timeout_ms: self.config.runtime.agent_idle_timeout_ms,
            context_guard_ratio: self.config.runtime.agent_context_guard_ratio,
            ..openrt_agents::AgentTurnLimits::default()
        };
        let tools = advertised_tools(&self.registry, &self.permission_policy);
        let mut agents = std::collections::HashMap::new();
        for kind in [
            SubAgentKind::Planner,
            SubAgentKind::Researcher,
            SubAgentKind::Coder,
            SubAgentKind::Verifier,
            SubAgentKind::Critic,
        ] {
            agents.insert(
                kind,
                Arc::new(
                    openrt_agents::MinimalAgent::new(
                        format!("role.{}", kind),
                        self.provider.clone(),
                        self.executor.clone(),
                        limits,
                        tools.clone(),
                    )
                    .with_system_prompt(environment_system_prompt(&self.config))
                    .with_parameters(generation_parameters(&self.config.runtime.generation)),
                ) as Arc<dyn openrt_agents::Agent>,
            );
        }
        agents
    }
}

/// Handle to a background task: cancel + retrieve the final task.
pub struct TaskHandle {
    task: openrt_core::Task,
    cancel: openrt_core::CancellationToken,
    /// Part C: resettable cooperative pause signal for this in-flight run.
    pause: std::sync::Arc<openrt_core::PauseToken>,
    session_locks: crate::locks::SessionLocks,
    store: Arc<dyn PersistenceStore>,
    pipeline: crate::run::Pipeline,
    active_tasks: std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<openrt_core::TaskId, ActiveTask>>,
    >,
    /// M10.7 lifecycle: fires TaskStarted/TaskCompleted around the run.
    hooks: std::sync::Arc<crate::hooks::HookLayer>,
    /// Supervisor global-budget snapshot (bounded by default) for this task's
    /// full multi-segment lifetime.
    task_budget: crate::run::TaskExecutionBudget,
}

/// The in-process authority for one live task: its cooperative cancellation
/// and (Part C) resettable pause signals. Stored centrally so the runtime's
/// `task_pause`/`task_resume`/`task_cancel` address the SAME signal the agent
/// loop is watching — never a second, UI-local channel.
pub(crate) struct ActiveTask {
    pub cancel: openrt_core::CancellationToken,
    pub pause: std::sync::Arc<openrt_core::PauseToken>,
}

impl Drop for TaskHandle {
    fn drop(&mut self) {
        // Phase 10 M10.4: a handle that completed, was cancelled, or was
        // abandoned must not keep its task registered as live in this
        // process — deregister on every exit path so the store (not a
        // leaked token) decides `task_recover`.
        self.active_tasks.lock().unwrap().remove(&self.task.id);
    }
}

impl std::fmt::Debug for TaskHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskHandle")
            .field("task_id", &self.task.id)
            .finish()
    }
}

impl TaskHandle {
    pub fn task_id(&self) -> openrt_core::TaskId {
        self.task.id
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Part C: request a cooperative pause — the run stops at its next turn
    /// boundary and persists as `Paused` (never terminal); `resume` on the
    /// runtime re-runs it. Idempotent while already paused.
    pub fn pause(&self) {
        self.pause.pause();
    }

    /// Part C: clear a pause requested on this in-flight run (only useful
    /// while [`Self::execute`] has not yet returned; once it has, the task is
    /// durably `Paused` and resume goes through the runtime's authority).
    pub fn resume(&self) {
        self.pause.resume();
    }

    /// Spawn and await completion.
    pub async fn execute(self) -> Result<Task, Error> {
        // With the `Drop` deregistration above the handle cannot be
        // destructured: run from clones of the shared pieces (all cheap
        // Arc/clone handles) and let `Drop` clean the registry on every path.
        let task = self.task.clone();
        // Take the per-session lock and load the session *inside* it so the
        // transcript fold cannot interleave with another run on the same
        // session between spawn and execute (SEC-08).
        let _guard = self.session_locks.lock(task.session_id).await;
        let session = self
            .store
            .load_session(task.session_id)
            .await
            .map_err(Error::Persistence)?;
        // M10.7 lifecycle: the task run begins.
        self.hooks
            .fire(openrt_workspace_assets::HookTarget::TaskStarted)
            .await?;
        let pipeline = self.pipeline.clone();
        let cancel = self.cancel.clone();
        let pause = self.pause.clone();
        let budget = self.task_budget;
        let t = task.clone();
        let done = tokio::spawn({
            let prompt = t.prompt.clone();
            async move {
                pipeline
                    .run_supervised(&session, t, &prompt, &cancel, Some(pause), budget)
                    .await
            }
        })
        .await
        .map_err(|e| Error::Join(e.to_string()))?;
        // M10.7 lifecycle: the task run reached its end (success or
        // failure). On a failed run the run error wins — a secondary hook
        // failure must not mask the surfaced run outcome.
        match done {
            Ok(task) => {
                self.hooks
                    .fire(openrt_workspace_assets::HookTarget::TaskCompleted)
                    .await?;
                Ok(task)
            }
            Err(e) => {
                let _ = self
                    .hooks
                    .fire(openrt_workspace_assets::HookTarget::TaskCompleted)
                    .await;
                Err(e)
            }
        }
    }
}

/// Assembly entry point.
#[derive(Default)]
pub struct RuntimeBuilder {
    config: Option<openrt_config::Config>,
    store: Option<Arc<dyn PersistenceStore>>,
    provider_override: Option<Arc<dyn openrt_models::ModelProvider>>,
    sinks: Vec<Arc<dyn EventSink>>,
    bus_capacity: usize,
    permission_resolver: Option<Arc<dyn openrt_permissions::PermissionResolver>>,
    gpu_observer_override: Option<Arc<dyn openrt_resources::GpuObserver>>,
}

impl RuntimeBuilder {
    pub fn new() -> Self {
        Self {
            config: None,
            store: None,
            provider_override: None,
            sinks: Vec::new(),
            bus_capacity: 1024,
            permission_resolver: None,
            gpu_observer_override: None,
        }
    }

    pub fn config(mut self, config: openrt_config::Config) -> Self {
        self.config = Some(config);
        self
    }

    /// Inject a store instead of building from config.
    pub fn store(mut self, store: Arc<dyn PersistenceStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Force the provider (e.g. mock for offline demo/tests). Respects config
    /// otherwise.
    pub fn with_provider(mut self, provider: Arc<dyn openrt_models::ModelProvider>) -> Self {
        self.provider_override = Some(provider);
        self
    }

    /// Attach an extra event sink (ordering added after the persistence sink).
    pub fn sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.sinks.push(sink);
        self
    }

    pub fn bus_capacity(mut self, cap: usize) -> Self {
        self.bus_capacity = cap.max(1);
        self
    }

    /// Attach an approval resolver used for `Ask` policy decisions (P2.2).
    /// When absent, `Ask` is a hard deny (fail closed).
    pub fn permission_resolver(
        mut self,
        resolver: Arc<dyn openrt_permissions::PermissionResolver>,
    ) -> Self {
        self.permission_resolver = Some(resolver);
        self
    }

    /// Inject a VRAM observer (M9). Defaults to the honest no-op observer.
    pub fn gpu_observer(mut self, observer: Arc<dyn openrt_resources::GpuObserver>) -> Self {
        self.gpu_observer_override = Some(observer);
        self
    }

    /// Build and start the runtime. Requires a running tokio runtime.
    /// Assemble the runtime. Returns `Arc<Runtime>` so shared subsystems
    /// (Phase 11 extension tool bindings) can hold a `Weak` handle without
    /// keeping the runtime alive.
    pub async fn build(self) -> Result<Arc<Runtime>, Error> {
        let config = self.config.unwrap_or_default();
        config.validate()?;

        // Store.
        let store: Arc<dyn PersistenceStore> = match self.store {
            Some(s) => s,
            None => match config.persistence.kind {
                openrt_config::StoreKind::File => {
                    let dir = config
                        .persistence
                        .dir
                        .clone()
                        .unwrap_or_else(openrt_persistence::default_store_dir);
                    Arc::new(openrt_persistence::LocalFileStore::open(&dir).await?)
                }
                openrt_config::StoreKind::Memory => Arc::new(MemoryStore::new()),
            },
        };

        // Event bus: persistence sink first, then history (M5.2), then caller sinks.
        let persistence_sink = Arc::new(PersistenceSink::new(store.clone()));
        let event_log_health = persistence_sink.health().clone();
        let history_sink = Arc::new(crate::sinks::HistorySink::new(store.clone()));
        let mut sink_builder = EventBusBuilder::new().capacity(self.bus_capacity);
        sink_builder = sink_builder.sink(persistence_sink);
        sink_builder = sink_builder.sink(history_sink);
        for s in self.sinks {
            sink_builder = sink_builder.sink(s);
        }
        let (bus, dispatcher) = sink_builder.start();
        bus.emit_correlated(
            openrt_core::EventKind::RuntimeStarted,
            None,
            None,
            None,
            serde_json::json!({ "mode": serde_json::to_value(config.mode).unwrap_or_default() }),
        )
        .await;

        // Provider.
        let provider: Arc<dyn openrt_models::ModelProvider> = match self.provider_override {
            Some(p) => p,
            None => build_default_provider(&config)?,
        };

        // P3 model fabric (ADR-010): one mode engine + registry + router over
        // the same store. Config supplies the *initial* mode only when no
        // operator transition is persisted yet (fail-closed bootstrap).
        let modes = openrt_modes::ModeManager::new(store.clone());
        modes.seed_from_config(config.mode.into()).await?;
        // A second manager over the same store is coherent: both read the
        // persisted record on every call, so operator transitions are visible
        // to the registry without shared mutable state.
        let model_registry = openrt_registry::ModelRegistry::new(
            store.clone(),
            openrt_modes::ModeManager::new(store.clone()),
        );
        let router = openrt_router::Router::new();

        // Phase 11 M11.2: load the durable asset store ONCE here, before the
        // registry snapshot and MCP wiring, so Active-at-build extensions
        // bind through the ONE registry and the ONE MCP manager (never a
        // second authority; fail-closed like the end-of-build load).
        let ext_assets = crate::phase5::load_assets_quiet(&*store, &modes, &config).await?;

        // Policy + resources + gate.
        let policy = Arc::new(policy_from_config(&config.permissions));
        let resource_gate = Arc::new(resource_gate_from_config(&config.resources));
        let resources = resource_gate.clone();
        // M9: optional VRAM observer; defaults to the honest no-op observer.
        let gpu_observer: Arc<dyn openrt_resources::GpuObserver> = self
            .gpu_observer_override
            .unwrap_or_else(|| Arc::new(openrt_resources::NoGpuObserver));
        // M5.4: the gate policy and the hook permission checks share ONE Arc
        // (no second policy authority).
        let permission_policy = policy.clone();
        // SEC-01: confine the filesystem tools to the configured workspace;
        // without a workspace the tools stay registered but forbid everything.
        let mut registry = match &config.runtime.workspace_dir {
            Some(dir) => openrt_tools::ToolRegistry::with_filesystem_root(dir),
            None => openrt_tools::ToolRegistry::builtins(),
        };
        // M2.3/M2.5 computer-use tools (filesystem write/list/move/delete,
        // terminal, screen/input/browser) share the same fail-closed jail.
        let computer_root = config.runtime.workspace_dir.as_deref();
        // M2.4 python sandbox tool.
        let sb = config.runtime.python_sandbox.as_ref();
        let python = sb
            .and_then(|s| s.python.as_ref())
            .cloned()
            .unwrap_or_else(|| std::path::PathBuf::from("/usr/bin/python3"));
        let mut python_tool = openrt_sandbox::PythonRun::new(python);
        if let Some(sb) = sb {
            python_tool.limits = openrt_sandbox::SandboxLimits {
                wall_timeout: sb
                    .wall_timeout_secs
                    .map(std::time::Duration::from_secs)
                    .unwrap_or(openrt_sandbox::DEFAULT_LIMITS.wall_timeout),
                cpu_seconds: sb
                    .cpu_seconds
                    .unwrap_or(openrt_sandbox::DEFAULT_LIMITS.cpu_seconds),
                memory_bytes: sb
                    .memory_bytes
                    .unwrap_or(openrt_sandbox::DEFAULT_LIMITS.memory_bytes),
                max_procs: sb
                    .max_procs
                    .unwrap_or(openrt_sandbox::DEFAULT_LIMITS.max_procs),
                output_bytes: sb
                    .output_bytes
                    .unwrap_or(openrt_sandbox::DEFAULT_LIMITS.output_bytes),
            };
        }
        // ADR-008 kill switch: shared with the gate, terminal and sandbox so a
        // HALT aborts in-flight computer actions.
        let kill_switch = std::sync::Arc::new(openrt_safety::KillSwitch::new());
        python_tool.sandbox.kill = Some(kill_switch.clone());
        // Fail-safe (P2.7): bubblewrap is the only confined backend; the
        // degraded unconfined fallback runs only when explicitly opted in.
        python_tool.allow_unconfined_fallback = sb
            .and_then(|s| s.allow_unconfined_fallback)
            .unwrap_or(false);
        // ADR-002: chained audit log over the same store. Created here (not
        // at gate construction) so the sandbox authority and the gate share
        // ONE AuditLog instance — two instances would each load the same tail
        // and corrupt the chain. Shared with the gate below.
        let audit: std::sync::Arc<openrt_audit::AuditLog> =
            std::sync::Arc::new(openrt_audit::AuditLog::new(store.clone()));
        // Phase 13 (Part E): ONE NetworkPolicy instance shared by the sandbox
        // admission path, the model-egress guards and the browser tool
        // context. The egress guards also get their own ModeManager over the
        // same store — coherent reads, like the second manager above — so the
        // `modes` binding can still be moved into the Runtime itself.
        let netpol = std::sync::Arc::new(openrt_netpol::NetworkPolicy::new(store.clone()));
        let egress_modes = std::sync::Arc::new(openrt_modes::ModeManager::new(store.clone()));
        // ADR-010: wrap the ONE provider BEFORE anything clones it (agent
        // pipeline, verifier backends, model rerank), so no networked
        // `generate` can reach a socket without passing the egress authority.
        let provider: Arc<dyn openrt_models::ModelProvider> =
            std::sync::Arc::new(crate::model_egress::EgressGuardedProvider::new(
                provider,
                egress_modes.clone(),
                netpol.clone(),
                audit.clone(),
            ));
        // Phase 8 (M8.1/M8.9): unified sandbox authority — one admission flow
        // (policy → host ResourceGate → mode/netpol → backend detection →
        // execution → verified cleanup, audited as `sandbox.*`). `python.run`
        // is the first production caller (Strict profile; the unconfined
        // fallback follows the operator's explicit config opt-in).
        let sandbox = std::sync::Arc::new(
            openrt_sandbox::SandboxManager::new()
                .with_store(store.clone())
                .with_audit(audit.clone())
                .with_resources(resources.clone())
                .with_netpol(netpol.clone())
                .with_kill(kill_switch.clone())
                .with_policy(
                    openrt_sandbox::SandboxPolicy::for_profile(
                        openrt_sandbox::SandboxProfile::Strict,
                    )
                    .with_fallback(python_tool.allow_unconfined_fallback),
                )
                .with_policy(
                    openrt_sandbox::SandboxPolicy::for_profile(
                        openrt_sandbox::SandboxProfile::Development,
                    )
                    .with_fallback(python_tool.allow_unconfined_fallback),
                ),
        );
        sandbox.require("python.run", openrt_sandbox::SandboxProfile::Strict);
        python_tool.manager = Some(sandbox.clone());
        python_tool.profile = openrt_sandbox::SandboxProfile::Strict;
        let _ = registry.register(std::sync::Arc::new(python_tool));
        let screen =
            openrt_computer::default_screen_backend(config.runtime.screen_backend.as_deref());
        for tool in openrt_computer::computer_tools_with_screen(
            computer_root,
            Some(openrt_computer::TerminalLimits {
                timeout: config
                    .runtime
                    .terminal_timeout_secs
                    .map(std::time::Duration::from_secs)
                    .unwrap_or(openrt_computer::DEFAULT_TIMEOUT),
                max_output_bytes: config
                    .runtime
                    .terminal_max_output_bytes
                    .unwrap_or(openrt_computer::DEFAULT_MAX_OUTPUT_BYTES),
                ..Default::default()
            }),
            Some(kill_switch.clone()),
            screen.clone(),
        ) {
            // Names are unique (filesystem.*, terminal.run, screen.*, input.*,
            // browser.*); registration cannot fail here.
            let _ = registry.register(tool);
        }
        // P7 Tool Fabric: environment discovery + ComputerControl behind
        // `computer.*` tools. `computer_backend` config: `"sim"` forces the
        // deterministic harness (tests/CI), explicit ids pin a backend, and
        // `None`/`"auto"` discovers the host and resolves through the six-layer
        // priority. Fail-soft: if discovery panics we still build (never block
        // startup on a desktop probe).
        let computer_control =
            std::sync::Arc::new(match config.runtime.computer_backend.as_deref() {
                Some("sim") => openrt_platform::ComputerControl::with_profile(
                    openrt_platform::EnvironmentProfile::host(),
                    Some(openrt_platform::BackendKind::Sim),
                ),
                // Explicit backend id pins it; `"auto"`/unknown ids fall through
                // to natural discovery + six-layer priority resolution.
                Some(kind) => match openrt_platform::BackendKind::parse(kind) {
                    Some(k) => openrt_platform::ComputerControl::with_profile(
                        openrt_platform::EnvironmentProfile::host(),
                        Some(k),
                    ),
                    None => openrt_platform::ComputerControl::discover(),
                },
                None => openrt_platform::ComputerControl::discover(),
            });
        // Desktop-entry knowledge for `computer.app` (read-only scan).
        let host_apps = openrt_platform::apps::discover_host_apps();
        for tool in openrt_platform::computer_tools(computer_control.clone(), host_apps.clone()) {
            let _ = registry.register(tool);
        }
        // Phase 11 M11.8: the ONE `computer.use` tool — observation bundle →
        // semantic target resolution chain → digital twin (invalidated after
        // every action) → advisory plan → execution → bounded verification.
        // Vision grounding rides the single `openrt-vlm` pipeline and only
        // POINTS (never executes): VLM intent → policy → AgentShield →
        // ComputerControl → action → verification.
        let vlm_endpoint = config
            .providers
            .iter()
            .find(|p| p.kind == openrt_config::ProviderKind::Ollama)
            .and_then(|p| p.base_url.clone())
            .unwrap_or_else(|| openrt_vlm::DEFAULT_VLM_ENDPOINT.to_string());
        let computer_use = std::sync::Arc::new(
            openrt_computer::ComputerUse::new(screen.clone(), computer_control.clone())
                .with_vision(std::sync::Arc::new(
                    crate::model_egress::EgressGuardedVision::new(
                        vlm_endpoint.clone(),
                        std::sync::Arc::new(openrt_computer::VlmGrounding::with_default_model(
                            vlm_endpoint,
                        )),
                        egress_modes.clone(),
                        netpol.clone(),
                        audit.clone(),
                    ),
                )),
        );
        let _ = registry.register(std::sync::Arc::new(openrt_computer::ComputerUseTool {
            spec: openrt_computer::computer_use_spec(),
            engine: computer_use.clone(),
        }));
        // M7B Tool Fabric: database / office / browser / MCP / application
        // tools over the SAME registry, gate, policy, and audit path (one
        // execution authority — never a second one). Fail-soft wiring: a
        // subsystem that cannot start registers zero tools or honest
        // UNAVAILABLE tools; startup never blocks on a backend probe
        // (LibreOffice/MCP timeouts are bounded and only run for configured
        // backends).
        let fabric = openrt_fabric::FabricState::new(store.clone());
        let host = openrt_platform::EnvironmentProfile::host().profile_id;
        // Database connections from config (none configured → no tools
        // registered for them; list/query report UNAVAILABLE honestly).
        let database = std::sync::Arc::new(openrt_database::DatabaseManager::from_config(
            &config.fabric.database,
        ));
        for tool in openrt_database::database_tools(database.clone()) {
            let _ = registry.register(tool);
        }
        // Office tools (pure-Rust OOXML writers; soffice probed once for
        // render/export — absent backend reports UNAVAILABLE, never fake).
        let office = std::sync::Arc::new(openrt_office::office_ctx(
            fabric.artifacts.clone(),
            config.fabric.office.soffice.as_deref(),
        ));
        for tool in openrt_office::office_tools(office.clone()) {
            let _ = registry.register(tool);
        }
        // Browser tools (`auto` resolves to the real backend or UNAVAILABLE —
        // never silently falls back to the mock).
        let browser =
            std::sync::Arc::new(openrt_browser::BrowserManager::new(&config.fabric.browser));
        let browser_support = browser.support();
        let browser_ctx = std::sync::Arc::new(openrt_browser::BrowserToolCtx {
            manager: browser.clone(),
            artifacts: fabric.artifacts.clone(),
            modes: egress_modes,
            netpol: netpol.clone(),
        });
        for tool in openrt_browser::browser_tools(browser_ctx.clone()) {
            let _ = registry.register(tool);
        }
        // Phase 11 M11.2: the durable `mcp.servers` registry is the ONE view
        // of every MCP server — operator config, Active-extension
        // declarations, and CLI registrations all sync into it (config wins
        // name conflicts; extensions are always seeded Untrusted and never
        // self-elevate), and the registry's fail-closed spawn gate decides
        // who connects: enabled + connectable trust (`untrusted`/`trusted`)
        // + not flagged by a scan. Trust never bypasses policy — imported
        // tools still require `mcp.invoke.all` + `mcp.<server>.invoke`.
        let mcp_config =
            crate::phase11::sync_mcp_registry(&*store, &config.fabric.mcp, &ext_assets).await?;
        // MCP: fail-closed per server at wiring (initialize + tools/list with
        // bounded timeouts; a failing server imports nothing and records why).
        let mcp = openrt_mcp::McpManager::connect_all(&mcp_config);
        for tool in mcp.imported_tools() {
            let _ = registry.register(tool);
        }
        for tool in openrt_mcp::management_tools(mcp.clone()) {
            let _ = registry.register(tool);
        }
        // Application adapters + artifact registry (describe/verify only —
        // launch stays with the frozen `computer.app`).
        {
            use openrt_fabric::SupportState;
            let env = host.as_str();
            fabric.register_adapter(
                openrt_fabric::ApplicationAdapter::new(
                    "firefox",
                    "Mozilla Firefox",
                    "firefox-bidi",
                    "BiDi context state + screenshot",
                )
                .with_platform(env, browser_support)
                .powers(
                    &[
                        "browser.navigate",
                        "browser.find",
                        "browser.click",
                        "browser.type",
                        "browser.select",
                        "browser.submit",
                        "browser.screenshot",
                        "browser.download",
                        "browser.upload",
                        "browser.status",
                        "browser.close",
                    ],
                    &[
                        "browser.navigate.all",
                        "browser.find.all",
                        "browser.click.all",
                        "browser.type.all",
                        "browser.select.all",
                        "browser.submit.all",
                        "browser.screenshot.all",
                        "browser.download.all",
                        "browser.upload.all",
                        "browser.status.all",
                        "browser.close.all",
                    ],
                ),
            );
            fabric.register_adapter(
                openrt_fabric::ApplicationAdapter::new(
                    "libreoffice",
                    "LibreOffice (soffice)",
                    "soffice",
                    "re-read / PDF conversion output digest",
                )
                .with_platform(
                    env,
                    if office.soffice.is_some() {
                        SupportState::Green
                    } else {
                        SupportState::Unavailable
                    },
                )
                .powers(
                    &[
                        "excel.create",
                        "excel.write",
                        "excel.read",
                        "ppt.create",
                        "ppt.render",
                        "ppt.export_pdf",
                        "docx.create",
                        "docx.read",
                        "docx.export-pdf",
                    ],
                    &[
                        "artifact.excel.write",
                        "artifact.excel.read",
                        "artifact.ppt.write",
                        "artifact.ppt.read",
                        "artifact.docx.write",
                        "artifact.docx.read",
                    ],
                ),
            );
            fabric.register_adapter(
                openrt_fabric::ApplicationAdapter::new(
                    "sqlite",
                    "SQLite (rusqlite)",
                    "rusqlite",
                    "PRAGMA integrity_check / re-read",
                )
                .with_platform(env, SupportState::Green)
                .powers(
                    &[
                        "database.connect",
                        "database.list_tables",
                        "database.describe_table",
                        "database.query",
                        "database.insert",
                        "database.update",
                        "database.delete",
                        "database.begin",
                        "database.commit",
                        "database.rollback",
                        "database.transaction",
                    ],
                    &[
                        "database.read.all",
                        "database.query.all",
                        "database.write.all",
                        "database.transaction.all",
                    ],
                ),
            );
        }
        for tool in openrt_fabric::application_tools(
            fabric.applications(),
            computer_control.clone(),
            host_apps,
        ) {
            let _ = registry.register(tool);
        }
        let _ = registry.register(openrt_fabric::artifact_list_tool(fabric.artifacts.clone()));
        // One metadata catalog covering 7A + 7B (`tools.list` reads it).
        for meta in openrt_fabric::computer_metas(&host) {
            fabric.catalog.register(meta);
        }
        for meta in openrt_fabric::fabric_metas(&host) {
            fabric.catalog.register(meta);
        }
        for meta in openrt_database::database_metas(&host) {
            fabric.catalog.register(meta);
        }
        for meta in openrt_office::office_metas(&host) {
            fabric.catalog.register(meta);
        }
        for meta in openrt_browser::browser_metas(&host, browser_support) {
            fabric.catalog.register(meta);
        }
        for meta in mcp.metas(&host) {
            fabric.catalog.register(meta);
        }
        // Phase 12 (M12.1–M12.20): industrial / trading / learning tools +
        // their fabric metadata — same registry, same catalog, registered
        // BEFORE the `tools.list` snapshot and the gate's clone.
        {
            let sovereign = modes.current().await?.mode == openrt_modes::ExecutionMode::Sovereign;
            let operators = crate::phase5::asset_operators();
            for tool in crate::phase12::phase12_tools(store.clone(), sovereign, operators) {
                let _ = registry.register(tool);
            }
            for meta in crate::phase12::phase12_metas() {
                fabric.catalog.register(meta);
            }
        }
        // Phase 11 M11.2: skill-backed tool bindings of every Active
        // extension — registered into the SAME registry BEFORE the
        // `tools.list` snapshot and the gate's clone, so discovery, the
        // provider tool list and execution see one identical entry. A name
        // conflict (builtin or earlier registration) fails the build — never
        // a silent skip.
        let mut ext_bindings = Vec::new();
        for (_ext_id, _ext_ver, ext_tool) in ext_assets.active_extension_tools() {
            let binding = std::sync::Arc::new(crate::exttool::ExtensionToolBinding::new(&ext_tool));
            registry.register(binding.clone()).map_err(|e| {
                Error::Extension(format!(
                    "tool binding `{}` (skill `{}`): {e}",
                    ext_tool.name, ext_tool.skill
                ))
            })?;
            ext_bindings.push(binding);
        }
        // `tools.list` registers LAST, then snapshots the completed registry
        // (so the discovery snapshot includes `tools.list` itself).
        let tools_list =
            std::sync::Arc::new(openrt_fabric::ToolsListTool::new(fabric.catalog.clone()));
        registry
            .register(tools_list.clone())
            .expect("tools.list is a unique name — registration cannot fail");
        tools_list.set_registry(registry.clone());
        // Tools advertised to the model are policy-filtered: anything the
        // policy DENIES outright is withheld (deny-only filter — Ask/Allow
        // stay visible; the gate still adjudicates every call at execution).
        let tools = advertised_tools(&registry, &policy);
        // ADR-002: the chained audit log is the one created above (shared
        // with the sandbox authority); the gate records permission decisions
        // through the same instance.
        // M10.7: ONE hook implementation shared by the gate (`ToolCall`) and
        // every lifecycle fire point, plus the deny-only asset-rule layer the
        // gate consults before policy.
        let hook_layer = std::sync::Arc::new(crate::hooks::HookLayer::new(
            store.clone(),
            permission_policy.clone(),
        ));
        let rule_layer = std::sync::Arc::new(crate::gate::AssetRuleLayer::default());
        // M10.9: AgentShield at the enforcement edge — the one evaluation
        // layer; findings land in this store's shield namespace.
        let shield_layer = std::sync::Arc::new(crate::shield::ShieldLayer::new(store.clone()));
        let executor: Arc<dyn ToolExecutor> = Arc::new(
            GateExecutor::with_kill(
                registry.clone(),
                policy,
                resources,
                bus.clone(),
                self.permission_resolver,
                Some(kill_switch.clone()),
            )
            .with_audit(Some(audit.clone()))
            // Phase 8: gate-time sandbox admission for tools with a declared
            // requirement (deny-by-default before execution).
            .with_sandbox(Some(sandbox.clone()))
            .with_hooks(Some(hook_layer.clone()))
            .with_asset_rules(Some(rule_layer.clone()))
            .with_shield(Some(shield_layer.clone())),
        );

        // Agent.
        let limits = openrt_agents::AgentTurnLimits {
            max_turns: config.runtime.max_turns,
            max_tool_calls: config.runtime.max_tool_calls,
            // Interactive task path: one bounded retry on transient provider
            // errors; permanent parse/config errors fail fast. Empty responses
            // use a small bounded retry for provider/tool-call parse loss.
            provider_retries: 1,
            // Part C surfaces: operator-set streaming idle budget and context
            // guard ratio must reach the agent, or the config knobs silently
            // do nothing.
            idle_timeout_ms: config.runtime.agent_idle_timeout_ms,
            context_guard_ratio: config.runtime.agent_context_guard_ratio,
            ..openrt_agents::AgentTurnLimits::default()
        };
        let agent: Arc<dyn openrt_agents::Agent> = Arc::new(
            MinimalAgent::new(
                config.runtime.default_agent.clone(),
                provider.clone(),
                executor.clone(),
                limits,
                tools,
            )
            .with_system_prompt(environment_system_prompt(&config))
            .with_parameters(generation_parameters(&config.runtime.generation)),
        );

        // Phase 9 M9.1: content-addressed document corpus over the same store.
        let corpus = openrt_ingest::corpus::DocCorpus::new(
            store.clone(),
            openrt_ingest::corpus::IngestLimits::default(),
        );
        let runtime = Arc::new(Runtime {
            config,
            provider,
            agent,
            executor,
            registry,
            store,
            bus,
            orchestrator: AgentId::new(),
            event_log_health,
            dispatcher,
            session_locks: crate::locks::SessionLocks::new(),
            kill_switch,
            screen,
            computer: computer_control,
            computer_engine: computer_use,
            resource_gate,
            gpu_observer,
            audit,
            sandbox,
            modes,
            model_registry,
            router,
            fabric,
            database,
            browser,
            mcp,
            corpus,
            engine_cancel: openrt_core::CancellationToken::new(),
            workflow_pause: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            permission_policy,
            metrics: tokio::sync::Mutex::new(openrt_telemetry::MetricsRegistry::new()),
            applied_log: Arc::new(tokio::sync::Mutex::new(std::collections::BTreeMap::new())),
            working: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            active_tasks: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            rule_layer,
            hook_layer,
            shield_layer,
        });
        // Phase 11: wire the extension tool bindings to the built runtime.
        // `Weak` only — a binding never keeps the runtime alive, and an
        // unwired or torn-down binding fails closed at invocation.
        for binding in &ext_bindings {
            binding.bind(Arc::downgrade(&runtime));
        }
        // Phase 10 M10.4: crash recovery at boot — scan for incomplete
        // runs/tasks/stale sandboxes, reconcile them (never claiming
        // unfinished work successful), and record the recovery report
        // BEFORE the runtime serves anything. Fail-closed: a scan error
        // fails the build.
        runtime.boot_recover().await?;
        // M10.7: seed the gate's deny-only ECC layer from the durable store
        // so rules are in force from the first tool call (fail-closed).
        let assets = runtime.load_assets().await?;
        runtime.rule_layer.refresh_from(&assets);
        Ok(runtime)
    }
}

impl Runtime {
    /// Run the configured VRAM observer once (M9). Yields the honest empty
    /// observation when no vendor probe is wired, so callers can distinguish
    /// "nothing measured" from "measured, but no devices reported".
    pub async fn observe_gpu(&self) -> openrt_resources::GpuObservation {
        let observer = self.gpu_observer.clone();
        tokio::task::spawn_blocking(move || observer.observe())
            .await
            .unwrap_or_else(|_| openrt_resources::GpuObservation::empty())
    }

    /// Lightweight clone of the pipeline pieces for a background task.
    pub(crate) fn snapshot_pipeline(&self) -> crate::run::Pipeline {
        crate::run::Pipeline {
            provider: self.provider.clone(),
            agent: self.agent.clone(),
            executor: self.executor.clone(),
            store: self.store.clone(),
            bus: self.bus.clone(),
            agent_id: AgentId::new(),
        }
    }
}

/// Resolve the default provider from config.
pub fn build_default_provider(
    config: &openrt_config::Config,
) -> Result<Arc<dyn openrt_models::ModelProvider>, Error> {
    let pc = config
        .providers
        .iter()
        .find(|p| p.id == config.runtime.default_provider)
        .ok_or_else(|| {
            Error::ProviderUnavailable(
                config.runtime.default_provider.clone(),
                "not found in config.providers".into(),
            )
        })?;
    match pc.kind {
        openrt_config::ProviderKind::Mock => {
            Ok(Arc::new(MockProvider::tool_sequence(pc.id.clone())))
        }
        openrt_config::ProviderKind::Ollama => {
            #[cfg(feature = "ollama")]
            {
                let base = pc.base_url.clone().unwrap_or_default();
                let mut p = openrt_models::ollama::OllamaProvider::new(
                    pc.id.clone(),
                    base,
                    pc.default_model.clone(),
                )
                .map_err(Error::Model)?;
                p = p.with_timeout(std::time::Duration::from_secs(
                    config.runtime.model_timeout_secs,
                ));
                Ok(Arc::new(p))
            }
            #[cfg(not(feature = "ollama"))]
            {
                let _ = pc;
                Err(Error::OllamaFeatureDisabled)
            }
        }
        openrt_config::ProviderKind::Llamacpp => {
            #[cfg(feature = "llamacpp")]
            {
                let base = pc.base_url.clone().unwrap_or_default();
                Ok(Arc::new(openrt_provider_llamacpp::LlamacppProvider::new(
                    pc.id.clone(),
                    base,
                )))
            }
            #[cfg(not(feature = "llamacpp"))]
            {
                let _ = pc;
                Err(Error::Model(openrt_models::ModelError::NotConfigured(
                    "Llamacpp provider requires building with the `llamacpp` feature".into(),
                )))
            }
        }
        openrt_config::ProviderKind::Vllm => {
            #[cfg(feature = "vllm")]
            {
                let base = pc.base_url.clone().unwrap_or_default();
                Ok(Arc::new(openrt_provider_vllm::VllmProvider::new(
                    pc.id.clone(),
                    base,
                )))
            }
            #[cfg(not(feature = "vllm"))]
            {
                let _ = pc;
                Err(Error::Model(openrt_models::ModelError::NotConfigured(
                    "Vllm provider requires building with the `vllm` feature".into(),
                )))
            }
        }
    }
}

/// Fallback store dir used by CLI/doctor display.
pub fn default_store_location() -> PathBuf {
    openrt_persistence::default_store_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_core::TaskStatus;
    use openrt_persistence::MemoryStore;

    async fn mock_runtime() -> Arc<Runtime> {
        RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(Arc::new(MemoryStore::new()))
            .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
            .bus_capacity(16)
            .build()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn session_and_task_round_trip() {
        let rt = mock_runtime().await;
        assert_eq!(rt.config.runtime.default_provider, "ollama"); // default config, overridden provider
        let session = rt.create_session("hello").await.unwrap();
        assert_eq!(session.title, "hello");

        let sessions = rt.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 1);

        let task = rt.run_task(session.id, "echo hello rt").await.unwrap();
        assert_eq!(task.status, TaskStatus::Completed);
        assert!(task.result.as_deref().unwrap_or_default().contains("hello"));

        let tasks = rt.store.list_tasks(Some(session.id)).await.unwrap();
        assert_eq!(tasks.len(), 1);
    }

    /// Deny-only advertisement filter: policy-denied tools never reach the
    /// model; Ask and Allow stay visible (the gate re-checks at execution).
    #[test]
    fn advertised_tools_filters_policy_denied() {
        let registry = openrt_tools::ToolRegistry::builtins();
        let names = |tools: &[serde_json::Value]| -> Vec<String> {
            tools
                .iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        };

        // Default config: echo/system.info/filesystem.read allowed, the
        // rest (terminal.run, writes, computer.*) withheld.
        let default = crate::gate::policy_from_config(&openrt_config::PermissionsConfig::default());
        let n = names(&advertised_tools(&registry, &default));
        assert!(n.contains(&"echo".to_string()), "{n:?}");
        assert!(n.contains(&"system.info".to_string()), "{n:?}");
        assert!(n.contains(&"filesystem.read".to_string()), "{n:?}");
        // All builtins are allow-listed by default: nothing withheld.
        assert_eq!(n.len(), registry.provider_tools().len(), "{n:?}");

        // Explicit deny wins: even echo disappears when the only rule denies it.
        let deny = openrt_config::PermissionsConfig {
            rules: vec![openrt_config::RuleConfig {
                effect: openrt_config::RuleEffect::Deny,
                capability: "echo".into(),
                resource: None,
            }],
        };
        let n = names(&advertised_tools(
            &registry,
            &crate::gate::policy_from_config(&deny),
        ));
        assert!(!n.contains(&"echo".to_string()), "{n:?}");

        // Ask stays visible: the model may attempt it and the operator
        // decides at the gate (both gate checks — tool id and declared
        // capability — must be Ask/Allow, mirroring the two-step evaluation).
        let ask = openrt_config::PermissionsConfig {
            rules: vec![openrt_config::RuleConfig {
                effect: openrt_config::RuleEffect::Ask,
                capability: "echo".into(),
                resource: None,
            }],
        };
        let n = names(&advertised_tools(
            &registry,
            &crate::gate::policy_from_config(&ask),
        ));
        assert!(n.contains(&"echo".to_string()), "{n:?}");
    }

    /// The environment prompt names the configured workspace root (so the
    /// model explores instead of guessing paths) and is honest when no
    /// workspace is configured.
    #[test]
    fn environment_system_prompt_reports_workspace_root() {
        let mut config = openrt_config::Config::default();
        config.runtime.workspace_dir = Some(std::path::PathBuf::from("/tmp/some-workspace"));
        let prompt = environment_system_prompt(&config);
        assert!(prompt.contains("/tmp/some-workspace"), "{prompt}");
        assert!(prompt.contains("filesystem.list"), "{prompt}");

        config.runtime.workspace_dir = None;
        let prompt = environment_system_prompt(&config);
        assert!(prompt.contains("No workspace root"), "{prompt}");
    }

    /// The agent system prompt carries the semantic intent rules that keep
    /// "open Firefox" on `computer.app` and URL navigation on
    /// `browser.navigate` — guidance only: the model still picks, the gate
    /// still decides.
    #[test]
    fn environment_system_prompt_routes_tool_intent() {
        let config = openrt_config::Config::default();
        let prompt = environment_system_prompt(&config);
        assert!(prompt.contains("APPLICATIONS"), "{prompt}");
        assert!(prompt.contains("BROWSER"), "{prompt}");
        assert!(prompt.contains("FILESYSTEM"), "{prompt}");
        assert!(prompt.contains("TERMINAL/COMPUTE"), "{prompt}");
        assert!(prompt.contains("OBSERVATION"), "{prompt}");
        assert!(prompt.contains("`computer.app` opens/launches"), "{prompt}");
        assert!(
            prompt.contains("\"Open Firefox.\" -> computer.app"),
            "{prompt}"
        );
        assert!(prompt.contains("never computer.window"), "{prompt}");
        assert!(
            prompt.contains("`browser.navigate` opens a URL"),
            "{prompt}"
        );
        assert!(prompt.contains("domain-like strings"), "{prompt}");
        assert!(prompt.contains("policy gate"), "{prompt}");
        assert!(prompt.contains("You choose the tool"), "{prompt}");
    }

    /// Length budget for the agent system prompt: local models (llama3.1:8b)
    /// measurably stop emitting well-formed tool calls once the system +
    /// tools payload passes ~19.5 KB (malformed function JSON leaked as plain
    /// text, wrong tool chosen, planning prose instead of calls). Keep the
    /// prompt well under that cliff; the tools side is bounded by its own
    /// description-budget test in the integration suite.
    #[test]
    fn environment_system_prompt_stays_within_local_model_budget() {
        let mut config = openrt_config::Config::default();
        config.runtime.workspace_dir = Some(std::path::PathBuf::from("/tmp/some-workspace"));
        let prompt = environment_system_prompt(&config);
        assert!(
            prompt.len() <= 1650,
            "system prompt too long for local-model tool use: {} bytes",
            prompt.len()
        );
    }

    #[tokio::test]
    async fn background_task_cancels() {
        let rt = mock_runtime().await;
        let session = rt.create_session("cancel test").await.unwrap();
        let handle = rt.spawn_task(session.id, "hang").await.unwrap();
        let id = handle.task_id();
        handle.cancel();
        let task = handle.execute().await.unwrap();
        assert_eq!(task.id, id);
        assert_eq!(task.status, TaskStatus::Cancelled);
    }

    /// Wait for the async bus dispatcher to drain into the history ledger
    /// (HistorySink runs on the dispatcher task, not the emit path).
    async fn wait_for_history(rt: &Runtime, min: u64) {
        for _ in 0..200 {
            let n = openrt_timeseries::tail_seq(&*rt.store).await.unwrap_or(0);
            if n >= min {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let n = openrt_timeseries::tail_seq(&*rt.store).await.unwrap_or(0);
        panic!("history ledger did not reach {min} rows (has {n})");
    }

    /// M5.2 production path: a real session+task run is recorded on the
    /// ledger; the chain verifies; replay at seq=1 is a stable prefix; fork
    /// leaves the parent intact.
    #[tokio::test]
    async fn history_ledger_records_real_runs_and_supports_replay_fork() {
        let rt = mock_runtime().await;
        let session = rt.create_session("hist").await.unwrap();
        let task = rt.run_task(session.id, "echo history").await.unwrap();
        assert_eq!(task.status, TaskStatus::Completed);

        // RuntimeStarted + SessionCreated + task events at minimum.
        wait_for_history(&rt, 3).await;
        // Drain every queued event (flush barrier) so terminal task rows are
        // durable — not just the first few the dispatcher happened to reach.
        rt.shutdown().await;
        let tail = rt.history_tail().await.unwrap();
        assert!(tail >= 3, "tail={tail}");
        let verified = rt.history_verify().await.unwrap();
        assert_eq!(verified, tail);

        // TaskCompleted must land on the ledger (M5.2 production path) —
        // a dropped/undrained bus used to cancel this write on process exit.
        let rows = rt.history_replay(tail).await.unwrap();
        assert!(
            rows.iter().any(|r| r.event_kind == "TaskCompleted"),
            "TaskCompleted must be durable on the history ledger; kinds={:?}",
            rows.iter()
                .map(|r| r.event_kind.clone())
                .collect::<Vec<_>>()
        );
        assert!(
            rows.iter().any(|r| r.event_kind == "SessionCreated"),
            "SessionCreated must be durable on the history ledger"
        );

        // Time machine: prefix at seq=1 is a stable, byte-identical view and
        // does not corrupt later history.
        let as_of_1 = rt.history_replay(1).await.unwrap();
        assert_eq!(as_of_1.len(), 1);
        let as_of_1_again = rt.history_replay(1).await.unwrap();
        assert_eq!(
            serde_json::to_vec(&as_of_1).unwrap(),
            serde_json::to_vec(&as_of_1_again).unwrap()
        );
        // Later history still present and still valid after the read-only view.
        let full = rt.history_replay(tail).await.unwrap();
        assert_eq!(full.len() as u64, tail);
        assert_eq!(rt.history_verify().await.unwrap(), tail);

        // At least one row is correlated to our real session id.
        assert!(
            full.iter().any(|r| r.session_id == session.id.to_string()),
            "session-correlated ledger row expected"
        );

        // Fork from seq.1: parent unchanged, fork chain valid.
        rt.history_fork("branch.a", "seq.000000000001")
            .await
            .unwrap();
        rt.history_fork_commit("branch.a", "node-fork", "step", "ForkStep", "divergent")
            .await
            .unwrap();
        assert_eq!(rt.history_fork_verify("branch.a").await.unwrap(), 1);
        assert_eq!(rt.history_verify().await.unwrap(), tail);
        assert_eq!(rt.history_tail().await.unwrap(), tail);
    }

    /// M5.3 production path: a real Runtime remember → Phase-4 retrieval
    /// recall with verified citations; duplicate fails closed; unregistered
    /// origin fails closed; no-evidence query answers honest empty.
    #[tokio::test]
    async fn memory_remember_recall_is_real_and_fail_closed() {
        let rt = mock_runtime().await;
        rt.memory_remember_origin("run.m53", "canonical-origin-m53")
            .await
            .unwrap();

        let item = rt
            .memory_remember(
                "fact.deploy",
                "run.m53",
                "src.pipeline",
                "ops",
                "deploy.cmd",
                "cargo test --offline",
                "2026-09-23T00:00:00Z",
                false,
            )
            .await
            .unwrap();
        item.reopen().unwrap();
        item.citation.validate().unwrap();

        // Query path rides openrt_knowledge::retrieve and surfaces the fact.
        let set = rt.memory_recall("deploy.cmd", 5, 0.0).await.unwrap();
        assert!(!set.items.is_empty(), "Phase-4 retrieval must hit");
        assert!(set.with_citations());
        for c in set.citations() {
            c.validate().unwrap();
        }

        // Duplicate id → fail closed unless explicitly overridden.
        let dup = rt
            .memory_remember(
                "fact.deploy",
                "run.m53",
                "src.pipeline",
                "ops",
                "deploy.cmd",
                "other",
                "2026-09-23T00:00:01Z",
                false,
            )
            .await;
        assert!(dup.is_err(), "duplicate id must fail closed");
        rt.memory_remember(
            "fact.deploy",
            "run.m53",
            "src.pipeline",
            "ops",
            "deploy.cmd",
            "cargo test",
            "2026-09-23T00:00:02Z",
            true,
        )
        .await
        .unwrap();

        // Unregistered origin → fail closed (citation never invented).
        let ghost = rt
            .memory_remember(
                "fact.ghost",
                "run.ghost",
                "src",
                "ops",
                "k",
                "v",
                "2026-09-23T00:00:00Z",
                false,
            )
            .await;
        assert!(ghost.is_err(), "unregistered origin must fail closed");

        // No-evidence query → honest empty (never fabricated citations).
        // Threshold 1.0 requires a perfect score; unrelated queries score
        // far below that under DeterministicHashEmbedder cosine, so this is
        // a genuine "no verified evidence" answer.
        let none = rt
            .memory_recall("zzz-no-such-fact-zzz", 5, 1.0)
            .await
            .unwrap();
        assert!(none.items.is_empty(), "empty recall must stay empty");
        assert!(none.with_citations());
    }

    /// M5.3 episodic production path through the Runtime: roundtrip +
    /// fail-closed on tamper.
    #[tokio::test]
    async fn memory_episode_roundtrip_via_runtime() {
        let rt = mock_runtime().await;
        let rec = rt
            .memory_remember_episode(
                "run.e1",
                "sess.e1",
                "train",
                "2026-09-23T01:00:00Z",
                "trained ok",
                "run.e1",
                "src.hist",
                false,
            )
            .await
            .unwrap();
        let back = rt.memory_recall_episode(&rec.digest).await.unwrap();
        assert_eq!(back, rec);

        // Tamper the stored payload → recall fails closed (never repaired).
        let mut v = rt
            .store
            .get_record(openrt_memory::EPISODIC_NS, &rec.digest)
            .await
            .unwrap()
            .unwrap();
        v["payload"] = serde_json::json!("evil");
        rt.store
            .put_record(openrt_memory::EPISODIC_NS, &rec.digest, &v)
            .await
            .unwrap();
        assert!(rt.memory_recall_episode(&rec.digest).await.is_err());
    }
}

/// Tool definitions advertised to the model, minus everything the permission
/// policy DENIES outright (deny-only filter: `Ask` and `Allow` stay visible
/// so an operator can approve at the gate; the gate re-evaluates every call
/// regardless — this list only keeps the model from wasting turns on tools it
/// can never invoke). Probing uses empty arguments, so a `Deny` rule scoped
/// to non-empty resources may not match here; the gate enforces it at call
/// time.
pub(crate) fn advertised_tools(
    registry: &openrt_tools::ToolRegistry,
    policy: &openrt_permissions::Policy,
) -> Vec<serde_json::Value> {
    use openrt_permissions::{Decision, PermissionContext, PermissionRequest, PolicyEvaluator};
    let evaluator = PolicyEvaluator::new(policy);
    let pctx = PermissionContext::new();
    let empty = serde_json::json!({});
    registry
        .provider_tools()
        .into_iter()
        .filter(|t| {
            let Some(name) = t.get("name").and_then(|n| n.as_str()) else {
                return false;
            };
            let Some(tool) = registry.get(name) else {
                return false;
            };
            // Same two checks the gate runs (tool id, then every declared
            // capability); a Deny on either withholds the tool.
            if evaluator.evaluate(&PermissionRequest::tool(name, &empty, &pctx)) == Decision::Deny {
                return false;
            }
            tool.spec().required_caps.iter().all(|cap| {
                let req = PermissionRequest::capability(name, cap.clone(), None);
                evaluator.evaluate(&req) != Decision::Deny
            })
        })
        .collect()
}

/// Environment context attached to every model request by the agent
/// (workspace root + tool-selection guidance + tool-result conventions).
/// Plain string building: no policy, tool or gate involvement. Sent per
/// request; never persisted into the transcript.
///
/// Keep this prompt SHORT: local models (llama3.1:8b via Ollama) measurably
/// degrade — malformed tool-call JSON emitted as plain text, wrong tool
/// chosen — once system + tools payload grows past roughly 19.5 KB (the
/// tool-selection regression tests pin the length budget). Every rule here
/// is semantic guidance for the model's own choice; nothing in this prompt
/// executes or bypasses the policy gate.
pub(crate) fn environment_system_prompt(config: &openrt_config::Config) -> String {
    let scope = match &config.runtime.workspace_dir {
        Some(root) => {
            let r = root.display();
            format!(
                "- Workspace root: `{r}`; filesystem tools take absolute paths inside it only; \
                 call `filesystem.list` first, do not guess paths."
            )
        }
        None => "- No workspace root is configured, so filesystem tools refuse every path; \
                 say so instead of guessing paths."
            .to_string(),
    };
    format!(
        "You are an AI assistant running in the OpenRT agent harness on the user's machine.\n\
         {scope}\n\
         - Tool selection by group (pick inside the matching group):\n\
         \x20  APPLICATIONS: `computer.app` opens/launches an application by name (\"Open \
         Firefox.\" -> computer.app, even if it may already be running; never \
         computer.window); `computer.window` only focuses/closes existing windows.\n\
         \x20  BROWSER: `browser.navigate` opens a URL/website (domain-like strings, e.g. \
         example.com or localhost:8080, are URLs); `browser.click`/`browser.type`/\
         `browser.select`/`browser.submit`/`browser.find` interact with the page after \
         navigate (`browser.find` first to locate elements before clicking); \
         `browser.status`/`browser.screenshot`/`browser.close` manage the session.\n\
         \x20  FILESYSTEM: `filesystem.list`/`filesystem.read`/`filesystem.write`/\
         `filesystem.move`/`filesystem.delete`.\n\
         \x20  TERMINAL/COMPUTE: `terminal.run` (shell), `python.run` (Python).\n\
         \x20  OBSERVATION: `computer.screen`/`screen.capture`/`screen.ground` (\"Take a \
         screenshot.\" -> computer.screen), `computer.env`.\n\
         - Tool results are JSON: `{{\"ok\":true,...}}` on success or `{{\"ok\":false,...}}` \
         on failure. On `ok:false` call a different tool next - never repeat the same failing \
         call, never claim success.\n\
         - You choose the tool; every call still passes the policy gate - nothing runs \
         without it.\n\
         - Do the work now with tool calls; never reply with a list of planned calls.\n\
         - If a tool is denied or unavailable, say so plainly and continue with the tools \
         that are allowed."
    )
}

/// Map the config `generation` surface onto the agent's per-request
/// [`openrt_models::ModelParameters`]. Every field is optional so an unset
/// config leaves each provider's defaults untouched — this is an explicit
/// override surface, never a changed default.
pub(crate) fn generation_parameters(
    gen: &openrt_config::GenerationConfig,
) -> openrt_models::ModelParameters {
    openrt_models::ModelParameters {
        temperature: gen.temperature,
        top_p: gen.top_p,
        max_tokens: gen.max_tokens,
        stop: gen.stop.clone(),
        seed: gen.seed,
        unbounded_output: gen.unbounded_output,
        context_window: gen.context_window,
    }
}

#[cfg(test)]
mod resource_override_authority_tests {
    use super::*;
    use std::sync::Arc;

    /// A runtime whose gate is guaranteed to refuse, so every assertion is about
    /// the override path rather than about the host's actual resources.
    async fn runtime_with_limits(allow_override: bool) -> Arc<Runtime> {
        let mut config = openrt_config::Config::default();
        config.resources.allow_operator_override = Some(allow_override);
        // Force a denial regardless of the host we happen to run on.
        // 100% headroom required: a denial regardless of the real host.
        config.resources.min_memory_headroom_percent = Some(100);
        crate::RuntimeBuilder::new()
            .config(config)
            .store(Arc::new(openrt_persistence::MemoryStore::new()))
            .with_provider(Arc::new(openrt_models::mock::MockProvider::tool_sequence(
                "mock",
            )))
            .build()
            .await
            .unwrap_or_else(|e| panic!("runtime: {e:?}"))
    }

    /// The invariant this whole module exists for: an ordinary refusal stays an
    /// ordinary refusal. No UI involvement, no override, no execution.
    #[tokio::test]
    async fn an_ordinary_refusal_stays_a_refusal() {
        let rt = runtime_with_limits(true).await;
        let admission = rt.preflight_task().await;
        assert!(
            !admission.allowed(),
            "a host under pressure must refuse without any override"
        );
        assert!(
            admission.denial.is_some(),
            "the refusal must carry a structured denial, not a bare sentence"
        );
        assert!(!admission.overridden);
    }

    /// Task-level preflight is still enforced. This is the path `run_task`
    /// takes, and it must refuse with no override armed.
    #[tokio::test]
    async fn task_level_preflight_is_still_enforced() {
        let rt = runtime_with_limits(true).await;
        assert!(!rt.preflight_task().await.allowed());
        // And again — enforcing it must not somehow consume an authority.
        assert!(!rt.preflight_task().await.allowed());
    }

    /// An operator override granted through the authority is auditable *before*
    /// it is armed.
    #[tokio::test]
    async fn an_authorized_override_is_audited_then_armed() {
        let rt = runtime_with_limits(true).await;
        let before = rt.audit.count().await.unwrap_or(0);

        rt.grant_resource_override("operator freed headroom", "alice")
            .await
            .expect("policy allows this override");

        let after = rt.audit.count().await.unwrap_or(0);
        assert!(
            after > before,
            "a granted override must leave an audit row, or it cannot be reviewed"
        );
        assert!(
            rt.resource_gate.override_pending(),
            "the audit must precede arming, so the grant is live afterwards"
        );
    }

    /// Policy that forbids overrides must refuse at the authority, and the
    /// caller must be told the *original* denial rather than losing it.
    #[tokio::test]
    async fn an_unauthorized_override_remains_a_refusal() {
        let rt = runtime_with_limits(false).await;
        let err = rt
            .grant_resource_override("please", "mallory")
            .await
            .expect_err("policy forbids overrides");
        assert!(
            !rt.resource_gate.override_pending(),
            "a refused override must arm nothing"
        );
        // The resource denial distinction is preserved: the caller still learns
        // it is a resource refusal, not an unrelated failure.
        assert!(
            matches!(err, Error::ResourceBlocked(_)),
            "expected the structured resource denial, got {err:?}"
        );
        assert!(!rt.preflight_task().await.allowed());
    }

    /// A forged operator string must not be enough. The authority is the runtime
    /// method plus its audit row, not the caller's self-description.
    #[tokio::test]
    async fn a_self_asserted_operator_string_grants_nothing_by_itself() {
        let rt = runtime_with_limits(false).await;
        // The caller claims to be the operator. Policy still wins.
        assert!(rt
            .grant_resource_override("reason", "i-am-the-operator")
            .await
            .is_err());
        assert!(!rt.resource_gate.override_pending());
    }

    /// Repeated grants do not accumulate into standing policy. Each is audited,
    /// and each arms only one admission.
    #[tokio::test]
    async fn repeated_grants_do_not_become_standing_policy() {
        let rt = runtime_with_limits(true).await;
        for i in 0..3 {
            rt.grant_resource_override(&format!("attempt {i}"), "alice")
                .await
                .expect("policy allows");
            let admission = rt.preflight_task().await;
            assert!(
                admission.allowed() && admission.overridden,
                "grant {i} should admit exactly one admission"
            );
        }
        // The fourth admission has nothing left to spend.
        let fourth = rt.preflight_task().await;
        assert!(
            !fourth.allowed(),
            "three one-shot grants must not authorise a fourth admission"
        );
    }

    /// A consumed task-level override must be attributable in the chain. Before
    /// this, the task path wrote nothing at all, so an override that let work
    /// proceed left no evidence whatsoever.
    #[tokio::test]
    async fn a_consumed_task_override_records_its_operator_and_reason() {
        let rt = runtime_with_limits(true).await;
        rt.grant_resource_override("the operator read the dashboard", "alice")
            .await
            .expect("policy allows");

        let before = rt.audit.count().await.unwrap_or(0);
        let admission = rt.preflight_task().await;
        assert!(admission.overridden, "the grant should have been spent");
        assert_eq!(admission.override_operator.as_deref(), Some("alice"));
        assert_eq!(
            admission.override_reason.as_deref(),
            Some("the operator read the dashboard")
        );

        // The runtime's own audit path is exercised by run_task; call it here to
        // prove a failure to record is refused rather than ignored.
        rt.audit_task_override(&admission)
            .await
            .expect("the audit chain is available in this runtime");
        assert!(
            rt.audit.count().await.unwrap_or(0) > before,
            "the consumed override must appear in the chained log"
        );
    }

    /// An admission that was NOT overridden writes nothing. A plain refusal must
    /// not manufacture override evidence.
    #[tokio::test]
    async fn a_plain_refusal_writes_no_override_row() {
        let rt = runtime_with_limits(true).await;
        let admission = rt.preflight_task().await;
        assert!(!admission.overridden);
        let before = rt.audit.count().await.unwrap_or(0);
        rt.audit_task_override(&admission).await.expect("no-op");
        assert_eq!(
            rt.audit.count().await.unwrap_or(0),
            before,
            "a refusal is not an override"
        );
    }
}
