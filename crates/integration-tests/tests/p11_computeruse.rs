//! Phase 11 — ComputerUse integration tests 25–36.
//!
//! Track I–S (M11.8 + M11.9 + M11.10):
//! * observation bundle (screenshot + window metadata + elements + clipboard
//!   ON DEMAND + focus + cursor + profile) with twin generation;
//! * semantic target resolution chain (accessibility → window tree →
//!   structured elements → vision → coordinate) with confidence/source/
//!   bounds/window/timestamp and a full chain trace;
//! * digital twin invalidated after EVERY action (in-memory, never a store);
//! * action execution through the one ComputerControl/screen authority with
//!   advisory prediction and BOUNDED verification (≤3, fail closed);
//! * `computer.use` registered, deny-by-default, gated + audited through the
//!   ONE gate; VLM grounds only — never executes;
//! * M11.9: typed/clipboard text scanned command-shaped through the shield,
//!   with findings that never carry the raw payload.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use openrt_audit::AuditQuery;
use openrt_computer::{
    ComputerAction, ComputerUse, ComputerUseError, ScreenBackend, ScreenCapture, ScreenElement,
    VisionGrounding,
};
use openrt_config::{Config, PermissionsConfig, RuleConfig, RuleEffect};
use openrt_models::mock::MockProvider;
use openrt_persistence::MemoryStore;
use openrt_platform::backend::{
    BackendKind, ComputerBackend, ControlError, ControlOp, Observation, OperationResult, WindowRef,
};
use openrt_platform::capability::CapabilityReport;
use openrt_platform::{ComputerControl, EnvironmentProfile};
use openrt_runtime::RuntimeBuilder;
use openrt_shield::detectors::ShieldEvaluator;
use openrt_shield::ShieldContext;
use openrt_tools::{ToolError, ToolExecutionContext, ToolOutput};

// ---- helpers ----

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p11cu-{prefix}-{}-{:x}",
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
    let rules: Vec<RuleConfig> = caps
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
    PermissionsConfig { rules }
}

/// Deterministic runtime over the sim screen + sim computer backend.
async fn build_rt(caps: &[&str]) -> Arc<openrt_runtime::Runtime> {
    let config = Config {
        runtime: openrt_config::RuntimeConfig {
            screen_backend: Some("sim".into()),
            computer_backend: Some("sim".into()),
            ..Default::default()
        },
        permissions: allow_rules(caps),
        ..Default::default()
    };
    RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(MemoryStore::new()))
        .with_provider(Arc::new(MockProvider::echo("mock")))
        .build()
        .await
        .unwrap()
}

fn ctx() -> ToolExecutionContext {
    ToolExecutionContext {
        session_id: None,
        task_id: None,
        channel: None,
    }
}

fn sim_control() -> Arc<ComputerControl> {
    Arc::new(ComputerControl::with_profile(
        EnvironmentProfile::host(),
        Some(BackendKind::Sim),
    ))
}

/// Deterministic screen fixture: fixed geometry/elements, optional raster
/// image for the vision branch, no real display.
struct FixtureScreen {
    width: u64,
    height: u64,
    elements: Vec<ScreenElement>,
    image: Option<PathBuf>,
    windows: Vec<String>,
}

impl FixtureScreen {
    fn new(width: u64, height: u64) -> Self {
        Self {
            width,
            height,
            elements: Vec::new(),
            image: None,
            windows: Vec::new(),
        }
    }

    fn with_elements(mut self, elements: Vec<ScreenElement>) -> Self {
        self.elements = elements;
        self
    }

    fn with_image(mut self, path: PathBuf) -> Self {
        self.image = Some(path);
        self
    }

    fn snapshot(&self) -> ScreenCapture {
        ScreenCapture {
            backend: "fixture".into(),
            width: self.width,
            height: self.height,
            elements: self.elements.clone(),
            image_path: self.image.clone(),
        }
    }
}

#[async_trait]
impl ScreenBackend for FixtureScreen {
    fn kind(&self) -> &'static str {
        "fixture"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn width(&self) -> u64 {
        self.width
    }

    fn height(&self) -> u64 {
        self.height
    }

    async fn elements(&self) -> Result<Vec<ScreenElement>, ToolError> {
        Ok(self.elements.clone())
    }

    async fn capture(&self) -> Result<ScreenCapture, ToolError> {
        Ok(self.snapshot())
    }

    async fn pointer_move(&self, _x: i64, _y: i64) -> Result<(), ToolError> {
        Ok(())
    }

    async fn pointer_click(&self, _x: i64, _y: i64, _button: u32) -> Result<(), ToolError> {
        Ok(())
    }

    async fn pointer_drag(&self, _from: (i64, i64), _to: (i64, i64)) -> Result<(), ToolError> {
        Ok(())
    }

    async fn keyboard_type(&self, _text: &str) -> Result<(), ToolError> {
        Ok(())
    }

    async fn keyboard_key(&self, _key: &str) -> Result<(), ToolError> {
        Ok(())
    }

    async fn window_list(&self) -> Result<Vec<String>, ToolError> {
        Ok(self.windows.clone())
    }

    async fn window_focus(&self, _name: &str) -> Result<(), ToolError> {
        Ok(())
    }
}

fn fixture_screen(f: &FixtureScreen) -> Arc<dyn ScreenBackend> {
    Arc::new(FixtureScreen {
        width: f.width,
        height: f.height,
        elements: f.elements.clone(),
        image: f.image.clone(),
        windows: f.windows.clone(),
    })
}

/// Mock vision backend: counts calls, returns a fixed reply (never touches
/// a real VLM — offline determinism).
struct MockVision {
    calls: AtomicUsize,
    reply: String,
}

impl MockVision {
    fn new(reply: &str) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            reply: reply.into(),
        }
    }
}

#[async_trait]
impl VisionGrounding for MockVision {
    async fn ground(&self, _image: &[u8], _target: &str) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.reply.clone())
    }
}

/// Backend whose focus "succeeds" but never becomes observable — drives the
/// bounded (≤3) verification-mismatch path to a hard `Verify` error.
struct StaleFocusBackend;

#[async_trait]
impl ComputerBackend for StaleFocusBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Sim
    }

    fn capabilities(&self) -> Vec<CapabilityReport> {
        Vec::new()
    }

    async fn execute(&self, op: ControlOp) -> Result<OperationResult, ControlError> {
        match op {
            ControlOp::ListWindows => Ok(OperationResult::ok(
                "list_windows",
                json!([WindowRef {
                    id: "w1".into(),
                    app_id: None,
                    pid: None,
                    title: Some("Stale Window".into()),
                    workspace: Some("1".into()),
                    focused: false,
                }]),
            )),
            ControlOp::FocusWindow(_) => Ok(OperationResult::ok(
                "focus_window",
                json!({ "focused": "w1" }),
            )),
            other => Err(ControlError::Unsupported(other_name(&other))),
        }
    }

    async fn observe(&self) -> Result<Observation, ControlError> {
        Ok(Observation {
            backend: "stale".into(),
            active_window: None,
            window_count: 1,
            monitors: Vec::new(),
            cursor: None,
            captured_at_ms: 0,
        })
    }
}

fn other_name(op: &ControlOp) -> String {
    format!("{op:?}")
}

/// Backend whose pointer mechanism exists but is unavailable on this host
/// (no ydotool …) — the honest UNAVAILABLE refusal, never a fake success.
struct UnavailablePointerBackend;

#[async_trait]
impl ComputerBackend for UnavailablePointerBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Sim
    }

    fn capabilities(&self) -> Vec<CapabilityReport> {
        Vec::new()
    }

    async fn execute(&self, op: ControlOp) -> Result<OperationResult, ControlError> {
        match op {
            ControlOp::MovePointer(..) | ControlOp::ClickPointer => Ok(
                OperationResult::unavailable("move_pointer", "no pointer mechanism on this host"),
            ),
            other => Ok(OperationResult::ok(other_name(&other), json!({}))),
        }
    }

    async fn observe(&self) -> Result<Observation, ControlError> {
        Ok(Observation {
            backend: "stale".into(),
            active_window: None,
            window_count: 0,
            monitors: Vec::new(),
            cursor: None,
            captured_at_ms: 0,
        })
    }
}

fn engine(screen: Arc<dyn ScreenBackend>, control: Arc<ComputerControl>) -> ComputerUse {
    ComputerUse::new(screen, control)
}

// ---- tests ----

/// TEST 25: the observation bundle assembles every required facet and the
/// clipboard is read ON DEMAND only (never implicitly).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t25_observation_bundle_facets_and_clipboard_on_demand() {
    let screen =
        fixture_screen(
            &FixtureScreen::new(1280, 720).with_elements(vec![ScreenElement::new(
                "Fixture Window",
                "window",
                10,
                10,
                600,
                400,
            )]),
        );
    let e = engine(screen, sim_control());

    let b1 = e.observe(false).await.expect("bundle 1");
    assert_eq!(b1.screen_backend, "fixture");
    assert_eq!(b1.width, 1280);
    assert_eq!(b1.height, 720);
    assert_eq!(b1.elements.len(), 1);
    assert_eq!(b1.twin_generation, 1);
    assert!(
        !b1.clipboard_requested,
        "clipboard must not be read implicitly"
    );
    assert!(
        b1.clipboard.is_none(),
        "clipboard must be absent by default"
    );
    assert!(b1.profile.get("os_family").is_some(), "{}", b1.profile);
    assert!(!b1.captured_at.is_empty());
    assert!(b1.computer_backend == "sim", "{}", b1.computer_backend);
    // Cursor comes from the platform authority (sim starts at origin).
    assert_eq!(b1.cursor, Some((0, 0)));

    let b2 = e.observe(true).await.expect("bundle 2");
    assert!(b2.clipboard_requested, "explicit clipboard read");
    assert!(
        b2.clipboard.is_some(),
        "on-demand clipboard present (empty string on sim)"
    );
    assert_eq!(b2.twin_generation, 2);

    let twin = e.twin_status();
    assert_eq!(twin["valid"], true);
    assert_eq!(twin["observations"], 2);
}

/// TEST 26: the resolution chain prefers structured sources and fails closed
/// with the full trace when nothing matches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t26_resolution_chain_structured_sources_and_fail_closed() {
    let screen = fixture_screen(&FixtureScreen::new(1280, 720).with_elements(vec![
        ScreenElement::new("Fixture Window", "window", 10, 10, 400, 300),
        ScreenElement::new("OK Button", "button", 100, 100, 80, 30),
    ]));
    let e = engine(screen, sim_control());

    // Structured element: exact match, confidence 1.0, center point.
    let r = e.resolve_target("OK Button", None).await.expect("element");
    assert!(matches!(r.source, openrt_computer::TargetSource::Elements));
    assert_eq!(r.confidence, 1.0);
    assert_eq!(r.point, Some((140, 115)));
    assert_eq!((r.bounds.unwrap().x, r.bounds.unwrap().width), (100, 80));
    assert!(!r.timestamp.is_empty());
    // Chain always starts with the honest accessibility step.
    assert_eq!(r.chain[0].source, "accessibility");
    assert_eq!(r.chain[0].status, "unavailable");

    // Window-role element: window-tree with geometry.
    let r = e
        .resolve_target("Fixture Window", None)
        .await
        .expect("window");
    assert!(matches!(
        r.source,
        openrt_computer::TargetSource::WindowTree
    ));
    assert_eq!(r.point, Some((210, 160)));
    assert_eq!(r.window.as_deref(), Some("Fixture Window"));

    // Platform window metadata (the sim control window).
    let r = e.resolve_target("sim-1", None).await.expect("platform id");
    assert!(matches!(
        r.source,
        openrt_computer::TargetSource::WindowTree
    ));
    assert!(r.point.is_none(), "platform match without geometry");

    // Unknown target: NotFound carrying the chain trace.
    let err = e
        .resolve_target("Nonexistent Gizmo", None)
        .await
        .expect_err("must fail closed");
    let msg = err.to_string();
    assert!(msg.contains("target not found"), "{msg}");
    assert!(msg.contains("chain:"), "{msg}");
    assert!(msg.contains("window-tree=no-match"), "{msg}");
    assert!(msg.contains("elements=no-match"), "{msg}");
}

/// TEST 27: vision grounding resolves a target no structured source can
/// see — with the full chain (a11y unavailable, vision matched, heuristic
/// confidence < 1.0).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t27_vision_grounding_step_resolves_after_structured_misses() {
    let dir = tmp_dir("vision");
    let img = dir.join("shot.png");
    std::fs::write(&img, b"fake-png-bytes").unwrap();
    let screen = fixture_screen(&FixtureScreen::new(1280, 720).with_image(img));
    let vision = Arc::new(MockVision::new("400,300"));
    let e = engine(screen, sim_control()).with_vision(vision.clone());

    let r = e
        .resolve_target("Frobnicator Widget", None)
        .await
        .expect("vision resolves");
    assert!(matches!(r.source, openrt_computer::TargetSource::Vision));
    assert_eq!(r.point, Some((400, 300)));
    assert!((r.confidence - 0.6).abs() < 1e-9, "heuristic confidence");
    assert!(r
        .chain
        .iter()
        .any(|s| s.source == "accessibility" && s.status == "unavailable"));
    assert!(r
        .chain
        .iter()
        .any(|s| s.source == "vision" && s.status == "matched"));
    assert_eq!(
        vision.calls.load(Ordering::SeqCst),
        1,
        "exactly one grounding call"
    );
}

/// TEST 28: coordinate fallback — explicit coordinates resolve only after
/// every name source misses; without coordinates the call fails closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t28_coordinate_fallback_is_last_and_never_guessed() {
    let screen = fixture_screen(&FixtureScreen::new(1280, 720));
    let e = engine(screen, sim_control()); // no vision backend

    let r = e
        .resolve_target("Ghost", Some((5, 6)))
        .await
        .expect("coordinate fallback");
    assert!(matches!(
        r.source,
        openrt_computer::TargetSource::Coordinate
    ));
    assert_eq!(r.point, Some((5, 6)));
    assert_eq!(r.confidence, 1.0);
    assert!(r
        .chain
        .iter()
        .any(|s| s.source == "vision" && s.status == "unavailable"));
    assert!(r
        .chain
        .iter()
        .any(|s| s.source == "coordinate" && s.status == "matched"));

    let err = e
        .resolve_target("Ghost", None)
        .await
        .expect_err("no coordinates, no match → fail closed");
    assert!(err.to_string().contains("chain:"), "{err}");
}

/// TEST 29: the digital twin is invalidated after EVERY action and only a
/// fresh observation makes it valid again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t29_digital_twin_invalidated_after_every_action() {
    let screen = fixture_screen(&FixtureScreen::new(800, 600));
    let e = engine(screen, sim_control());

    e.observe(false).await.expect("seed observation");
    let before = e.twin_status();
    assert_eq!(before["valid"], true);
    assert_eq!(before["observations"], 1);

    e.execute(ComputerAction::Hotkey {
        key: "ctrl+c".into(),
    })
    .await
    .expect("hotkey");
    let after = e.twin_status();
    assert_eq!(after["valid"], false, "action must invalidate the twin");
    assert_eq!(after["invalidations"], 1);
    assert_eq!(
        after["observations"], 1,
        "invalidation is not an observation"
    );

    e.execute(ComputerAction::Clipboard {
        set: Some("payload".into()),
    })
    .await
    .expect("clipboard set");
    assert_eq!(e.twin_status()["invalidations"], 2);

    // A fresh observation restores validity with a new generation.
    let b = e.observe(false).await.expect("re-observe");
    assert_eq!(b.twin_generation, 2);
    assert_eq!(e.twin_status()["valid"], true);
}

/// TEST 30: focus executes through the platform authority, verifies against
/// observation within the bound, and reports advisory + resolution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t30_focus_executes_verifies_and_reports_advisory() {
    let screen = fixture_screen(&FixtureScreen::new(1280, 720));
    let e = engine(screen, sim_control());

    let report = e
        .execute(ComputerAction::Focus {
            target: "Sim Window".into(),
        })
        .await
        .expect("focus");
    assert!(report.ok);
    assert_eq!(report.action, "focus");
    assert_eq!(report.verified, Some(true), "focus verified by observation");
    assert!(
        report.attempts >= 1 && report.attempts <= openrt_computer::MAX_ATTEMPTS,
        "{}",
        report.attempts
    );
    assert_eq!(report.result["op"], "focus_window");
    let source = report.resolved.as_ref().unwrap().source;
    assert!(matches!(source, openrt_computer::TargetSource::WindowTree));
    assert_eq!(report.advisory["structured_alternative"], "computer.window");
    assert!(
        report.advisory["predicted_effect"]
            .as_str()
            .unwrap()
            .contains("Sim Window"),
        "{}",
        report.advisory
    );
    // The executed report carries the post-invalidation twin.
    assert_eq!(report.twin["valid"], false);
}

/// TEST 31: bounded failure recovery — a focus whose effect never becomes
/// observable fails closed after exactly MAX_ATTEMPTS (3), never spins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t31_bounded_verification_fails_after_three_attempts() {
    assert_eq!(openrt_computer::MAX_ATTEMPTS, 3);
    let screen = fixture_screen(&FixtureScreen::new(1024, 768));
    let control = Arc::new(ComputerControl::from_backend(
        EnvironmentProfile::host(),
        Arc::new(StaleFocusBackend),
    ));
    let e = engine(screen, control);

    let err = e
        .execute(ComputerAction::Focus {
            target: "Stale Window".into(),
        })
        .await
        .expect_err("verification must exhaust and fail closed");
    match err {
        ComputerUseError::Verify { attempts, reason } => {
            assert_eq!(attempts, openrt_computer::MAX_ATTEMPTS, "{reason}");
            assert!(reason.contains("active window"), "{reason}");
        }
        other => panic!("expected Verify error, got {other}"),
    }
    // The failed attempt still invalidated the twin (partial effect possible).
    assert_eq!(e.twin_status()["valid"], false);
    assert_eq!(e.twin_status()["invalidations"], 1);
}

/// TEST 31b: a non-ok platform result fails closed as the HONEST
/// Unavailable before any verification loop — no retry spin, no fabricated
/// "verification failed", no fake success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t31b_unavailable_mechanism_surfaces_honest_unavailable() {
    let screen = fixture_screen(&FixtureScreen::new(1280, 720));
    let control = Arc::new(ComputerControl::from_backend(
        EnvironmentProfile::host(),
        Arc::new(UnavailablePointerBackend),
    ));
    let e = engine(screen, control);

    let err = e
        .execute(ComputerAction::Click {
            target: None,
            x: Some(10),
            y: Some(20),
            button: 1,
        })
        .await
        .expect_err("unavailable mechanism must not fake success");
    match err {
        ComputerUseError::Unavailable(msg) => {
            assert!(msg.contains("no pointer mechanism"), "{msg}");
        }
        other => panic!("expected honest Unavailable, got {other}"),
    }
    // No verification loop ran — the refusal happened at injection time.
    assert_eq!(e.twin_status()["invalidations"], 1, "still invalidated");
}

/// TEST 32: pointer actions — click verifies cursor position, scroll rides
/// the platform position + screen wheel layer, drag reports honestly, and
/// out-of-bounds points are rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t32_click_scroll_drag_execute_within_bounds() {
    let screen = fixture_screen(&FixtureScreen::new(1280, 720));
    let e = engine(screen, sim_control());

    // Click at explicit coordinates → coordinate fallback → control path.
    let report = e
        .execute(ComputerAction::Click {
            target: None,
            x: Some(10),
            y: Some(20),
            button: 1,
        })
        .await
        .expect("click");
    assert_eq!(report.verified, Some(true), "cursor observed at target");
    assert_eq!(report.attempts, 1);
    assert_eq!(
        report.resolved.as_ref().unwrap().source.as_str(),
        "coordinate"
    );
    assert_eq!(report.result["click"]["op"], "click_pointer");

    // Only left-click is expressible through the platform authority.
    let err = e
        .execute(ComputerAction::Click {
            target: None,
            x: Some(10),
            y: Some(20),
            button: 3,
        })
        .await
        .expect_err("button 3 has no platform verb");
    assert!(err.to_string().contains("button 1"), "{err}");

    // Scroll: position through the platform (verified), wheel through the
    // screen layer (reported as injected steps — honest about the signal).
    let report = e
        .execute(ComputerAction::Scroll {
            target: None,
            x: Some(30),
            y: Some(40),
            delta: -100,
        })
        .await
        .expect("scroll");
    assert_eq!(report.result["layer"], "screen");
    assert_eq!(report.result["wheel_button"], 5);
    assert_eq!(report.result["wheel_steps"], 1);
    assert_eq!(report.verified, Some(true), "position verified");
    assert_eq!(report.result["position_verified"], true);

    // Drag: screen layer, non-idempotent, honestly unverifiable.
    let report = e
        .execute(ComputerAction::Drag {
            from_x: 0,
            from_y: 0,
            to_x: 50,
            to_y: 50,
        })
        .await
        .expect("drag");
    assert_eq!(report.verified, None, "drag has no observation signal");
    assert_eq!(report.attempts, 1, "non-idempotent actions never retry");
    assert_eq!(report.result["op"], "drag");

    // Out-of-bounds coordinate: rejected before any injection.
    let err = e
        .execute(ComputerAction::Click {
            target: None,
            x: Some(5000),
            y: Some(5000),
            button: 1,
        })
        .await
        .expect_err("outside the screen");
    assert!(err.to_string().contains("outside screen bounds"), "{err}");
}

/// TEST 33: `computer.use` is registered and DENY-BY-DEFAULT — no allow
/// rule, no execution (the Phase 11 non-negotiable in one assertion).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t33_computer_use_registered_and_deny_by_default() {
    let rt = build_rt(&[]).await;
    assert!(
        rt.registry.contains("computer.use"),
        "computer.use missing from {:?}",
        rt.registry.names()
    );
    let spec = rt.registry.get("computer.use").unwrap().spec().clone();
    assert_eq!(spec.required_caps.len(), 1);
    assert_eq!(spec.required_caps[0].to_string(), "computer.use.all");

    let err = rt
        .executor
        .execute(
            "computer.use",
            json!({"action": "click", "x": 10, "y": 10}),
            &ctx(),
        )
        .await
        .expect_err("deny-by-default: no allow rule → no execution");
    match err {
        ToolError::PermissionDenied { tool, reason } => {
            assert_eq!(tool, "computer.use");
            assert!(reason.contains("policy"), "{reason}");
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
}

/// TEST 34: with an allow rule the SAME call crosses the one gate — shield
/// scan, policy grant, execution through ComputerControl, and an audit row
/// (`permission.decision` … allowed) exist for the computer-scoped tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t34_computer_use_gated_and_audited_with_allow_rule() {
    let rt = build_rt(&["computer.use"]).await;

    let out = rt
        .executor
        .execute(
            "computer.use",
            json!({"action": "click", "x": 5, "y": 6}),
            &ctx(),
        )
        .await
        .expect("allow rule permits the call");
    let v = match out {
        ToolOutput::Json(v) => v,
        ToolOutput::Text(t) => panic!("expected JSON, got {t}"),
    };
    assert_eq!(v["action"], "click");
    assert_eq!(v["ok"], true);
    assert_eq!(v["verified"], true, "cursor verified at (5,6): {v}");
    assert_eq!(v["twin"]["valid"], false, "twin invalidated: {v}");
    let attempts = v["attempts"].as_u64().unwrap();
    assert!(attempts >= 1 && attempts <= 3, "{attempts}");

    // ADR-002: the allowed computer-use decision was recorded BEFORE run.
    let records = rt
        .audit_query(&AuditQuery::new().with_action_prefix("permission.decision"))
        .await
        .unwrap();
    assert!(
        records
            .iter()
            .any(|r| r.entry.subject == "computer.use" && r.entry.result == "allowed"),
        "missing allowed audit row: {:?}",
        records
            .iter()
            .map(|r| (&r.entry.subject, &r.entry.result))
            .collect::<Vec<_>>()
    );
    assert_eq!(records[0].entry.actor, "system");
    rt.audit_verify().await.unwrap();
}

/// TEST 35 (M11.9): typed text is command-shaped input — the shield scans
/// `computer.*` text args with the same detectors as a direct terminal
/// call (dangerous text is HALTED at the gate), findings never carry the
/// raw payload, and benign typing is never flagged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t35_shield_scans_typed_text_without_leaking_payload() {
    // (a) Direct evaluator: dangerous typed text produces a finding whose
    // target is the TOOL (not the payload) and whose serialization never
    // contains the raw typed string.
    let evaluator = ShieldEvaluator::new();
    let report = evaluator.evaluate(&ShieldContext {
        source: "gate".into(),
        tool: "computer.use".into(),
        args: json!({"action": "type", "text": "rm -rf /"}),
        required_caps: vec!["computer.use.all".into()],
        claimed_permissions: Vec::new(),
        sandboxed: false,
        sovereign: false,
        session_id: None,
        task_id: None,
    });
    assert!(!report.is_clean(), "typed destructive text must be flagged");
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.reason.contains("removal of system path")),
        "{:?}",
        report.findings
    );
    let dumped = serde_json::to_string(&report.findings).unwrap();
    assert!(
        !dumped.contains("rm -rf /"),
        "payload leaked into findings: {dumped}"
    );

    // Clipboard set with credential material: the credential detector fires
    // on the needle, never carrying the payload beyond the shared needle.
    let report = evaluator.evaluate(&ShieldContext {
        source: "gate".into(),
        tool: "computer.clipboard".into(),
        args: json!({"action": "set", "text": "cp ~/.ssh/id_rsa /tmp/stolen"}),
        required_caps: vec!["computer.clipboard.all".into()],
        claimed_permissions: Vec::new(),
        sandboxed: false,
        sovereign: false,
        session_id: None,
        task_id: None,
    });
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.category.as_str() == "credential_access"),
        "{:?}",
        report.findings
    );
    let dumped = serde_json::to_string(&report.findings).unwrap();
    assert!(
        !dumped.contains("id_rsa"),
        "secret path leaked into findings: {dumped}"
    );

    // Benign typing is never flagged (no false positives on the GUI path).
    let report = evaluator.evaluate(&ShieldContext {
        source: "gate".into(),
        tool: "input.keyboard".into(),
        args: json!({"action": "type", "text": "hello world"}),
        required_caps: vec!["input.keyboard.all".into()],
        claimed_permissions: Vec::new(),
        sandboxed: false,
        sovereign: false,
        session_id: None,
        task_id: None,
    });
    assert!(report.is_clean(), "benign typing flagged: {report:?}");

    // (b) Through the ONE gate: the same dangerous text is stopped before
    // execution (Halt → kill switch, ComputerUse never runs the action).
    let rt = build_rt(&["computer.use"]).await;
    let err = rt
        .executor
        .execute(
            "computer.use",
            json!({"action": "type", "text": "rm -rf /"}),
            &ctx(),
        )
        .await
        .expect_err("typed destructive command must not execute");
    match err {
        ToolError::KillSwitchEngaged { tool, reason } => {
            assert_eq!(tool, "computer.use");
            assert!(reason.to_lowercase().contains("halt"), "{reason}");
        }
        ToolError::PermissionDenied { reason, .. } => {
            assert!(reason.contains("AgentShield"), "{reason}");
        }
        other => panic!("expected shield halt/deny, got {other:?}"),
    }
}

/// TEST 36: the VLM GROUNDS only — one grounding call feeds the resolution,
/// execution goes through ComputerControl (never through the vision
/// backend), and the advisory prefers the structured input tool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t36_vlm_grounds_but_never_executes() {
    let dir = tmp_dir("ground");
    let img = dir.join("shot.png");
    std::fs::write(&img, b"fake-png-bytes").unwrap();
    let screen = fixture_screen(&FixtureScreen::new(1280, 720).with_image(img));
    let vision = Arc::new(MockVision::new("640,360"));
    let e = engine(screen, sim_control()).with_vision(vision.clone());

    let report = e
        .execute(ComputerAction::Click {
            target: Some("GhostButton".into()),
            x: None,
            y: None,
            button: 1,
        })
        .await
        .expect("vision-grounded click");

    // Resolution provenance: vision, heuristic confidence.
    let resolved = report.resolved.as_ref().unwrap();
    assert!(matches!(
        resolved.source,
        openrt_computer::TargetSource::Vision
    ));
    assert_eq!(resolved.point, Some((640, 360)));
    assert!(resolved.confidence < 1.0);
    assert!(report.advisory["structured_alternative"] == "input.mouse");

    // Execution rode the platform authority (move + click), verified.
    assert_eq!(report.result["op"], "move_pointer");
    assert_eq!(report.result["click"]["op"], "click_pointer");
    assert_eq!(report.verified, Some(true));

    // The vision backend was invoked EXACTLY once (resolution) — it never
    // participates in execution.
    assert_eq!(
        vision.calls.load(Ordering::SeqCst),
        1,
        "vision grounds once, never executes"
    );
}
