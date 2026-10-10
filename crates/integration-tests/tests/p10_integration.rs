//! Phase 10 — the eight mandated integration chains: history→skill
//! generation; skill→workflow execution; workflow→AgentShield;
//! AgentShield→sandbox; genome→fresh session; recovery→resume;
//! TUI/runtime shared state; WebUI/runtime shared state. Each chain
//! crosses subsystem boundaries through the production APIs over one
//! durable store — no chain-local shortcuts.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use openrt_audit::AuditQuery;
use openrt_checkpoint::{
    Checkpoint, CheckpointKind, CheckpointManager, CheckpointStatus, DependencyState, NodeEffect,
    NodeState, NodeStatus, RecoveryPolicy, RunRefs, CHECKPOINT_SCHEMA_VERSION,
};
use openrt_config::Config;
use openrt_core::{now, EventKind, Task, TaskStatus};
use openrt_models::error::ModelError;
use openrt_models::mock::MockProvider;
use openrt_models::provider::{ModelCapabilities, ModelProvider};
use openrt_models::types::{ModelInfo, ModelRequest, ModelResponse};
use openrt_orch::{SubAgentKind, Workflow, WorkflowStatus, WorkflowStep, RUNS_NS};
use openrt_persistence::{MemoryStore, PersistenceStore};
use openrt_runtime::{Runtime, RuntimeBuilder};
use openrt_selfimprove::{MetricDirection, ProposalKind, TargetKind};
use openrt_shield::FindingStatus;
use openrt_telemetry::ChannelSink;
use openrt_tools::{ToolError, ToolExecutionContext};
use openrt_webui::{CommandPalette, DispatchRequest, PaletteOutcome};
use openrt_workspace_assets::{
    ActivationState, AgentGenome, Instinct, InstinctId, OperatorGrant,
    Provenance as AssetProvenance, ReviewState, Skill, SkillId, SkillVersion, TRUST_VETTED,
};

async fn rt_toolseq() -> Arc<Runtime> {
    RuntimeBuilder::new()
        .config(Config::default())
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .bus_capacity(32)
        .build()
        .await
        .unwrap()
}

async fn rt_echo() -> Arc<Runtime> {
    RuntimeBuilder::new()
        .config(Config::default())
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .bus_capacity(32)
        .build()
        .await
        .unwrap()
}

fn operator(rt: &Runtime) -> String {
    rt.asset_operators()[0].clone()
}

fn prov() -> AssetProvenance {
    AssetProvenance::new("run.chain", "src.p10_integration")
}

fn tctx() -> ToolExecutionContext {
    ToolExecutionContext {
        session_id: None,
        task_id: None,
        channel: None,
    }
}

/// Poll until the async bus dispatcher has drained and the tail stops
/// moving — deterministic baseline for timeline assertions.
async fn wait_ledger_stable(rt: &Runtime) {
    let mut last = rt.history_tail().await.unwrap();
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let cur = rt.history_tail().await.unwrap();
        if cur == last && cur > 0 {
            return;
        }
        last = cur;
    }
    panic!("history ledger never settled (tail {last})");
}

async fn poll_evidence(rt: &Runtime, session: &str) -> Vec<(String, u64)> {
    for _ in 0..200 {
        let evidence = rt.history_tool_evidence(session).await.unwrap();
        if !evidence.is_empty() {
            return evidence;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("no tool evidence landed on the ledger for `{session}`");
}

fn manifest_skill(id: &str) -> Skill {
    let mut s = Skill::new(
        SkillId(id.to_string()),
        SkillVersion(1),
        "demo manifest skill",
        vec!["fs.read".to_string()],
        vec![],
        prov(),
    )
    .unwrap();
    s.goal = "say hello".to_string();
    s.tools = vec!["echo".to_string()];
    s.sandbox = Some("strict".to_string());
    s.trust = TRUST_VETTED.to_string();
    s
}

// ---- 1: history → skill generation ----------------------------------------

/// Real execution history materially derives a candidate (content, not
/// just identity) → provenance → validation → operator approval →
/// activation through the scan-gated production path → execution through
/// the real gate → recorded id + version + digest.
#[tokio::test]
async fn chain_history_to_skill_generation() {
    let rt = rt_toolseq().await;
    let op = operator(&rt);

    let session = rt.create_session("chain gen").await.unwrap();
    rt.run_task(session.id, "collect facts with echo")
        .await
        .unwrap();
    let sid = session.id.to_string();
    let evidence = poll_evidence(&rt, &sid).await;
    assert!(!evidence.is_empty());

    let cid = rt
        .skill_submit_from_history(
            "req.chain1",
            "collect facts with echo",
            vec!["fs.read".into()],
            &sid,
        )
        .await
        .unwrap();

    // Content from evidence: the observed tools become the manifest, the
    // provenance names the history source, the review starts Generated.
    {
        let mut assets = rt.load_assets().await.unwrap();
        let cand = assets.get_candidate(&cid).unwrap();
        assert!(matches!(cand.review, ReviewState::Generated));
        assert!(
            cand.skill.tools.iter().any(|t| t == "echo"),
            "derived tools: {:?}",
            cand.skill.tools
        );
        assert_eq!(cand.skill.goal, "collect facts with echo");
        assert_eq!(cand.provenance.source, "history.timeseries");

        // Approval before validation fails closed (store level).
        let grant = OperatorGrant::new(&op).unwrap();
        let err = assets.approve_candidate(&cid, Some(&grant)).unwrap_err();
        assert!(err.to_string().contains("not validated"), "{err}");
    }

    // Validation + operator approval through the production wrappers —
    // same fail-closed rule, error mapped at the boundary (review only;
    // the registry mutation stays behind the scan-gated activation).
    let err = rt.assets_approve_candidate(&cid, &op).await.unwrap_err();
    assert!(err.to_string().contains("not validated"), "{err}");
    rt.assets_validate_candidate(&cid).await.unwrap();
    rt.assets_approve_candidate(&cid, &op).await.unwrap();

    // Activation: operator path + AgentShield content scan first.
    let skill_id = rt.assets_activate_candidate(&cid, &op).await.unwrap();
    {
        let assets = rt.load_assets().await.unwrap();
        let skill = assets.get_skill(&skill_id).unwrap();
        assert_eq!(skill.activation, ActivationState::Active);
        assert!(matches!(
            assets.get_candidate(&cid).unwrap().review,
            ReviewState::Approved
        ));
    }

    // Execution through the real gate → recorded id + version + digest.
    let exec = rt
        .skill_execute(session.id, &skill_id.0, None)
        .await
        .unwrap();
    assert_eq!(exec.mode, "agent");
    assert_eq!(exec.id, skill_id.0);
    assert_eq!(exec.version, 1);
    let report = rt.skill_load(&exec.id, None).await.unwrap();
    assert!(report.passed, "checks: {:?}", report.checks);
    assert_eq!(report.activation, "active");
    assert!(!report.digest.is_empty());
    assert_eq!(report.version, exec.version);
}

// ---- 2: skill → workflow execution ----------------------------------------

/// Compile links a validated DAG back onto the exact skill version and
/// demotes the approval; only a fresh operator approval executes the
/// workflow through the real gate, persisting the run report durably.
#[tokio::test]
async fn chain_skill_to_workflow_execution() {
    use openrt_config::{PermissionsConfig, RuleConfig, RuleEffect};
    let mut cfg = Config::default();
    cfg.permissions = PermissionsConfig {
        rules: [
            "echo",
            "system.info",
            "system.info.all",
            "filesystem.read",
            "filesystem.read.all",
            "filesystem.write",
            "filesystem.write.all",
            "fs.read",
            "tool.run",
            "tool.other",
        ]
        .iter()
        .map(|c| RuleConfig {
            effect: RuleEffect::Allow,
            capability: c.to_string(),
            resource: None,
        })
        .collect(),
    };
    let rt = RuntimeBuilder::new()
        .config(cfg)
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .bus_capacity(32)
        .build()
        .await
        .unwrap();
    let op = operator(&rt);
    let mut skill = manifest_skill("skill.chainwf");
    skill.goal = "Read report.txt then write notes.md".to_string();
    rt.assets_install_skill(skill).await.unwrap();
    rt.assets_validate_skill("skill.chainwf").await.unwrap();
    rt.assets_activate_skill("skill.chainwf", &op)
        .await
        .unwrap();

    let wf = rt.skill_compile("skill.chainwf", None).await.unwrap();
    let rep = rt.skill_inspect("skill.chainwf", None).await.unwrap();
    assert_eq!(rep.activation, "validated");
    assert_eq!(rep.workflow.as_deref(), Some(wf.as_str()));
    assert!(rep.workflow_executable, "checks: {:?}", rep.checks);

    let session = rt.create_session("chain wf").await.unwrap();
    let err = rt
        .skill_execute(session.id, "skill.chainwf", None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("activate"), "{err}");

    rt.assets_activate_skill("skill.chainwf", &op)
        .await
        .unwrap();
    let exec = rt
        .skill_execute(session.id, "skill.chainwf", None)
        .await
        .unwrap();
    assert_eq!(exec.mode, "workflow");
    let wfr = exec.workflow_report.expect("workflow mode runs the DAG");
    assert_eq!(wfr.workflow_id, wf);

    // The run persisted to the shared ADR-006 runs namespace, and the
    // exact executed version still carries a digest under `active`.
    let run = rt
        .store
        .get_record(RUNS_NS, wf.as_str())
        .await
        .unwrap()
        .expect("workflow run report must be durable");
    assert!(!run.is_null());
    let report = rt.skill_load("skill.chainwf", None).await.unwrap();
    assert_eq!(report.activation, "active");
    assert!(!report.digest.is_empty());
}

// ---- 3: workflow → AgentShield --------------------------------------------

/// A tool call issued from INSIDE a workflow step goes through the same
/// gated executor: AgentShield denies the credential access before
/// execution, persists the finding, and audits the denial.
#[tokio::test]
async fn chain_workflow_to_agentshield() {
    let provider = MockProvider::tool_sequence("mock").with_tool_script(vec![(
        "filesystem.read",
        serde_json::json!({ "path": "/home/user/.ssh/id_rsa" }),
    )]);
    let rt = RuntimeBuilder::new()
        .config(Config::default())
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(provider))
        .bus_capacity(32)
        .build()
        .await
        .unwrap();
    let session = rt.create_session("chain shield").await.unwrap();
    let wf = Workflow::new("wf.chainshield").step(WorkflowStep::new(
        "plan",
        SubAgentKind::Planner,
        "layout the change",
    ));

    // The workflow's sub-agent runs through `role_agents` → the runtime's
    // GATED executor, so the shield layer sees the hostile call first.
    let _ = rt.run_workflow(session.id, wf).await;

    let denied = openrt_shield::store::list_findings(&*rt.store, Some(FindingStatus::Denied))
        .await
        .unwrap();
    let hit = denied
        .iter()
        .find(|f| f.category.as_str() == "credential_access")
        .expect("workflow tool call denied and recorded as a finding");
    assert_eq!(hit.severity.as_str(), "HIGH");
    assert_eq!(hit.source, "shield.credential_access");

    // Computer-scoped denial lands on the verifiable audit chain.
    let records = rt
        .audit_query(&AuditQuery::new().with_action_prefix("permission.decision"))
        .await
        .unwrap();
    let row = records
        .iter()
        .find(|r| r.entry.subject == "filesystem.read" && r.entry.result == "shield denied")
        .expect("workflow shield denial audited");
    assert_eq!(
        row.entry.ext.get("via").and_then(|v| v.as_str()),
        Some("agentshield")
    );
}

// ---- 4: AgentShield → sandbox ---------------------------------------------

/// `python.run` carries a declared Strict sandbox requirement, so the gate
/// evaluates the call with `sandboxed=true`: the critical command finding
/// is containment-downgraded (CRITICAL→HIGH, policy notes the downgrade)
/// and still fails closed at RequireApproval — audited, never executed.
#[tokio::test]
async fn chain_agentshield_to_sandbox_containment() {
    let rt = rt_echo().await;
    match rt
        .executor
        .execute(
            "python.run",
            serde_json::json!({ "command": "chmod -R 777 /tmp" }),
            &tctx(),
        )
        .await
        .unwrap_err()
    {
        ToolError::PermissionDenied { tool, reason } => {
            assert_eq!(tool, "python.run");
            assert!(reason.contains("AgentShield"), "{reason}");
        }
        other => panic!("expected shield denial for python.run, got {other:?}"),
    }

    let denied = openrt_shield::store::list_findings(&*rt.store, Some(FindingStatus::Denied))
        .await
        .unwrap();
    let hit = denied
        .iter()
        .find(|f| f.category.as_str() == "dangerous_command")
        .expect("sandboxed command denial recorded");
    // Downgraded severity PROVES the gate saw sandboxed=true (unsandboxed
    // this detector fires CRITICAL).
    assert_eq!(hit.severity.as_str(), "HIGH");
    assert!(
        hit.policy.contains("(sandbox containment)"),
        "policy: {}",
        hit.policy
    );

    let records = rt
        .audit_query(&AuditQuery::new().with_action_prefix("permission.decision"))
        .await
        .unwrap();
    let row = records
        .iter()
        .find(|r| r.entry.subject == "python.run" && r.entry.result == "shield denied")
        .expect("sandboxed shield denial audited");
    assert_eq!(
        row.entry.ext.get("via").and_then(|v| v.as_str()),
        Some("agentshield")
    );
}

// ---- 5: genome → fresh session --------------------------------------------

/// Applying a genome to a FRESH session records the exact id/version/digest,
/// activates its referenced instinct and skill through the approval-gated
/// (scan-first) apply path, and the run context carries the instinct.
#[tokio::test]
async fn chain_genome_to_fresh_session() {
    let rt = rt_toolseq().await;
    let op = operator(&rt);

    let mut inst = Instinct::new(
        InstinctId("instinct.chain5".into()),
        1,
        "prefer echo under uncertainty",
        90,
        prov(),
    )
    .unwrap();
    inst.trigger = "any task".to_string();
    inst.recommendation = "use the echo tool first".to_string();
    inst.confidence = 0.9;
    let mut sk = Skill::new(
        SkillId("skill.chain5".into()),
        SkillVersion(1),
        "chain five skill",
        vec!["fs.read".into()],
        vec![],
        prov(),
    )
    .unwrap();
    sk.goal = "say hello".to_string();
    sk.tools = vec!["echo".to_string()];
    sk.sandbox = Some("strict".to_string());
    {
        let mut assets = rt.load_assets().await.unwrap();
        assets.register_instinct(inst).unwrap();
        assets
            .validate_instinct(&InstinctId("instinct.chain5".into()))
            .unwrap();
        assets.register_skill(sk).unwrap();
        assets
            .validate_skill(&SkillId("skill.chain5".into()))
            .unwrap();
        assets
            .register_genome(
                AgentGenome::new(
                    openrt_workspace_assets::GenomeId("genome.chain5".into()),
                    1,
                    vec![(SkillId("skill.chain5".into()), SkillVersion(1))],
                    vec![("instinct.chain5".into(), 1)],
                    vec![],
                    vec![],
                    prov(),
                )
                .unwrap(),
            )
            .unwrap();
        rt.save_assets(&assets).await.unwrap();
    }

    let s5 = rt.create_session("chain5 fresh").await.unwrap();
    let reference = rt
        .session_apply_genome(s5.id, "genome.chain5", &op)
        .await
        .unwrap();
    assert_eq!(reference.id, "genome.chain5");
    assert_eq!(reference.version, 1);
    assert!(!reference.digest.is_empty());

    // References activated through the apply path (skill scan first).
    let assets = rt.load_assets().await.unwrap();
    assert_eq!(
        assets
            .get_skill(&SkillId("skill.chain5".into()))
            .unwrap()
            .activation,
        ActivationState::Active
    );
    assert_eq!(
        assets
            .get_instinct(&InstinctId("instinct.chain5".into()))
            .unwrap()
            .activation,
        ActivationState::Active
    );
    assert_eq!(
        assets.meta().active_genome.as_deref(),
        Some("genome.chain5")
    );
    drop(assets);

    // The genome's instinct shapes the fresh session's run context.
    let task = rt.run_task(s5.id, "plain base prompt").await.unwrap();
    assert!(
        task.prompt.contains("[instincts"),
        "prompt: {}",
        task.prompt
    );
    assert!(task.prompt.contains("use the echo tool first"));

    // Durable: the session keeps its own reference after restart-read.
    let reloaded = rt.store.load_session(s5.id).await.unwrap();
    assert_eq!(reloaded.genome, Some(reference));
}

// ---- 6: recovery → resume --------------------------------------------------

fn dep_workflow(id: &str) -> Workflow {
    Workflow::new(id)
        .step(WorkflowStep::new(
            "plan",
            SubAgentKind::Planner,
            "layout the change",
        ))
        .step(WorkflowStep::new("code", SubAgentKind::Coder, "implement it").dep("plan"))
}

struct CountingProvider {
    inner: MockProvider,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ModelProvider for CountingProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn info(&self) -> ModelInfo {
        self.inner.info()
    }
    async fn generate(&self, req: ModelRequest) -> Result<ModelResponse, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.generate(req).await
    }
    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }
}

/// Durable leftovers of a run that died mid-flight: the workflow task is
/// stuck `Running` and the checkpoint has `plan` completed, `code` pending.
async fn seed_crashed_run(
    store: &Arc<dyn PersistenceStore>,
    session_id: openrt_core::SessionId,
    workflow_id: &str,
) -> (Task, openrt_core::CheckpointId) {
    let mut task = Task::new(session_id, format!("workflow {workflow_id}"));
    task.transition(TaskStatus::Running).unwrap();
    store.save_task(&task).await.unwrap();

    let checkpoint = Checkpoint {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        checkpoint_id: openrt_core::CheckpointId::new(),
        session_id,
        task_id: Some(task.id),
        workflow_id: workflow_id.into(),
        created_at: now(),
        kind: CheckpointKind::Initial,
        status: CheckpointStatus::Running,
        node_states: vec![
            NodeStatus {
                step: "plan".into(),
                state: NodeState::Completed,
                attempts: 1,
                artifact_key: Some("output.plan".into()),
                error: None,
                failure_category: String::new(),
                effect: NodeEffect::Unknown,
            },
            NodeStatus {
                step: "code".into(),
                state: NodeState::Pending,
                attempts: 0,
                artifact_key: None,
                error: None,
                failure_category: String::new(),
                effect: NodeEffect::Unknown,
            },
        ],
        completed: vec!["plan".into()],
        pending: vec!["code".into()],
        failed: vec![],
        uncertain: vec![],
        current_node: None,
        dependency_state: DependencyState {
            ready: vec!["code".into()],
            deps_by_step: [
                ("plan".to_string(), vec![]),
                ("code".to_string(), vec!["plan".to_string()]),
            ]
            .into_iter()
            .collect(),
        },
        retry_counters: std::collections::HashMap::new(),
        blackboard: Default::default(),
        artifact_refs: vec![],
        execution: Default::default(),
        refs: RunRefs::default(),
        digest: None,
    };
    let manager = CheckpointManager::new(store.clone());
    let id = manager.create(&checkpoint).await.unwrap();
    (task, id)
}

/// Boot discovers + reconciles the crash (never marked successful), the
/// operator resumes at the safe point with no duplicate execution, the
/// immutable ledger prefix survives byte-identical, and the chain ends
/// terminal on a further boot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chain_recovery_to_resume() {
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());

    let rt1 = RuntimeBuilder::new()
        .config(Config::default())
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .sink(Arc::new(ChannelSink::new()))
        .bus_capacity(16)
        .build()
        .await
        .unwrap();
    let session = rt1.create_session("chain recovery").await.unwrap();
    rt1.shutdown().await;
    drop(rt1);

    let (crashed_task, checkpoint_id) = seed_crashed_run(&store, session.id, "wf.chainrec").await;

    // Boot: discovery + reconciliation + resume point + boot event.
    let sink = Arc::new(ChannelSink::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let rt2 = RuntimeBuilder::new()
        .config(Config::default())
        .store(store.clone())
        .with_provider(Arc::new(CountingProvider {
            inner: MockProvider::echo("mock"),
            calls: calls.clone(),
        }))
        .sink(sink.clone())
        .bus_capacity(16)
        .build()
        .await
        .unwrap();

    let task_after = store.load_task(crashed_task.id).await.unwrap();
    assert_eq!(task_after.status, TaskStatus::Interrupted);
    assert!(task_after.completed_at.is_none());

    let records = rt2.recovery_records().await.unwrap();
    let record = records.last().expect("boot wrote a recovery record");
    assert!(!record.is_clean());
    assert_eq!(record.interrupted_tasks.len(), 1);
    assert_eq!(record.incomplete_runs.len(), 1);
    let run = &record.incomplete_runs[0];
    assert_eq!(run.workflow_id, "wf.chainrec");
    assert_eq!(run.resume.resume_nodes, vec!["code"]);
    assert_eq!(run.resume.skip_nodes, vec!["plan"]);

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let n = sink
            .snapshot()
            .iter()
            .filter(|e| {
                e.kind == EventKind::RecoveryStarted
                    && e.data.get("phase").and_then(|p| p.as_str()) == Some("boot")
            })
            .count();
        if n >= 1 || std::time::Instant::now() > deadline {
            assert!(n >= 1, "boot emitted a recovery signal");
            break;
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Pre-resume ledger prefix (immutable history reference).
    let tail_before = rt2.history_tail().await.unwrap();
    let prefix_before = openrt_timeseries::materialize_as_of(&*store, tail_before)
        .await
        .unwrap();

    // Operator resumes at the safe point: `code` runs exactly once.
    let report = rt2
        .resume_workflow(
            session.id,
            dep_workflow("wf.chainrec"),
            checkpoint_id,
            openrt_runtime::CheckpointOptions::default(),
            RecoveryPolicy::default(),
        )
        .await
        .unwrap();
    assert_eq!(report.status, WorkflowStatus::Completed);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the completed `plan` node must never re-execute"
    );
    let old_task = store.load_task(crashed_task.id).await.unwrap();
    assert_eq!(
        old_task.status,
        TaskStatus::Interrupted,
        "the crashed attempt stays honestly interrupted"
    );
    let checkpoints = rt2.list_checkpoints(Some("wf.chainrec")).await.unwrap();
    assert_eq!(
        checkpoints.last().unwrap().status,
        CheckpointStatus::Completed
    );

    // The ledger was never rewritten: prefix byte-identical, tail grew.
    let tail_after = rt2.history_tail().await.unwrap();
    assert!(tail_after > tail_before, "resume appends, never rewrites");
    let prefix_after = openrt_timeseries::materialize_as_of(&*store, tail_before)
        .await
        .unwrap();
    assert_eq!(prefix_before, prefix_after);
    rt2.shutdown().await;
    drop(rt2);

    // A further boot finds nothing incomplete — the chain is terminal.
    let rt3 = RuntimeBuilder::new()
        .config(Config::default())
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .sink(Arc::new(ChannelSink::new()))
        .bus_capacity(16)
        .build()
        .await
        .unwrap();
    let records = rt3.recovery_records().await.unwrap();
    assert!(records.last().unwrap().is_clean());
    rt3.shutdown().await;
}

// ---- 7: TUI/runtime shared state ------------------------------------------

/// The TUI Ops surfaces read the SAME runtime store: runtime-side mutations
/// appear on refresh, and UI-side actions land in the durable store the
/// runtime reads back — no UI-local state anywhere.
#[tokio::test]
async fn chain_tui_runtime_shared_state() {
    let rt = RuntimeBuilder::new()
        .config(Config::default())
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .bus_capacity(32)
        .build()
        .await
        .unwrap();

    // Runtime-side mutations first.
    let session = rt.create_session("tui chain").await.unwrap();
    rt.run_task(session.id, "run through the shared runtime")
        .await
        .unwrap();
    rt.assets_register_skill("skill.tui", "tui chain skill", vec!["fs.read".into()])
        .await
        .unwrap();
    rt.assets_validate_skill("skill.tui").await.unwrap();
    let err = rt
        .executor
        .execute(
            "filesystem.read",
            serde_json::json!({ "path": "/home/user/.ssh/id_rsa" }),
            &tctx(),
        )
        .await
        .unwrap_err();
    match err {
        ToolError::PermissionDenied { .. } => {}
        other => panic!("expected shield denial, got {other:?}"),
    }

    // The App wraps the SAME Arc<Runtime> — refresh reads it directly
    // (the ledger applier runs asynchronously: poll the shared view).
    let mut app = openrt_tui::App::new(rt.clone()).await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        app.refresh_ops().await.unwrap();
        if !app.ops_timeline.is_empty() || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!app.ops_timeline.is_empty(), "ledger rows surface in Ops");
    assert!(app
        .ops_skills
        .iter()
        .any(|s| s.id == "skill.tui" && s.state == "validated"));
    assert!(app
        .ops_findings
        .iter()
        .any(|f| { f.category == "credential_access" && f.status == "denied" }));

    // UI-side action → the durable store the runtime reads.
    app.create_session("from the tui").await.unwrap();
    app.run_prompt("prompt typed in the tui").await.unwrap();
    let tasks = rt.store.list_tasks(None).await.unwrap();
    assert!(
        tasks
            .iter()
            .any(|t| t.prompt.contains("prompt typed in the tui")),
        "the TUI wrote through to the shared store"
    );

    // A later runtime-side mutation shows up on the next refresh; the
    // ledger rows for the UI-created session drain asynchronously.
    let op = operator(&rt);
    rt.assets_activate_skill("skill.tui", &op).await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        app.refresh_ops().await.unwrap();
        let active = app
            .ops_skills
            .iter()
            .any(|s| s.id == "skill.tui" && s.state == "active");
        if (active && !app.ops_timeline.is_empty()) || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(app
        .ops_skills
        .iter()
        .any(|s| s.id == "skill.tui" && s.state == "active"));
    assert!(
        !app.ops_timeline.is_empty(),
        "session-scoped ledger rows surface after the UI run"
    );

    // M10.5 time machine through the same App: fork → compare →
    // state-as-of → return — every fact on the status line comes from the
    // shared runtime (store-derived branch names, never UI-local state).
    app.ops_time_machine_fork().await;
    assert!(
        app.status.starts_with("forked branch `tui-fork-"),
        "{}",
        app.status
    );
    let forks = rt.history_forks().await.unwrap();
    assert!(!forks.is_empty(), "fork catalog shares the runtime view");
    app.ops_time_machine_compare().await;
    assert!(app.status.starts_with("branch `"), "{}", app.status);
    assert!(app.status.contains("fork_verified="), "{}", app.status);
    app.ops_time_machine_state().await;
    assert!(app.status.contains("state as-of seq"), "{}", app.status);
    app.ops_time_machine_return().await;
    assert!(
        app.status.starts_with("returned to current head"),
        "{}",
        app.status
    );
    // The parent chain never rewrites.
    let verified = rt.history_verify().await.unwrap();
    assert!(verified >= 1, "parent chain intact: {verified}");
}

// ---- 8: WebUI/runtime shared state ----------------------------------------

/// The command palette dispatches against the SAME durable store + the
/// production Runtime: reads surface runtime state, and a token-authenticated
/// operator approval is written through the runtime (durable: true) that
/// heads the system — one shared state, no palette-local copy.
#[tokio::test]
async fn chain_webui_runtime_shared_state() {
    let rt = RuntimeBuilder::new()
        .config(Config::default())
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .bus_capacity(32)
        .build()
        .await
        .unwrap();
    let op = operator(&rt);

    // Durable state seeded THROUGH the runtime.
    rt.improve_record_baseline(
        "baseline.chain8",
        "latency_ms",
        100.0,
        MetricDirection::LowerIsBetter,
        "run.chain8",
    )
    .await
    .unwrap();
    rt.improve_propose(
        "prop.chain8",
        ProposalKind::SelfOptimize,
        "speed it up",
        TargetKind::Router,
        "router.main",
        Some("baseline.chain8".into()),
        Some(75.0),
    )
    .await
    .unwrap();
    rt.improve_evaluate("prop.chain8").await.unwrap();
    let session = rt.create_session("webui chain").await.unwrap();
    rt.run_task(session.id, "run seen by the palette")
        .await
        .unwrap();
    wait_ledger_stable(&rt).await;

    // Palette over the same ImproveStore + the live Runtime. Dispatch is
    // synchronous by design (the HTTP server thread calls it the same way),
    // so run it on a blocking thread — never on the async worker itself.
    let improve = rt.load_improve().await.unwrap();
    let handle = tokio::runtime::Handle::current();
    let sid = session.id.to_string();
    let rt2 = rt.clone();
    let (timeline, proposals, approve, fork, ret) = tokio::task::spawn_blocking(move || {
        let mut palette = CommandPalette::with_runtime(improve, rt2, handle);
        let token = palette.issue_token(&op);
        let timeline = palette
            .dispatch(&DispatchRequest::new("history.timeline").with_arg("session", &sid))
            .unwrap();
        let proposals = palette
            .dispatch(&DispatchRequest::new("improve.proposals"))
            .unwrap();
        let approve = palette
            .dispatch(
                &DispatchRequest::new("improve.approve")
                    .with_arg("proposal_id", "prop.chain8")
                    .with_auth_token(&token),
            )
            .unwrap();
        let fork = palette
            .dispatch(
                &DispatchRequest::new("history.fork")
                    .with_arg("branch", "chain8-fork")
                    .with_auth_token(&token),
            )
            .unwrap();
        let ret = palette
            .dispatch(&DispatchRequest::new("history.return"))
            .unwrap();
        (timeline, proposals, approve, fork, ret)
    })
    .await
    .unwrap();

    // Read side: the palette sees the runtime's ledger and proposals.
    match timeline {
        PaletteOutcome::Completed { detail, .. } => {
            let v: serde_json::Value = serde_json::from_str(&detail)
                .unwrap_or_else(|e| panic!("timeline detail not JSON ({e}): {detail}"));
            assert!(
                v.get("count").and_then(|c| c.as_u64()).unwrap_or(0) > 0,
                "{detail}"
            );
        }
        PaletteOutcome::Denied { reason, .. } => panic!("timeline denied: {reason}"),
    }
    match proposals {
        PaletteOutcome::Completed { detail, .. } => {
            assert!(
                detail.contains("prop.chain8") && detail.contains("eval_passed"),
                "{detail}"
            );
        }
        PaletteOutcome::Denied { reason, .. } => panic!("proposals denied: {reason}"),
    }

    // Write side: operator approval goes through the runtime (durable).
    let detail = match approve {
        PaletteOutcome::Completed { detail, .. } => detail,
        PaletteOutcome::Denied { reason, .. } => panic!("approve denied: {reason}"),
    };
    assert!(
        detail.contains("\"durable\": true") || detail.contains("\"durable\":true"),
        "{detail}"
    );

    // The runtime's authoritative view reflects the UI's dispatch.
    let improve = rt.load_improve().await.unwrap();
    let states = improve.proposal_states();
    assert!(
        states
            .iter()
            .any(|(id, s)| id == "prop.chain8" && *s == "approved"),
        "states: {states:?}"
    );

    // M10.5 time machine through the palette: operator-gated fork writes
    // the shared ledger's branch catalog; return reads the parent head.
    match fork {
        PaletteOutcome::Completed { detail, .. } => {
            assert!(detail.contains("chain8-fork"), "{detail}")
        }
        PaletteOutcome::Denied { reason, .. } => panic!("fork denied: {reason}"),
    }
    match ret {
        PaletteOutcome::Completed { detail, .. } => {
            let v: serde_json::Value = serde_json::from_str(&detail)
                .unwrap_or_else(|e| panic!("return detail not JSON ({e}): {detail}"));
            assert!(v.get("tail_seq").is_some(), "{detail}");
            assert!(v.get("forks").is_some(), "{detail}");
        }
        PaletteOutcome::Denied { reason, .. } => panic!("return denied: {reason}"),
    }
    let forks = rt.history_forks().await.unwrap();
    assert!(
        forks.iter().any(|f| f.branch == "chain8-fork"),
        "the palette fork lands on the shared store: {forks:?}"
    );
}
