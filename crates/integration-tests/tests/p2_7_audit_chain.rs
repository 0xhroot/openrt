//! P2.7 ADR-002 chained audit log, end-to-end through the real runtime:
//! * every computer-use permission decision passes through the gate executor
//!   and is recorded in a single chained, tamper-evident log (`audit`
//!   namespace);
//! * the ADR-008 HALT signal is recorded (`safety.halt`) and, on a subsequent
//!   computer action, the gate records `permission.decision` result
//!   `kill_blocked`;
//! * the whole chain verifies after each append;
//! * tampering a stored record is detected (content hash mismatch — not just
//!   the tail);
//! * the H1 engine cancel surfaces as `WorkflowStatus::Cancelled` promptly.
//!
//! All tests are deterministic and network-free (mock providers, sim screen,
//! in-memory / `LocalFileStore` persistence).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use openrt_audit::{seq_key, AuditError, AUDIT_NS};
use openrt_config::{Config, PermissionsConfig, RuleConfig, RuleEffect};
use openrt_core::{SessionId, TaskId};
use openrt_models::mock::MockProvider;
use openrt_models::provider::{ModelCapabilities, ModelProvider};
use openrt_models::types::{ModelInfo, ModelRequest, ModelResponse};
use openrt_orch::{SubAgentKind, Workflow, WorkflowStatus, WorkflowStep};
use openrt_persistence::{MemoryStore, PersistenceStore};
use openrt_tools::ToolError;

fn tmp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "openrt-p2_7-{prefix}-{}-{:x}",
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
    // Gate enforces `required_caps` (e.g. `filesystem.read.all`) separately.
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

fn ctx() -> openrt_tools::ToolExecutionContext {
    openrt_tools::ToolExecutionContext {
        session_id: Some(SessionId::new().to_string()),
        task_id: Some(TaskId::new().to_string()),
        channel: None,
    }
}

/// In-memory runtime with a sim screen and a configurable allowlist. Returns
/// the runtime plus a handle on the same underlying store (for tamper-checks).
async fn build(
    ws: &std::path::Path,
    caps: &[&str],
) -> (Arc<openrt_runtime::Runtime>, Arc<dyn PersistenceStore>) {
    let config = Config {
        runtime: openrt_config::RuntimeConfig {
            workspace_dir: Some(ws.to_path_buf()),
            screen_backend: Some("sim".into()),
            ..Default::default()
        },
        permissions: allow_rules(caps),
        ..Default::default()
    };
    let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(store.clone())
        .with_provider(Arc::new(MockProvider::echo("audit")))
        .build()
        .await
        .unwrap();
    (rt, store)
}

/// Permissions decisions for computer tools are chained into the audit log;
/// the HALT is recorded as `safety.halt`; and a post-halt computer action is
/// blocked AND recorded as `kill_blocked` — the chain verifies throughout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permission_decisions_and_halt_are_chained_and_verify() {
    let ws = tmp_dir("chain");
    let (rt, _store) = build(&ws, &["input.mouse"]).await;

    // Allowed computer use → `permission.decision`/`allowed`.
    rt.executor
        .execute(
            "input.mouse",
            serde_json::json!({ "action": "click", "x": 5, "y": 5 }),
            &ctx(),
        )
        .await
        .unwrap();
    // Denied computer use (no rule) → decision recorded with `denied`.
    let err = rt
        .executor
        .execute("screen.capture", serde_json::json!({}), &ctx())
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::PermissionDenied { .. }), "{err:?}");

    // ADR-008 HALT is durable in the same chain.
    assert!(rt.halt("operator emergency (audit test)").await.unwrap());
    let n_after_halt = rt.audit.count().await.unwrap();
    assert_eq!(rt.audit.verify().await.unwrap(), n_after_halt);

    // Post-halt computer action: blocked by the gate AND recorded.
    let err = rt
        .executor
        .execute(
            "input.mouse",
            serde_json::json!({ "action": "click", "x": 5, "y": 5 }),
            &ctx(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, ToolError::KillSwitchEngaged { .. }),
        "{err:?}"
    );

    let n = rt.audit.count().await.unwrap();
    assert!(n >= 4, "expected >=4 records, got {n}");
    assert_eq!(rt.audit.verify().await.unwrap(), n, "chain must verify");

    // Inspect the records: decisions are `permission.decision` with the tool
    // as subject, results `allowed`/`denied`/`kill_blocked`; the HALT is
    // `safety.halt`/`tripped` with the operator reason.
    let mut decisions = Vec::new();
    let mut halt = None;
    for seq in 1..=n {
        let rec = rt.audit.get(seq).await.unwrap();
        assert_eq!(rec.seq, seq, "sequence must be monotonic");
        if rec.entry.action == "permission.decision" {
            decisions.push((rec.entry.subject.clone(), rec.entry.result.clone()));
        } else if rec.entry.action == "safety.halt" {
            halt = Some(rec);
        }
    }
    assert!(
        decisions
            .iter()
            .any(|(sub, res)| sub == "input.mouse" && res == "allowed"),
        "{decisions:?}"
    );
    assert!(
        decisions
            .iter()
            .any(|(sub, res)| sub == "screen.capture" && res == "denied"),
        "{decisions:?}"
    );
    assert!(
        decisions
            .iter()
            .any(|(sub, res)| sub == "input.mouse" && res == "kill_blocked"),
        "{decisions:?}"
    );
    let halt = halt.expect("safety.halt must be recorded");
    assert_eq!(halt.entry.actor, "operator");
    assert_eq!(halt.entry.result, "tripped");
    assert_eq!(
        halt.entry
            .ext
            .get("reason")
            .and_then(serde_json::Value::as_str),
        Some("operator emergency (audit test)")
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// Editing a stored record out from under the log is detected: the recomputed
/// hash no longer matches what is stored, so `verify()` fails on that record —
/// a content tamper is never silently masked by the tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tampered_audit_record_is_detected() {
    let ws = tmp_dir("tamper");
    let (rt, store) = build(&ws, &["input.mouse"]).await;

    rt.executor
        .execute(
            "input.mouse",
            serde_json::json!({ "action": "click", "x": 5, "y": 5 }),
            &ctx(),
        )
        .await
        .unwrap();
    rt.halt("tamper baseline").await.unwrap();
    let n = rt.audit.count().await.unwrap();
    assert_eq!(rt.audit.verify().await.unwrap(), n);

    // Force the persisted record to disagree with its own content.
    let mut rec = rt.audit.get(n).await.unwrap();
    rec.hash = "0".repeat(64);
    store
        .put_record(AUDIT_NS, &seq_key(n), &serde_json::to_value(&rec).unwrap())
        .await
        .unwrap();

    let err = rt.audit.verify().await.unwrap_err();
    assert!(
        matches!(&err, AuditError::Tampered(seq, _) if *seq == n),
        "{err:?}"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

/// A provider that blocks every invocation until released, keeping the engine
/// inside a step while we cancel it.
struct BlockingProvider {
    gate_open: tokio::sync::Notify,
    calls: AtomicUsize,
}

impl BlockingProvider {
    fn new() -> Self {
        Self {
            gate_open: tokio::sync::Notify::new(),
            calls: AtomicUsize::new(0),
        }
    }
    fn reached(&self) -> bool {
        self.calls.load(Ordering::SeqCst) > 0
    }
    fn release(&self) {
        self.gate_open.notify_waiters();
    }
}

#[async_trait::async_trait]
impl ModelProvider for BlockingProvider {
    fn id(&self) -> &str {
        "blocking"
    }
    fn info(&self) -> ModelInfo {
        MockProvider::echo("blocking").info()
    }
    fn capabilities(&self) -> ModelCapabilities {
        MockProvider::echo("blocking").capabilities()
    }
    async fn generate(
        &self,
        req: ModelRequest,
    ) -> Result<ModelResponse, openrt_models::error::ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.gate_open.notified().await;
        MockProvider::echo("blocking").generate(req).await
    }
}

/// H1: a workflow mid-step is cancelled promptly and reports `Cancelled`
/// (never a continued `Failed`); the audit chain still verifies afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_running_workflow_reports_cancelled() {
    let ws = tmp_dir("cancel");
    let config = Config {
        runtime: openrt_config::RuntimeConfig {
            workspace_dir: Some(ws.to_path_buf()),
            screen_backend: Some("sim".into()),
            ..Default::default()
        },
        permissions: allow_rules(&[]),
        ..Default::default()
    };
    let blocking = Arc::new(BlockingProvider::new());
    let rt = openrt_runtime::RuntimeBuilder::new()
        .config(config)
        .store(Arc::new(MemoryStore::new()))
        .with_provider(blocking.clone())
        .build()
        .await
        .unwrap();
    let session = rt.create_session("cancel").await.unwrap();
    let wf = Workflow::new("wf-cancel")
        .step(WorkflowStep::new("plan", SubAgentKind::Planner, "plan it"))
        .step(WorkflowStep::new("code", SubAgentKind::Coder, "implement it").dep("plan"));

    let rt2 = rt.clone();
    let handle = tokio::spawn(async move {
        let session_id = session.id;
        rt2.run_workflow(session_id, wf.clone()).await
    });

    // Wait until the engine entered a step (provider was invoked), then cancel.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !blocking.reached() {
        assert!(
            std::time::Instant::now() < deadline,
            "engine never invoked the provider"
        );
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    rt.cancel_workflow();

    let started = std::time::Instant::now();
    let report = handle.await.unwrap().unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "cancel must be prompt, took {:?}",
        started.elapsed()
    );
    assert_eq!(report.status, WorkflowStatus::Cancelled, "{report:?}");

    // The chain was not corrupted by the cancellation path.
    let n = rt.audit.count().await.unwrap();
    assert_eq!(rt.audit.verify().await.unwrap(), n);
    blocking.release();
    let _ = std::fs::remove_dir_all(&ws);
}
