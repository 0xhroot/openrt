//! Interactive agent tool availability: the zero-config `openrt agent` path
//! (effective config → developer profile → gate → model advertisement).
//!
//! Contract under test:
//! * safe tools are advertised AND allowed (the model can actually call them);
//! * mutation tools are advertised AND `Ask` (visible, one approval, never
//!   silently executed, fail-closed without a resolver);
//! * unlisted tools stay deny-by-default: never advertised, never executed;
//! * a detected workspace root reaches the effective config (no more
//!   "No workspace root configured");
//! * real tool calls produce real permission events and real denials come
//!   back to the model as structured `{"ok":false,…}` feedback;
//! * Sovereign mode blocks external network use before any socket.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use openrt_agents::{Agent, AgentContext, AgentEndReason, MinimalAgent};
use openrt_config::{
    detect_workspace_root, Config, RuleConfig, RuleEffect, RuntimeConfig, WorkspaceSource,
};
use openrt_core::{EventKind, Id};
use openrt_models::mock::MockProvider;
use openrt_permissions::{
    Decision, MockResolver, PermissionContext, PermissionRequest, PolicyEvaluator,
};
use openrt_persistence::MemoryStore;
use openrt_runtime::{Runtime, RuntimeBuilder};
use openrt_telemetry::ChannelSink;
use openrt_tools::{ToolExecutionContext, ToolOutput};

// ---- helpers ---------------------------------------------------------------

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-agentprof-{prefix}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn config(workspace: &PathBuf) -> Config {
    Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(workspace.clone()),
            ..RuntimeConfig::default()
        },
        ..Config::default()
    }
}

/// Rules are first-match-wins: an explicit rule must come before the
/// developer profile's rules to override them.
fn allow_first(cfg: &mut Config, caps: &[&str]) {
    for cap in caps {
        cfg.permissions.rules.insert(
            0,
            RuleConfig {
                effect: RuleEffect::Allow,
                capability: (*cap).to_string(),
                resource: None,
            },
        );
    }
}

async fn build_with(cfg: Config, sink: Option<Arc<ChannelSink>>, approving: bool) -> Arc<Runtime> {
    let mut builder = RuntimeBuilder::new()
        .config(cfg)
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")));
    if let Some(sink) = sink {
        builder = builder.sink(sink);
    }
    if approving {
        builder = builder.permission_resolver(Arc::new(MockResolver::allow_all()));
    }
    builder.build().await.unwrap()
}

async fn build(cfg: Config) -> Arc<Runtime> {
    build_with(cfg, None, false).await
}

fn ctx() -> ToolExecutionContext {
    ToolExecutionContext {
        session_id: None,
        task_id: None,
        channel: None,
    }
}

fn advertised_names(rt: &Runtime) -> HashSet<String> {
    rt.advertised_tools()
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect()
}

fn decision(rt: &Runtime, tool: &str) -> Decision {
    let evaluator = PolicyEvaluator::new(&rt.permission_policy);
    evaluator.evaluate(&PermissionRequest::tool(
        tool,
        &serde_json::json!({}),
        &PermissionContext::new(),
    ))
}

/// Sink writes are async; wait for a terminal event before snapshotting
/// (same pattern as `gate_deny.rs`).
async fn wait_for(channel: &ChannelSink, kind: EventKind) {
    for _ in 0..200 {
        if channel.snapshot().iter().any(|e| e.kind == kind) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{kind:?} never arrived");
}

// ---- 1. advertisement: allow / ask / deny ----------------------------------

/// Safe tools are advertised AND allowed: the model sees them and a call
/// actually succeeds (advertisement without execution rights is a trap).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn profile_advertises_safe_tools_and_allows_them() {
    let ws = tmp_dir("allow");
    let rt = build(config(&ws)).await;
    let advertised = advertised_names(&rt);

    for tool in [
        "echo",
        "system.info",
        "tools.list",
        "filesystem.list",
        "filesystem.read",
        "application.list",
        "artifact.list",
        "computer.env",
        "browser.status",
        "browser.find",
    ] {
        assert!(
            advertised.contains(tool),
            "`{tool}` must be advertised to the model; hidden: {:?}",
            advertised
        );
        assert_eq!(
            decision(&rt, tool),
            Decision::Allow,
            "`{tool}` must be Allowed, not merely visible"
        );
    }

    // And an allowed read really executes through the gate.
    let out = rt
        .executor
        .execute("echo", serde_json::json!({ "text": "hello" }), &ctx())
        .await
        .expect("allowed tool must execute");
    assert!(out.render().contains("hello"), "{out:?}");
    rt.shutdown().await;
}

/// Mutation tools stay visible to the model while policy says `Ask` — the
/// model must be able to *attempt* them so the approval card can appear.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_tools_remain_visible_to_the_model() {
    let ws = tmp_dir("ask");
    let rt = build(config(&ws)).await;
    let advertised = advertised_names(&rt);

    for tool in [
        "filesystem.write",
        "filesystem.delete",
        "filesystem.move",
        "terminal.run",
        "python.run",
        "computer.app",
        "computer.use",
        "browser.navigate",
        "screen.capture",
    ] {
        assert!(
            advertised.contains(tool),
            "`{tool}` is Ask, not Deny — it must stay advertised; hidden: {:?}",
            advertised
        );
        assert_eq!(
            decision(&rt, tool),
            Decision::Ask,
            "`{tool}` must require approval"
        );
    }
    rt.shutdown().await;
}

/// Deny-by-default: unlisted tools are never advertised and the policy says
/// `Deny` for every one of them (deny-only filter ⇒ hidden ⇒ Deny).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_tools_are_hidden_from_the_model() {
    let ws = tmp_dir("deny");
    let rt = build(config(&ws)).await;
    let advertised = advertised_names(&rt);
    let registered: HashSet<String> = rt.registry.names().into_iter().collect();

    // Advertisement is a strict subset: some tools exist yet are withheld.
    assert!(
        advertised.len() < registered.len(),
        "profile must withhold something: {} advertised of {}",
        advertised.len(),
        registered.len()
    );
    assert!(advertised.is_subset(&registered));

    for tool in [
        "database.query",
        "database.insert",
        "industrial.assets",
        "industrial.analysis",
        "trading.backtest",
        "learn.experience",
        "mcp.discover",
        "mcp.servers",
    ] {
        assert!(
            !advertised.contains(tool),
            "`{tool}` must never be advertised to the model"
        );
        assert_eq!(
            decision(&rt, tool),
            Decision::Deny,
            "`{tool}` must be Deny-by-default"
        );
    }

    // Every hidden tool's id decision is Deny (the advertisement filter is
    // deny-only — nothing is hidden for any other reason).
    for name in registered.difference(&advertised) {
        assert_eq!(
            decision(&rt, name),
            Decision::Deny,
            "hidden tool `{name}` must be Deny, not an unrelated withholding"
        );
    }
    rt.shutdown().await;
}

/// computer.app specifically: a platform Ask tool is advertised (usable by
/// the model) while still gated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn computer_app_is_advertised_when_ask() {
    let ws = tmp_dir("computer-app");
    let rt = build(config(&ws)).await;
    assert!(
        advertised_names(&rt).contains("computer.app"),
        "computer.app must be advertised"
    );
    assert_eq!(decision(&rt, "computer.app"), Decision::Ask);

    // Ask without a resolver: fail closed — never executes.
    let err = rt
        .executor
        .execute(
            "computer.app",
            serde_json::json!({ "action": "open", "name": "firefox" }),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, openrt_tools::ToolError::PermissionDenied { .. }),
        "must fail closed without a resolver, got {err:?}"
    );
    rt.shutdown().await;
}

/// terminal.run specifically: advertised (visible) yet gated by Ask.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_run_is_advertised_when_ask() {
    let ws = tmp_dir("terminal");
    let rt = build(config(&ws)).await;
    assert!(
        advertised_names(&rt).contains("terminal.run"),
        "terminal.run must be advertised"
    );
    assert_eq!(decision(&rt, "terminal.run"), Decision::Ask);
    let err = rt
        .executor
        .execute(
            "terminal.run",
            serde_json::json!({ "command": "id" }),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, openrt_tools::ToolError::PermissionDenied { .. }),
        "must fail closed without a resolver, got {err:?}"
    );
    rt.shutdown().await;
}

/// filesystem.write specifically: advertised (visible) yet gated by Ask.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filesystem_write_is_advertised_when_ask() {
    let ws = tmp_dir("write");
    let rt = build(config(&ws)).await;
    assert!(
        advertised_names(&rt).contains("filesystem.write"),
        "filesystem.write must be advertised"
    );
    assert_eq!(decision(&rt, "filesystem.write"), Decision::Ask);
    rt.shutdown().await;
}

// ---- 7. a real tool call reaches the gate ----------------------------------

/// The call is NOT a UI-side fiction: it crosses the gate and produces
/// PermissionRequested → PermissionGranted → ToolInvocationStarted events,
/// then executes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_tool_call_reaches_the_gate_with_permission_events() {
    let ws = tmp_dir("events");
    let sink = Arc::new(ChannelSink::new());
    let rt = build_with(config(&ws), Some(sink.clone()), true).await;

    let out = rt
        .executor
        .execute(
            "terminal.run",
            serde_json::json!({ "command": "id" }),
            &ctx(),
        )
        .await
        .expect("approved ask tool must execute");
    assert!(matches!(out, ToolOutput::Json(_)), "{out:?}");

    wait_for(&sink, EventKind::PermissionGranted).await;
    let events = sink.snapshot();
    let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    assert!(
        kinds.contains(&EventKind::PermissionRequested),
        "expected PermissionRequested, got {kinds:?}"
    );
    assert!(
        kinds.contains(&EventKind::PermissionGranted),
        "expected PermissionGranted, got {kinds:?}"
    );
    assert!(
        kinds.contains(&EventKind::ToolInvocationStarted),
        "expected the tool to actually start, got {kinds:?}"
    );
    rt.shutdown().await;
}

// ---- 8. a denial reaches the model as structured feedback ------------------

/// The gate refuses an unlisted tool mid-task; the agent loop feeds the
/// denial back as machine-readable `{"ok":false,…}` instead of crashing —
/// the model can see the obstacle and adapt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_denial_reaches_the_model_as_structured_feedback() {
    let ws = tmp_dir("feedback");
    let rt = build(config(&ws)).await;

    let provider = MockProvider::tool_sequence("mock").with_tool_script(vec![(
        "database.query",
        serde_json::json!({ "sql": "select 1" }),
    )]);
    let agent = MinimalAgent::with_default_limits(
        "minimal",
        Arc::new(provider),
        rt.executor.clone(),
        rt.registry.provider_tools(),
    );
    let result = agent
        .run(&AgentContext::new(Id::new(), Id::new()), "try it")
        .await
        .unwrap();

    assert_eq!(result.ended, AgentEndReason::Completed);
    assert!(
        result.final_text.contains("mock final"),
        "the task must survive the denial: {}",
        result.final_text
    );
    let tool_msgs: Vec<_> = result
        .messages
        .iter()
        .filter(|m| m.role == openrt_core::message::MessageRole::Tool)
        .collect();
    assert_eq!(tool_msgs.len(), 1, "one tool call, one structured denial");
    let feedback = &tool_msgs[0].content;
    assert!(
        feedback.contains("\"ok\":false"),
        "denial must be structured feedback: {feedback}"
    );
    assert!(
        feedback.contains("database.query") && feedback.contains("permission denied"),
        "feedback must name the tool and the denial: {feedback}"
    );
    rt.shutdown().await;
}

// ---- 9. workspace root detection -------------------------------------------

/// The effective config never leaves the agent rootless: an unconfigured
/// `runtime.workspace_dir` is filled from real workspace markers (git root
/// first), never invented from nowhere.
#[test]
fn workspace_root_is_detected_by_the_effective_config() {
    let root = tmp_dir("gitroot");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let nested = root.join("crates").join("itest");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("Cargo.toml"), b"[package]\nname = \"itest\"\n").unwrap();

    let (detected, source) = detect_workspace_root(&nested);
    assert_eq!(detected, root, "must walk up to the git root");
    assert_eq!(source, WorkspaceSource::GitRoot);

    // No operator config on disk: load_effective still yields a workspace.
    let cfg = Config::load_effective_from(None).expect("effective config resolves");
    assert!(
        cfg.runtime.workspace_dir.is_some(),
        "interactive default must never be workspace-less"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---- 10. external network blocked in Sovereign Mode ------------------------

/// The browser tool refuses an external URL in Sovereign mode *before* any
/// backend/socket involvement — through the real gate, with the profile's
/// Ask bypassed by an explicit operator allow (so the net guard, not the
/// permission layer, is what answers).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sovereign_mode_blocks_external_browser_navigate_before_backend() {
    let ws = tmp_dir("sovereign");
    let mut cfg = config(&ws);
    allow_first(&mut cfg, &["browser.navigate", "browser.navigate.all"]);
    let rt = build(cfg).await;

    // Defense in depth: the Sovereign network rule (shield, before policy)
    // and the browser tool's net guard (before any socket) both refuse —
    // whichever layer answers, no egress may happen.
    match rt
        .executor
        .execute(
            "browser.navigate",
            serde_json::json!({ "url": "https://example.com/" }),
            &ctx(),
        )
        .await
    {
        Err(openrt_tools::ToolError::PermissionDenied { reason, .. }) => {
            assert!(
                reason.contains("AgentShield") || reason.contains("policy"),
                "external egress must be refused by an authority: {reason}"
            );
        }
        Ok(ToolOutput::Json(v)) => {
            assert_eq!(v["ok"], serde_json::json!(false), "{v}");
            assert_eq!(
                v["error"]["code"],
                serde_json::json!("POLICY_DENIED"),
                "{v}"
            );
        }
        other => panic!("external navigate must be refused, got {other:?}"),
    }

    // Loopback is always allowed under Sovereign: the denial above is about
    // the external destination, never about the browser tool itself.
    let local = rt
        .executor
        .execute(
            "browser.navigate",
            serde_json::json!({ "url": "http://127.0.0.1:9/" }),
            &ctx(),
        )
        .await;
    match local {
        Ok(ToolOutput::Json(v)) => assert_ne!(
            v["error"]["code"],
            serde_json::json!("POLICY_DENIED"),
            "loopback must not be sovereign-blocked: {v}"
        ),
        Ok(_) => {}
        Err(openrt_tools::ToolError::PermissionDenied { reason, .. }) => {
            panic!("loopback must not be permission-denied: {reason}")
        }
        Err(other) => panic!("loopback refusal must not be a permission error: {other:?}"),
    }
    rt.shutdown().await;
}
