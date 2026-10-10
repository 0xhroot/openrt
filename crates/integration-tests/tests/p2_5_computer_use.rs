//! P2.5 computer use, end-to-end through the real runtime and gate:
//! * M2.5 tools (screen capture, deterministic grounding, mouse, keyboard,
//!   window-level browser) are registered behind permissions;
//! * the computer-use loop completes a controlled "fill and submit a form"
//!   task against the deterministic in-memory screen;
//! * the ADR-008 computer-use kill switch halts a terminal **mid-action**,
//!   blocks subsequent computer input, and is audited on the bus;
//! * a computer-scope HALT does not touch other subsystems (echo/filesystem).
//!
//! All tests run headless against the `SimScreen` backend (the roadmap's
//! "no real display needed" harness).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use openrt_computer::ScreenElement;
use openrt_config::{Config, PermissionsConfig, RuleConfig, RuleEffect};
use openrt_core::EventKind;
use openrt_telemetry::ChannelSink;
use openrt_tools::ToolError;

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p2_5-{prefix}-{}-{:x}",
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
    // Gate enforces `required_caps` (e.g. `terminal.run.all`) separately.
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

fn runtime_config(ws: &Path) -> openrt_config::RuntimeConfig {
    openrt_config::RuntimeConfig {
        workspace_dir: Some(ws.to_path_buf()),
        terminal_timeout_secs: Some(30),
        screen_backend: Some("sim".into()),
        ..Default::default()
    }
}

async fn build(ws: &Path, caps: &[&str]) -> (Arc<openrt_runtime::Runtime>, Arc<ChannelSink>) {
    let sink = Arc::new(ChannelSink::new());
    let config = Config {
        runtime: runtime_config(ws),
        permissions: allow_rules(caps),
        ..Default::default()
    };
    let rt = openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(openrt_persistence::MemoryStore::new()))
        .sink(sink.clone())
        .with_provider(Arc::new(openrt_models::mock::MockProvider::tool_sequence(
            "mock",
        )))
        .build()
        .await
        .unwrap();
    (rt, sink)
}

/// Poll the sink until at least `n` `SafetyHalt` events are visible (the bus
/// dispatcher drains asynchronously, so a fixed sleep can miss late events).
async fn wait_for_halts(sink: &ChannelSink, n: usize) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let count = sink
            .snapshot()
            .into_iter()
            .filter(|e| e.kind == EventKind::SafetyHalt)
            .count();
        if count >= n {
            return true;
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

fn ctx() -> openrt_tools::ToolExecutionContext {
    openrt_tools::ToolExecutionContext {
        session_id: Some(openrt_core::SessionId::new().to_string()),
        task_id: Some(openrt_core::TaskId::new().to_string()),
        channel: None,
    }
}

/// A scripted desktop for the harness: a form window with a name field and a
/// submit button, at deterministic geometry.
fn seed_screen(rt: &openrt_runtime::Runtime) {
    let sim = rt
        .screen
        .as_any()
        .downcast_ref::<openrt_computer::SimScreen>()
        .expect("sim backend configured");
    for e in [
        ScreenElement::new("session-form", "window", 20, 40, 600, 400),
        ScreenElement::new("name-field", "field", 40, 90, 260, 24),
        ScreenElement::new("submit", "button", 40, 140, 120, 32),
    ] {
        sim.add_element(e);
    }
}

/// M2.5 tools are registered when a workspace is configured, and every one is
/// deny-by-default until an explicit allow rule.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn computer_tools_are_registered_and_deny_by_default() {
    let ws = tmp_dir("reg");
    let (rt, _sink) = build(&ws, &["filesystem.read"]).await;
    for name in [
        "screen.capture",
        "screen.ground",
        "input.mouse",
        "input.keyboard",
        "browser.list",
        "browser.activate",
        "terminal.run",
        "python.run",
    ] {
        assert!(
            rt.registry.names().contains(&name.to_string()),
            "{name} missing from {:#?}",
            rt.registry.names()
        );
    }
    // No allow rule for input.mouse → PermissionDenied, tool never runs.
    let err = rt
        .executor
        .execute(
            "input.mouse",
            serde_json::json!({ "action": "click", "x": 1, "y": 1 }),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::PermissionDenied { .. }));
    let _ = std::fs::remove_dir_all(&ws);
}

/// The computer-use loop completes a controlled task on a headless harness:
/// focus the form, ground a field by name, type into it, click submit.
/// (DoD: "fill and submit a local form" in a sandboxed test environment.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grounding_plus_input_completes_a_fixture() {
    let ws = tmp_dir("loop");
    let (rt, _sink) = build(
        &ws,
        &[
            "input.mouse",
            "input.keyboard",
            "screen.ground",
            "browser.activate",
        ],
    )
    .await;
    seed_screen(&rt);

    // Scripted plan, like an agent would produce from the grounded snapshot:
    // 1. activate the form window, 2. click the name field, 3. type a name,
    // 4. click submit, 5. done.
    let step = Arc::new(AtomicUsize::new(0));
    let step2 = step.clone();
    let mut looped = rt.computer_loop(6, move |cap| {
        let s = step2.fetch_add(1, Ordering::SeqCst);
        match s {
            0 => {
                let _ = cap
                    .ground("session-form")
                    .first()
                    .cloned()
                    .expect("form grounded");
                Some(openrt_computer::ToolCall::new(
                    "browser.activate",
                    serde_json::json!({ "window": "session-form" }),
                ))
            }
            1 => {
                let f = cap
                    .ground("name-field")
                    .first()
                    .cloned()
                    .expect("field grounded");
                let (x, y) = f.center();
                Some(openrt_computer::ToolCall::new(
                    "input.mouse",
                    serde_json::json!({ "action": "click", "x": x, "y": y }),
                ))
            }
            2 => Some(openrt_computer::ToolCall::new(
                "input.keyboard",
                serde_json::json!({ "action": "type", "text": "Ada" }),
            )),
            3 => {
                let b = cap
                    .ground("submit")
                    .first()
                    .cloned()
                    .expect("submit grounded");
                let (x, y) = b.center();
                Some(openrt_computer::ToolCall::new(
                    "input.mouse",
                    serde_json::json!({ "action": "click", "x": x, "y": y }),
                ))
            }
            _ => None,
        }
    });
    let report = looped.run().await.expect("loop completes the fixture");
    assert!(report.completed, "loop should finish within budget");
    assert_eq!(report.steps, 4);
    assert_eq!(report.actions.len(), 4);
    let tools: Vec<&str> = report.actions.iter().map(|a| a.tool.as_str()).collect();
    assert_eq!(
        tools,
        vec![
            "browser.activate",
            "input.mouse",
            "input.keyboard",
            "input.mouse"
        ]
    );

    // The scripted screen recorded the exact interactions.
    let sim = rt
        .screen
        .as_any()
        .downcast_ref::<openrt_computer::SimScreen>()
        .unwrap();
    let clicks = sim.pointer_clicks();
    assert!(clicks.contains(&"name-field".to_string()), "{clicks:?}");
    assert!(clicks.contains(&"submit".to_string()), "{clicks:?}");
    assert!(
        sim.typed()
            .iter()
            .any(|t| t.starts_with("name-field:") && t.contains("Ada")),
        "{:?}",
        sim.typed()
    );
    let focused = sim.focused_windows();
    assert!(focused.iter().any(|w| w == "session-form"), "{focused:?}");
    let _ = std::fs::remove_dir_all(&ws);
}

/// ADR-008: a HALT halts a long-running terminal command mid-action, blocks
/// subsequent computer input, and the SafetyHalt signal is emitted and
/// audited on the bus.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_switch_halts_terminal_mid_action_and_is_audited() {
    let ws = tmp_dir("kill");
    let (rt, sink) = build(&ws, &["terminal.run", "input.mouse"]).await;

    // Start a long-running command on a background task.
    let exec = rt.executor.clone();
    let task_ctx = ctx();
    let handle = tokio::spawn(async move {
        exec.execute(
            "terminal.run",
            serde_json::json!({ "command": "sleep 300" }),
            &task_ctx,
        )
        .await
    });

    // Wait for the command to be running, then trip the switch.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let started = std::time::Instant::now();
    assert!(rt.halt("operator emergency").await.unwrap());

    // The in-flight command is killed mid-action, promptly.
    let err = handle.await.unwrap().unwrap_err();
    assert!(
        matches!(&err, ToolError::KillSwitchEngaged { reason, .. } if reason.contains("operator emergency")),
        "{err:?}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "halt must be prompt, took {:?}",
        started.elapsed()
    );

    // Subsequent computer input is blocked by the gate before it can run.
    let err = rt
        .executor
        .execute(
            "input.mouse",
            serde_json::json!({ "action": "move", "x": 1, "y": 1 }),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::KillSwitchEngaged { .. }));
    assert_eq!(rt.halted().as_deref(), Some("operator emergency"));

    // Audited: the HALT signal and the blocked-reaction denial are on the bus.
    assert!(
        wait_for_halts(&sink, 2).await,
        "expected trip + at least one blocked reaction on the bus"
    );
    let halts: usize = sink
        .snapshot()
        .into_iter()
        .filter(|e| e.kind == EventKind::SafetyHalt)
        .count();
    assert!(
        halts >= 2,
        "trip + at least one blocked reaction, got {halts}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// ADR-008 scoping discipline: a computer HALT leaves other subsystems fully
/// operational and never triggers their mitigations.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_halt_leaves_other_subsystems_alone() {
    let ws = tmp_dir("scope");
    let (rt, _sink) = build(&ws, &["filesystem.read", "input.mouse", "echo"]).await;
    seed_screen(&rt);
    assert!(rt.halt("compute only").await.unwrap());

    // Computer scope: blocked.
    let err = rt
        .executor
        .execute(
            "input.mouse",
            serde_json::json!({ "action": "click", "x": 1, "y": 1 }),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::KillSwitchEngaged { .. }));

    // Other subsystems: untouched.
    let out = rt
        .executor
        .execute("echo", serde_json::json!({ "text": "still here" }), &ctx())
        .await
        .unwrap();
    assert_eq!(out.render(), "still here");

    // And after an operator-verified all-clear, computer tools work again.
    rt.clear_halt();
    assert_eq!(rt.halted(), None);
    let out = rt
        .executor
        .execute(
            "screen.ground",
            serde_json::json!({ "target": "submit" }),
            &ctx(),
        )
        .await;
    // screen.ground is allow-listed via filesystem.read? No — it has no allow
    // rule here, so expect PermissionDenied, NOT a kill-switch error: the
    // clear switch must not leave computer tools half-open.
    let err = out.unwrap_err();
    assert!(matches!(err, ToolError::PermissionDenied { .. }));
    let _ = std::fs::remove_dir_all(&ws);
}
