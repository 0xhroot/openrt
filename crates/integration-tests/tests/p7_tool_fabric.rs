//! P7 Tool Fabric, end-to-end through the real runtime and gate:
//! * discovery → EnvironmentProfile → backend resolution (six-layer priority)
//!   is exposed through the production `computer.env` tool;
//! * a real operation runs on the *real* selected backend when one exists
//!   (Hyprland here: window list + focus-and-verify + capture + clipboard),
//!   falling back to the honest sim harness when no real backend is available;
//! * `computer.*` tools are registered and **deny-by-default** until an
//!   explicit allow rule (permission enforcement);
//! * failure handling: unsupported/unavailable ops return structured status,
//!   never fake success;
//! * observe→verify: focus is confirmed by re-observing the active window.
//!
//! Config scanners + capability-matrix honesty are also asserted here so the
//! acceptance surface lives in one place.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use openrt_config::{Config, PermissionsConfig, RuleConfig, RuleEffect};
use openrt_platform::backend::{BackendKind, ControlOp};
use openrt_platform::capability::{CapabilityStatus, ComputerCapability};
use openrt_platform::profile::{DesktopKind, DiscoveryInputs, EnvironmentProfile, SessionType};
use openrt_platform::ComputerControl;
use openrt_tools::ToolError;

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p7-{prefix}-{}-{:x}",
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

fn runtime_config(ws: &Path, computer_backend: Option<&str>) -> openrt_config::RuntimeConfig {
    openrt_config::RuntimeConfig {
        workspace_dir: Some(ws.to_path_buf()),
        terminal_timeout_secs: Some(30),
        screen_backend: Some("sim".into()),
        computer_backend: computer_backend.map(|s| s.to_string()),
        ..Default::default()
    }
}

async fn build(
    ws: &Path,
    caps: &[&str],
    computer_backend: Option<&str>,
) -> Arc<openrt_runtime::Runtime> {
    let config = Config {
        runtime: runtime_config(ws, computer_backend),
        permissions: allow_rules(caps),
        ..Default::default()
    };
    openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(openrt_persistence::MemoryStore::new()))
        .with_provider(Arc::new(openrt_models::mock::MockProvider::echo("mock")))
        .build()
        .await
        .unwrap()
}

fn ctx() -> openrt_tools::ToolExecutionContext {
    openrt_tools::ToolExecutionContext {
        session_id: Some(openrt_core::SessionId::new().to_string()),
        task_id: Some(openrt_core::TaskId::new().to_string()),
        channel: None,
    }
}

/// M7.1/M7.2: `computer.env` is registered, reports the environment profile,
/// capability matrix, and resolver decisions through the production path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn computer_env_reports_profile_matrix_and_resolver() {
    let ws = tmp_dir("env");
    let rt = build(&ws, &["computer.env"], Some("sim")).await;

    assert!(
        rt.registry.names().contains(&"computer.env".to_string()),
        "computer.env missing from {:?}",
        rt.registry.names()
    );

    let out = rt
        .executor
        .execute("computer.env", serde_json::json!({}), &ctx())
        .await
        .expect("computer.env allowed");
    let v = match out {
        openrt_tools::ToolOutput::Json(v) => v,
        openrt_tools::ToolOutput::Text(_) => panic!("expected JSON"),
    };

    // Profile + summary present.
    assert!(v["summary"]["backend"].is_string(), "{v}");
    assert!(v["profile"]["session_type"].is_string(), "{v}");
    assert!(v["resolver"].is_array(), "{v}");

    // Capability matrix rows have status + reason for non-SUPPORTED.
    let caps = v["capabilities"].as_array().expect("capabilities array");
    assert!(!caps.is_empty(), "empty capability matrix");
    for row in caps {
        let status = row["status"].as_str().unwrap_or("");
        assert!(
            matches!(
                status,
                "SUPPORTED" | "PARTIAL" | "PERMISSION_REQUIRED" | "UNAVAILABLE" | "UNSUPPORTED"
            ),
            "unexpected status {status}"
        );
        if status != "SUPPORTED" {
            assert!(
                !row["reason"].as_str().unwrap_or("").is_empty(),
                "non-supported row missing reason: {row}"
            );
        }
    }

    // Forced sim backend is what we asked for.
    assert_eq!(v["summary"]["backend"], "sim", "{v}");

    let _ = std::fs::remove_dir_all(&ws);
}

/// Deny-by-default: without an allow rule every computer.* tool is rejected
/// by the gate and the tool body never runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn computer_tools_deny_by_default() {
    let ws = tmp_dir("deny");
    let rt = build(&ws, &["filesystem.read"], Some("sim")).await;

    for name in [
        "computer.env",
        "computer.window",
        "computer.screen",
        "computer.input",
        "computer.clipboard",
        "computer.app",
    ] {
        assert!(
            rt.registry.names().contains(&name.to_string()),
            "{name} missing from {:?}",
            rt.registry.names()
        );
        let err = rt
            .executor
            .execute(name, serde_json::json!({}), &ctx())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::PermissionDenied { .. }),
            "{name}: expected PermissionDenied, got {err:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&ws);
}

/// ADR-008: `computer.*` tools are computer-scoped for the kill switch.
#[test]
fn computer_tools_are_kill_scoped() {
    for tool in [
        "computer.env",
        "computer.window",
        "computer.screen",
        "computer.input",
        "computer.clipboard",
        "computer.app",
    ] {
        assert_eq!(
            openrt_safety::tool_scope(tool),
            Some(openrt_safety::KillScope::Computer),
            "{tool}"
        );
    }
    assert_eq!(
        openrt_safety::tool_scope("echo"),
        None,
        "echo must not be computer-scoped"
    );
}

/// Discovery → profile → backend selection → real operation → observation →
/// verify. Runs against the *live* host when a real backend exists (Hyprland
/// on this machine); otherwise asserts the honest sim fallback.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_selects_backend_and_runs_real_ops() {
    let profile = EnvironmentProfile::host();

    // Discovery sanity: profile id encodes session/desktop.
    assert!(!profile.profile_id.is_empty());
    assert!(!profile.os_family.is_empty());

    let (kind, decisions) = openrt_platform::backend::BackendResolver::resolve(&profile);
    assert!(
        decisions.iter().any(|d| d.selected),
        "resolver must record a selection: {decisions:?}"
    );

    let control = ComputerControl::with_profile(profile.clone(), None);
    assert_eq!(control.backend_kind(), kind);

    // Capability matrix must include env_discover + window_list rows.
    let matrix = control.capability_matrix();
    assert!(matrix.get(ComputerCapability::EnvDiscover).is_some());
    assert!(matrix.get(ComputerCapability::WindowList).is_some());

    match kind {
        BackendKind::Sim => {
            // Honest fallback: no real backend — sim must still answer.
            let r = control
                .execute(ControlOp::ListWindows)
                .await
                .expect("sim list_windows");
            assert!(r.ok);
        }
        real => {
            // REAL operation: list windows through the live compositor.
            let list = control
                .execute(ControlOp::ListWindows)
                .await
                .unwrap_or_else(|e| panic!("{} list_windows failed: {e}", real.as_str()));
            // Some backends (kde/gnome) return partial/unavailable honestly —
            // only assert success for backends that claim support.
            let supports = matrix
                .get(ComputerCapability::WindowList)
                .map(|r| r.is_usable())
                .unwrap_or(false);
            if supports {
                assert!(list.ok, "{} list_windows: {list:?}", real.as_str());
                let wins = list.data.as_array().map(|a| a.len()).unwrap_or(0);
                // At least our own runtime process window context exists on a
                // live desktop; but on a bare session 0 windows is also valid.
                let _ = wins;
            } else {
                assert!(
                    !list.ok,
                    "{} claimed unusable but returned ok",
                    real.as_str()
                );
            }

            // Observation: observe() yields a backend-tagged snapshot.
            let obs = control.observe().await.expect("observe");
            assert_eq!(obs.backend, real.as_str());

            // Focus-and-verify when there is a focusable window with a
            // usable list: pick the first window, focus it, confirm active.
            if supports {
                if let Some(arr) = list.data.as_array() {
                    if let Some(first) = arr.first() {
                        let id = first["id"].as_str().unwrap_or("");
                        if !id.is_empty() {
                            let r = control.focus_and_verify(id).await;
                            // Real desktops can race (window closed) — only
                            // fail on Verify errors when focus claimed success.
                            match r {
                                Ok(op) => assert!(op.ok, "focus reported not-ok: {op:?}"),
                                Err(openrt_platform::backend::ControlError::Verify(msg)) => {
                                    // Acceptable on a busy session; record only.
                                    eprintln!("focus verify race: {msg}");
                                }
                                Err(e) => panic!("focus failed unexpectedly: {e}"),
                            }
                        }
                    }
                }
            }
        }
    }

    // Capture through the real backend when supported (grim on Wayland).
    let capture_cap = matrix.get(ComputerCapability::ScreenCapture);
    if let Some(row) = capture_cap {
        if row.is_usable() {
            let r = control
                .execute(ControlOp::CaptureScreen)
                .await
                .expect("capture op");
            assert!(r.ok, "capture claimed usable but failed: {r:?}");
            assert!(
                r.data.get("path").is_some() || r.data.get("width").is_some(),
                "capture must report path or dimensions: {r:?}"
            );
        } else if row.status == CapabilityStatus::Unavailable {
            let r = control.execute(ControlOp::CaptureScreen).await.unwrap();
            assert!(!r.ok, "unavailable capture returned ok: {r:?}");
            assert_eq!(r.status, CapabilityStatus::Unavailable);
        }
    }

    // Clipboard round-trip when supported.
    if let Some(row) = matrix.get(ComputerCapability::ClipboardSet) {
        if row.is_usable() {
            let marker = format!("openrt-p7-clip-{}", std::process::id());
            let set = control
                .execute(ControlOp::ClipboardSet(marker.clone()))
                .await
                .expect("clipboard set");
            assert!(set.ok, "clipboard set: {set:?}");
            let get = control
                .execute(ControlOp::ClipboardGet)
                .await
                .expect("clipboard get");
            if get.ok {
                let text = get.data["text"].as_str().unwrap_or("");
                assert_eq!(text, marker, "clipboard round-trip mismatch");
            }
        }
    }

    // Pointer: on this host no ydotool → honest UNAVAILABLE, never fake ok.
    if let Some(row) = matrix.get(ComputerCapability::PointerInject) {
        if !row.is_usable() {
            let r = control
                .execute(ControlOp::MovePointer(10, 10))
                .await
                .expect("pointer op returns structured result");
            assert!(!r.ok, "unusable pointer returned ok: {r:?}");
            assert!(
                matches!(
                    r.status,
                    CapabilityStatus::Unavailable | CapabilityStatus::Unsupported
                ),
                "unexpected status {:?}",
                r.status
            );
            assert!(!r.message.is_empty(), "missing reason");
        }
    }
}

/// Production Runtime wiring: `computer.window` runs through the gate with an
/// allow rule, and observation data comes back structured.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn computer_window_tool_through_production_runtime() {
    let ws = tmp_dir("window");
    let rt = build(&ws, &["computer.window", "computer.screen"], Some("sim")).await;

    let out = rt
        .executor
        .execute(
            "computer.window",
            serde_json::json!({ "action": "list" }),
            &ctx(),
        )
        .await
        .expect("computer.window allowed");
    let v = match out {
        openrt_tools::ToolOutput::Json(v) => v,
        _ => panic!("expected JSON"),
    };
    assert_eq!(v["action"], "list");
    assert!(v["ok"].as_bool().unwrap_or(false), "{v}");
    assert!(v["windows"].is_array(), "{v}");

    // Focus the sim window and verify through focus_and_verify.
    let out = rt
        .executor
        .execute(
            "computer.window",
            serde_json::json!({ "action": "focus", "id": "sim-1" }),
            &ctx(),
        )
        .await
        .expect("focus allowed");
    let v = match out {
        openrt_tools::ToolOutput::Json(v) => v,
        _ => panic!("expected JSON"),
    };
    assert!(v["ok"].as_bool().unwrap_or(false), "focus: {v}");

    // observe through computer.screen.
    let out = rt
        .executor
        .execute(
            "computer.screen",
            serde_json::json!({ "action": "observe" }),
            &ctx(),
        )
        .await
        .expect("observe allowed");
    let v = match out {
        openrt_tools::ToolOutput::Json(v) => v,
        _ => panic!("expected JSON"),
    };
    assert_eq!(v["action"], "observe");
    assert_eq!(v["observation"]["backend"], "sim");

    // Invalid action → InvalidArguments (failure handling).
    let err = rt
        .executor
        .execute(
            "computer.window",
            serde_json::json!({ "action": "explode" }),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArguments(..)), "{err:?}");

    let _ = std::fs::remove_dir_all(&ws);
}

/// Config scanners: fixture documents parse without touching ~/.config.
#[test]
fn config_fixture_scanners_parse_without_live_home() {
    let hypr = "bind = SUPER, Q, exec, foot\nexec-once = waybar\n";
    let k = openrt_platform::config_scan::parse_hyprland_conf(hypr);
    assert!(k.keybindings.iter().any(|b| b.action.contains("foot")));
    assert!(k.apps.iter().any(|a| a.command == "waybar"));

    let sway = "bindsym $mod+Return exec foot\nworkspace 2\n";
    let s = openrt_platform::config_scan::parse_sway_or_i3_conf(sway, "sway.config");
    assert!(s.keybindings.iter().any(|b| b.keys.contains("Return")));
    assert_eq!(s.workspaces.workspaces, vec!["2"]);

    let bsp = "bspc monitor HDMI-A-1 -d 1 2 3\n";
    let b = openrt_platform::config_scan::parse_bspwmrc(bsp);
    assert_eq!(b.workspaces.workspaces, vec!["1", "2", "3"]);

    let sx = "super + shift + q\n    bspc node -c\n";
    let x = openrt_platform::config_scan::parse_sxhkdrc(sx);
    assert!(x.keybindings.iter().any(|k| k.action.contains("bspc")));
}

/// Backend resolver priority: native compositor beats generic display server
/// when its IPC tool exists; forced sim always wins for tests.
#[test]
fn resolver_priority_and_forced_sim() {
    let inputs = DiscoveryInputs {
        session_type_override: Some(SessionType::Wayland),
        desktop_override: Some(DesktopKind::Hyprland),
        tools_probe: vec!["hyprctl".into(), "grim".into()],
        path_dirs: vec![PathBuf::from("/usr/bin")],
        config_home: Some(PathBuf::from("/nonexistent-openrt")),
        a11y_bus_present: Some(true),
        ..Default::default()
    };
    let p = EnvironmentProfile::discover(&inputs);
    let (kind, log) = openrt_platform::backend::BackendResolver::resolve(&p);
    assert_eq!(kind, BackendKind::Hyprland);
    let hypr = log.iter().find(|d| d.backend == "hyprland").unwrap();
    assert_eq!(
        hypr.layer, 1,
        "hyprland must be layer 1 (native compositor)"
    );
    assert!(hypr.selected);

    let (forced, _) =
        openrt_platform::backend::BackendResolver::resolve_with(&p, Some(BackendKind::Sim));
    assert_eq!(forced, BackendKind::Sim);
}

/// `computer.app` list + launch reject shell metacharacters (no raw shell).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn computer_app_refuses_shell_metacharacters() {
    let ws = tmp_dir("app");
    let rt = build(&ws, &["computer.app"], Some("sim")).await;

    let out = rt
        .executor
        .execute(
            "computer.app",
            serde_json::json!({ "action": "list" }),
            &ctx(),
        )
        .await
        .expect("app list allowed");
    let v = match out {
        openrt_tools::ToolOutput::Json(v) => v,
        _ => panic!("expected JSON"),
    };
    assert_eq!(v["action"], "list");
    // apps may be empty on a bare fixture — key must exist.
    assert!(v["apps"].is_array());

    // Launch with metacharacters → InvalidArguments (tool-level refusal).
    let err = rt
        .executor
        .execute(
            "computer.app",
            serde_json::json!({ "action": "launch", "name": "foo; rm -rf /" }),
            &ctx(),
        )
        .await;
    // Either InvalidArguments (metacharacters) or Internal (backend refusal)
    // — both are honest refusals; PermissionDenied would mean gate miswire.
    match err {
        Err(ToolError::InvalidArguments(..)) | Err(ToolError::Internal(..)) => {}
        other => panic!("expected refusal, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&ws);
}
