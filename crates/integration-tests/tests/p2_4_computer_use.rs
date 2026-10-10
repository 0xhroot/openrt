//! P2.4 computer-use tools, end-to-end through the real runtime and gate:
//! * jail-scoped filesystem write/list/move/delete and terminal tools;
//! * escape attempts (traversal, out-of-root, symlink) denied in the tool
//!   layer even when policy allows the capability;
//! * dangerous tools are deny-by-default until an explicit allow rule;
//! * the python sandbox runs confined scripts and contains escape attempts.

use std::path::PathBuf;
use std::sync::Arc;

use openrt_config::{
    Config, PermissionsConfig, PythonSandboxConfig, RuleConfig, RuleEffect, RuntimeConfig,
};
use openrt_models::mock::MockProvider;
use openrt_tools::ToolError;

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p2_4-{prefix}-{}-{:x}",
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
    // Gate enforces `required_caps` (e.g. `filesystem.write.all`) as separate
    // permission requests — allow the tool id and its `.all` annotation.
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
    build_with_resolver(config, true).await
}

/// No permission resolver: `Ask` decisions must fail closed.
async fn build_no_resolver(config: Config) -> Arc<openrt_runtime::Runtime> {
    build_with_resolver(config, false).await
}

async fn build_with_resolver(config: Config, approving: bool) -> Arc<openrt_runtime::Runtime> {
    let mut builder = openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(openrt_persistence::MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::tool_sequence("mock")));
    if approving {
        // M10.9: AgentShield high-severity findings route through the same
        // approval path as policy `Ask`. Approvals are granted here so the
        // TOOL-layer scope contract is what actually stops escape attempts
        // (without a resolver the shield would fail-closed deny first).
        builder =
            builder.permission_resolver(Arc::new(openrt_permissions::MockResolver::allow_all()));
    }
    builder.build().await.unwrap()
}

fn ctx(
    session: openrt_core::SessionId,
    task: openrt_core::TaskId,
) -> openrt_tools::ToolExecutionContext {
    openrt_tools::ToolExecutionContext {
        session_id: Some(session.to_string()),
        task_id: Some(task.to_string()),
        channel: None,
    }
}

async fn tool_ctx(
    rt: &openrt_runtime::Runtime,
) -> (
    openrt_core::SessionId,
    openrt_core::TaskId,
    openrt_tools::ToolExecutionContext,
) {
    let session = rt.create_session("p2.4").await.unwrap();
    let task = openrt_core::Task::new(session.id, "computer-use");
    let c = ctx(session.id, task.id);
    (session.id, task.id, c)
}

/// M2.3 tools are registered when a workspace is configured.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn computer_tools_are_registered() {
    let ws = tmp_dir("ws-reg");
    let config = Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws.clone()),
            ..RuntimeConfig::default()
        },
        ..Config::default()
    };
    let rt = build(config).await;
    let names = rt.registry.names();
    for tool in [
        "filesystem.write",
        "filesystem.list",
        "filesystem.move",
        "filesystem.delete",
        "terminal.run",
        "python.run",
    ] {
        assert!(
            names.contains(&tool.to_string()),
            "missing {tool}: {names:?}"
        );
    }
    rt.shutdown().await;
}

/// Jail-scoped filesystem mutations work inside the workspace and every
/// out-of-root / traversal escape is rejected in the tool layer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filesystem_tools_are_jail_scoped_end_to_end() {
    let ws = tmp_dir("ws-tools");
    let config = Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws.clone()),
            ..RuntimeConfig::default()
        },
        permissions: allow_rules(&[
            "filesystem.write",
            "filesystem.list",
            "filesystem.move",
            "filesystem.delete",
        ]),
        ..Config::default()
    };
    let rt = build(config).await;
    let (_, _, c) = tool_ctx(&rt).await;

    let file = ws.join("a.txt");
    let out = rt
        .executor
        .execute(
            "filesystem.write",
            serde_json::json!({ "path": file.display().to_string(), "content": "hello" }),
            &c,
        )
        .await
        .unwrap();
    assert!(out.render().contains("bytes_written"), "{}", out.render());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello");

    let list = rt
        .executor
        .execute("filesystem.list", serde_json::json!({}), &c)
        .await
        .unwrap();
    assert!(list.render().contains("a.txt"), "{}", list.render());

    let moved = ws.join("b.txt");
    rt.executor
        .execute(
            "filesystem.move",
            serde_json::json!({
                "source": file.display().to_string(),
                "destination": moved.display().to_string()
            }),
            &c,
        )
        .await
        .unwrap();
    assert!(moved.exists() && !file.exists());

    rt.executor
        .execute(
            "filesystem.delete",
            serde_json::json!({ "path": moved.display().to_string() }),
            &c,
        )
        .await
        .unwrap();
    assert!(!moved.exists());

    // Escape attempts are denied by the tool layer even though policy allows.
    for (tool, args) in [
        (
            "filesystem.write",
            serde_json::json!({ "path": "/tmp/openrt-p2_4-outside.txt" }),
        ),
        (
            "filesystem.write",
            serde_json::json!({ "path": format!("{}/../../etc/pwned", ws.display()) }),
        ),
        (
            "filesystem.delete",
            serde_json::json!({ "path": "/etc/hostname" }),
        ),
    ] {
        let err = rt.executor.execute(tool, args, &c).await.unwrap_err();
        assert!(
            matches!(err, ToolError::ScopeViolation { .. }),
            "{tool} should be a ScopeViolation, got {err:?}"
        );
    }

    rt.shutdown().await;
}

/// Terminal runs commands with working-dir scoping and enforces the timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_runs_and_enforces_timeout() {
    let ws = tmp_dir("ws-term");
    let config = Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws.clone()),
            terminal_timeout_secs: Some(1),
            ..RuntimeConfig::default()
        },
        permissions: allow_rules(&["terminal.run"]),
        ..Config::default()
    };
    let rt = build(config).await;
    let (_, _, c) = tool_ctx(&rt).await;

    let out = rt
        .executor
        .execute(
            "terminal.run",
            serde_json::json!({ "command": "printf 'ready'" }),
            &c,
        )
        .await
        .unwrap();
    assert!(out.render().contains("ready"), "{}", out.render());

    let err = rt
        .executor
        .execute(
            "terminal.run",
            serde_json::json!({ "command": "sleep 30" }),
            &c,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, ToolError::Internal(_, ref m) if m.contains("timed out")),
        "expected timeout, got {err:?}"
    );

    // Out-of-scope cwd is rejected.
    let err = rt
        .executor
        .execute(
            "terminal.run",
            serde_json::json!({ "command": "pwd", "cwd": "/etc" }),
            &c,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::ScopeViolation { .. }), "{err:?}");

    rt.shutdown().await;
}

/// Python sandbox runs confined scripts; a filesystem escape attempt is
/// contained when the bubblewrap backend is available.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_sandbox_runs_and_contains_escape() {
    let ws = tmp_dir("ws-sb");
    let config = Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws.clone()),
            python_sandbox: Some(PythonSandboxConfig {
                wall_timeout_secs: Some(6),
                ..PythonSandboxConfig::default()
            }),
            ..RuntimeConfig::default()
        },
        permissions: allow_rules(&["python.run"]),
        ..Config::default()
    };
    let rt = build(config).await;
    let (_, _, c) = tool_ctx(&rt).await;

    let out = rt
        .executor
        .execute(
            "python.run",
            serde_json::json!({ "script": "print('hi from sb')" }),
            &c,
        )
        .await
        .unwrap();
    let rendered = out.render();
    assert!(rendered.contains("exit_code"), "{}", rendered);

    // Escape attempts must not write outside the sandbox.
    let escape = rt
        .executor
        .execute(
            "python.run",
            serde_json::json!({
                "script": "\
try:
    open('/etc/openrt-pwned','w').write('x')
    print('WROTE_ETC')
except Exception as e:
    print('denied:', type(e).__name__)"
            }),
            &c,
        )
        .await
        .unwrap();
    let rendered = escape.render();
    assert!(
        !rendered.contains("WROTE_ETC"),
        "escape succeeded: {rendered}"
    );
    assert!(!std::path::Path::new("/etc/openrt-pwned").exists());
    rt.shutdown().await;
}

/// The developer profile gates dangerous tools instead of hiding them:
/// mutations are `Ask` — fail-closed with no resolver, executed only after
/// an approval — while unlisted tools remain deny-by-default even with an
/// approving resolver (deny-by-default never becomes allow-by-registration).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dangerous_tools_are_approval_gated_and_unlisted_tools_denied() {
    let ws = tmp_dir("ws-deny");
    let config = Config {
        runtime: RuntimeConfig {
            workspace_dir: Some(ws.clone()),
            ..RuntimeConfig::default()
        },
        ..Config::default()
    };
    let dangerous = [
        "filesystem.write",
        "filesystem.delete",
        "terminal.run",
        "python.run",
    ];
    let args_for = |tool: &str| match tool {
        "filesystem.write" => serde_json::json!({ "path": ws.join("x").display().to_string() }),
        "filesystem.delete" => serde_json::json!({ "path": ws.join("x").display().to_string() }),
        "terminal.run" => serde_json::json!({ "command": "id" }),
        "python.run" => serde_json::json!({ "script": "print(1)" }),
        _ => unreachable!(),
    };

    // 1. Ask without a resolver: every dangerous tool is refused at the
    //    gate, before execution, with no side effects.
    let rt = build_no_resolver(config.clone()).await;
    let (_, _, c) = tool_ctx(&rt).await;
    for tool in dangerous {
        let err = rt
            .executor
            .execute(tool, args_for(tool), &c)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::PermissionDenied { .. }),
            "{tool} must fail closed without a resolver, got {err:?}"
        );
    }
    assert!(
        !ws.join("x").exists(),
        "no side effect may happen before approval"
    );
    rt.shutdown().await;

    // 2. With an approving resolver the same tools execute — approval, not
    //    registration, is what opens the gate.
    let rt = build(config).await;
    let (_, _, c) = tool_ctx(&rt).await;
    for tool in dangerous {
        let out = rt.executor.execute(tool, args_for(tool), &c).await.unwrap();
        assert!(
            matches!(out, openrt_tools::ToolOutput::Json(_)),
            "{tool} must execute after approval, got {out:?}"
        );
    }

    // 3. Unlisted tool: still deny-by-default — the approving resolver must
    //    never turn an unlisted tool into an allowed one.
    let err = rt
        .executor
        .execute("database.query", serde_json::json!({}), &c)
        .await
        .unwrap_err();
    match err {
        ToolError::PermissionDenied { reason, .. } => {
            assert!(reason.contains("policy"), "{reason}")
        }
        other => panic!("unlisted tool must stay deny-by-default, got {other:?}"),
    }
    rt.shutdown().await;
}
