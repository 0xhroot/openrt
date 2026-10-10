//! Phase 8 (M8.8/M8.9) — the unified sandbox, end-to-end through the real
//! runtime:
//! * `python.run` production output carries the sandbox identity (manager
//!   path, not the legacy Phase 2.4 path);
//! * workflow-level sandbox admission fails closed on a declared unknown
//!   profile, before any step runs;
//! * `sandbox.*` admission decisions land in the chained audit log;
//! * classified escape probes against the live bwrap backend.
//!
//! Escape-test classification (M8.8):
//! * SAFE_UNIT — sandbox-crate self-tests inside `openrt-sandbox`;
//! * SAFE_HOST — probes executed INSIDE the real backend verifying the HOST
//!   is untouched (this file: no host tampering, no destructive ops);
//! * DESTRUCTIVE — bare-host tampering (kill init, /proc corruption,
//!   disabling the host network) — intentionally NOT executed here.

use std::path::PathBuf;
use std::sync::Arc;

use openrt_config::{
    Config, PermissionsConfig, PythonSandboxConfig, RuleConfig, RuleEffect, RuntimeConfig,
};
use openrt_models::mock::MockProvider;
use openrt_orch::WorkflowError;

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p8-{prefix}-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn allow_rules(caps: &[&str]) -> PermissionsConfig {
    let mut rules: Vec<RuleConfig> = caps
        .iter()
        .flat_map(|c| {
            [
                RuleConfig {
                    effect: RuleEffect::Allow,
                    capability: (*c).to_string(),
                    resource: None,
                },
                RuleConfig {
                    effect: RuleEffect::Allow,
                    capability: format!("{c}.all"),
                    resource: None,
                },
            ]
        })
        .collect();
    rules.dedup_by(|a, b| a.capability == b.capability);
    PermissionsConfig { rules }
}

async fn build(config: Config) -> Arc<openrt_runtime::Runtime> {
    openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(openrt_persistence::MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")))
        .build()
        .await
        .unwrap()
}

async fn tool_ctx(
    rt: &openrt_runtime::Runtime,
) -> (
    openrt_core::SessionId,
    openrt_core::TaskId,
    openrt_tools::ToolExecutionContext,
) {
    let session = rt.create_session("p8").await.unwrap();
    let task = openrt_core::Task::new(session.id, "sandbox");
    let c = openrt_tools::ToolExecutionContext {
        session_id: Some(session.id.to_string()),
        task_id: Some(task.id.to_string()),
        channel: None,
    };
    (session.id, task.id, c)
}

/// M8.9: the registered `python.run` tool runs through the unified manager —
/// its output carries the sandbox identity (id, state, profile, isolation,
/// network, policy digest, verified cleanup), which the legacy path cannot
/// produce.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_run_is_managed_and_carries_sandbox_identity() {
    let ws = tmp_dir("identity");
    let config = Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws.clone()),
            ..RuntimeConfig::default()
        },
        permissions: allow_rules(&["python.run"]),
        ..Config::default()
    };
    let rt = build(config).await;
    let (_, _, c) = tool_ctx(&rt).await;

    // Wiring: the gate-side requirement is registered for python.run.
    assert_eq!(
        rt.sandbox.requirement("python.run").map(|p| p.as_str()),
        Some("strict")
    );

    let out = rt
        .executor
        .execute(
            "python.run",
            serde_json::json!({ "script": "print('managed')" }),
            &c,
        )
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out.render()).unwrap();
    assert_eq!(v["stdout"].as_str().unwrap().trim(), "managed");
    assert_eq!(v["exit_code"], 0);
    assert_eq!(v["profile"], "strict");
    assert_eq!(v["state"], "succeeded");
    assert_eq!(v["network"], "none");
    assert_eq!(v["isolation"], "namespace");
    assert_eq!(
        v["cleanup_verified"],
        serde_json::json!(true),
        "cleanup must be verified: {v}"
    );
    let id = v["sandbox_id"].as_str().unwrap();
    assert!(!id.is_empty());

    // The instance is discoverable through the authority's registry.
    let rec = rt.sandbox.inspect(id).await.unwrap();
    assert_eq!(rec.id.to_string(), id);

    // Admission decisions are recorded in the chained audit log.
    let records = rt
        .audit
        .query(&openrt_audit::AuditQuery::new().with_action_prefix("sandbox."))
        .await
        .unwrap();
    assert!(!records.is_empty(), "expected sandbox.* audit actions");

    rt.shutdown().await;
}

/// M8.9: profile admission (workflow-level check) accepts known profiles and
/// rejects unknown ones with a structured admission failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_profile_accepts_known_and_rejects_unknown() {
    let ws = tmp_dir("profiles");
    let config = Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws),
            ..RuntimeConfig::default()
        },
        ..Config::default()
    };
    let rt = build(config).await;
    rt.sandbox
        .check_profile("strict")
        .expect("strict available");
    rt.sandbox
        .check_profile("development")
        .expect("development available");
    let err = rt.sandbox.check_profile("totally-bogus").unwrap_err();
    assert_eq!(err.code.as_str(), "ADMISSION_DENIED");
    rt.shutdown().await;
}

/// M8.9: a workflow declaring an unknown sandbox profile is rejected BEFORE
/// the first step runs (WorkflowError::SandboxDenied, fail closed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_sandbox_admission_fails_closed_on_unknown_profile() {
    let ws = tmp_dir("wf-deny");
    let config = Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws),
            ..RuntimeConfig::default()
        },
        ..Config::default()
    };
    let rt = build(config).await;
    let session = rt.create_session("p8wf-deny").await.unwrap();

    use openrt_orch::{SubAgentKind, Workflow, WorkflowStep};
    let wf = Workflow::new("wf.p8.deny")
        .step(WorkflowStep::new("plan", SubAgentKind::Planner, "plan it").sandboxed("no-such"));
    let err = rt.run_workflow(session.id, wf).await.unwrap_err();
    match &err {
        openrt_runtime::Error::Orchestration(WorkflowError::SandboxDenied { step, reason }) => {
            assert_eq!(step, "plan");
            assert!(reason.contains("no-such"), "{reason}");
        }
        other => panic!("expected SandboxDenied, got {other:?}"),
    }
    rt.shutdown().await;
}

/// A declared KNOWN profile passes admission and the workflow proceeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_sandbox_admission_accepts_known_profile() {
    let ws = tmp_dir("wf-ok");
    let config = Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws),
            ..RuntimeConfig::default()
        },
        ..Config::default()
    };
    let rt = build(config).await;
    let session = rt.create_session("p8wf-ok").await.unwrap();

    use openrt_orch::{SubAgentKind, Workflow, WorkflowStep};
    let wf = Workflow::new("wf.p8.ok")
        .step(WorkflowStep::new("plan", SubAgentKind::Planner, "plan it").sandboxed("strict"));
    let report = rt.run_workflow(session.id, wf).await.unwrap();
    assert_eq!(
        report.status,
        openrt_orch::WorkflowStatus::Completed,
        "{report:?}"
    );
    rt.shutdown().await;
}

/// M8.8 (SAFE_HOST): a filesystem escape attempt inside the managed sandbox
/// never touches the host.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escape_write_outside_host_is_denied() {
    let marker = std::env::temp_dir().join(format!("openrt-p8-escape-{}", std::process::id()));
    let ws = tmp_dir("escape");
    let config = Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws),
            ..RuntimeConfig::default()
        },
        permissions: allow_rules(&["python.run"]),
        ..Config::default()
    };
    let rt = build(config).await;
    let (_, _, c) = tool_ctx(&rt).await;

    let marker_arg = marker.display().to_string();
    let out = rt
        .executor
        .execute(
            "python.run",
            serde_json::json!({ "script": format!(
                "try:\n\
                 \x20   open({marker_arg:?},'w').write('pwned')\n\
                 \x20   print('WROTE_HOST')\n\
                 except Exception as e:\n\
                 \x20   print('denied:', type(e).__name__)"
            ) }),
            &c,
        )
        .await
        .unwrap();
    let rendered = out.render();
    assert!(!rendered.contains("WROTE_HOST"), "escape: {rendered}");
    assert!(!marker.exists(), "host marker file was created");
    rt.shutdown().await;
}

/// M8.8 (SAFE_HOST): NetworkMode::None (Strict profile) blocks even loopback
/// egress from inside the sandbox while the host listener stays reachable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn network_none_blocks_loopback_from_sandbox() {
    use tokio::net::TcpListener;
    let ws = tmp_dir("net");
    let config = Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws),
            ..RuntimeConfig::default()
        },
        permissions: allow_rules(&["python.run"]),
        ..Config::default()
    };
    let rt = build(config).await;
    let (_, _, c) = tool_ctx(&rt).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let out = rt
        .executor
        .execute(
            "python.run",
            serde_json::json!({ "script": format!(
                "import socket\n\
                 s=socket.socket()\n\
                 s.settimeout(2)\n\
                 try:\n\
                 \x20   s.connect(('127.0.0.1',{port}))\n\
                 \x20   print('CONNECTED')\n\
                 except Exception as e:\n\
                 \x20   print('denied:', type(e).__name__)"
            ) }),
            &c,
        )
        .await
        .unwrap();
    let rendered = out.render();
    assert!(
        !rendered.contains("CONNECTED"),
        "egress allowed: {rendered}"
    );
    assert!(rendered.contains("denied"), "{rendered}");
    rt.shutdown().await;
}

/// M8.8 (SAFE_HOST): wall-clock timeout kills the sandboxed run and the tool
/// reports the failure instead of hanging the host.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wall_timeout_kills_run_and_reports() {
    let ws = tmp_dir("timeout");
    let config = Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws),
            python_sandbox: Some(PythonSandboxConfig {
                wall_timeout_secs: Some(2),
                ..PythonSandboxConfig::default()
            }),
            ..RuntimeConfig::default()
        },
        permissions: allow_rules(&["python.run"]),
        ..Config::default()
    };
    let rt = build(config).await;
    let (_, _, c) = tool_ctx(&rt).await;

    let err = rt
        .executor
        .execute(
            "python.run",
            serde_json::json!({ "script": "import time; time.sleep(30); print('SLEPT')" }),
            &c,
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("timeout") || msg.contains("sandbox"), "{msg}");
    rt.shutdown().await;
}
