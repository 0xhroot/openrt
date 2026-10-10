//! M10.9 AgentShield at the enforcement edge: one deterministic security
//! scan of every resolved gate-side tool call.
//!
//! The layer never invents a second policy engine — the report's action is
//! mapped onto the gate's EXISTING authorities (approval path, permission
//! denial, ADR-008 kill switch) by [`crate::gate::GateExecutor`]. Findings
//! are persisted durably so every scan is observable for audit.

use std::sync::Arc;

use openrt_persistence::PersistenceStore;
use openrt_shield::{
    detectors::ShieldEvaluator, store::record_enforced, FindingStatus, ShieldContext, ShieldReport,
};

/// The gate's shield step: scan + durable finding recording.
///
/// Attached with `GateExecutor::with_shield`. Sovereign-ness is read live
/// from the same store the runtime's `ModeManager` writes (one authority for
/// mode state — the layer holds a read handle, never a second source of
/// truth; an unreadable mode record fails the scan closed).
pub struct ShieldLayer {
    evaluator: ShieldEvaluator,
    store: Arc<dyn PersistenceStore>,
    modes: openrt_modes::ModeManager,
}

impl ShieldLayer {
    /// Build over the shared persistence store (same durable namespace
    /// family the runtime uses for modes, findings, and everything else).
    pub fn new(store: Arc<dyn PersistenceStore>) -> Self {
        let modes = openrt_modes::ModeManager::new(store.clone());
        Self {
            evaluator: ShieldEvaluator::new(),
            store,
            modes,
        }
    }

    /// Human-readable attachment descriptor (doctor row evidence).
    pub fn describe(&self) -> String {
        "shield layer attached (source=gate, evaluator=ShieldEvaluator)".to_string()
    }

    /// Scan one gate-side call. A mode-read failure propagates — the gate
    /// turns it into a blocked call (fail closed).
    pub async fn scan(
        &self,
        tool: &str,
        args: &serde_json::Value,
        required_caps: Vec<String>,
        sandboxed: bool,
        session_id: Option<String>,
        task_id: Option<String>,
    ) -> Result<ShieldReport, crate::Error> {
        let sovereign = self.modes.current().await?.mode == openrt_modes::ExecutionMode::Sovereign;
        let ctx = ShieldContext {
            source: "gate".into(),
            tool: tool.to_string(),
            args: args.clone(),
            required_caps,
            claimed_permissions: Vec::new(),
            sandboxed,
            sovereign,
            session_id,
            task_id,
        };
        Ok(self.evaluator.evaluate(&ctx))
    }

    /// Persist the findings stamped with the status that reflects how the
    /// gate enforced (or let through) the call. Clean reports record nothing.
    pub async fn record(
        &self,
        report: &ShieldReport,
        status: FindingStatus,
    ) -> Result<(), crate::Error> {
        if report.findings.is_empty() {
            return Ok(());
        }
        record_enforced(&*self.store, report, status).await?;
        Ok(())
    }
}

impl crate::runtime::Runtime {
    /// M10.9: advisory scan through the SAME shield layer the gate enforces
    /// with (one evaluator, one mode record, one store) — the call is never
    /// executed and nothing is recorded (use the gate for enforcement).
    pub async fn shield_scan(
        &self,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<ShieldReport, crate::Error> {
        let caps: Vec<String> = self
            .registry
            .get(tool)
            .map(|t| {
                t.spec()
                    .required_caps
                    .iter()
                    .map(|c| c.as_str().to_string())
                    .collect()
            })
            .unwrap_or_default();
        let sandboxed = self.sandbox.requirement(tool).is_some();
        self.shield_layer
            .scan(tool, &args, caps, sandboxed, None, None)
            .await
    }
}
