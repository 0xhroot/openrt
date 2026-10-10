//! Phase 10 M10.7 — ECC lifecycle integration tests:
//! * deny-only asset rules enforced at the gate (deny over rules, refresh
//!   from the durable store on load/save — never a grant path),
//! * hook lifecycle fire points (run/task/session/skill/workflow/tool),
//! * bounded instinct influence on run context (advisory, never permissions).

use std::sync::Arc;

use openrt_core::SessionStatus;
use openrt_models::mock::MockProvider;
use openrt_persistence::{MemoryStore, PersistenceStore};
use openrt_runtime::{Runtime, RuntimeBuilder};
use openrt_tools::ToolError;
use openrt_workspace_assets::{
    Hook, HookId, HookTarget, Instinct, InstinctId, OperatorGrant, Provenance as AssetProvenance,
    Rule, RuleEffect, RuleId,
};

fn prov() -> AssetProvenance {
    AssetProvenance::new("run.m10", "src.p10_ecc")
}

fn ctx() -> openrt_tools::ToolExecutionContext {
    openrt_tools::ToolExecutionContext {
        session_id: None,
        task_id: None,
        channel: None,
    }
}

async fn rt() -> Arc<Runtime> {
    RuntimeBuilder::new()
        .config(openrt_config::Config::default())
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

async fn active_hook(rt: &Runtime, id: &str, target: HookTarget, requires: Option<&str>) {
    let op = operator(rt);
    let grant = OperatorGrant::new(&op).unwrap();
    let mut assets = rt.load_assets().await.unwrap();
    assets
        .register_hook(
            Hook::new(
                HookId(id.to_string()),
                1,
                target,
                "m10 lifecycle hook",
                requires.map(str::to_string),
                prov(),
            )
            .unwrap(),
        )
        .unwrap();
    assets
        .activate_hook(&HookId(id.to_string()), Some(&grant))
        .unwrap();
    rt.save_assets(&assets).await.unwrap();
}

async fn deny_gate_rule(rt: &Runtime, id: &str, subject: &str) {
    let mut assets = rt.load_assets().await.unwrap();
    assets
        .register_rule(
            Rule::new(
                RuleId(id.to_string()),
                1,
                "gate",
                subject,
                RuleEffect::Deny,
                10,
                true,
                prov(),
            )
            .unwrap(),
        )
        .unwrap();
    rt.save_assets(&assets).await.unwrap();
}

// ---- deny-only rules at the gate ----

/// The default policy ALLOWS `echo`; only an enabled deny rule at the gate
/// scope stops it — and the deny message carries the rule id.
#[tokio::test]
async fn m10_gate_deny_rule_overrides_policy_allow() {
    let rt = rt().await;
    let out = rt
        .executor
        .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
        .await
        .unwrap();
    assert_eq!(out.render(), "hi");

    deny_gate_rule(&rt, "rule.m10.echo", "echo").await;

    let err = rt
        .executor
        .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
        .await
        .unwrap_err();
    match err {
        ToolError::PermissionDenied { tool, reason } => {
            assert_eq!(tool, "echo");
            assert!(reason.contains("rule.m10.echo"), "{reason}");
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
}

/// A second runtime over the SAME store (never saved through it) must load
/// the deny rule at build time — the layer seeds from durable state.
#[tokio::test]
async fn m10_gate_rule_layer_seeds_from_existing_store() {
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    {
        let rt = RuntimeBuilder::new()
            .config(openrt_config::Config::default())
            .store(store.clone())
            .with_provider(Arc::new(MockProvider::echo("mock")))
            .bus_capacity(32)
            .build()
            .await
            .unwrap();
        deny_gate_rule(&rt, "rule.m10.seed", "echo").await;
    }
    let rt2 = RuntimeBuilder::new()
        .config(openrt_config::Config::default())
        .store(store)
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .bus_capacity(32)
        .build()
        .await
        .unwrap();
    let err = rt2
        .executor
        .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
        .await
        .unwrap_err();
    match err {
        ToolError::PermissionDenied { reason, .. } => {
            assert!(reason.contains("rule.m10.seed"), "{reason}");
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
}

/// Deny-only: an Allow rule at the gate scope must not widen the policy —
/// `filesystem.write` stays denied (deny-by-default) despite the rule.
#[tokio::test]
async fn m10_gate_allow_rule_never_grants() {
    let rt = rt().await;
    let mut assets = rt.load_assets().await.unwrap();
    assets
        .register_rule(
            Rule::new(
                RuleId("rule.m10.widen".into()),
                1,
                "gate",
                "filesystem.write",
                RuleEffect::Allow,
                10,
                true,
                prov(),
            )
            .unwrap(),
        )
        .unwrap();
    rt.save_assets(&assets).await.unwrap();

    let err = rt
        .executor
        .execute(
            "filesystem.write",
            serde_json::json!({ "path": "/nonexistent-m10/x", "content": "x" }),
            &ctx(),
        )
        .await
        .unwrap_err();
    match err {
        ToolError::PermissionDenied { reason, .. } => {
            // The policy still decides (the asset Allow rule never granted):
            // the developer profile Asks `filesystem.write`, which without a
            // resolver fails closed — same denial strength, policy authority.
            assert!(
                reason.contains("denied by policy") || reason.contains("no resolver configured"),
                "policy still decides — rules never grant: {reason}"
            );
            assert!(!reason.contains("asset rule"), "{reason}");
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
}

/// Exact matching: the same subject under a different scope never reaches
/// the gate.
#[tokio::test]
async fn m10_gate_rule_scope_mismatch_does_not_deny() {
    let rt = rt().await;
    let mut assets = rt.load_assets().await.unwrap();
    assets
        .register_rule(
            Rule::new(
                RuleId("rule.m10.elsewhere".into()),
                1,
                "assets",
                "echo",
                RuleEffect::Deny,
                10,
                true,
                prov(),
            )
            .unwrap(),
        )
        .unwrap();
    rt.save_assets(&assets).await.unwrap();

    let out = rt
        .executor
        .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
        .await
        .unwrap();
    assert_eq!(out.render(), "hi");
}

// ---- hook lifecycle fire points ----

/// A `ToolCall` hook whose permission is denied blocks every tool call at
/// the gate (hooks carry the same permission checks as tools).
#[tokio::test]
async fn m10_gate_tool_call_hook_fails_closed() {
    let rt = rt().await;
    active_hook(
        &rt,
        "hook.m10.toolcall",
        HookTarget::ToolCall,
        Some("network.http"),
    )
    .await;

    let err = rt
        .executor
        .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
        .await
        .unwrap_err();
    match err {
        ToolError::Internal(tool, msg) => {
            assert_eq!(tool, "echo");
            assert!(msg.contains("ToolCall hook failed"), "{msg}");
        }
        other => panic!("expected Internal (hook fail-closed), got {other:?}"),
    }
}

/// An active permission-free `ToolCall` hook observes without blocking.
#[tokio::test]
async fn m10_gate_tool_call_hook_observes_without_blocking() {
    let rt = rt().await;
    active_hook(&rt, "hook.m10.toolcall.ok", HookTarget::ToolCall, None).await;

    let out = rt
        .executor
        .execute("echo", serde_json::json!({ "text": "hi" }), &ctx())
        .await
        .unwrap();
    assert_eq!(out.render(), "hi");
}

/// `RunStarted` fires before the run builds a task — a denied hook fails
/// the run closed with no task persisted.
#[tokio::test]
async fn m10_run_started_hook_fires() {
    let rt = rt().await;
    let session = rt.create_session("m10-run").await.unwrap();
    active_hook(
        &rt,
        "hook.m10.runstart",
        HookTarget::RunStarted,
        Some("network.http"),
    )
    .await;

    let err = rt.run_task(session.id, "hello").await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("hook.m10.runstart"),
        "error must come from the RunStarted fire: {msg}"
    );
}

/// `TaskStarted` fires inside `TaskHandle::execute` before the pipeline.
#[tokio::test]
async fn m10_task_started_hook_fires() {
    let rt = rt().await;
    let session = rt.create_session("m10-task").await.unwrap();
    active_hook(
        &rt,
        "hook.m10.taskstart",
        HookTarget::TaskStarted,
        Some("network.http"),
    )
    .await;

    let handle = rt.spawn_task(session.id, "hello").await.unwrap();
    let err = handle.execute().await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("hook.m10.taskstart"),
        "error must come from the TaskStarted fire: {msg}"
    );
}

/// `SessionEnded` fires from `session_end`; the archive transition happens
/// first (durable even when the hook fails closed).
#[tokio::test]
async fn m10_session_end_archives_and_fires_hook() {
    let rt = rt().await;
    let session = rt.create_session("m10-end").await.unwrap();
    active_hook(
        &rt,
        "hook.m10.sessend",
        HookTarget::SessionEnded,
        Some("network.http"),
    )
    .await;

    let err = rt.session_end(session.id).await.unwrap_err();
    assert!(
        err.to_string().contains("hook.m10.sessend"),
        "error must come from the SessionEnded fire: {err}"
    );
    let loaded = rt.load_session(session.id).await.unwrap();
    assert_eq!(loaded.status, SessionStatus::Archived);
}

/// `SkillActivated` fires after activation is saved (the activation survives
/// a hook failure — proven by the state query after the error).
#[tokio::test]
async fn m10_skill_activated_hook_fires() {
    let rt = rt().await;
    active_hook(
        &rt,
        "hook.m10.skillact",
        HookTarget::SkillActivated,
        Some("network.http"),
    )
    .await;
    rt.assets_register_skill("skill.m10.act", "demo", vec!["fs.read".into()])
        .await
        .unwrap();
    rt.assets_validate_skill("skill.m10.act").await.unwrap();

    let err = rt
        .assets_activate_skill("skill.m10.act", &operator(&rt))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("hook.m10.skillact"),
        "error must come from the SkillActivated fire: {err}"
    );
    let states = rt.assets_skill_states().await.unwrap();
    assert!(
        states
            .iter()
            .any(|(id, s)| id == "skill.m10.act" && s == "active"),
        "activation saved before the hook fired: {states:?}"
    );
}

fn wf() -> openrt_orch::Workflow {
    openrt_orch::Workflow::new("wf.m10").step(openrt_orch::WorkflowStep::new(
        "s1",
        openrt_orch::SubAgentKind::Planner,
        "plan it",
    ))
}

/// `WorkflowStarted` fires before the engine runs — a denied hook fails the
/// run closed before any step executes.
#[tokio::test]
async fn m10_workflow_started_hook_fires() {
    let rt = rt().await;
    let session = rt.create_session("m10-wf").await.unwrap();
    active_hook(
        &rt,
        "hook.m10.wfstart",
        HookTarget::WorkflowStarted,
        Some("network.http"),
    )
    .await;

    let err = rt.run_workflow(session.id, wf()).await.unwrap_err();
    assert!(
        err.to_string().contains("hook.m10.wfstart"),
        "error must come from the WorkflowStarted fire: {err}"
    );
}

/// `WorkflowCompleted` fires after the report is durable — the run reached
/// its end (the error can only come from that fire).
#[tokio::test]
async fn m10_workflow_completed_hook_fires() {
    let rt = rt().await;
    let session = rt.create_session("m10-wf2").await.unwrap();
    active_hook(
        &rt,
        "hook.m10.wfdone",
        HookTarget::WorkflowCompleted,
        Some("network.http"),
    )
    .await;

    let err = rt.run_workflow(session.id, wf()).await.unwrap_err();
    assert!(
        err.to_string().contains("hook.m10.wfdone"),
        "error must come from the WorkflowCompleted fire (engine succeeded first): {err}"
    );
}

// ---- bounded instinct influence on run context ----

/// Active instincts with confidence >= 0.5 append advisory context to the
/// run prompt (top 3, truncated); below-threshold and legacy instincts
/// never influence. The original prompt stays the prefix.
#[tokio::test]
async fn m10_instinct_influence_on_run_context_is_bounded() {
    let rt = rt().await;
    let session = rt.create_session("m10-inst").await.unwrap();
    let op = operator(&rt);
    let grant = OperatorGrant::new(&op).unwrap();

    let mut assets = rt.load_assets().await.unwrap();
    let mut confident = Instinct::new(
        InstinctId("instinct.m10.high".into()),
        1,
        "always use tool x",
        90,
        prov(),
    )
    .unwrap();
    confident.trigger = "tool.y".into();
    confident.recommendation = "prefer tool x over tool y".into();
    confident.confidence = 0.9;
    assets.register_instinct(confident).unwrap();

    let mut low = Instinct::new(
        InstinctId("instinct.m10.low".into()),
        1,
        "weak guess",
        40,
        prov(),
    )
    .unwrap();
    low.recommendation = "weak suggestion must not appear".into();
    low.confidence = 0.4;
    assets.register_instinct(low).unwrap();

    // Legacy instinct: default confidence 0.0 (unspecified) — never influences.
    let legacy = Instinct::new(
        InstinctId("instinct.m10.legacy".into()),
        1,
        "legacy pattern",
        10,
        prov(),
    )
    .unwrap();
    assets.register_instinct(legacy).unwrap();

    for id in [
        "instinct.m10.high",
        "instinct.m10.low",
        "instinct.m10.legacy",
    ] {
        let iid = InstinctId(id.to_string());
        assets.validate_instinct(&iid).unwrap();
        assets.activate_instinct(&iid, Some(&grant)).unwrap();
    }
    rt.save_assets(&assets).await.unwrap();

    let task = rt.run_task(session.id, "base prompt").await.unwrap();
    assert!(task.prompt.starts_with("base prompt"));
    assert!(
        task.prompt.contains("[instincts"),
        "active confident instinct must append context: {}",
        task.prompt
    );
    assert!(task.prompt.contains("prefer tool x over tool y"));
    assert!(
        !task.prompt.contains("weak suggestion"),
        "confidence 0.4 is below the 0.5 threshold"
    );
    assert_eq!(
        task.prompt.matches("\n- ").count(),
        1,
        "exactly one instinct line (0.9 only; 0.4 and legacy excluded)"
    );
}

/// With no active instincts the prompt passes through byte-for-byte.
#[tokio::test]
async fn m10_no_active_instincts_prompt_unchanged() {
    let rt = rt().await;
    let session = rt.create_session("m10-noinst").await.unwrap();
    let task = rt.run_task(session.id, "plain prompt").await.unwrap();
    assert_eq!(task.prompt, "plain prompt");
}
