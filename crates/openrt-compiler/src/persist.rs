//! Compiled workflow persistence under the `compiled` namespace (ADR-006).
//!
//! The executable graph is the same [`openrt_orch::Workflow`] the orchestrator
//! already persists under `dag`; here we store the *compiled record* — goal,
//! decomposition, capability plan, provenance, validation report, and a
//! reference to the `dag` workflow id — so `inspect`/`explain`/`run` can load
//! it later without recompiling.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use openrt_orch::Workflow;
use openrt_persistence::{PersistenceError, PersistenceStore};

use crate::capability::CapabilityPlan;
use crate::compile::ReplanRecord;
use crate::decompose::DecomposedGoal;
use crate::diagnostics::CompileDiagnostic;
use crate::goal::GoalSpec;
use crate::graph::Provenance;
use crate::validate::ValidationReport;
use crate::{COMPILED_NS, COMPILER_VERSION};

/// Durable record of one successful compilation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompiledRecord {
    /// Workflow id (also the `dag` namespace key for the executable graph).
    pub workflow_id: String,
    pub goal: GoalSpec,
    pub decomposition: DecomposedGoal,
    pub capabilities: CapabilityPlan,
    pub provenance: Provenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<ValidationReport>,
    /// Warnings emitted during compilation (errors already failed closed).
    #[serde(default)]
    pub diagnostics: Vec<CompileDiagnostic>,
    pub compiler_version: String,
    /// Unix millis at compile time.
    pub compiled_at: u64,
    /// Bounded replan trail when one ran (original + correction + final).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replan: Option<ReplanRecord>,
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Persist a compiled record under `compiled/<workflow_id>.json`.
///
/// The caller is responsible for also persisting the executable [`Workflow`]
/// under `dag` (done by `WorkflowEngine::run` or explicitly via `put_record`).
pub async fn persist_compiled(
    store: &Arc<dyn PersistenceStore>,
    workflow_id: &str,
    goal: GoalSpec,
    decomposition: DecomposedGoal,
    capabilities: CapabilityPlan,
    provenance: Provenance,
    validation: Option<ValidationReport>,
    diagnostics: Vec<CompileDiagnostic>,
    replan: Option<ReplanRecord>,
) -> Result<(), PersistenceError> {
    let record = CompiledRecord {
        workflow_id: workflow_id.to_string(),
        goal,
        decomposition,
        capabilities,
        provenance,
        validation,
        diagnostics,
        compiler_version: COMPILER_VERSION.to_string(),
        compiled_at: now_millis(),
        replan,
    };
    let value = serde_json::to_value(&record)?;
    store.put_record(COMPILED_NS, workflow_id, &value).await
}

/// Load a compiled record by workflow id (`None` when absent).
pub async fn load_compiled(
    store: &Arc<dyn PersistenceStore>,
    workflow_id: &str,
) -> Result<Option<CompiledRecord>, PersistenceError> {
    let Some(value) = store.get_record(COMPILED_NS, workflow_id).await? else {
        return Ok(None);
    };
    let record: CompiledRecord = serde_json::from_value(value)?;
    Ok(Some(record))
}

/// List all compiled records sorted by `workflow_id` (deterministic order).
pub async fn list_compiled(
    store: &Arc<dyn PersistenceStore>,
) -> Result<Vec<CompiledRecord>, PersistenceError> {
    let mut out = Vec::new();
    for (key, value) in store.list_records(COMPILED_NS).await? {
        match serde_json::from_value::<CompiledRecord>(value) {
            Ok(r) => out.push(r),
            Err(e) => {
                return Err(PersistenceError::Corrupt(
                    format!("compiled/{key}"),
                    e.to_string(),
                ))
            }
        }
    }
    out.sort_by(|a, b| a.workflow_id.cmp(&b.workflow_id));
    Ok(out)
}

/// Load the executable workflow from the `dag` namespace for a compiled id.
pub async fn load_dag_workflow(
    store: &Arc<dyn PersistenceStore>,
    workflow_id: &str,
) -> Result<Option<Workflow>, PersistenceError> {
    let Some(value) = store.get_record("dag", workflow_id).await? else {
        return Ok(None);
    };
    let wf: Workflow = serde_json::from_value(value)?;
    Ok(Some(wf))
}

/// Persist the executable workflow graph under `dag/<workflow_id>` so
/// `inspect`/`run` can fetch it without waiting for a live engine run.
pub async fn persist_dag_workflow(
    store: &Arc<dyn PersistenceStore>,
    workflow: &Workflow,
) -> Result<(), PersistenceError> {
    let value = serde_json::to_value(workflow)?;
    store.put_record("dag", &workflow.id, &value).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{plan_capabilities, RegistryView};
    use crate::decompose::decompose;
    use crate::graph::build_workflow;
    use openrt_persistence::MemoryStore;

    fn allow(_p: &str) -> bool {
        true
    }

    async fn make_record() -> CompiledRecord {
        let goal = GoalSpec {
            id: GoalSpec::id_for("persist me"),
            original_request: "Create a file containing the current project status".into(),
            objective: "Create a file containing the current project status".into(),
            inputs: vec![],
            expected_outputs: vec!["status.txt".into()],
            constraints: vec![],
            session_id: None,
            workspace: None,
            requires_network: false,
            requires_verification: false,
        };
        let decomp = decompose(&goal);
        let tools = vec![
            "filesystem.read".to_string(),
            "filesystem.write".to_string(),
            "echo".to_string(),
        ];
        let reg = RegistryView {
            tool_names: &tools,
            policy_allows: &allow,
            network_allowed: true,
            model_available: true,
        };
        let plan = plan_capabilities(&decomp, &reg);
        let (wf, prov, _d) = build_workflow(&goal, &decomp, &plan, "wf.persist").unwrap();
        CompiledRecord {
            workflow_id: wf.id.clone(),
            goal,
            decomposition: decomp,
            capabilities: plan,
            provenance: prov,
            validation: None,
            diagnostics: vec![],
            compiler_version: COMPILER_VERSION.to_string(),
            compiled_at: now_millis(),
            replan: None,
        }
    }

    #[tokio::test]
    async fn persist_load_roundtrip() {
        let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
        let rec = make_record().await;
        let wf_id = rec.workflow_id.clone();
        persist_compiled(
            &store,
            &wf_id,
            rec.goal.clone(),
            rec.decomposition.clone(),
            rec.capabilities.clone(),
            rec.provenance.clone(),
            rec.validation.clone(),
            rec.diagnostics.clone(),
            rec.replan.clone(),
        )
        .await
        .unwrap();
        let loaded = load_compiled(&store, &wf_id).await.unwrap().unwrap();
        assert_eq!(loaded.workflow_id, wf_id);
        assert_eq!(loaded.goal.id, rec.goal.id);
        assert_eq!(loaded.compiler_version, COMPILER_VERSION);
        let list = list_compiled(&store).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].workflow_id, wf_id);
    }

    #[tokio::test]
    async fn missing_record_is_none() {
        let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
        assert!(load_compiled(&store, "wf.nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn dag_workflow_roundtrip() {
        let store: Arc<dyn PersistenceStore> = Arc::new(MemoryStore::new());
        let goal = GoalSpec {
            id: GoalSpec::id_for("dag"),
            original_request: "Create status file".into(),
            objective: "Create status file".into(),
            inputs: vec![],
            expected_outputs: vec!["s.txt".into()],
            constraints: vec![],
            session_id: None,
            workspace: None,
            requires_network: false,
            requires_verification: false,
        };
        let decomp = decompose(&goal);
        let tools = vec![
            "filesystem.read".to_string(),
            "filesystem.write".to_string(),
            "echo".to_string(),
        ];
        let reg = RegistryView {
            tool_names: &tools,
            policy_allows: &allow,
            network_allowed: true,
            model_available: true,
        };
        let plan = plan_capabilities(&decomp, &reg);
        let (wf, _p, _d) = build_workflow(&goal, &decomp, &plan, "wf.dag").unwrap();
        persist_dag_workflow(&store, &wf).await.unwrap();
        let loaded = load_dag_workflow(&store, &wf.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, wf.id);
        assert_eq!(loaded.steps.len(), wf.steps.len());
    }
}
