//! Research experiment records: hypothesis → dataset → strategy →
//! backtest → conclusion, stored in `trading.experiments` for the workbench
//! and CLI to inspect.

use openrt_knowledge::safe_key;
use openrt_persistence::PersistenceStore;
use serde::{Deserialize, Serialize};

use crate::model::Provenance;
use crate::TradingError;

pub const EXPERIMENTS_NS: &str = "trading.experiments";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResearchExperiment {
    pub id: String,
    pub hypothesis: String,
    pub dataset_id: String,
    pub strategy_id: String,
    pub backtest_id: String,
    pub headline_metrics: serde_json::Value,
    pub conclusion: String,
    pub provenance: Provenance,
    pub created_at: String,
}

fn exp_key(id: &str) -> String {
    format!("experiment.{}", safe_key(id))
}

pub async fn put_experiment(
    store: &dyn PersistenceStore,
    exp: &ResearchExperiment,
) -> Result<(), TradingError> {
    let v = serde_json::to_value(exp)
        .map_err(|e| TradingError::Serde(format!("serialize experiment: {e}")))?;
    store
        .put_record(EXPERIMENTS_NS, &exp_key(&exp.id), &v)
        .await?;
    Ok(())
}

pub async fn get_experiment(
    store: &dyn PersistenceStore,
    id: &str,
) -> Result<Option<ResearchExperiment>, TradingError> {
    match store.get_record(EXPERIMENTS_NS, &exp_key(id)).await? {
        None => Ok(None),
        Some(v) => serde_json::from_value(v)
            .map(Some)
            .map_err(|e| TradingError::Corrupt(exp_key(id), e.to_string())),
    }
}

pub async fn list_experiments(
    store: &dyn PersistenceStore,
) -> Result<Vec<ResearchExperiment>, TradingError> {
    let mut out = Vec::new();
    for (key, value) in store.list_records(EXPERIMENTS_NS).await? {
        if !key.starts_with("experiment.") {
            continue;
        }
        let exp: ResearchExperiment =
            serde_json::from_value(value).map_err(|e| TradingError::Corrupt(key, e.to_string()))?;
        out.push(exp);
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}
