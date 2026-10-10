//! `openrt-train` — honest model-adaptation experiment framework
//! (Phase 4 allocation `train.runs`, implemented for Phase 12 M12.16).
//!
//! SFT/LoRA/QLoRA experiment records with dataset/model/adapter lineage,
//! an environment probe that reports training backends truthfully, and
//! metric-comparison evaluation. No training runs unless a real
//! `TrainingBackend` is configured — the probe never fabricates capability.

use std::collections::BTreeMap;

use openrt_knowledge::safe_key;
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const RUNS_NS: &str = "train.runs";

#[derive(Debug, Error)]
pub enum TrainError {
    #[error("store error: {0}")]
    Store(#[from] openrt_persistence::PersistenceError),
    #[error("serialization error: {0}")]
    Serde(String),
    #[error("corrupt record `{0}`: {1}")]
    Corrupt(String, String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("training backend unavailable: {0}")]
    BackendUnavailable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrainMethod {
    Sft,
    Lora,
    Qlora,
}

impl TrainMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrainMethod::Sft => "sft",
            TrainMethod::Lora => "lora",
            TrainMethod::Qlora => "qlora",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdapterSpec {
    pub kind: String,
    pub rank: u32,
    pub alpha: f64,
    pub target_modules: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentSpec {
    pub id: String,
    pub dataset_id: String,
    pub dataset_digest: Option<String>,
    pub base_model: String,
    pub method: TrainMethod,
    pub adapter: AdapterSpec,
    #[serde(default)]
    pub params: BTreeMap<String, f64>,
    pub hardware: String,
    pub provenance: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentStatus {
    Planned,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl ExperimentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExperimentStatus::Planned => "planned",
            ExperimentStatus::Running => "running",
            ExperimentStatus::Completed => "completed",
            ExperimentStatus::Failed => "failed",
            ExperimentStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentRun {
    pub id: String,
    pub spec: ExperimentSpec,
    pub status: ExperimentStatus,
    /// Honest backend report for this run (probe at start time).
    pub backend: String,
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub metrics: BTreeMap<String, f64>,
    pub adapter_ref: Option<String>,
    pub parent_run: Option<String>,
    /// Operator decision gate: runs are never promoted implicitly.
    pub approval: OperatorDecision,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorDecision {
    Pending,
    Approved,
    Rejected,
}

fn run_key(id: &str) -> String {
    format!("run.{}", safe_key(id))
}

pub async fn put_run(store: &dyn PersistenceStore, run: &ExperimentRun) -> Result<(), TrainError> {
    if run.id.trim().is_empty() {
        return Err(TrainError::Invalid("run id must not be empty".into()));
    }
    let v =
        serde_json::to_value(run).map_err(|e| TrainError::Serde(format!("serialize run: {e}")))?;
    store.put_record(RUNS_NS, &run_key(&run.id), &v).await?;
    Ok(())
}

pub async fn get_run(
    store: &dyn PersistenceStore,
    id: &str,
) -> Result<Option<ExperimentRun>, TrainError> {
    match store.get_record(RUNS_NS, &run_key(id)).await? {
        None => Ok(None),
        Some(v) => serde_json::from_value(v)
            .map(Some)
            .map_err(|e| TrainError::Corrupt(run_key(id), e.to_string())),
    }
}

pub async fn list_runs(store: &dyn PersistenceStore) -> Result<Vec<ExperimentRun>, TrainError> {
    let mut out: Vec<ExperimentRun> = Vec::new();
    for (key, value) in store.list_records(RUNS_NS).await? {
        if !key.starts_with("run.") {
            continue;
        }
        out.push(
            serde_json::from_value(value).map_err(|e| TrainError::Corrupt(key, e.to_string()))?,
        );
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

pub fn new_run(spec: ExperimentSpec) -> Result<ExperimentRun, TrainError> {
    if spec.id.trim().is_empty() {
        return Err(TrainError::Invalid(
            "experiment id must not be empty".into(),
        ));
    }
    if spec.base_model.trim().is_empty() {
        return Err(TrainError::Invalid("base_model must not be empty".into()));
    }
    let now = openrt_core::now().to_rfc3339();
    Ok(ExperimentRun {
        id: spec.id.clone(),
        spec,
        status: ExperimentStatus::Planned,
        backend: "none".into(),
        failure_reason: None,
        metrics: BTreeMap::new(),
        adapter_ref: None,
        parent_run: None,
        approval: OperatorDecision::Pending,
        created_at: now.clone(),
        updated_at: now,
    })
}

/// Environment probe: what could actually train here? Reported truthfully;
/// `trainable` only when interpreter + adapter library are both present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendProbe {
    pub python: bool,
    pub adapter_lib: bool,
    pub gpu: bool,
    pub trainable: bool,
    pub reasons: Vec<String>,
    pub probed_at: String,
}

pub fn probe_backend() -> BackendProbe {
    let python = std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let adapter_lib = python
        && std::process::Command::new("python3")
            .args(["-c", "import peft"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
    let gpu = std::process::Command::new("nvidia-smi")
        .args(["-L"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);
    let mut reasons = Vec::new();
    if !python {
        reasons.push("python3 interpreter not found".into());
    }
    if !adapter_lib {
        reasons.push("python `peft` library not importable".into());
    }
    if !gpu {
        reasons.push("no NVIDIA GPU reported by nvidia-smi".into());
    }
    let trainable = python && adapter_lib;
    if !trainable {
        reasons.push("training backends UNAVAILABLE — experiments cannot run here".into());
    }
    BackendProbe {
        python,
        adapter_lib,
        gpu,
        trainable,
        reasons,
        probed_at: openrt_core::now().to_rfc3339(),
    }
}

/// Real training execution surface. Nothing in OpenRT implements this in
/// Phase 12 — experiments honestly fail with `BackendUnavailable`.
pub trait TrainingBackend: Send + Sync {
    fn name(&self) -> &str;
    fn train(&self, spec: &ExperimentSpec) -> Result<TrainOutcome, TrainError>;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrainOutcome {
    #[serde(default)]
    pub metrics: BTreeMap<String, f64>,
    pub adapter_ref: String,
}

/// Attempt to start a run: with no backend (the Phase 12 state) the run
/// transitions to Failed with an explicit reason — never to a fake success.
pub async fn start_run(
    store: &dyn PersistenceStore,
    id: &str,
    backend: Option<&dyn TrainingBackend>,
) -> Result<ExperimentRun, TrainError> {
    let mut run = get_run(store, id)
        .await?
        .ok_or_else(|| TrainError::NotFound(format!("run `{id}`")))?;
    if !matches!(
        run.status,
        ExperimentStatus::Planned | ExperimentStatus::Failed
    ) {
        return Err(TrainError::Invalid(format!(
            "run `{id}` is {} — only planned/failed runs can be (re)started",
            run.status.as_str()
        )));
    }
    match backend {
        None => {
            let probe = probe_backend();
            run.status = ExperimentStatus::Failed;
            run.backend = "none".into();
            run.failure_reason = Some(format!(
                "no training backend configured (environment probe: trainable={}, {})",
                probe.trainable,
                probe.reasons.join("; ")
            ));
        }
        Some(b) => {
            run.backend = b.name().to_string();
            match b.train(&run.spec) {
                Ok(outcome) => {
                    run.status = ExperimentStatus::Completed;
                    run.metrics = outcome.metrics;
                    run.adapter_ref = Some(outcome.adapter_ref);
                }
                Err(e) => {
                    run.status = ExperimentStatus::Failed;
                    run.failure_reason = Some(e.to_string());
                }
            }
        }
    }
    run.updated_at = openrt_core::now().to_rfc3339();
    put_run(store, &run).await?;
    Ok(run)
}

/// Metrics tracked for regression comparison (lower-is-better unless
/// listed in `higher_is_better`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvalPolicy {
    pub higher_is_better: &'static [&'static str],
}

impl Default for EvalPolicy {
    fn default() -> Self {
        Self {
            higher_is_better: &["accuracy", "f1", "bleu", "reward", "win_rate"],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionVerdict {
    Promote,
    Hold,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalComparison {
    pub candidate: String,
    pub baseline: Option<String>,
    #[serde(default)]
    pub deltas: BTreeMap<String, f64>,
    pub regressed: Vec<String>,
    pub improved: Vec<String>,
    pub verdict: PromotionVerdict,
    pub note: String,
}

/// Compare candidate metrics against a baseline run (or a metric target
/// map). Measured numbers only — no subjective scoring.
pub fn evaluate(
    candidate: &ExperimentRun,
    baseline: Option<&ExperimentRun>,
    policy: EvalPolicy,
) -> EvalComparison {
    let mut deltas = BTreeMap::new();
    let mut regressed = Vec::new();
    let mut improved = Vec::new();
    for (metric, value) in &candidate.metrics {
        let base = baseline.and_then(|b| b.metrics.get(metric));
        match base {
            None => continue,
            Some(b) => {
                let delta = value - b;
                let higher_better = policy.higher_is_better.contains(&metric.as_str());
                deltas.insert(metric.clone(), delta);
                if delta == 0.0 {
                    continue;
                }
                let good = if higher_better {
                    delta > 0.0
                } else {
                    delta < 0.0
                };
                if good {
                    improved.push(metric.clone());
                } else {
                    regressed.push(metric.clone());
                }
            }
        }
    }
    regressed.sort();
    improved.sort();
    let verdict = if !regressed.is_empty() {
        PromotionVerdict::Reject
    } else if !improved.is_empty() {
        PromotionVerdict::Promote
    } else {
        PromotionVerdict::Hold
    };
    EvalComparison {
        candidate: candidate.id.clone(),
        baseline: baseline.map(|b| b.id.clone()),
        deltas,
        regressed,
        improved,
        verdict,
        note: match verdict {
            PromotionVerdict::Promote => {
                "candidate improved or matched every tracked metric".into()
            }
            PromotionVerdict::Hold => "no measurable improvement — hold".into(),
            PromotionVerdict::Reject => "regression detected on at least one tracked metric".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openrt_persistence::MemoryStore;

    fn spec(id: &str) -> ExperimentSpec {
        let mut params = BTreeMap::new();
        params.insert("lr".to_string(), 2e-4);
        ExperimentSpec {
            id: id.to_string(),
            dataset_id: "ds.1".into(),
            dataset_digest: Some("abc123".into()),
            base_model: "llama-3-8b".into(),
            method: TrainMethod::Lora,
            adapter: AdapterSpec {
                kind: "lora".into(),
                rank: 8,
                alpha: 16.0,
                target_modules: vec!["q_proj".into(), "v_proj".into()],
            },
            params,
            hardware: "cpu".into(),
            provenance: "unit-test".into(),
        }
    }

    #[tokio::test]
    async fn run_lifecycle_without_backend_fails_honestly() {
        let store = MemoryStore::new();
        let run = new_run(spec("exp.1")).unwrap();
        assert_eq!(run.status, ExperimentStatus::Planned);
        assert_eq!(run.approval, OperatorDecision::Pending);
        put_run(&store, &run).await.unwrap();
        let started = start_run(&store, "exp.1", None).await.unwrap();
        assert_eq!(started.status, ExperimentStatus::Failed);
        let reason = started.failure_reason.unwrap();
        assert!(reason.contains("no training backend"), "{reason}");
        assert!(get_run(&store, "exp.1").await.unwrap().is_some());
        assert_eq!(list_runs(&store).await.unwrap().len(), 1);
    }

    #[test]
    fn probe_is_honest_about_environment() {
        let probe = probe_backend();
        if probe.trainable {
            assert!(probe.python && probe.adapter_lib);
        } else {
            assert!(!probe.reasons.is_empty());
        }
    }

    #[test]
    fn evaluation_detects_regression_and_promotion() {
        let mut base = new_run(spec("b")).unwrap();
        base.status = ExperimentStatus::Completed;
        base.metrics.insert("loss".to_string(), 1.0);
        base.metrics.insert("accuracy".to_string(), 0.80);

        let mut cand = new_run(spec("c")).unwrap();
        cand.status = ExperimentStatus::Completed;
        cand.metrics.insert("loss".to_string(), 0.7);
        cand.metrics.insert("accuracy".to_string(), 0.85);
        let good = evaluate(&cand, Some(&base), EvalPolicy::default());
        assert_eq!(good.verdict, PromotionVerdict::Promote);
        assert_eq!(
            good.improved,
            vec!["accuracy".to_string(), "loss".to_string()]
        );

        let mut bad = new_run(spec("d")).unwrap();
        bad.status = ExperimentStatus::Completed;
        bad.metrics.insert("loss".to_string(), 1.4);
        bad.metrics.insert("accuracy".to_string(), 0.82);
        let reg = evaluate(&bad, Some(&base), EvalPolicy::default());
        assert_eq!(reg.verdict, PromotionVerdict::Reject);
        assert_eq!(reg.regressed, vec!["loss".to_string()]);

        let empty = evaluate(
            &new_run(spec("e")).unwrap(),
            Some(&base),
            EvalPolicy::default(),
        );
        assert_eq!(empty.verdict, PromotionVerdict::Hold);
    }
}
